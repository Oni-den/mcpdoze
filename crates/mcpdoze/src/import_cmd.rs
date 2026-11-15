// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdoze import`: migrate an agent's MCP servers onto the broker. The
//! logic lives in `mcpdoze_core::import`; this module handles flags,
//! confirmation and the best-effort `POST /admin/v1/reload`.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use mcpdoze_core::config::{default_data_dir, Config, Scope};
use mcpdoze_core::import::{
    apply, build_plan_with, rollback, Agent, ApplyOptions, ImportEnv, PlanOptions,
};

/// Arguments of `mcpdoze import`.
#[derive(Debug, Clone)]
pub struct ImportArgs {
    pub from: String,
    pub dry_run: bool,
    pub yes: bool,
    pub rollback: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub broker_url: String,
    pub scope: Option<String>,
    /// Import `headersHelper` commands from project-scoped configs too.
    pub allow_helpers: bool,
}

/// Runs the import with human-readable output.
#[allow(dead_code)]
pub fn run(args: ImportArgs) -> anyhow::Result<()> {
    run_with(args, false)
}

/// Like [`run`]; with `json` the plan is printed as JSON (dry run) and the
/// result as a JSON object.
pub fn run_with(args: ImportArgs, json: bool) -> anyhow::Result<()> {
    let env = ImportEnv::detect(&args.broker_url, args.config.clone())?;
    if let Some(manifest) = &args.rollback {
        return do_rollback(manifest, &env, json);
    }
    let agent: Agent = args.from.parse()?;
    let scope = match args.scope.as_deref() {
        None => None,
        Some(s) => Some(parse_scope(s)?),
    };
    let plan = build_plan_with(
        agent,
        &env,
        &PlanOptions {
            scope,
            allow_helpers: args.allow_helpers,
        },
    )?;
    if args.dry_run {
        if json {
            println!("{}", plan.to_json());
        } else {
            print!("{}", plan.render_text());
            println!("\nDry run: nothing was changed.");
        }
        return Ok(());
    }
    if plan.items.is_empty() {
        if json {
            println!("{}", plan.to_json());
        } else {
            print!("{}", plan.render_text());
        }
        return Ok(());
    }
    if !json {
        print!("{}", plan.render_text());
    }
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            bail!("refusing to apply without confirmation: pass --yes (stdin is not a terminal)");
        }
        if !confirm("\nApply this plan? [y/N] ")? {
            bail!("aborted; nothing was changed");
        }
    }
    let result = apply(&plan, &env.data_dir, &ApplyOptions::default())?
        .ok_or_else(|| anyhow!("nothing to apply"))?;
    let reload = try_reload(&env);
    if json {
        let out = serde_json::json!({
            "manifest": result.manifest_path,
            "registered": result.manifest.registered_servers,
            "migrated": result.manifest.migrated,
            "reload": reload.as_ref().map(|_| "ok").unwrap_or("skipped"),
        });
        println!("{out}");
        return Ok(());
    }
    println!("\nMigrated {} server(s).", result.manifest.migrated.len());
    println!("Manifest: {}", result.manifest_path.display());
    for f in &result.manifest.files {
        if let Some(b) = &f.backup {
            println!("Backup:   {}", b.display());
        }
    }
    match reload {
        Ok(()) => println!("Broker reloaded."),
        Err(why) => println!("Broker not reloaded ({why}); run `mcpdoze reload` or restart it."),
    }
    for it in plan.items.iter().filter(|i| !i.needs_env_var.is_empty()) {
        for n in &it.needs_env_var {
            println!(
                "Set {} for server {} ({} `{}`) before first use.",
                n.suggested_env_var, it.name, n.location, n.key
            );
        }
    }
    println!(
        "Set {} to the broker token in the agent's environment (the token is never written to a file).",
        plan.token_env_var
    );
    println!("{}", rollback_hint(&result.manifest_path));
    Ok(())
}

