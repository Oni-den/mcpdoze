// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

const HOME: &str = concat!("/ho", "me/canaryuser");
const CANARY: &str = "canary-text-4471";

fn lint() -> LintContext {
    LintContext::default()
        .with_user("canaryuser")
        .with_host("canaryhost")
        .with_home(HOME)
}

fn opts() -> ExportOptions {
    let mut o = ExportOptions::new(1_800_000_000_000);
    o.scrub = ScrubContext::default()
        .with_home(HOME)
        .with_user("canaryuser")
        .with_host("canaryhost");
    o.lint = lint();
    o.salt = Salt::from_bytes([7; 32]);
    o
}

fn record(msg: &str) -> Value {
    json!({
        "v": 1, "ts": 1_799_999_000_000u64, "id": "f-000001", "kind": "exit_unexpected",
        "repeat": 3, "server": "example", "transport": "stdio", "launcher": "npx",
        "scope": "shared", "instance": "0a1b2c3d", "generation": 1,
        "backend": {"state": "failed", "protocol": "2025-11-25",
            "server_info": {"name": "example-server", "version": "1.0.0"}, "caps": ["tools"]},
        "request": {"req": 1, "method": "tools/call", "tool": "echo", "bytes": 80,
            "shape": {"k#11223344": {"t": "string", "len": 5}}},
        "exit": {"code": 1},
        "error": {"origin": "backend", "message": msg},
        "frames": [{"dt_ms": 0, "dir": "in", "type": "noise", "bytes": 12, "sig": "content_length"}],
        "stderr": {"total": 2, "tail": [msg, "<line len=20>"]},
        "cause": {"guess": "the server exited", "confidence": "low", "hints": [msg]},
        "http": {"url": {"scheme": "https", "host": "99887766", "path_depth": 2}},
        "broker": {"version": "0.3.0", "os": "windows", "arch": "x86_64"}
    })
}

fn input(msg: &str) -> ExportInput {
    ExportInput {
        records: vec![record(msg)],
        servers: Vec::new(),
        stats: None,
        broker_running: false,
    }
}

#[test]
fn self_check_rules() {
    let c = lint();
    let hit = |t: &str| !self_check(t, &c).is_empty();
    assert!(hit(concat!("see /ho", "me/someone/file")));
    assert!(hit(concat!("C:\\Us", "ers\\someone\\x")));
    assert!(!hit("/home/example/x and <home>/x and /Users/<user>"));
    assert!(hit(concat!("mail bob@corp-mail", ".org")));
    assert!(!hit("bot@users.noreply.github.com a@example.com"));
    assert!(hit(concat!("gh", "p_abcdefghijklmnopqrstuvwxyz")));
    assert!(hit("Authorization: Bearer abcdefgh12345678"));
    assert!(!hit("Bearer <redacted:12>"));
    assert!(hit(concat!("host 192.", "168.1.5 up")));
    assert!(!hit("version 1.2.3.4 and 8.8.8.8"));
    assert!(hit("user canaryuser ran"));
    assert!(!hit("canaryusers are fine"));
    assert!(hit("on CANARYHOST now"));
    assert!(hit(concat!("path /ho", "me/canaryuser")));
    assert!(hit(&"a".repeat(40)));
    assert!(!hit("f-000123 k#11223344 <user>"));
    let f = self_check(
        concat!(
            "ok
bob@corp-mail",
            ".org"
        ),
        &c,
    );
    assert_eq!(
        f,
        vec![Finding {
            rule: "email address",
            line: 2
        }]
    );
}

#[test]
fn clean_export_passes_and_rekeys() {
    let e = build(input("exit with code 1"), &opts()).unwrap();
    assert!(e.text.contains("## Manifest"));
    assert!(e.text.contains("## Replay"));
    assert!(!e.text.contains("0a1b2c3d"));
    assert!(!e.text.contains("99887766"));
    assert!(!e.text.contains("k#11223344"));
    assert!(e.text.contains("k#"));
    assert_eq!(e.counts.rekeyed, 3);
}

#[test]
fn rekey_is_consistent_and_salt_dependent() {
    let a = build(input("x"), &opts()).unwrap();
    let b = build(input("x"), &opts()).unwrap();
    assert_eq!(a.text, b.text);
    let mut o = opts();
    o.salt = Salt::from_bytes([9; 32]);
    let c = build(input("x"), &o).unwrap();
    assert_ne!(a.text, c.text);
}

#[test]
fn second_pass_scrubs_machine_names() {
    let e = build(input(&format!("failed in {HOME}/work/app.js")), &opts()).unwrap();
    assert!(!e.text.contains("canaryuser"));
    assert!(e.counts.rescrubbed >= 1);
}

