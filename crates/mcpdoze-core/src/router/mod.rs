// SPDX-License-Identifier: GPL-3.0-or-later

//! The router: agent sessions on one side, backend instances on the other.
//!
//! * Agent sessions are broker-side objects with their own negotiated
//!   protocol version; every backend sees one synthetic client, the broker.
//! * Tools and prompts are exposed as `<prefix>__<name>` ([`naming`]);
//!   resource URIs are mapped reversibly to `mcpdoze://<prefix>/...`
//!   ([`uri`]). Resource subscriptions are not supported (v0.2).
//! * Each forwarded request gets a fresh per-backend id; the backend id map
//!   holds `(session, original id, progress token)` until the response.
//! * Progress tokens are rewritten to broker tokens and routed back by token.
//! * `notifications/cancelled` from an agent is translated to the backend id;
//!   a dropped call future (agent disconnect) or an ended session cancels the
//!   backend call too.
//!
//! # Catalog
//!
//! `tools/list`, `prompts/list`, `resources/list` and
//! `resources/templates/list` are answered from the [`Catalog`], with no
//! backend running. A server registered with `probe` and no valid catalog
//! entry (missing, or another launch-spec [`fingerprint`]) is probed once in
//! the background: a throwaway instance is started, listed and stopped. A
//! server without an entry is started (its real instance) on the first list.
//!
//! The catalog is refreshed in the background, under one lock per server:
//! after every spawn of a catalogued backend (re-list on the new process),
//! on backend `notifications/*/list_changed`, after a tool call the catalog
//! does not list or the backend rejects as unknown, when the entry is older
//! than the server's `max_age`, and on demand ([`Router::refresh`]). A
//! non-empty diff is stored and `notifications/<list>/list_changed` is sent
//! to every session with an open notification stream.
//!
//! # Notifications to agents
//!
//! Each session has one bounded queue ([`SESSION_OUTBOX`], oldest dropped and
//! counted). Progress goes to the session owning the token; backend
//! `notifications/message` goes to the sessions with a call in flight on that
//! backend connection; list changes go to sessions with an open stream. The
//! HTTP layer drains the queue into the session's `GET /mcp` event stream.
//!
//! # Scopes
//!
//! Backend instances are keyed by `(server, scope_key)` (see
//! `docs/SCOPES.md`): `shared` uses the one [`Server::instance`];
//! `per-session` keys by agent session id; `per-cwd` keys by the agent's
//! working directory (its first `file://` root, else `X-Mcpdoze-Cwd` /
//! `?cwd=`), falling back to the shared instance when unknown. Each instance
//! has its own lifecycle; the catalog stays one entry per server. At most
//! `max_instances` scoped instances exist per server.
//!
//! # Server-to-client requests
//!
//! A backend request (sampling, elicitation, ...) goes to the instance's
//! owning session (per-session) or to the only session with calls in flight
//! on that connection, over the session's event stream with a broker id; the
//! agent's POSTed response is forwarded back. Ambiguous, stream-less or
//! timed-out requests get a JSON-RPC error. `ping` and `roots/list` are
//! answered by the backend layer itself.

pub mod catalog;
pub mod naming;
pub mod uri;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::task::Waker;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::backend::jsonrpc::{self, Kind};
use crate::backend::stdio::lock;
use crate::backend::{
    BackendEvents, BackendSpec, CallError, CallMeta, ClientSide, Conn, Instance, NoEvents,
    ServerRequest, StateKind, Tuning,
};
use crate::config::Scope;
pub use catalog::{
    fingerprint, Catalog, CatalogEntry, DiskCatalog, ListChanges, MemoryCatalog, CATALOG_VERSION,
};

/// Protocol versions accepted from agents, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
/// Most sessions kept; the least recently used is evicted beyond this.
pub const MAX_SESSIONS: usize = 1024;
/// Notifications buffered per session for delivery (oldest dropped).
pub const SESSION_OUTBOX: usize = 256;
/// Interval of keep-alive comments on notification streams.
pub const SSE_KEEPALIVE: Duration = Duration::from_secs(25);
/// Default wait for an agent's answer to a forwarded server-to-client request.
pub const DEFAULT_CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Default idle time after which a session without a stream is ended.
pub const DEFAULT_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Longest wait for an agent's `roots/list` answer.
pub const ROOTS_TIMEOUT: Duration = Duration::from_secs(10);
/// Most server-to-client requests awaiting an answer per session.
pub const MAX_CLIENT_PENDING: usize = 32;
/// Default cap on scoped instances per server.
pub const DEFAULT_MAX_INSTANCES: usize = crate::config::DEFAULT_MAX_INSTANCES;
/// Longest wait for one list request to a backend.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Most pages fetched from one backend per list.
const MAX_LIST_PAGES: usize = 64;
/// Most items kept per list and backend.
const MAX_LIST_ITEMS: usize = 10_000;

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

/// Catalog behaviour of one registered server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CatalogOptions {
    /// Probe in the background at registration when no valid entry exists.
    pub probe: bool,
    /// Refresh in the background once the entry is older than this.
    pub max_age: Option<Duration>,
}

/// Result of one catalog refresh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshOutcome {
    /// Any list differed from the stored entry.
    pub changed: bool,
    /// Tools listed.
    pub tools: usize,
    /// Prompts listed.
    pub prompts: usize,
    /// Resources listed.
    pub resources: usize,
    /// Resource templates listed.
    pub resource_templates: usize,
}

/// Instance scoping of one registered server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeOptions {
    /// How instances are keyed.
    pub scope: Scope,
    /// Most scoped instances at once (the shared fallback is not counted).
    pub max_instances: usize,
}

impl Default for ScopeOptions {
    fn default() -> Self {
        Self {
            scope: Scope::Shared,
            max_instances: DEFAULT_MAX_INSTANCES,
        }
    }
}

/// Stable name of a scope (`shared`, `per-cwd`, `per-session`).
pub fn scope_name(s: Scope) -> &'static str {
    match s {
        Scope::Shared => "shared",
        Scope::PerCwd => "per-cwd",
        Scope::PerSession => "per-session",
    }
}

/// A registered backend server.
pub struct Server {
    /// Unique server name.
    pub name: String,
    /// Unique tool prefix.
    pub prefix: String,
    /// The shared instance: the only one for `shared` scope, the fallback
    /// (agent directory unknown, stateless client) for the other scopes.
    pub instance: Arc<Instance>,
    scope: ScopeOptions,
    /// Launch spec as registered (`${cwd}` unexpanded).
    base_spec: BackendSpec,
    /// Scoped instances by key (session id or normalised directory).
    scoped: Mutex<BTreeMap<String, Arc<Instance>>>,
    fingerprint: String,
    options: CatalogOptions,
    /// Serialises listing and catalog updates of this server.
    refresh_lock: tokio::sync::Mutex<()>,
    /// Instance generation whose lists are known (0: none).
    listed_gen: AtomicU64,
    /// A deduplicated background refresh is queued or running.
    bg_refresh: AtomicBool,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("name", &self.name)
            .field("prefix", &self.prefix)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Fingerprint of the launch spec.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Catalog options.
    pub fn catalog_options(&self) -> CatalogOptions {
        self.options
    }

    /// Scope options.
    pub fn scope_options(&self) -> ScopeOptions {
        self.scope
    }

