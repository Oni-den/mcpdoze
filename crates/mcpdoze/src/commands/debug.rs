// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdoze debug on|off|status|show|export`.
//!
//! `on`, `off` and `status` talk to the admin API (`/debug`). `show` and
//! `export` read the failure log files directly, so they work with the
//! broker down; when it is reachable it is asked to flush first.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use mcpdoze_core::admin::api::{DebugSetRequest, FailuresQuery};
use mcpdoze_core::config::{parse_duration, Config, DebugConfig};
use mcpdoze_core::diag::export::{
    self, ExportError, ExportInput, ExportOptions, Filter, LaunchShape, MAX_RECORDS,
};
use mcpdoze_core::diag::FILE_NAME;
use mcpdoze_core::stats::now_ms;
use serde_json::Value;

use super::OK;
use crate::cli::{DebugAction, DebugExportArgs, DebugOffArgs, DebugOnArgs, DebugShowArgs};
use crate::client::{Client, Conn};

/// Raw frame capture is not built into this version.
pub const RAW_CAPTURE_AVAILABLE: bool = false;
/// What `--raw` prints while [`RAW_CAPTURE_AVAILABLE`] is false.
pub const RAW_UNAVAILABLE: &str =
    "raw capture not available in this version; use `mcpdoze debug on` (failures) or `--trace`";

/// Runs a `debug` subcommand.
pub async fn run(action: DebugAction, out: &mut dyn Write) -> Result<u8> {
    match action {
        DebugAction::On(a) => on(a, out).await.map(|()| OK),
        DebugAction::Off(a) => off(a, out).await.map(|()| OK),
        DebugAction::Status(a) => status(&Conn::resolve(&a.conn), out).await.map(|()| OK),
        DebugAction::Show(a) => show(a, out).await.map(|()| OK),
        DebugAction::Export(a) => export_cmd(a, out).await,
    }
}

// ---------------------------------------------------------- on/off/status

/// The `POST /debug` body of `debug on`.
pub(crate) fn build_on_request(a: &DebugOnArgs) -> Result<DebugSetRequest> {
    if let Some(d) = &a.duration {
        parse_duration(d).map_err(|e| anyhow!("--for: {e}"))?;
    }
    Ok(DebugSetRequest {
        mode: if a.trace { "trace" } else { "failures" }.into(),
        servers: a.servers.clone(),
        raw: a.raw,
        duration: a.duration.clone(),
    })
}

async fn on(a: DebugOnArgs, out: &mut dyn Write) -> Result<()> {
    let req = build_on_request(&a)?;
    if req.raw && !RAW_CAPTURE_AVAILABLE {
        bail!("{RAW_UNAVAILABLE}");
    }
    let st = Client::new(&Conn::resolve(&a.conn))?
        .debug_set(&req)
        .await?;
    let scope = if req.servers.is_empty() {
        "all servers".to_owned()
    } else {
        req.servers.join(", ")
    };
    let until = match st.expires_in_secs {
        Some(s) => format!(", reverts in {}", super::fmt_secs(s)),
        None => ", until the broker restarts".to_owned(),
    };
    writeln!(out, "debug {} for {scope}{until}", req.mode)?;
    writeln!(
        out,
        "reproduce the problem, then run `mcpdoze doctor` or `mcpdoze debug export`"
    )?;
    Ok(())
}

async fn off(a: DebugOffArgs, out: &mut dyn Write) -> Result<()> {
    let req = DebugSetRequest {
        mode: "off".into(),
        servers: a.servers.clone(),
        raw: false,
        duration: None,
    };
    Client::new(&Conn::resolve(&a.conn))?
        .debug_set(&req)
        .await?;
    let scope = if a.servers.is_empty() {
        "all servers".to_owned()
    } else {
        a.servers.join(", ")
    };
    writeln!(out, "debug off for {scope}")?;
    Ok(())
}

