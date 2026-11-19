// SPDX-License-Identifier: GPL-3.0-or-later

//! Agent requests: dispatch by method and forwarding to backends (id,
//! progress-token and cancellation mapping).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::events::ProgressRoute;
use super::optimize::{self, meta, TAG_ARGS};
use super::session::RootsState;
use super::usage::{CallExtras, CallRecord, CallStart, CallTrace};
use super::{naming, uri, Router, Server, Session};
use crate::backend::jsonrpc::{self, Kind};
use crate::backend::stdio::id_type_of;
use crate::backend::stdio::lock;
use crate::backend::Instance;
use crate::backend::{CallError, CallMeta, Conn, StateKind};
use crate::config::CacheHit;
use crate::diag::{DiagTap, Era as DEra, ExitView, RequestView};
/// Page size of `mcpdoze__result_get` for a cached result.
const CACHE_PAGE: usize = 8 * 1024;

/// What the result cache has to do around one `tools/call`.
enum CacheCall {
    /// A tool not known to be read-only: invalidates the server's entries.
    Writer,
    /// An eligible read: store the result under `key` if nothing
    /// invalidated the server since `epoch`.
    Reader {
        key: optimize::cache::Key,
        epoch: u64,
        ttl: std::time::Duration,
    },
}

/// Removes a session's in-flight entry when the call ends.
pub(super) struct InflightEntry<'a> {
    session: &'a Session,
    key: String,
}

impl Drop for InflightEntry<'_> {
    fn drop(&mut self) {
        lock(&self.session.inflight).remove(&self.key);
    }
}

/// Sends `notifications/cancelled` to the backend if the call did not
/// complete (agent cancel, session end, or the call future being dropped
/// because the agent disconnected).
pub(super) struct CancelOnDrop {
    conn: Arc<Conn>,
    id: u64,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.conn.cancel(self.id, "cancelled by client");
        }
    }
}

/// Request facts kept while a call runs, for diagnostics records. Built
/// only when the instance's tap is enabled. Dropped while still armed (the
/// agent went away and the call future was dropped) it records
/// `agent_disconnected`.
struct CallDiag {
    tap: Arc<DiagTap>,
    inst: Arc<Instance>,
    req: u64,
    era: DEra,
    method: String,
    tool: Option<String>,
    id_type: crate::diag::IdType,
    progress: bool,
    bytes: u64,
    started: std::time::Instant,
    acquire_ms: u64,
    cold_start: bool,
    arguments: Option<Value>,
    disconnect: bool,
}

impl CallDiag {
    fn view<'a>(&'a self, suspect: bool, schema: Option<&'a Value>) -> RequestView<'a> {
        let mut v = RequestView::new(self.req, self.era, &self.method);
        v.tool = self.tool.as_deref();
        v.id_type = Some(self.id_type);
        v.progress = self.progress;
        v.bytes = self.bytes;
        v.elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        v.acquire_ms = self.acquire_ms;
        v.cold_start = self.cold_start;
        v.suspect = suspect;
        v.arguments = self.arguments.as_ref();
        v.input_schema = schema;
        v
    }

    fn slow(&self) {
        self.inst
            .with_view(|b| self.tap.on_slow_call(&self.view(false, None), b));
    }

    fn stalled(&self) {
        self.inst.with_view(|b| {
            self.inst
                .with_stderr(|s| self.tap.on_stalled(&self.view(false, None), b, Some(s)));
        });
    }

    fn cancelled(&self) {
        self.inst
            .with_view(|b| self.tap.on_agent_cancelled(&self.view(false, None), b));
    }

    fn crashed(&self, incident: &str, exit: ExitView, suspect: bool, schema: Option<&Value>) {
        self.inst.with_view(|b| {
            self.inst.with_stderr(|s| {
                self.tap.on_crash_in_flight(
                    incident,
                    exit,
                    &self.view(suspect, schema),
                    b,
                    Some(s),
                );
            });
        });
    }

    /// Records a JSON-RPC error or (with `tool_errors`) an `isError` result.
    fn answered(&self, resp: &Value, schema: Option<&Value>) {
        if let Some(e) = resp.get("error").filter(|e| e.is_object()) {
            let code = e["code"].as_i64().unwrap_or(0);
            let msg = e["message"].as_str().unwrap_or("");
            self.inst.with_view(|b| {
                self.inst.with_stderr(|s| {
                    self.tap.on_rpc_error(
                        &self.view(false, schema),
                        code,
                        msg,
                        e.get("data"),
                        b,
                        Some(s),
                    );
                });
            });
        } else if resp["result"]["isError"] == Value::Bool(true) && self.tap.tool_errors() {
            let msg = resp["result"]["content"]
                .as_array()
                .and_then(|c| c.iter().find_map(|b| b["text"].as_str()));
            self.inst.with_view(|b| {
                self.inst.with_stderr(|s| {
                    self.tap
                        .on_tool_error(&self.view(false, schema), msg, b, Some(s));
                });
            });
        }
    }
}

