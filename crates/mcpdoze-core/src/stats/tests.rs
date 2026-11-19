// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

const DAY: u64 = MS_PER_DAY;
/// 2026-03-15T00:00:00Z.
const MAR_15: u64 = 20_527 * DAY;

fn call(server: &str, ts: u64, ms: u64, ok: bool, cold: bool, session: u64) -> Event {
    Event::Call(CallEvent {
        ts,
        server: server.into(),
        kind: CallKind::Tool,
        name: "echo".into(),
        duration_ms: ms,
        ok,
        error_code: (!ok).then_some(-32000),
        fail: None,
        cold_start: cold,
        spawn_wait_ms: if cold { 100 } else { 0 },
        bytes_in: 10,
        bytes_out: 20,
        era: "legacy".into(),
        scope: "shared".into(),
        instance: None,
        req_id: None,
        acquire_ms: None,
        backend_ms: None,
        bytes_sent: None,
        opt: Vec::new(),
        cached: false,
        dup: false,
        session: Some(session),
    })
}

fn mem(private_mb: u64, rss_mb: u64, procs: usize) -> TreeMemory {
    TreeMemory {
        rss_bytes: rss_mb * 1024 * 1024,
        private_bytes: private_mb * 1024 * 1024,
        process_count: procs,
    }
}

#[test]
fn dates_and_file_names() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(20_527), (2026, 3, 15));
    for d in [-1000i64, 0, 59, 60, 10_957, 20_527, 40_000] {
        let (y, m, dd) = civil_from_days(d);
        assert_eq!(days_from_civil(y, m, dd), d);
    }
    assert_eq!(month_of(MAR_15), "2026-03");
    assert_eq!(file_name_for(MAR_15), "stats-2026-03.jsonl");
    assert_eq!(
        month_end_ms("stats-2026-12.jsonl"),
        Some(days_from_civil(2027, 1, 1) as u64 * DAY)
    );
    assert_eq!(month_end_ms("stats-2026-13.jsonl"), None);
    assert_eq!(month_end_ms("notes.txt"), None);
}

#[test]
fn call_event_shape() {
    let ev = call("example", MAR_15, 12, false, true, 42);
    let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["ev"], "call");
    assert_eq!(v["server"], "example");
    assert_eq!(v["kind"], "tool");
    assert_eq!(v["duration_ms"], 12);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error_code"], -32000);
    assert_eq!(v["cold_start"], true);
    assert_eq!(v["spawn_wait_ms"], 100);
    assert_eq!(v["era"], "legacy");
    // The in-memory session key and unset debug fields are not written.
    assert!(v.get("session").is_none(), "{v}");
    assert!(v.get("req_id").is_none(), "{v}");
    assert!(v.get("instance").is_none(), "{v}");
    let back: Event = serde_json::from_value(v).unwrap();
    let Event::Call(c) = back else { panic!() };
    assert_eq!(c.session, None);
    assert_eq!(c.duration_ms, 12);
}

#[test]
fn other_event_shapes() {
    let v = serde_json::to_value(Event::Unload(UnloadEvent {
        ts: 1,
        server: "s".into(),
        instance: Some(short_hash("/work/example")),
        reason: "idle".into(),
        uptime_s: 5,
        rss_mb: None,
        exit_code: None,
        signal: None,
        generation: 2,
    }))
    .unwrap();
    assert_eq!(v["ev"], "unload");
    assert_eq!(v["instance"].as_str().unwrap().len(), 12);
    assert!(v.get("rss_mb").is_none());
    let v = serde_json::to_value(Event::Session(SessionEvent {
        ts: 1,
        action: "open".into(),
        count: 3,
    }))
    .unwrap();
    assert_eq!(
        v,
        serde_json::json!({"ev":"session","ts":1,"action":"open","count":3})
    );
}

