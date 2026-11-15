// SPDX-License-Identifier: GPL-3.0-or-later

//! Runs the real binary as a Claude Code hook: one deny, one allow, one
//! bypass. Uses only stdin/stdout; no settings file is touched.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run_hook(event: &str, bypass: bool) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mcpdoze"));
    cmd.args(["hook", "claude-code"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove("MCPDOZE_HOOK_BYPASS");
    if bypass {
        cmd.env("MCPDOZE_HOOK_BYPASS", "1");
    }
    let mut child = cmd.spawn().expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(event.as_bytes()).unwrap();
    drop(stdin);
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!("hook did not exit in time");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn deny_allow_and_bypass() {
    let deny =
        r#"{"tool_name":"Bash","tool_input":{"command":"claude mcp add foo -- npx -y bar"}}"#;
    let (code, out) = run_hook(deny, false);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(!out.contains("npx -y bar"));

    let allow = r#"{"tool_name":"Bash","tool_input":{"command":"claude mcp list"}}"#;
    assert_eq!(run_hook(allow, false), (0, String::new()));
    assert_eq!(run_hook("garbage", false), (0, String::new()));
    assert_eq!(run_hook(deny, true), (0, String::new()));
}
