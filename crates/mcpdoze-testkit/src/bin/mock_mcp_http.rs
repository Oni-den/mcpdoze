// SPDX-License-Identifier: GPL-3.0-or-later

//! Mock Streamable HTTP MCP server for tests. Listens on 127.0.0.1 with an
//! ephemeral port and prints that port as the first stdout line. Exits when
//! its stdin closes.
//!
//! Flags: `--tools a,b` (extra tools), `--sse` (stream responses as
//! text/event-stream with a progress notification first), `--require-header
//! NAME=VALUE` (401 otherwise), `--drop-session-after N` (a session is dropped
//! after N requests: later requests with it get 404 until re-initialised),
//! `--slow-ms N`, `--reject-calls N` (the first N `tools/call` requests get
//! 401), `--subscribe` (advertise `resources.subscribe`, serve
//! subscribe/unsubscribe and keep `GET` event streams open; tool `touch`
//! sends `notifications/resources/updated` on them, tool `sub_stats`
//! reports counters), `--status N` (every `tools/call` is answered with HTTP
//! status N and a plain-text body), `--html-error` (every `tools/call` is
//! answered with an HTML error page; status 502 unless `--status` is given).
//! Built-in tools: `echo`, `session_count` (initialize calls seen) and
//! `delete_count` (DELETE requests seen).
//!
//! Minimal hand-written HTTP/1.1 (keep-alive, Content-Length bodies, chunked
//! SSE responses) so the testkit needs no HTTP framework.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const MAX_BODY: usize = 1024 * 1024;

#[derive(Default, Clone)]
struct Cfg {
    tools: Vec<String>,
    sse: bool,
    require: Option<(String, String)>,
    drop_after: Option<u64>,
    slow_ms: u64,
    reject_calls: u64,
    subscribe: bool,
    status: Option<u16>,
    html_error: bool,
}

#[derive(Default)]
struct State {
    inits: u64,
    deletes: u64,
    rejected: u64,
    next: u64,
    sessions: HashMap<String, u64>,
    subscribes: u64,
    unsubscribes: u64,
    /// Open `GET` event streams.
    listeners: Vec<tokio::sync::mpsc::UnboundedSender<String>>,
}

struct Req {
    method: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn parse_args() -> Cfg {
    let mut cfg = Cfg::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--tools" => {
                cfg.tools = it
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
            "--sse" => cfg.sse = true,
            "--subscribe" => cfg.subscribe = true,
            "--require-header" => {
                let v = it.next().unwrap_or_default();
                if let Some((k, v)) = v.split_once('=') {
                    cfg.require = Some((k.to_ascii_lowercase(), v.to_owned()));
                }
            }
            "--drop-session-after" => cfg.drop_after = it.next().and_then(|v| v.parse().ok()),
            "--slow-ms" => cfg.slow_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--status" => cfg.status = it.next().and_then(|v| v.parse().ok()),
            "--html-error" => cfg.html_error = true,
            "--reject-calls" => {
                cfg.reject_calls = it.next().and_then(|v| v.parse().ok()).unwrap_or(0)
            }
            _ => {}
        }
    }
    cfg
}