#[test]
fn hashes_are_short_stable_and_opaque() {
    let h = short_hash("/home/example/project");
    assert_eq!(h.len(), 12);
    assert_eq!(h, short_hash("/home/example/project"));
    assert_ne!(h, short_hash("/home/example/other"));
    assert!(!h.contains("example"));
    assert_ne!(session_key("a"), session_key("b"));
    assert_ne!(session_key(""), STATELESS_SESSION);
}

#[test]
fn sample_estimates_direct_memory() {
    let mut agg = Aggregator::default();
    // Two sessions call `a`, one calls `b`; `c` runs but is not called.
    for (server, s) in [("a", 1), ("a", 2), ("a", 1), ("b", 3)] {
        agg.on_event(&mut call(server, MAR_15, 1, true, false, s));
    }
    let input = SampleInput {
        broker: mem(10, 20, 1),
        sessions: 3,
        instances: vec![
            InstanceMeasure {
                server: "a".into(),
                instance: None,
                memory: mem(50, 60, 2),
            },
            InstanceMeasure {
                server: "b".into(),
                instance: None,
                memory: mem(30, 35, 1),
            },
            InstanceMeasure {
                server: "c".into(),
                instance: None,
                memory: mem(5, 6, 1),
            },
        ],
    };
    let s = agg.sample(input, MAR_15, Duration::from_secs(60), 7);
    assert_eq!(s.interval_s, 60);
    assert_eq!(s.running_instances, 3);
    assert_eq!(s.dropped, 7);
    let a = s.servers.iter().find(|x| x.server == "a").unwrap();
    assert_eq!((a.sessions, a.est_direct_mb, a.pids), (2, 100.0, 2));
    let b = s.servers.iter().find(|x| x.server == "b").unwrap();
    assert_eq!((b.sessions, b.est_direct_mb), (1, 30.0));
    let c = s.servers.iter().find(|x| x.server == "c").unwrap();
    assert_eq!((c.sessions, c.est_direct_mb), (0, 0.0));
    assert_eq!(s.est_direct_mb, 130.0);
    // broker 10 + backends 50 + 30 + 5
    assert_eq!(s.actual_mb, 95.0);
    assert_eq!(s.saved_mb, 35.0);

    // Next window: `a` unloaded but called again by one session; its last
    // measured size is used. The interval comes from the timestamps.
    agg.on_event(&mut call("a", MAR_15 + 1, 1, true, true, 9));
    let mut unload = Event::Unload(UnloadEvent {
        ts: MAR_15 + 2,
        server: "b".into(),
        instance: None,
        reason: "idle".into(),
        uptime_s: 30,
        rss_mb: None,
        exit_code: None,
        signal: None,
        generation: 1,
    });
    agg.on_event(&mut unload);
    let Event::Unload(u) = unload else { panic!() };
    assert_eq!(u.rss_mb, Some(35.0));
    let s2 = agg.sample(
        SampleInput {
            broker: mem(10, 20, 1),
            sessions: 1,
            instances: Vec::new(),
        },
        MAR_15 + 30_000,
        Duration::from_secs(60),
        0,
    );
    assert_eq!(s2.interval_s, 30);
    let a = s2.servers.iter().find(|x| x.server == "a").unwrap();
    assert_eq!((a.instances, a.sessions, a.est_direct_mb), (0, 1, 50.0));
    assert_eq!(s2.actual_mb, 10.0);
}

