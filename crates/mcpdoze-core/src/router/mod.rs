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
//! backend; list changes go to sessions with an open stream. The HTTP layer
//! drains the queue into the session's `GET /mcp` event stream.

pub mod catalog;
pub mod naming;
pub mod uri;

use std::collections::{BTreeMap, HashMap, VecDeque};
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
    BackendEvents, BackendSpec, CallError, CallMeta, Conn, Instance, NoEvents, Tuning,
};
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

/// A registered backend server.
pub struct Server {
    /// Unique server name.
    pub name: String,
    /// Unique tool prefix.
    pub prefix: String,
    /// The single shared instance.
    pub instance: Arc<Instance>,
    fingerprint: String,
    options: CatalogOptions,
    /// Serialises listing and catalog updates of this server.
    refresh_lock: tokio::sync::Mutex<()>,
    /// Instance generation whose lists are known (0: none).
    listed_gen: AtomicU64,
    /// A deduplicated background refresh is queued or running.
    bg_refresh: AtomicBool,
    /// Sessions with calls in flight here: id -> (session, count).
    active: Mutex<HashMap<String, (Weak<Session>, usize)>>,
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

/// One agent session.
pub struct Session {
    id: String,
    protocol_version: String,
    ephemeral: bool,
    inflight: Mutex<HashMap<String, oneshot::Sender<()>>>,
    outbox: Mutex<Outbox>,
    next_stream: AtomicU64,
    last_used: Mutex<Instant>,
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

    /// Stream `id` went away (client disconnected).
    pub fn close_stream(&self, id: u64) {
        let mut o = lock(&self.outbox);
        if o.stream == Some(id) {
            o.stream = None;
            o.waker = None;
        }
    }

    /// Ends the session's streams for good.
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
            "notifications/message" => {
                if let Some(r) = self.router.upgrade() {
                    r.deliver_to_active(server, msg);
                }
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

/// Counts a session as active on a server while a call is in flight.
struct ActiveEntry<'a> {
    server: &'a Server,
    session: String,
}

impl<'a> ActiveEntry<'a> {
    fn new(server: &'a Server, session: &Arc<Session>) -> Self {
        lock(&server.active)
            .entry(session.id.clone())
            .or_insert_with(|| (Arc::downgrade(session), 0))
            .1 += 1;
        Self {
            server,
            session: session.id.clone(),
        }
    }
}

impl Drop for ActiveEntry<'_> {
    fn drop(&mut self) {
        let mut a = lock(&self.server.active);
        if let Some(e) = a.get_mut(&self.session) {
            e.1 = e.1.saturating_sub(1);
            if e.1 == 0 {
                a.remove(&self.session);
            }
        }
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
            let server = Arc::new(Server {
                name: name.to_owned(),
                prefix: prefix.to_owned(),
                fingerprint: fingerprint(&spec),
                instance: Instance::with_tuning(name, spec, self.events.clone(), self.tuning),
                options,
                refresh_lock: tokio::sync::Mutex::new(()),
                listed_gen: AtomicU64::new(0),
                bg_refresh: AtomicBool::new(false),
                active: Mutex::new(HashMap::new()),
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

    /// Ends a session, closes its stream and cancels its in-flight calls.
    /// False if unknown.
    pub fn end_session(&self, id: &str) -> bool {
        let session = lock(&self.sessions).remove(id);
        match session {
            Some(s) => {
                s.end();
                s.cancel_all();
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

    fn deliver_to_active(&self, server: &str, msg: Value) {
        let Some(server) = self.server(server) else {
            return;
        };
        let targets: Vec<Arc<Session>> = lock(&server.active)
            .values()
            .filter_map(|(w, _)| w.upgrade())
            .collect();
        for s in targets {
            s.push(msg.clone());
        }
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
            s.instance.shutdown().await;
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
        let conn = conn.or_else(|| server.instance.running_conn());
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
        if let Some(conn) = server.instance.running_conn() {
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
                if let Some(conn) = server.instance.running_conn() {
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
        let guard = match server.instance.acquire().await {
            Ok(g) => g,
            Err(e) => return jsonrpc::error(id, jsonrpc::BACKEND_UNAVAILABLE, e.to_string()),
        };
        self.note_conn(server, guard.conn());
        let key = jsonrpc::id_key(&id);
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        lock(&session.inflight).insert(key.clone(), cancel_tx);
        let _entry = InflightEntry { session, key };
        let _active = ActiveEntry::new(server, session);
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
