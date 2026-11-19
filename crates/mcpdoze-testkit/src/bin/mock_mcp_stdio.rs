// SPDX-License-Identifier: GPL-3.0-or-later
//! Scriptable mock MCP stdio server (newline-delimited JSON-RPC 2.0).
//!
//! Output-shape knobs: `--oversize-response` (tool replies are one line over
//! 16 MiB), `--string-ids` (response ids are JSON strings), `--content-length-framing`
//! (LSP-style headers), `--pretty-json` (multi-line JSON). Init knobs:
//! `--init-error`, `--init-version V`. Exit knobs: `--exit-code N` (tool `exit`
//! exits with N; with `--exit-after K` also on call index K, 0-based, like
//! `--crash-after`). `--stderr-leak` prints `MCPDOZE_TEST_*` env values and the
//! call arguments to stderr. `--script FILE` runs scripted steps (see `load_script`).
//! Part A knobs: `--big-items N` (tool `big`), `--structured` (tool `structured`
//! with an output schema), `--read-only TOOLS` (`readOnlyHint`),
//! `--verbose-descriptions`. Tools `counter` and `write` are enabled via `--tools`.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_VERSION: &str = "2025-11-25";
/// Length of the `--oversize-response` line: past the 16 MiB frame limit.
const OVERSIZE_BYTES: usize = 16 * 1024 * 1024 + 1024;
const VERBOSE_TEXT: &str =
    "This mock tool exists for tests and echoes its arguments back as text and structured content. ";

struct Config {
    name: String,
    tools: Vec<String>,
    delay_ms: u64,
    crash_after: Option<u64>,
    garbage: bool,
    list_changed_after: Option<u64>,
    sampling: bool,
    elicitation: bool,
    roots: bool,
    noise: bool,
    startup_delay_ms: u64,
    prompts: Vec<String>,
    resources: Vec<String>,
    templates: Vec<String>,
    log_messages: bool,
    /// Extra variable names the `info` tool reports (`--echo-env NAME`).
    echo_env: Vec<String>,
    /// Advertise `resources.subscribe` and serve subscribe/unsubscribe.
    subscribe: bool,
    /// Exit code for tool `exit` (and for `exit_after`); default 3 for `exit_after`.
    exit_code: Option<i32>,
    /// Exit on call index `n` and later (0-based, like `crash_after`).
    exit_after: Option<u64>,
    oversize: bool,
    string_ids: bool,
    framed: bool,
    pretty: bool,
    init_error: bool,
    init_version: Option<String>,
    stderr_leak: bool,
    script: Vec<Step>,
    big_items: Option<u64>,
    structured: bool,
    read_only: Option<Vec<String>>,
    verbose: bool,
}

/// One `--script` step: matches a request by method (and tool name for
/// `tools/call`); `nth` (1-based) restricts it to the nth match.
struct Step {
    method: String,
    tool: Option<String>,
    nth: Option<u64>,
    actions: Vec<Action>,
    hits: AtomicU64,
}

enum Action {
    /// Run the normal handler for the request.
    Respond,
    StdoutRaw(String),
    Stderr(String),
    DelayMs(u64),
    Exit(i32),
    /// Emit a reply for the request padded to at least this many bytes.
    Oversize(usize),
    /// Switch output to string ids from here on.
    StringId,
    /// Switch output to Content-Length framing from here on.
    ContentLengthFraming,
    /// Switch output to pretty-printed JSON from here on.
    PrettyJson,
}

struct State {
    cfg: Config,
    out: Mutex<std::io::Stdout>,
    calls: AtomicU64,
    /// Value returned by the `counter` tool (incremented per call).
    counter: AtomicU64,
    /// Output encoding; initialised from the flags, switched by script actions.
    string_ids: AtomicBool,
    framed: AtomicBool,
    pretty: AtomicBool,
    extra: AtomicBool,
    next_sid: AtomicU64,
    waiters: Mutex<HashMap<String, Sender<Value>>>,
    cancelled: Mutex<HashSet<String>>,
    /// Client capabilities from `initialize`.
    client_caps: Mutex<Value>,
    /// Subscribed URIs and request counters (`sub_stats` tool).
    subs: Mutex<HashSet<String>>,
    subscribes: AtomicU64,
    unsubscribes: AtomicU64,
}

