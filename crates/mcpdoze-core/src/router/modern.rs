// SPDX-License-Identifier: GPL-3.0-or-later

//! The 2026-07-28 (stateless) lane of the router; see
//! `docs/decisions/0002-stateless-protocol-support.md`.
//!
//! * Each modern request runs on an ephemeral [`Era::Modern`] session that
//!   lives for that request (or for a `subscriptions/listen` stream) and is
//!   tracked in the router's live set meanwhile, so request-scoped log
//!   messages reach it and backend server-to-client requests can be
//!   recognised and declined.
//! * The modern `_meta` keys (`io.modelcontextprotocol/...`) are stripped
//!   before forwarding: backends stay on 2025-11-25. `progressToken` and
//!   other keys are kept.
//! * `server/discover` is answered from the broker's own capabilities.
//! * Results gain `resultType: "complete"` and the broker's `serverInfo` in
//!   `_meta`; list, read and discover results also gain `ttlMs` and
//!   `cacheScope: "private"` (answers depend on the client's directory).
//! * `subscriptions/listen` records the client's notification filter on the
//!   session and feeds `resourceSubscriptions` into the subscription
//!   registry (`subs.rs`).

use std::sync::{Arc, Weak};

use serde_json::{json, Map, Value};

use super::session::ListenFilter;
use super::{Era, Router, Session, SUPPORTED_PROTOCOL_VERSIONS};
use crate::backend::jsonrpc;
use crate::backend::stdio::lock;

/// The stateless protocol revision served on the modern lane.
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
/// `_meta` key carrying the protocol version of a modern request.
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
/// `_meta` key carrying the client capabilities.
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
/// `_meta` key carrying the requested log level.
pub const META_LOG_LEVEL: &str = "io.modelcontextprotocol/logLevel";
/// `_meta` key carrying the server info on results.
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
/// `_meta` key tagging messages of a `subscriptions/listen` stream.
pub const META_SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";
/// Prefix of every protocol-reserved `_meta` key (stripped before forwarding).
const META_PREFIX: &str = "io.modelcontextprotocol/";
/// `ttlMs` on cacheable results.
pub const RESULT_TTL_MS: u64 = 60_000;

/// Methods served on the modern lane.
pub const MODERN_METHODS: &[&str] = &[
    "server/discover",
    "tools/list",
    "tools/call",
    "prompts/list",
    "prompts/get",
    "resources/list",
    "resources/templates/list",
    "resources/read",
    "subscriptions/listen",
];

/// Methods whose `params.name` / `params.uri` is mirrored in `Mcp-Name`.
pub const NAMED_METHODS: &[&str] = &["tools/call", "resources/read", "prompts/get"];

const CACHEABLE: &[&str] = &[
    "server/discover",
    "tools/list",
    "prompts/list",
    "resources/list",
    "resources/templates/list",
    "resources/read",
];

/// Versions the broker serves, for `server/discover` and `-32022` data.
pub fn supported_versions() -> Vec<&'static str> {
    let mut v = vec![MODERN_PROTOCOL_VERSION];
    v.extend(SUPPORTED_PROTOCOL_VERSIONS);
    v
}

/// Syslog order of an MCP log level (0 = debug).
pub fn log_level_rank(level: &str) -> Option<u8> {
    [
        "debug",
        "info",
        "notice",
        "warning",
        "error",
        "critical",
        "alert",
        "emergency",
    ]
    .iter()
    .position(|l| *l == level)
    .map(|p| p as u8)
}

/// Drops a modern session from the live set when its request or stream ends.
pub struct LiveGuard {
    router: Weak<Router>,
    id: String,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        if let Some(r) = self.router.upgrade() {
            lock(&r.live).remove(&self.id);
        }
    }
}

impl Router {
    /// The broker's aggregated capabilities.
    pub(super) fn capabilities(&self) -> Value {
        json!({
            "tools": {"listChanged": true},
            "prompts": {"listChanged": true},
            "resources": {"listChanged": true, "subscribe": self.any_subscribe()},
        })
    }

    fn server_info() -> Value {
        json!({"name": "mcpdoze", "version": env!("CARGO_PKG_VERSION")})
    }

    /// A fresh session for one 2026-07-28 request (or listen stream),
    /// tracked as live until the returned guard drops. `meta` is the
    /// request's `params._meta`.
    pub fn modern_session(&self, meta: &Value) -> (Arc<Session>, LiveGuard) {
        let session = Arc::new(Session::new(MODERN_PROTOCOL_VERSION, true));
        session.set_era(Era::Modern);
        session.set_log_level(meta[META_LOG_LEVEL].as_str().and_then(log_level_rank));
        lock(&self.live).insert(session.id.clone(), Arc::downgrade(&session));
        let guard = LiveGuard {
            router: self.me.clone(),
            id: session.id.clone(),
        };
        (session, guard)
    }

    /// 2026-07-28 requests and listen streams in progress.
    pub fn live_session_count(&self) -> usize {
        self.live_sessions().len()
    }

    /// A live 2026-07-28 session by id.
    pub(super) fn live_session(&self, id: &str) -> Option<Arc<Session>> {
        lock(&self.live).get(id).and_then(Weak::upgrade)
    }

