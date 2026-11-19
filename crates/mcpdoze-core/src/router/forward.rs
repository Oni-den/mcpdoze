// SPDX-License-Identifier: GPL-3.0-or-later

//! Agent requests: dispatch by method and forwarding to backends (id,
//! progress-token and cancellation mapping).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::events::ProgressRoute;
use super::session::RootsState;
use super::usage::{CallStart, CallTrace};
use super::{naming, uri, Router, Server, Session};
use crate::backend::jsonrpc::{self, Kind};
use crate::backend::stdio::id_type_of;
use crate::backend::stdio::lock;
use crate::backend::Instance;
use crate::backend::{CallError, CallMeta, Conn, StateKind};
use crate::diag::{DiagTap, Era as DEra, ExitView, RequestView};
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
        let Some((server, tool)) = self.named(Some(&name)) else {
            return jsonrpc::error(id, jsonrpc::INVALID_PARAMS, format!("unknown tool: {name}"));
        };
        params["name"] = Value::String(tool.clone());
        let resp = self
            .forward_routed(session, &server, id, "tools/call", params)
            .await;
        // Stale catalog: the backend rejected a listed tool, or served one
        // the catalog does not know.
        let ckey = self
            .session_key_now(session)
            .and_then(|k| server.catalog_key_of(&k));
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
        mut params: Value,
    ) -> Value {
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
        let resp = self
            .forward(session, server, id, method, params, orig_token)
            .await;
        drop(route);
        resp
    }

    /// Forwards one request to `server`, starting it if needed, and maps the
    /// response back to the agent's id. Logged to the statistics log when
    /// one is set.
    pub(super) async fn forward(
        &self,
        session: &Arc<Session>,
        server: &Arc<Server>,
        id: Value,
        method: &str,
        params: Value,
        progress_token: Option<Value>,
    ) -> Value {
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
        if let Some((st, start)) = logged {
            self.record_call(&st, session, server, start, &trace, &resp);
        }
        resp
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