impl State {
    fn send(&self, v: &Value) {
        let mut o = self.out.lock().unwrap();
        let is_reply = v.get("result").is_some() || v.get("error").is_some();
        let text = if is_reply && v["id"].is_number() && self.string_ids.load(Ordering::SeqCst) {
            let mut w = v.clone();
            w["id"] = Value::String(v["id"].to_string());
            self.encode(&w)
        } else {
            self.encode(v)
        };
        if self.framed.load(Ordering::SeqCst) {
            let _ = write!(o, "Content-Length: {}\r\n\r\n{text}\n", text.len());
        } else {
            let _ = writeln!(o, "{text}");
        }
        let _ = o.flush();
    }
    fn encode(&self, v: &Value) -> String {
        if self.pretty.load(Ordering::SeqCst) {
            serde_json::to_string_pretty(v).unwrap_or_default()
        } else {
            v.to_string()
        }
    }
    /// Writes `text` as one line, bypassing the encoding switches.
    fn send_raw(&self, text: &str) {
        let mut o = self.out.lock().unwrap();
        let _ = writeln!(o, "{text}");
        let _ = o.flush();
    }
    /// Sends a successful reply for `id` whose JSON is at least `bytes` long.
    fn send_padded(&self, id: &Value, bytes: usize) {
        let base =
            json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":""}]}})
                .to_string()
                .len();
        let pad = "x".repeat(bytes.saturating_sub(base));
        self.send(&json!({"jsonrpc":"2.0","id":id,
            "result":{"content":[{"type":"text","text":pad}]}}));
    }
    fn log(&self, msg: &str) {
        if self.cfg.noise {
            eprintln!("[mock {}] {msg}", self.cfg.name);
        }
    }
    fn tool_names(&self) -> Vec<String> {
        let mut t = self.cfg.tools.clone();
        let mut extra: Vec<&str> = Vec::new();
        if self.extra.load(Ordering::SeqCst) {
            extra.push("extra");
        }
        if self.cfg.big_items.is_some() {
            extra.push("big");
        }
        if self.cfg.structured {
            extra.push("structured");
        }
        for n in extra {
            if !t.iter().any(|x| x == n) {
                t.push(n.into());
            }
        }
        t
    }
}

fn parse_action(v: &Value) -> Result<Action, String> {
    if let Some(name) = v.as_str() {
        return match name {
            "respond" => Ok(Action::Respond),
            "string_id" => Ok(Action::StringId),
            "content_length_framing" => Ok(Action::ContentLengthFraming),
            "pretty_json" => Ok(Action::PrettyJson),
            other => Err(format!("unknown action {other}")),
        };
    }
    let Some((key, arg)) = v
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.iter().next())
    else {
        return Err(format!("bad action {v}"));
    };
    let bad = || format!("bad argument for {key}: {arg}");
    match key.as_str() {
        "stdout_raw" => Ok(Action::StdoutRaw(arg.as_str().ok_or_else(bad)?.to_owned())),
        "stderr" => Ok(Action::Stderr(arg.as_str().ok_or_else(bad)?.to_owned())),
        "delay_ms" => Ok(Action::DelayMs(arg.as_u64().ok_or_else(bad)?)),
        "exit" => Ok(Action::Exit(arg["code"].as_i64().ok_or_else(bad)? as i32)),
        "oversize" => Ok(Action::Oversize(
            arg["bytes"].as_u64().ok_or_else(bad)? as usize
        )),
        other => Err(format!("unknown action {other}")),
    }
}