#[test]
fn summary_math() {
    let mut b = SummaryBuilder::new(MAR_15, MAR_15 + DAY);
    for (i, ms) in (1..=20u64).enumerate() {
        let ok = i % 10 != 0; // 2 errors
        let cold = i < 4; // 4 cold starts
        b.add(&call("a", MAR_15 + i as u64, ms, ok, cold, 1));
    }
    // Outside the period.
    b.add(&call("a", MAR_15 - 1, 9999, true, false, 1));
    b.add(&Event::Spawn(SpawnEvent {
        ts: MAR_15,
        server: "a".into(),
        instance: None,
        startup_ms: 80,
        generation: 1,
    }));
    for reason in ["idle", "idle", "crash"] {
        b.add(&Event::Unload(UnloadEvent {
            ts: MAR_15,
            server: "a".into(),
            instance: None,
            reason: reason.into(),
            uptime_s: 1,
            rss_mb: None,
            exit_code: None,
            signal: None,
            generation: 1,
        }));
    }
    b.add(&Event::Session(SessionEvent {
        ts: MAR_15,
        action: "open".into(),
        count: 1,
    }));
    for (est, actual, sessions) in [(100.0, 40.0, 2), (200.0, 60.0, 4)] {
        b.add(&Event::Sample(SampleEvent {
            ts: MAR_15 + 10,
            interval_s: 1800,
            broker_rss_mb: 0.0,
            broker_private_mb: 0.0,
            sessions,
            running_instances: 1,
            servers: Vec::new(),
            est_direct_mb: est,
            actual_mb: actual,
            saved_mb: est - actual,
            dropped: 3,
        }));
    }
    let s = b.finish();
    assert_eq!(s.events, 20 + 1 + 3 + 1 + 2);
    assert_eq!(s.sessions_opened, 1);
    assert_eq!(s.peak_sessions, 4);
    assert_eq!(s.dropped, 3);
    let a = &s.servers[0];
    assert_eq!(a.calls, 20);
    assert_eq!(a.errors, 2);
    assert_eq!(a.error_rate, 0.1);
    assert_eq!(a.cold_starts, 4);
    assert_eq!(a.cold_start_rate, 0.2);
    assert_eq!(a.p50_ms, 10);
    assert_eq!(a.p95_ms, 19);
    assert_eq!(a.spawns, 1);
    assert_eq!(a.unloads["idle"], 2);
    assert_eq!(a.unloads["crash"], 1);
    let m = &s.memory;
    assert_eq!(m.samples, 2);
    assert_eq!(m.avg_actual_mb, 50.0);
    assert_eq!(m.peak_actual_mb, 60.0);
    assert_eq!(m.avg_est_direct_mb, 150.0);
    assert_eq!(m.peak_est_direct_mb, 200.0);
    assert_eq!(m.avg_saved_mb, 100.0);
    // (60 + 140) MB over half an hour each.
    assert_eq!(m.mb_hours_saved, 100.0);
    assert_eq!(percentile(&[], 50.0), 0);
    assert_eq!(percentile(&[7], 95.0), 7);
}

#[test]
fn retention_prunes_whole_old_months() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "stats-2025-11.jsonl",
        "stats-2025-12.jsonl",
        "stats-2026-01.jsonl",
        "stats-2026-03.jsonl",
        "unrelated.txt",
    ] {
        fs::write(dir.path().join(name), "{}\n").unwrap();
    }
    // Now 2026-03-15, keep 60 days: cutoff 2026-01-14. December ended
    // 2026-01-01 (gone); January ends 2026-02-01 (kept).
    let removed = prune(dir.path(), 60, MAR_15).unwrap();
    assert_eq!(removed, ["stats-2025-11.jsonl", "stats-2025-12.jsonl"]);
    assert!(dir.path().join("stats-2026-01.jsonl").exists());
    assert!(dir.path().join("stats-2026-03.jsonl").exists());
    assert!(dir.path().join("unrelated.txt").exists());
}

#[test]
fn overflow_drops_and_counts() {
    // No writer thread: the queue fills up after two messages.
    let (tx, _rx) = mpsc::sync_channel(2);
    let stats = Stats {
        tx: Some(tx),
        thread: Mutex::new(None),
        dropped: Arc::new(AtomicU64::new(0)),
        dir: PathBuf::new(),
        debug: false,
        interval: Duration::from_secs(60),
        dups: DupTracker::default(),
    };
    for i in 0..5 {
        stats.record(call("a", i, 1, true, false, 1));
    }
    assert_eq!(stats.dropped(), 3);
    stats.sample(SampleInput::default());
    assert_eq!(stats.dropped(), 4);
    assert!(!stats.flush(Duration::from_millis(20)));
}

