// SPDX-License-Identifier: GPL-3.0-or-later

//! Command tests: argument-to-request building, rendering, and the client
//! commands against a real in-process broker on an ephemeral port.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mcpdoze_core::admin::api::ServerStatus;
use mcpdoze_core::config::{EnvValue, Transport};
use mcpdoze_core::daemon::{Daemon, ServeOptions};
use mcpdoze_testkit::MockServerSpec;

use super::*;
use crate::cli::{Cli, Command, ConnArgs};
use clap::Parser;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

fn parse(args: &[&str]) -> Command {
    let mut v = vec!["mcpdoze"];
    v.extend_from_slice(args);
    Cli::try_parse_from(v).unwrap().command
}

fn add_args(args: &[&str]) -> AddArgs {
    match parse(args) {
        Command::Add(a) => a,
        _ => panic!("not add"),
    }
}

// ------------------------------------------------------------------ unit

#[test]
fn add_builds_env_refs_and_command() {
    let a = add_args(&[
        "add",
        "example",
        "--ttl",
        "5m",
        "--env",
        "API_KEY=EXAMPLE_API_KEY",
        "--",
        "npx",
        "-y",
        "pkg",
    ]);
    let r = build_register(&a).unwrap();
    assert_eq!(r.name, "example");
    assert_eq!(r.server.command.as_deref(), Some("npx"));
    assert_eq!(r.server.args, vec!["-y", "pkg"]);
    assert_eq!(r.server.idle_ttl, Some(Duration::from_secs(300)));
    assert_eq!(
        r.server.env["API_KEY"],
        EnvValue::Ref {
            from_env: "EXAMPLE_API_KEY".into()
        }
    );
}

#[test]
fn add_refuses_literal_secret_without_echoing_it() {
    let a = add_args(&[
        "add",
        "x",
        "--env",
        "API_KEY=sk-abc123-very-secret",
        "--",
        "cmd",
    ]);
    let e = build_register(&a).unwrap_err().to_string();
    assert!(!e.contains("very-secret"), "{e}");
    assert!(e.contains("environment variable"));
    let a = add_args(&["add", "x", "--env", "NOEQUALS", "--", "cmd"]);
    assert!(build_register(&a).is_err());
}

#[test]
fn add_literal_with_opt_in_and_http_rules() {
    let a = add_args(&[
        "add",
        "x",
        "--env-literal",
        "MODE=fast",
        "--allow-literal-env",
        "--",
        "cmd",
    ]);
    let r = build_register(&a).unwrap();
    assert!(r.server.allow_literal_env);
    assert_eq!(r.server.env["MODE"], EnvValue::Literal("fast".into()));

    assert!(
        build_register(&add_args(&["add", "x"])).is_err(),
        "stdio needs a command"
    );
    assert!(build_register(&add_args(&["add", "x", "--transport", "http"])).is_err());
    let a = add_args(&[
        "add",
        "x",
        "--transport",
        "http",
        "--url",
        "https://example.com/mcp",
        "--header",
        "Authorization=REMOTE_AUTH",
    ]);
    let r = build_register(&a).unwrap();
    assert_eq!(r.server.transport, Transport::Http);
    assert!(r.server.headers.contains_key("Authorization"));
    let a = add_args(&["add", "x", "--ttl", "soon", "--", "cmd"]);
    assert!(build_register(&a).is_err());
}

fn status_row(name: &str, running: bool) -> ServerStatus {
    ServerStatus {
        name: name.into(),
        prefix: name.into(),
        transport: Transport::Stdio,
        state: if running { "running" } else { "stopped" }.into(),
        pids: if running { vec![10, 11] } else { vec![] },
        rss_bytes: 0,
        private_bytes: 75_497_472,
        in_flight: 1,
        idle_remaining_secs: running.then_some(125),
        calls: 17,
        restarts: 2,
        failures: 0,
        last_error: None,
        scope: "shared".into(),
        instances: Vec::new(),
    }
}

#[test]
fn status_table_rendering() {
    let t = render_status(&[status_row("alpha", true), status_row("b", false)]);
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].starts_with("SERVER"));
    assert!(lines[1].contains("running") && lines[1].contains("10,11"));
    assert!(lines[1].contains("72.0") && lines[1].contains("2m5s"));
    assert!(lines[2].contains("stopped") && lines[2].contains(" - "));
}

#[test]
fn duration_and_pid_formatting() {
    assert_eq!(fmt_secs(5), "5s");
    assert_eq!(fmt_secs(600), "10m");
    assert_eq!(fmt_secs(3700), "1h1m");
    assert_eq!(fmt_pids(&[1, 2, 3, 4, 5]), "1,2,3 +2");
}