#[tokio::main]
async fn main() {
    let cfg = Arc::new(parse_args());
    let state = Arc::new(Mutex::new(State::default()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    println!("{}", listener.local_addr().expect("addr").port());
    // The port line must reach the parent even through a pipe.
    std::io::Write::flush(&mut std::io::stdout()).ok();

    tokio::spawn(async {
        let mut buf = [0u8; 64];
        let mut stdin = tokio::io::stdin();
        while matches!(stdin.read(&mut buf).await, Ok(n) if n > 0) {}
        std::process::exit(0);
    });

    loop {
        let Ok((sock, _)) = listener.accept().await else {
            continue;
        };
        let (cfg, state) = (cfg.clone(), state.clone());
        tokio::spawn(async move {
            let _ = serve(sock, cfg, state).await;
        });
    }
}

async fn read_req(r: &mut BufReader<TcpStream>) -> std::io::Result<Option<Req>> {
    let mut line = String::new();
    if r.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    let method = line.split_whitespace().next().unwrap_or("").to_owned();
    let mut headers = HashMap::new();
    loop {
        line.clear();
        if r.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .min(MAX_BODY);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(Req {
        method,
        headers,
        body,
    }))
}

async fn simple(
    s: &mut BufReader<TcpStream>,
    status: &str,
    extra: &[(&str, &str)],
    ctype: &str,
    body: &str,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    head.push_str(body);
    s.write_all(head.as_bytes()).await?;
    s.flush().await
}

async fn serve(sock: TcpStream, cfg: Arc<Cfg>, state: Arc<Mutex<State>>) -> std::io::Result<()> {
    let mut s = BufReader::new(sock);
    while let Some(req) = read_req(&mut s).await? {
        handle(&mut s, req, &cfg, &state).await?;
    }
    Ok(())
}

fn lock(s: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

async fn handle(
    s: &mut BufReader<TcpStream>,
    req: Req,
    cfg: &Cfg,
    state: &Mutex<State>,
) -> std::io::Result<()> {
    if let Some((k, v)) = &cfg.require {
        if req.headers.get(k) != Some(v) {
            return simple(s, "401 Unauthorized", &[], "text/plain", "unauthorized").await;
        }
    }
    let sid = req.headers.get("mcp-session-id").cloned();
    if req.method == "DELETE" {
        {
            let mut st = lock(state);
            st.deletes += 1;
            if let Some(id) = &sid {
                st.sessions.remove(id);
            }
        }
        return simple(s, "200 OK", &[], "text/plain", "").await;
    }
    if req.method == "GET" && cfg.subscribe {
        let known = sid
            .as_ref()
            .is_some_and(|k| lock(state).sessions.contains_key(k));
        if !known {
            return simple(s, "404 Not Found", &[], "text/plain", "unknown session").await;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        lock(state).listeners.push(tx);
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                    Transfer-Encoding: chunked\r\n\r\n";
        s.write_all(head.as_bytes()).await?;
        s.flush().await?;
        let mut scratch = [0u8; 256];
        loop {
            // Also watch the socket, so a client that goes away ends this.
            let next = tokio::select! {
                m = rx.recv() => m,
                r = s.read(&mut scratch) => match r {
                    Ok(n) if n > 0 => continue,
                    _ => None,
                },
            };
            let Some(m) = next else { break };
            let ev = format!("event: message\ndata: {m}\n\n");
            s.write_all(format!("{:x}\r\n{ev}\r\n", ev.len()).as_bytes())
                .await?;
            s.flush().await?;
        }
        return Ok(());
    }
    if req.method != "POST" {
        return simple(s, "405 Method Not Allowed", &[], "text/plain", "").await;
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&req.body) else {
        return simple(s, "400 Bad Request", &[], "text/plain", "bad json").await;
    };
    let method = msg["method"].as_str().unwrap_or("").to_owned();
    let id = msg.get("id").cloned().filter(|i| !i.is_null());
    if method == "tools/call" {
        let reject = {
            let mut st = lock(state);
            let r = st.rejected < cfg.reject_calls;
            if r {
                st.rejected += 1;
            }
            r
        };
        if reject {
            return simple(s, "401 Unauthorized", &[], "text/plain", "expired").await;
        }
        if cfg.html_error || cfg.status.is_some() {
            let code = cfg.status.unwrap_or(502);
            let status = format!("{code} Mock Error");
            let (ctype, body) = if cfg.html_error {
                (
                    "text/html",
                    format!("<html><body><h1>{code} Mock Error</h1></body></html>"),
                )
            } else {
                ("text/plain", format!("mock error {code}"))
            };
            return simple(s, &status, &[], ctype, &body).await;
        }
    }

    if method == "initialize" {
        let new_sid = {
            let mut st = lock(state);
            st.inits += 1;
            st.next += 1;
            let n = st.next;
            let sid = format!("sess-{n}");
            st.sessions.insert(sid.clone(), 0);
            sid
        };
        let body = json!({"jsonrpc":"2.0","id":id,"result":{
            "protocolVersion": "2025-11-25",
            "capabilities": if cfg.subscribe {
                json!({"tools": {}, "resources": {"subscribe": true}})
            } else {
                json!({"tools": {}})
            },
            "serverInfo": {"name": "mock-mcp-http", "version": "0.0.1"}}});
        return simple(
            s,
            "200 OK",
            &[("Mcp-Session-Id", &new_sid)],
            "application/json",
            &body.to_string(),
        )
        .await;
    }

    // Session validation and the drop-after counter.
    let valid = {
        let mut st = lock(state);
        match sid
            .as_ref()
            .and_then(|k| st.sessions.get_mut(k).map(|c| (k, c)))
        {
            None => false,
            Some((k, count)) => {
                if id.is_some() {
                    *count += 1;
                }
                if cfg.drop_after.is_some_and(|n| *count > n) {
                    let k = k.clone();
                    st.sessions.remove(&k);
                    false
                } else {
                    true
                }
            }
        }
    };
    if !valid {
        return simple(s, "404 Not Found", &[], "text/plain", "unknown session").await;
    }
    let Some(id) = id else {
        return simple(s, "202 Accepted", &[], "text/plain", "").await;
    };
    if cfg.slow_ms > 0 {
        tokio::time::sleep(Duration::from_millis(cfg.slow_ms)).await;
    }
    let reply = answer(&method, &msg, id, cfg, state);
    if cfg.sse {
        let progress = json!({"jsonrpc":"2.0","method":"notifications/progress","params":{
            "progressToken": msg["params"]["_meta"]["progressToken"].clone(),
            "progress": 1, "total": 2}});
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                    Transfer-Encoding: chunked\r\n\r\n";
        s.write_all(head.as_bytes()).await?;
        for (i, m) in [progress, reply].iter().enumerate() {
            if i > 0 {
                s.flush().await?;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let ev = format!("event: message\ndata: {m}\n\n");
            s.write_all(format!("{:x}\r\n{ev}\r\n", ev.len()).as_bytes())
                .await?;
            s.flush().await?;
        }
        s.write_all(b"0\r\n\r\n").await?;
        return s.flush().await;
    }
    simple(s, "200 OK", &[], "application/json", &reply.to_string()).await
}

fn answer(method: &str, msg: &Value, id: Value, cfg: &Cfg, state: &Mutex<State>) -> Value {
    let ok = |r: Value| json!({"jsonrpc":"2.0","id":id.clone(),"result":r});
    match method {
        "ping" => ok(json!({})),
        "resources/list" if cfg.subscribe => ok(json!({"resources": [
            {"uri": "mock://remote", "name": "remote", "mimeType": "text/plain"}]})),
        "resources/subscribe" | "resources/unsubscribe" if cfg.subscribe => {
            let mut st = lock(state);
            if method == "resources/subscribe" {
                st.subscribes += 1;
            } else {
                st.unsubscribes += 1;
            }
            ok(json!({}))
        }
        "tools/list" => {
            let mut names = vec!["echo", "session_count", "delete_count"];
            if cfg.subscribe {
                names.extend(["touch", "sub_stats"]);
            }
            names.extend(cfg.tools.iter().map(String::as_str));
            let tools: Vec<Value> = names
                .iter()
                .map(|n| {
                    json!({"name": n, "description": "mock tool",
                    "inputSchema": {"type": "object"}})
                })
                .collect();
            ok(json!({"tools": tools}))
        }
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or("");
            let args = &msg["params"]["arguments"];
            let text = match name {
                "session_count" => lock(state).inits.to_string(),
                "delete_count" => lock(state).deletes.to_string(),
                "touch" => {
                    let uri = args["uri"].as_str().unwrap_or("mock://remote").to_owned();
                    let note = json!({"jsonrpc":"2.0","method":"notifications/resources/updated",
                        "params":{"uri":uri}})
                    .to_string();
                    let mut st = lock(state);
                    st.listeners.retain(|l| l.send(note.clone()).is_ok());
                    format!("listeners:{}", st.listeners.len())
                }
                "sub_stats" => {
                    let mut st = lock(state);
                    st.listeners.retain(|l| !l.is_closed());
                    json!({"subscribes": st.subscribes, "unsubscribes": st.unsubscribes,
                        "listeners": st.listeners.len()})
                    .to_string()
                }
                n if n == "echo" || cfg.tools.iter().any(|t| t == n) => {
                    format!("{n}:{}", args)
                }
                _ => {
                    return json!({"jsonrpc":"2.0","id":id,"error":
                        {"code": -32602, "message": "unknown tool"}})
                }
            };
            ok(json!({"content": [{"type": "text", "text": text}]}))
        }
        _ => json!({"jsonrpc":"2.0","id":id,"error":
            {"code": -32601, "message": "method not found"}}),
    }
}
