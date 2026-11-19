// SPDX-License-Identifier: GPL-3.0-or-later

//! Failure diagnostics on stdio backends: spawn failure, exit, crash with a
//! call in flight, and stdout pollution, through the real router and the
//! mock server.

use std::sync::Arc;
use std::time::Duration;

use mcpdoze_core::backend::BackendSpec;
use mcpdoze_core::diag::{Diag, DiagConfig, Level};
use mcpdoze_core::router::Router;
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");

struct Rig {
    router: Arc<Router>,
    diag: Arc<Diag>,
    _dir: tempfile::TempDir,
}

fn rig(spec: BackendSpec, level: Level) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let diag = Diag::new(DiagConfig::new(dir.path().to_path_buf()));
    let router = Router::new();
    router.set_diag(diag.clone(), level);
    router.register("srv", "p", spec).unwrap();
    Rig {
        router,
        diag,
        _dir: dir,
    }
}

fn mock_spec(mock: &MockServerSpec) -> BackendSpec {
    BackendSpec::new(BIN, mock.args()).start_timeout(Duration::from_secs(20))
}

impl Rig {
    async fn call(&self, tool: &str) -> Value {
        let session = self.router.create_session("2025-11-25");
        let msg = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": format!("p__{tool}"), "arguments": {"text": "hi"}}
        });
        tokio::time::timeout(Duration::from_secs(30), self.router.handle(&session, msg))
            .await
            .expect("call timed out")
            .expect("response")
    }

    fn records(&self) -> Vec<Value> {
        assert!(self.diag.flush(Duration::from_secs(5)));
        std::fs::read_to_string(self.diag.file())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Waits until a record of `kind` exists.
    async fn wait_kind(&self, kind: &str) -> Vec<Value> {
        for _ in 0..100 {
            let all = self.records();
            let hit: Vec<Value> = all.into_iter().filter(|r| r["kind"] == kind).collect();
            if !hit.is_empty() {
                return hit;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("no {kind} record; got {:?}", self.records());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_failed_is_recorded() {
    let spec = BackendSpec::new("mcpdoze-test-no-such-program-xyz", Vec::<String>::new());
    let r = rig(spec, Level::Failures);
    let resp = r.call("echo").await;
    assert_eq!(resp["error"]["code"], -32000, "{resp}");
    let recs = r.wait_kind("spawn_failed").await;
    assert_eq!(recs[0]["server"], "srv");
    assert_eq!(recs[0]["transport"], "stdio");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_records_nothing() {
    let spec = BackendSpec::new("mcpdoze-test-no-such-program-xyz", Vec::<String>::new());
    let r = rig(spec, Level::Off);
    let _ = r.call("echo").await;
    assert!(r.records().is_empty());
    assert!(!r.diag.file().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_with_call_in_flight_records_crash_and_exit() {
    let mock = MockServerSpec::new().tools(&["echo", "exit"]).exit_code(7);
    let r = rig(mock_spec(&mock), Level::Failures);
    let resp = r.call("exit").await;
    assert_eq!(resp["error"]["code"], -32000, "{resp}");

    let crashes = r.wait_kind("crash_in_flight").await;
    assert_eq!(crashes.len(), 1, "{crashes:?}");
    let c = &crashes[0];
    assert_eq!(c["request"]["tool"], "exit");
    assert_eq!(c["request"]["suspect"], true);
    assert!(c["incident"].as_str().is_some_and(|i| i.starts_with("i-")));
    assert_eq!(c["exit"]["code"], 7, "{c}");

    let exits = r.wait_kind("exit_unexpected").await;
    assert_eq!(exits[0]["exit"]["code"], 7, "{:?}", exits[0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdout_pollution_is_recorded_at_trace() {
    let mock = MockServerSpec::new().tools(&["echo"]).garbage_on_start();
    let r = rig(mock_spec(&mock), Level::Trace);
    let resp = r.call("echo").await;
    assert!(resp.get("result").is_some(), "{resp}");
    let recs = r.wait_kind("stdout_pollution").await;
    assert!(recs[0]["backend"]["stdout_noise"].as_u64().unwrap_or(0) >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdout_pollution_is_not_recorded_at_failures() {
    let mock = MockServerSpec::new().tools(&["echo"]).garbage_on_start();
    let r = rig(mock_spec(&mock), Level::Failures);
    let resp = r.call("echo").await;
    assert!(resp.get("result").is_some(), "{resp}");
    assert!(r.records().iter().all(|x| x["kind"] != "stdout_pollution"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_timeout_returns_error_and_records_stalled() {
    let mock = MockServerSpec::new().tools(&["slow"]).delay_ms(5000);
    let spec = mock_spec(&mock).call_timeout(Duration::from_millis(300));
    let r = rig(spec, Level::Failures);
    let resp = r.call("slow").await;
    assert_eq!(resp["error"]["code"], -32000, "{resp}");
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("call_timeout"));
    let recs = r.wait_kind("stalled").await;
    assert_eq!(recs[0]["request"]["tool"], "slow");
}