    /// Live 2026-07-28 sessions.
    pub(super) fn live_sessions(&self) -> Vec<Arc<Session>> {
        lock(&self.live)
            .values()
            .filter_map(Weak::upgrade)
            .collect()
    }

    /// Handles one 2026-07-28 request (already validated by the HTTP layer,
    /// not `subscriptions/listen`) and returns the response.
    pub async fn handle_modern(&self, session: &Arc<Session>, mut msg: Value) -> Value {
        session.touch();
        let method = msg["method"].as_str().unwrap_or_default().to_owned();
        strip_modern_meta(&mut msg);
        let resp = match method.as_str() {
            "server/discover" => jsonrpc::result(msg["id"].clone(), self.discover_result()),
            _ => self.request(session, msg).await,
        };
        decorate(&method, resp)
    }

    fn discover_result(&self) -> Value {
        json!({
            "supportedVersions": supported_versions(),
            "capabilities": self.capabilities(),
            "instructions": "MCPDoze broker: tools and prompts are named <prefix>__<name>; \
                             resource URIs are mapped to mcpdoze://<prefix>/<encoded-uri>.",
        })
    }

    /// Sets up a `subscriptions/listen` session: records the notification
    /// filter and subscribes the requested resources. Returns the
    /// `notifications` object the broker honours (the acknowledgment).
    pub async fn listen(&self, session: &Arc<Session>, params: &Value) -> Value {
        let n = &params["notifications"];
        let filter = ListenFilter {
            tools: n["toolsListChanged"] == true,
            prompts: n["promptsListChanged"] == true,
            resources: n["resourcesListChanged"] == true,
        };
        session.set_listen(filter);
        let mut honored = Map::new();
        for (on, key) in [
            (filter.tools, "toolsListChanged"),
            (filter.prompts, "promptsListChanged"),
            (filter.resources, "resourcesListChanged"),
        ] {
            if on {
                honored.insert(key.into(), Value::Bool(true));
            }
        }
        let mut uris = Vec::new();
        for u in n["resourceSubscriptions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            match self.subscribe_uri(session, u).await {
                Ok(()) => uris.push(Value::String(u.to_owned())),
                Err((_, e)) => {
                    tracing::info!(error = %e, "listen: resource subscription not honoured")
                }
            }
        }
        if !uris.is_empty() {
            honored.insert("resourceSubscriptions".into(), Value::Array(uris));
        }
        Value::Object(honored)
    }
}

/// Removes the protocol-reserved `_meta` keys (and an emptied `_meta`).
fn strip_modern_meta(msg: &mut Value) {
    let Some(params) = msg.get_mut("params").and_then(Value::as_object_mut) else {
        return;
    };
    let empty = match params.get_mut("_meta").and_then(Value::as_object_mut) {
        Some(meta) => {
            meta.retain(|k, _| !k.starts_with(META_PREFIX));
            meta.is_empty()
        }
        None => false,
    };
    if empty {
        params.remove("_meta");
    }
}

/// Adds the 2026-07-28 result fields to a success response.
fn decorate(method: &str, mut resp: Value) -> Value {
    let Some(result) = resp.get_mut("result").and_then(Value::as_object_mut) else {
        return resp;
    };
    result
        .entry("resultType")
        .or_insert_with(|| Value::String("complete".into()));
    if CACHEABLE.contains(&method) {
        result.insert("ttlMs".into(), json!(RESULT_TTL_MS));
        result.insert("cacheScope".into(), json!("private"));
    }
    let meta = result.entry("_meta").or_insert_with(|| json!({}));
    if let Some(m) = meta.as_object_mut() {
        m.insert(META_SERVER_INFO.into(), Router::server_info());
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_reserved_meta() {
        let mut m = json!({"id":1,"method":"tools/call","params":{"name":"x","_meta":{
            META_PROTOCOL_VERSION: "2026-07-28", META_LOG_LEVEL: "info",
            "progressToken": 5, "trace": "t"}}});
        strip_modern_meta(&mut m);
        assert_eq!(
            m["params"]["_meta"],
            json!({"progressToken": 5, "trace": "t"})
        );
        let mut m = json!({"params":{"_meta":{META_PROTOCOL_VERSION: "x"}}});
        strip_modern_meta(&mut m);
        assert_eq!(m["params"], json!({}));
    }

    #[test]
    fn decorates_results_not_errors() {
        let r = decorate(
            "tools/list",
            jsonrpc::result(json!(1), json!({"tools": []})),
        );
        assert_eq!(r["result"]["resultType"], "complete");
        assert_eq!(r["result"]["cacheScope"], "private");
        assert_eq!(r["result"]["ttlMs"], RESULT_TTL_MS);
        assert_eq!(r["result"]["_meta"][META_SERVER_INFO]["name"], "mcpdoze");
        let r = decorate(
            "tools/call",
            jsonrpc::result(json!(1), json!({"content": []})),
        );
        assert!(r["result"].get("ttlMs").is_none());
        let e = decorate("tools/list", jsonrpc::error(json!(1), -1, "x"));
        assert!(e.get("result").is_none());
        assert_eq!(log_level_rank("warning"), Some(3));
        assert_eq!(log_level_rank("loud"), None);
        assert_eq!(supported_versions()[0], MODERN_PROTOCOL_VERSION);
    }
}
