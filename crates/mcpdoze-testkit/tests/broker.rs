// SPDX-License-Identifier: GPL-3.0-or-later

//! Broker data-path integration tests (M2/M3 acceptance) against the mock
//! stdio server. Every test has a hard timeout; backends are shut down at the
//! end, and dropping the router kills any remaining process tree.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mcpdoze_core::backend::{BackendSpec, Instance, StateKind, Tuning};
use mcpdoze_core::router::{Catalog, MemoryCatalog, Router};
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
const TOKEN: &str = "test-token-for-broker-integration";

fn security() -> mcpdoze_core::http::Security {
    let token = mcpdoze_core::auth::Token::from_string(TOKEN.into()).unwrap();
    mcpdoze_core::http::Security::new(token, Vec::new())
}

fn tuning() -> Tuning {
    Tuning {
        stop_grace: Duration::from_millis(500),
        backoff_base: Duration::from_millis(200),
        backoff_max: Duration::from_secs(1),
        healthy_reset: Duration::from_secs(30),
    }
}

fn spec(mock: MockServerSpec, ttl: Duration) -> BackendSpec {
    BackendSpec::new(BIN, mock.args())
        .idle_ttl(ttl)
        .start_timeout(Duration::from_secs(20))
}

struct Broker {
    addr: SocketAddr,
    router: Arc<Router>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Broker {
    async fn start(router: Arc<Router>) -> Broker {
        let listener = mcpdoze_core::http::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let r = router.clone();
        let task = tokio::spawn(async move {
            let _ = mcpdoze_core::http::serve_router(listener, r, security(), async {
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

    async fn one(mock: MockServerSpec, ttl: Duration) -> Broker {
        let router = Router::with(Arc::new(MemoryCatalog::default()), tuning());
        router.register("mock", "m", spec(mock, ttl)).unwrap();
        Broker::start(router).await
    }

    fn inst(&self) -> Arc<Instance> {
        self.router.instance("mock").unwrap()
    }

    async fn finish(mut self) {
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

#[derive(Debug)]
struct Resp {
    status: u16,
    session: Option<String>,
    body: Option<Value>,
}

fn http_request(method: &str, session: Option<&str>, body: &str) -> String {
    let mut req = format!(
        "{method} /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: {VERSION}\r\n\
         Authorization: Bearer {TOKEN}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(s) = session {
        req.push_str(&format!("Mcp-Session-Id: {s}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    req
}

async fn send_raw(addr: SocketAddr, method: &str, session: Option<&str>, body: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(http_request(method, session, body).as_bytes())
        .await
        .unwrap();
    s
}

async fn http(addr: SocketAddr, method: &str, session: Option<&str>, body: &str) -> Resp {
    let s = send_raw(addr, method, session, body).await;
    let mut raw = Vec::new();
    s.take(32 * 1024 * 1024)
        .read_to_end(&mut raw)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&raw);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let session = head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case("mcp-session-id")
            .then(|| v.trim().to_owned())
    });
    let body = serde_json::from_str(rest.trim()).ok();
    Resp {
        status,
        session,
        body,
    }
}

struct Client {
    addr: SocketAddr,
    session: String,
}

impl Client {
    async fn init(addr: SocketAddr) -> Client {
        let r = http(
            addr,
            "POST",
            None,
            &json!({"jsonrpc":"2.0","id":0,"method":"initialize",
                "params":{"protocolVersion":VERSION,"capabilities":{},
                "clientInfo":{"name":"test","version":"0"}}})
            .to_string(),
        )
        .await;
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(r.body.unwrap()["result"]["protocolVersion"], VERSION);
        let session = r.session.expect("session id");
        let c = Client { addr, session };
        let n = c
            .post(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        assert_eq!(n.status, 202);
        c
    }

    async fn post(&self, msg: Value) -> Resp {
        http(self.addr, "POST", Some(&self.session), &msg.to_string()).await
    }

    async fn rpc(&self, id: Value, method: &str, params: Value) -> Value {
        let r = self
            .post(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        assert_eq!(r.status, 200, "{r:?}");
        r.body.expect("json body")
    }

    async fn call(&self, id: impl Into<Value>, tool: &str, args: Value) -> Value {
        self.rpc(
            id.into(),
            "tools/call",
            json!({"name": format!("m__{tool}"), "arguments": args}),
        )
        .await
    }
}

fn pid_of(resp: &Value) -> u32 {
    resp["result"]["structuredContent"]["pid"]
        .as_u64()
        .unwrap_or_else(|| panic!("no pid in {resp}")) as u32
}

fn text_of(resp: &Value) -> String {
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
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

async fn wait_until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(20)).await;
    }
}

async fn with_timeout(f: impl std::future::Future<Output = ()>) {
    timeout(TEST_TIMEOUT, f).await.expect("test timed out");
}

/// (a) Two clients on two sessions share exactly one backend process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_share_one_backend() {
    with_timeout(async {
        let b = Broker::one(
            MockServerSpec::new().tools(&["pid", "slow"]).delay_ms(300),
            Duration::from_secs(30),
        )
        .await;
        let (c1, c2) = tokio::join!(Client::init(b.addr), Client::init(b.addr));
        assert_ne!(c1.session, c2.session);
        let work = |c: Client| async move {
            let mut pids = Vec::new();
            let (slow, p1) =
                tokio::join!(c.call(1, "slow", json!({})), c.call(2, "pid", json!({})));
            assert!(slow.get("result").is_some(), "{slow}");
            pids.push(pid_of(&p1));
            for i in 3..6 {
                pids.push(pid_of(&c.call(i, "pid", json!({})).await));
            }
            pids
        };
        let (a, bb) = tokio::join!(work(c1), work(c2));
        let all: Vec<u32> = a.into_iter().chain(bb).collect();
        assert!(all.iter().all(|p| *p == all[0]), "{all:?}");
        assert_eq!(b.inst().spawn_count(), 1);
        assert_eq!(b.inst().pid(), Some(all[0]));
        b.finish().await;
        assert!(!alive(all[0]));
    })
    .await;
}

/// (b) The first call spawns; with a snapshot, tools/list needs no process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_from_snapshot_with_zero_processes() {
    with_timeout(async {
        let b = Broker::one(
            MockServerSpec::new().tools(&["echo", "pid"]),
            Duration::from_millis(300),
        )
        .await;
        let c = Client::init(b.addr).await;
        assert_eq!(b.inst().state(), StateKind::Stopped);
        assert_eq!(b.inst().spawn_count(), 0);
        let names = |r: &Value| -> Vec<String> {
            r["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_owned())
                .collect()
        };
        // No snapshot yet: listing spawns once.
        let r = c.rpc(json!(1), "tools/list", json!({})).await;
        assert_eq!(names(&r), ["m__echo", "m__pid"]);
        assert_eq!(b.inst().spawn_count(), 1);
        wait_until("unload", Duration::from_secs(10), || {
            b.inst().state() == StateKind::Stopped
        })
        .await;
        // Snapshot exists: answered with zero processes.
        let r = c.rpc(json!(2), "tools/list", json!({})).await;
        assert_eq!(names(&r), ["m__echo", "m__pid"]);
        assert_eq!(b.inst().state(), StateKind::Stopped);
        assert_eq!(b.inst().spawn_count(), 1);
        // A call spawns lazily.
        let p = pid_of(&c.call(3, "pid", json!({})).await);
        assert_eq!(b.inst().spawn_count(), 2);
        b.finish().await;
        assert!(!alive(p));

        // Registration-time snapshot: never spawned at all.
        let catalog = Arc::new(MemoryCatalog::default());
        catalog.set(
            "mock",
            vec![json!({"name": "echo", "inputSchema": {"type": "object"}})],
        );
        let router = Router::with(catalog, tuning());
        router
            .register(
                "mock",
                "m",
                spec(MockServerSpec::new(), Duration::from_secs(30)),
            )
            .unwrap();
        let b = Broker::start(router).await;
        let c = Client::init(b.addr).await;
        let r = c.rpc(json!(1), "tools/list", json!({})).await;
        assert_eq!(names(&r), ["m__echo"]);
        assert_eq!(b.inst().spawn_count(), 0);
        b.finish().await;
    })
    .await;
}

/// (c) Idle TTL unloads to zero (pid dead) and the next call respawns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ttl_unloads_and_respawns() {
    with_timeout(async {
        let b = Broker::one(
            MockServerSpec::new().tools(&["pid"]),
            Duration::from_millis(300),
        )
        .await;
        let c = Client::init(b.addr).await;
        let p1 = pid_of(&c.call(1, "pid", json!({})).await);
        assert!(alive(p1));
        wait_until("unload", Duration::from_secs(10), || {
            b.inst().state() == StateKind::Stopped
        })
        .await;
        wait_until("pid gone", Duration::from_secs(10), || !alive(p1)).await;
        let p2 = pid_of(&c.call(2, "pid", json!({})).await);
        assert_ne!(p1, p2);
        assert_eq!(b.inst().spawn_count(), 2);
        b.finish().await;
        assert!(!alive(p2));
    })
    .await;
}

/// (d) A call longer than the TTL keeps the backend loaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_unloads_with_call_in_flight() {
    with_timeout(async {
        let b = Broker::one(
            MockServerSpec::new().tools(&["slow", "pid"]).delay_ms(1500),
            Duration::from_millis(200),
        )
        .await;
        let c = Arc::new(Client::init(b.addr).await);
        let p = pid_of(&c.call(1, "pid", json!({})).await);
        let c2 = c.clone();
        let slow = tokio::spawn(async move { c2.call(2, "slow", json!({"k": 1})).await });
        wait_until("in flight", Duration::from_secs(5), || {
            b.inst().in_flight() == 1
        })
        .await;
        while !slow.is_finished() {
            assert_eq!(b.inst().state(), StateKind::Running);
            assert_eq!(b.inst().pid(), Some(p));
            sleep(Duration::from_millis(50)).await;
        }
        let r = slow.await.unwrap();
        assert!(text_of(&r).contains("\"k\":1"), "{r}");
        assert_eq!(b.inst().spawn_count(), 1);
        wait_until("unload after call", Duration::from_secs(10), || {
            b.inst().state() == StateKind::Stopped
        })
        .await;
        b.finish().await;
        assert!(!alive(p));
    })
    .await;
}

/// (e) Calls arriving around TTL expiry always succeed (200 iterations).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_at_ttl_expiry_succeeds() {
    with_timeout(async {
        let ttl = Duration::from_millis(30);
        let router = Router::with(Arc::new(MemoryCatalog::default()), tuning());
        router
            .register(
                "mock",
                "m",
                spec(MockServerSpec::new().tools(&["echo"]), ttl),
            )
            .unwrap();
        let session = router.create_session(VERSION);
        let call = |i: u64| {
            let router = router.clone();
            let session = session.clone();
            async move {
                router
                    .handle(
                        &session,
                        json!({"jsonrpc":"2.0","id":i,"method":"tools/call",
                            "params":{"name":"m__echo","arguments":{"i":i}}}),
                    )
                    .await
                    .unwrap()
            }
        };
        for i in 0..200u64 {
            let r = call(i * 2).await;
            assert!(r.get("result").is_some(), "iteration {i}: {r}");
            // Land within +-10 ms of the expiry, both sides of it.
            let jitter = (i % 21) as i64 - 10;
            sleep(Duration::from_millis((30 + jitter) as u64)).await;
            let r = call(i * 2 + 1).await;
            assert!(r.get("result").is_some(), "iteration {i}: {r}");
            assert_eq!(r["id"], i * 2 + 1);
        }
        let inst = router.instance("mock").unwrap();
        assert!(inst.spawn_count() > 1, "TTL never expired");
        router.shutdown().await;
        assert_eq!(inst.state(), StateKind::Stopped);
    })
    .await;
}

/// (f) A crash fails the in-flight call; after the backoff a call succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_fails_call_then_recovers_after_backoff() {
    with_timeout(async {
        let b = Broker::one(
            MockServerSpec::new().tools(&["pid"]).crash_after(1),
            Duration::from_secs(30),
        )
        .await;
        let c = Client::init(b.addr).await;
        let p1 = pid_of(&c.call(1, "pid", json!({})).await);
        let r = c.call(2, "pid", json!({})).await;
        assert_eq!(r["id"], 2);
        assert_eq!(r["error"]["code"], -32000, "{r}");
        wait_until("failed state", Duration::from_secs(5), || {
            b.inst().state() == StateKind::Failed
        })
        .await;
        // During the backoff calls fail fast without spawning.
        let r = c.call(3, "pid", json!({})).await;
        assert!(r.get("error").is_some(), "{r}");
        assert_eq!(b.inst().spawn_count(), 1);
        wait_until("old pid gone", Duration::from_secs(10), || !alive(p1)).await;
        sleep(Duration::from_millis(300)).await;
        let r = c.call(4, "pid", json!({})).await;
        let p2 = pid_of(&r);
        assert_ne!(p1, p2);
        assert_eq!(b.inst().spawn_count(), 2);
        assert_eq!(b.inst().failures(), 1);
        b.finish().await;
        assert!(!alive(p2));
    })
    .await;
}

/// (g) Concurrent out-of-order responses map back to the right session and
/// id, even when two sessions use the same id; progress follows its token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_order_responses_map_back() {
    with_timeout(async {
        let b = Broker::one(
            MockServerSpec::new().tools(&["slow", "echo"]).delay_ms(800),
            Duration::from_secs(30),
        )
        .await;
        let (ca, cb) = tokio::join!(Client::init(b.addr), Client::init(b.addr));
        let order = std::sync::Mutex::new(Vec::new());
        async fn run(
            c: &Client,
            order: &std::sync::Mutex<Vec<&'static str>>,
            id: Value,
            tool: &str,
            who: &str,
            tag: &'static str,
        ) -> Value {
            let r = c
                .rpc(
                    id,
                    "tools/call",
                    json!({"name": format!("m__{tool}"), "arguments": {"who": who},
                        "_meta": {"progressToken": format!("tok-{who}")}}),
                )
                .await;
            order.lock().unwrap().push(tag);
            r
        }
        let a_slow = run(&ca, &order, json!(1), "slow", "A", "a1");
        let a_fast = async {
            sleep(Duration::from_millis(100)).await;
            run(&ca, &order, json!("x"), "echo", "A2", "ax").await
        };
        let b_fast = async {
            sleep(Duration::from_millis(100)).await;
            run(&cb, &order, json!(1), "echo", "B", "b1").await
        };
        let (ra, rax, rb) = tokio::join!(a_slow, a_fast, b_fast);
        assert_eq!(ra["id"], 1);
        assert!(text_of(&ra).contains("\"who\":\"A\""), "{ra}");
        assert_eq!(rax["id"], "x");
        assert!(text_of(&rax).contains("\"who\":\"A2\""), "{rax}");
        assert_eq!(rb["id"], 1);
        assert!(text_of(&rb).contains("\"who\":\"B\""), "{rb}");
        assert_eq!(order.lock().unwrap().last(), Some(&"a1"));
        // Progress from the slow call reached session A with its own token.
        let sa = b.router.session(&ca.session).unwrap();
        let sb = b.router.session(&cb.session).unwrap();
        let progress = sa.take_notifications();
        assert!(!progress.is_empty());
        for p in &progress {
            assert_eq!(p["method"], "notifications/progress");
            assert_eq!(p["params"]["progressToken"], "tok-A");
        }
        assert!(sb.take_notifications().is_empty());
        assert_eq!(b.inst().spawn_count(), 1);
        b.finish().await;
    })
    .await;
}

/// (h) Agent cancellation, session end and agent disconnect all cancel the
/// backend call, and the backend stays usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_and_disconnect() {
    with_timeout(async {
        let b = Broker::one(
            MockServerSpec::new()
                .tools(&["slow", "pid"])
                .delay_ms(20_000),
            Duration::from_secs(30),
        )
        .await;
        let c = Arc::new(Client::init(b.addr).await);
        let pid = pid_of(&c.call(1, "pid", json!({})).await);
        let inst = b.inst();

        // notifications/cancelled from the agent.
        let c2 = c.clone();
        let started = Instant::now();
        let slow = tokio::spawn(async move { c2.call(5, "slow", json!({})).await });
        wait_until("pending", Duration::from_secs(5), || {
            inst.pending_calls() == 1
        })
        .await;
        let n = c
            .post(json!({"jsonrpc":"2.0","method":"notifications/cancelled",
                "params":{"requestId":5,"reason":"test"}}))
            .await;
        assert_eq!(n.status, 202);
        let r = slow.await.unwrap();
        assert_eq!(r["id"], 5);
        assert_eq!(r["error"]["code"], -32800, "{r}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(inst.pending_calls(), 0);
        wait_until("released", Duration::from_secs(5), || inst.in_flight() == 0).await;

        // Agent disconnect mid-call.
        let body = json!({"jsonrpc":"2.0","id":6,"method":"tools/call",
            "params":{"name":"m__slow","arguments":{}}})
        .to_string();
        let sock = send_raw(b.addr, "POST", Some(&c.session), &body).await;
        wait_until("pending", Duration::from_secs(5), || {
            inst.pending_calls() == 1
        })
        .await;
        drop(sock);
        wait_until("disconnect cancels", Duration::from_secs(10), || {
            inst.pending_calls() == 0 && inst.in_flight() == 0
        })
        .await;

        // DELETE ends the session and cancels its in-flight call.
        let c3 = Client::init(b.addr).await;
        let session = c3.session.clone();
        let slow = tokio::spawn(async move { c3.call(7, "slow", json!({})).await });
        wait_until("pending", Duration::from_secs(5), || {
            inst.pending_calls() == 1
        })
        .await;
        let d = http(b.addr, "DELETE", Some(&session), "").await;
        assert_eq!(d.status, 200);
        let r = slow.await.unwrap();
        assert_eq!(r["error"]["code"], -32800, "{r}");
        let gone = http(
            b.addr,
            "POST",
            Some(&session),
            &json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
        )
        .await;
        assert_eq!(gone.status, 404);

        // Same backend, still healthy.
        assert_eq!(pid_of(&c.call(8, "pid", json!({})).await), pid);
        assert_eq!(inst.spawn_count(), 1);
        b.finish().await;
        assert!(!alive(pid));
    })
    .await;
}

/// (i) A grandchild started by the backend dies when the backend unloads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grandchild_killed_on_unload() {
    with_timeout(async {
        let b = Broker::one(
            MockServerSpec::new().tools(&["spawn_child", "pid"]),
            Duration::from_millis(300),
        )
        .await;
        let c = Client::init(b.addr).await;
        let root = pid_of(&c.call(1, "pid", json!({})).await);
        let child = pid_of(&c.call(2, "spawn_child", json!({})).await);
        assert_ne!(root, child);
        assert!(alive(child));
        wait_until("unload", Duration::from_secs(10), || {
            b.inst().state() == StateKind::Stopped
        })
        .await;
        wait_until("grandchild gone", Duration::from_secs(10), || !alive(child)).await;
        assert!(!alive(root));
        b.finish().await;
    })
    .await;
}

/// Stateless clients: no session id, protocol header required.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stateless_requests_and_http_basics() {
    with_timeout(async {
        let b = Broker::one(
            MockServerSpec::new().tools(&["pid"]),
            Duration::from_secs(30),
        )
        .await;
        let body = json!({"jsonrpc":"2.0","id":9,"method":"tools/call",
            "params":{"name":"m__pid","arguments":{}}})
        .to_string();
        let r = http(b.addr, "POST", None, &body).await;
        assert_eq!(r.status, 200, "{r:?}");
        let pid = pid_of(r.body.as_ref().unwrap());
        assert_eq!(b.router.session_count(), 0);
        // Missing protocol header without a session is rejected.
        let raw = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Authorization: Bearer {TOKEN}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut s = TcpStream::connect(b.addr).await.unwrap();
        s.write_all(raw.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.take(1 << 20).read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
        // GET is 405, unknown session is 404, parse error is 400.
        assert_eq!(http(b.addr, "GET", None, "").await.status, 405);
        assert_eq!(http(b.addr, "POST", Some("nope"), &body).await.status, 404);
        assert_eq!(http(b.addr, "POST", None, "{not json").await.status, 400);
        b.finish().await;
        assert!(!alive(pid));
    })
    .await;
}
