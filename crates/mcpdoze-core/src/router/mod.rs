// SPDX-License-Identifier: GPL-3.0-or-later

//! The router: agent sessions on one side, backend instances on the other.
//!
//! * Agent sessions are broker-side objects with their own negotiated
//!   protocol version; every backend sees one synthetic client, the broker.
//! * Tools are exposed as `<prefix>__<tool>` ([`naming`]).
//! * Each forwarded request gets a fresh per-backend id; the backend id map
//!   holds `(session, original id, progress token)` until the response.
//! * Progress tokens are rewritten to broker tokens and routed back by token.
//! * `notifications/cancelled` from an agent is translated to the backend id;
//!   a dropped call future (agent disconnect) or an ended session cancels the
//!   backend call too.
//! * `tools/list` is answered from the [`Catalog`]; a server without a
//!   snapshot is started once to list its tools.

pub mod catalog;
pub mod naming;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::backend::jsonrpc::{self, Kind};
use crate::backend::stdio::{lock, StdioConn};
use crate::backend::{BackendEvents, BackendSpec, CallError, CallMeta, Instance, Tuning};
pub use catalog::{Catalog, MemoryCatalog};

/// Protocol versions accepted from agents, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
/// Most sessions kept; the least recently used is evicted beyond this.
pub const MAX_SESSIONS: usize = 1024;
/// Notifications buffered per session for delivery (oldest dropped).
pub const SESSION_OUTBOX: usize = 256;
/// Most `tools/list` pages fetched from one backend.
const MAX_LIST_PAGES: usize = 64;

/// True if `v` is a protocol version the broker speaks with agents.
pub fn is_supported_version(v: &str) -> bool {
    SUPPORTED_PROTOCOL_VERSIONS.contains(&v)
}

/// Why a registration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegisterError {
    /// The prefix is not `[a-z0-9_]{1,32}` without `__`.
    #[error("invalid prefix {0:?}")]
    InvalidPrefix(String),
    /// Another server already uses this name.
    #[error("duplicate server name {0:?}")]
    DuplicateName(String),
    /// Another server already uses this prefix.
    #[error("duplicate prefix {0:?}")]
    DuplicatePrefix(String),
}

/// A registered backend server.
#[derive(Debug)]
pub struct Server {
    /// Unique server name.
    pub name: String,
    /// Unique tool prefix.
    pub prefix: String,
    /// The single shared instance.
    pub instance: Arc<Instance>,
}

#[derive(Default)]
struct Registry {
    by_name: BTreeMap<String, Arc<Server>>,
    by_prefix: BTreeMap<String, Arc<Server>>,
}

/// One agent session.
#[derive(Debug)]
pub struct Session {
    id: String,
    protocol_version: String,
    ephemeral: bool,
    inflight: Mutex<HashMap<String, oneshot::Sender<()>>>,
    outbox: Mutex<VecDeque<Value>>,
    last_used: Mutex<Instant>,
}

impl Session {
    fn new(protocol_version: &str, ephemeral: bool) -> Self {
        Self {
            id: random_id(),
            protocol_version: protocol_version.to_owned(),
            ephemeral,
            inflight: Mutex::new(HashMap::new()),
            outbox: Mutex::new(VecDeque::new()),
            last_used: Mutex::new(Instant::now()),
        }
    }

    /// Session id (the `Mcp-Session-Id` value).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Negotiated protocol version.
    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// True for a per-request session of a stateless client.
    pub fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// Requests of this session currently forwarded to a backend.
    pub fn in_flight(&self) -> usize {
        lock(&self.inflight).len()
    }

    /// Drains notifications routed to this session (e.g. progress).
    ///
    /// JSON response mode cannot deliver them yet; an SSE stream will.
    pub fn take_notifications(&self) -> Vec<Value> {
        lock(&self.outbox).drain(..).collect()
    }

    /// Cancels every in-flight request of this session.
    pub fn cancel_all(&self) {
        let all: Vec<_> = lock(&self.inflight).drain().map(|(_, tx)| tx).collect();
        for tx in all {
            let _ = tx.send(());
        }
    }

    fn cancel(&self, id: &Value) -> bool {
        match lock(&self.inflight).remove(&jsonrpc::id_key(id)) {
            Some(tx) => tx.send(()).is_ok(),
            None => false,
        }
    }

