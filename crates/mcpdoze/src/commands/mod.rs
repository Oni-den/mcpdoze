// SPDX-License-Identifier: GPL-3.0-or-later

//! Client commands. Each writes to a caller-supplied writer so tests can
//! inspect the output. Nothing here prints the bearer token (except
//! `token --show` on a terminal) or backend environment values.

mod debug;
mod doctor;
mod table;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::{anyhow, bail, Result};
use mcpdoze_core::admin::api::{
    RegisterRequest, ServerEntry, ServerStatus, StatsResponse, StatusResponse,
};
use mcpdoze_core::config::{
    parse_duration, EnvValue, Scope, ServerConfig, Transport, ValidationError,
};

use crate::cli::{
    AddArgs, Command, ConnArgs, ListArgs, LogsArgs, NameArgs, RefreshArgs, ScopeArg, StatsArgs,
    StatusArgs, StopArgs, TokenArgs, TransportArg,
};
use crate::client::{Client, Conn, TokenSource};
use table::Table;

/// Exit code of a successful command.
pub const OK: u8 = 0;
/// Exit code when a check failed.
pub const FAILED: u8 = 1;

/// Runs a client command; returns the process exit code.
pub async fn run(cmd: Command, out: &mut dyn Write, stdout_is_tty: bool) -> Result<u8> {
    match cmd {
        Command::Status(a) => status(a, out).await.map(|()| OK),
        Command::List(a) => list(a, out).await.map(|()| OK),
        Command::Add(a) => add(a, out).await.map(|()| OK),
        Command::Remove(a) => remove(a, out).await.map(|()| OK),
        Command::Stop(a) => stop(a, out).await.map(|()| OK),
        Command::Reload(c) => reload(c, out).await.map(|()| OK),
        Command::Refresh(a) => refresh(a, out).await.map(|()| OK),
        Command::Logs(a) => logs(a, out).await.map(|()| OK),
        Command::Token(a) => token(a, out, stdout_is_tty).map(|()| OK),
        Command::Doctor(c) => doctor::run(c, out).await,
        Command::Debug { action } => debug::run(action, out).await,
        Command::Stats(a) => stats(a, out).await.map(|()| OK),
        Command::Autostart { action } => {
            let msg = match action {
                crate::cli::AutostartCmd::Install { config, bind } => {
                    crate::autostart::install(crate::autostart::AutostartOpts { config, bind })?
                }
                crate::cli::AutostartCmd::Remove => crate::autostart::remove()?,
            };
            writeln!(out, "{msg}")?;
            Ok(OK)
        }
        Command::Import(a) => {
            let json = a.json;
            crate::import_cmd::run_with(
                crate::import_cmd::ImportArgs {
                    from: a.from.unwrap_or_default(),
                    dry_run: a.dry_run,
                    yes: a.yes,
                    rollback: a.rollback,
                    config: a.config,
                    broker_url: a.broker_url,
                    scope: a.scope,
                    allow_helpers: a.allow_helpers,
                },
                json,
            )?;
            Ok(OK)
        }
        Command::Hook { action } => crate::hook::run(action, out),
        Command::Serve(_) => Err(anyhow!("serve is not a client command")),
    }
}

fn client(conn: &ConnArgs) -> Result<Client> {
    Client::new(&Conn::resolve(conn))
}

// ------------------------------------------------------------- formatting

fn fmt_mb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

fn fmt_secs(s: u64) -> String {
    match (s / 3600, (s % 3600) / 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m{s}s"),
        (h, m, _) => format!("{h}h{m}m"),
    }
}

fn fmt_pids(pids: &[u32]) -> String {
    if pids.is_empty() {
        return "-".into();
    }
    let shown: Vec<String> = pids.iter().take(3).map(u32::to_string).collect();
    let more = pids.len().saturating_sub(3);
    if more > 0 {
        format!("{} +{more}", shown.join(","))
    } else {
        shown.join(",")
    }
}

fn transport_name(t: Transport) -> &'static str {
    match t {
        Transport::Stdio => "stdio",
        Transport::Http => "http",
    }
}

