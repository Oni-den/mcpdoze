// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;

use serde_json::{json, Value};

use super::*;

fn cfg(dir: &Path) -> DiagConfig {
    DiagConfig::new(dir.join("debug"))
}

fn ident(server: &str) -> TapIdent {
    TapIdent {
        server: server.to_string(),
        transport: Transport::Stdio,
        launcher: Some("C:\\tools\\npx.cmd".to_string()),
        scope: Scope::Shared,
        instance_key: Some("<placeholder-cwd>".to_string()),
    }
}

fn read_lines(diag: &Diag) -> Vec<Value> {
    assert!(diag.flush(Duration::from_secs(5)));
    fs::read_to_string(diag.file())
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn off_mode_does_nothing_observable() {
    let dir = tempfile::tempdir().unwrap();
    let diag = Diag::new(cfg(dir.path()));
    let tap = diag.tap(
        ident("example"),
        Level::Off,
        Arc::new(ScrubContext::default()),
    );
    assert!(!tap.enabled());

    let args = json!({"query": "secret text", "n": 1});
    let req = {
        let mut r = RequestView::new(1, Era::Legacy, "tools/call");
        r.tool = Some("search");
        r.arguments = Some(&args);
        r
    };
    let be = BackendView::new("running", 1);
    let exit = ExitView {
        code: Some(1),
        signal: None,
    };

    tap.on_spawn_failed(io::ErrorKind::NotFound, true, &be);
    tap.on_init_timeout(Phase::InitSent, &be, None);
    tap.on_init_error("2025-11-25", None, None, &be, None);
    tap.on_exit(exit, &be, None);
    assert!(tap.new_incident(1).is_empty());
    tap.on_crash_in_flight("", exit, &req, &be, None);
    tap.on_transition("running", "spawn", 0);
    for _ in 0..100 {
        tap.on_frame(Dir::Out, FrameType::Request, "tools/call", 7, 100);
    }
    tap.on_stdout_noise(b"hello", &be);
    tap.on_frame_oversize(1, &be);
    tap.on_frame_invalid(3, &be);
    tap.on_id_type_mismatch(IdType::String, &be);
    tap.on_late_response(3, &be);
    tap.on_rpc_error(&req, -32602, "bad", None, &be, None);
    tap.on_tool_error(&req, Some("x"), &be, None);
    tap.on_stalled(&req, &be, None);
    tap.on_slow_call(&req, &be);
    tap.on_agent_cancelled(&req, &be);
    tap.on_agent_disconnected(&req, &be);
    tap.on_race_unload("acquire", &be);
    tap.on_backoff_rejected(&be);
    tap.on_http(Kind::HttpStatus, None, &HttpView::default(), &be);
    tap.on_client_request_unserved("sampling/createMessage", Unserved::Unsupported, &be);
    tap.on_list_failed(false, "tools/list", 5, 1, &be);

    // No allocation proxies: the rings never grew, no counter moved, no
    // directory, salt, file or writer thread exists.
    assert_eq!(tap.frames_held(), 0);
    assert_eq!(tap.frames.capacity(), 0);
    assert_eq!(tap.noise.load(Ordering::Relaxed), 0);
    assert_eq!(tap.incidents.load(Ordering::Relaxed), 0);
    assert!(tap.transitions.snapshot().is_empty());
    assert_eq!(
        diag.stats(),
        DiagStats {
            dropped: 0,
            written: 0,
            write_errors: 0,
            active: 0
        }
    );
    assert!(diag.tx.get().is_none());
    assert!(diag.salt.get().is_none());
    assert!(!diag.dir().exists());
    assert!(diag.flush(Duration::from_millis(10)));
    assert!(!dir.path().join("debug").exists());
}

#[test]
fn a_tap_without_a_recorder_cannot_be_enabled() {
    let tap = DiagTap::disabled();
    tap.set_level(Level::Trace);
    assert!(!tap.enabled());
}

#[test]
fn failure_record_end_to_end_holds_no_canary() {
    let dir = tempfile::tempdir().unwrap();
    let diag = Diag::new(cfg(dir.path()));
    let scrub = ScrubContext::new([("API_TOKEN".to_string(), "canary-env-value-123".to_string())])
        .with_home("/home/example")
        .with_user("example");
    let tap = diag.tap(ident("example"), Level::Failures, Arc::new(scrub));

    let schema = json!({
        "type": "object",
        "required": ["query"],
        "properties": {"query": {"type": "string"}, "cursor": {"type": "string"}}
    });
    let args = json!({
        "query": "canary-arg-value-456",
        "cursor": null,
        "canary-key-789": "x"
    });
    let mut req = RequestView::new(881, Era::Legacy, "tools/call");
    req.tool = Some("search");
    req.arguments = Some(&args);
    req.input_schema = Some(&schema);
    req.suspect = true;
    req.id_type = Some(IdType::Number);
    let mut be = BackendView::new("failed", 4);
    be.prev_state = Some("running");
    be.in_flight = 2;
    be.protocol = Some("2025-11-25");
    be.server_info = Some(("example-server", Some("1.2.3")));
    be.caps = &["tools", "resources.subscribe"];

    tap.on_transition("running", "spawn", 0);
    tap.on_frame(Dir::Out, FrameType::Request, "tools/call", 18, 2311);
    tap.on_stdout_noise(b"not json", &be); // trace only: no record at this level
    let stderr_lines = vec![
        "TypeError: canary-arg-value-456 is null for canary-env-value-123".to_string(),
        "    at /home/example/proj/index.js:41:17".to_string(),
        "starting up".to_string(),
    ];
    let stderr = StderrView {
        total: 3,
        lines: &stderr_lines,
    };
    let incident = tap.new_incident(4);
    assert!(incident.starts_with("i-4-"));
    tap.on_crash_in_flight(
        &incident,
        ExitView {
            code: Some(1),
            signal: None,
        },
        &req,
        &be,
        Some(&stderr),
    );

    let lines = read_lines(&diag);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let r = &lines[0];
    assert_eq!(r["v"], 1);
    assert_eq!(r["id"], "f-000001");
    assert_eq!(r["kind"], "crash_in_flight");
    assert_eq!(r["incident"], incident.as_str());
    assert_eq!(r["server"], "example");
    assert_eq!(r["launcher"], "npx");
    assert_eq!(r["generation"], 4);
    assert_eq!(r["backend"]["state"], "failed");
    assert_eq!(r["backend"]["server_info"]["name"], "example-server");
    assert_eq!(r["backend"]["stdout_noise"], 1);
    assert_eq!(r["request"]["req"], 881);
    assert_eq!(r["request"]["suspect"], true);
    assert_eq!(r["request"]["shape"]["cursor"]["t"], "null");
    assert_eq!(
        r["request"]["schema_diff"]["type_mismatch"][0],
        "/cursor: null, schema says string"
    );
    assert_eq!(r["exit"]["code"], 1);
    assert_eq!(r["error"]["code"], -32000);
    assert_eq!(
        r["cause"]["guess"],
        "server crashed on a null optional argument"
    );
    assert_eq!(r["stderr"]["total"], 3);
    assert_eq!(r["stderr"]["tail"][2], "<line len=11>");
    let frames = r["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["type"], "request");
    assert_eq!(frames[1]["sig"], "text");

    let text = fs::read_to_string(diag.file()).unwrap();
    for canary in [
        "canary-env-value-123",
        "canary-arg-value-456",
        "canary-key-789",
        "/home/example",
        "secret",
    ] {
        assert!(!text.contains(canary), "{canary} leaked: {text}");
    }
    assert!(text.contains("<env:API_TOKEN>") && text.contains("<arg:/query>"));
}

#[test]
fn trace_only_kinds_follow_the_level() {
    let dir = tempfile::tempdir().unwrap();
    let diag = Diag::new(cfg(dir.path()));
    let tap = diag.tap(
        ident("example"),
        Level::Failures,
        Arc::new(ScrubContext::default()),
    );
    let be = BackendView::new("running", 1);
    tap.on_stdout_noise(b"Content-Length: 5", &be);
    assert!(read_lines(&diag).is_empty());
    tap.set_level(Level::Trace);
    tap.on_stdout_noise(b"Content-Length: 5", &be);
    let lines = read_lines(&diag);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["kind"], "stdout_pollution");
    assert_eq!(
        lines[0]["cause"]["guess"],
        "server uses LSP-style Content-Length framing"
    );
    tap.set_level(Level::Off);
    tap.on_exit(ExitView::default(), &be, None);
    assert_eq!(read_lines(&diag).len(), 1);
}

#[cfg(unix)]
#[test]
fn files_and_directory_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let diag = Diag::new(cfg(dir.path()));
    let tap = diag.tap(
        ident("example"),
        Level::Failures,
        Arc::new(ScrubContext::default()),
    );
    tap.on_exit(ExitView::default(), &BackendView::new("failed", 1), None);
    read_lines(&diag);
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(diag.dir()), 0o700);
    assert_eq!(mode(&diag.file()), 0o600);
    assert_eq!(mode(&diag.dir().join(SALT_NAME)), 0o600);
}

