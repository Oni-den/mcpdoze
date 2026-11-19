// SPDX-License-Identifier: GPL-3.0-or-later

//! Failure diagnostics, one test per kind the mock knobs can drive through
//! the real router. The kinds already covered by `diag_stdio.rs` (spawn
//! failure, crash in flight, stalled call, stdout pollution at trace) and by
//! `diag_http.rs` (HTTP status, auth, session, timeout, connect) are not
//! repeated here, except for the canary test, which must hold for both
//! transports.
//!
//! Kinds not driven here: `stalled` and `slow_call` (`diag_stdio.rs`, and
//! the timing-only `slow_call` needs a 30 s threshold), `list_cursor_loop`
//! (the mock does not paginate), `late_response` and `race_unload` /
//! `backoff_rejected` (no knob delays a reply past its own timeout or
//! races an unload), `sse_truncated` (the mock has no SSE truncation knob),
//! `http_non_mcp_body` and `tool_errors` `isError` (not needed for the
//! classification contract).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mcpdoze_core::backend::BackendSpec;
use mcpdoze_core::diag::{Diag, DiagConfig, Level};
use mcpdoze_core::router::Router;
use mcpdoze_testkit::{MockHttpSpec, MockServerSpec};
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const HTTP_BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-http");

const ENV_CANARY: &str = "canary-env-8842";
const ARG_CANARY: &str = "canary-arg-3317";
const CWD_CANARY: &str = "canary-cwd-6620";
const HDR_CANARY: &str = "canary-hdr-1184";
const QUERY_CANARY: &str = "canary-query-5519";

struct Rig {
    router: Arc<Router>,
    diag: Arc<Diag>,
    _dir: tempfile::TempDir,
}

fn rig(spec: BackendSpec, level: Level) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let diag = Diag::new(DiagConfig::new(dir.path().join("debug")));
    let router = Router::new();
    router.set_diag(diag.clone(), level);
    router.register("srv", "p", spec).unwrap();
    Rig {
        router,
        diag,
        _dir: dir,
    }
}

fn spec_with_start(mock: &MockServerSpec, start: Duration) -> BackendSpec {
    BackendSpec::new(BIN, mock.args()).start_timeout(start)
}

fn mock_spec(mock: &MockServerSpec) -> BackendSpec {
    spec_with_start(mock, Duration::from_secs(20))
}

fn tools_call(id: u64, tool: &str, args: Value) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": format!("p__{tool}"), "arguments": args}
    })
}

/// Writes a `--script` file into `dir` and returns its path.
fn script(dir: &Path, steps: Value) -> PathBuf {
    let p = dir.join("script.json");
    std::fs::write(&p, steps.to_string()).unwrap();
    p
}

impl Rig {
    async fn call(&self, tool: &str) -> Value {
        self.call_with(tool, json!({"text": "hi"})).await
    }

    async fn call_with(&self, tool: &str, args: Value) -> Value {
        let session = self.router.create_session("2025-11-25");
        tokio::time::timeout(
            Duration::from_secs(30),
            self.router.handle(&session, tools_call(1, tool, args)),
        )
        .await
        .expect("call timed out")
        .expect("response")
    }

    fn text(&self) -> String {
        assert!(self.diag.flush(Duration::from_secs(5)));
        std::fs::read_to_string(self.diag.file()).unwrap_or_default()
    }