#[test]
fn token_show_refused_when_not_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let tf = dir.path().join("token");
    std::fs::write(&tf, "unit-test-token-value-1\n").unwrap();
    let args = TokenArgs {
        conn: ConnArgs {
            config: Some(dir.path().join("absent.toml")),
            url: None,
            token_file: Some(tf.clone()),
        },
        show: true,
    };
    let mut out = Vec::new();
    let e = token(args, &mut out, false).unwrap_err().to_string();
    assert!(!e.contains("unit-test-token-value"));
    assert!(out.is_empty());

    let args = TokenArgs {
        conn: ConnArgs {
            config: Some(dir.path().join("absent.toml")),
            url: None,
            token_file: Some(tf.clone()),
        },
        show: false,
    };
    token(args, &mut out, false).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(s.trim(), tf.display().to_string());

    let args = TokenArgs {
        conn: ConnArgs {
            config: Some(dir.path().join("absent.toml")),
            url: None,
            token_file: Some(tf),
        },
        show: true,
    };
    let mut out = Vec::new();
    token(args, &mut out, true).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap().trim(),
        "unit-test-token-value-1"
    );
}

// ----------------------------------------------------------- integration

struct Env {
    _dir: tempfile::TempDir,
    config: PathBuf,
    token_file: PathBuf,
    daemon: Daemon,
    token: String,
}

async fn start(extra: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("token");
    let config = dir.path().join("config.toml");
    let text = format!(
        "[broker]\nbind = \"127.0.0.1:0\"\ntoken_file = '{}'\n{extra}",
        token_file.display()
    );
    std::fs::write(&config, text).unwrap();
    let daemon = Daemon::start(ServeOptions {
        config_path: Some(config.clone()),
        watch: false,
        ..ServeOptions::default()
    })
    .await
    .expect("daemon start");
    let token = std::fs::read_to_string(&token_file)
        .unwrap()
        .trim()
        .to_owned();
    assert!(token.len() >= 32);
    Env {
        _dir: dir,
        config,
        token_file,
        daemon,
        token,
    }
}

async fn exec(cmd: Command) -> (Result<u8>, String) {
    let mut out = Vec::new();
    let r = tokio::time::timeout(TEST_TIMEOUT, run(cmd, &mut out, false))
        .await
        .expect("command timed out");
    (r, String::from_utf8(out).unwrap())
}

fn with_conn(env: &Env, base: &[&str]) -> Command {
    let mut v: Vec<String> = base.iter().map(|s| (*s).to_owned()).collect();
    let (url, tf, cfg) = (
        format!("http://{}", env.daemon.local_addr()),
        env.token_file.display().to_string(),
        env.config.display().to_string(),
    );
    // `--` terminates options for add; put connection flags before it.
    let at = v.iter().position(|s| s == "--").unwrap_or(v.len());
    let flags = [
        "--url".to_owned(),
        url,
        "--token-file".into(),
        tf,
        "--config".into(),
        cfg,
    ];
    let flags: Vec<String> = if v.first().map(String::as_str) == Some("add") {
        let mut f = flags.to_vec();
        f[0] = "--broker-url".into();
        f
    } else {
        flags.to_vec()
    };
    for (i, f) in flags.into_iter().enumerate() {
        v.insert(at + i, f);
    }
    let refs: Vec<&str> = v.iter().map(String::as_str).collect();
    parse(&refs)
}

