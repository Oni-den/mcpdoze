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
        let Some(session) = target.as_deref().and_then(|id| self.session(id)) else {
            tracing::debug!(server = %req.server, %method, sessions = req.sessions.len(), "server request not routable");
            return req.reply.error(
                jsonrpc::METHOD_NOT_FOUND,
                format!(
                    "{method} is not supported here: the mcpdoze broker cannot tell which \
                     agent session it is for"
                ),
            );
        };
        if !session.has_stream() {
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
        rt.spawn(async move {
            match session.client_request(id, &method, params, limit).await {
                Ok(resp) => reply.forward(resp),
                Err(e) => reply.error(jsonrpc::INTERNAL_ERROR, e),
            }
        });
    }
}
