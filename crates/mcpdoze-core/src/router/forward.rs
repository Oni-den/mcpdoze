// SPDX-License-Identifier: GPL-3.0-or-later

//! Agent requests: dispatch by method and forwarding to backends (id,
//! progress-token and cancellation mapping).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::events::ProgressRoute;
use super::session::RootsState;
use super::{naming, uri, Router, Server, Session};
use crate::backend::jsonrpc::{self, Kind};
use crate::backend::stdio::lock;
use crate::backend::{CallError, CallMeta, Conn};
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

impl Router {
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
            "resources/subscribe" | "resources/unsubscribe" => jsonrpc::error(
                id,
                jsonrpc::METHOD_NOT_FOUND,
                "resource subscriptions are not supported by the mcpdoze broker yet",
            ),
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
    /// response back to the agent's id.
    pub(super) async fn forward(
        &self,
        session: &Arc<Session>,
        server: &Arc<Server>,
        id: Value,
        method: &str,
        params: Value,
        progress_token: Option<Value>,
    ) -> Value {
        let inst = match self.instance_for(session, server).await {
            Ok(i) => i,
            Err((code, msg)) => return jsonrpc::error(id, code, msg),
        };
        let guard = match inst.acquire().await {
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
        let (back_id, rx) = match guard.conn().begin(method, params, meta).await {
            Ok(x) => x,
            Err(e) => return jsonrpc::error(id, jsonrpc::BACKEND_UNAVAILABLE, e.to_string()),
        };
        // Declared after `guard`, so it runs first on drop: the backend hears
        // about the cancellation before the call stops counting as in flight.
        let mut cancel = CancelOnDrop {
            conn: guard.conn().clone(),
            id: back_id,
            armed: true,
        };
        let outcome = tokio::select! {
            r = rx => r.unwrap_or(Err(CallError::Closed)),
            Ok(()) = &mut cancel_rx => Err(CallError::Cancelled),
        };
        match outcome {
            Ok(resp) => {
                cancel.armed = false;
                resp
            }
            Err(CallError::Cancelled) => {
                jsonrpc::error(id, jsonrpc::REQUEST_CANCELLED, "request cancelled")
            }
            Err(e) => {
                cancel.armed = false;
                jsonrpc::error(id, jsonrpc::BACKEND_UNAVAILABLE, e.to_string())
            }
        }
    }
}