fn rec(server: &str, kind: Kind, tool: &str, ts: u64) -> (Record, SigKey) {
    let ident = Identity {
        server: Name::new(server),
        transport: Transport::Stdio,
        launcher: None,
        scope: Scope::Shared,
        instance: None,
    };
    let r = Record::new(kind, ident, ts);
    let sig = SigKey {
        kind,
        tool: Some(tool.to_string()),
        shape: None,
        exit: (None, None),
    };
    (r, sig)
}

fn sink() -> Sink<Vec<u8>> {
    Sink::new(Vec::new(), 20, Arc::new(Counters::default()))
}

fn lines(s: &Sink<Vec<u8>>) -> Vec<Value> {
    String::from_utf8(s.out.clone())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn rate_limit_is_20_per_minute_per_server_then_one_suppressed_record() {
    let mut s = sink();
    let t0 = 1_767_225_600_000u64;
    for i in 0..30 {
        let (r, k) = rec("a", Kind::RpcError, &format!("tool{i}"), t0 + i);
        s.handle(r, k);
    }
    // Another server has its own budget.
    let (r, k) = rec("b", Kind::RpcError, "t", t0 + 40);
    s.handle(r, k);
    let l = lines(&s);
    assert_eq!(l.iter().filter(|v| v["server"] == "a").count(), 20);
    assert_eq!(l.iter().filter(|v| v["server"] == "b").count(), 1);

    // The next minute: the summary comes first, then the new record.
    let (r, k) = rec("a", Kind::RpcError, "next", t0 + 61_000);
    s.handle(r, k);
    let l = lines(&s);
    let sup: Vec<_> = l.iter().filter(|v| v["kind"] == "suppressed").collect();
    assert_eq!(sup.len(), 1);
    assert_eq!(sup[0]["suppressed"], 10);
    assert_eq!(sup[0]["server"], "a");
    let last = l.last().unwrap();
    assert_eq!(last["kind"], "rpc_error");
    assert_eq!(l[l.len() - 2]["kind"], "suppressed");
}

#[test]
fn pending_suppressed_count_is_written_on_flush() {
    let mut s = sink();
    let t0 = 1_767_225_600_000u64;
    for i in 0..25 {
        let (r, k) = rec("a", Kind::RpcError, &format!("t{i}"), t0);
        s.handle(r, k);
    }
    assert!(lines(&s).iter().all(|v| v["kind"] != "suppressed"));
    s.expire(true);
    let l = lines(&s);
    assert_eq!(l.last().unwrap()["kind"], "suppressed");
    assert_eq!(l.last().unwrap()["suppressed"], 5);
}

#[test]
fn identical_signatures_become_compact_repeat_lines() {
    let mut s = sink();
    let t0 = 1_767_225_600_000u64;
    for i in 0..4 {
        let (r, k) = rec("a", Kind::CrashInFlight, "search", t0 + i);
        s.handle(r, k);
    }
    let l = lines(&s);
    assert_eq!(l.len(), 4);
    assert_eq!(l[0]["id"], "f-000001");
    assert!(l[0].get("backend").is_none() || l[0]["broker"].is_object());
    for (i, v) in l[1..].iter().enumerate() {
        assert_eq!(v["of"], "f-000001");
        assert_eq!(v["repeat"], i + 2);
        assert!(v.get("broker").is_none(), "compact line: {v}");
    }
    // A different tool is a new signature.
    let (r, k) = rec("a", Kind::CrashInFlight, "other", t0 + 10);
    s.handle(r, k);
    assert_eq!(lines(&s).last().unwrap()["id"], "f-000002");
    // After the TTL the same signature is a full record again.
    let (r, k) = rec("a", Kind::CrashInFlight, "search", t0 + SIG_TTL_MS + 5);
    s.handle(r, k);
    assert_eq!(lines(&s).last().unwrap()["id"], "f-000003");
}

#[test]
fn the_log_rotates_by_size_and_keeps_n_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(dir.path());
    c.max_file_bytes = 2048;
    c.keep = 2;
    c.per_minute = 10_000;
    let diag = Diag::new(c);
    let tap = diag.tap(
        ident("example"),
        Level::Failures,
        Arc::new(ScrubContext::default()),
    );
    let be = BackendView::new("running", 1);
    for i in 0..60 {
        let mut req = RequestView::new(i, Era::Legacy, "tools/call");
        let tool = format!("tool{i}");
        req.tool = Some(&tool);
        tap.on_stalled(&req, &be, None);
    }
    assert!(diag.flush(Duration::from_secs(5)));
    let base = diag.file();
    assert!(base.exists());
    assert!(crate::logfile::rotated_path(&base, 1).exists());
    assert!(crate::logfile::rotated_path(&base, 2).exists());
    assert!(!crate::logfile::rotated_path(&base, 3).exists());
    assert!(fs::metadata(&base).unwrap().len() <= 2048 + 4096);
}

