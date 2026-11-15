// SPDX-License-Identifier: GPL-3.0-or-later

//! M4 acceptance: disk catalog, probe, refresh, `list_changed`, the SSE
//! notification channel and prompts/resources passthrough, against the mock
//! stdio server. Every test has a hard timeout; brokers are shut down at the
//! end (probes and backends included).

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mcpdoze_core::auth::Token;
use mcpdoze_core::backend::{BackendSpec, StateKind, Tuning};
use mcpdoze_core::daemon::{Daemon, ServeOptions};
use mcpdoze_core::http::Security;
use mcpdoze_core::router::{Catalog, MemoryCatalog, Router};
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const TEST_TIMEOUT: Duration = Duration::from_secs(90);
const VERSION: &str = "2025-11-25";
/// Bearer token of the in-memory test brokers (not a real secret).
const TOKEN: &str = "catalog-test-token-3b9e1f";
/// Literal env value that must never reach a catalog file.
const SENTINEL: &str = "sentinel-value-7d41c";
/// Most bytes kept from one SSE stream.
const SSE_CAP: usize = 4 * 1024 * 1024;

fn tuning() -> Tuning {
    Tuning {
        stop_grace: Duration::from_millis(500),
        backoff_base: Duration::from_millis(200),
        backoff_max: Duration::from_secs(1),
        healthy_reset: Duration::from_secs(30),
    }
}

async fn with_timeout(f: impl std::future::Future<Output = ()>) {
    timeout(TEST_TIMEOUT, f).await.expect("test timed out");
}

async fn wait_until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(50)).await;
    }
}

// ------------------------------------------------------------ raw HTTP

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
    token: &str,
    extra: &[(&str, &str)],
    body: &str,
    close: bool,
) -> String {
    let mut req =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if close {
        req.push_str("Connection: close\r\n");
    }
    req.push_str(&format!(
        "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ));
    req
}

async fn raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> Resp {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request_text(method, path, token, extra, body, true).as_bytes())
        .await
        .unwrap();
    let mut out = Vec::new();
    s.take(8 * 1024 * 1024).read_to_end(&mut out).await.unwrap();
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

#[derive(Clone)]
struct Api {
    addr: SocketAddr,
    token: String,
}

impl Api {
    async fn ok(&self, method: &str, path: &str) -> Value {
        let r = raw(self.addr, method, path, &self.token, &[], "").await;
        assert!((200..300).contains(&r.status), "{method} {path}: {r:?}");
        r.json()
    }

    async fn status_of(&self, name: &str) -> Value {
        let s = self.ok("GET", "/admin/v1/status").await;
        s["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no status for {name}: {s}"))
    }

    /// One JSON-RPC request; stateless unless `session` is given.
    async fn rpc(&self, session: Option<&str>, method: &str, params: Value) -> Value {
        let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
        let mut hdrs = vec![("MCP-Protocol-Version", VERSION)];
        if let Some(s) = session {
            hdrs.push(("Mcp-Session-Id", s));
        }
        let r = raw(self.addr, "POST", "/mcp", &self.token, &hdrs, &body).await;
        assert_eq!(r.status, 200, "{method}: {r:?}");
        r.json()
    }

    async fn tool_names(&self) -> Vec<String> {
        let r = self.rpc(None, "tools/list", json!({})).await;
        r["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{r}"))
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }

    async fn session(&self) -> String {
        let body = json!({"jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":VERSION,"capabilities":{},
            "clientInfo":{"name":"test","version":"0"}}})
        .to_string();
        let r = raw(self.addr, "POST", "/mcp", &self.token, &[], &body).await;
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(
            r.json()["result"]["capabilities"]["tools"]["listChanged"],
            true
        );
        r.header("mcp-session-id").expect("session id")
    }
}

/// An open `GET /mcp` event stream.
struct Sse {
    sock: TcpStream,
    buf: Vec<u8>,
}