/// Reads a script: a JSON array of `{"on": {"method", "tool"?, "nth"?},
/// "do": [action, ...]}`. Actions are `respond`, `string_id`,
/// `content_length_framing`, `pretty_json`, or one-key objects
/// `{"stdout_raw": TEXT}`, `{"stderr": TEXT}`, `{"delay_ms": N}`,
/// `{"exit": {"code": N}}`, `{"oversize": {"bytes": N}}`. Actions run in
/// order. The first matching step handles a request, and only `respond`
/// sends the normal reply, so a step without it leaves the request unanswered.
fn load_script(path: &str) -> Result<Vec<Step>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("script {path}: {e}"))?;
    let doc: Value = serde_json::from_str(&text).map_err(|e| format!("script {path}: {e}"))?;
    let steps = doc.as_array().ok_or("script must be a JSON array")?;
    steps
        .iter()
        .map(|s| -> Result<Step, String> {
            let method = s["on"]["method"]
                .as_str()
                .ok_or("step needs on.method")?
                .to_owned();
            let tool = s["on"]["tool"].as_str().map(str::to_owned);
            let nth = match s["on"].get("nth") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_u64()
                        .filter(|n| *n >= 1)
                        .ok_or("on.nth must be a number >= 1")?,
                ),
            };
            let actions = s["do"]
                .as_array()
                .ok_or("step needs a do array")?
                .iter()
                .map(parse_action)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Step {
                method,
                tool,
                nth,
                actions,
                hits: AtomicU64::new(0),
            })
        })
        .collect()
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
        elicitation: false,
        roots: false,
        noise: false,
        startup_delay_ms: 0,
        prompts: vec!["greet".into()],
        resources: vec!["mock://readme".into()],
        templates: Vec::new(),
        log_messages: false,
        echo_env: Vec::new(),
        subscribe: false,
        exit_code: None,
        exit_after: None,
        oversize: false,
        string_ids: false,
        framed: false,
        pretty: false,
        init_error: false,
        init_version: None,
        stderr_leak: false,
        script: Vec::new(),
        big_items: None,
        structured: false,
        read_only: None,
        verbose: false,
    };
    let list = |s: String| -> Vec<String> {
        s.split(',')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
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
            "--elicitation-request" => c.elicitation = true,
            "--roots-request" => c.roots = true,
            "--stderr-noise" => c.noise = true,
            "--prompts" => c.prompts = list(val()?),
            "--resources" => c.resources = list(val()?),
            "--resource-templates" => c.templates = list(val()?),
            "--log-messages" => c.log_messages = true,
            "--subscribe" => c.subscribe = true,
            "--echo-env" => c.echo_env.push(val()?),
            "--exit-code" => {
                c.exit_code = Some(
                    val()?
                        .parse::<i32>()
                        .map_err(|e| format!("bad number: {e}"))?,
                )
            }
            "--exit-after" => c.exit_after = Some(num(val()?)?),
            "--oversize-response" => c.oversize = true,
            "--string-ids" => c.string_ids = true,
            "--content-length-framing" => c.framed = true,
            "--pretty-json" => c.pretty = true,
            "--init-error" => c.init_error = true,
            "--init-version" => c.init_version = Some(val()?),
            "--stderr-leak" => c.stderr_leak = true,
            "--script" => c.script = load_script(&val()?)?,
            "--big-items" => c.big_items = Some(num(val()?)?),
            "--structured" => c.structured = true,
            "--read-only" => c.read_only = Some(list(val()?)),
            "--verbose-descriptions" => c.verbose = true,
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
    if st.cfg.stderr_leak {
        let env: Vec<String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("MCPDOZE_TEST_"))
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        eprintln!("[mock leak] tool={tool} args={args} env={}", env.join(" "));
    }
    let exit_now =
        (st.cfg.exit_code.is_some() && tool == "exit") || st.cfg.exit_after.is_some_and(|c| n >= c);
    if exit_now {
        st.log("exiting");
        std::process::exit(st.cfg.exit_code.unwrap_or(3));
    }
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
    if st.cfg.log_messages {
        st.send(&json!({"jsonrpc":"2.0","method":"notifications/message",
            "params":{"level":"info","logger":"mock","data":format!("calling {tool}")}}));
    }
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
        "ask" if st.cfg.sampling => client_request(
            &st,
            "ask",
            "sampling/createMessage",
            json!({"messages":[{"role":"user","content":{"type":"text","text":"hi"}}],"maxTokens":16}),
        ),
        "elicit" if st.cfg.elicitation => client_request(
            &st,
            "elicit",
            "elicitation/create",
            json!({"message":"pick one","requestedSchema":{"type":"object",
                "properties":{"choice":{"type":"string"}}}}),
        ),
        "touch" => {
            // Report a change of resource `uri` (default: the first one).
            let uri = args["uri"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| st.cfg.resources.first().cloned())
                .unwrap_or_default();
            st.send(
                &json!({"jsonrpc":"2.0","method":"notifications/resources/updated",
                "params":{"uri":uri}}),
            );
            text_result(uri.clone(), json!({"tool":"touch","uri":uri}))
        }
        "sub_stats" => {
            let mut active: Vec<String> = st.subs.lock().unwrap().iter().cloned().collect();
            active.sort();
            let v = json!({"tool":"sub_stats","pid":std::process::id(),
                "subscribes":st.subscribes.load(Ordering::SeqCst),
                "unsubscribes":st.unsubscribes.load(Ordering::SeqCst),"active":active});
            text_result(v.to_string(), v)
        }
        "roots" if st.cfg.roots => client_request(&st, "roots", "roots/list", json!({})),
        "info" => {
            let env: serde_json::Map<String, Value> = std::env::vars()
                .filter(|(k, _)| k.starts_with("MCPDOZE_TEST_") || st.cfg.echo_env.contains(k))
                .map(|(k, v)| (k, Value::String(v)))
                .collect();
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let info = json!({"tool":"info","pid":std::process::id(),
                "args":std::env::args().skip(1).collect::<Vec<_>>(),
                "cwd":cwd,"env":env,"client_capabilities":st.client_caps.lock().unwrap().clone()});
            text_result(info.to_string(), info)
        }
        "spawn_child_inherit" => {
            // The grandchild inherits stdout, so the pipe stays open after
            // this process dies (exercises exit detection by process exit).
            let exe = std::env::current_exe().unwrap();
            match Command::new(exe)
                .arg("--sleep-forever")
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(ch) => text_result(
                    ch.id().to_string(),
                    json!({"tool":"spawn_child_inherit","pid":ch.id()}),
                ),
                Err(e) => json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
            }
        }
        "big" => {
            let items: Vec<Value> = (0..st.cfg.big_items.unwrap_or(0))
                .map(|i| json!({"id":i,"name":format!("item-{i}"),"active":i % 2 == 0}))
                .collect();
            let count = items.len();
            text_result(
                serde_json::to_string_pretty(&items).unwrap_or_default(),
                json!({"tool":"big","count":count}),
            )
        }
        "structured" => {
            let v = json!({"tool":"structured","value":42});
            text_result(v.to_string(), v)
        }
        "counter" => {
            let value = st.counter.fetch_add(1, Ordering::SeqCst) + 1;
            text_result(value.to_string(), json!({"tool":"counter","value":value}))
        }
        name => text_result(args.to_string(), json!({"tool":name})),
    };
    if st.cfg.oversize {
        st.send_padded(&id, OVERSIZE_BYTES);
    } else {
        st.send(&json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
    if st.cfg.list_changed_after.is_some_and(|c| n + 1 >= c)
        && !st.extra.swap(true, Ordering::SeqCst)
    {
        st.send(&json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}));
    }
}

