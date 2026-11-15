// SPDX-License-Identifier: GPL-3.0-or-later

//! M7 acceptance: `per-session` and `per-cwd` scopes, `max_instances`,
//! server-to-client routing (sampling, elicitation, roots) over the session
//! event stream, and crash detection by process exit. Every test has a hard
//! timeout; brokers shut their backends down at the end and each test checks
//! that the processes it saw are gone.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mcpdoze_core::auth::Token;
use mcpdoze_core::backend::{BackendSpec, StateKind, Tuning};
use mcpdoze_core::config::Scope;
use mcpdoze_core::http::Security;
use mcpdoze_core::router::{CatalogOptions, MemoryCatalog, Router, ScopeOptions};
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const TEST_TIMEOUT: Duration = Duration::from_secs(90);
const VERSION: &str = "2025-11-25";
/// Bearer token of the test brokers (not a real secret).
const TOKEN: &str = "scopes-test-token-5c2a9e";
/// Most bytes kept from one SSE stream or response.
const CAP: usize = 4 * 1024 * 1024;

fn tuning() -> Tuning {
    Tuning {
        stop_grace: Duration::from_millis(300),
        backoff_base: Duration::from_millis(200),
        backoff_max: Duration::from_secs(1),
        healthy_reset: Duration::from_secs(30),
    }
}

async fn with_timeout(f: impl std::future::Future<Output = ()>) {
    timeout(TEST_TIMEOUT, f).await.expect("test timed out");
}

fn spec(mock: MockServerSpec) -> BackendSpec {
    BackendSpec::new(BIN, mock.args())
        .idle_ttl(Duration::from_secs(30))
        .start_timeout(Duration::from_secs(20))
}

fn scoped(scope: Scope, max_instances: usize) -> ScopeOptions {
    ScopeOptions {
        scope,
        max_instances,
    }
}

fn alive(pid: u32) -> bool {
    let mut sys = System::new().expect("sysinfo");
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[p]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(p).is_some()
}

fn kill(pid: u32) {
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

async fn wait_until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_gone(pids: &[u32]) {
    for &p in pids {
        wait_until(&format!("pid {p} gone"), Duration::from_secs(10), || {
            !alive(p)
        })
        .await;
    }
}

fn percent(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn canon(p: &str) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|e| panic!("canonicalize {p}: {e}"))
}

// ------------------------------------------------------------------ http

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