impl Sse {
    async fn open(api: &Api, session: &str) -> Sse {
        let req = request_text(
            "GET",
            "/mcp",
            &api.token,
            &[
                ("Accept", "text/event-stream"),
                ("Mcp-Session-Id", session),
                ("MCP-Protocol-Version", VERSION),
            ],
            "",
            false,
        );
        let mut sock = TcpStream::connect(api.addr).await.unwrap();
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut sse = Sse {
            sock,
            buf: Vec::new(),
        };
        let text = sse
            .wait_for("mcpdoze stream open", Duration::from_secs(10))
            .await;
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(
            text.to_ascii_lowercase()
                .contains("content-type: text/event-stream"),
            "{text}"
        );
        sse
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }

    /// Reads until `needle` appears; returns everything read so far.
    async fn wait_for(&mut self, needle: &str, limit: Duration) -> String {
        let deadline = Instant::now() + limit;
        loop {
            let t = self.text();
            if t.contains(needle) {
                return t;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "SSE never showed {needle:?}:\n{t}");
            let mut chunk = [0u8; 8192];
            match timeout(left, self.sock.read(&mut chunk)).await {
                Ok(Ok(0)) => panic!("SSE closed before {needle:?}:\n{t}"),
                Ok(Ok(n)) => {
                    assert!(self.buf.len() + n <= SSE_CAP, "SSE stream too large");
                    self.buf.extend_from_slice(&chunk[..n]);
                }
                Ok(Err(e)) => panic!("SSE read failed: {e}"),
                Err(_) => panic!("SSE never showed {needle:?}:\n{t}"),
            }
        }
    }

    /// Waits for the server to end the stream.
    async fn wait_closed(&mut self, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "SSE stream never closed");
            let mut chunk = [0u8; 8192];
            match timeout(left, self.sock.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return,
                Ok(Ok(n)) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    // End of a chunked body.
                    if self.text().ends_with("0\r\n\r\n") {
                        return;
                    }
                }
                Err(_) => panic!("SSE stream never closed:\n{}", self.text()),
            }
        }
    }
}

// --------------------------------------------------------- brokers

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

fn broker_toml(dir: &Path) -> String {
    format!(
        "[broker]\nbind = \"127.0.0.1:0\"\ntoken_file = {}\ncatalog_dir = {}\n",
        toml_str(&dir.join("token").to_string_lossy()),
        toml_str(&dir.join("catalog").to_string_lossy())
    )
}

fn server_toml(name: &str, mock: &MockServerSpec, extra: &str) -> String {
    format!(
        "[servers.{name}]\ntransport = \"stdio\"\ncommand = {}\nargs = {}\nidle_ttl = \"30s\"\n{extra}",
        toml_str(BIN),
        serde_json::to_string(&mock.args()).unwrap()
    )
}

fn write_file(path: &Path, text: &str) {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
    f.sync_all().unwrap();
}

fn catalog_file(dir: &Path, server: &str) -> PathBuf {
    dir.join("catalog").join(format!("{server}.json"))
}

fn read_catalog(dir: &Path, server: &str) -> Option<Value> {
    let text = std::fs::read_to_string(catalog_file(dir, server)).ok()?;
    serde_json::from_str(&text).ok()
}

async fn start_daemon(config: &Path) -> (Daemon, Api) {
    let d = Daemon::start(ServeOptions {
        config_path: Some(config.to_path_buf()),
        bind: None,
        watch: false,
        tuning: tuning(),
        catalog: None,
    })
    .await
    .expect("daemon start");
    let token = std::fs::read_to_string(d.token_file().expect("token file"))
        .unwrap()
        .trim()
        .to_owned();
    let api = Api {
        addr: d.local_addr(),
        token,
    };
    (d, api)
}