/// Renders the `status` table.
pub fn render_status(servers: &[ServerStatus]) -> String {
    let mut t = Table::new(&[
        "SERVER",
        "TRANSPORT",
        "STATE",
        "PIDS",
        "MEM_MB",
        "IN_FLIGHT",
        "IDLE_IN",
        "CALLS",
        "RESTARTS",
    ]);
    for s in servers {
        t.row(vec![
            s.name.clone(),
            transport_name(s.transport).into(),
            s.state.clone(),
            fmt_pids(&s.pids),
            if s.pids.is_empty() {
                "-".into()
            } else {
                fmt_mb(s.private_bytes)
            },
            s.in_flight.to_string(),
            s.idle_remaining_secs.map_or("-".into(), fmt_secs),
            s.calls.to_string(),
            s.restarts.to_string(),
        ]);
    }
    t.render()
}

/// Renders the `list` table.
pub fn render_list(servers: &[ServerEntry]) -> String {
    let mut t = Table::new(&[
        "SERVER",
        "PREFIX",
        "TRANSPORT",
        "TARGET",
        "REGISTERED",
        "NOTE",
    ]);
    for s in servers {
        let (tr, target) = match &s.config {
            Some(c) => (
                transport_name(c.transport).to_owned(),
                match c.transport {
                    Transport::Stdio => c.command.clone().unwrap_or_default(),
                    Transport::Http => c.url.clone().unwrap_or_default(),
                },
            ),
            None => ("-".into(), "-".into()),
        };
        t.row(vec![
            s.name.clone(),
            s.prefix.clone(),
            tr,
            target,
            if s.registered { "yes" } else { "no" }.into(),
            s.error.clone().unwrap_or_default(),
        ]);
    }
    t.render()
}

fn fmt_rate(r: f64) -> String {
    format!("{:.1}", r * 100.0)
}

/// Renders the `stats` summary (`period` as typed, e.g. `7d`).
pub fn render_stats(s: &StatsResponse, period: &str) -> String {
    let mut o = format!("statistics for the last {period} ({} events)\n", s.events);
    if s.servers.is_empty() {
        o.push_str("no calls logged\n");
    } else {
        let mut t = Table::new(&[
            "SERVER", "CALLS", "ERR%", "P50_MS", "P95_MS", "COLD", "COLD%", "SPAWNS", "UNLOADS",
        ]);
        for v in &s.servers {
            let unloads = if v.unloads.is_empty() {
                "-".to_owned()
            } else {
                v.unloads
                    .iter()
                    .map(|(k, n)| format!("{k}:{n}"))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            t.row(vec![
                v.server.clone(),
                v.calls.to_string(),
                fmt_rate(v.error_rate),
                v.p50_ms.to_string(),
                v.p95_ms.to_string(),
                v.cold_starts.to_string(),
                fmt_rate(v.cold_start_rate),
                v.spawns.to_string(),
                unloads,
            ]);
        }
        o.push_str(&t.render());
    }
    let m = &s.memory;
    if m.samples == 0 {
        o.push_str("memory: no samples yet\n");
    } else {
        o.push_str(&format!(
            "memory ({} samples): broker-side avg {:.1} MB, peak {:.1} MB; \
             estimated direct avg {:.1} MB, peak {:.1} MB\n",
            m.samples, m.avg_actual_mb, m.peak_actual_mb, m.avg_est_direct_mb, m.peak_est_direct_mb
        ));
        o.push_str(&format!(
            "saved: avg {:.1} MB, {:.1} MB-hours in total (estimate; see docs/STATS.md)\n",
            m.avg_saved_mb, m.mb_hours_saved
        ));
    }
    o.push_str(&format!(
        "sessions: {} opened, peak {} at once",
        s.sessions_opened, s.peak_sessions
    ));
    if s.dropped > 0 {
        o.push_str(&format!("; {} events dropped (queue full)", s.dropped));
    }
    o.push('\n');
    o
}

// --------------------------------------------------------------- commands

async fn stats(a: StatsArgs, out: &mut dyn Write) -> Result<()> {
    let period = parse_duration(&a.since).map_err(|e| anyhow!("--since: {e}"))?;
    let conn = Conn::resolve(&a.conn);
    let from_broker = match Client::new(&conn) {
        Ok(c) => c.stats(&a.since).await,
        Err(e) => Err(e),
    };
    let (summary, note) = match from_broker {
        Ok(s) => (s, None),
        Err(e) => {
            let cfg = conn.config.as_ref().and_then(|c| c.as_ref().ok());
            let dir = cfg
                .map(|c| c.stats.clone())
                .unwrap_or_default()
                .resolved_dir()
                .filter(|d| d.is_dir());
            let Some(dir) = dir else { return Err(e) };
            let until = mcpdoze_core::stats::now_ms();
            let from = until.saturating_sub(period.as_millis() as u64);
            let s = mcpdoze_core::stats::summarize_dir(&dir, from, until)?;
            (
                s,
                Some(format!("(broker not available; read {})", dir.display())),
            )
        }
    };
    if a.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&summary)?)?;
    } else {
        if let Some(n) = note {
            writeln!(out, "{n}")?;
        }
        write!(out, "{}", render_stats(&summary, &a.since))?;
    }
    Ok(())
}