fn request_text(
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

async fn raw(
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
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Resp {
        status,
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

struct Broker {
    addr: SocketAddr,
    router: Arc<Router>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Broker {
    async fn start(
        mock: MockServerSpec,
        scope: ScopeOptions,
        edit: impl FnOnce(&mut BackendSpec),
    ) -> Broker {
        let router = Router::with(Arc::new(MemoryCatalog::default()), tuning());
        let mut s = spec(mock);
        edit(&mut s);
        router
            .register_scoped("mock", "m", s, CatalogOptions::default(), scope)
            .unwrap();
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

    async fn finish(mut self) {
        self.router.shutdown().await;
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = timeout(Duration::from_secs(10), &mut self.task).await;
    }

    async fn status(&self) -> Value {
        let r = raw(self.addr, "GET", "/admin/v1/status", &[], "").await;
        assert_eq!(r.status, 200, "{r:?}");
        r.json()["servers"][0].clone()
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One agent session over HTTP.
#[derive(Clone)]
struct Client {
    addr: SocketAddr,
    session: String,
    /// Path used for every request (may carry `?cwd=`).
    path: String,
    /// Extra headers for every request.
    headers: Vec<(String, String)>,
}

impl Client {
    async fn init_with(
        addr: SocketAddr,
        path: &str,
        headers: &[(&str, &str)],
        capabilities: Value,
    ) -> Client {
        let body = json!({"jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":VERSION,"capabilities":capabilities,
            "clientInfo":{"name":"test","version":"0"}}})
        .to_string();
        let r = raw(addr, "POST", path, headers, &body).await;
        assert_eq!(r.status, 200, "{r:?}");
        let session = r.header("mcp-session-id").expect("session id");
        Client {
            addr,
            session,
            path: path.to_owned(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    async fn init(addr: SocketAddr) -> Client {
        Self::init_with(addr, "/mcp", &[], json!({})).await
    }

    async fn post(&self, msg: Value) -> Resp {
        let mut h: Vec<(&str, &str)> = vec![
            ("Mcp-Session-Id", &self.session),
            ("MCP-Protocol-Version", VERSION),
        ];
        for (k, v) in &self.headers {
            h.push((k, v));
        }
        raw(self.addr, "POST", &self.path, &h, &msg.to_string()).await
    }

    async fn call(&self, id: i64, tool: &str) -> Value {
        let r = self
            .post(json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                "params":{"name":format!("m__{tool}"),"arguments":{}}}))
            .await;
        assert_eq!(r.status, 200, "{r:?}");
        r.json()
    }

    async fn delete(&self) -> u16 {
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

    async fn sse(&self) -> Sse {
        let req = request_text(
            "GET",
            "/mcp",
            &[
                ("Accept", "text/event-stream"),
                ("Mcp-Session-Id", &self.session),
                ("MCP-Protocol-Version", VERSION),
            ],
            "",
            false,
        );
        let mut sock = TcpStream::connect(self.addr).await.unwrap();
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut sse = Sse {
            sock,
            buf: Vec::new(),
            seen: 0,
        };
        sse.wait_for("mcpdoze stream open").await;
        sse
    }
}

fn structured(resp: &Value) -> Value {
    resp["result"]["structuredContent"].clone()
}

fn pid_of(resp: &Value) -> u32 {
    structured(resp)["pid"]
        .as_u64()
        .unwrap_or_else(|| panic!("no pid in {resp}")) as u32
}

/// An open `GET /mcp` event stream.
struct Sse {
    sock: TcpStream,
    buf: Vec<u8>,
    /// Bytes already consumed by `next_request`.
    seen: usize,
}

impl Sse {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }

    async fn read_more(&mut self, deadline: Instant, what: &str) {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "SSE never showed {what}:\n{}", self.text());
        let mut chunk = [0u8; 8192];
        match timeout(left, self.sock.read(&mut chunk)).await {
            Ok(Ok(0)) => panic!("SSE closed before {what}:\n{}", self.text()),
            Ok(Ok(n)) => {
                assert!(self.buf.len() + n <= CAP, "SSE stream too large");
                self.buf.extend_from_slice(&chunk[..n]);
            }
            Ok(Err(e)) => panic!("SSE read failed: {e}"),
            Err(_) => panic!("SSE never showed {what}:\n{}", self.text()),
        }
    }

    async fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.text().contains(needle) {
            self.read_more(deadline, needle).await;
        }
    }

    /// The next JSON-RPC message with `method` on the stream.
    async fn next_request(&mut self, method: &str) -> Value {
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
                    if v["method"] == method {
                        self.seen = pos;
                        return v;
                    }
                }
            }
            self.read_more(deadline, method).await;
        }
    }
}

// ----------------------------------------------------------------- tests

/// Two per-session sessions get two processes; each dies with its session;
/// status lists one instance per session; sampling and elicitation are
/// advertised to per-session backends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_session_instances_follow_their_sessions() {
    with_timeout(async {
        let b = Broker::start(
            MockServerSpec::new().tools(&["pid", "info"]),
            scoped(Scope::PerSession, 8),
            |_| {},
        )
        .await;
        let (c1, c2) = tokio::join!(Client::init(b.addr), Client::init(b.addr));
        let p1 = pid_of(&c1.call(1, "pid").await);
        let p2 = pid_of(&c2.call(1, "pid").await);
        assert_ne!(p1, p2);
        assert_eq!(pid_of(&c1.call(2, "pid").await), p1);
        let info = structured(&c1.call(3, "info").await);
        let caps = &info["client_capabilities"];
        assert!(caps.get("sampling").is_some(), "{caps}");
        assert!(caps.get("elicitation").is_some(), "{caps}");
        assert!(caps.get("roots").is_some(), "{caps}");

        let st = b.status().await;
        assert_eq!(st["scope"], "per-session", "{st}");
        assert_eq!(st["state"], "running");
        let inst = st["instances"].as_array().unwrap();
        assert_eq!(inst.len(), 2, "{st}");
        for i in inst {
            assert!(i["scope_key"].as_str().unwrap().starts_with("session:"));
            assert_eq!(i["state"], "running");
        }
        let mut pids: Vec<u64> = st["pids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_u64)
            .collect();
        pids.sort();
        let mut want = [p1 as u64, p2 as u64];
        want.sort();
        assert!(want.iter().all(|p| pids.contains(p)), "{st}");

        assert_eq!(c1.delete().await, 200);
        wait_gone(&[p1]).await;
        assert!(alive(p2), "other session's backend died");
        assert_eq!(pid_of(&c2.call(4, "pid").await), p2);
        wait_until("instance removed", Duration::from_secs(5), || {
            b.router.server("mock").unwrap().scoped_count() == 1
        })
        .await;
        assert_eq!(c2.delete().await, 200);
        wait_gone(&[p2]).await;
        b.finish().await;
    })
    .await;
}

/// Same directory shares one process, another directory gets its own;
/// `${cwd}` is expanded in args and literal env (not in `from_env` values),
/// the process runs in that directory, `roots/list` is answered with it, and
/// a session without a directory uses the shared fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_cwd_shares_by_directory_and_expands_cwd() {
    with_timeout(async {
        let da = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let a = da.path().to_string_lossy().into_owned();
        let bdir = db.path().to_string_lossy().into_owned();
        let b = Broker::start(
            MockServerSpec::new()
                .tools(&["pid", "info", "roots"])
                .roots_request()
                .name("${cwd}"),
            scoped(Scope::PerCwd, 8),
            |s| {
                s.env.insert("MCPDOZE_TEST_DIR".into(), "${cwd}/sub".into());
                s.env.insert("MCPDOZE_TEST_REF".into(), "${cwd}".into());
                s.ref_env.insert("MCPDOZE_TEST_REF".into());
            },
        )
        .await;
        let s1 = Client::init_with(b.addr, "/mcp", &[("X-Mcpdoze-Cwd", &a)], json!({})).await;
        let s2 = Client::init_with(b.addr, "/mcp", &[("X-Mcpdoze-Cwd", &a)], json!({})).await;
        let q = format!("/mcp?cwd={}", percent(&bdir));
        let s3 = Client::init_with(b.addr, &q, &[], json!({})).await;
        let none = Client::init(b.addr).await;

        let p1 = pid_of(&s1.call(1, "pid").await);
        let p2 = pid_of(&s2.call(1, "pid").await);
        let p3 = pid_of(&s3.call(1, "pid").await);
        let p0 = pid_of(&none.call(1, "pid").await);
        assert_eq!(p1, p2, "same cwd must share one process");
        assert_ne!(p1, p3, "different cwd must get its own process");
        assert_ne!(p0, p1);
        assert_ne!(p0, p3);

        let info = structured(&s1.call(2, "info").await);
        assert_eq!(canon(info["cwd"].as_str().unwrap()), canon(&a), "{info}");
        let args: Vec<&str> = info["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(args.contains(&a.as_str()), "{info}");
        assert_eq!(
            info["env"]["MCPDOZE_TEST_DIR"],
            format!("{a}/sub"),
            "{info}"
        );
        assert_eq!(info["env"]["MCPDOZE_TEST_REF"], "${cwd}", "{info}");
        let caps = &info["client_capabilities"];
        assert!(caps.get("sampling").is_none(), "{caps}");
        assert!(caps.get("roots").is_some(), "{caps}");

        let info3 = structured(&s3.call(2, "info").await);
        assert_eq!(
            canon(info3["cwd"].as_str().unwrap()),
            canon(&bdir),
            "{info3}"
        );

        // roots/list from the backend is answered by the broker with the cwd.
        let roots = structured(&s3.call(3, "roots").await);
        let uri = roots["reply"]["roots"][0]["uri"]
            .as_str()
            .unwrap_or_else(|| panic!("{roots}"));
        assert!(uri.starts_with("file://"), "{uri}");
        let name = db
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(uri.ends_with(&name), "{uri}");

        // The fallback has `${cwd}` = the broker's directory.
        let info0 = structured(&none.call(2, "info").await);
        let args0: Vec<&str> = info0["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(!args0.contains(&"${cwd}"), "{info0}");
        let roots0 = structured(&none.call(3, "roots").await);
        assert_eq!(roots0["reply"]["roots"], json!([]), "{roots0}");

        let st = b.status().await;
        assert_eq!(st["scope"], "per-cwd");
        let keys: Vec<String> = st["instances"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["scope_key"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(keys.len(), 3, "{st}");
        assert!(keys.contains(&String::new()), "{st}");

        // Per-cwd instances survive their sessions (TTL only).
        assert_eq!(s1.delete().await, 200);
        assert_eq!(s2.delete().await, 200);
        sleep(Duration::from_millis(300)).await;
        assert!(alive(p1));
        b.finish().await;
        wait_gone(&[p0, p1, p3]).await;
    })
    .await;
}

/// An agent advertising `roots` is asked over its stream; its first root
/// wins over the declared header.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_roots_take_precedence_over_header() {
    with_timeout(async {
        let da = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let a = da.path().to_string_lossy().into_owned();
        let bdir = db.path().to_string_lossy().into_owned();
        let b = Broker::start(
            MockServerSpec::new().tools(&["info"]),
            scoped(Scope::PerCwd, 8),
            |_| {},
        )
        .await;
        let c = Client::init_with(
            b.addr,
            "/mcp",
            &[("X-Mcpdoze-Cwd", &a)],
            json!({"roots":{"listChanged":true}}),
        )
        .await;
        let mut sse = c.sse().await;
        let caller = c.clone();
        let call = tokio::spawn(async move { caller.call(1, "info").await });
        let req = sse.next_request("roots/list").await;
        let uri = format!(
            "file:///{}",
            percent(&bdir.replace('\\', "/")).trim_start_matches('/')
        );
        let r = c
            .post(
                json!({"jsonrpc":"2.0","id":req["id"],"result":{"roots":[{"uri":uri,"name":"b"}]}}),
            )
            .await;
        assert_eq!(r.status, 202, "{r:?}");
        let info = structured(&call.await.unwrap());
        assert_eq!(canon(info["cwd"].as_str().unwrap()), canon(&bdir), "{info}");
        b.finish().await;
        wait_gone(&[info["pid"].as_u64().unwrap() as u32]).await;
    })
    .await;
}

/// Beyond `max_instances` a new scope key gets a clear JSON-RPC error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_instances_is_enforced() {
    with_timeout(async {
        let b = Broker::start(
            MockServerSpec::new().tools(&["pid"]),
            scoped(Scope::PerSession, 2),
            |_| {},
        )
        .await;
        let c1 = Client::init(b.addr).await;
        let c2 = Client::init(b.addr).await;
        let c3 = Client::init(b.addr).await;
        let p1 = pid_of(&c1.call(1, "pid").await);
        let p2 = pid_of(&c2.call(1, "pid").await);
        let r = c3.call(1, "pid").await;
        assert_eq!(r["id"], 1);
        assert_eq!(r["error"]["code"], -32001, "{r}");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("max_instances"),
            "{r}"
        );
        // Freeing one slot lets the third session in.
        assert_eq!(c1.delete().await, 200);
        wait_gone(&[p1]).await;
        let p3 = pid_of(&c3.call(2, "pid").await);
        b.finish().await;
        wait_gone(&[p2, p3]).await;
    })
    .await;
}

/// Sampling and elicitation from a per-session backend reach the owning
/// session over SSE with a broker id; the agent's answer returns to the
/// backend. Without an open stream the backend gets an error at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sampling_and_elicitation_reach_the_owning_session() {
    with_timeout(async {
        let b = Broker::start(
            MockServerSpec::new()
                .tools(&["ask", "elicit", "pid"])
                .sampling_request()
                .elicitation_request(),
            scoped(Scope::PerSession, 8),
            |_| {},
        )
        .await;
        let owner = Client::init(b.addr).await;
        let other = Client::init(b.addr).await;
        let mut sse = owner.sse().await;
        let other_sse = other.sse().await;

        let caller = owner.clone();
        let call = tokio::spawn(async move { caller.call(1, "ask").await });
        let req = sse.next_request("sampling/createMessage").await;
        let id = req["id"].as_str().expect("broker id is a string");
        assert!(id.starts_with("mcpdoze-"), "{req}");
        assert_eq!(req["params"]["maxTokens"], 16);
        let r = owner
            .post(json!({"jsonrpc":"2.0","id":id,"result":{"role":"assistant",
                "content":{"type":"text","text":"hello from the agent"},"model":"example"}}))
            .await;
        assert_eq!(r.status, 202);
        let resp = call.await.unwrap();
        let reply = &structured(&resp)["reply"];
        assert_eq!(reply["role"], "assistant", "{resp}");
        assert_eq!(reply["content"]["text"], "hello from the agent");

        let caller = owner.clone();
        let call = tokio::spawn(async move { caller.call(2, "elicit").await });
        let req = sse.next_request("elicitation/create").await;
        owner
            .post(json!({"jsonrpc":"2.0","id":req["id"],"result":{"action":"accept","content":{"choice":"b"}}}))
            .await;
        let resp = call.await.unwrap();
        assert_eq!(structured(&resp)["reply"]["content"]["choice"], "b", "{resp}");
        // The other session saw nothing of it.
        assert!(!other_sse.text().contains("createMessage"));
        assert!(!other_sse.text().contains("elicitation"));

        // An agent error is forwarded as an error.
        let caller = owner.clone();
        let call = tokio::spawn(async move { caller.call(3, "ask").await });
        let req = sse.next_request("sampling/createMessage").await;
        owner
            .post(json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-1,"message":"user declined"}}))
            .await;
        let resp = call.await.unwrap();
        assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("user declined"), "{resp}");

        // No stream: immediate error.
        let nostream = Client::init(b.addr).await;
        let started = Instant::now();
        let resp = nostream.call(1, "ask").await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap().to_owned();
        assert!(text.contains("-32601") && text.contains("event stream"), "{resp}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let pids: Vec<u32> = vec![
            pid_of(&owner.call(9, "pid").await),
            pid_of(&other.call(9, "pid").await),
            pid_of(&nostream.call(9, "pid").await),
        ];
        b.finish().await;
        wait_gone(&pids).await;
    })
    .await;
}