/// Sends a server-to-client request and waits (bounded) for the answer.
fn client_request(st: &State, tool: &str, method: &str, params: Value) -> Value {
    let sid = format!("s{}", st.next_sid.fetch_add(1, Ordering::SeqCst));
    let (tx, rx) = channel();
    st.waiters
        .lock()
        .unwrap()
        .insert(json!(sid).to_string(), tx);
    st.send(&json!({"jsonrpc":"2.0","id":sid,"method":method,"params":params}));
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(r) => text_result(r.to_string(), json!({"tool":tool,"reply":r})),
        Err(_) => {
            json!({"content":[{"type":"text","text":format!("{method} timeout")}],"isError":true})
        }
    }
}

fn tools_list(st: &State) -> Value {
    let tools: Vec<Value> = st
        .tool_names()
        .iter()
        .map(|n| {
            let desc = if st.cfg.verbose {
                format!("mock tool {n}. {}", VERBOSE_TEXT.repeat(8))
            } else {
                format!("mock tool {n}")
            };
            let mut t = json!({"name":n,"description":desc,
                "inputSchema":{"type":"object","additionalProperties":true}});
            if let Some(ro) = &st.cfg.read_only {
                t["annotations"] = json!({"readOnlyHint": ro.contains(n)});
            }
            if n == "structured" {
                t["outputSchema"] = json!({"type":"object",
                    "properties":{"tool":{"type":"string"},"value":{"type":"integer"}},
                    "required":["tool","value"]});
            }
            t
        })
        .collect();
    json!({"tools":tools})
}

