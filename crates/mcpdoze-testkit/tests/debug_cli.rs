// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdoze debug export` against records written by the real router: the
//! canaries planted in env, arguments and working directory must not appear
//! in the export, the export passes its own self-check, and `strict` has no
//! free text. The export works from the files alone (no broker running).

use std::sync::Arc;
use std::time::Duration;

use mcpdoze_core::backend::BackendSpec;
use mcpdoze_core::diag::export::{
    build, read_records, ExportInput, ExportOptions, Filter, LaunchShape, MAX_RECORDS,
};
use mcpdoze_core::diag::{Diag, DiagConfig, Level};
use mcpdoze_core::router::Router;
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const ENV_CANARY: &str = "canary-env-5530";
const ARG_CANARY: &str = "canary-arg-5531";
const CWD_CANARY: &str = "canary-cwd-5532";

struct Run {
    diag: Arc<Diag>,
    _dir: tempfile::TempDir,
}

async fn crashing_run() -> Run {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join(CWD_CANARY);
    std::fs::create_dir_all(&cwd).unwrap();
    let diag = Diag::new(DiagConfig::new(dir.path().join("debug")));
    let router = Router::new();
    router.set_diag(diag.clone(), Level::Failures);
    let mock = MockServerSpec::new()
        .tools(&["echo", "exit"])
        .exit_code(7)
        .stderr_leak();
    let mut spec = BackendSpec::new(BIN, mock.args()).start_timeout(Duration::from_secs(20));
    spec.env
        .insert("MCPDOZE_TEST_SECRET".into(), ENV_CANARY.into());
    spec.cwd = Some(cwd);
    router.register("srv", "p", spec).unwrap();
    let session = router.create_session("2025-11-25");
    let msg = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "p__exit", "arguments": {"text": ARG_CANARY}}
    });
    let resp = tokio::time::timeout(Duration::from_secs(30), router.handle(&session, msg))
        .await
        .expect("call timed out")
        .expect("response");
    assert_eq!(resp["error"]["code"], -32000, "{resp}");
    assert!(diag.flush(Duration::from_secs(5)));
    Run { diag, _dir: dir }
}

fn read(run: &Run) -> Vec<Value> {
    read_records(&run.diag.file(), 5, &Filter::default(), MAX_RECORDS)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_of_a_canary_run_passes_the_self_check_and_leaks_nothing() {
    let run = crashing_run().await;
    let records = read(&run);
    assert!(
        records.iter().any(|r| r["kind"] == "crash_in_flight"),
        "{records:?}"
    );
    let cfg: mcpdoze_core::config::ServerConfig = serde_json::from_value(json!({
        "transport": "stdio",
        "command": "mock",
        "args": [format!("--flag={ARG_CANARY}")],
        "env": {"SECRET": ENV_CANARY},
    }))
    .unwrap();
    let shape = LaunchShape::from_config("srv", &cfg);
    let opts = ExportOptions::new(mcpdoze_core::stats::now_ms());
    let e = build(
        ExportInput {
            records: records.clone(),
            servers: vec![shape.clone()],
            stats: None,
            broker_running: false,
        },
        &opts,
    )
    .expect("export must pass its own self-check");
    for c in [ENV_CANARY, ARG_CANARY, CWD_CANARY] {
        assert!(!e.text.contains(c), "canary {c} leaked:\n{}", e.text);
    }
    assert!(e.text.contains("## Manifest"));
    assert!(e.text.contains("\"env_names\": [\n      \"SECRET\"\n    ]"));
    assert!(e.text.contains("crash_in_flight"));
    assert!(e.text.contains("## Replay"));

    // Strict: no free text at all.
    let mut strict = ExportOptions::new(mcpdoze_core::stats::now_ms());
    strict.strict = true;
    let s = build(
        ExportInput {
            records,
            servers: vec![shape],
            stats: None,
            broker_running: false,
        },
        &strict,
    )
    .expect("strict export");
    for key in ["\"message\"", "\"tail\"", "\"hints\""] {
        assert!(!s.text.contains(key), "strict export has {key}");
    }
    assert!(s.counts.dropped >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hashes_are_rekeyed_per_export() {
    let run = crashing_run().await;
    let records = read(&run);
    let a = build(
        ExportInput {
            records: records.clone(),
            ..Default::default()
        },
        &ExportOptions::new(1),
    )
    .unwrap();
    let b = build(
        ExportInput {
            records,
            ..Default::default()
        },
        &ExportOptions::new(1),
    )
    .unwrap();
    // Two exports use two random salts; hashed fields (the shape keys of the
    // arguments) differ whenever any were present.
    if a.counts.rekeyed > 0 {
        assert_ne!(a.text, b.text);
    }
}