/// A shared backend asking while two sessions have calls in flight gets a
/// "not supported" error; with exactly one session it is routed; `ping` is
/// answered by the broker; sampling is not advertised to shared backends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_server_request_is_refused() {
    with_timeout(async {
        let b = Broker::start(
            MockServerSpec::new()
                .tools(&["ask", "slow", "info", "pid"])
                .sampling_request()
                .delay_ms(3000),
            ScopeOptions::default(),
            |_| {},
        )
        .await;
        let c1 = Client::init(b.addr).await;
        let c2 = Client::init(b.addr).await;
        let mut sse1 = c1.sse().await;
        let _sse2 = c2.sse().await;
        let info = structured(&c1.call(1, "info").await);
        assert!(info["client_capabilities"].get("sampling").is_none(), "{info}");

        // Exactly one session in flight: routed.
        let caller = c1.clone();
        let call = tokio::spawn(async move { caller.call(2, "ask").await });
        let req = sse1.next_request("sampling/createMessage").await;
        c1.post(json!({"jsonrpc":"2.0","id":req["id"],"result":{"role":"assistant","content":{"type":"text","text":"x"}}}))
            .await;
        assert_eq!(structured(&call.await.unwrap())["reply"]["role"], "assistant");

        // Two sessions in flight: ambiguous.
        let slow = c2.clone();
        let slow = tokio::spawn(async move { slow.call(3, "slow").await });
        let inst = b.router.instance("mock").unwrap();
        wait_until("slow call pending", Duration::from_secs(5), || inst.pending_calls() >= 1).await;
        let started = Instant::now();
        let resp = c1.call(4, "ask").await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap().to_owned();
        assert!(text.contains("-32601"), "{resp}");
        assert!(started.elapsed() < Duration::from_secs(2), "not immediate");
        assert!(slow.await.unwrap().get("result").is_some());
        let pid = pid_of(&c1.call(5, "pid").await);
        b.finish().await;
        wait_gone(&[pid]).await;
    })
    .await;
}

