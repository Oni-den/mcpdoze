// SPDX-License-Identifier: GPL-3.0-or-later

//! M6 acceptance: HTTP passthrough to a remote Streamable HTTP MCP server
//! (the `mock-mcp-http` binary). Every test has a hard timeout and the mock
//! is killed when its handle drops.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mcpdoze_core::backend::{jsonrpc, BackendSpec, StateKind, Tuning};
use mcpdoze_core::router::{MemoryCatalog, Router, Session};
use mcpdoze_testkit::{MockHttpServer, MockHttpSpec};
use serde_json::{json, Value};
use tokio::time::{sleep, timeout, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-http");
const VERSION: &str = "2025-11-25";

fn tuning() -> Tuning {
    Tuning {
        stop_grace: Duration::from_millis(500),
        backoff_base: Duration::from_millis(300),
        backoff_max: Duration::from_secs(2),
        healthy_reset: Duration::from_secs(30),
    }
}

async fn guarded<F: Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(60), f)
        .await
        .expect("test exceeded its hard timeout")
}

async fn mock(spec: MockHttpSpec) -> MockHttpServer {
    spec.program(BIN).spawn().await.expect("mock-mcp-http")
}

fn setup(spec: BackendSpec) -> (Arc<Router>, Arc<Session>) {
    let router = Router::with(Arc::new(MemoryCatalog::default()), tuning());
    router.register("remote", "m", spec).unwrap();
    let session = router.create_session(VERSION);
    (router, session)
}

async fn call(r: &Router, s: &Arc<Session>, id: i64, tool: &str) -> Value {
    let msg = json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
        "params":{"name": tool, "arguments": {"text": "hi"}}});
    r.handle(s, msg).await.expect("response")
}