impl Drop for CallDiag {
    fn drop(&mut self) {
        if self.disconnect && self.tap.enabled() {
            self.inst
                .with_view(|b| self.tap.on_agent_disconnected(&self.view(false, None), b));
        }
    }
}

impl Router {
    /// The catalog `inputSchema` of `tool` on `server` as this session
    /// sees it (diagnostics only: cloned on a failure path).
    fn input_schema(&self, session: &Session, server: &Server, tool: &str) -> Option<Value> {
        let ckey = self
            .session_key_now(session)
            .and_then(|k| server.catalog_key_of(&k));
        let (entry, _) = self.entry_for(server, ckey.as_deref())?;
        entry
            .tools
            .iter()
            .find(|t| t["name"] == tool)
            .and_then(|t| t.get("inputSchema").cloned())
    }

    /// Handles one agent message. Returns the response for requests (and
    /// for invalid messages), `None` for notifications and responses.
    pub async fn handle(&self, session: &Arc<Session>, msg: Value) -> Option<Value> {
        session.touch();
        match jsonrpc::kind(&msg) {
            Kind::Request => Some(self.request(session, msg).await),
            Kind::Notification => {
                self.notification(session, &msg);
                None
            }
            Kind::Response => {
                if !session.answer(msg) {
                    tracing::debug!("agent response without a pending request dropped");
                }
                None
            }
            Kind::Invalid => Some(jsonrpc::error(
                msg.get("id").cloned().unwrap_or(Value::Null),
                jsonrpc::INVALID_REQUEST,
                "invalid JSON-RPC message",
            )),
        }
    }

    pub(super) fn notification(&self, session: &Session, msg: &Value) {
        match msg["method"].as_str().unwrap_or("") {
            "notifications/cancelled" => {
                if let Some(rid) = msg["params"].get("requestId") {
                    session.cancel(rid);
                }
            }
            "notifications/initialized" => {}
            "notifications/roots/list_changed" => {
                *lock(&session.roots) = RootsState::NotAsked;
            }
            method => tracing::debug!(method, "agent notification ignored"),
        }
    }

    pub(super) async fn request(&self, session: &Arc<Session>, msg: Value) -> Value {
        let id = msg["id"].clone();
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        match msg["method"].as_str().unwrap_or("") {
            "initialize" => {
                session.set_roots_capable(params["capabilities"].get("roots").is_some());
                let requested = params["protocolVersion"].as_str();
                jsonrpc::result(id, self.initialize_result(requested).1)
            }
            "ping" => jsonrpc::result(id, json!({})),
            "tools/list" => jsonrpc::result(id, json!({"tools": self.list_tools(session).await})),
            "tools/call" => self.call_tool(session, id, params).await,
            "prompts/list" => {
                jsonrpc::result(id, json!({"prompts": self.list_prompts(session).await}))
            }
            "prompts/get" => self.get_prompt(session, id, params).await,
            "resources/list" => {
                jsonrpc::result(id, json!({"resources": self.list_resources(session).await}))
            }
            "resources/templates/list" => jsonrpc::result(
                id,
                json!({"resourceTemplates": self.list_resource_templates(session).await}),
            ),
            "resources/read" => self.read_resource(session, id, params).await,
            "resources/subscribe" => self.subscribe_request(session, id, params).await,
            "resources/unsubscribe" => self.unsubscribe_request(session, id, params).await,
            method => jsonrpc::error(
                id,
                jsonrpc::METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            ),
        }
    }

    pub(super) fn named(&self, name: Option<&str>) -> Option<(Arc<Server>, String)> {
        naming::split(name?).and_then(|(p, t)| self.by_prefix(p).map(|s| (s, t.to_owned())))
    }