/// Waits until `server`'s probe stored a catalog with `tool`.
async fn wait_catalog_with(dir: &Path, server: &str, tool: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(c) = read_catalog(dir, server) {
            if c["tools"]
                .as_array()
                .is_some_and(|t| t.iter().any(|x| x["name"] == tool))
            {
                return c;
            }
        }
        assert!(
            Instant::now() < deadline,
            "catalog of {server} never listed {tool}"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

/// An in-memory broker (no config file) on an ephemeral port.
struct MemBroker {
    api: Api,
    router: Arc<Router>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl MemBroker {
    async fn start(router: Arc<Router>) -> MemBroker {
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
        MemBroker {
            api: Api {
                addr,
                token: TOKEN.into(),
            },
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
}

impl Drop for MemBroker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn mem_router() -> Arc<Router> {
    Router::with(Arc::new(MemoryCatalog::default()), tuning())
}

fn spec(mock: MockServerSpec) -> BackendSpec {
    BackendSpec::new(BIN, mock.args())
        .idle_ttl(Duration::from_secs(30))
        .start_timeout(Duration::from_secs(20))
}

// ----------------------------------------------------------------- tests

/// Probe at registration, `tools/list` with zero processes, the catalog
/// survives a restart (no new probe), env values never reach the file, a
/// fingerprint change re-probes, and the admin refresh endpoints.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_catalog_survives_restart_with_zero_processes() {
    with_timeout(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let mock = MockServerSpec::new().tools(&["echo", "pid"]);
        let env = format!("env = {{ EXAMPLE_SETTING = {} }}\n", toml_str(SENTINEL));
        write_file(
            &cfg,
            &format!(
                "{}\n{}",
                broker_toml(dir.path()),
                server_toml("mock", &mock, &env)
            ),
        );

        // First run: one background probe writes the catalog.
        let (d, api) = start_daemon(&cfg).await;
        wait_catalog_with(dir.path(), "mock", "pid").await;
        let router = d.control().router().clone();
        assert_eq!(router.probe_count(), 1);
        let text = std::fs::read_to_string(catalog_file(dir.path(), "mock")).unwrap();
        assert!(!text.contains(SENTINEL), "env value leaked:\n{text}");
        assert!(
            !text.contains("EXAMPLE_SETTING"),
            "env name stored:\n{text}"
        );
        let c: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(c["version"], 1);
        assert!(c["fingerprint"].as_str().unwrap().starts_with("v1-"));
        assert_eq!(c["server_info"]["name"], "mock");
        assert_eq!(api.tool_names().await, ["mock__echo", "mock__pid"]);
        let s = api.status_of("mock").await;
        assert_eq!(s["state"], "stopped", "{s}");
        assert_eq!(s["pids"], json!([]));
        assert_eq!(s["calls"], 0);
        d.shutdown().await.unwrap();

        // Second run: warm catalog, nothing spawned at all.
        let (d, api) = start_daemon(&cfg).await;
        let router = d.control().router().clone();
        assert_eq!(api.tool_names().await, ["mock__echo", "mock__pid"]);
        let p = api.rpc(None, "prompts/list", json!({})).await;
        assert_eq!(p["result"]["prompts"][0]["name"], "mock__greet", "{p}");
        let s = api.status_of("mock").await;
        assert_eq!(s["state"], "stopped", "{s}");
        assert_eq!(s["pids"], json!([]));
        assert_eq!(s["calls"], 0);
        assert_eq!(s["restarts"], 0);
        assert_eq!(router.probe_count(), 0);
        assert_eq!(router.instance("mock").unwrap().spawn_count(), 0);

        // Admin refresh: probes (backend is stopped), unchanged.
        let r = api.ok("POST", "/admin/v1/servers/mock/refresh").await;
        assert_eq!(r["name"], "mock");
        assert_eq!(r["changed"], false, "{r}");
        assert_eq!(r["tools"], 2);
        assert_eq!(r["error"], Value::Null);
        assert_eq!(router.probe_count(), 1);
        let r = api.ok("POST", "/admin/v1/refresh").await;
        assert_eq!(r["servers"][0]["name"], "mock", "{r}");
        let nope = raw(
            api.addr,
            "POST",
            "/admin/v1/servers/nope/refresh",
            &api.token,
            &[],
            "",
        )
        .await;
        assert_eq!(nope.status, 404);
        assert_eq!(router.instance("mock").unwrap().spawn_count(), 0);

        // Fingerprint change (new args): reload re-probes.
        let changed = MockServerSpec::new().tools(&["echo", "pid", "newer"]);
        write_file(
            &cfg,
            &format!(
                "{}\n{}",
                broker_toml(dir.path()),
                server_toml("mock", &changed, &env)
            ),
        );
        let probes = router.probe_count();
        let r = api.ok("POST", "/admin/v1/reload").await;
        assert_eq!(r["changed"], json!(["mock"]), "{r}");
        wait_catalog_with(dir.path(), "mock", "newer").await;
        assert_eq!(
            api.tool_names().await,
            ["mock__echo", "mock__pid", "mock__newer"]
        );
        assert_eq!(
            router.probe_count(),
            probes + 1,
            "one probe for the changed spec"
        );
        let s = api.status_of("mock").await;
        assert_eq!(s["state"], "stopped", "{s}");
        d.shutdown().await.unwrap();
    })
    .await;
}

/// A corrupt catalog file is ignored and rewritten by a probe; a server
/// with `probe = false` stays uncatalogued until its first list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_catalog_recovery_and_probe_false() {
    with_timeout(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        write_file(&catalog_file(dir.path(), "mock"), "{ this is not json");
        let mock = MockServerSpec::new().tools(&["echo"]);
        let lazy = MockServerSpec::new().tools(&["pid"]);
        write_file(
            &cfg,
            &format!(
                "{}\n{}\n{}",
                broker_toml(dir.path()),
                server_toml("mock", &mock, ""),
                server_toml("lazy", &lazy, "probe = false\n")
            ),
        );
        let (d, api) = start_daemon(&cfg).await;
        let router = d.control().router().clone();
        wait_catalog_with(dir.path(), "mock", "echo").await;
        assert_eq!(router.probe_count(), 1);
        assert!(read_catalog(dir.path(), "lazy").is_none());
        assert_eq!(api.status_of("lazy").await["state"], "stopped");
        // The first list starts the uncatalogued server itself.
        assert_eq!(api.tool_names().await, ["lazy__pid", "mock__echo"]);
        assert_eq!(router.instance("lazy").unwrap().spawn_count(), 1);
        assert_eq!(router.instance("mock").unwrap().spawn_count(), 0);
        wait_catalog_with(dir.path(), "lazy", "pid").await;
        assert_eq!(router.probe_count(), 1);
        d.shutdown().await.unwrap();
    })
    .await;
}

/// The mock's `list_changed` updates the catalog and is delivered over SSE
/// to a connected session; DELETE closes the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_changed_updates_catalog_and_reaches_sse() {
    with_timeout(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let mock = MockServerSpec::new().tools(&["echo"]).list_changed_after(1);
        write_file(
            &cfg,
            &format!(
                "{}\n{}",
                broker_toml(dir.path()),
                server_toml("mock", &mock, "")
            ),
        );
        let (d, api) = start_daemon(&cfg).await;
        wait_catalog_with(dir.path(), "mock", "echo").await;
        let session = api.session().await;
        let mut sse = Sse::open(&api, &session).await;
        let r = api
            .rpc(
                Some(&session),
                "tools/call",
                json!({"name":"mock__echo","arguments":{}}),
            )
            .await;
        assert!(r.get("result").is_some(), "{r}");
        sse.wait_for("notifications/tools/list_changed", Duration::from_secs(20))
            .await;
        wait_catalog_with(dir.path(), "mock", "extra").await;
        assert_eq!(api.tool_names().await, ["mock__echo", "mock__extra"]);
        // DELETE ends the session and its stream.
        let del = raw(
            api.addr,
            "DELETE",
            "/mcp",
            &api.token,
            &[("Mcp-Session-Id", &session)],
            "",
        )
        .await;
        assert_eq!(del.status, 200);
        sse.wait_closed(Duration::from_secs(10)).await;
        d.shutdown().await.unwrap();
    })
    .await;
}

