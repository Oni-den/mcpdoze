// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdoze doctor`: environment checks with actionable hints.

use std::io::Write;
use std::net::{TcpListener, ToSocketAddrs};

use anyhow::Result;
use mcpdoze_core::config::{Config, EnvValue, Transport};
use mcpdoze_core::proctree;

use super::{validation_lines, FAILED, OK};
use crate::cli::ConnArgs;
use crate::client::{Client, Conn, TokenSource};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

struct Report {
    items: Vec<(Level, String, Option<String>)>,
}

impl Report {
    fn add(&mut self, l: Level, msg: impl Into<String>, hint: Option<&str>) {
        self.items.push((l, msg.into(), hint.map(str::to_owned)));
    }
}

fn host_port(base: &str) -> &str {
    base.strip_prefix("http://")
        .or_else(|| base.strip_prefix("https://"))
        .unwrap_or(base)
}

const WINDOW_24H_MS: u64 = 24 * 3600 * 1000;

/// The debug mode line, the raw-capture warning, and per server with
/// failures in the last 24 hours: counts by kind, the top cause guess and
/// the export command. Reads the failure files directly.
async fn failure_log(
    r: &mut Report,
    conn: &Conn,
    config: Option<&Config>,
    client: Option<&Client>,
    reachable: bool,
    broker_failures: u64,
) {
    let mut on_servers: Vec<String> = Vec::new();
    let mut mode = config.map_or("off", |c| c.debug.mode.as_str()).to_owned();
    let mut raw = false;
    if reachable {
        if let Some(Ok(d)) = match client {
            Some(c) => Some(c.debug_status().await),
            None => None,
        } {
            raw = d.raw;
            on_servers = d
                .servers
                .iter()
                .filter(|(_, m)| m.as_str() != "off")
                .map(|(n, _)| n.clone())
                .collect();
            mode = d.mode;
        }
    } else if let Some(c) = config {
        on_servers = c
            .servers
            .iter()
            .filter(|(_, s)| s.debug.is_some_and(|m| m.as_str() != "off"))
            .map(|(n, _)| n.clone())
            .collect();
    }
    let off = mode == "off" && on_servers.is_empty();
    if off {
        if broker_failures > 0 {
            r.add(
                Level::Warn,
                format!(
                    "{broker_failures} backend failure(s) since the broker started, and the failure log is off"
                ),
                Some("run `mcpdoze debug on`, reproduce the problem, then `mcpdoze doctor` or `mcpdoze debug export`"),
            );
        } else {
            r.add(
                Level::Ok,
                "failure log: off (`mcpdoze debug on` records classified failures)",
                None,
            );
        }
    } else if on_servers.is_empty() {
        r.add(
            Level::Ok,
            format!("failure log: {mode} for all servers"),
            None,
        );
    } else {
        r.add(
            Level::Ok,
            format!(
                "failure log: {mode} globally; on for {}",
                on_servers.join(", ")
            ),
            None,
        );
    }
    if raw {
        r.add(
            Level::Warn,
            "raw frame capture is ON: full request and response frames are written to disk",
            Some("turn it off with `mcpdoze debug off`; never share the raw file"),
        );
    }

    let Some((file, keep)) = super::debug::failure_file(conn) else {
        return;
    };
    let since_ms = mcpdoze_core::stats::now_ms().saturating_sub(WINDOW_24H_MS);
    let filter = mcpdoze_core::diag::export::Filter {
        since_ms,
        ..Default::default()
    };
    let recs = mcpdoze_core::diag::export::read_records(&file, keep, &filter, 2000);
    for (name, f) in super::debug::summarize_failures(&recs) {
        let total: u64 = f.by_kind.values().sum();
        let kinds: Vec<String> = f.by_kind.iter().map(|(k, n)| format!("{k}={n}")).collect();
        let cause = f
            .top_guess
            .map(|g| format!("likely cause: {g}. "))
            .unwrap_or_default();
        r.add(
            Level::Warn,
            format!(
                "{name}: {total} failure(s) in the last 24h ({})",
                kinds.join(", ")
            ),
            Some(&format!(
                "{cause}share with `mcpdoze debug export --server {name}`"
            )),
        );
    }
}