/// Index of the first `--script` step that handles this request, if any.
fn script_actions<'a>(st: &'a State, method: &str, params: &Value) -> Option<&'a [Action]> {
    for step in &st.cfg.script {
        if step.method != method {
            continue;
        }
        if let Some(t) = &step.tool {
            if params["name"].as_str() != Some(t.as_str()) {
                continue;
            }
        }
        let hit = step.hits.fetch_add(1, Ordering::SeqCst) + 1;
        if step.nth.is_none_or(|n| n == hit) {
            return Some(&step.actions);
        }
    }
    None
}

fn run_action(st: &Arc<State>, a: &Action, method: &str, id: &Value, params: &Value) {
    match a {
        Action::Respond => dispatch(st, method, id.clone(), params.clone()),
        Action::StdoutRaw(s) => st.send_raw(s),
        Action::Stderr(s) => eprintln!("{s}"),
        Action::DelayMs(ms) => std::thread::sleep(Duration::from_millis(*ms)),
        Action::Exit(code) => std::process::exit(*code),
        Action::Oversize(bytes) => st.send_padded(id, *bytes),
        Action::StringId => st.string_ids.store(true, Ordering::SeqCst),
        Action::ContentLengthFraming => st.framed.store(true, Ordering::SeqCst),
        Action::PrettyJson => st.pretty.store(true, Ordering::SeqCst),
    }
}