/// Progress (by the agent's own token) and backend log messages of an
/// in-flight call arrive over SSE; the POST response stays JSON.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_and_log_messages_over_sse() {
    with_timeout(async {
        let router = mem_router();
        router
            .register(
                "mock",
                "m",
                spec(
                    MockServerSpec::new()
                        .tools(&["slow"])
                        .delay_ms(300)
                        .log_messages(),
                ),
            )
            .unwrap();
        let b = MemBroker::start(router).await;
        let session = b.api.session().await;
        let mut sse = Sse::open(&b.api, &session).await;
        let r = b
            .api
            .rpc(
                Some(&session),
                "tools/call",
                json!({"name":"m__slow","arguments":{},"_meta":{"progressToken":"tok-sse-1"}}),
            )
            .await;
        assert!(r.get("result").is_some(), "{r}");
        let text = sse
            .wait_for("\"progress\":2", Duration::from_secs(10))
            .await;
        assert!(text.contains("\"progressToken\":\"tok-sse-1\""), "{text}");
        assert!(!text.contains("mcpdoze-"), "broker token leaked: {text}");
        sse.wait_for("notifications/message", Duration::from_secs(10))
            .await;
        assert!(sse.text().contains("calling slow"));
        // Another session without a stream got nothing.
        let other = b.router.session(&b.api.session().await).unwrap();
        assert_eq!(other.queued_notifications(), 0);
        b.finish().await;
    })
    .await;
}