    fn records(&self) -> Vec<Value> {
        self.text()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Waits until a record of `kind` exists.
    async fn wait_kind(&self, kind: &str) -> Vec<Value> {
        for _ in 0..100 {
            let hit: Vec<Value> = self
                .records()
                .into_iter()
                .filter(|r| r["kind"] == kind)
                .collect();
            if !hit.is_empty() {
                return hit;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("no {kind} record; got {:?}", self.records());
    }
}

// ------------------------------------------------------------ lifecycle

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_no_output_when_the_backend_is_silent_at_start() {
    let mock = MockServerSpec::new()
        .tools(&["echo"])
        .startup_delay_ms(4000);
    let r = rig(
        spec_with_start(&mock, Duration::from_millis(800)),
        Level::Failures,
    );
    let resp = r.call("echo").await;
    assert_eq!(resp["error"]["code"], -32000, "{resp}");
    let recs = r.wait_kind("init_no_output").await;
    assert_eq!(recs[0]["server"], "srv");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_timeout_when_stdout_started_but_no_reply() {
    let dir = tempfile::tempdir().unwrap();
    let steps = script(
        dir.path(),
        json!([{
            "on": {"method": "initialize"},
            "do": [{"stdout_raw": "booting the server"}, {"delay_ms": 4000}, "respond"]
        }]),
    );
    let mock = MockServerSpec::new().tools(&["echo"]).script(steps);
    let r = rig(
        spec_with_start(&mock, Duration::from_millis(800)),
        Level::Failures,
    );
    let resp = r.call("echo").await;
    assert_eq!(resp["error"]["code"], -32000, "{resp}");
    let recs = r.wait_kind("init_timeout").await;
    assert_eq!(recs[0]["server"], "srv");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_error_is_a_handshake_rpc_error() {
    let mock = MockServerSpec::new().tools(&["echo"]).init_error();
    let r = rig(mock_spec(&mock), Level::Failures);
    let resp = r.call("echo").await;
    assert_eq!(resp["error"]["code"], -32000, "{resp}");
    let recs = r.wait_kind("init_error").await;
    assert!(recs[0]["error"]["code"].is_number(), "{:?}", recs[0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_mismatch_names_the_returned_version() {
    let mock = MockServerSpec::new()
        .tools(&["echo"])
        .init_version("1999-01-01");
    let r = rig(mock_spec(&mock), Level::Failures);
    let resp = r.call("echo").await;
    // A different version is accepted and only recorded.
    assert!(resp.get("result").is_some(), "{resp}");
    let recs = r.wait_kind("version_mismatch").await;
    assert!(recs[0]["cause"]["hints"].to_string().contains("1999-01-01"));
}

// -------------------------------------------------------------- framing

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frame_oversize_is_recorded() {
    let mock = MockServerSpec::new().tools(&["echo"]).oversize_response();
    let r = rig(mock_spec(&mock), Level::Failures);
    let _ = r.call("echo").await;
    let recs = r.wait_kind("frame_oversize").await;
    assert_eq!(recs[0]["server"], "srv");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frame_invalid_is_json_that_is_not_json_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let steps = script(
        dir.path(),
        json!([{
            "on": {"method": "tools/call", "tool": "echo"},
            "do": [{"stdout_raw": "{\"hello\":1}"}, "respond"]
        }]),
    );
    let mock = MockServerSpec::new().tools(&["echo"]).script(steps);
    let r = rig(mock_spec(&mock), Level::Failures);
    let resp = r.call("echo").await;
    assert!(resp.get("result").is_some(), "{resp}");
    let recs = r.wait_kind("frame_invalid").await;
    assert_eq!(recs[0]["server"], "srv");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn id_type_mismatch_when_the_backend_echoes_string_ids() {
    let mock = MockServerSpec::new().tools(&["echo"]).string_ids();
    let r = rig(mock_spec(&mock), Level::Failures);
    let resp = r.call("echo").await;
    assert!(resp.get("result").is_some(), "{resp}");
    let recs = r.wait_kind("id_type_mismatch").await;
    assert_eq!(recs[0]["server"], "srv");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_length_framing_is_a_pollution_signature_at_trace() {
    let mock = MockServerSpec::new()
        .tools(&["echo"])
        .content_length_framing();
    let r = rig(mock_spec(&mock), Level::Trace);
    let resp = r.call("echo").await;
    assert!(resp.get("result").is_some(), "{resp}");
    let recs = r.wait_kind("stdout_pollution").await;
    let guess = recs[0]["cause"]["guess"].as_str().unwrap_or("");
    assert!(guess.contains("Content-Length"), "{guess}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pretty_json_is_a_pollution_signature_at_trace() {
    let dir = tempfile::tempdir().unwrap();
    // Pretty-printed replies from the first call on; the reply cannot be
    // read as one message, so the call stalls out at its call timeout.
    let steps = script(
        dir.path(),
        json!([{
            "on": {"method": "tools/call", "tool": "echo"},
            "do": ["pretty_json", "respond"]
        }]),
    );
    let mock = MockServerSpec::new().tools(&["echo"]).script(steps);
    let spec = mock_spec(&mock).call_timeout(Duration::from_millis(500));
    let r = rig(spec, Level::Trace);
    let resp = r.call("echo").await;
    assert_eq!(resp["error"]["code"], -32000, "{resp}");
    let recs = r.wait_kind("stdout_pollution").await;
    let hit = recs.iter().any(|x| {
        x["cause"]["guess"]
            .as_str()
            .is_some_and(|g| g.contains("several lines"))
    });
    assert!(hit, "{recs:?}");
}

// ------------------------------------------------------- JSON-RPC errors

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_error_from_the_backend_is_recorded_with_its_code() {
    // The router routes any name with a known prefix; the mock rejects a
    // tool it does not serve with -32602.
    let mock = MockServerSpec::new().tools(&["echo"]);
    let r = rig(mock_spec(&mock), Level::Failures);
    let resp = r.call("nosuch").await;
    assert_eq!(resp["error"]["code"], -32602, "{resp}");
    let recs = r.wait_kind("rpc_error").await;
    assert_eq!(recs[0]["error"]["code"], -32602, "{:?}", recs[0]);
}

// ------------------------------------------------------- agent side

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_cancel_is_recorded_at_trace() {
    let mock = MockServerSpec::new()
        .tools(&["slow", "echo"])
        .delay_ms(5000);
    let r = rig(mock_spec(&mock), Level::Trace);
    let session = r.router.create_session("2025-11-25");
    // Start the backend first, so the slow call is in flight well before the
    // cancel or the drop below (a cold start can outlast the sleep).
    let warm = r
        .router
        .handle(&session, tools_call(1, "echo", json!({})))
        .await;
    assert!(warm.is_some());
    let router = r.router.clone();
    let s2 = session.clone();
    let call =
        tokio::spawn(async move { router.handle(&s2, tools_call(7, "slow", json!({}))).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cancel = json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 7, "reason": "test"}
    });
    assert!(r.router.handle(&session, cancel).await.is_none());
    let resp = tokio::time::timeout(Duration::from_secs(10), call)
        .await
        .expect("cancelled call returned")
        .unwrap();
    assert!(resp.is_some(), "a cancelled call still gets an answer");
    r.wait_kind("agent_cancelled").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_disconnect_is_recorded_at_trace() {
    let mock = MockServerSpec::new()
        .tools(&["slow", "echo"])
        .delay_ms(5000);
    let r = rig(mock_spec(&mock), Level::Trace);
    let session = r.router.create_session("2025-11-25");
    // Start the backend first, so the slow call is in flight well before the
    // cancel or the drop below (a cold start can outlast the sleep).
    let warm = r
        .router
        .handle(&session, tools_call(1, "echo", json!({})))
        .await;
    assert!(warm.is_some());
    // Dropping the future is what an agent that hung up does.
    let dropped = tokio::time::timeout(
        Duration::from_millis(300),
        r.router.handle(&session, tools_call(3, "slow", json!({}))),
    )
    .await;
    assert!(dropped.is_err(), "the call should still be in flight");
    r.wait_kind("agent_disconnected").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_request_nobody_serves_is_recorded() {
    let mock = MockServerSpec::new().tools(&["ask"]).sampling_request();
    let r = rig(mock_spec(&mock), Level::Failures);
    let _ = r.call("ask").await;
    let recs = r.wait_kind("client_request_unserved").await;
    assert!(
        recs[0].to_string().contains("sampling/createMessage"),
        "{:?}",
        recs[0]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_timeout_is_a_list_failed_record() {
    // The router's list timeout is 30 s; the reply comes after it.
    let dir = tempfile::tempdir().unwrap();
    let steps = script(
        dir.path(),
        json!([{
            "on": {"method": "tools/list"},
            "do": [{"delay_ms": 31000}, "respond"]
        }]),
    );
    let mock = MockServerSpec::new().tools(&["echo"]).script(steps);
    let r = rig(mock_spec(&mock), Level::Failures);
    let session = r.router.create_session("2025-11-25");
    let _ = tokio::time::timeout(Duration::from_secs(60), r.router.list_tools(&session))
        .await
        .expect("list finished");
    let recs = r.wait_kind("list_failed").await;
    assert_eq!(recs[0]["server"], "srv");
}

// ------------------------------------------------------ canary and off

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canaries_planted_in_env_args_cwd_and_headers_never_reach_the_log() {
    // stdio: env value, call arguments and working directory; --stderr-leak
    // makes the backend print all of them to stderr, which the records carry.
    let parent = tempfile::tempdir().unwrap();
    let cwd = parent.path().join(CWD_CANARY);
    std::fs::create_dir_all(&cwd).unwrap();
    let mock = MockServerSpec::new()
        .tools(&["echo", "exit"])
        .exit_code(7)
        .stderr_leak();
    let mut spec = mock_spec(&mock);
    spec.env
        .insert("MCPDOZE_TEST_SECRET".into(), ENV_CANARY.into());
    spec.cwd = Some(cwd);
    let r = rig(spec, Level::Failures);
    let resp = r.call_with("exit", json!({"text": ARG_CANARY})).await;
    assert_eq!(resp["error"]["code"], -32000, "{resp}");
    let crashes = r.wait_kind("crash_in_flight").await;
    assert!(
        crashes[0]["stderr"].is_object(),
        "no stderr attached: {crashes:?}"
    );
    let text = r.text();
    for c in [ENV_CANARY, ARG_CANARY, CWD_CANARY] {
        assert!(!text.contains(c), "canary {c} leaked:\n{text}");
    }

    // http: header value and URL query.
    let srv = MockHttpSpec::new()
        .html_error()
        .program(HTTP_BIN)
        .spawn()
        .await
        .expect("mock-mcp-http");
    let http = BackendSpec::http(format!("{}?k={QUERY_CANARY}", srv.url()))
        .header("authorization", format!("Bearer {HDR_CANARY}"));
    let h = rig(http, Level::Failures);
    let _ = h.call("echo").await;
    h.wait_kind("http_status").await;
    let text = h.text();
    for c in [HDR_CANARY, QUERY_CANARY] {
        assert!(!text.contains(c), "canary {c} leaked:\n{text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_default_config_records_nothing_and_creates_nothing() {
    // No recorder at all (the default): a failing call leaves no trace.
    let mock = MockServerSpec::new().tools(&["exit"]).exit_code(7);
    let bare = Router::new();
    bare.register("srv", "p", mock_spec(&mock)).unwrap();
    let session = bare.create_session("2025-11-25");
    let resp = tokio::time::timeout(
        Duration::from_secs(30),
        bare.handle(&session, tools_call(1, "exit", json!({}))),
    )
    .await
    .expect("call timed out");
    assert_eq!(resp.unwrap()["error"]["code"], -32000);

    // A recorder present but at the default level (off).
    let r = rig(mock_spec(&mock), Level::Off);
    let resp = r.call("exit").await;
    assert_eq!(resp["error"]["code"], -32000, "{resp}");
    assert!(r.diag.flush(Duration::from_secs(5)));
    assert!(!r.diag.dir().exists(), "debug directory was created");
    assert!(!r.diag.file().exists(), "failures file was created");
    let stats = r.diag.stats();
    assert_eq!(stats.active, 0, "{stats:?}");
    assert_eq!(stats.written, 0, "{stats:?}");
    assert_eq!(stats.dropped, 0, "{stats:?}");
}
