// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdoze hook install|remove`: edit a Claude Code `settings.json`.
//!
//! Only our own `hooks.PreToolUse` entry is touched (recognised by a command
//! ending in `hook claude-code`). Everything else, including key order, is
//! preserved. The file is backed up before it is written and written
//! atomically (temp file + rename). Invalid JSON is never overwritten.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

/// Suffix that identifies our hook command.
const OUR_SUFFIX: &str = "hook claude-code";
/// Tool matcher of our entry.
pub const MATCHER: &str = "Bash|Write|Edit|MultiEdit";
/// Largest settings file we will read.
const MAX_SETTINGS_BYTES: u64 = 4 * 1024 * 1024;

/// What to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Add or update our entry.
    Install,
    /// Delete our entry.
    Remove,
}

/// A computed change, not yet written.
#[derive(Debug)]
pub struct Plan {
    /// Settings file.
    pub path: PathBuf,
    /// Whether the file exists now.
    pub existed: bool,
    /// New file text, or `None` when nothing changes.
    pub new_text: Option<String>,
    /// Human-readable description of the change.
    pub summary: Vec<String>,
}

/// The hook command line for an executable path: quoted, forward slashes.
pub fn command_line(exe: &Path) -> Result<String> {
    let s = exe.to_string_lossy().replace('\\', "/");
    if s.contains('"') {
        bail!("the executable path contains a quote character");
    }
    Ok(format!("\"{s}\" {OUR_SUFFIX}"))
}

fn is_ours(cmd: &str) -> bool {
    cmd.trim_end().ends_with(OUR_SUFFIX)
}

fn our_hooks_in(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|h| {
            h.iter().any(|x| {
                x.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_ours)
            })
        })
}

fn new_entry(command: &str) -> Value {
    json!({"matcher": MATCHER, "hooks": [{"type": "command", "command": command}]})
}

/// Applies `op` to parsed settings. Returns descriptions of the changes
/// (empty when already in the desired state).
pub fn apply_op(root: &mut Value, op: Op, command: &str) -> Result<Vec<String>> {
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings root is not a JSON object"))?;
    let mut changes = Vec::new();
    match op {
        Op::Install => {
            let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
            let hooks = hooks
                .as_object_mut()
                .ok_or_else(|| anyhow!("`hooks` is not an object"))?;
            let pre = hooks.entry("PreToolUse").or_insert_with(|| json!([]));
            let pre = pre
                .as_array_mut()
                .ok_or_else(|| anyhow!("`hooks.PreToolUse` is not an array"))?;
            let mut found = false;
            for entry in pre.iter_mut().filter(|e| our_hooks_in(e)) {
                found = true;
                if entry.get("matcher").and_then(Value::as_str) != Some(MATCHER) {
                    entry["matcher"] = json!(MATCHER);
                    changes.push(format!("updated matcher to {MATCHER}"));
                }
                if let Some(hs) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                    for h in hs.iter_mut() {
                        let cur = h.get("command").and_then(Value::as_str).unwrap_or("");
                        if is_ours(cur) && cur != command {
                            h["command"] = json!(command);
                            changes.push(format!("updated hook command to {command}"));
                        }
                    }
                }
            }
            if !found {
                pre.push(new_entry(command));
                changes.push(format!(
                    "added hooks.PreToolUse entry (matcher {MATCHER}, command {command})"
                ));
            }
        }
        Op::Remove => {
            let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) else {
                return Ok(changes);
            };
            let mut emptied_pre = false;
            if let Some(pre) = hooks.get_mut("PreToolUse").and_then(Value::as_array_mut) {
                let mut kept = Vec::with_capacity(pre.len());
                for mut entry in std::mem::take(pre) {
                    if !our_hooks_in(&entry) {
                        kept.push(entry);
                        continue;
                    }
                    if let Some(hs) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                        hs.retain(|h| {
                            !h.get("command")
                                .and_then(Value::as_str)
                                .is_some_and(is_ours)
                        });
                        changes.push("removed the MCPDoze hook command".to_owned());
                        if hs.is_empty() {
                            changes.push("removed the now-empty PreToolUse entry".to_owned());
                            continue;
                        }
                    }
                    kept.push(entry);
                }
                *pre = kept;
                emptied_pre = pre.is_empty() && !changes.is_empty();
            }
            if emptied_pre {
                hooks.remove("PreToolUse");
                changes.push("removed the now-empty hooks.PreToolUse".to_owned());
                if hooks.is_empty() {
                    obj.remove("hooks");
                    changes.push("removed the now-empty hooks".to_owned());
                }
            }
        }
    }
    Ok(changes)
}

/// Reads the file (if any) and computes the change.
pub fn plan(op: Op, path: &Path, command: &str) -> Result<Plan> {
    let existed = path.exists();
    let mut root = if existed {
        let meta = std::fs::metadata(path)?;
        if meta.len() > MAX_SETTINGS_BYTES {
            bail!(
                "{} is larger than 4 MiB; refusing to edit it",
                path.display()
            );
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if text.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str::<Value>(&text).map_err(|e| {
                anyhow!(
                    "{} is not valid JSON ({e}); it was not changed. Fix or move the file, then retry",
                    path.display()
                )
            })?
        }
    } else {
        json!({})
    };
    let changes = apply_op(&mut root, op, command)
        .with_context(|| format!("unexpected structure in {}", path.display()))?;
    let new_text = if changes.is_empty() {
        None
    } else {
        Some(format!("{}\n", serde_json::to_string_pretty(&root)?))
    };
    Ok(Plan {
        path: path.to_owned(),
        existed,
        new_text,
        summary: changes,
    })
}

