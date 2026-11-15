// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared broker harness for HTTP-level tests: a broker on an ephemeral
//! loopback port, raw HTTP/1.1 requests, legacy (2025-11-25) sessions and
//! SSE readers. Included with `#[path]`; not every test uses every helper.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mcpdoze_core::auth::Token;
use mcpdoze_core::backend::Tuning;
use mcpdoze_core::http::Security;
use mcpdoze_core::router::Router;
use serde_json::{json, Value};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Instant};

pub const LEGACY: &str = "2025-11-25";
pub const MODERN: &str = "2026-07-28";
/// Bearer token of the test brokers (not a real secret).
pub const TOKEN: &str = "harness-test-token-3e8a41";
/// Most bytes kept from one response or stream.
pub const CAP: usize = 4 * 1024 * 1024;

pub fn tuning() -> Tuning {
    Tuning {
        stop_grace: Duration::from_millis(300),
        backoff_base: Duration::from_millis(200),
        backoff_max: Duration::from_secs(1),
        healthy_reset: Duration::from_secs(30),
    }
}

pub async fn with_timeout(limit: Duration, f: impl std::future::Future<Output = ()>) {
    timeout(limit, f).await.expect("test timed out");
}

pub async fn wait_until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(20)).await;
    }
}

pub fn kill(pid: u32) {
    let mut sys = System::new().expect("sysinfo");
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[p]),
        true,
        ProcessRefreshKind::nothing(),
    );
    assert!(
        sys.process(p).expect("process to kill").kill(),
        "kill {pid}"
    );
}

#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub head: String,
    pub body: String,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {self:?}"))
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }
}