    fn push(&self, msg: Value) {
        let mut q = lock(&self.outbox);
        if q.len() == SESSION_OUTBOX {
            q.pop_front();
        }
        q.push_back(msg);
    }

    fn touch(&self) {
        *lock(&self.last_used) = Instant::now();
    }
}

/// 128-bit session id from the std randomly keyed hasher plus a counter and
/// the clock. Not a security boundary: M5 adds bearer-token auth.
fn random_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut out = String::with_capacity(32);
    for salt in 0..2u8 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(n);
        h.write_u128(t);
        h.write_u8(salt);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Broker progress token -> (owning session, agent's token).
#[derive(Default)]
struct ProgressTable {
    next: AtomicU64,
    routes: Mutex<HashMap<String, (Weak<Session>, Value)>>,
}

struct RouterEvents {
    progress: Arc<ProgressTable>,
}

impl BackendEvents for RouterEvents {
    fn notification(&self, server: &str, mut msg: Value) {
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
            // M4 hook: refresh the catalog and notify stateful sessions.
            "notifications/tools/list_changed" => {
                tracing::debug!(server, "backend tool list changed")
            }
            method => tracing::debug!(server, method, "backend notification dropped"),
        }
    }
}

/// Removes a progress route when the call ends.
struct ProgressRoute {
    table: Arc<ProgressTable>,
    token: String,
}

impl Drop for ProgressRoute {
    fn drop(&mut self) {
        lock(&self.table.routes).remove(&self.token);
    }
}

/// Removes a session's in-flight entry when the call ends.
struct InflightEntry<'a> {
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
struct CancelOnDrop {
    conn: Arc<StdioConn>,
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

/// The broker core shared by every transport.
pub struct Router {
    registry: RwLock<Registry>,
    catalog: Arc<dyn Catalog>,
    progress: Arc<ProgressTable>,
    events: Arc<RouterEvents>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    tuning: Tuning,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router").finish_non_exhaustive()
    }
}

impl Router {
    /// A router with an in-memory catalog and default lifecycle timing.
    pub fn new() -> Arc<Self> {
        Self::with(Arc::new(MemoryCatalog::default()), Tuning::default())
    }

    /// A router with an explicit catalog and lifecycle timing.
    pub fn with(catalog: Arc<dyn Catalog>, tuning: Tuning) -> Arc<Self> {
        let progress = Arc::new(ProgressTable::default());
        Arc::new(Self {
            registry: RwLock::new(Registry::default()),
            catalog,
            events: Arc::new(RouterEvents {
                progress: progress.clone(),
            }),
            progress,
            sessions: Mutex::new(HashMap::new()),
            tuning,
        })
    }

    /// Registers a stdio backend. Nothing is spawned.
    pub fn register(
        &self,
        name: &str,
        prefix: &str,
        spec: BackendSpec,
    ) -> Result<(), RegisterError> {
        if !naming::valid_prefix(prefix) {
            return Err(RegisterError::InvalidPrefix(prefix.to_owned()));
        }
        let mut reg = self.registry.write().unwrap_or_else(|e| e.into_inner());
        if reg.by_name.contains_key(name) {
            return Err(RegisterError::DuplicateName(name.to_owned()));
        }
        if reg.by_prefix.contains_key(prefix) {
            return Err(RegisterError::DuplicatePrefix(prefix.to_owned()));
        }
        let server = Arc::new(Server {
            name: name.to_owned(),
            prefix: prefix.to_owned(),
            instance: Instance::with_tuning(name, spec, self.events.clone(), self.tuning),
        });
        reg.by_name.insert(name.to_owned(), server.clone());
        reg.by_prefix.insert(prefix.to_owned(), server);
        Ok(())
    }

    fn servers(&self) -> Vec<Arc<Server>> {
        let reg = self.registry.read().unwrap_or_else(|e| e.into_inner());
        reg.by_name.values().cloned().collect()
    }

    fn by_prefix(&self, prefix: &str) -> Option<Arc<Server>> {
        let reg = self.registry.read().unwrap_or_else(|e| e.into_inner());
        reg.by_prefix.get(prefix).cloned()
    }

    /// The instance of server `name`.
    pub fn instance(&self, name: &str) -> Option<Arc<Instance>> {
        let reg = self.registry.read().unwrap_or_else(|e| e.into_inner());
        reg.by_name.get(name).map(|s| s.instance.clone())
    }