async fn status(conn: &Conn, out: &mut dyn Write) -> Result<()> {
    let st = Client::new(conn)?.debug_status().await?;
    writeln!(out, "debug mode: {}", st.mode)?;
    if !st.available {
        writeln!(
            out,
            "no debug directory can be determined; set [debug] dir in the config"
        )?;
    }
    if st.raw {
        writeln!(out, "raw capture: ON (full frames are written to disk)")?;
    }
    if let Some(s) = st.expires_in_secs {
        writeln!(out, "runtime override reverts in {}", super::fmt_secs(s))?;
    }
    for (name, mode) in &st.servers {
        writeln!(out, "  {name}: {mode}")?;
    }
    writeln!(
        out,
        "records written: {}, dropped: {}",
        st.written, st.dropped
    )?;
    Ok(())
}

// ---------------------------------------------------------------- files

/// The failure log file and the number of rotated files kept, from the
/// config (defaults without one).
pub(super) fn failure_file(conn: &Conn) -> Option<(PathBuf, usize)> {
    let cfg: Option<&Config> = conn.config.as_ref().and_then(|c| c.as_ref().ok());
    let d = cfg.map_or_else(DebugConfig::default, |c| c.debug.clone());
    Some((d.resolved_dir()?.join(FILE_NAME), d.keep))
}

/// `YYYY-MM-DD HH:MM:SS` (UTC) of a unix time in ms.
pub(crate) fn fmt_time(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// ------------------------------------------------------------------ show

fn render_failures(records: &[Value]) -> String {
    let mut t = super::table::Table::new(&["TIME (UTC)", "ID", "SERVER", "KIND", "COUNT", "CAUSE"]);
    for r in records {
        t.row(vec![
            fmt_time(r["ts"].as_u64().unwrap_or(0)),
            r["id"].as_str().unwrap_or("-").into(),
            r["server"].as_str().unwrap_or("-").into(),
            r["kind"].as_str().unwrap_or("-").into(),
            r["repeat"].as_u64().unwrap_or(1).to_string(),
            r.pointer("/cause/guess")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .into(),
        ]);
    }
    t.render()
}

async fn show(a: DebugShowArgs, out: &mut dyn Write) -> Result<()> {
    let conn = Conn::resolve(&a.conn);
    let limit = a.last.clamp(1, MAX_RECORDS * 5);
    let q = FailuresQuery {
        server: a.server.clone(),
        kind: a.kind.clone(),
        since: None,
        limit: Some(limit),
    };
    let from_broker = match Client::new(&conn) {
        Ok(c) => c.failures(&q).await.ok().map(|r| r.records),
        Err(_) => None,
    };
    let records = match from_broker {
        Some(r) => r,
        None => {
            let (file, keep) = failure_file(&conn)
                .ok_or_else(|| anyhow!("cannot determine the debug directory; set [debug] dir"))?;
            let f = Filter {
                server: a.server.clone(),
                kind: a.kind.clone(),
                since_ms: 0,
            };
            export::read_records(&file, keep, &f, limit)
        }
    };
    if a.json {
        for r in &records {
            writeln!(out, "{r}")?;
        }
    } else if records.is_empty() {
        writeln!(
            out,
            "no failures recorded; turn recording on with `mcpdoze debug on`"
        )?;
    } else {
        write!(out, "{}", render_failures(&records))?;
    }
    Ok(())
}

// ---------------------------------------------------------------- export

/// A path for terminal output: relative to the working directory when it
/// lies below it, else just the file name (never the home directory).
fn shown(p: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = p.strip_prefix(&cwd) {
            return rel.display().to_string();
        }
    }
    p.file_name()
        .map_or_else(|| "<file>".to_owned(), |n| n.to_string_lossy().into_owned())
}

fn default_out(now: u64) -> PathBuf {
    PathBuf::from(format!("mcpdoze-debug-{}.md", now / 1000))
}