    pub(super) async fn call_tool(
        &self,
        session: &Arc<Session>,
        id: Value,
        mut params: Value,
    ) -> Value {
        let Some(name) = params["name"].as_str().map(str::to_owned) else {
            return jsonrpc::error(id, jsonrpc::INVALID_PARAMS, "tools/call needs a name");
        };
        // Reserved prefix: broker-served meta-tools, before server routing.
        if let Some(bare) = meta::bare(&name) {
            let tool = self
                .meta
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .find(bare);
            return match tool {
                Some(t) => {
                    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                    // Registered as in flight so `notifications/cancelled`
                    // drops the tool future (a batch then cancels its
                    // inner calls).
                    let key = jsonrpc::id_key(&id);
                    let (cancel_tx, mut cancel_rx) = oneshot::channel();
                    lock(&session.inflight).insert(key.clone(), cancel_tx);
                    let _entry = InflightEntry { session, key };
                    let call = t.call(session, args);
                    tokio::pin!(call);
                    tokio::select! {
                        r = &mut call => jsonrpc::result(id, r),
                        Ok(()) = &mut cancel_rx => {
                            jsonrpc::error(id, jsonrpc::REQUEST_CANCELLED, "request cancelled")
                        }
                    }
                }
                None => {
                    jsonrpc::error(id, jsonrpc::INVALID_PARAMS, format!("unknown tool: {name}"))
                }
            };
        }
        let Some((server, tool)) = self.named(Some(&name)) else {
            return jsonrpc::error(id, jsonrpc::INVALID_PARAMS, format!("unknown tool: {name}"));
        };
        let policy = self.policy(&server.name);
        if policy.as_ref().is_some_and(|p| !p.allows(&tool)) {
            return jsonrpc::error(id, jsonrpc::INVALID_PARAMS, format!("unknown tool: {name}"));
        }
        params["name"] = Value::String(tool.clone());
        let ckey = self
            .session_key_now(session)
            .and_then(|k| server.catalog_key_of(&k));
        let mut tags = Vec::new();
        if let Some(p) = &policy {
            let schema = self.input_schema(session, &server, &tool);
            if p.apply_default_args(&server.name, &tool, &mut params, schema.as_ref()) {
                tags.push(TAG_ARGS.to_owned());
            }
        }
        // Session result cache (technique 5, strict opt-in; see
        // `optimize::cache`). A tool that is not known to be read-only
        // invalidates the server's entries around the call.
        let mut cache_call = None;
        if let Some(cp) = policy.as_ref().and_then(|p| p.cache()) {
            let (read_only, has_output_schema) = self.tool_hints(&server, ckey.as_deref(), &tool);
            if !read_only {
                self.cache.invalidate_server(&server.name);
                cache_call = Some(CacheCall::Writer);
            } else if cp.matches(&tool) && !session.is_ephemeral() {
                let key = optimize::cache::Key::new(
                    &server.name,
                    ckey.as_deref().unwrap_or(""),
                    session.id(),
                    &tool,
                    params.get("arguments"),
                );
                if let Some(hit) = self.cache.lookup(&key, std::time::Instant::now()) {
                    let reference = cp.on_hit() == CacheHit::Reference && !has_output_schema;
                    return self.serve_cache_hit(
                        session, &server, id, &tool, &params, key, hit, reference,
                    );
                }
                cache_call = Some(CacheCall::Reader {
                    key,
                    epoch: self.cache.epoch(&server.name),
                    ttl: cp.ttl(),
                });
            }
        }
        let (mut resp, rec) = self
            .forward_routed_open(session, &server, id, "tools/call", params)
            .await;
        // Result transforms (never on errors; see `optimize::result`).
        let mut raw_bytes = None;
        if let Some(p) = policy.as_ref().filter(|p| p.transforms_results()) {
            let raw = crate::stats::json_len(&resp);
            // Stateless (ephemeral) sessions have no stable identity to bind
            // refs to: no store, so no truncation for them.
            let ctx = (!session.is_ephemeral()).then(|| optimize::result::StoreCtx {
                store: self.result_store(),
                session,
            });
            let applied = p.transform_result(&tool, &mut resp, ctx.as_ref());
            if !applied.is_empty() {
                raw_bytes = Some(raw);
                tags.extend(applied);
            }
        }
        match cache_call {
            Some(CacheCall::Writer) => self.cache.invalidate_server(&server.name),
            Some(CacheCall::Reader { key, epoch, ttl }) => {
                if optimize::cache::cacheable(&resp) {
                    let stored = self.cache.store(
                        key,
                        resp["result"].clone(),
                        epoch,
                        ttl,
                        std::time::Instant::now(),
                    );
                    if let optimize::cache::Stored::Yes {
                        first_for_session: true,
                    } = stored
                    {
                        let cache = self.cache.clone();
                        let sid = session.id().to_owned();
                        session.on_end(move || cache.purge_session(&sid));
                    }
                } else {
                    self.cache.remove(&key);
                }
            }
            None => {}
        }
        if let Some(rec) = rec {
            // `bytes_out` stays the raw backend size, `bytes_sent` is what
            // the agent gets.
            let extras = CallExtras {
                raw_bytes,
                opt: tags,
                ..CallExtras::default()
            };
            rec.finish(self, session, &server, &resp, extras);
        }
        // Stale catalog: the backend rejected a listed tool, or served one
        // the catalog does not know.
        if let Some((entry, _)) = self.entry_for(&server, ckey.as_deref()) {
            let rejected = matches!(
                resp["error"]["code"].as_i64(),
                Some(jsonrpc::INVALID_PARAMS | jsonrpc::METHOD_NOT_FOUND)
            );
            if rejected || !entry.has_tool(&tool) {
                if let Some(target) = server.running_for(ckey.as_deref()) {
                    self.spawn_dedup_refresh(&server, Some(target), false, "stale catalog");
                }
            }
        }
        resp
    }

