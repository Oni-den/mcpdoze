// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdoze__batch` (technique 8): several tool calls in one agent request.
//!
//! Every inner call runs through the normal `Router::call_tool` path, so
//! statistics, optimisation transforms, cancellation, server-to-client
//! routing, per-server policy and diagnostics all apply per call. The batch
//! only adds admission (per-server `batch` mode, no nesting, a size limit),
//! bounded concurrency and the result layout. Dropping the batch future
//! (agent cancel, disconnect) aborts the inner tasks, which cancels their
//! backend requests.

use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::optimize::meta::PREFIX;
use super::{naming, scope_name, Era, Router, Session};
use crate::config::BatchMode;
use crate::stats::{self, CallEvent, CallKind, Event};

/// Most calls one batch may carry.
pub const MAX_CALLS: usize = 16;
/// Inner calls in flight at once.
pub const CONCURRENCY: usize = 4;
/// Stats tag of the batch call itself.
pub const TAG_BATCH: &str = "batch";

impl Router {
    /// Whether any server allows batching (lists `mcpdoze__batch`).
    pub fn batch_enabled(&self) -> bool {
        self.policies
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|p| p.batch() != BatchMode::Off)
    }

    /// Why the inner call `name` is refused, or `None` when it may run.
    fn batch_refusal(&self, session: &Session, name: &str) -> Option<String> {
        if name.starts_with(&format!("{PREFIX}{}", naming::SEP)) {
            return Some("nested mcpdoze__ calls are not allowed inside a batch".to_owned());
        }
        let Some((server, tool)) = self.named(Some(name)) else {
            return Some(format!("unknown tool: {name}"));
        };
        let policy = self.policy(&server.name);
        if policy.as_ref().is_some_and(|p| !p.allows(&tool)) {
            return Some(format!("unknown tool: {name}"));
        }
        match policy.map_or(BatchMode::Off, |p| p.batch()) {
            BatchMode::Off => Some(format!(
                "server `{}` does not allow batched calls (batch = off)",
                server.name
            )),
            BatchMode::All => None,
            BatchMode::ReadOnly => {
                let ckey = self
                    .session_key_now(session)
                    .and_then(|k| server.catalog_key_of(&k));
                let (read_only, _) = self.tool_hints(&server, ckey.as_deref(), &tool);
                (!read_only).then(|| {
                    format!(
                        "{name} is not annotated readOnlyHint=true; server `{}` allows only read-only tools in a batch (batch = read-only)",
                        server.name
                    )
                })
            }
        }
    }
}

fn tool_error(msg: impl Into<String>) -> Value {
    json!({"content":[{"type":"text","text":msg.into()}],"isError":true})
}

/// `(isError, text)` of an inner response.
fn render(resp: &Value) -> (bool, String) {
    if let Some(e) = resp.get("error") {
        let code = e["code"].as_i64().unwrap_or(0);
        let msg = e["message"].as_str().unwrap_or("");
        return (true, format!("error {code}: {msg}"));
    }
    let result = &resp["result"];
    let blocks = result["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let text: Vec<String> = blocks
        .iter()
        .map(|b| match b["text"].as_str() {
            Some(t) if b["type"] == "text" => t.to_owned(),
            _ => format!(
                "[{} content omitted]",
                b["type"].as_str().unwrap_or("non-text")
            ),
        })
        .collect();
    (result["isError"] == Value::Bool(true), text.join("\n"))
}

/// Runs `mcpdoze__batch` for `session`.
pub(crate) async fn run(router: Arc<Router>, session: &Arc<Session>, arguments: Value) -> Value {
    let started = Instant::now();
    let bytes_in = stats::json_len(&arguments);
    let result = execute(&router, session, &arguments).await;
    record(&router, session, started, bytes_in, &result);
    result
}

async fn execute(router: &Arc<Router>, session: &Arc<Session>, arguments: &Value) -> Value {
    let Some(calls) = arguments["calls"].as_array() else {
        return tool_error("batch needs an array argument `calls`");
    };
    if calls.is_empty() {
        return tool_error("`calls` is empty");
    }
    if calls.len() > MAX_CALLS {
        return tool_error(format!(
            "too many calls: {} (at most {MAX_CALLS} per batch)",
            calls.len()
        ));
    }
    let mut parsed = Vec::with_capacity(calls.len());
    for (i, c) in calls.iter().enumerate() {
        let Some(name) = c["name"].as_str() else {
            return tool_error(format!("calls[{i}] needs a string `name`"));
        };
        let args = match &c["arguments"] {
            Value::Null => json!({}),
            a if a.is_object() => a.clone(),
            _ => return tool_error(format!("calls[{i}].arguments must be an object")),
        };
        parsed.push((name.to_owned(), args));
    }

    // Admission first (cheap, no backend work), then the admitted calls.
    let mut done: Vec<Option<(bool, String)>> = vec![None; parsed.len()];
    let sem = Arc::new(Semaphore::new(CONCURRENCY));
    let mut tasks: JoinSet<(usize, Value)> = JoinSet::new();
    for (i, (name, args)) in parsed.iter().enumerate() {
        if let Some(why) = router.batch_refusal(session, name) {
            done[i] = Some((true, why));
            continue;
        }
        let (router, session, sem) = (router.clone(), session.clone(), sem.clone());
        let params = json!({"name": name, "arguments": args});
        let id = json!(format!("mcpdoze-batch-{}", router.next_request_id()));
        tasks.spawn(async move {
            let _permit = sem.acquire_owned().await;
            (i, router.call_tool(&session, id, params).await)
        });
    }
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((i, resp)) => done[i] = Some(render(&resp)),
            Err(e) => {
                // A panicked inner call: its slot is filled below.
                tracing::warn!(error = %e, "batch inner call failed");
            }
        }
    }

    let content: Vec<Value> = parsed
        .iter()
        .zip(done)
        .enumerate()
        .map(|(i, ((name, _), d))| {
            let (is_err, body) = d.unwrap_or((true, "the call did not complete".to_owned()));
            json!({"type":"text","text": format!("[{i}] {name} isError={is_err}\n{body}")})
        })
        .collect();
    json!({"content": content, "isError": false})
}

/// Records the batch call once; inner calls were recorded as they ran.
fn record(router: &Router, session: &Session, started: Instant, bytes_in: u64, result: &Value) {
    let Some(st) = router.stats() else { return };
    let session_key = if session.is_ephemeral() {
        stats::STATELESS_SESSION
    } else {
        stats::session_key(session.id())
    };
    let ok = result["isError"] != Value::Bool(true);
    st.record(Event::Call(CallEvent {
        ts: stats::now_ms(),
        server: PREFIX.to_owned(),
        kind: CallKind::Tool,
        name: "batch".to_owned(),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ok,
        error_code: None,
        fail: None,
        cold_start: false,
        spawn_wait_ms: 0,
        bytes_in,
        bytes_out: stats::json_len(result),
        era: match session.era() {
            Era::Legacy => "legacy",
            Era::Modern => "modern",
        }
        .to_owned(),
        scope: scope_name(crate::config::Scope::Shared).to_owned(),
        instance: None,
        req_id: None,
        acquire_ms: None,
        backend_ms: None,
        bytes_sent: None,
        opt: vec![TAG_BATCH.to_owned()],
        cached: false,
        dup: false,
        session: Some(session_key),
    }));
}