#[test]
fn leak_in_a_non_scrubbed_field_fails_closed() {
    let mut i = input("x");
    i.records[0]["backend"]["server_info"]["name"] = json!("canaryuser");
    let err = build(i, &opts()).unwrap_err();
    let ExportError::SelfCheck(f) = &err;
    assert!(f.iter().any(|x| x.rule == "user name"));
    assert!(!err.to_string().contains("canaryuser"));
}

#[test]
fn strict_drops_all_free_text() {
    let mut o = opts();
    o.strict = true;
    let e = build(input(CANARY), &o).unwrap();
    assert!(!e.text.contains(CANARY));
    assert!(!e.text.contains("\"message\""));
    assert!(!e.text.contains("\"tail\""));
    assert!(!e.text.contains("\"hints\""));
    assert_eq!(e.counts.dropped, 4);
    assert!(e.text.contains("\"mode\": \"strict\""));
    let s = build(input(CANARY), &opts()).unwrap();
    assert!(s.text.contains(CANARY));
}

#[test]
fn strict_passes_where_text_would_fail() {
    // A stderr tail the second pass cannot reduce (a bare host name next to
    // a name the scrubber does not know) fails the standard export only.
    let mut i = input("x");
    i.records[0]["stderr"]["tail"] = json!(["note: owner is canaryuser"]);
    i.records[0]["error"]["message"] = json!("x");
    let mut o = opts();
    // The scrubber does not know the user, the self-check does.
    o.scrub = ScrubContext::default();
    let err = build(
        ExportInput {
            records: i.records.clone(),
            ..Default::default()
        },
        &o,
    )
    .unwrap_err();
    assert!(matches!(err, ExportError::SelfCheck(_)));
    o.strict = true;
    assert!(build(i, &o).is_ok());
}

fn server_config() -> ServerConfig {
    toml::from_str(
        r#"
transport = "stdio"
command = "C:\\tools\\npx.cmd"
args = ["-y", "@scope/pkg@1.2.3", "--token=canary-arg-3317", "--root", "/srv/canaryuser/x"]
[env]
API_KEY = "canary-env-8842"
"#,
    )
    .unwrap()
}

#[test]
fn launch_shape_has_names_only() {
    let l = LaunchShape::from_config("example", &server_config());
    assert_eq!(l.launcher.as_deref(), Some("npx"));
    assert_eq!(l.flags, vec!["-y", "--token", "--root"]);
    assert_eq!(l.arg_count, 5);
    assert_eq!(l.env_names, vec!["API_KEY"]);
    assert_eq!(l.package.as_deref(), Some("@scope/pkg@1.2.3"));
    let mut i = input("x");
    i.servers.push(l.clone());
    let plain = build(
        ExportInput {
            records: i.records.clone(),
            servers: i.servers.clone(),
            ..Default::default()
        },
        &opts(),
    )
    .unwrap();
    for c in [
        "canary-arg-3317",
        "canary-env-8842",
        "canaryuser",
        "@scope/pkg",
    ] {
        assert!(!plain.text.contains(c), "{c}");
    }
    let mut o = opts();
    o.identify = true;
    let id = build(i, &o).unwrap();
    assert!(id.text.contains("@scope/pkg@1.2.3"));
    assert!(!id.text.contains("canary-arg-3317"));
}

#[test]
fn replay_describes_a_mock_script() {
    let e = build(input("x"), &opts()).unwrap();
    assert!(e.text.contains("\"exit\": {"));
    assert!(e.text.contains("\"expected_kind\": \"exit_unexpected\""));
}

#[test]
fn reader_folds_repeats_and_filters() {
    let dir = std::env::temp_dir().join(format!("mcpdoze-export-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("failures.jsonl");
    let a = json!({"v":1,"id":"f-000001","server":"a","kind":"stalled","ts":10,"repeat":1});
    let b = json!({"v":1,"id":"f-000002","server":"b","kind":"stalled","ts":20,"repeat":1});
    let rep = json!({"v":1,"ts":30,"kind":"stalled","server":"a","repeat":4,"of":"f-000001"});
    std::fs::write(&file, format!("{a}\n{b}\n{rep}\nnot json\n")).unwrap();
    let all = read_records(&file, 2, &Filter::default(), 10);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["repeat"], 4);
    let f = Filter {
        server: Some("b".into()),
        ..Default::default()
    };
    assert_eq!(read_records(&file, 2, &f, 10).len(), 1);
    let f = Filter {
        since_ms: 15,
        ..Default::default()
    };
    assert_eq!(read_records(&file, 2, &f, 10)[0]["id"], "f-000002");
    assert_eq!(
        read_records(&file, 2, &Filter::default(), 1)[0]["id"],
        "f-000002"
    );
    assert!(parse_since("24h", 10_000_000).unwrap() < 10_000_000);
    let _ = std::fs::remove_dir_all(&dir);
}