fn text(resp: &Value) -> String {
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text in {resp}"))
        .to_owned()
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sessions_share_one_upstream_session() {
    guarded(async {
        let srv = mock(MockHttpSpec::new()).await;
        let (r, s1) = setup(BackendSpec::http(srv.url()));
        let s2 = r.create_session(VERSION);
        let (a, b) = tokio::join!(call(&r, &s1, 1, "m__echo"), call(&r, &s2, 1, "m__echo"));
        assert_eq!(text(&a), "echo:{\"text\":\"hi\"}");
        assert_eq!(text(&b), "echo:{\"text\":\"hi\"}");
        assert_eq!(text(&call(&r, &s1, 2, "m__session_count").await), "1");
        assert_eq!(r.instance("remote").unwrap().pid(), None);
        r.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_response_is_streamed_with_progress() {
    guarded(async {
        let srv = mock(MockHttpSpec::new().sse()).await;
        let (r, s) = setup(BackendSpec::http(srv.url()));
        let msg = json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":"m__echo","arguments":{},"_meta":{"progressToken":"tok"}}});
        let resp = r.handle(&s, msg).await.unwrap();
        assert_eq!(resp["id"], json!(7));
        assert_eq!(text(&resp), "echo:{}");
        let notes = s.take_notifications();
        assert!(
            notes.iter().any(|n| n["method"] == "notifications/progress"
                && n["params"]["progressToken"] == "tok"),
            "{notes:?}"
        );
        r.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headers_are_injected_and_missing_header_errors_cleanly() {
    guarded(async {
        let srv = mock(MockHttpSpec::new().require_header("X-Api-Key", "example-value")).await;
        let (r, s) = setup(BackendSpec::http(srv.url()).header("X-Api-Key", "example-value"));
        assert_eq!(
            text(&call(&r, &s, 1, "m__echo").await),
            "echo:{\"text\":\"hi\"}"
        );
        r.shutdown().await;

        let (r, s) = setup(BackendSpec::http(srv.url()));
        let resp = call(&r, &s, 1, "m__echo").await;
        assert_eq!(resp["error"]["code"], jsonrpc::BACKEND_UNAVAILABLE);
        let msg = resp["error"]["message"].as_str().unwrap();
        assert!(msg.contains("rejected"), "{msg}");
        r.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_404_triggers_transparent_reinitialise() {
    guarded(async {
        let srv = mock(MockHttpSpec::new().drop_session_after(2)).await;
        let (r, s) = setup(BackendSpec::http(srv.url()));
        for i in 1..=3 {
            let resp = call(&r, &s, i, "m__echo").await;
            assert!(resp.get("error").is_none(), "call {i}: {resp}");
        }
        // Call 3 hit a dropped session, re-initialised and succeeded.
        assert_eq!(text(&call(&r, &s, 4, "m__session_count").await), "2");
        r.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_ttl_drops_upstream_session_and_next_call_reinitialises() {
    guarded(async {
        let srv = mock(MockHttpSpec::new()).await;
        let (r, s) = setup(BackendSpec::http(srv.url()).idle_ttl(Duration::from_millis(300)));
        let inst = r.instance("remote").unwrap();
        assert!(call(&r, &s, 1, "m__echo").await.get("error").is_none());
        assert!(matches!(
            inst.state(),
            StateKind::Running | StateKind::Draining | StateKind::Stopping | StateKind::Stopped
        ));
        let until = Instant::now() + Duration::from_secs(10);
        while inst.state() != StateKind::Stopped {
            assert!(Instant::now() < until, "never unloaded: {:?}", inst.state());
            sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(inst.pid(), None);
        // Next call re-initialises; the mock saw the DELETE of session one.
        assert_eq!(text(&call(&r, &s, 2, "m__delete_count").await), "1");
        assert_eq!(text(&call(&r, &s, 3, "m__session_count").await), "2");
        r.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_refused_gives_clear_error_and_backs_off() {
    guarded(async {
        let url = {
            let mut srv = mock(MockHttpSpec::new()).await;
            let u = srv.url();
            srv.stop().await;
            u
        };
        let (r, s) = setup(BackendSpec::http(url));
        let inst = r.instance("remote").unwrap();
        let resp = call(&r, &s, 1, "m__echo").await;
        assert_eq!(resp["error"]["code"], jsonrpc::BACKEND_UNAVAILABLE);
        assert_eq!(inst.state(), StateKind::Failed);
        assert_eq!(inst.failures(), 1);
        let again = call(&r, &s, 2, "m__echo").await;
        let msg = again["error"]["message"].as_str().unwrap();
        assert!(msg.contains("retry in"), "{msg}");
        assert_eq!(inst.failures(), 1, "backoff must not retry immediately");
        r.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn header_values_never_reach_tracing_output() {
    guarded(async {
        const SECRET: &str = "s3cr3t-token-value-9f2";
        let cap = Capture::default();
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(cap.clone())
            .with_ansi(false)
            .finish();
        let _g = tracing::subscriber::set_default(sub);

        let srv = mock(
            MockHttpSpec::new()
                .require_header("Authorization", SECRET)
                .drop_session_after(1),
        )
        .await;
        let spec = BackendSpec::http(srv.url()).header("Authorization", SECRET);
        assert!(!format!("{spec:?}").contains(SECRET));
        let (r, s) = setup(spec);
        for i in 1..=3 {
            let _ = call(&r, &s, i, "m__echo").await;
        }
        r.shutdown().await;

        // A wrong value is rejected; the error must not echo it either.
        let (r2, s2) = setup(BackendSpec::http(srv.url()).header("Authorization", "wrong-value-1"));
        let resp = call(&r2, &s2, 1, "m__echo").await;
        assert!(!resp.to_string().contains("wrong-value-1"));
        r2.shutdown().await;

        let out = String::from_utf8_lossy(&cap.0.lock().unwrap()).into_owned();
        assert!(!out.is_empty(), "expected some tracing output");
        assert!(!out.contains(SECRET), "header value leaked into tracing");
        assert!(!out.contains("wrong-value-1"));
    })
    .await;
}