#[test]
fn a_full_queue_drops_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(dir.path());
    c.queue = 1;
    c.per_minute = 10_000;
    let diag = Diag::new(c);
    let tap = diag.tap(
        ident("example"),
        Level::Failures,
        Arc::new(ScrubContext::default()),
    );
    let be = BackendView::new("running", 1);
    for _ in 0..500 {
        tap.on_exit(ExitView::default(), &be, None);
    }
    assert!(diag.flush(Duration::from_secs(5)));
    // Every call was either queued (and written as a full or repeat line)
    // or dropped and counted; none vanished.
    let s = diag.stats();
    assert_eq!(s.written + s.dropped, 500, "{s:?}");
    assert_eq!(s.active, 500);
}

#[test]
fn http_records_hold_a_reduced_url_and_no_query() {
    let dir = tempfile::tempdir().unwrap();
    let diag = Diag::new(cfg(dir.path()));
    let tap = diag.tap(
        TapIdent {
            transport: Transport::Http,
            launcher: None,
            ..ident("example")
        },
        Level::Failures,
        Arc::new(ScrubContext::default()),
    );
    let be = BackendView::new("running", 1);
    let h = HttpView {
        url: Some("https://mcp.example.com/v1/mcp?token=canary-query-1"),
        status: Some(200),
        content_type: Some("text/html; charset=utf-8"),
        body_len: Some(512),
        body_class: Some(BodyClass::Html),
        www_authenticate: Some("Bearer realm=\"x\""),
        session_present: Some(true),
        ..HttpView::default()
    };
    tap.on_http(Kind::HttpNonMcpBody, None, &h, &be);
    let l = read_lines(&diag);
    assert_eq!(l.len(), 1);
    assert_eq!(l[0]["http"]["url"]["path_depth"], 2);
    assert_eq!(l[0]["http"]["content_type"], "text/html");
    assert_eq!(l[0]["http"]["www_authenticate"], "Bearer");
    assert_eq!(
        l[0]["cause"]["guess"],
        "URL is not the MCP endpoint, or a proxy or login page answered"
    );
    let text = fs::read_to_string(diag.file()).unwrap();
    assert!(!text.contains("canary-query-1") && !text.contains("mcp.example.com"));
}
