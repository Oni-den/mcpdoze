// SPDX-License-Identifier: GPL-3.0-or-later

//! Backend events: notifications (progress, list changes, log messages) and
//! server-to-client requests routed to agent sessions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::{json, Value};

use super::{Router, Session};
use crate::backend::jsonrpc;
use crate::backend::stdio::lock;
use crate::backend::{BackendEvents, ServerRequest};
use crate::diag::Unserved;

/// Broker progress token -> (owning session, agent's token).
#[derive(Default)]
pub(super) struct ProgressTable {
    pub(super) next: AtomicU64,
    pub(super) routes: Mutex<HashMap<String, (Weak<Session>, Value)>>,
}

pub(super) struct RouterEvents {
    pub(super) router: Weak<Router>,
    pub(super) progress: Arc<ProgressTable>,
}

impl BackendEvents for RouterEvents {
    fn notification(&self, server: &str, scope_key: &str, mut msg: Value) {
        match msg["method"].as_str().unwrap_or("") {
            "notifications/progress" => {
                let Some(token) = msg["params"]["progressToken"].as_str().map(str::to_owned) else {
                    return;
                };
                let route = lock(&self.progress.routes).get(&token).cloned();
                if let Some((session, orig)) = route {
                    if let Some(session) = session.upgrade() {
                        msg["params"]["progressToken"] = orig;
                        session.push(msg);
                    }
                }
            }
            "notifications/tools/list_changed"
            | "notifications/prompts/list_changed"
            | "notifications/resources/list_changed" => {
                tracing::debug!(server, "backend list changed");
                if let Some(r) = self.router.upgrade() {
                    r.on_list_changed(server, scope_key);
                }
            }
            "notifications/resources/updated" => {
                if let Some(r) = self.router.upgrade() {
                    r.on_resource_updated(server, scope_key, msg);
                }
            }
            method => tracing::debug!(server, method, "backend notification dropped"),
        }
    }

    fn notification_from(&self, server: &str, scope_key: &str, msg: Value, sessions: Vec<String>) {
        if msg["method"] == "notifications/message" {
            if let Some(r) = self.router.upgrade() {
                r.deliver_to(&sessions, msg);
            }
            return;
        }
        self.notification(server, scope_key, msg);
    }

    fn server_request(&self, req: ServerRequest) {
        match self.router.upgrade() {
            Some(r) => r.route_server_request(req),
            None => req.reply.error(
                jsonrpc::METHOD_NOT_FOUND,
                "method not supported by the mcpdoze broker",
            ),
        }
    }
}

/// Removes a progress route when the call ends.
pub(super) struct ProgressRoute {
    pub(super) table: Arc<ProgressTable>,
    pub(super) token: String,
}

impl Drop for ProgressRoute {
    fn drop(&mut self) {
        lock(&self.table.routes).remove(&self.token);
    }
}

impl Router {
    /// Records a backend-to-client request that could not be served.
    pub(super) fn note_unserved(
        &self,
        server: &str,
        scope_key: &str,
        method: &str,
        reason: Unserved,
    ) {
        let Some(inst) = self.instance_for_scope(server, scope_key) else {
            return;
        };
        let tap = inst.tap();
        if tap.enabled() {
            inst.with_view(|b| tap.on_client_request_unserved(method, reason, b));
        }
    }

    pub(super) fn deliver_to(&self, sessions: &[String], msg: Value) {
        let targets: Vec<Arc<Session>> = {
            let map = lock(&self.sessions);
            sessions
                .iter()
                .filter_map(|id| map.get(id).cloned())
                .collect()
        };
        for s in targets {
            s.push(msg.clone());
        }
        // 2026-07-28 requests get log messages only on their own SSE
        // response, and only at or above the level they asked for.
        let level = msg["params"]["level"]
            .as_str()
            .and_then(super::modern::log_level_rank)
            .unwrap_or(0);
        for s in sessions.iter().filter_map(|id| self.live_session(id)) {
            if s.has_stream() && s.log_level().is_some_and(|min| level >= min) {
                s.push(msg.clone());
            }
        }
    }

    /// Routes a backend's server-to-client request to the owning agent
    /// session (see the module docs) or answers it with an error.
    pub(super) fn route_server_request(&self, req: ServerRequest) {
        let method = req.msg["method"].as_str().unwrap_or_default().to_owned();
        let target = match (&req.owner, req.sessions.as_slice()) {
            (Some(owner), _) => Some(owner.clone()),
            (None, [one]) => Some(one.clone()),
            _ => None,
        };
        if req.owner.is_none()
            && req
                .sessions
                .iter()
                .any(|id| self.live_session(id).is_some())
        {
            // 2026-07-28 forbids server-to-client requests on any stream;
            // mapping them to MRTR input requests is not implemented.
            tracing::debug!(server = %req.server, %method, "server request during a 2026-07-28 call declined");
            self.note_unserved(&req.server, &req.scope_key, &method, Unserved::Unsupported);
            return req.reply.error(
                jsonrpc::METHOD_NOT_FOUND,
                format!(
                    "{method} declined: the calling agent uses MCP 2026-07-28, which the \
                     mcpdoze broker cannot forward server-to-client requests to"
                ),
            );
        }
        let Some(session) = target.as_deref().and_then(|id| self.session(id)) else {
            tracing::debug!(server = %req.server, %method, sessions = req.sessions.len(), "server request not routable");
            self.note_unserved(&req.server, &req.scope_key, &method, Unserved::NotRouted);
            return req.reply.error(
                jsonrpc::METHOD_NOT_FOUND,
                format!(
                    "{method} is not supported here: the mcpdoze broker cannot tell which \
                     agent session it is for"
                ),
            );
        };
        if !session.has_stream() {
            self.note_unserved(&req.server, &req.scope_key, &method, Unserved::NotRouted);
            return req.reply.error(
                jsonrpc::METHOD_NOT_FOUND,
                format!(
                    "{method} is not supported here: the agent session has no open event stream"
                ),
            );
        }
        let n = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("mcpdoze-s2c-{n}");
        let params = req.msg.get("params").cloned().unwrap_or_else(|| json!({}));
        let limit = self.client_request_timeout();
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return req
                .reply
                .error(jsonrpc::INTERNAL_ERROR, "broker is shutting down");
        };
        let reply = req.reply;
        let (server, scope_key) = (req.server, req.scope_key);
        let me = self.me.clone();
        rt.spawn(async move {
            match session.client_request(id, &method, params, limit).await {
                Ok(resp) => reply.forward(resp),
                Err(e) => {
                    if let Some(r) = me.upgrade() {
                        let why = if e.contains("timed out") {
                            Unserved::TimedOut
                        } else {
                            Unserved::NotRouted
                        };
                        r.note_unserved(&server, &scope_key, &method, why);
                    }
                    reply.error(jsonrpc::INTERNAL_ERROR, e);
                }
            }
        });
    }
}
