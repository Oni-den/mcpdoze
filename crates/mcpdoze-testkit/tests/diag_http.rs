// SPDX-License-Identifier: GPL-3.0-or-later

//! Failure diagnostics of the HTTP backend: the capture points in
//! `backend/http.rs` against the `mock-mcp-http` knobs. Records hold response
//! shapes only: the tests plant canaries in a header value and a URL query
//! and assert they never reach the log.

use std::sync::Arc;
use std::time::Duration;

use mcpdoze_core::backend::http::HttpConn;
use mcpdoze_core::backend::{BackendSpec, CallMeta, NoEvents};
use mcpdoze_core::diag::{
    Diag, DiagConfig, DiagTap, Level, Scope, ScrubContext, TapIdent, Transport,
};
use mcpdoze_testkit::{MockHttpServer, MockHttpSpec};
use serde_json::{json, Value};
use tokio::time::timeout;

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-http");
const HDR_CANARY: &str = "canary-header-7731";
const QUERY_CANARY: &str = "canary-query-5519";

struct Rig {
    diag: Arc<Diag>,
    conn: Arc<HttpConn>,
    _dir: tempfile::TempDir,
}

async fn mock(spec: MockHttpSpec) -> MockHttpServer {
    spec.program(BIN).spawn().await.expect("mock-mcp-http")
}

fn rig(url: &str, level: Level, timeout: Option<Duration>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let diag = Diag::new(DiagConfig::new(dir.path().join("debug")));
    let tap: Arc<DiagTap> = diag.tap(
        TapIdent {
            server: "remote".into(),
            transport: Transport::Http,
            launcher: None,
            scope: Scope::Shared,
            instance_key: None,
        },
        level,
        Arc::new(ScrubContext::default()),
    );
    let mut spec = BackendSpec::http(format!("{url}?k={QUERY_CANARY}"))
        .header("authorization", format!("Bearer {HDR_CANARY}"));
    if let Some(t) = timeout {
        spec = spec.request_timeout(t);
    }
    let conn = HttpConn::new("remote", 1, &spec, Arc::new(NoEvents)).unwrap();
    conn.set_diag(tap);
    Rig {
        diag,
        conn,
        _dir: dir,
    }
}

async fn call_echo(r: &Rig) -> Result<Value, String> {
    let meta = CallMeta {
        orig_id: json!(1),
        ..CallMeta::default()
    };
    let params = json!({"name": "echo", "arguments": {"text": "hi"}});
    timeout(
        Duration::from_secs(30),
        r.conn.call("tools/call", params, meta),
    )
    .await
    .expect("call exceeded its hard timeout")
    .map_err(|e| e.to_string())
}

fn records(r: &Rig) -> Vec<Value> {
    assert!(r.diag.flush(Duration::from_secs(5)));
    std::fs::read_to_string(r.diag.file())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn of_kind<'a>(recs: &'a [Value], kind: &str) -> Vec<&'a Value> {
    recs.iter().filter(|v| v["kind"] == kind).collect()
}

fn assert_no_canary(r: &Rig) {
    let text = std::fs::read_to_string(r.diag.file()).unwrap_or_default();
    assert!(
        !text.contains(HDR_CANARY) && !text.contains(QUERY_CANARY),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn html_error_page_is_a_status_record_with_html_shape() {
    let srv = mock(MockHttpSpec::new().html_error()).await;
    let r = rig(&srv.url(), Level::Failures, None);
    r.conn.initialize().await.unwrap();
    let err = call_echo(&r).await.unwrap_err();
    assert!(err.contains("HTTP 502"), "{err}");
    let recs = records(&r);
    let st = of_kind(&recs, "http_status");
    assert_eq!(st.len(), 1, "{recs:?}");
    let h = &st[0]["http"];
    assert_eq!(h["status"], 502);
    assert_eq!(h["content_type"], "text/html");
    assert_eq!(h["body_class"], "html");
    assert!(h["body_len"].as_u64().unwrap() > 0);
    assert_eq!(h["session_present"], true);
    assert_eq!(h["helper_rerun"], false);
    assert_no_canary(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_status_is_recorded_with_its_code() {
    let srv = mock(MockHttpSpec::new().status(429)).await;
    let r = rig(&srv.url(), Level::Failures, None);
    r.conn.initialize().await.unwrap();
    assert!(call_echo(&r).await.is_err());
    let recs = records(&r);
    let st = of_kind(&recs, "http_status");
    assert_eq!(st.len(), 1, "{recs:?}");
    assert_eq!(st[0]["http"]["status"], 429);
    assert_eq!(st[0]["http"]["body_class"], "text");
    assert_no_canary(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_credentials_are_an_auth_record() {
    let srv = mock(MockHttpSpec::new().reject_calls(1)).await;
    let r = rig(&srv.url(), Level::Failures, None);
    r.conn.initialize().await.unwrap();
    assert!(call_echo(&r).await.unwrap_err().contains("401/403"));
    let recs = records(&r);
    let a = of_kind(&recs, "http_auth");
    assert_eq!(a.len(), 1, "{recs:?}");
    assert_eq!(a[0]["http"]["status"], 401);
    assert_no_canary(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_session_is_recorded_and_the_call_recovers() {
    let srv = mock(MockHttpSpec::new().drop_session_after(1)).await;
    let r = rig(&srv.url(), Level::Failures, None);
    r.conn.initialize().await.unwrap();
    call_echo(&r).await.expect("first call");
    call_echo(&r)
        .await
        .expect("second call recovers by re-initialising");
    let recs = records(&r);
    let s = of_kind(&recs, "http_session_lost");
    assert_eq!(s.len(), 1, "{recs:?}");
    assert_eq!(s[0]["http"]["status"], 404);
    assert_eq!(s[0]["http"]["session_present"], true);
    assert_no_canary(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_upstream_is_a_timeout_record() {
    let srv = mock(MockHttpSpec::new().slow_ms(3000)).await;
    let r = rig(
        &srv.url(),
        Level::Failures,
        Some(Duration::from_millis(300)),
    );
    r.conn.initialize().await.unwrap();
    assert!(call_echo(&r).await.unwrap_err().contains("timed out"));
    let recs = records(&r);
    let t = of_kind(&recs, "http_timeout");
    assert_eq!(t.len(), 1, "{recs:?}");
    assert_eq!(t[0]["http"]["transport_error"], "timeout");
    assert_no_canary(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_upstream_is_a_connect_record() {
    let r = rig("http://127.0.0.1:1/mcp", Level::Failures, None);
    let err = r.conn.initialize().await.unwrap_err();
    assert!(err.contains("connection failed"), "{err}");
    let recs = records(&r);
    let c = of_kind(&recs, "connect_failed");
    assert_eq!(c.len(), 1, "{recs:?}");
    assert_eq!(c[0]["http"]["transport_error"], "connect");
    assert_eq!(c[0]["http"]["session_present"], false);
    assert_no_canary(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_records_nothing_and_creates_nothing() {
    let srv = mock(MockHttpSpec::new().html_error()).await;
    let r = rig(&srv.url(), Level::Off, None);
    r.conn.initialize().await.unwrap();
    assert!(call_echo(&r).await.is_err());
    assert!(r.diag.flush(Duration::from_secs(5)));
    assert!(!r.diag.dir().exists());
}