    /// The instance scope.
    pub fn scope(&self) -> Scope {
        self.scope.scope
    }

    /// Every instance: the shared one first (key `""`), then the scoped
    /// ones by key (stopped, unreferenced ones are dropped first). For
    /// scoped servers the shared fallback is listed only once it has been
    /// used.
    pub fn instances(&self) -> Vec<(String, Arc<Instance>)> {
        let mut out = Vec::new();
        if self.scope.scope == Scope::Shared || self.instance.spawn_count() > 0 {
            out.push((String::new(), self.instance.clone()));
        }
        let mut map = lock(&self.scoped);
        Server::prune(&mut map, false);
        for (k, i) in map.iter() {
            out.push((display_key(self.scope.scope, k), i.clone()));
        }
        out
    }

    /// Every instance including an unused shared fallback (stop, unload).
    pub fn all_instances(&self) -> Vec<Arc<Instance>> {
        let mut out = vec![self.instance.clone()];
        out.extend(lock(&self.scoped).values().cloned());
        out
    }

    /// Scoped instances currently tracked (the shared fallback excluded).
    pub fn scoped_count(&self) -> usize {
        lock(&self.scoped).len()
    }

    /// The scoped instance with raw key `key` (session id or normalised
    /// directory), if any.
    pub fn scoped_instance(&self, key: &str) -> Option<Arc<Instance>> {
        lock(&self.scoped).get(key).cloned()
    }

    /// A running connection of any instance (catalog re-lists).
    pub fn any_running_conn(&self) -> Option<Arc<Conn>> {
        self.all_instances().iter().find_map(|i| i.running_conn())
    }

    /// The most active state over all instances.
    pub fn state(&self) -> StateKind {
        fn rank(s: StateKind) -> u8 {
            match s {
                StateKind::Running => 5,
                StateKind::Starting => 4,
                StateKind::Draining => 3,
                StateKind::Stopping => 2,
                StateKind::Failed => 1,
                StateKind::Stopped => 0,
            }
        }
        self.all_instances()
            .iter()
            .map(|i| i.state())
            .max_by_key(|s| rank(*s))
            .unwrap_or(StateKind::Stopped)
    }

    /// Drops scoped instances nothing else references and that hold no
    /// process (`Stopped`; with `failed`, also `Failed`). Caller holds the
    /// map lock.
    fn prune(map: &mut BTreeMap<String, Arc<Instance>>, failed: bool) {
        map.retain(|_, i| {
            let idle = match i.state() {
                StateKind::Stopped => true,
                StateKind::Failed => failed,
                _ => false,
            };
            !(idle && Arc::strong_count(i) == 1 && i.in_flight() == 0)
        });
    }
}

/// Status form of a raw scope key: the directory for `per-cwd`, a short
/// `session:<prefix>` for `per-session` (session ids are not shown whole).
fn display_key(scope: Scope, key: &str) -> String {
    match scope {
        Scope::PerSession => format!("session:{}", key.get(..8).unwrap_or(key)),
        _ => key.to_owned(),
    }
}

#[derive(Default)]
struct Registry {
    by_name: BTreeMap<String, Arc<Server>>,
    by_prefix: BTreeMap<String, Arc<Server>>,
}

#[derive(Default)]
struct Outbox {
    queue: VecDeque<Value>,
    dropped: u64,
    /// Id of the open notification stream.
    stream: Option<u64>,
    waker: Option<Waker>,
    /// The session ended; streams close and cannot reopen.
    closed: bool,
}

/// What a notification stream should do next.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamPoll {
    /// Deliver this message.
    Message(Value),
    /// Nothing queued; the waker is registered.
    Empty,
    /// The stream is over (session ended or replaced by a newer stream).
    Closed,
}

/// What the broker learnt from the agent's `roots/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RootsState {
    /// Not asked yet (or the agent said its roots changed).
    NotAsked,
    /// Asked; the answer (first `file://` root) or `None`.
    Known(Option<PathBuf>),
}

/// One agent session.
pub struct Session {
    id: String,
    protocol_version: String,
    ephemeral: bool,
    inflight: Mutex<HashMap<String, oneshot::Sender<()>>>,
    outbox: Mutex<Outbox>,
    next_stream: AtomicU64,
    last_used: Mutex<Instant>,
    /// Directory from `X-Mcpdoze-Cwd` / `?cwd=`.
    declared_cwd: Mutex<Option<PathBuf>>,
    /// The agent advertised the `roots` client capability.
    roots_capable: AtomicBool,
    roots: Mutex<RootsState>,
    roots_lock: tokio::sync::Mutex<()>,
    /// Server-to-client requests sent to this agent, by broker id key.
    client_pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// Logged that the directory is unknown (once per session).
    cwd_fallback_logged: AtomicBool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("protocol_version", &self.protocol_version)
            .field("ephemeral", &self.ephemeral)
            .finish_non_exhaustive()
    }
}

impl Session {
    fn new(protocol_version: &str, ephemeral: bool) -> Self {
        Self {
            id: random_id(),
            protocol_version: protocol_version.to_owned(),
            ephemeral,
            inflight: Mutex::new(HashMap::new()),
            outbox: Mutex::new(Outbox::default()),
            next_stream: AtomicU64::new(1),
            last_used: Mutex::new(Instant::now()),
            declared_cwd: Mutex::new(None),
            roots_capable: AtomicBool::new(false),
            roots: Mutex::new(RootsState::NotAsked),
            roots_lock: tokio::sync::Mutex::new(()),
            client_pending: Mutex::new(HashMap::new()),
            cwd_fallback_logged: AtomicBool::new(false),
        }
    }

    /// Records the working directory the client declared
    /// (`X-Mcpdoze-Cwd` header or `?cwd=` query). Relative paths are ignored.
    pub fn set_declared_cwd(&self, cwd: PathBuf) {
        if cwd.is_absolute() {
            *lock(&self.declared_cwd) = Some(cwd);
        } else {
            tracing::info!("relative client directory ignored");
        }
    }

    /// The directory declared by header or query, if any.
    pub fn declared_cwd(&self) -> Option<PathBuf> {
        lock(&self.declared_cwd).clone()
    }

    /// Records whether the agent advertised the `roots` capability.
    pub fn set_roots_capable(&self, on: bool) {
        self.roots_capable.store(on, Ordering::SeqCst);
    }

    /// True if the agent advertised the `roots` capability.
    pub fn roots_capable(&self) -> bool {
        self.roots_capable.load(Ordering::SeqCst)
    }

    /// The directory from the agent's roots, once asked and known.
    pub fn roots_cwd(&self) -> Option<PathBuf> {
        match &*lock(&self.roots) {
            RootsState::Known(p) => p.clone(),
            RootsState::NotAsked => None,
        }
    }

    /// True once the session ended (`DELETE`, eviction, idle timeout).
    pub fn is_ended(&self) -> bool {
        lock(&self.outbox).closed
    }

    /// Server-to-client requests awaiting this agent's answer.
    pub fn pending_client_requests(&self) -> usize {
        lock(&self.client_pending).len()
    }