    /// `(readOnlyHint == true, has an outputSchema)` of `tool` in the raw
    /// catalog; `(false, false)` when the catalog does not know it.
    pub(super) fn tool_hints(
        &self,
        server: &Server,
        ckey: Option<&str>,
        tool: &str,
    ) -> (bool, bool) {
        let Some((entry, _)) = self.entry_for(server, ckey) else {
            return (false, false);
        };
        entry
            .tools
            .iter()
            .find(|t| t["name"] == tool)
            .map_or((false, false), |t| {
                (
                    t["annotations"]["readOnlyHint"] == Value::Bool(true),
                    t.get("outputSchema").is_some_and(|o| !o.is_null()),
                )
            })
    }

    /// Answers a `tools/call` from the result cache and records it.
    #[allow(clippy::too_many_arguments)]
    fn serve_cache_hit(
        &self,
        session: &Arc<Session>,
        server: &Arc<Server>,
        id: Value,
        tool: &str,
        params: &Value,
        key: optimize::cache::Key,
        hit: optimize::cache::Hit,
        reference: bool,
    ) -> Value {
        let full = crate::stats::json_len(&hit.result);
        let mut raw_bytes = None;
        let mut result = (*hit.result).clone();
        if reference {
            let store = self.result_store();
            let live = hit
                .reference
                .filter(|r| store.get(session.id(), r).is_some());
            let id = live.or_else(|| {
                let put = store.put(
                    session.id(),
                    optimize::store::Stored {
                        tool: tool.to_owned(),
                        result: (*hit.result).clone(),
                        page: CACHE_PAGE,
                    },
                )?;
                if put.first_for_session {
                    let st = store.clone();
                    let sid = session.id().to_owned();
                    session.on_end(move || st.purge_session(&sid));
                }
                self.cache.set_reference(&key, put.id.clone());
                Some(put.id)
            });
            if let Some(r) = id {
                let stub = optimize::cache::stub(&r, full);
                if crate::stats::json_len(&stub) < full {
                    result = stub;
                    raw_bytes = Some(full);
                }
            }
        }
        let resp = jsonrpc::result(id, result);
        if let Some(st) = self.stats() {
            if let Some(start) = CallStart::new("tools/call", params) {
                let rec = CallRecord {
                    st,
                    start,
                    trace: CallTrace {
                        req: self.next_request_id(),
                        ..CallTrace::default()
                    },
                };
                let extras = CallExtras {
                    raw_bytes,
                    opt: vec![optimize::cache::TAG_CACHE.to_owned()],
                    cached: true,
                };
                rec.finish(self, session, server, &resp, extras);
            }
        }
        resp
    }

    pub(super) async fn get_prompt(
        &self,
        session: &Arc<Session>,
        id: Value,
        mut params: Value,
    ) -> Value {
        let Some((server, prompt)) = self.named(params["name"].as_str()) else {
            return jsonrpc::error(id, jsonrpc::INVALID_PARAMS, "unknown prompt");
        };
        params["name"] = Value::String(prompt);
        self.forward_routed(session, &server, id, "prompts/get", params)
            .await
    }