pub fn request_text(
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: &str,
    close: bool,
) -> String {
    let mut req =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if close {
        req.push_str("Connection: close\r\n");
    }
    req.push_str(&format!(
        "Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    ));
    req
}

pub async fn raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> Resp {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request_text(method, path, extra, body, true).as_bytes())
        .await
        .unwrap();
    let mut out = Vec::new();
    s.take(CAP as u64).read_to_end(&mut out).await.unwrap();
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

pub struct Broker {
    pub addr: SocketAddr,
    pub router: Arc<Router>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Broker {
    /// Serves `router` (servers already registered).
    pub async fn start(router: Arc<Router>) -> Broker {
        let listener = mcpdoze_core::http::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let sec = Security::new(Token::from_string(TOKEN.into()).unwrap(), Vec::new());
        let r = router.clone();
        let task = tokio::spawn(async move {
            let _ = mcpdoze_core::http::serve_router(listener, r, sec, async {
                let _ = rx.await;
            })
            .await;
        });
        Broker {
            addr,
            router,
            stop: Some(tx),
            task,
        }
    }

    pub async fn finish(mut self) {
        self.router.shutdown().await;
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = timeout(Duration::from_secs(10), &mut self.task).await;
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One legacy (2025-11-25) agent session.
#[derive(Clone)]
pub struct Client {
    pub addr: SocketAddr,
    pub session: String,
    pub headers: Vec<(String, String)>,
}

impl Client {
    pub async fn init_with(addr: SocketAddr, headers: &[(&str, &str)]) -> Client {
        let body = json!({"jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":LEGACY,"capabilities":{},
            "clientInfo":{"name":"test","version":"0"}}})
        .to_string();
        let r = raw(addr, "POST", "/mcp", headers, &body).await;
        assert_eq!(r.status, 200, "{r:?}");
        Client {
            addr,
            session: r.header("mcp-session-id").expect("session id"),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    pub async fn init(addr: SocketAddr) -> Client {
        Self::init_with(addr, &[]).await
    }

    /// The `initialize` result a new session gets.
    pub async fn init_result(addr: SocketAddr) -> Value {
        let body = json!({"jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":LEGACY,"capabilities":{},
            "clientInfo":{"name":"test","version":"0"}}})
        .to_string();
        let r = raw(addr, "POST", "/mcp", &[], &body).await;
        assert_eq!(r.status, 200, "{r:?}");
        r.json()["result"].clone()
    }

    fn hdrs(&self) -> Vec<(&str, &str)> {
        let mut h: Vec<(&str, &str)> = vec![
            ("Mcp-Session-Id", &self.session),
            ("MCP-Protocol-Version", LEGACY),
        ];
        for (k, v) in &self.headers {
            h.push((k, v));
        }
        h
    }

    pub async fn post(&self, msg: Value) -> Resp {
        raw(self.addr, "POST", "/mcp", &self.hdrs(), &msg.to_string()).await
    }

    /// A request; returns the whole JSON-RPC response.
    pub async fn rpc(&self, id: i64, method: &str, params: Value) -> Value {
        let r = self
            .post(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        assert_eq!(r.status, 200, "{r:?}");
        r.json()
    }

    /// Calls tool `m__<tool>`.
    pub async fn call(&self, id: i64, tool: &str, args: Value) -> Value {
        self.rpc(
            id,
            "tools/call",
            json!({"name":format!("m__{tool}"),"arguments":args}),
        )
        .await
    }

    pub async fn delete(&self) -> u16 {
        raw(
            self.addr,
            "DELETE",
            "/mcp",
            &[("Mcp-Session-Id", &self.session)],
            "",
        )
        .await
        .status
    }

    /// Opens the session's `GET /mcp` event stream.
    pub async fn sse(&self) -> Sse {
        let mut h = self.hdrs();
        h.push(("Accept", "text/event-stream"));
        let mut sse = Sse::open(self.addr, request_text("GET", "/mcp", &h, "", false)).await;
        sse.wait_for("mcpdoze stream open").await;
        sse
    }
}

pub fn structured(resp: &Value) -> Value {
    resp["result"]["structuredContent"].clone()
}

/// An open SSE response (a `GET` stream or a streamed `POST`).
pub struct Sse {
    pub sock: TcpStream,
    pub buf: Vec<u8>,
    /// Bytes already consumed by `next`.
    seen: usize,
}

impl Sse {
    /// Sends `req` (a full HTTP request text) and keeps the connection.
    pub async fn open(addr: SocketAddr, req: String) -> Sse {
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(req.as_bytes()).await.unwrap();
        Sse {
            sock,
            buf: Vec::new(),
            seen: 0,
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }

    /// Reads more; false once the peer closed.
    pub async fn read_more(&mut self, deadline: Instant, what: &str) -> bool {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "SSE never showed {what}:\n{}", self.text());
        let mut chunk = [0u8; 8192];
        match timeout(left, self.sock.read(&mut chunk)).await {
            Ok(Ok(0)) => false,
            Ok(Ok(n)) => {
                assert!(self.buf.len() + n <= CAP, "SSE stream too large");
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
            Ok(Err(_)) => false,
            Err(_) => panic!("SSE never showed {what}:\n{}", self.text()),
        }
    }

    pub async fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.text().contains(needle) {
            assert!(
                self.read_more(deadline, needle).await,
                "SSE closed before {needle}:\n{}",
                self.text()
            );
        }
    }

    /// Every complete `data:` JSON message received so far.
    pub fn messages(&self) -> Vec<Value> {
        let text = self.text();
        text.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// The next message (after the last one returned) matching `pred`.
    pub async fn next(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = self.text();
            let mut pos = self.seen;
            while let Some(i) = text[pos..].find("data: ") {
                let start = pos + i + 6;
                let Some(end) = text[start..].find('\n') else {
                    break;
                };
                let line = &text[start..start + end];
                pos = start + end;
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    if pred(&v) {
                        self.seen = pos;
                        return v;
                    }
                }
            }
            assert!(
                self.read_more(deadline, what).await,
                "SSE closed before {what}:\n{}",
                self.text()
            );
        }
    }

    /// The next message with `method`.
    pub async fn next_method(&mut self, method: &str) -> Value {
        self.next(method, |v| v["method"] == method).await
    }

    /// Reads until the peer closes (bounded).
    pub async fn read_to_close(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.read_more(deadline, "close").await {}
    }
}