/// A call to a tool listed only by a stale catalog returns a normal
/// JSON-RPC error and triggers a refresh that drops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_catalog_call_errors_then_refreshes() {
    with_timeout(async {
        let catalog = Arc::new(MemoryCatalog::default());
        let router = Router::with(catalog.clone(), tuning());
        router
            .register("mock", "m", spec(MockServerSpec::new().tools(&["echo"])))
            .unwrap();
        let s = router.create_session(VERSION);
        let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}});
        router.handle(&s, list.clone()).await.unwrap();
        let before = router.refresh_count();
        catalog.set("mock", vec![json!({"name":"echo"}), json!({"name":"gone"})]);
        let r = router.handle(&s, list.clone()).await.unwrap();
        assert_eq!(r["result"]["tools"][1]["name"], "m__gone");
        let r = router
            .handle(
                &s,
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                    "params":{"name":"m__gone","arguments":{}}}),
            )
            .await
            .unwrap();
        assert_eq!(r["id"], 2);
        assert_eq!(r["error"]["code"], -32602, "{r}");
        wait_until("refresh", Duration::from_secs(10), || {
            router.refresh_count() > before && !catalog.entry("mock").unwrap().has_tool("gone")
        })
        .await;
        let r = router.handle(&s, list).await.unwrap();
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 1, "{r}");
        router.shutdown().await;
    })
    .await;
}