/// Writes the plan: backup (if the file exists), then atomic replace.
/// Returns the backup path, if one was made.
pub fn apply(plan: &Plan) -> Result<Option<PathBuf>> {
    let Some(text) = &plan.new_text else {
        return Ok(None);
    };
    let path = &plan.path;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let backup = if plan.existed {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".mcpdoze-bak-{}", utc_stamp(SystemTime::now())));
        let b = PathBuf::from(name);
        std::fs::copy(path, &b).with_context(|| format!("backing up to {}", b.display()))?;
        Some(b)
    } else {
        None
    };
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".mcpdoze-tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp_name);
    let res = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow!("writing {}: {e}", path.display()));
    }
    Ok(backup)
}

/// `YYYYMMDDTHHMMSSZ` in UTC.
pub fn utc_stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
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
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "\"/opt/x/mcpdoze\" hook claude-code";

    fn plan_in(dir: &Path, op: Op, cmd: &str) -> Plan {
        plan(op, &dir.join("settings.json"), cmd).unwrap()
    }

    #[test]
    fn stamp_known_value() {
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(utc_stamp(t), "20231114T221320Z");
    }

    #[test]
    fn fresh_file_install() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join(".claude");
        let p = plan_in(&sub, Op::Install, CMD);
        assert!(!p.existed);
        assert!(apply(&p).unwrap().is_none());
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(sub.join("settings.json")).unwrap())
                .unwrap();
        let e = &v["hooks"]["PreToolUse"][0];
        assert_eq!(e["matcher"], MATCHER);
        assert_eq!(e["hooks"][0]["type"], "command");
        assert_eq!(e["hooks"][0]["command"], CMD);
    }

    #[test]
    fn preserves_other_settings_and_order() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("settings.json");
        std::fs::write(
            &f,
            r#"{"zeta":1,"hooks":{"PostToolUse":[{"matcher":"X","hooks":[]}],"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"other.sh"}]}]},"alpha":2}"#,
        )
        .unwrap();
        let p = plan(Op::Install, &f, CMD).unwrap();
        let bak = apply(&p).unwrap().expect("backup");
        assert!(bak.to_string_lossy().contains(".mcpdoze-bak-"));
        assert!(std::fs::read_to_string(&bak).unwrap().contains("other.sh"));
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.find("zeta").unwrap() < text.find("hooks").unwrap());
        assert!(text.find("hooks").unwrap() < text.find("alpha").unwrap());
        let v: Value = serde_json::from_str(&text).unwrap();
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["hooks"][0]["command"], "other.sh");
        assert_eq!(pre[1]["hooks"][0]["command"], CMD);
        assert!(v["hooks"]["PostToolUse"].is_array());
    }

    #[test]
    fn reinstall_updates_path_without_duplicate() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("settings.json");
        apply(&plan(Op::Install, &f, CMD).unwrap()).unwrap();
        let same = plan(Op::Install, &f, CMD).unwrap();
        assert!(same.new_text.is_none(), "identical install is a no-op");
        let new_cmd = "\"/new/place/mcpdoze\" hook claude-code";
        let p = plan(Op::Install, &f, new_cmd).unwrap();
        assert!(p.new_text.is_some());
        apply(&p).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["hooks"][0]["command"], new_cmd);
    }

    #[test]
    fn remove_only_ours_and_cleans_empties() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("settings.json");
        std::fs::write(
            &f,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"other.sh"}]}]},"x":1}"#,
        )
        .unwrap();
        apply(&plan(Op::Install, &f, CMD).unwrap()).unwrap();
        apply(&plan(Op::Remove, &f, CMD).unwrap()).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["hooks"][0]["command"], "other.sh");
        assert_eq!(v["x"], 1);

        // Fresh install then remove leaves no empty containers.
        let g = d.path().join("only.json");
        apply(&plan(Op::Install, &g, CMD).unwrap()).unwrap();
        apply(&plan(Op::Remove, &g, CMD).unwrap()).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&g).unwrap()).unwrap();
        assert_eq!(v, json!({}));
        // Removing again is a no-op.
        assert!(plan(Op::Remove, &g, CMD).unwrap().new_text.is_none());
    }

    #[test]
    fn remove_missing_file_is_noop() {
        let d = tempfile::tempdir().unwrap();
        let p = plan_in(d.path(), Op::Remove, CMD);
        assert!(p.new_text.is_none() && !p.existed);
        assert!(!d.path().join("settings.json").exists());
    }

    #[test]
    fn invalid_json_refused_and_untouched() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("settings.json");
        std::fs::write(&f, "{ not json").unwrap();
        let e = plan(Op::Install, &f, CMD).unwrap_err().to_string();
        assert!(e.contains("not valid JSON"), "{e}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{ not json");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn wrong_shapes_refused() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("settings.json");
        for body in ["[]", r#"{"hooks":[]}"#, r#"{"hooks":{"PreToolUse":{}}}"#] {
            std::fs::write(&f, body).unwrap();
            assert!(plan(Op::Install, &f, CMD).is_err(), "{body}");
        }
    }

    #[test]
    fn dry_run_plan_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("settings.json");
        let p = plan(Op::Install, &f, CMD).unwrap();
        assert!(p.new_text.is_some() && !p.summary.is_empty());
        assert!(!f.exists());
    }

    #[test]
    fn command_line_is_quoted_with_forward_slashes() {
        let c = command_line(Path::new("C:\\Program Files\\mcpdoze\\mcpdoze.exe")).unwrap();
        assert_eq!(
            c,
            "\"C:/Program Files/mcpdoze/mcpdoze.exe\" hook claude-code"
        );
    }
}