async fn export_cmd(a: DebugExportArgs, out: &mut dyn Write) -> Result<u8> {
    let now = now_ms();
    let since_ms = export::parse_since(&a.since, now).map_err(|e| anyhow!("--since: {e}"))?;
    let conn = Conn::resolve(&a.conn);
    let client = Client::new(&conn).ok();
    // Ask the broker to flush pending records first; the log is read from
    // disk either way.
    let mut broker_running = false;
    if let Some(c) = &client {
        let q = FailuresQuery {
            limit: Some(1),
            ..Default::default()
        };
        broker_running = c.failures(&q).await.is_ok();
    }
    let (file, keep) = failure_file(&conn)
        .ok_or_else(|| anyhow!("cannot determine the debug directory; set [debug] dir"))?;
    let filter = Filter {
        server: a.server.clone(),
        kind: None,
        since_ms,
    };
    let records = export::read_records(&file, keep, &filter, MAX_RECORDS);
    if records.is_empty() {
        bail!(
            "no failures recorded in the last {} (log: {}); turn recording on with `mcpdoze debug on`, reproduce the problem, then run this again",
            a.since,
            shown(&file)
        );
    }

    let cfg: Option<&Config> = conn.config.as_ref().and_then(|c| c.as_ref().ok());
    let mut names: Vec<String> = records
        .iter()
        .filter_map(|r| r["server"].as_str().map(str::to_owned))
        .collect();
    names.sort();
    names.dedup();
    let servers: Vec<LaunchShape> = cfg
        .map(|c| {
            c.servers
                .iter()
                .filter(|(n, _)| names.contains(n))
                .map(|(n, s)| LaunchShape::from_config(n, s))
                .collect()
        })
        .unwrap_or_default();

    let period = parse_duration(&a.since).map_err(|e| anyhow!("--since: {e}"))?;
    let stats = match (&client, broker_running) {
        (Some(c), true) => c.stats(&a.since).await.ok(),
        _ => cfg
            .map(|c| c.stats.clone())
            .unwrap_or_default()
            .resolved_dir()
            .filter(|d| d.is_dir())
            .and_then(|d| {
                let from = now.saturating_sub(period.as_millis() as u64);
                mcpdoze_core::stats::summarize_dir(&d, from, now).ok()
            }),
    };

    let mut secrets: Vec<(String, String)> = cfg
        .map(|c| {
            c.servers
                .values()
                .flat_map(export::config_secrets)
                .collect()
        })
        .unwrap_or_default();
    if let Ok(t) = std::env::var(mcpdoze_core::backend::TOKEN_ENV_VAR) {
        secrets.push((mcpdoze_core::backend::TOKEN_ENV_VAR.to_owned(), t));
    }
    let mut opts = ExportOptions::new(now).with_secrets(secrets);
    opts.strict = a.strict;
    opts.identify = a.identify;
    opts.since_ms = since_ms;
    let built = export::build(
        ExportInput {
            records,
            servers,
            stats,
            broker_running,
        },
        &opts,
    );
    let ex = match built {
        Ok(e) => e,
        Err(e @ ExportError::SelfCheck(_)) => {
            if a.strict {
                bail!("{e}, even with --strict; nothing was written. Do not share the failure log; describe the problem without it");
            }
            bail!("{e}; nothing was written. re-run with --strict");
        }
    };

    let note = format!(
        "{} record(s), {} hash(es) re-keyed{}",
        ex.counts.records,
        ex.counts.rekeyed,
        if a.strict {
            ", strict (no free text)"
        } else {
            ""
        }
    );
    if a.stdout {
        out.write_all(ex.text.as_bytes())?;
        eprintln!("mcpdoze: {note}");
    } else {
        let path = a.out.clone().unwrap_or_else(|| default_out(now));
        let mut o = std::fs::OpenOptions::new();
        o.write(true);
        if a.out.is_some() {
            o.create(true).truncate(true);
        } else {
            o.create_new(true);
        }
        let mut f = o
            .open(&path)
            .with_context(|| format!("cannot write {}", shown(&path)))?;
        f.write_all(ex.text.as_bytes())
            .with_context(|| format!("cannot write {}", shown(&path)))?;
        writeln!(out, "wrote {} ({note})", shown(&path))?;
        writeln!(
            out,
            "it is built to be pasted into an issue; skim it first. `mcpdoze logs` output is not safe to paste."
        )?;
    }
    Ok(OK)
}

// -------------------------------------------------------- doctor helpers

/// Failures of one server, for `doctor`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ServerFailures {
    /// Occurrences by kind.
    pub by_kind: BTreeMap<String, u64>,
    /// The most frequent `cause.guess`.
    pub top_guess: Option<String>,
}