/// The undo hint printed after an import. `FROM` is positional and not needed
/// with `--rollback`, so the hint names only the manifest.
fn rollback_hint(manifest: &Path) -> String {
    let p = manifest.display().to_string();
    let p = if p.contains(char::is_whitespace) {
        format!("\"{p}\"")
    } else {
        p
    };
    format!("Undo with: mcpdoze import --rollback {p}")
}

fn do_rollback(manifest: &Path, env: &ImportEnv, json: bool) -> anyhow::Result<()> {
    let r = rollback(manifest)?;
    let reload = try_reload(env);
    if json {
        let out = serde_json::json!({
            "restored": r.restored,
            "removed_servers": r.removed_servers,
            "reload": reload.as_ref().map(|_| "ok").unwrap_or("skipped"),
        });
        println!("{out}");
        return Ok(());
    }
    for p in &r.restored {
        println!("Restored {}", p.display());
    }
    for s in &r.removed_servers {
        println!("Removed server {s} from the broker config");
    }
    match reload {
        Ok(()) => println!("Broker reloaded."),
        Err(why) => println!("Broker not reloaded ({why})."),
    }
    Ok(())
}

fn parse_scope(s: &str) -> anyhow::Result<Scope> {
    match s {
        "shared" => Ok(Scope::Shared),
        "per-cwd" => Ok(Scope::PerCwd),
        "per-session" => Ok(Scope::PerSession),
        other => bail!("unknown scope `{other}` (use shared, per-cwd or per-session)"),
    }
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Ask a running broker to re-read its config. Best effort: any failure is
/// returned as a short reason (never containing the token). The broker also
/// watches its config file, so a failure here is not an error.
fn try_reload(env: &ImportEnv) -> Result<(), String> {
    let cfg = Config::load(&env.broker_config).map_err(|e| format!("config: {e}"))?;
    let token = read_token(&cfg, &env.data_dir).ok_or("no readable token")?;
    let authority = env
        .broker_url
        .split_once("://")
        .map(|x| x.1)
        .unwrap_or(&env.broker_url)
        .split('/')
        .next()
        .unwrap_or("");
    let loopback = authority.starts_with("127.")
        || authority.starts_with("localhost")
        || authority.starts_with("[::1]");
    if !loopback {
        return Err("broker URL is not loopback".into());
    }
    let addr = authority
        .to_socket_addrs()
        .map_err(|_| "cannot resolve broker address")?
        .next()
        .ok_or("cannot resolve broker address")?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|_| "broker not reachable")?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    s.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let req = format!(
        "POST /admin/v1/reload HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).map_err(|_| "write failed")?;
    let mut buf = Vec::new();
    s.take(4096)
        .read_to_end(&mut buf)
        .map_err(|_| "no response")?;
    let head = String::from_utf8_lossy(&buf);
    let status = head.lines().next().unwrap_or("");
    if status.split_whitespace().nth(1) == Some("200") {
        Ok(())
    } else {
        Err(format!("reload answered `{}`", status.trim()))
    }
}

fn read_token(cfg: &Config, data_dir: &Path) -> Option<String> {
    let data = || default_data_dir().unwrap_or_else(|| data_dir.to_path_buf());
    let raw = if let Some(var) = &cfg.broker.token_env {
        std::env::var(var).ok()?
    } else {
        let f = cfg
            .broker
            .token_file
            .clone()
            .unwrap_or_else(|| PathBuf::from("token"));
        let f = if f.is_absolute() { f } else { data().join(f) };
        std::fs::read_to_string(f).context("token").ok()?
    };
    let t = raw.trim().to_string();
    (!t.is_empty()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_hint_uses_real_cli_shape() {
        let h = rollback_hint(Path::new("m.json"));
        assert_eq!(h, "Undo with: mcpdoze import --rollback m.json");
        assert!(!h.contains("--from"));
        assert_eq!(
            rollback_hint(Path::new("a b.json")),
            "Undo with: mcpdoze import --rollback \"a b.json\""
        );
    }
}