/// A backend killed from outside mid-call fails the call quickly, even when
/// a grandchild keeps its stdout open; the tree is reaped; the next call
/// after the backoff respawns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_kill_fails_call_fast_and_respawns() {
    with_timeout(async {
        let b = Broker::start(
            MockServerSpec::new()
                .tools(&["pid", "slow", "spawn_child_inherit"])
                .delay_ms(20_000),
            ScopeOptions::default(),
            |_| {},
        )
        .await;
        let c = Client::init(b.addr).await;
        let p1 = pid_of(&c.call(1, "pid").await);
        let child = pid_of(&c.call(2, "spawn_child_inherit").await);
        assert!(alive(child));
        let caller = c.clone();
        let call = tokio::spawn(async move { caller.call(3, "slow").await });
        let inst = b.router.instance("mock").unwrap();
        wait_until("slow call pending", Duration::from_secs(5), || {
            inst.pending_calls() >= 1
        })
        .await;
        let started = Instant::now();
        kill(p1);
        let r = call.await.unwrap();
        let took = started.elapsed();
        assert_eq!(r["id"], 3);
        assert_eq!(r["error"]["code"], -32000, "{r}");
        assert!(
            took < Duration::from_secs(2),
            "crash noticed after {took:?}"
        );
        wait_until("failed state", Duration::from_secs(2), || {
            inst.state() == StateKind::Failed
        })
        .await;
        assert!(inst.running_conn().is_none());
        let st = b.status().await;
        assert_eq!(st["state"], "failed", "{st}");
        assert!(st["pids"].as_array().unwrap().is_empty(), "{st}");
        wait_gone(&[p1, child]).await;
        sleep(Duration::from_millis(300)).await;
        let p2 = pid_of(&c.call(4, "pid").await);
        assert_ne!(p1, p2);
        assert_eq!(inst.spawn_count(), 2);
        b.finish().await;
        wait_gone(&[p2]).await;
    })
    .await;
}

/// `Path` helpers round-trip through `file://` URIs.
#[test]
fn file_uri_round_trip() {
    use mcpdoze_core::router::{file_uri, path_from_file_uri};
    let dir = std::env::temp_dir().join("mcpdoze example dir");
    let uri = file_uri(&dir);
    assert!(uri.starts_with("file:///"), "{uri}");
    assert!(uri.contains("%20"), "{uri}");
    assert_eq!(path_from_file_uri(&uri).as_deref(), Some(dir.as_path()));
    assert!(path_from_file_uri("file://example.com/x").is_none());
}