    /// The tool catalog.
    pub fn catalog(&self) -> &Arc<dyn Catalog> {
        &self.catalog
    }

    /// Negotiates a protocol version and builds the `initialize` result.
    pub fn initialize_result(&self, requested: Option<&str>) -> (String, Value) {
        let version = requested
            .filter(|v| is_supported_version(v))
            .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0])
            .to_owned();
        let result = json!({
            "protocolVersion": version,
            // listChanged arrives with the M4 catalog.
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "mcpdoze", "version": env!("CARGO_PKG_VERSION")},
        });
        (version, result)
    }

    /// Creates and stores a session (evicting the least recently used one
    /// beyond [`MAX_SESSIONS`]).
    pub fn create_session(&self, protocol_version: &str) -> Arc<Session> {
        let session = Arc::new(Session::new(protocol_version, false));
        let evicted = {
            let mut map = lock(&self.sessions);
            let evicted = if map.len() >= MAX_SESSIONS {
                let oldest = map
                    .iter()
                    .min_by_key(|(_, s)| *lock(&s.last_used))
                    .map(|(k, _)| k.clone());
                oldest.and_then(|k| map.remove(&k))
            } else {
                None
            };
            map.insert(session.id.clone(), session.clone());
            evicted
        };
        if let Some(old) = evicted {
            old.cancel_all();
        }
        session
    }

    /// A session for one request of a stateless client; not stored.
    pub fn ephemeral_session(&self, protocol_version: &str) -> Arc<Session> {
        Arc::new(Session::new(protocol_version, true))
    }

    /// Looks up a stored session.
    pub fn session(&self, id: &str) -> Option<Arc<Session>> {
        lock(&self.sessions).get(id).cloned()
    }

    /// Number of stored sessions.
    pub fn session_count(&self) -> usize {
        lock(&self.sessions).len()
    }

    /// Ends a session and cancels its in-flight calls. False if unknown.
    pub fn end_session(&self, id: &str) -> bool {
        let session = lock(&self.sessions).remove(id);
        match session {
            Some(s) => {
                s.cancel_all();
                true
            }
            None => false,
        }
    }

    /// Stops every backend (broker shutdown).
    pub async fn shutdown(&self) {
        for s in self.servers() {
            s.instance.shutdown().await;
        }
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
                // No server-to-client requests are forwarded yet (M7).
                tracing::debug!("agent response without a pending request dropped");
                None
            }
            Kind::Invalid => Some(jsonrpc::error(
                msg.get("id").cloned().unwrap_or(Value::Null),
                jsonrpc::INVALID_REQUEST,
                "invalid JSON-RPC message",
            )),
        }
    }

    fn notification(&self, session: &Session, msg: &Value) {
        match msg["method"].as_str().unwrap_or("") {
            "notifications/cancelled" => {
                if let Some(rid) = msg["params"].get("requestId") {
                    session.cancel(rid);
                }
            }
            "notifications/initialized" => {}
            method => tracing::debug!(method, "agent notification ignored"),
        }
    }

    async fn request(&self, session: &Arc<Session>, msg: Value) -> Value {
        let id = msg["id"].clone();
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        match msg["method"].as_str().unwrap_or("") {
            "initialize" => {
                let requested = params["protocolVersion"].as_str();
                jsonrpc::result(id, self.initialize_result(requested).1)
            }
            "ping" => jsonrpc::result(id, json!({})),
            "tools/list" => jsonrpc::result(id, json!({"tools": self.list_tools().await})),
            "tools/call" => self.call_tool(session, id, params).await,
            // Not advertised yet (M4); answer empty so lenient clients work.
            "prompts/list" => jsonrpc::result(id, json!({"prompts": []})),
            "resources/list" => jsonrpc::result(id, json!({"resources": []})),
            "resources/templates/list" => jsonrpc::result(id, json!({"resourceTemplates": []})),
            method => jsonrpc::error(
                id,
                jsonrpc::METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            ),
        }
    }

    /// Aggregated, prefixed tool list. Servers without a snapshot are
    /// started once to list; failures are logged and skipped.
    pub async fn list_tools(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for server in self.servers() {
            let tools = match self.catalog.get(&server.name) {
                Some(t) => t,
                None => match self.refresh_tools(&server).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(server = %server.name, error = %e, "tools/list failed");
                        continue;
                    }
                },
            };
            for tool in tools.iter() {
                let Some(name) = tool["name"].as_str() else {
                    continue;
                };
                let mut t = tool.clone();
                t["name"] = Value::String(naming::join(&server.prefix, name));
                out.push(t);
            }
        }
        out
    }

    /// Lists a server's tools from the backend (starting it if needed) and
    /// stores the snapshot.
    async fn refresh_tools(&self, server: &Server) -> Result<Arc<Vec<Value>>, CallError> {
        let guard = server.instance.acquire().await?;
        let mut tools = Vec::new();
        let mut cursor: Option<Value> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let resp = guard
                .conn()
                .call("tools/list", params, CallMeta::default())
                .await?;
            let Some(result) = resp.get("result") else {
                return Err(CallError::Unavailable(format!(
                    "tools/list error: {}",
                    resp.get("error").cloned().unwrap_or(Value::Null)
                )));
            };
            if let Some(list) = result["tools"].as_array() {
                tools.extend(list.iter().cloned());
            }
            cursor = result.get("nextCursor").filter(|c| !c.is_null()).cloned();
            if cursor.is_none() {
                break;
            }
        }
        self.catalog.set(&server.name, tools);
        Ok(self.catalog.get(&server.name).unwrap_or_default())
    }

    async fn call_tool(&self, session: &Arc<Session>, id: Value, mut params: Value) -> Value {
        let Some(name) = params["name"].as_str() else {
            return jsonrpc::error(id, jsonrpc::INVALID_PARAMS, "tools/call needs a name");
        };
        let Some((server, tool)) =
            naming::split(name).and_then(|(p, t)| self.by_prefix(p).map(|s| (s, t.to_owned())))
        else {
            return jsonrpc::error(id, jsonrpc::INVALID_PARAMS, format!("unknown tool: {name}"));
        };
        params["name"] = Value::String(tool);
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
            .forward(session, &server, id, "tools/call", params, orig_token)
            .await;
        drop(route);
        resp
    }

    /// Forwards one request to `server`, starting it if needed, and maps the
    /// response back to the agent's id.
    async fn forward(
        &self,
        session: &Arc<Session>,
        server: &Server,
        id: Value,
        method: &str,
        params: Value,
        progress_token: Option<Value>,
    ) -> Value {
        let guard = match server.instance.acquire().await {
            Ok(g) => g,
            Err(e) => return jsonrpc::error(id, jsonrpc::BACKEND_UNAVAILABLE, e.to_string()),
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_versions() {
        let r = Router::new();
        assert_eq!(r.initialize_result(Some("2025-06-18")).0, "2025-06-18");
        assert_eq!(r.initialize_result(Some("1999-01-01")).0, "2025-11-25");
        assert_eq!(r.initialize_result(None).0, "2025-11-25");
    }

    #[test]
    fn registration_rejects_duplicates() {
        let r = Router::new();
        let spec = || BackendSpec::new("prog", Vec::<String>::new());
        r.register("a", "pa", spec()).unwrap();
        assert_eq!(
            r.register("a", "pb", spec()),
            Err(RegisterError::DuplicateName("a".into()))
        );
        assert_eq!(
            r.register("b", "pa", spec()),
            Err(RegisterError::DuplicatePrefix("pa".into()))
        );
        assert!(matches!(
            r.register("c", "Bad", spec()),
            Err(RegisterError::InvalidPrefix(_))
        ));
    }

    #[test]
    fn session_ids_are_unique() {
        let a = random_id();
        assert_eq!(a.len(), 32);
        assert_ne!(a, random_id());
    }

    #[tokio::test]
    async fn unknown_tool_and_method() {
        let r = Router::new();
        let s = r.create_session("2025-11-25");
        let resp = r
            .handle(
                &s,
                jsonrpc::request(1, "tools/call", json!({"name": "x__y"})),
            )
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], jsonrpc::INVALID_PARAMS);
        let resp = r
            .handle(&s, jsonrpc::request(2, "nope", json!({})))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], jsonrpc::METHOD_NOT_FOUND);
        assert!(r
            .handle(
                &s,
                jsonrpc::notification("notifications/initialized", json!({}))
            )
            .await
            .is_none());
        assert!(r.end_session(s.id()));
        assert!(!r.end_session(s.id()));
    }
}
