// SPDX-License-Identifier: GPL-3.0-or-later
//! Scriptable mock MCP stdio server (newline-delimited JSON-RPC 2.0).

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_VERSION: &str = "2025-11-25";

struct Config {
    name: String,
    tools: Vec<String>,
    delay_ms: u64,
    crash_after: Option<u64>,
    garbage: bool,
    list_changed_after: Option<u64>,
    sampling: bool,
    noise: bool,
    startup_delay_ms: u64,
}

struct State {
    cfg: Config,
    out: Mutex<std::io::Stdout>,
    calls: AtomicU64,
    extra: AtomicBool,
    next_sid: AtomicU64,
    waiters: Mutex<HashMap<String, Sender<Value>>>,
    cancelled: Mutex<HashSet<String>>,
}

impl State {
    fn send(&self, v: &Value) {
        let mut o = self.out.lock().unwrap();
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    }
    fn log(&self, msg: &str) {
        if self.cfg.noise {
            eprintln!("[mock {}] {msg}", self.cfg.name);
        }
    }
    fn tool_names(&self) -> Vec<String> {
        let mut t = self.cfg.tools.clone();
        if self.extra.load(Ordering::SeqCst) {
            t.push("extra".into());
        }
        t
    }
}

fn parse_args() -> Result<Option<Config>, String> {
    let mut c = Config {
        name: "mock".into(),
        tools: vec!["echo".into()],
        delay_ms: 0,
        crash_after: None,
        garbage: false,
        list_changed_after: None,
        sampling: false,
        noise: false,
        startup_delay_ms: 0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        let num = |s: String| s.parse::<u64>().map_err(|e| format!("bad number: {e}"));
        match a.as_str() {
            "--name" => c.name = val()?,
            "--tools" => {
                c.tools = val()?
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            }
            "--delay-ms" => c.delay_ms = num(val()?)?,
            "--crash-after" => c.crash_after = Some(num(val()?)?),
            "--list-changed-after" => c.list_changed_after = Some(num(val()?)?),
            "--startup-delay-ms" => c.startup_delay_ms = num(val()?)?,
            "--garbage-on-start" => c.garbage = true,
            "--sampling-request" => c.sampling = true,
            "--stderr-noise" => c.noise = true,
            "--sleep-forever" => return Ok(None),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(Some(c))
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

fn text_result(text: String, structured: Value) -> Value {
    json!({"content":[{"type":"text","text":text}],"structuredContent":structured,"isError":false})
}

fn sleep_cancellable(st: &State, key: &str, ms: u64) -> bool {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        if st.cancelled.lock().unwrap().contains(key) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

fn handle_call(st: Arc<State>, id: Value, params: Value) {
    let key = id_key(&id);
    let n = st.calls.fetch_add(1, Ordering::SeqCst);
    if st.cfg.crash_after.is_some_and(|c| n >= c) {
        st.log("crashing");
        std::process::exit(3);
    }
    let tool = params["name"].as_str().unwrap_or("").to_string();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    if !st.tool_names().contains(&tool) {
        st.send(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":format!("unknown tool {tool}")}}));
        return;
    }
    let token = params["_meta"]["progressToken"].clone();
    let progress = |p: u64| {
        if !token.is_null() {
            st.send(&json!({"jsonrpc":"2.0","method":"notifications/progress",
                "params":{"progressToken":token,"progress":p,"total":2}}));
        }
    };
    let result = match tool.as_str() {
        "slow" => {
            progress(1);
            if !sleep_cancellable(&st, &key, st.cfg.delay_ms) {
                return;
            }
            progress(2);
            text_result(args.to_string(), json!({"tool":"slow"}))
        }
        "pid" => text_result(
            std::process::id().to_string(),
            json!({"tool":"pid","pid":std::process::id()}),
        ),
        "spawn_child" => {
            let exe = std::env::current_exe().unwrap();
            match Command::new(exe)
                .arg("--sleep-forever")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(ch) => text_result(
                    ch.id().to_string(),
                    json!({"tool":"spawn_child","pid":ch.id()}),
                ),
                Err(e) => json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
            }
        }
        "ask" if st.cfg.sampling => {
            let sid = format!("s{}", st.next_sid.fetch_add(1, Ordering::SeqCst));
            let (tx, rx) = channel();
            st.waiters
                .lock()
                .unwrap()
                .insert(json!(sid).to_string(), tx);
            st.send(&json!({"jsonrpc":"2.0","id":sid,"method":"sampling/createMessage",
                "params":{"messages":[{"role":"user","content":{"type":"text","text":"hi"}}],"maxTokens":16}}));
            match rx.recv_timeout(Duration::from_secs(15)) {
                Ok(r) => text_result(r.to_string(), json!({"tool":"ask","reply":r})),
                Err(_) => {
                    json!({"content":[{"type":"text","text":"sampling timeout"}],"isError":true})
                }
            }
        }
        name => text_result(args.to_string(), json!({"tool":name})),
    };
    st.send(&json!({"jsonrpc":"2.0","id":id,"result":result}));
    if st.cfg.list_changed_after.is_some_and(|c| n + 1 >= c)
        && !st.extra.swap(true, Ordering::SeqCst)
    {
        st.send(&json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}));
    }
}

fn tools_list(st: &State) -> Value {
    let tools: Vec<Value> = st
        .tool_names()
        .iter()
        .map(|n| {
            json!({"name":n,"description":format!("mock tool {n}"),
                "inputSchema":{"type":"object","additionalProperties":true}})
        })
        .collect();
    json!({"tools":tools})
}

fn handle(st: &Arc<State>, msg: Value) {
    let method = msg["method"].as_str();
    let id = msg.get("id").cloned();
    let Some(method) = method else {
        // A response to one of our requests (sampling).
        if let Some(id) = id {
            if let Some(tx) = st.waiters.lock().unwrap().remove(&id_key(&id)) {
                let _ = tx.send(msg.get("result").cloned().unwrap_or(msg["error"].clone()));
            }
        }
        return;
    };
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    st.log(method);
    let Some(id) = id else {
        if method == "notifications/cancelled" {
            if let Some(r) = params.get("requestId") {
                st.cancelled.lock().unwrap().insert(id_key(r));
            }
        }
        return;
    };
    let reply = |result: Value| st.send(&json!({"jsonrpc":"2.0","id":id,"result":result}));
    match method {
        "initialize" => {
            let v = params["protocolVersion"]
                .as_str()
                .unwrap_or(DEFAULT_VERSION);
            reply(json!({"protocolVersion":v,
                "capabilities":{"tools":{"listChanged":true},"prompts":{},"resources":{}},
                "serverInfo":{"name":st.cfg.name,"version":"0.0.1"}}));
        }
        "ping" => reply(json!({})),
        "tools/list" => reply(tools_list(st)),
        "prompts/list" => reply(json!({"prompts":[{"name":"greet","description":"mock prompt"}]})),
        "resources/list" => reply(
            json!({"resources":[{"uri":"mock://readme","name":"readme","mimeType":"text/plain"}]}),
        ),
        "tools/call" => {
            let st = st.clone();
            std::thread::spawn(move || handle_call(st, id, params));
        }
        _ => st.send(&json!({"jsonrpc":"2.0","id":id,
            "error":{"code":-32601,"message":format!("method not found: {method}")}})),
    }
}

fn main() {
    let cfg = match parse_args() {
        Ok(Some(c)) => c,
        Ok(None) => {
            // Idle mode; capped so a leaked grandchild cannot live forever.
            std::thread::sleep(Duration::from_secs(600));
            return;
        }
        Err(e) => {
            eprintln!("mock-mcp-stdio: {e}");
            std::process::exit(2);
        }
    };
    if cfg.startup_delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(cfg.startup_delay_ms));
    }
    let st = Arc::new(State {
        cfg,
        out: Mutex::new(std::io::stdout()),
        calls: AtomicU64::new(0),
        extra: AtomicBool::new(false),
        next_sid: AtomicU64::new(1),
        waiters: Mutex::new(HashMap::new()),
        cancelled: Mutex::new(HashSet::new()),
    });
    if st.cfg.garbage {
        let mut o = st.out.lock().unwrap();
        let _ = writeln!(o, "this is not json");
        let _ = o.flush();
    }
    st.log("started");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(v) => handle(&st, v),
            Err(_) => st.send(&json!({"jsonrpc":"2.0","id":null,
                "error":{"code":-32700,"message":"parse error"}})),
        }
    }
    std::process::exit(0);
}
