// SPDX-License-Identifier: GPL-3.0-or-later

//! Replays failure bundles (fixtures and a live export round trip) through an
//! in-process broker and checks the recorded `kind` reappears. This
//! reproduces the broker-visible symptom, not the server's bug.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mcpdoze_core::backend::BackendSpec;
use mcpdoze_core::diag::export::{build, ExportInput, ExportOptions};
use mcpdoze_core::diag::{Diag, DiagConfig, Level};
use mcpdoze_core::router::Router;
use mcpdoze_testkit::replay::{from_bundle, from_text};
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/failures")
        .join(format!("{name}.md"))
}

async fn replay_fixture(kind: &str) {
    let replay = from_bundle(fixture(kind)).unwrap();
    let sc = replay.scenario(kind).expect("scenario");
    let kinds = sc.run(BIN).await.unwrap();
    assert!(
        kinds.iter().any(|k| k == kind),
        "want {kind}, got {kinds:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_stdout_pollution() {
    replay_fixture("stdout_pollution").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_exit_unexpected() {
    replay_fixture("exit_unexpected").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_frame_oversize() {
    replay_fixture("frame_oversize").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_id_type_mismatch() {
    replay_fixture("id_type_mismatch").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_init_timeout() {
    replay_fixture("init_timeout").await;
}

#[test]
fn synthetic_arguments_match_shape() {
    let r = from_bundle(fixture("exit_unexpected")).unwrap();
    let a = r.scenarios()[0].synthetic_arguments();
    assert_eq!(a["k#11223344"].as_str().map(str::len), Some(12));
    assert_eq!(a["k#55667788"], json!(100));
}

/// Real failure -> failure log -> export -> replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_trip_through_export() {
    let dir = tempfile::tempdir().unwrap();
    let diag = Diag::new(DiagConfig::new(dir.path().to_path_buf()));
    let router = Router::new();
    router.set_diag(diag.clone(), Level::Failures);
    let mock = MockServerSpec::new().tools(&["echo", "exit"]).exit_code(7);
    let spec = BackendSpec::new(BIN, mock.args()).start_timeout(Duration::from_secs(20));
    router.register("srv", "p", spec).unwrap();
    let session = router.create_session("2025-11-25");
    let msg = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"p__exit","arguments":{"text":"hello world"}}});
    let resp = tokio::time::timeout(Duration::from_secs(30), router.handle(&session, msg))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp["error"]["code"], -32000, "{resp}");
    let _: &Arc<Router> = &router;

    let mut records = Vec::new();
    for _ in 0..100 {
        assert!(diag.flush(Duration::from_secs(5)));
        records = std::fs::read_to_string(diag.file())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect();
        if records.iter().any(|r| r["kind"] == "exit_unexpected") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let export = build(
        ExportInput {
            records,
            ..Default::default()
        },
        &ExportOptions::new(1_800_000_000_000),
    )
    .expect("export");

    let replay = from_text(&export.text).unwrap();
    let sc = replay.scenario("exit_unexpected").expect("exit scenario");
    let kinds = sc.run(BIN).await.unwrap();
    assert!(
        kinds.iter().any(|k| k == "exit_unexpected"),
        "got {kinds:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_init_no_output() {
    replay_fixture("init_no_output").await;
}