fn handle(st: &Arc<State>, msg: Value) {
    let method = msg["method"].as_str();
    let id = msg.get("id").cloned();
    let Some(method) = method else {
        // A response to one of our requests (sampling).
        if let Some(id) = id {
            if let Some(tx) = st.waiters.lock().unwrap().remove(&id_key(&id)) {
                let _ = tx.send(
                    msg.get("result")
                        .cloned()
                        .unwrap_or(json!({"error": msg["error"].clone()})),
                );
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
    if let Some(actions) = script_actions(st, method, &params) {
        for a in actions {
            run_action(st, a, method, &id, &params);
        }
        return;
    }
    dispatch(st, method, id, params);
}

fn dispatch(st: &Arc<State>, method: &str, id: Value, params: Value) {
    let reply = |result: Value| st.send(&json!({"jsonrpc":"2.0","id":id,"result":result}));
    match method {
        "initialize" => {
            *st.client_caps.lock().unwrap() = params["capabilities"].clone();
            if st.cfg.init_error {
                st.send(&json!({"jsonrpc":"2.0","id":id,
                    "error":{"code":-32603,"message":"mock init error"}}));
                return;
            }
            let v = match &st.cfg.init_version {
                Some(v) => v.clone(),
                None => params["protocolVersion"]
                    .as_str()
                    .unwrap_or(DEFAULT_VERSION)
                    .to_owned(),
            };
            let resources = if st.cfg.subscribe {
                json!({"subscribe":true})
            } else {
                json!({})
            };
            reply(json!({"protocolVersion":v,
                "capabilities":{"tools":{"listChanged":true},"prompts":{},"resources":resources},
                "serverInfo":{"name":st.cfg.name,"version":"0.0.1"}}));
        }
        "ping" => reply(json!({})),
        "tools/list" => reply(tools_list(st)),
        "prompts/list" => {
            let prompts: Vec<Value> = st
                .cfg
                .prompts
                .iter()
                .map(|n| json!({"name":n,"description":"mock prompt"}))
                .collect();
            reply(json!({ "prompts": prompts }))
        }
        "prompts/get" => {
            let name = params["name"].as_str().unwrap_or("");
            if st.cfg.prompts.iter().any(|p| p == name) {
                reply(
                    json!({"description":"mock prompt","messages":[{"role":"user",
                    "content":{"type":"text","text":format!("prompt {name} args {}",
                        params.get("arguments").cloned().unwrap_or(json!({})))}}]}),
                );
            } else {
                st.send(&json!({"jsonrpc":"2.0","id":id,
                    "error":{"code":-32602,"message":format!("unknown prompt {name}")}}));
            }
        }
        "resources/list" => {
            let resources: Vec<Value> = st
                .cfg
                .resources
                .iter()
                .map(|u| {
                    let name = u.rsplit('/').next().unwrap_or(u);
                    json!({"uri":u,"name":name,"mimeType":"text/plain"})
                })
                .collect();
            reply(json!({ "resources": resources }))
        }
        "resources/templates/list" => {
            let templates: Vec<Value> = st
                .cfg
                .templates
                .iter()
                .map(|t| json!({"uriTemplate":t,"name":"template","mimeType":"text/plain"}))
                .collect();
            reply(json!({ "resourceTemplates": templates }))
        }
        "resources/read" => {
            let uri = params["uri"].as_str().unwrap_or("").to_string();
            let known = st.cfg.resources.contains(&uri)
                || st.cfg.templates.iter().any(|t| {
                    t.split('{')
                        .next()
                        .is_some_and(|lit| !lit.is_empty() && uri.starts_with(lit))
                });
            if known {
                reply(json!({"contents":[{"uri":uri,"mimeType":"text/plain",
                    "text":format!("contents of {uri}")}]}));
            } else {
                st.send(&json!({"jsonrpc":"2.0","id":id,
                    "error":{"code":-32002,"message":"resource not found"}}));
            }
        }
        "resources/subscribe" | "resources/unsubscribe" if st.cfg.subscribe => {
            let uri = params["uri"].as_str().unwrap_or("").to_string();
            if method == "resources/subscribe" {
                st.subscribes.fetch_add(1, Ordering::SeqCst);
                st.subs.lock().unwrap().insert(uri);
            } else {
                st.unsubscribes.fetch_add(1, Ordering::SeqCst);
                st.subs.lock().unwrap().remove(&uri);
            }
            reply(json!({}))
        }
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
    let (string_ids, framed, pretty) = (cfg.string_ids, cfg.framed, cfg.pretty);
    let st = Arc::new(State {
        cfg,
        out: Mutex::new(std::io::stdout()),
        calls: AtomicU64::new(0),
        counter: AtomicU64::new(0),
        string_ids: AtomicBool::new(string_ids),
        framed: AtomicBool::new(framed),
        pretty: AtomicBool::new(pretty),
        extra: AtomicBool::new(false),
        next_sid: AtomicU64::new(1),
        waiters: Mutex::new(HashMap::new()),
        cancelled: Mutex::new(HashSet::new()),
        client_caps: Mutex::new(Value::Null),
        subs: Mutex::new(HashSet::new()),
        subscribes: AtomicU64::new(0),
        unsubscribes: AtomicU64::new(0),
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