#[test]
fn writer_appends_jsonl_and_summarizes() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = StatsConfig::default();
    let stats = Stats::start(&cfg, dir.path().to_path_buf()).unwrap();
    let ts = now_ms();
    stats.record(call("a", ts, 5, true, true, 1));
    stats.record(call("a", ts, 7, true, false, 2));
    stats.sample(SampleInput {
        broker: mem(10, 12, 1),
        sessions: 2,
        instances: vec![InstanceMeasure {
            server: "a".into(),
            instance: None,
            memory: mem(40, 50, 1),
        }],
    });
    assert!(stats.flush(Duration::from_secs(5)));
    let text = fs::read_to_string(dir.path().join(file_name_for(ts))).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    let sample: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(sample["ev"], "sample");
    assert_eq!(sample["est_direct_mb"], 80.0);
    assert_eq!(sample["actual_mb"], 50.0);
    drop(stats);
    let s = summarize_dir(dir.path(), ts - 1000, now_ms() + 1000).unwrap();
    assert_eq!(s.servers[0].calls, 2);
    assert_eq!(s.memory.samples, 1);
    assert_eq!(s.memory.avg_saved_mb, 30.0);
    let empty = summarize_dir(&dir.path().join("missing"), 0, u64::MAX).unwrap();
    assert_eq!(empty.events, 0);
}

#[test]
fn json_len_counts_compact_bytes() {
    let v = serde_json::json!({"a":[1,2],"b":"x"});
    assert_eq!(json_len(&v), v.to_string().len() as u64);
}

fn tool_call(tool: &str, bytes_out: u64, dup: bool, sent: Option<u64>) -> Event {
    let Event::Call(mut c) = call("srv", MAR_15, 1, true, false, 1) else {
        unreachable!()
    };
    c.name = tool.into();
    c.bytes_out = bytes_out;
    c.dup = dup;
    c.bytes_sent = sent;
    Event::Call(c)
}

#[test]
fn tool_table_ranks_by_bytes() {
    let mut b = SummaryBuilder::new(MAR_15 - 1, MAR_15 + 1);
    for ev in [
        tool_call("small", 100, false, None),
        tool_call("small", 100, true, None),
        tool_call("big", 9000, false, Some(1000)),
        tool_call("big", 1000, false, None),
    ] {
        b.add(&ev);
    }
    let s = b.finish();
    assert_eq!(s.tools.len(), 2);
    let big = &s.tools[0];
    assert_eq!(
        (big.tool.as_str(), big.calls, big.total_bytes),
        ("big", 2, 10_000)
    );
    assert_eq!((big.avg_bytes_out, big.p95_bytes_out), (5000, 9000));
    assert_eq!(
        (big.est_tokens, big.bytes_saved, big.dup_calls),
        (2500, 8000, 0)
    );
    assert!((big.share - 0.980).abs() < 0.001);
    assert_eq!(s.tools[1].dup_calls, 1);
}

#[test]
fn old_call_lines_parse_and_new_fields_are_not_written_by_default() {
    let old = r#"{"ev":"call","ts":1,"server":"a","kind":"tool","name":"x","duration_ms":1,"ok":true,"cold_start":false,"spawn_wait_ms":0,"bytes_in":1,"bytes_out":2,"era":"legacy","scope":"shared"}"#;
    let Event::Call(c) = serde_json::from_str::<Event>(old).unwrap() else {
        panic!()
    };
    assert!(!c.dup && !c.cached && c.opt.is_empty() && c.bytes_sent.is_none());
    let line = serde_json::to_string(&Event::Call(c)).unwrap();
    assert!(!line.contains("dup") && !line.contains("cached") && !line.contains("opt"));
}

#[test]
fn dup_tracker_window() {
    let t = DupTracker::default();
    let h = call_hash("s", "t", &serde_json::json!({"a": 1}));
    assert!(!t.check(1, h));
    assert!(t.check(1, h));
    assert!(!t.check(2, h), "rings are per session");
    for i in 0..DUP_WINDOW as u64 {
        t.check(1, 1000 + i);
    }
    assert!(!t.check(1, h), "pushed out of the window");
}
