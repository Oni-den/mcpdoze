// SPDX-License-Identifier: GPL-3.0-or-later

//! ADR 0002 (v0.1): a modern (2026-07-28, stateless) probe is refused with a
//! plain `-32600` 400 so dual-era clients fall back to `initialize`; the
//! broker keeps serving 2025-11-25 clients afterwards. Hard timeout; the
//! broker shuts its backend down at the end.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mcpdoze_core::auth::Token;
use mcpdoze_core::backend::{BackendSpec, Tuning};
use mcpdoze_core::http::Security;
use mcpdoze_core::router::{MemoryCatalog, Router};
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const TEST_TIMEOUT: Duration = Duration::from_secs(60);
const LEGACY: &str = "2025-11-25";
const MODERN: &str = "2026-07-28";
/// Bearer token of the test broker (not a real secret).
const TOKEN: &str = "fallback-test-token-7d1b3f";
const CAP: u64 = 1024 * 1024;

#[derive(Debug)]
struct Resp {
    status: u16,
    head: String,
    body: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {self:?}"))
    }
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }
}

async fn raw(addr: SocketAddr, method: &str, extra: &[(&str, &str)], body: &str) -> Resp {
    let mut req = format!(
        "{method} /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\
         Connection: close\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n"
    );
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.take(CAP).read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Resp {
        status: head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

async fn run(addr: SocketAddr) {
    // (a) modern probe: 400, -32600, no session header.
    let probe = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{
        "io.modelcontextprotocol/protocolVersion": MODERN,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name":"test","version":"0"}}}});
    let r = raw(
        addr,
        "POST",
        &[
            ("MCP-Protocol-Version", MODERN),
            ("Mcp-Method", "tools/list"),
        ],
        &probe.to_string(),
    )
    .await;
    assert_eq!(r.status, 400, "{r:?}");
    let code = r.json()["error"]["code"].as_i64().expect("error code");
    assert_eq!(code, -32600, "{r:?}");
    assert!(code != -32020 && code != -32022);
    assert!(r.header("mcp-session-id").is_none(), "{r:?}");

    // (b) fallback: normal 2025-11-25 initialize, then tools/list.
    let init = json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{
        "protocolVersion": LEGACY, "capabilities": {},
        "clientInfo": {"name":"test","version":"0"}}});
    let r = raw(
        addr,
        "POST",
        &[("MCP-Protocol-Version", LEGACY)],
        &init.to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.json()["result"]["protocolVersion"], LEGACY, "{r:?}");
    let session = r.header("mcp-session-id").expect("session id");
    let list = json!({"jsonrpc":"2.0","id":3,"method":"tools/list"});
    let r = raw(
        addr,
        "POST",
        &[
            ("Mcp-Session-Id", &session),
            ("MCP-Protocol-Version", LEGACY),
        ],
        &list.to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.json()["result"]["tools"].is_array(), "{r:?}");

    // (c) GET without a session: clean 4xx, not 5xx.
    let r = raw(addr, "GET", &[("Accept", "text/event-stream")], "").await;
    assert!((400..500).contains(&r.status), "{r:?}");

    let r = raw(addr, "DELETE", &[("Mcp-Session-Id", &session)], "").await;
    assert_eq!(r.status, 200, "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modern_probe_falls_back_to_initialize() {
    let tuning = Tuning {
        stop_grace: Duration::from_millis(300),
        backoff_base: Duration::from_millis(200),
        backoff_max: Duration::from_secs(1),
        healthy_reset: Duration::from_secs(30),
    };
    let router = Router::with(Arc::new(MemoryCatalog::default()), tuning);
    let spec = BackendSpec::new(BIN, MockServerSpec::new().tools(&["pid"]).args())
        .idle_ttl(Duration::from_secs(30))
        .start_timeout(Duration::from_secs(20));
    router.register("mock", "m", spec).unwrap();
    let listener = mcpdoze_core::http::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let sec = Security::new(Token::from_string(TOKEN.into()).unwrap(), Vec::new());
    let r = router.clone();
    let mut task = tokio::spawn(async move {
        let _ = mcpdoze_core::http::serve_router(listener, r, sec, async {
            let _ = rx.await;
        })
        .await;
    });
    let res = timeout(TEST_TIMEOUT, run(addr)).await;
    router.shutdown().await;
    let _ = tx.send(());
    let _ = timeout(Duration::from_secs(10), &mut task).await;
    task.abort();
    res.expect("test timed out");
}