    pub(super) async fn read_resource(
        &self,
        session: &Arc<Session>,
        id: Value,
        mut params: Value,
    ) -> Value {
        let Some((server, original)) = params["uri"]
            .as_str()
            .and_then(uri::unmap)
            .and_then(|(p, u)| self.by_prefix(p).map(|s| (s, u)))
        else {
            return jsonrpc::error(id, jsonrpc::INVALID_PARAMS, "unknown resource URI");
        };
        params["uri"] = Value::String(original);
        let mut resp = self
            .forward_routed(session, &server, id, "resources/read", params)
            .await;
        if let Some(contents) = resp
            .pointer_mut("/result/contents")
            .and_then(Value::as_array_mut)
        {
            for c in contents {
                if let Some(u) = c["uri"].as_str() {
                    c["uri"] = Value::String(uri::map_uri(&server.prefix, u));
                }
            }
        }
        resp
    }

    /// Rewrites the progress token (if any) and forwards.
    pub(super) async fn forward_routed(
        &self,
        session: &Arc<Session>,
        server: &Arc<Server>,
        id: Value,
        method: &str,
        params: Value,
    ) -> Value {
        let (resp, rec) = self
            .forward_routed_open(session, server, id, method, params)
            .await;
        if let Some(rec) = rec {
            rec.finish(self, session, server, &resp, CallExtras::default());
        }
        resp
    }

    /// Like [`forward_routed`](Self::forward_routed), but hands back the
    /// unfinished call record so the caller can add what its transforms did.
    pub(super) async fn forward_routed_open(
        &self,
        session: &Arc<Session>,
        server: &Arc<Server>,
        id: Value,
        method: &str,
        mut params: Value,
    ) -> (Value, Option<CallRecord>) {
        let mut route = None;
        let orig_token = params
            .pointer("/_meta/progressToken")
            .filter(|t| !t.is_null())
            .cloned();
        if let Some(orig) = &orig_token {
            let n = self.progress.next.fetch_add(1, Ordering::Relaxed);
            let token = format!("mcpdoze-{n}");
            lock(&self.progress.routes)
                .insert(token.clone(), (Arc::downgrade(session), orig.clone()));
            params["_meta"]["progressToken"] = Value::String(token.clone());
            route = Some(ProgressRoute {
                table: self.progress.clone(),
                token,
            });
        }
        let out = self
            .forward_open(session, server, id, method, params, orig_token)
            .await;
        drop(route);
        out
    }