async fn status(a: StatusArgs, out: &mut dyn Write) -> Result<()> {
    let st: StatusResponse = client(&a.conn)?.status().await?;
    if a.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&st)?)?;
    } else {
        writeln!(
            out,
            "mcpdoze {} - {} session(s), {} server(s)",
            st.version,
            st.sessions,
            st.servers.len()
        )?;
        write!(out, "{}", render_status(&st.servers))?;
    }
    Ok(())
}

async fn list(a: ListArgs, out: &mut dyn Write) -> Result<()> {
    let r = client(&a.conn)?.servers().await?;
    if a.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&r)?)?;
    } else {
        write!(out, "{}", render_list(&r.servers))?;
    }
    Ok(())
}

fn valid_env_name(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn valid_header_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parses `NAME=ENVVAR`. The right side must look like an environment
/// variable name, so a literal secret typed by mistake is refused (and is
/// not echoed back).
fn parse_ref(kind: &str, s: &str, name_ok: fn(&str) -> bool) -> Result<(String, EnvValue)> {
    let (name, var) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("--{kind} expects NAME=ENVVAR"))?;
    if !name_ok(name) {
        bail!("--{kind}: invalid name {name:?}");
    }
    if !valid_env_name(var) {
        bail!(
            "--{kind} {name}: the value must be the NAME of an environment variable (letters, digits, underscore), not the value itself; \
             to store a literal anyway use --env-literal with --allow-literal-env"
        );
    }
    Ok((
        name.to_owned(),
        EnvValue::Ref {
            from_env: var.to_owned(),
        },
    ))
}

/// Builds the request for `add` (pure, for tests).
pub fn build_register(a: &AddArgs) -> Result<RegisterRequest> {
    let mut env: BTreeMap<String, EnvValue> = BTreeMap::new();
    for e in &a.env {
        let (k, v) = parse_ref("env", e, valid_env_name)?;
        env.insert(k, v);
    }
    for e in &a.env_literal {
        if !a.allow_literal_env {
            bail!("--env-literal needs --allow-literal-env");
        }
        let (k, v) = e
            .split_once('=')
            .ok_or_else(|| anyhow!("--env-literal expects NAME=VALUE"))?;
        if !valid_env_name(k) {
            bail!("--env-literal: invalid name {k:?}");
        }
        env.insert(k.to_owned(), EnvValue::Literal(v.to_owned()));
    }
    let mut headers = BTreeMap::new();
    for h in &a.header {
        let (k, v) = parse_ref("header", h, valid_header_name)?;
        headers.insert(k, v);
    }
    let idle_ttl = match &a.ttl {
        Some(t) => Some(parse_duration(t).map_err(|e| anyhow!("--ttl: {e}"))?),
        None => None,
    };
    let (transport, command, args, url) = match a.transport {
        TransportArg::Stdio => {
            let Some((cmd, rest)) = a.command.split_first() else {
                bail!("a stdio server needs a command after `--`, e.g. `mcpdoze add {} -- npx -y some-mcp-server`", a.name);
            };
            if a.url.is_some() || !headers.is_empty() {
                bail!("--url and --header are for --transport http");
            }
            (Transport::Stdio, Some(cmd.clone()), rest.to_vec(), None)
        }
        TransportArg::Http => {
            if !a.command.is_empty() || a.cwd.is_some() || !env.is_empty() {
                bail!("--transport http takes --url and --header, not a command, --cwd or --env");
            }
            let Some(u) = a.url.clone() else {
                bail!("an http server needs --url");
            };
            (Transport::Http, None, Vec::new(), Some(u))
        }
    };
    Ok(RegisterRequest {
        name: a.name.clone(),
        server: ServerConfig {
            transport,
            command,
            args,
            env,
            cwd: a.cwd.clone(),
            scope: match a.scope {
                None | Some(ScopeArg::Shared) => Scope::Shared,
                Some(ScopeArg::PerCwd) => Scope::PerCwd,
                Some(ScopeArg::PerSession) => Scope::PerSession,
            },
            idle_ttl,
            prefix: a.prefix.clone(),
            enabled: true,
            url,
            headers,
            headers_helper: None,
            allow_literal_env: a.allow_literal_env,
            probe: true,
            catalog_max_age: None,
            max_instances: None,
            subscriptions: Default::default(),
            debug: None,
            call_timeout: None,
            optimize: None,
            tools: Default::default(),
        },
    })
}

