// SPDX-License-Identifier: GPL-3.0-or-later

//! M5a acceptance: security layers (Host / Origin / bearer token), the admin
//! API, config write-back, hot reload and graceful shutdown, against the
//! mock stdio server. Every test has a hard timeout; brokers are shut down
//! at the end and dropping them kills any remaining process tree.

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[path = "support/log_capture.rs"]
mod log_capture;
use log_capture::Capture;
use std::time::Duration;

use mcpdoze_core::auth::Token;
use mcpdoze_core::backend::Tuning;
use mcpdoze_core::daemon::{Daemon, ServeOptions};
use mcpdoze_core::http::Security;
use mcpdoze_core::router::{MemoryCatalog, Router};
use mcpdoze_testkit::MockServerSpec;
use serde_json::{json, Value};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_mock-mcp-stdio");
const TEST_TIMEOUT: Duration = Duration::from_secs(90);
const VERSION: &str = "2025-11-25";
/// Bearer token of the in-memory test brokers (not a real secret).
const TOKEN: &str = "admin-test-token-5d1e0c";

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

/// One HTTP/1.1 request. `host: None` omits the Host header; `token: None`
/// omits Authorization.
async fn raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: Option<&str>,
    token: Option<&str>,
    extra: &[(&str, &str)],
    body: &str,
) -> Resp {
    let mut req = format!("{method} {path} HTTP/1.1\r\n");
    if let Some(h) = host {
        req.push_str(&format!("Host: {h}\r\n"));
    }
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!(
        "Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ));
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
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

/// Authenticated client for one broker.
#[derive(Clone)]
struct Api {
    addr: SocketAddr,
    token: String,
}

impl Api {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Resp {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        raw(
            self.addr,
            method,
            path,
            Some("127.0.0.1"),
            Some(&self.token),
            &[],
            &body,
        )
        .await
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let r = self.call(method, path, body).await;
        assert!((200..300).contains(&r.status), "{method} {path}: {r:?}");
        r.json()
    }

    /// Stateless `tools/call`; returns the JSON-RPC response.
    async fn tool(&self, name: &str) -> Value {
        let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":name,"arguments":{}}})
        .to_string();
        let r = raw(
            self.addr,
            "POST",
            "/mcp",
            Some("127.0.0.1"),
            Some(&self.token),
            &[("MCP-Protocol-Version", VERSION)],
            &body,
        )
        .await;
        assert_eq!(r.status, 200, "{r:?}");
        r.json()
    }

    async fn status_of(&self, name: &str) -> Option<Value> {
        let s = self.ok("GET", "/admin/v1/status", None).await;
        s["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == name)
            .cloned()
    }
}

fn pid_of(resp: &Value) -> u32 {
    resp["result"]["structuredContent"]["pid"]
        .as_u64()
        .unwrap_or_else(|| panic!("no pid in {resp}")) as u32
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
        sleep(Duration::from_millis(50)).await;
    }
}

// --------------------------------------------------------- brokers

/// An in-memory broker (no config file) on an ephemeral port.
struct MemBroker {
    api: Api,
    router: Arc<Router>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl MemBroker {
    async fn start(allowed_hosts: Vec<String>) -> MemBroker {
        let router = Router::with(Arc::new(MemoryCatalog::default()), tuning());
        let listener = mcpdoze_core::http::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let sec = Security::new(Token::from_string(TOKEN.into()).unwrap(), allowed_hosts);
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

fn toml_str(s: &str) -> String {
    // JSON string escaping is valid TOML basic-string escaping.
    serde_json::to_string(s).unwrap()
}

fn mock_server_toml(name: &str, mock: &MockServerSpec) -> String {
    format!(
        "[servers.{name}]\ntransport = \"stdio\"\ncommand = {}\nargs = {}\nidle_ttl = \"30s\"\n",
        toml_str(BIN),
        serde_json::to_string(&mock.args()).unwrap()
    )
}

fn broker_toml(dir: &Path) -> String {
    format!(
        "[broker]\nbind = \"127.0.0.1:0\"\ntoken_file = {}\ncatalog_dir = {}\n",
        toml_str(&dir.join("token").to_string_lossy()),
        toml_str(&dir.join("catalog").to_string_lossy())
    )
}

fn write_file(path: &Path, text: &str) {
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
    f.sync_all().unwrap();
}

async fn start_daemon(config: &Path, watch: bool) -> (Daemon, Api) {
    let d = Daemon::start(ServeOptions {
        config_path: Some(config.to_path_buf()),
        bind: None,
        watch,
        tuning: tuning(),
        catalog: None,
    })
    .await
    .expect("daemon start");
    let token = std::fs::read_to_string(d.token_file().expect("token file"))
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(token.len(), 64);
    let api = Api {
        addr: d.local_addr(),
        token,
    };
    (d, api)
}

// ----------------------------------------------------------------- tests

/// Host / Origin (DNS rebinding), bearer token, open /healthz.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_origin_and_token_checks() {
    with_timeout(async {
        let b = MemBroker::start(vec!["broker.example.com".into()]).await;
        let a = b.addr();
        let ping = json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string();
        let proto = ("MCP-Protocol-Version", VERSION);

        // DNS-rebinding style Host names are refused before auth.
        for host in [
            "evil.example.com",
            "localhost.example.com",
            "127.0.0.1.example.com",
            "example.com:7878",
            "localhost@example.com",
            "[::2]",
            "",
        ] {
            let r = raw(a, "POST", "/mcp", Some(host), Some(TOKEN), &[proto], &ping).await;
            assert_eq!(r.status, 403, "host {host:?}: {r:?}");
        }
        let r = raw(a, "POST", "/mcp", None, Some(TOKEN), &[proto], &ping).await;
        assert!(r.status == 403 || r.status == 400, "missing host: {r:?}");
        // Allowed names, with and without port.
        for host in [
            "127.0.0.1",
            "localhost:7878",
            "[::1]:1",
            "broker.example.com",
        ] {
            let r = raw(a, "POST", "/mcp", Some(host), Some(TOKEN), &[proto], &ping).await;
            assert_eq!(r.status, 200, "host {host:?}: {r:?}");
        }

        // Origin.
        for origin in [
            "http://evil.example.com",
            "null",
            "file://",
            "http://localhost.example.com:80",
        ] {
            let r = raw(
                a,
                "POST",
                "/mcp",
                Some("127.0.0.1"),
                Some(TOKEN),
                &[proto, ("Origin", origin)],
                &ping,
            )
            .await;
            assert_eq!(r.status, 403, "origin {origin:?}: {r:?}");
        }
        let r = raw(
            a,
            "POST",
            "/mcp",
            Some("127.0.0.1"),
            Some(TOKEN),
            &[proto, ("Origin", "http://localhost:3000")],
            &ping,
        )
        .await;
        assert_eq!(r.status, 200, "{r:?}");

        // Token: missing, wrong, wrong scheme -> 401 with a challenge.
        for (tok, extra) in [
            (None, None),
            (Some("wrong-token"), None),
            (Some(&TOKEN[..TOKEN.len() - 1]), None),
            (None, Some(("Authorization", "Basic dXNlcjpwYXNz"))),
        ] {
            let mut hdrs = vec![proto];
            hdrs.extend(extra);
            for path in ["/mcp", "/admin/v1/status"] {
                let r = raw(a, "POST", path, Some("127.0.0.1"), tok, &hdrs, &ping).await;
                assert_eq!(r.status, 401, "{path} {tok:?}: {r:?}");
                assert_eq!(r.header("www-authenticate").as_deref(), Some("Bearer"));
                assert!(!r.body.contains(TOKEN));
            }
        }
        let r = raw(
            a,
            "POST",
            "/mcp",
            Some("127.0.0.1"),
            Some(TOKEN),
            &[proto],
            &ping,
        )
        .await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["id"], 1);
        let r = raw(
            a,
            "GET",
            "/admin/v1/status",
            Some("localhost"),
            Some(TOKEN),
            &[],
            "",
        )
        .await;
        assert_eq!(r.status, 200, "{r:?}");

        // /healthz needs no token but still checks Host.
        let r = raw(a, "GET", "/healthz", Some("127.0.0.1"), None, &[], "").await;
        assert_eq!((r.status, r.body.as_str()), (200, "ok"));
        let r = raw(
            a,
            "GET",
            "/healthz",
            Some("evil.example.com"),
            None,
            &[],
            "",
        )
        .await;
        assert_eq!(r.status, 403);
        b.finish().await;
    })
    .await;
}

impl MemBroker {
    fn addr(&self) -> SocketAddr {
        self.api.addr
    }
}

/// The token (right or wrong) never reaches the logs, at TRACE level, across
/// auth failures, MCP calls and admin calls. Single-threaded runtime so every
/// task logs on the capturing thread (see `support/log_capture.rs`).
#[tokio::test(flavor = "current_thread")]
async fn token_never_logged() {
    let cap = Capture::start();
    with_timeout(async {
        let b = MemBroker::start(Vec::new()).await;
        let reg = json!({"name":"mock","server":{"transport":"stdio","command":BIN,
            "args": MockServerSpec::new().tools(&["pid"]).stderr_noise().args()}});
        b.api.ok("POST", "/admin/v1/servers", Some(reg)).await;
        let wrong = "wrong-token-7c2a91";
        for tok in [None, Some(wrong), Some(TOKEN)] {
            let _ = raw(
                b.addr(),
                "GET",
                "/admin/v1/status",
                Some("127.0.0.1"),
                tok,
                &[],
                "",
            )
            .await;
        }
        let _ = raw(
            b.addr(),
            "POST",
            "/mcp",
            Some("evil.example.com"),
            Some(TOKEN),
            &[],
            "{}",
        )
        .await;
        let pid = pid_of(&b.api.tool("mock__pid").await);
        b.api
            .ok("POST", "/admin/v1/servers/mock/stop?force=true", None)
            .await;
        assert!(!alive(pid));
        b.finish().await;
        let logs = cap.text();
        // Both lines are emitted before the matching response is returned,
        // so they prove the capture saw the broker's events.
        assert!(
            logs.contains("backend running"),
            "nothing captured:\n{logs}"
        );
        assert!(
            logs.contains("bearer token"),
            "auth failure not logged:\n{logs}"
        );
        assert!(!logs.contains(TOKEN), "token leaked into logs");
        assert!(!logs.contains(wrong), "presented token leaked into logs");
        assert!(!logs.to_ascii_lowercase().contains("authorization:"));
    })
    .await;
}

/// Register, call, status, logs, stop (drain and force), remove; errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_register_status_stop_remove_in_memory() {
    with_timeout(async {
        let b = MemBroker::start(Vec::new()).await;
        let api = b.api.clone();
        let mock = MockServerSpec::new()
            .tools(&["pid", "spawn_child"])
            .stderr_noise();
        let reg = json!({"name":"mock","server":{"transport":"stdio","command":BIN,
            "args": mock.args(), "idle_ttl":"30s", "prefix":"m"}});
        let r = api
            .call("POST", "/admin/v1/servers", Some(reg.clone()))
            .await;
        assert_eq!(r.status, 201, "{r:?}");
        let r = r.json();
        assert_eq!(r["persisted"], false);
        assert_eq!(r["server"]["registered"], true);
        assert_eq!(r["server"]["prefix"], "m");

        // Errors: duplicate, bad name, literal secret, missing env, bad body.
        assert_eq!(
            api.call("POST", "/admin/v1/servers", Some(reg))
                .await
                .status,
            409
        );
        let bad_name = json!({"name":"a b","server":{"transport":"stdio","command":"x"}});
        assert_eq!(
            api.call("POST", "/admin/v1/servers", Some(bad_name))
                .await
                .status,
            400
        );
        let secret = json!({"name":"s","server":{"transport":"stdio","command":"x",
            "env":{"API_TOKEN":"literal-value"}}});
        let r = api.call("POST", "/admin/v1/servers", Some(secret)).await;
        assert_eq!(r.status, 400, "{r:?}");
        assert!(r.json()["details"][0]
            .as_str()
            .unwrap()
            .contains("servers.s.env.API_TOKEN"));
        assert!(!r.body.contains("literal-value"));
        let missing = json!({"name":"e","server":{"transport":"stdio","command":"x",
            "env":{"K":{"from_env":"MCPDOZE_TEST_SURELY_UNSET_VAR"}}}});
        assert_eq!(
            api.call("POST", "/admin/v1/servers", Some(missing))
                .await
                .status,
            400
        );
        let r = api
            .call("POST", "/admin/v1/servers", Some(json!({"nope": 1})))
            .await;
        assert_eq!(r.status, 400);

        // Lazy: registered but not started.
        let s = api.status_of("mock").await.unwrap();
        assert_eq!(s["state"], "stopped");
        assert_eq!(s["transport"], "stdio");
        let pid = pid_of(&api.tool("m__pid").await);
        let child = pid_of(&api.tool("m__spawn_child").await);
        let s = api.status_of("mock").await.unwrap();
        assert_eq!(s["state"], "running", "{s}");
        let pids: Vec<u64> = s["pids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_u64().unwrap())
            .collect();
        assert!(pids.contains(&(pid as u64)), "{s}");
        assert!(pids.contains(&(child as u64)), "{s}");
        assert!(s["rss_bytes"].as_u64().unwrap() > 0, "{s}");
        assert!(s["private_bytes"].as_u64().unwrap() > 0, "{s}");
        assert_eq!(s["calls"], 2);
        assert_eq!(s["in_flight"], 0);
        assert!(s["idle_remaining_secs"].as_u64().unwrap() <= 30);
        assert_eq!(s["restarts"], 0);

        let servers = api.ok("GET", "/admin/v1/servers", None).await;
        assert_eq!(servers["servers"][0]["name"], "mock");
        assert_eq!(servers["servers"][0]["config"]["command"], BIN);

        let logs = api
            .ok("GET", "/admin/v1/servers/mock/logs?lines=2", None)
            .await;
        let lines = logs["lines"].as_array().unwrap();
        assert!(!lines.is_empty() && lines.len() <= 2, "{logs}");
        assert!(lines[0].as_str().unwrap().contains("[mock"), "{logs}");

        // Drain stop: process tree gone, still registered, restarts lazily.
        let r = api.ok("POST", "/admin/v1/servers/mock/stop", None).await;
        assert_eq!(r["state"], "stopped");
        wait_until("tree gone", Duration::from_secs(10), || {
            !alive(pid) && !alive(child)
        })
        .await;
        let pid2 = pid_of(&api.tool("m__pid").await);
        assert_ne!(pid, pid2);
        assert_eq!(api.status_of("mock").await.unwrap()["restarts"], 1);
        let r = api
            .ok("POST", "/admin/v1/servers/mock/stop?force=true", None)
            .await;
        assert_eq!(r["state"], "stopped");
        assert!(!alive(pid2));

        // Unknown names, no config file to reload.
        for (m, p) in [
            ("POST", "/admin/v1/servers/nope/stop"),
            ("GET", "/admin/v1/servers/nope/logs"),
            ("DELETE", "/admin/v1/servers/nope"),
        ] {
            assert_eq!(api.call(m, p, None).await.status, 404, "{m} {p}");
        }
        assert_eq!(api.call("POST", "/admin/v1/reload", None).await.status, 409);

        // Remove: unregistered, tools gone.
        let pid3 = pid_of(&api.tool("m__pid").await);
        let r = api.ok("DELETE", "/admin/v1/servers/mock", None).await;
        assert_eq!(r["name"], "mock");
        assert!(!alive(pid3));
        assert!(api.status_of("mock").await.is_none());
        let r = api.tool("m__pid").await;
        assert!(r.get("error").is_some(), "{r}");
        assert_eq!(
            api.call("DELETE", "/admin/v1/servers/mock", None)
                .await
                .status,
            404
        );
        b.finish().await;
    })
    .await;
}

/// Admin add/remove edit the config file atomically and keep comments;
/// `POST /reload` applies external edits (change and removal).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_write_back_and_reload() {
    with_timeout(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let alpha = MockServerSpec::new().tools(&["pid"]);
        let original = format!(
            "# Example MCPDoze config\n{}\n# alpha: the first mock\n{}",
            broker_toml(dir.path()),
            mock_server_toml("alpha", &alpha)
        );
        write_file(&cfg, &original);
        let (d, api) = start_daemon(&cfg, false).await;
        assert!(api.status_of("alpha").await.is_some());

        let reg = json!({"name":"beta","server":{"transport":"stdio","command":BIN,
            "args": MockServerSpec::new().tools(&["pid"]).args(),
            "env": {"EXAMPLE_KEY": {"from_env": "PATH"}}}});
        let r = api.ok("POST", "/admin/v1/servers", Some(reg)).await;
        assert_eq!(r["persisted"], true);
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            text.starts_with(&original),
            "existing content changed:\n{text}"
        );
        assert!(text.contains("[servers.beta]"), "{text}");
        assert!(
            text.contains(r#"env = { EXAMPLE_KEY = { from_env = "PATH" } }"#),
            "{text}"
        );
        let pid = pid_of(&api.tool("beta__pid").await);

        // Remove beta: file back to the original, backend stopped.
        api.ok("DELETE", "/admin/v1/servers/beta", None).await;
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(text.trim_end(), original.trim_end());
        assert!(!alive(pid));
        let leftovers: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // External edit: alpha changes (new tool list) -> unloaded, re-registered.
        let p1 = pid_of(&api.tool("alpha__pid").await);
        let changed = original.replace("\"pid\"", "\"pid,echo\"");
        assert_ne!(changed, original);
        write_file(&cfg, &changed);
        let r = api.ok("POST", "/admin/v1/reload", None).await;
        assert_eq!(r["changed"], json!(["alpha"]), "{r}");
        assert!(!alive(p1));
        let p2 = pid_of(&api.tool("alpha__pid").await);
        assert_ne!(p1, p2);
        // An invalid file is refused and the running config kept.
        write_file(&cfg, "[servers.x]\ntransport = \"nope\"\n");
        assert_eq!(api.call("POST", "/admin/v1/reload", None).await.status, 400);
        assert!(api.status_of("alpha").await.is_some());
        // Removal.
        write_file(&cfg, &broker_toml(dir.path()));
        let r = api.ok("POST", "/admin/v1/reload", None).await;
        assert_eq!(r["removed"], json!(["alpha"]), "{r}");
        assert!(!alive(p2));
        assert!(api.status_of("alpha").await.is_none());
        d.shutdown().await.unwrap();
    })
    .await;
}

/// The file watcher picks up an added server without any admin call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hot_reload_adds_server() {
    with_timeout(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        write_file(&cfg, &broker_toml(dir.path()));
        let (d, api) = start_daemon(&cfg, true).await;
        assert!(api.status_of("late").await.is_none());
        let text = format!(
            "{}\n{}",
            broker_toml(dir.path()),
            mock_server_toml("late", &MockServerSpec::new().tools(&["pid"]))
        );
        write_file(&cfg, &text);
        let deadline = Instant::now() + Duration::from_secs(20);
        while api.status_of("late").await.is_none() {
            assert!(
                Instant::now() < deadline,
                "hot reload never registered the server"
            );
            sleep(Duration::from_millis(100)).await;
        }
        let pid = pid_of(&api.tool("late__pid").await);
        d.shutdown().await.unwrap();
        assert!(!alive(pid));
    })
    .await;
}

/// Shutdown stops every backend tree, grandchildren included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_shutdown_leaves_no_processes() {
    with_timeout(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let mock = MockServerSpec::new().tools(&["pid", "spawn_child"]);
        let text = format!(
            "{}\n{}\n{}",
            broker_toml(dir.path()),
            mock_server_toml("one", &mock),
            mock_server_toml("two", &mock)
        );
        write_file(&cfg, &text);
        let (d, api) = start_daemon(&cfg, true).await;
        let mut pids = Vec::new();
        for s in ["one", "two"] {
            pids.push(pid_of(&api.tool(&format!("{s}__pid")).await));
            pids.push(pid_of(&api.tool(&format!("{s}__spawn_child")).await));
        }
        assert!(pids.iter().all(|p| alive(*p)), "{pids:?}");
        d.shutdown().await.unwrap();
        wait_until("all processes gone", Duration::from_secs(10), || {
            pids.iter().all(|p| !alive(*p))
        })
        .await;
        // The port is closed.
        assert!(TcpStream::connect(api.addr).await.is_err());
    })
    .await;
}