fn mock_command() -> (String, Vec<String>) {
    let spec = MockServerSpec::new().name("cli-test").tools(&["echo"]);
    (spec.program_path().display().to_string(), spec.args())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_lifecycle_never_prints_the_token() {
    let env = start("").await;
    let mut all = String::new();
    let (prog, margs) = mock_command();

    let mut add = vec![
        "add",
        "example",
        "--ttl",
        "5m",
        "--env",
        "EXAMPLE_VAR=PATH",
        "--",
    ];
    let prog_s = prog.as_str();
    add.push(prog_s);
    let margs_s: Vec<&str> = margs.iter().map(String::as_str).collect();
    add.extend(margs_s);
    let (r, out) = exec(with_conn(&env, &add)).await;
    assert_eq!(r.unwrap(), OK, "{out}");
    assert!(out.contains("added example"), "{out}");
    all += &out;

    // Persisted to the config file as a reference.
    let text = std::fs::read_to_string(&env.config).unwrap();
    assert!(text.contains("from_env"), "{text}");

    // Duplicate name is a friendly error.
    let (r, _) = exec(with_conn(&env, &["add", "example", "--", "cmd"])).await;
    assert!(r.unwrap_err().to_string().contains("409"));

    let (r, out) = exec(with_conn(&env, &["list"])).await;
    assert_eq!(r.unwrap(), OK);
    assert!(out.contains("example") && out.contains("yes"), "{out}");
    all += &out;

    let (r, out) = exec(with_conn(&env, &["status"])).await;
    assert_eq!(r.unwrap(), OK);
    assert!(out.contains("example") && out.contains("stopped"), "{out}");
    all += &out;

    let (r, out) = exec(with_conn(&env, &["status", "--json"])).await;
    assert_eq!(r.unwrap(), OK);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["servers"][0]["name"], "example");
    all += &out;

    let (r, out) = exec(with_conn(&env, &["logs", "example", "-n", "5"])).await;
    assert_eq!(r.unwrap(), OK);
    all += &out;

    let (r, out) = exec(with_conn(&env, &["stop", "example", "--force"])).await;
    assert_eq!(r.unwrap(), OK);
    assert!(out.contains("example: stopped"), "{out}");
    all += &out;

    let (r, out) = exec(with_conn(&env, &["reload"])).await;
    assert_eq!(r.unwrap(), OK, "{out}");
    all += &out;

    // Refresh exists once the core provides it; until then a clear error.
    let (r, out) = exec(with_conn(&env, &["refresh", "example"])).await;
    match r {
        Ok(_) => all += &out,
        Err(e) => {
            let m = e.to_string();
            assert!(m.contains("404") || m.contains("HTTP"), "{m}");
        }
    }

    let (r, out) = exec(with_conn(&env, &["remove", "example"])).await;
    assert_eq!(r.unwrap(), OK, "{out}");
    all += &out;
    let (r, _) = exec(with_conn(&env, &["stop", "example"])).await;
    assert!(r.unwrap_err().to_string().contains("404"));

    // Token path only.
    let (r, out) = exec(with_conn(&env, &["token"])).await;
    assert_eq!(r.unwrap(), OK);
    assert_eq!(out.trim(), env.token_file.display().to_string());
    all += &out;

    assert!(!all.contains(&env.token), "token leaked into output");
    env.daemon.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_passes_for_a_healthy_setup_and_flags_missing_commands() {
    let me = std::env::current_exe().unwrap();
    let extra = format!(
        "\n[servers.good]\ntransport = \"stdio\"\ncommand = '{}'\n",
        me.display()
    );
    let env = start(&extra).await;
    let (r, out) = exec(with_conn(&env, &["doctor"])).await;
    assert_eq!(r.unwrap(), OK, "{out}");
    assert!(
        out.contains("broker answers") && out.contains("all checks passed"),
        "{out}"
    );
    assert!(!out.contains(&env.token));

    // A server whose command does not exist makes doctor fail with a hint.
    let (r, out) = exec(with_conn(
        &env,
        &["add", "bad", "--", "mcpdoze-no-such-command-xyz"],
    ))
    .await;
    r.unwrap();
    let _ = out;
    let (r, out) = exec(with_conn(&env, &["doctor"])).await;
    assert_eq!(r.unwrap(), FAILED, "{out}");
    assert!(
        out.contains("bad: command") && out.contains("hint:"),
        "{out}"
    );
    assert!(!out.contains(&env.token));
    env.daemon.shutdown().await.unwrap();
}

fn closed_port_url() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{p}")
}

fn tmp_conn(dir: &Path, url: String) -> ConnArgs {
    let tf = dir.join("token");
    std::fs::write(&tf, "offline-test-token-value\n").unwrap();
    ConnArgs {
        config: Some(dir.join("none.toml")),
        url: Some(url),
        token_file: Some(tf),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_down_gives_friendly_errors() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tmp_conn(dir.path(), closed_port_url());
    let (r, _) = exec(Command::Status(crate::cli::StatusArgs {
        conn: conn.clone(),
        json: false,
    }))
    .await;
    let e = r.unwrap_err().to_string();
    assert!(
        e.contains("mcpdoze serve") && e.contains("autostart install"),
        "{e}"
    );
    assert!(!e.contains("offline-test-token"));

    let (r, out) = exec(Command::Doctor(conn)).await;
    assert_eq!(r.unwrap(), FAILED);
    assert!(
        out.contains("no broker answers") && out.contains("hint:"),
        "{out}"
    );
    assert!(!out.contains("offline-test-token"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_token_is_reported_without_printing_it() {
    let env = start("").await;
    let dir = tempfile::tempdir().unwrap();
    let mut conn = tmp_conn(dir.path(), format!("http://{}", env.daemon.local_addr()));
    conn.config = Some(env.config.clone());
    let (r, _) = exec(Command::Status(crate::cli::StatusArgs {
        conn,
        json: false,
    }))
    .await;
    let e = r.unwrap_err().to_string();
    assert!(e.contains("rejected the token"), "{e}");
    assert!(!e.contains("offline-test-token"));
    env.daemon.shutdown().await.unwrap();
}