async fn add(a: AddArgs, out: &mut dyn Write) -> Result<()> {
    let req = build_register(&a)?;
    let r = client(&a.conn())?.register(&req).await?;
    writeln!(
        out,
        "added {} (prefix {}){}",
        r.server.name,
        r.server.prefix,
        if r.persisted {
            ", saved to the config file"
        } else {
            ", not persisted (the broker runs without a config file)"
        }
    )?;
    writeln!(
        out,
        "it starts on its first call; check with `mcpdoze doctor`"
    )?;
    Ok(())
}

async fn remove(a: NameArgs, out: &mut dyn Write) -> Result<()> {
    let r = client(&a.conn)?.remove(&a.name).await?;
    writeln!(
        out,
        "removed {}{}",
        r.name,
        if r.persisted {
            " from the config file"
        } else {
            ""
        }
    )?;
    Ok(())
}

async fn stop(a: StopArgs, out: &mut dyn Write) -> Result<()> {
    let r = client(&a.conn)?.stop(&a.name, a.force).await?;
    writeln!(out, "{}: {}", r.name, r.state)?;
    Ok(())
}

async fn reload(c: ConnArgs, out: &mut dyn Write) -> Result<()> {
    let r = client(&c)?.reload().await?;
    let list = |v: &[String]| {
        if v.is_empty() {
            "-".to_owned()
        } else {
            v.join(", ")
        }
    };
    writeln!(out, "added:   {}", list(&r.added))?;
    writeln!(out, "removed: {}", list(&r.removed))?;
    writeln!(out, "changed: {}", list(&r.changed))?;
    for e in &r.errors {
        writeln!(out, "error:   {e}")?;
    }
    if !r.restart_required.is_empty() {
        writeln!(
            out,
            "restart the broker to apply: {}",
            r.restart_required.join(", ")
        )?;
    }
    Ok(())
}

async fn refresh(a: RefreshArgs, out: &mut dyn Write) -> Result<()> {
    let v = client(&a.conn)?.refresh(a.name.as_deref()).await?;
    match &a.name {
        Some(n) => writeln!(out, "refreshed {n}")?,
        None => writeln!(out, "refreshed all servers")?,
    }
    if !v.is_null() {
        let s = serde_json::to_string_pretty(&v)?;
        let s: String = s.chars().take(4000).collect();
        writeln!(out, "{s}")?;
    }
    Ok(())
}

async fn logs(a: LogsArgs, out: &mut dyn Write) -> Result<()> {
    let r = client(&a.conn)?.logs(&a.name, a.lines.min(500)).await?;
    if r.lines.is_empty() {
        writeln!(out, "(no output captured for {})", r.name)?;
    }
    for l in &r.lines {
        writeln!(out, "{l}")?;
    }
    Ok(())
}

fn token(a: TokenArgs, out: &mut dyn Write, tty: bool) -> Result<()> {
    let conn = Conn::resolve(&a.conn);
    if a.headers_json {
        // For an agent's headersHelper: machine-readable, so allowed when
        // stdout is a pipe. Read errors never contain the token.
        let header = format!("Bearer {}", conn.read_token()?);
        writeln!(out, "{}", serde_json::json!({ "Authorization": header }))?;
        return Ok(());
    }
    if a.show {
        if !tty {
            bail!("refusing to print the token: stdout is not a terminal (use the token file path instead)");
        }
        writeln!(out, "{}", conn.read_token()?)?;
        return Ok(());
    }
    match &conn.token {
        TokenSource::File(p) => writeln!(out, "{}", p.display())?,
        TokenSource::Env(v) => writeln!(
            out,
            "the token comes from the environment variable {v} (broker.token_env); there is no token file"
        )?,
    }
    Ok(())
}

/// Validation messages for display (kept separate for `doctor`).
pub(crate) fn validation_lines(errs: &[ValidationError]) -> Vec<String> {
    errs.iter().map(ToString::to_string).collect()
}
