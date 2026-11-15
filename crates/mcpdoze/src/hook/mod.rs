// SPDX-License-Identifier: GPL-3.0-or-later

//! Optional Claude Code hook that blocks native MCP-server installs and
//! points the agent at `mcpdoze add`. See `docs/HOOK.md`.

pub mod check;
pub mod install;

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::cli::{HookAction, HookAgent, HookInstallArgs, HookScope};
use check::{check_event, Decision, BYPASS_ENV, DENY_REASON};
use install::Op;

/// Largest hook event read from stdin.
const MAX_EVENT_BYTES: u64 = 1024 * 1024;

/// JSON printed to stdout for a denied call.
pub fn deny_output() -> Value {
    json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": DENY_REASON,
    }})
}

/// Decides from raw stdin bytes. Anything unparsable allows (prints nothing).
pub fn decide_bytes(bytes: &[u8], bypass: bool) -> Decision {
    if bypass || bytes.len() as u64 > MAX_EVENT_BYTES {
        return Decision::Allow;
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v) => check_event(&v),
        Err(_) => Decision::Allow,
    }
}

/// `mcpdoze hook claude-code`: never fails; exit code is always 0.
fn run_check(out: &mut dyn Write) -> u8 {
    let bypass = std::env::var(BYPASS_ENV).is_ok_and(|v| v == "1");
    if bypass || std::io::stdin().is_terminal() {
        return 0;
    }
    let mut buf = Vec::new();
    // Read one byte past the cap so oversized input is detected.
    if std::io::stdin()
        .lock()
        .take(MAX_EVENT_BYTES + 1)
        .read_to_end(&mut buf)
        .is_err()
    {
        return 0;
    }
    if decide_bytes(&buf, false) == Decision::Deny {
        let _ = writeln!(out, "{}", deny_output());
    }
    0
}

fn settings_path(args: &HookInstallArgs) -> Result<PathBuf> {
    if let Some(p) = &args.settings {
        return Ok(p.clone());
    }
    match args.scope {
        HookScope::Project => Ok(PathBuf::from(".claude").join("settings.json")),
        HookScope::User => {
            let home = std::env::var_os("USERPROFILE")
                .filter(|v| !v.is_empty())
                .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()))
                .map(PathBuf::from)
                .ok_or_else(|| anyhow!("cannot find the home directory; pass --settings PATH"))?;
            Ok(home.join(".claude").join("settings.json"))
        }
    }
}

/// Entry point for `mcpdoze hook ...`; returns the exit code.
pub fn run(action: HookAction, out: &mut dyn Write) -> Result<u8> {
    match action {
        HookAction::ClaudeCode => Ok(run_check(out)),
        HookAction::Install(a) => edit(Op::Install, &a, out),
        HookAction::Remove(a) => edit(Op::Remove, &a, out),
    }
}

fn confirm(out: &mut dyn Write, question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    write!(out, "{question} [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn edit(op: Op, a: &HookInstallArgs, out: &mut dyn Write) -> Result<u8> {
    match a.agent {
        HookAgent::ClaudeCode => {}
    }
    let path = settings_path(a)?;
    let exe = std::env::current_exe()?;
    let command = install::command_line(&exe)?;
    let plan = install::plan(op, &path, &command)?;
    writeln!(out, "settings file: {}", path.display())?;
    if plan.new_text.is_none() {
        writeln!(
            out,
            "nothing to change ({}).",
            match op {
                Op::Install => "the hook is already installed with this path",
                Op::Remove => "the hook is not installed",
            }
        )?;
        return Ok(0);
    }
    for line in &plan.summary {
        writeln!(out, "  - {line}")?;
    }
    if plan.existed {
        writeln!(out, "  (the existing file will be backed up first)")?;
    } else {
        writeln!(out, "  (the file does not exist and will be created)")?;
    }
    if a.dry_run {
        writeln!(out, "dry run: nothing was written.")?;
        return Ok(0);
    }
    if !a.yes && !confirm(out, "Apply this change?")? {
        bail!("not applied: confirm interactively or pass --yes (use --dry-run to preview)");
    }
    let backup = install::apply(&plan)?;
    match backup {
        Some(b) => writeln!(out, "backup: {}", b.display())?,
        None => writeln!(out, "no backup (new file)")?,
    }
    writeln!(out, "written: {}", path.display())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_output_shape_and_no_input_text() {
        let ev = json!({"tool_name":"Bash","tool_input":{"command":"claude mcp add sekrit-token-123 -- x"}});
        assert_eq!(check_event(&ev), Decision::Deny);
        let s = deny_output().to_string();
        let v: Value = serde_json::from_str(&s).unwrap();
        let h = &v["hookSpecificOutput"];
        assert_eq!(h["hookEventName"], "PreToolUse");
        assert_eq!(h["permissionDecision"], "deny");
        assert!(h["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("mcpdoze add"));
        assert!(!s.contains("sekrit-token-123"));
    }

    #[test]
    fn malformed_oversized_and_bypass_allow() {
        assert_eq!(decide_bytes(b"{not json", false), Decision::Allow);
        assert_eq!(decide_bytes(b"", false), Decision::Allow);
        let big = vec![b' '; (MAX_EVENT_BYTES + 1) as usize];
        assert_eq!(decide_bytes(&big, false), Decision::Allow);
        let ev = br#"{"tool_name":"Bash","tool_input":{"command":"codex mcp add x -- y"}}"#;
        assert_eq!(decide_bytes(ev, false), Decision::Deny);
        assert_eq!(decide_bytes(ev, true), Decision::Allow);
    }
}