/// prompts/resources/templates are listed with prefixes and mapped URIs,
/// fetched through the mapping, and answered with zero processes once
/// catalogued; subscriptions are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompts_and_resources_round_trip() {
    with_timeout(async {
        let router = mem_router();
        let mock = MockServerSpec::new()
            .tools(&["echo"])
            .prompts(&["greet", "farewell"])
            .resources(&["file:///docs/a b.txt", "mock://readme"])
            .resource_templates(&["mock://items/{id}"]);
        router.register("mock", "m", spec(mock)).unwrap();
        let s = router.create_session(VERSION);
        let rpc = |id: u64, method: &str, params: Value| {
            let router = router.clone();
            let s = s.clone();
            let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
            async move { router.handle(&s, msg).await.unwrap() }
        };
        let p = rpc(1, "prompts/list", json!({})).await;
        let names: Vec<&str> = p["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["m__greet", "m__farewell"]);
        let inst = router.instance("mock").unwrap();
        assert_eq!(inst.spawn_count(), 1);
        let g = rpc(
            2,
            "prompts/get",
            json!({"name":"m__greet","arguments":{"who":"x"}}),
        )
        .await;
        let text = g["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{g}"));
        assert!(text.starts_with("prompt greet"), "{text}");
        let bad = rpc(3, "prompts/get", json!({"name":"m__nope"})).await;
        assert!(bad.get("error").is_some(), "{bad}");

        let r = rpc(4, "resources/list", json!({})).await;
        let uris: Vec<&str> = r["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["uri"].as_str().unwrap())
            .collect();
        assert_eq!(
            uris,
            [
                "mcpdoze://m/file%3A%2F%2F%2Fdocs%2Fa%20b.txt",
                "mcpdoze://m/mock%3A%2F%2Freadme"
            ]
        );
        let read = rpc(5, "resources/read", json!({"uri": uris[0]})).await;
        assert_eq!(read["result"]["contents"][0]["uri"], uris[0], "{read}");
        assert_eq!(
            read["result"]["contents"][0]["text"],
            "contents of file:///docs/a b.txt"
        );
        let t = rpc(6, "resources/templates/list", json!({})).await;
        let tmpl = t["result"]["resourceTemplates"][0]["uriTemplate"]
            .as_str()
            .unwrap_or_else(|| panic!("{t}"))
            .to_owned();
        assert_eq!(tmpl, "mcpdoze://m/mock%3A%2F%2Fitems%2F{id}");
        let read = rpc(
            7,
            "resources/read",
            json!({"uri": tmpl.replace("{id}", "42")}),
        )
        .await;
        assert_eq!(
            read["result"]["contents"][0]["text"], "contents of mock://items/42",
            "{read}"
        );
        assert_eq!(
            read["result"]["contents"][0]["uri"],
            "mcpdoze://m/mock%3A%2F%2Fitems%2F42"
        );
        let unknown = rpc(8, "resources/read", json!({"uri":"mcpdoze://zz/x"})).await;
        assert_eq!(unknown["error"]["code"], -32602);
        let sub = rpc(9, "resources/subscribe", json!({"uri": uris[0]})).await;
        assert_eq!(sub["error"]["code"], -32601);

        // Catalogued: lists need no process.
        inst.shutdown().await;
        assert_eq!(inst.state(), StateKind::Stopped);
        let spawns = inst.spawn_count();
        for m in [
            "tools/list",
            "prompts/list",
            "resources/list",
            "resources/templates/list",
        ] {
            let r = rpc(10, m, json!({})).await;
            assert!(r.get("result").is_some(), "{m}: {r}");
        }
        assert_eq!(inst.spawn_count(), spawns);
        assert_eq!(inst.state(), StateKind::Stopped);
        router.shutdown().await;
    })
    .await;
}

/// The per-session queue is bounded (oldest dropped and counted), a stream
/// delivers what is queued, and idle streams get keep-alive comments.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_queue_bound_and_keepalive() {
    with_timeout(async {
        let router = mem_router();
        router.set_sse_keepalive(Duration::from_millis(150));
        let b = MemBroker::start(router).await;
        let id = b.api.session().await;
        let session = b.router.session(&id).unwrap();
        let cap = mcpdoze_core::router::SESSION_OUTBOX;
        for n in 0..(cap + 500) {
            session.push(json!({"jsonrpc":"2.0","method":"notifications/test","params":{"n":n}}));
        }
        assert_eq!(session.queued_notifications(), cap);
        assert_eq!(session.dropped_notifications(), 500);
        let mut sse = Sse::open(&b.api, &id).await;
        let last = format!("{{\"n\":{}}}", cap + 499);
        let text = sse.wait_for(&last, Duration::from_secs(10)).await;
        assert!(text.contains("{\"n\":500}"), "oldest kept entry missing");
        assert!(!text.contains("{\"n\":499}"), "dropped entry delivered");
        sse.wait_for(": keep-alive", Duration::from_secs(5)).await;
        // GET without Accept: text/event-stream, or unknown session.
        let r = raw(
            b.api.addr,
            "GET",
            "/mcp",
            TOKEN,
            &[("Mcp-Session-Id", &id)],
            "",
        )
        .await;
        assert_eq!(r.status, 406);
        let r = raw(
            b.api.addr,
            "GET",
            "/mcp",
            TOKEN,
            &[("Mcp-Session-Id", "nope"), ("Accept", "text/event-stream")],
            "",
        )
        .await;
        assert_eq!(r.status, 404);
        // Shutdown closes the open stream instead of hanging on it.
        b.finish().await;
        sse.wait_closed(Duration::from_secs(10)).await;
    })
    .await;
}