    /// Forwards one request to `server`, starting it if needed, and maps the
    /// response back to the agent's id. Returns the raw response and, when a
    /// statistics log is set, the call record to finish once the final
    /// response is known (`bytes_out` is the raw size).
    pub(super) async fn forward_open(
        &self,
        session: &Arc<Session>,
        server: &Arc<Server>,
        id: Value,
        method: &str,
        params: Value,
        progress_token: Option<Value>,
    ) -> (Value, Option<CallRecord>) {
        let mut trace = CallTrace {
            req: self.next_request_id(),
            ..CallTrace::default()
        };
        let logged = self
            .stats()
            .and_then(|st| CallStart::new(method, &params).map(|c| (st, c)));
        let resp = self
            .forward_traced(
                session,
                server,
                id,
                method,
                params,
                progress_token,
                &mut trace,
            )
            .await;
        let rec = logged.map(|(st, start)| CallRecord { st, start, trace });
        (resp, rec)
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward_traced(
        &self,
        session: &Arc<Session>,
        server: &Arc<Server>,
        id: Value,
        method: &str,
        params: Value,
        progress_token: Option<Value>,
        trace: &mut CallTrace,
    ) -> Value {
        let inst = match self.instance_for(session, server).await {
            Ok(i) => i,
            Err((code, msg)) => return jsonrpc::error(id, code, msg),
        };
        trace.cold_start = inst.state() != StateKind::Running;
        trace.scope_key = Some(inst.spec().client.scope_key.clone());
        let acquiring = std::time::Instant::now();
        let acquired = inst.acquire().await;
        trace.acquire = acquiring.elapsed();
        let guard = match acquired {
            Ok(g) => g,
            Err(e) => return jsonrpc::error(id, jsonrpc::BACKEND_UNAVAILABLE, e.to_string()),
        };
        let inst_key = inst.spec().client.scope_key.clone();
        drop(inst);
        self.note_conn(server, &inst_key, guard.conn());
        let key = jsonrpc::id_key(&id);
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        lock(&session.inflight).insert(key.clone(), cancel_tx);
        let _entry = InflightEntry { session, key };
        let meta = CallMeta {
            session: Some(session.id.clone()),
            orig_id: id.clone(),
            progress_token,
        };
        let tap = guard.instance().tap();
        let mut cd = tap.enabled().then(|| CallDiag {
            tap: tap.clone(),
            inst: guard.instance().clone(),
            req: trace.req,
            era: match session.era() {
                super::Era::Legacy => DEra::Legacy,
                super::Era::Modern => DEra::Modern,
            },
            method: method.to_owned(),
            tool: (method == "tools/call")
                .then(|| params["name"].as_str().map(str::to_owned))
                .flatten(),
            id_type: id_type_of(&id),
            progress: meta.progress_token.is_some(),
            bytes: crate::stats::json_len(&params),
            started: std::time::Instant::now(),
            acquire_ms: u64::try_from(trace.acquire.as_millis()).unwrap_or(u64::MAX),
            cold_start: trace.cold_start,
            arguments: (method == "tools/call")
                .then(|| params.get("arguments").cloned())
                .flatten(),
            disconnect: true,
        });
        let sending = std::time::Instant::now();
        let (back_id, mut rx) = match guard.conn().begin(method, params, meta).await {
            Ok(x) => x,
            Err(e) => {
                if let Some(cd) = &mut cd {
                    cd.disconnect = false;
                }
                return jsonrpc::error(id, jsonrpc::BACKEND_UNAVAILABLE, e.to_string());
            }
        };
        // Declared after `guard`, so it runs first on drop: the backend hears
        // about the cancellation before the call stops counting as in flight.
        let mut cancel = CancelOnDrop {
            conn: guard.conn().clone(),
            id: back_id,
            armed: true,
        };
        // The in-flight sweep for `slow_call` (trace level): one timer per
        // traced call, firing once while the call is still running.
        let slow = tokio::time::sleep(tap.slow_call_threshold());
        tokio::pin!(slow);
        let mut slow_armed = cd.is_some() && tap.level() == crate::diag::Level::Trace;
        let mut cancel_live = true;
        let call_timeout = guard.instance().spec().call_timeout;
        let stall = tokio::time::sleep(call_timeout.unwrap_or(std::time::Duration::from_secs(1)));
        tokio::pin!(stall);
        let stall_armed = call_timeout.is_some();
        let mut stalled = false;
        let outcome = loop {
            tokio::select! {
                r = &mut rx => break r.unwrap_or(Err(CallError::Closed)),
                c = &mut cancel_rx, if cancel_live => {
                    cancel_live = false;
                    if c.is_ok() {
                        break Err(CallError::Cancelled);
                    }
                }
                () = &mut stall, if stall_armed => {
                    stalled = true;
                    break Err(CallError::Unavailable("call timed out".into()));
                }
                () = &mut slow, if slow_armed => {
                    slow_armed = false;
                    if let Some(cd) = &cd {
                        cd.slow();
                    }
                }
            }
        };
        trace.backend = sending.elapsed();
        if let Some(cd) = &mut cd {
            cd.disconnect = false;
        }
        match outcome {
            Ok(resp) => {
                cancel.armed = false;
                if let Some(cd) = &cd {
                    let failed = resp.get("error").is_some()
                        || (resp["result"]["isError"] == Value::Bool(true) && tap.tool_errors());
                    if failed {
                        let schema = cd
                            .tool
                            .as_deref()
                            .and_then(|t| self.input_schema(session, server, t));
                        cd.answered(&resp, schema.as_ref());
                    }
                }
                resp
            }
            Err(CallError::Cancelled) => {
                if let Some(cd) = &cd {
                    cd.cancelled();
                }
                jsonrpc::error(id, jsonrpc::REQUEST_CANCELLED, "request cancelled")
            }
            Err(_) if stalled => {
                // `cancel` stays armed: the backend is told to give up.
                if let Some(cd) = &cd {
                    cd.stalled();
                }
                let secs = call_timeout.map_or(0, |t| t.as_secs());
                jsonrpc::error(
                    id,
                    jsonrpc::BACKEND_UNAVAILABLE,
                    format!("backend did not answer within {secs} s (call_timeout)"),
                )
            }
            Err(e) => {
                cancel.armed = false;
                if let (Some(cd), CallError::Closed) = (&cd, &e) {
                    if let Some(c) = guard.conn().crash_for(back_id).await {
                        let schema = cd
                            .tool
                            .as_deref()
                            .and_then(|t| self.input_schema(session, server, t));
                        cd.crashed(&c.incident, c.exit, c.suspect, schema.as_ref());
                    }
                }
                jsonrpc::error(id, jsonrpc::BACKEND_UNAVAILABLE, e.to_string())
            }
        }
    }
}