/// Groups records (newest first or not) per server: counts by kind and the
/// most frequent cause guess. `suppressed` summaries are not counted.
pub(super) fn summarize_failures(records: &[Value]) -> BTreeMap<String, ServerFailures> {
    let mut guesses: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut out: BTreeMap<String, ServerFailures> = BTreeMap::new();
    for r in records {
        let (Some(server), Some(kind)) = (r["server"].as_str(), r["kind"].as_str()) else {
            continue;
        };
        if kind == "suppressed" {
            continue;
        }
        let n = r["repeat"].as_u64().unwrap_or(1).max(1);
        *out.entry(server.to_owned())
            .or_default()
            .by_kind
            .entry(kind.to_owned())
            .or_insert(0) += n;
        if let Some(g) = r.pointer("/cause/guess").and_then(Value::as_str) {
            *guesses
                .entry(server.to_owned())
                .or_default()
                .entry(g.to_owned())
                .or_insert(0) += n;
        }
    }
    for (s, f) in &mut out {
        f.top_guess = guesses
            .get(s)
            .and_then(|g| {
                g.iter()
                    .max_by_key(|(k, n)| (**n, std::cmp::Reverse((*k).clone())))
            })
            .map(|(g, _)| g.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command, DebugAction};
    use clap::Parser;
    use serde_json::json;

    fn on_args(args: &[&str]) -> DebugOnArgs {
        let mut v = vec!["mcpdoze", "debug", "on"];
        v.extend_from_slice(args);
        match Cli::try_parse_from(v).unwrap().command {
            Command::Debug {
                action: DebugAction::On(a),
            } => a,
            _ => panic!(),
        }
    }

    #[test]
    fn on_builds_the_request() {
        let r = build_on_request(&on_args(&["a", "--for", "30m"])).unwrap();
        assert_eq!(r.mode, "failures");
        assert_eq!(r.servers, vec!["a"]);
        assert_eq!(r.duration.as_deref(), Some("30m"));
        assert!(!r.raw);
        let r = build_on_request(&on_args(&["--trace"])).unwrap();
        assert_eq!(r.mode, "trace");
        assert!(r.servers.is_empty());
        let r = build_on_request(&on_args(&["--raw"])).unwrap();
        assert!(r.raw);
        assert!(build_on_request(&on_args(&["--for", "soon"])).is_err());
    }

    #[tokio::test]
    async fn raw_is_refused_with_a_clear_message() {
        let mut out = Vec::new();
        let e = on(on_args(&["--raw"]), &mut out).await.unwrap_err();
        assert!(e.to_string().contains("raw capture not available"), "{e}");
    }

    #[test]
    fn time_formatting() {
        assert_eq!(fmt_time(0), "1970-01-01 00:00:00");
        assert_eq!(fmt_time(1_709_210_096_000), "2024-02-29 12:34:56");
    }

    #[test]
    fn failure_summary_counts_and_picks_the_top_guess() {
        let recs = vec![
            json!({"server":"a","kind":"stalled","repeat":2,"cause":{"guess":"slow"}}),
            json!({"server":"a","kind":"rpc_error","cause":{"guess":"bad args"}}),
            json!({"server":"a","kind":"suppressed"}),
            json!({"server":"b","kind":"stalled"}),
        ];
        let s = summarize_failures(&recs);
        assert_eq!(s["a"].by_kind["stalled"], 2);
        assert_eq!(s["a"].by_kind["rpc_error"], 1);
        assert_eq!(s["a"].top_guess.as_deref(), Some("slow"));
        assert_eq!(s["b"].top_guess, None);
        assert!(!s["a"].by_kind.contains_key("suppressed"));
    }

    #[test]
    fn rendering_shows_the_cause() {
        let t = render_failures(&[
            json!({"ts":0,"id":"f-000001","server":"a","kind":"stalled","repeat":3,"cause":{"guess":"slow"}}),
        ]);
        assert!(t.contains("f-000001") && t.contains("slow") && t.contains('3'));
    }
}