/// Runs every check; returns 1 when any failed.
pub async fn run(args: ConnArgs, out: &mut dyn Write) -> Result<u8> {
    let conn = Conn::resolve(&args);
    let mut r = Report { items: Vec::new() };

    // Config.
    let config: Option<&Config> = match (&conn.config_path, &conn.config) {
        (Some(p), Some(Ok(c))) => {
            match c.validate() {
                Ok(()) => r.add(
                    Level::Ok,
                    format!("config {} parses and validates", p.display()),
                    None,
                ),
                Err(errs) => {
                    r.add(
                        Level::Fail,
                        format!(
                            "config {} is invalid: {}",
                            p.display(),
                            validation_lines(&errs).join("; ")
                        ),
                        Some("fix the listed fields, then run `mcpdoze reload`"),
                    );
                }
            }
            Some(c)
        }
        (Some(p), Some(Err(e))) => {
            r.add(
                Level::Fail,
                format!("config {} does not parse: {e}", p.display()),
                Some("fix the syntax, or pass --config"),
            );
            None
        }
        (Some(p), None) => {
            r.add(
                Level::Warn,
                format!("no config file at {}", p.display()),
                Some("the broker starts with no servers; add one with `mcpdoze add`"),
            );
            None
        }
        (None, _) => {
            r.add(
                Level::Warn,
                "no config path could be determined",
                Some("pass --config"),
            );
            None
        }
    };

    // Token.
    match &conn.token {
        TokenSource::File(p) => match conn.read_token() {
            Ok(_) => match mcpdoze_core::auth::warn_if_permissive(p) {
                Some(w) => r.add(Level::Warn, w, Some("restrict the file to the owner")),
                None => r.add(
                    Level::Ok,
                    format!("token file {} exists and is private", p.display()),
                    None,
                ),
            },
            Err(e) => r.add(
                Level::Fail,
                format!("{e:#}"),
                Some("run `mcpdoze serve` once to create the token, or pass --token-file"),
            ),
        },
        TokenSource::Env(v) => match conn.read_token() {
            Ok(_) => r.add(
                Level::Ok,
                format!("token environment variable {v} is set"),
                None,
            ),
            Err(e) => r.add(
                Level::Fail,
                format!("{e:#}"),
                Some("set the variable before running the broker and this command"),
            ),
        },
    }

    // Broker and port.
    let client = Client::new(&conn).ok();
    let reachable = match &client {
        Some(c) => c.healthz().await.unwrap_or(false),
        None => false,
    };
    // Backend failures counted by the broker since it started.
    let mut broker_failures: u64 = 0;
    if reachable {
        r.add(Level::Ok, format!("broker answers at {}", conn.base), None);
        if let Some(c) = &client {
            match c.status().await {
                Ok(st) => {
                    r.add(
                        Level::Ok,
                        format!(
                            "admin API accepts the token (mcpdoze {}, {} server(s))",
                            st.version,
                            st.servers.len()
                        ),
                        None,
                    );
                    broker_failures = st.servers.iter().map(|s| u64::from(s.failures)).sum();
                }
                Err(e) => r.add(
                    Level::Fail,
                    format!("admin API: {e:#}"),
                    Some("the broker was started with a different token; check --token-file"),
                ),
            }
        }
    } else {
        r.add(
            Level::Fail,
            format!("no broker answers at {}", conn.base),
            Some("start it with `mcpdoze serve`, or at login with `mcpdoze autostart install`"),
        );
        match host_port(&conn.base)
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
        {
            Some(addr) => match TcpListener::bind(addr) {
                Ok(_) => r.add(Level::Ok, format!("port {} is free", addr.port()), None),
                Err(e) => r.add(
                    Level::Fail,
                    format!("cannot bind {addr}: {e}"),
                    Some("another program uses the port; change `broker.bind` in the config"),
                ),
            },
            None => r.add(Level::Warn, format!("cannot resolve {}", conn.base), None),
        }
    }

    // Failure log (debug mode) and recent failures.
    failure_log(
        &mut r,
        &conn,
        config,
        client.as_ref(),
        reachable,
        broker_failures,
    )
    .await;

    // Background daemon, its log and the statistics log.
    match mcpdoze::run::daemon_log_path(config) {
        Some(p) => match std::fs::metadata(&p) {
            Ok(m) => r.add(
                Level::Ok,
                format!(
                    "daemon log (mcpdozed): {} ({} KB)",
                    p.display(),
                    m.len().div_ceil(1024)
                ),
                None,
            ),
            Err(_) => r.add(
                Level::Ok,
                format!("daemon log (mcpdozed): {} (not created yet)", p.display()),
                None,
            ),
        },
        None => r.add(
            Level::Warn,
            "cannot determine the daemon log directory",
            Some("set `broker.log_dir` in the config"),
        ),
    }
    if cfg!(windows) {
        if let Ok(exe) = std::env::current_exe() {
            match crate::autostart::daemon_next_to(&exe) {
                Some(d) => r.add(
                    Level::Ok,
                    format!("background daemon {} found", d.display()),
                    None,
                ),
                None => r.add(
                    Level::Warn,
                    format!("{} not found next to mcpdoze.exe", crate::autostart::DAEMON_EXE),
                    Some("`mcpdoze autostart install` then runs `mcpdoze serve`, which keeps a console window open; copy it from the release archive"),
                ),
            }
        }
    }
    let stats = config.map(|c| c.stats.clone()).unwrap_or_default();
    if !stats.enabled {
        r.add(
            Level::Ok,
            "statistics log disabled ([stats] enabled = false)",
            None,
        );
    } else {
        match stats.resolved_dir() {
            Some(d) => r.add(
                Level::Ok,
                format!("statistics log: {} (`mcpdoze stats`)", d.display()),
                None,
            ),
            None => r.add(
                Level::Warn,
                "cannot determine the statistics directory",
                Some("set `[stats] dir` in the config"),
            ),
        }
    }

    // Servers.
    if let Some(cfg) = config {
        for (name, s) in &cfg.servers {
            if !s.enabled {
                continue;
            }
            if s.transport == Transport::Stdio {
                let cmd = s.command.clone().unwrap_or_default();
                match proctree::resolve_command(&cmd, &s.args) {
                    Ok(_) => r.add(Level::Ok, format!("{name}: command `{cmd}` found"), None),
                    Err(e) => r.add(
                        Level::Fail,
                        format!("{name}: command `{cmd}`: {e}"),
                        Some("install it or give the full path; remember the broker needs it on its own PATH"),
                    ),
                }
            }
            let refs = s.env.values().chain(s.headers.values());
            for v in refs {
                if let EnvValue::Ref { from_env } = v {
                    if std::env::var_os(from_env).is_none() {
                        r.add(
                            Level::Warn,
                            format!(
                                "{name}: environment variable {from_env} is not set in this shell"
                            ),
                            Some("it must be set where the broker runs"),
                        );
                    }
                }
            }
        }
    }

    let mut failed = 0;
    for (l, msg, hint) in &r.items {
        let tag = match l {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => {
                failed += 1;
                "FAIL"
            }
        };
        writeln!(out, "[{tag}] {msg}")?;
        if let (Some(h), true) = (hint, *l != Level::Ok) {
            writeln!(out, "       hint: {h}")?;
        }
    }
    if failed > 0 {
        writeln!(out, "{failed} check(s) failed")?;
        Ok(FAILED)
    } else {
        writeln!(out, "all checks passed")?;
        Ok(OK)
    }
}