    /// Sends a request to the agent over its event stream and waits for the
    /// answer (the whole JSON-RPC response) for at most `limit`.
    pub async fn client_request(
        &self,
        id: String,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, String> {
        let key = jsonrpc::id_key(&Value::String(id.clone()));
        let (tx, rx) = oneshot::channel();
        {
            let mut p = lock(&self.client_pending);
            if p.len() >= MAX_CLIENT_PENDING {
                return Err("too many requests pending at the agent".into());
            }
            p.insert(key.clone(), tx);
        }
        struct Remove<'a>(&'a Session, String);
        impl Drop for Remove<'_> {
            fn drop(&mut self) {
                lock(&self.0.client_pending).remove(&self.1);
            }
        }
        let _remove = Remove(self, key);
        if self.is_ended() {
            return Err("agent session ended".into());
        }
        self.push(jsonrpc::request(id, method, params));
        match tokio::time::timeout(limit, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err("agent session ended".into()),
            Err(_) => Err(format!(
                "agent did not answer {method} within {} s",
                limit.as_secs()
            )),
        }
    }

    /// Delivers an agent's response to a pending server-to-client request.
    /// False if no request has that id.
    fn answer(&self, msg: Value) -> bool {
        let key = jsonrpc::id_key(&msg["id"]);
        match lock(&self.client_pending).remove(&key) {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
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

    /// Drains the queued notifications (those an open stream has not
    /// delivered yet).
    pub fn take_notifications(&self) -> Vec<Value> {
        lock(&self.outbox).queue.drain(..).collect()
    }

    /// Notifications currently queued.
    pub fn queued_notifications(&self) -> usize {
        lock(&self.outbox).queue.len()
    }

    /// Notifications dropped because the queue was full.
    pub fn dropped_notifications(&self) -> u64 {
        lock(&self.outbox).dropped
    }

    /// True while a notification stream is open.
    pub fn has_stream(&self) -> bool {
        lock(&self.outbox).stream.is_some()
    }

    /// Opens the notification stream, replacing (closing) an older one.
    /// `None` once the session has ended.
    pub fn open_stream(&self) -> Option<u64> {
        let id = self.next_stream.fetch_add(1, Ordering::Relaxed);
        let waker = {
            let mut o = lock(&self.outbox);
            if o.closed {
                return None;
            }
            o.stream = Some(id);
            o.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
        Some(id)
    }

    /// Next step of stream `id`; registers `waker` when nothing is queued.
    pub fn poll_stream(&self, id: u64, waker: &Waker) -> StreamPoll {
        let mut o = lock(&self.outbox);
        if o.closed || o.stream != Some(id) {
            return StreamPoll::Closed;
        }
        match o.queue.pop_front() {
            Some(m) => StreamPoll::Message(m),
            None => {
                o.waker = Some(waker.clone());
                StreamPoll::Empty
            }
        }
    }

    /// Stream `id` went away (client disconnected). The idle timeout counts
    /// from here.
    pub fn close_stream(&self, id: u64) {
        let mut o = lock(&self.outbox);
        if o.stream == Some(id) {
            o.stream = None;
            o.waker = None;
            drop(o);
            self.touch();
        }
    }

    /// Ends the session's streams for good and fails requests pending at
    /// the agent.
    fn end(&self) {
        let waker = {
            let mut o = lock(&self.outbox);
            o.closed = true;
            o.stream = None;
            o.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
        lock(&self.client_pending).clear();
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

    /// Queues a notification for delivery (oldest dropped when full).
    pub fn push(&self, msg: Value) {
        let waker = {
            let mut o = lock(&self.outbox);
            if o.queue.len() >= SESSION_OUTBOX {
                o.queue.pop_front();
                o.dropped += 1;
            }
            o.queue.push_back(msg);
            o.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }

    fn touch(&self) {
        *lock(&self.last_used) = Instant::now();
    }
}

/// 128-bit session id from the OS CSPRNG, as 32 lowercase hex digits.
fn random_id() -> String {
    let mut buf = [0u8; 16];
    // The OS generator only fails on broken systems; refuse to hand out a
    // guessable id rather than fall back to a weak source.
    getrandom::fill(&mut buf).expect("OS random number generator unavailable");
    let mut out = String::with_capacity(32);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
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
    router: Weak<Router>,
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
            "notifications/tools/list_changed"
            | "notifications/prompts/list_changed"
            | "notifications/resources/list_changed" => {
                tracing::debug!(server, "backend list changed");
                if let Some(r) = self.router.upgrade() {
                    r.on_list_changed(server);
                }
            }
            method => tracing::debug!(server, method, "backend notification dropped"),
        }
    }

    fn notification_from(&self, server: &str, msg: Value, sessions: Vec<String>) {
        if msg["method"] == "notifications/message" {
            if let Some(r) = self.router.upgrade() {
                r.deliver_to(&sessions, msg);
            }
            return;
        }
        self.notification(server, msg);
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

/// Clears a server's deduplication flag when a background refresh ends.
struct BgFlag(Arc<Server>);

impl Drop for BgFlag {
    fn drop(&mut self) {
        self.0.bg_refresh.store(false, Ordering::SeqCst);
    }
}

enum ListError {
    Call(CallError),
    Rpc(Value),
}

/// One list request with a timeout (cancelled at the backend on timeout).
async fn call_bounded(conn: &Conn, method: &str, params: Value) -> Result<Value, CallError> {
    let (id, rx) = conn.begin(method, params, CallMeta::default()).await?;
    match tokio::time::timeout(LIST_TIMEOUT, rx).await {
        Ok(r) => r.unwrap_or(Err(CallError::Closed)),
        Err(_) => {
            conn.cancel(id, "timed out");
            Err(CallError::Unavailable(format!("{method} timed out")))
        }
    }
}

/// Every page of a list method (bounded in pages and items).
async fn list_paged(conn: &Conn, method: &str, key: &str) -> Result<Vec<Value>, ListError> {
    let mut out = Vec::new();
    let mut cursor: Option<Value> = None;
    for _ in 0..MAX_LIST_PAGES {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let resp = call_bounded(conn, method, params)
            .await
            .map_err(ListError::Call)?;
        let Some(result) = resp.get("result") else {
            return Err(ListError::Rpc(
                resp.get("error").cloned().unwrap_or(Value::Null),
            ));
        };
        if let Some(list) = result[key].as_array() {
            let room = MAX_LIST_ITEMS.saturating_sub(out.len());
            out.extend(list.iter().take(room).cloned());
        }
        cursor = result.get("nextCursor").filter(|c| !c.is_null()).cloned();
        if cursor.is_none() || out.len() >= MAX_LIST_ITEMS {
            break;
        }
    }
    Ok(out)
}

/// A list the backend may not implement: JSON-RPC errors mean empty.
async fn list_optional(conn: &Conn, method: &str, key: &str) -> Result<Vec<Value>, CallError> {
    match list_paged(conn, method, key).await {
        Ok(v) => Ok(v),
        Err(ListError::Rpc(_)) => Ok(Vec::new()),
        Err(ListError::Call(e)) => Err(e),
    }
}

/// Lists everything a backend offers, per its advertised capabilities.
async fn list_all(conn: &Conn) -> Result<CatalogEntry, CallError> {
    let init = conn.init_result();
    let caps = init
        .get("capabilities")
        .cloned()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let tools = match list_paged(conn, "tools/list", "tools").await {
        Ok(v) => v,
        Err(ListError::Rpc(_)) if caps.get("tools").is_none() => Vec::new(),
        Err(ListError::Rpc(e)) => {
            return Err(CallError::Unavailable(format!("tools/list error: {e}")));
        }
        Err(ListError::Call(e)) => return Err(e),
    };
    let prompts = if caps.get("prompts").is_some() {
        list_optional(conn, "prompts/list", "prompts").await?
    } else {
        Vec::new()
    };
    let (resources, resource_templates) = if caps.get("resources").is_some() {
        (
            list_optional(conn, "resources/list", "resources").await?,
            list_optional(conn, "resources/templates/list", "resourceTemplates").await?,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    Ok(CatalogEntry {
        version: CATALOG_VERSION,
        server_info: init.get("serverInfo").cloned().unwrap_or(Value::Null),
        capabilities: caps,
        tools,
        prompts,
        resources,
        resource_templates,
        ..CatalogEntry::default()
    })
}

fn expired(entry: &CatalogEntry, server: &Server) -> bool {
    match server.options.max_age {
        Some(age) => catalog::now_secs().saturating_sub(entry.updated_at) >= age.as_secs().max(1),
        None => false,
    }
}

/// The broker core shared by every transport.
pub struct Router {
    me: Weak<Router>,
    registry: RwLock<Registry>,
    catalog: Arc<dyn Catalog>,
    progress: Arc<ProgressTable>,
    events: Arc<RouterEvents>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    tuning: Tuning,
    probes: Mutex<HashMap<u64, Arc<Instance>>>,
    next_probe: AtomicU64,
    probe_count: AtomicU64,
    refresh_count: AtomicU64,
    closing: AtomicBool,
    sse_keepalive: Mutex<Duration>,
    client_timeout: Mutex<Duration>,
    session_idle: Mutex<Duration>,
    reaper: AtomicBool,
    next_client_id: AtomicU64,
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
        Arc::new_cyclic(|me: &Weak<Router>| Self {
            me: me.clone(),
            registry: RwLock::new(Registry::default()),
            catalog,
            events: Arc::new(RouterEvents {
                router: me.clone(),
                progress: progress.clone(),
            }),
            progress,
            sessions: Mutex::new(HashMap::new()),
            tuning,
            probes: Mutex::new(HashMap::new()),
            next_probe: AtomicU64::new(1),
            probe_count: AtomicU64::new(0),
            refresh_count: AtomicU64::new(0),
            closing: AtomicBool::new(false),
            sse_keepalive: Mutex::new(SSE_KEEPALIVE),
            client_timeout: Mutex::new(DEFAULT_CLIENT_REQUEST_TIMEOUT),
            session_idle: Mutex::new(DEFAULT_SESSION_IDLE_TIMEOUT),
            reaper: AtomicBool::new(false),
            next_client_id: AtomicU64::new(1),
        })
    }

    /// Registers a backend without probing. Nothing is spawned; the first
    /// list starts it unless the catalog already has an entry.
    pub fn register(
        &self,
        name: &str,
        prefix: &str,
        spec: BackendSpec,
    ) -> Result<(), RegisterError> {
        self.register_with(name, prefix, spec, CatalogOptions::default())
    }

    /// Registers a backend with catalog options. A catalog entry with
    /// another fingerprint is discarded; with `probe`, a missing or
    /// discarded entry is filled by a background probe.
    pub fn register_with(
        &self,
        name: &str,
        prefix: &str,
        spec: BackendSpec,
        options: CatalogOptions,
    ) -> Result<(), RegisterError> {
        self.register_scoped(name, prefix, spec, options, ScopeOptions::default())
    }

    /// Registers a backend with catalog and scope options.
    pub fn register_scoped(
        &self,
        name: &str,
        prefix: &str,
        spec: BackendSpec,
        options: CatalogOptions,
        scope: ScopeOptions,
    ) -> Result<(), RegisterError> {
        if !naming::valid_prefix(prefix) {
            return Err(RegisterError::InvalidPrefix(prefix.to_owned()));
        }
        let server = {
            let mut reg = self.registry.write().unwrap_or_else(|e| e.into_inner());
            if reg.by_name.contains_key(name) {
                return Err(RegisterError::DuplicateName(name.to_owned()));
            }
            if reg.by_prefix.contains_key(prefix) {
                return Err(RegisterError::DuplicatePrefix(prefix.to_owned()));
            }
            let shared_spec = match scope.scope {
                Scope::Shared => spec.clone(),
                // The fallback instance of a scoped server: `${cwd}` is the
                // broker's own directory.
                _ => spec.expand_cwd(&broker_cwd(), false),
            };
            let server = Arc::new(Server {
                name: name.to_owned(),
                prefix: prefix.to_owned(),
                fingerprint: fingerprint(&spec),
                instance: Instance::with_tuning(
                    name,
                    shared_spec,
                    self.events.clone(),
                    self.tuning,
                ),
                scope: ScopeOptions {
                    scope: scope.scope,
                    max_instances: scope.max_instances.max(1),
                },
                base_spec: spec,
                scoped: Mutex::new(BTreeMap::new()),
                options,
                refresh_lock: tokio::sync::Mutex::new(()),
                listed_gen: AtomicU64::new(0),
                bg_refresh: AtomicBool::new(false),
            });
            reg.by_name.insert(name.to_owned(), server.clone());
            reg.by_prefix.insert(prefix.to_owned(), server.clone());
            server
        };
        self.check_catalog(&server);
        Ok(())
    }

    fn check_catalog(&self, server: &Arc<Server>) {
        match self.catalog.entry(&server.name) {
            Some(e) if !e.fingerprint.is_empty() && e.fingerprint != server.fingerprint => {
                tracing::info!(server = %server.name, "launch spec changed; catalog discarded");
                self.catalog.remove(&server.name);
                if server.options.probe {
                    self.spawn_refresh(server, None, true, "fingerprint changed");
                }
            }
            Some(e) => {
                if expired(&e, server) && server.options.probe {
                    self.spawn_dedup_refresh(server, None, true, "catalog expired");
                }
            }
            None => {
                if server.options.probe {
                    self.spawn_refresh(server, None, true, "no catalog");
                }
            }
        }
    }

    /// Removes server `name` from routing and returns it (its instance keeps
    /// running until the caller stops it). Its catalog entry is kept; see
    /// [`forget_catalog`](Router::forget_catalog).
    pub fn unregister(&self, name: &str) -> Option<Arc<Server>> {
        let mut reg = self.registry.write().unwrap_or_else(|e| e.into_inner());
        let server = reg.by_name.remove(name)?;
        reg.by_prefix.remove(&server.prefix);
        Some(server)
    }

    /// Drops the catalog entry of `name` (server removed for good).
    pub fn forget_catalog(&self, name: &str) {
        self.catalog.remove(name);
    }

    /// Every registered server, ordered by name.
    pub fn servers(&self) -> Vec<Arc<Server>> {
        let reg = self.registry.read().unwrap_or_else(|e| e.into_inner());
        reg.by_name.values().cloned().collect()
    }

    /// The registered server `name`.
    pub fn server(&self, name: &str) -> Option<Arc<Server>> {
        let reg = self.registry.read().unwrap_or_else(|e| e.into_inner());
        reg.by_name.get(name).cloned()
    }

    fn by_prefix(&self, prefix: &str) -> Option<Arc<Server>> {
        let reg = self.registry.read().unwrap_or_else(|e| e.into_inner());
        reg.by_prefix.get(prefix).cloned()
    }

    fn is_current(&self, server: &Arc<Server>) -> bool {
        self.server(&server.name)
            .is_some_and(|s| Arc::ptr_eq(&s, server))
    }

    /// The instance of server `name`.
    pub fn instance(&self, name: &str) -> Option<Arc<Instance>> {
        self.server(name).map(|s| s.instance.clone())
    }

    /// The catalog.
    pub fn catalog(&self) -> &Arc<dyn Catalog> {
        &self.catalog
    }

    /// Probes started so far (throwaway instances spawned to list).
    pub fn probe_count(&self) -> u64 {
        self.probe_count.load(Ordering::Relaxed)
    }

    /// Catalog refreshes completed so far (probes and re-lists).
    pub fn refresh_count(&self) -> u64 {
        self.refresh_count.load(Ordering::Relaxed)
    }

    /// Keep-alive interval of notification streams.
    pub fn sse_keepalive(&self) -> Duration {
        *lock(&self.sse_keepalive)
    }

    /// Sets the keep-alive interval of streams opened from now on.
    pub fn set_sse_keepalive(&self, d: Duration) {
        *lock(&self.sse_keepalive) = d.max(Duration::from_millis(10));
    }

    /// How long a forwarded server-to-client request waits for the agent.
    pub fn client_request_timeout(&self) -> Duration {
        *lock(&self.client_timeout)
    }

    /// Sets the server-to-client request timeout.
    pub fn set_client_request_timeout(&self, d: Duration) {
        *lock(&self.client_timeout) = d.max(Duration::from_millis(10));
    }

    /// Idle time (no request, no open stream) after which a session ends.
    pub fn session_idle_timeout(&self) -> Duration {
        *lock(&self.session_idle)
    }

    /// Sets the session idle timeout.
    pub fn set_session_idle_timeout(&self, d: Duration) {
        *lock(&self.session_idle) = d.max(Duration::from_millis(50));
    }

    /// True once [`close_streams`](Router::close_streams) ran (shutdown).
    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    /// Ends every notification stream (broker shutdown), so open streams do
    /// not hold up a graceful HTTP shutdown.
    pub fn close_streams(&self) {
        self.closing.store(true, Ordering::SeqCst);
        let all: Vec<_> = lock(&self.sessions).values().cloned().collect();
        for s in all {
            s.end();
        }
    }

    /// Negotiates a protocol version and builds the `initialize` result.
    pub fn initialize_result(&self, requested: Option<&str>) -> (String, Value) {
        let version = requested
            .filter(|v| is_supported_version(v))
            .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0])
            .to_owned();
        let result = json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": {"listChanged": true},
                "prompts": {"listChanged": true},
                "resources": {"listChanged": true, "subscribe": false},
            },
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
            old.end();
            old.cancel_all();
            self.drop_session_instances(&old.id);
        }
        self.start_reaper();
        session
    }

    /// Starts the idle-session reaper once (needs a runtime).
    fn start_reaper(&self) {
        if self.reaper.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.reaper.store(false, Ordering::SeqCst);
            return;
        };
        let me = self.me.clone();
        rt.spawn(async move {
            loop {
                let tick = match me.upgrade() {
                    Some(r) => (r.session_idle_timeout() / 4)
                        .clamp(Duration::from_millis(25), Duration::from_secs(30)),
                    None => return,
                };
                tokio::time::sleep(tick).await;
                let Some(r) = me.upgrade() else { return };
                r.reap_idle_sessions();
            }
        });
    }

    /// Ends sessions with no open stream, nothing in flight and no request
    /// for longer than the idle timeout.
    fn reap_idle_sessions(&self) {
        let idle = self.session_idle_timeout();
        let expired: Vec<String> = lock(&self.sessions)
            .values()
            .filter(|s| {
                !s.has_stream()
                    && s.in_flight() == 0
                    && s.pending_client_requests() == 0
                    && lock(&s.last_used).elapsed() >= idle
            })
            .map(|s| s.id.clone())
            .collect();
        for id in expired {
            tracing::info!("agent session idle; ended");
            self.end_session(&id);
        }
    }

    /// Unloads the per-session instances of session `id`.
    fn drop_session_instances(&self, id: &str) {
        let mut gone = Vec::new();
        for server in self.servers() {
            if server.scope.scope != Scope::PerSession {
                continue;
            }
            if let Some(inst) = lock(&server.scoped).remove(id) {
                gone.push(inst);
            }
        }
        if gone.is_empty() {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        rt.spawn(async move {
            for inst in gone {
                inst.shutdown().await;
            }
        });
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

    /// Ends a session, closes its stream and cancels its in-flight calls.
    /// False if unknown.
    pub fn end_session(&self, id: &str) -> bool {
        let session = lock(&self.sessions).remove(id);
        match session {
            Some(s) => {
                s.end();
                s.cancel_all();
                self.drop_session_instances(id);
                true
            }
            None => false,
        }
    }

    /// Queues `msg` for every stored session with an open stream.
    pub fn broadcast(&self, msg: &Value) {
        let all: Vec<_> = lock(&self.sessions)
            .values()
            .filter(|s| s.has_stream())
            .cloned()
            .collect();
        for s in all {
            s.push(msg.clone());
        }
    }

    fn deliver_to(&self, sessions: &[String], msg: Value) {
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
    fn route_server_request(&self, req: ServerRequest) {
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

    /// Stops every backend and probe and closes every stream (broker
    /// shutdown).
    pub async fn shutdown(&self) {
        self.close_streams();
        let probes: Vec<_> = lock(&self.probes).values().cloned().collect();
        for p in probes {
            p.shutdown().await;
        }
        for s in self.servers() {
            for inst in s.all_instances() {
                inst.shutdown().await;
            }
        }
    }

    // ------------------------------------------------------------ catalog

    /// Re-lists server `name` now: on its running process when it has one,
    /// else by probing. `None` if unknown.
    pub async fn refresh(&self, name: &str) -> Option<Result<RefreshOutcome, String>> {
        let server = self.server(name)?;
        Some(self.refresh_server(&server, None, true, "requested").await)
    }

    fn spawn_refresh(
        &self,
        server: &Arc<Server>,
        conn: Option<Arc<Conn>>,
        allow_probe: bool,
        why: &'static str,
    ) {
        let (Some(me), Ok(rt)) = (self.me.upgrade(), tokio::runtime::Handle::try_current()) else {
            return;
        };
        let server = server.clone();
        rt.spawn(async move {
            let _ = me.refresh_server(&server, conn, allow_probe, why).await;
        });
    }

    /// Like `spawn_refresh`, but at most one queued per server.
    fn spawn_dedup_refresh(
        &self,
        server: &Arc<Server>,
        conn: Option<Arc<Conn>>,
        allow_probe: bool,
        why: &'static str,
    ) {
        if server.bg_refresh.swap(true, Ordering::SeqCst) {
            return;
        }
        let flag = BgFlag(server.clone());
        let (Some(me), Ok(rt)) = (self.me.upgrade(), tokio::runtime::Handle::try_current()) else {
            return;
        };
        let server = server.clone();
        rt.spawn(async move {
            let _flag = flag;
            let _ = me.refresh_server(&server, conn, allow_probe, why).await;
        });
    }

    async fn refresh_server(
        &self,
        server: &Arc<Server>,
        conn: Option<Arc<Conn>>,
        allow_probe: bool,
        why: &'static str,
    ) -> Result<RefreshOutcome, String> {
        let _g = server.refresh_lock.lock().await;
        if !self.is_current(server) {
            return Err("server is no longer registered".into());
        }
        let conn = conn.or_else(|| server.any_running_conn());
        let listed = match conn {
            Some(c) => {
                server.listed_gen.store(c.generation(), Ordering::SeqCst);
                list_all(&c).await.map_err(|e| e.to_string())
            }
            None if allow_probe => self.probe_list(server).await,
            None => return Err("backend not running".into()),
        };
        match listed {
            Ok(entry) => {
                let (outcome, _) = self.apply_entry(server, entry);
                tracing::debug!(server = %server.name, why, changed = outcome.changed, "catalog refreshed");
                Ok(outcome)
            }
            Err(e) => {
                tracing::warn!(server = %server.name, why, error = %e, "catalog refresh failed");
                Err(e)
            }
        }
    }

    /// Starts a throwaway instance of `server`, lists it and stops it.
    async fn probe_list(&self, server: &Server) -> Result<CatalogEntry, String> {
        if self.is_closing() {
            return Err("broker is shutting down".into());
        }
        let inst = Instance::with_tuning(
            &server.name,
            server.instance.spec().clone(),
            Arc::new(NoEvents),
            self.tuning,
        );
        let id = self.next_probe.fetch_add(1, Ordering::Relaxed);
        lock(&self.probes).insert(id, inst.clone());
        self.probe_count.fetch_add(1, Ordering::Relaxed);
        tracing::info!(server = %server.name, "probing backend for its catalog");
        let res = async {
            let guard = inst.acquire().await.map_err(|e| e.to_string())?;
            let r = list_all(guard.conn()).await.map_err(|e| e.to_string());
            drop(guard);
            r
        }
        .await;
        inst.shutdown().await;
        lock(&self.probes).remove(&id);
        res
    }

    /// Stores `new` as the entry of `server` when it differs and tells
    /// streaming sessions about changed lists.
    fn apply_entry(
        &self,
        server: &Arc<Server>,
        mut new: CatalogEntry,
    ) -> (RefreshOutcome, Arc<CatalogEntry>) {
        new.version = CATALOG_VERSION;
        new.fingerprint = server.fingerprint.clone();
        new.updated_at = catalog::now_secs();
        let old = self.catalog.entry(&server.name);
        let changes = match &old {
            Some(o) => o.diff(&new),
            None => CatalogEntry::default().diff(&new),
        };
        let meta_changed = old.as_deref().is_none_or(|o| {
            o.fingerprint != new.fingerprint
                || o.capabilities != new.capabilities
                || o.server_info != new.server_info
        });
        let outcome = RefreshOutcome {
            changed: changes.any(),
            tools: new.tools.len(),
            prompts: new.prompts.len(),
            resources: new.resources.len(),
            resource_templates: new.resource_templates.len(),
        };
        self.refresh_count.fetch_add(1, Ordering::Relaxed);
        let new = Arc::new(new);
        if !self.is_current(server) {
            return (outcome, new);
        }
        if changes.any() || meta_changed || server.options.max_age.is_some() {
            self.catalog.put(&server.name, (*new).clone());
        }
        if changes.any() {
            tracing::info!(
                server = %server.name,
                tools = changes.tools,
                prompts = changes.prompts,
                resources = changes.resources,
                "catalog changed"
            );
            for (on, method) in [
                (changes.tools, "notifications/tools/list_changed"),
                (changes.prompts, "notifications/prompts/list_changed"),
                (changes.resources, "notifications/resources/list_changed"),
            ] {
                if on {
                    self.broadcast(&json!({"jsonrpc": "2.0", "method": method}));
                }
            }
        }
        (outcome, new)
    }

    fn on_list_changed(&self, server: &str) {
        let Some(server) = self.server(server) else {
            return;
        };
        if let Some(conn) = server.any_running_conn() {
            self.spawn_refresh(&server, Some(conn), false, "list_changed");
        }
    }

    /// Called with every connection a forwarded call uses: a new process of
    /// a catalogued server is re-listed in the background.
    fn note_conn(&self, server: &Arc<Server>, conn: &Arc<Conn>) {
        let generation = conn.generation();
        if server.listed_gen.load(Ordering::SeqCst) == generation
            || self.catalog.entry(&server.name).is_none()
        {
            return;
        }
        if server.listed_gen.swap(generation, Ordering::SeqCst) != generation {
            self.spawn_refresh(server, Some(conn.clone()), false, "spawn");
        }
    }

    /// Lists a server without an entry from its real instance (starting it
    /// if needed) and stores the entry.
    async fn list_uncatalogued(&self, server: &Arc<Server>) -> Result<Arc<CatalogEntry>, String> {
        let _g = server.refresh_lock.lock().await;
        if let Some(e) = self.catalog.entry(&server.name) {
            return Ok(e);
        }
        let guard = server.instance.acquire().await.map_err(|e| e.to_string())?;
        server
            .listed_gen
            .store(guard.conn().generation(), Ordering::SeqCst);
        let entry = list_all(guard.conn()).await.map_err(|e| e.to_string())?;
        drop(guard);
        Ok(self.apply_entry(server, entry).1)
    }

    /// Catalog entries of every server; servers without one are listed now,
    /// failures are logged and skipped.
    pub async fn catalog_entries(&self) -> Vec<(Arc<Server>, Arc<CatalogEntry>)> {
        let mut out = Vec::new();
        for server in self.servers() {
            let entry = match self.catalog.entry(&server.name) {
                Some(e) => {
                    if expired(&e, &server) {
                        self.spawn_dedup_refresh(
                            &server,
                            None,
                            server.options.probe,
                            "catalog expired",
                        );
                    }
                    e
                }
                None => match self.list_uncatalogued(&server).await {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(server = %server.name, error = %e, "list failed");
                        continue;
                    }
                },
            };
            out.push((server, entry));
        }
        out
    }

    /// Aggregated, prefixed tool list.
    pub async fn list_tools(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (server, entry) in self.catalog_entries().await {
            for tool in &entry.tools {
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

    /// Aggregated, prefixed prompt list.
    pub async fn list_prompts(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (server, entry) in self.catalog_entries().await {
            for p in &entry.prompts {
                let Some(name) = p["name"].as_str() else {
                    continue;
                };
                let mut p = p.clone();
                p["name"] = Value::String(naming::join(&server.prefix, name));
                out.push(p);
            }
        }
        out
    }

    /// Aggregated resource list with mapped URIs.
    pub async fn list_resources(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (server, entry) in self.catalog_entries().await {
            for r in &entry.resources {
                let Some(u) = r["uri"].as_str() else {
                    continue;
                };
                let mut r = r.clone();
                r["uri"] = Value::String(uri::map_uri(&server.prefix, u));
                out.push(r);
            }
        }
        out
    }

    /// Aggregated resource template list with mapped URI templates.
    pub async fn list_resource_templates(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (server, entry) in self.catalog_entries().await {
            for t in &entry.resource_templates {
                let Some(u) = t["uriTemplate"].as_str() else {
                    continue;
                };
                let mut t = t.clone();
                t["uriTemplate"] = Value::String(uri::map_template(&server.prefix, u));
                out.push(t);
            }
        }
        out
    }

    // ----------------------------------------------------------- requests

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

    fn notification(&self, session: &Session, msg: &Value) {
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

    async fn request(&self, session: &Arc<Session>, msg: Value) -> Value {
        let id = msg["id"].clone();
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        match msg["method"].as_str().unwrap_or("") {
            "initialize" => {
                session.set_roots_capable(params["capabilities"].get("roots").is_some());
                let requested = params["protocolVersion"].as_str();
                jsonrpc::result(id, self.initialize_result(requested).1)
            }
            "ping" => jsonrpc::result(id, json!({})),
            "tools/list" => jsonrpc::result(id, json!({"tools": self.list_tools().await})),
            "tools/call" => self.call_tool(session, id, params).await,
            "prompts/list" => jsonrpc::result(id, json!({"prompts": self.list_prompts().await})),
            "prompts/get" => self.get_prompt(session, id, params).await,
            "resources/list" => {
                jsonrpc::result(id, json!({"resources": self.list_resources().await}))
            }
            "resources/templates/list" => jsonrpc::result(
                id,
                json!({"resourceTemplates": self.list_resource_templates().await}),
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

    fn named(&self, name: Option<&str>) -> Option<(Arc<Server>, String)> {
        naming::split(name?).and_then(|(p, t)| self.by_prefix(p).map(|s| (s, t.to_owned())))
    }

    async fn call_tool(&self, session: &Arc<Session>, id: Value, mut params: Value) -> Value {
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
        if let Some(entry) = self.catalog.entry(&server.name) {
            let rejected = matches!(
                resp["error"]["code"].as_i64(),
                Some(jsonrpc::INVALID_PARAMS | jsonrpc::METHOD_NOT_FOUND)
            );
            if rejected || !entry.has_tool(&tool) {
                if let Some(conn) = server.any_running_conn() {
                    self.spawn_dedup_refresh(&server, Some(conn), false, "stale catalog");
                }
            }
        }
        resp
    }

    async fn get_prompt(&self, session: &Arc<Session>, id: Value, mut params: Value) -> Value {
        let Some((server, prompt)) = self.named(params["name"].as_str()) else {
            return jsonrpc::error(id, jsonrpc::INVALID_PARAMS, "unknown prompt");
        };
        params["name"] = Value::String(prompt);
        self.forward_routed(session, &server, id, "prompts/get", params)
            .await
    }

    async fn read_resource(&self, session: &Arc<Session>, id: Value, mut params: Value) -> Value {
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
    async fn forward_routed(
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
    async fn forward(
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
        drop(inst);
        self.note_conn(server, guard.conn());
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

// ------------------------------------------------------------- scopes

/// The broker's own working directory (`${cwd}` of shared fallbacks).
fn broker_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Normalised map key of a directory: trailing separators removed; on
/// Windows `/` becomes `\` and case is folded.
pub fn cwd_key(path: &Path) -> String {
    let mut s = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        s = s.replace('/', "\\").to_lowercase();
        while s.len() > 3 && s.ends_with('\\') {
            s.pop();
        }
    } else {
        while s.len() > 1 && s.ends_with('/') {
            s.pop();
        }
    }
    s
}

/// `file://` URI of an absolute directory.
pub fn file_uri(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !raw.starts_with('/') {
        out.push('/');
    }
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` escapes (invalid escapes are kept); `plus` also maps `+`
/// to a space (query strings).
pub fn percent_decode(s: &str, plus: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' if plus => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The local path of a `file://` URI (`localhost` or empty host only).
pub fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri
        .strip_prefix("file://")
        .or_else(|| uri.strip_prefix("FILE://"))?;
    let i = rest.find('/')?;
    let (host, path) = (&rest[..i], &rest[i..]);
    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let mut p = percent_decode(path, false);
    // `/C:/x` -> `C:/x`
    let b = p.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        p.remove(0);
    }
    let p = PathBuf::from(p);
    p.is_absolute().then_some(p)
}

/// A usable agent directory: absolute and an existing directory.
fn usable_dir(p: PathBuf) -> Option<PathBuf> {
    if p.is_absolute() && p.is_dir() {
        Some(p)
    } else {
        tracing::info!("agent directory is not an existing absolute directory; ignored");
        None
    }
}

impl Router {
    /// Applies a client-declared directory (`X-Mcpdoze-Cwd` header value or
    /// raw `?cwd=` query value, percent-decoded here when `query`).
    pub fn note_client_cwd(&self, session: &Session, raw: &str, query: bool) {
        let v = if query {
            percent_decode(raw, true)
        } else {
            raw.to_owned()
        };
        let v = v.trim();
        if v.is_empty() {
            return;
        }
        let p = match v.strip_prefix("file:") {
            Some(_) => path_from_file_uri(v),
            None => Some(PathBuf::from(v)),
        };
        if let Some(p) = p {
            session.set_declared_cwd(p);
        }
    }

    /// The agent's working directory: its first `file://` root (asked once
    /// over its event stream when it advertised `roots`), else the declared
    /// header / query directory.
    pub async fn session_cwd(&self, session: &Session) -> Option<PathBuf> {
        let ask = session.roots_capable()
            && session.has_stream()
            && *lock(&session.roots) == RootsState::NotAsked;
        if ask {
            let _g = session.roots_lock.lock().await;
            if *lock(&session.roots) == RootsState::NotAsked && session.has_stream() {
                let n = self.next_client_id.fetch_add(1, Ordering::Relaxed);
                let id = format!("mcpdoze-s2c-{n}");
                let answer = session
                    .client_request(id, "roots/list", json!({}), ROOTS_TIMEOUT)
                    .await;
                let root = match answer {
                    Ok(resp) => resp["result"]["roots"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|r| r["uri"].as_str())
                        .find_map(path_from_file_uri)
                        .and_then(usable_dir),
                    Err(e) => {
                        tracing::info!(error = %e, "roots/list to agent failed");
                        None
                    }
                };
                *lock(&session.roots) = RootsState::Known(root);
            }
        }
        if let Some(p) = session.roots_cwd() {
            return Some(p);
        }
        session.declared_cwd().and_then(usable_dir)
    }

    /// The instance that serves `session`'s calls to `server`. Errors are
    /// `(JSON-RPC code, message)`.
    async fn instance_for(
        &self,
        session: &Arc<Session>,
        server: &Arc<Server>,
    ) -> Result<Arc<Instance>, (i64, String)> {
        match server.scope.scope {
            Scope::Shared => Ok(server.instance.clone()),
            Scope::PerSession => {
                if session.is_ephemeral() {
                    if !session.cwd_fallback_logged.swap(true, Ordering::Relaxed) {
                        tracing::info!(server = %server.name, "stateless client; per-session server served by its shared instance");
                    }
                    return Ok(server.instance.clone());
                }
                if session.is_ended() {
                    return Err((jsonrpc::BACKEND_UNAVAILABLE, "agent session ended".into()));
                }
                let cwd = self.session_cwd(session).await;
                let key = session.id.clone();
                self.scoped_instance(server, key, || {
                    let mut spec = match &cwd {
                        Some(c) => server.base_spec.expand_cwd(c, true),
                        None => server.base_spec.expand_cwd(&broker_cwd(), false),
                    };
                    spec.client = ClientSide {
                        sampling: true,
                        elicitation: true,
                        root: cwd.as_deref().map(file_uri),
                        owner: Some(session.id.clone()),
                    };
                    spec
                })
            }
            Scope::PerCwd => {
                let Some(cwd) = self.session_cwd(session).await else {
                    if !session.cwd_fallback_logged.swap(true, Ordering::Relaxed) {
                        tracing::info!(server = %server.name, "agent directory unknown; per-cwd server served by its shared instance");
                    }
                    return Ok(server.instance.clone());
                };
                self.scoped_instance(server, cwd_key(&cwd), || {
                    let mut spec = server.base_spec.expand_cwd(&cwd, true);
                    spec.client = ClientSide {
                        root: Some(file_uri(&cwd)),
                        ..ClientSide::default()
                    };
                    spec
                })
            }
        }
    }

    /// The scoped instance `key` of `server`, created with `make` when new,
    /// within the server's `max_instances`.
    fn scoped_instance(
        &self,
        server: &Arc<Server>,
        key: String,
        make: impl FnOnce() -> BackendSpec,
    ) -> Result<Arc<Instance>, (i64, String)> {
        let mut map = lock(&server.scoped);
        if let Some(i) = map.get(&key) {
            return Ok(i.clone());
        }
        let max = server.scope.max_instances;
        if map.len() >= max {
            Server::prune(&mut map, false);
        }
        if map.len() >= max {
            Server::prune(&mut map, true);
        }
        if map.len() >= max {
            tracing::warn!(server = %server.name, max, "instance limit reached");
            return Err((
                jsonrpc::INSTANCE_LIMIT,
                format!(
                    "server {:?} already runs its maximum of {max} {} instances \
                     (max_instances); try again when one is unloaded",
                    server.name,
                    scope_name(server.scope.scope)
                ),
            ));
        }
        let inst = Instance::with_tuning(&server.name, make(), self.events.clone(), self.tuning);
        tracing::info!(server = %server.name, scope = scope_name(server.scope.scope), "new scoped instance");
        map.insert(key, inst.clone());
        Ok(inst)
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
        let caps = r.initialize_result(None).1["capabilities"].clone();
        assert_eq!(caps["tools"]["listChanged"], true);
        assert_eq!(caps["resources"]["subscribe"], false);
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
    fn cwd_helpers() {
        assert_eq!(percent_decode("a%20b+c%zz%4", true), "a b c%zz%4");
        assert_eq!(percent_decode("a+b", false), "a+b");
        assert!(path_from_file_uri("file://example.com/x").is_none());
        assert!(path_from_file_uri("http://localhost/x").is_none());
        if cfg!(windows) {
            assert_eq!(
                path_from_file_uri("file:///C:/Example%20Dir").unwrap(),
                PathBuf::from("C:/Example Dir")
            );
            assert_eq!(
                cwd_key(Path::new("C:/Example/Dir/")),
                cwd_key(Path::new(r"c:\example\dir"))
            );
            assert_eq!(cwd_key(Path::new("C:\\")), "c:\\");
        } else {
            assert_eq!(
                path_from_file_uri("file://localhost/srv/example%20dir").unwrap(),
                PathBuf::from("/srv/example dir")
            );
            assert_eq!(cwd_key(Path::new("/srv/x/")), "/srv/x");
            assert_eq!(cwd_key(Path::new("/")), "/");
            assert_eq!(file_uri(Path::new("/srv/a b")), "file:///srv/a%20b");
        }
    }

    #[test]
    fn expand_cwd_skips_from_env_values() {
        let mut spec = BackendSpec::new("prog", ["--dir", "${cwd}/x"]);
        spec.env.insert("LIT".into(), "${cwd}".into());
        spec.env.insert("REF".into(), "${cwd}".into());
        spec.ref_env.insert("REF".into());
        let dir = std::env::temp_dir();
        let e = spec.expand_cwd(&dir, true);
        let d = dir.to_string_lossy();
        assert_eq!(e.args[1], format!("{d}/x"));
        assert_eq!(e.env["LIT"], d);
        assert_eq!(e.env["REF"], "${cwd}");
        assert_eq!(e.cwd.as_deref(), Some(dir.as_path()));
        assert_eq!(
            fingerprint(&spec),
            fingerprint(&BackendSpec {
                client: ClientSide {
                    sampling: true,
                    ..ClientSide::default()
                },
                ..spec.clone()
            })
        );
    }

    #[test]
    fn session_ids_are_unique() {
        let a = random_id();
        assert_eq!(a.len(), 32);
        assert_ne!(a, random_id());
    }

    #[test]
    fn session_queue_is_bounded_and_counts_drops() {
        let s = Session::new("2025-11-25", false);
        for i in 0..(SESSION_OUTBOX + 100) {
            s.push(json!({"n": i}));
        }
        assert_eq!(s.queued_notifications(), SESSION_OUTBOX);
        assert_eq!(s.dropped_notifications(), 100);
        let q = s.take_notifications();
        assert_eq!(q[0]["n"], 100);
        assert_eq!(q.last().unwrap()["n"], SESSION_OUTBOX + 99);
    }

    #[test]
    fn streams_replace_and_close() {
        let s = Session::new("2025-11-25", false);
        let w = std::task::Waker::noop();
        let a = s.open_stream().unwrap();
        assert_eq!(s.poll_stream(a, w), StreamPoll::Empty);
        s.push(json!({"m": 1}));
        assert_eq!(s.poll_stream(a, w), StreamPoll::Message(json!({"m": 1})));
        let b = s.open_stream().unwrap();
        assert_eq!(s.poll_stream(a, w), StreamPoll::Closed);
        assert_eq!(s.poll_stream(b, w), StreamPoll::Empty);
        s.close_stream(a); // stale id: no effect
        assert!(s.has_stream());
        s.end();
        assert_eq!(s.poll_stream(b, w), StreamPoll::Closed);
        assert!(s.open_stream().is_none());
    }

    #[test]
    fn fingerprint_mismatch_discards_entry() {
        let c = Arc::new(MemoryCatalog::default());
        let r = Router::with(c.clone(), Tuning::default());
        let spec = BackendSpec::new("prog", ["a"]);
        c.put(
            "x",
            CatalogEntry {
                fingerprint: "v1-other".into(),
                ..CatalogEntry::default()
            },
        );
        c.put(
            "y",
            CatalogEntry {
                fingerprint: fingerprint(&spec),
                ..CatalogEntry::default()
            },
        );
        r.register("x", "x", spec.clone()).unwrap();
        r.register("y", "y", spec).unwrap();
        assert!(c.entry("x").is_none());
        assert!(c.entry("y").is_some());
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
        let resp = r
            .handle(
                &s,
                jsonrpc::request(3, "resources/subscribe", json!({"uri": "x"})),
            )
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], jsonrpc::METHOD_NOT_FOUND);
        let resp = r
            .handle(
                &s,
                jsonrpc::request(4, "resources/read", json!({"uri": "file:///x"})),
            )
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], jsonrpc::INVALID_PARAMS);
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
