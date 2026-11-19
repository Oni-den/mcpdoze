// SPDX-License-Identifier: GPL-3.0-or-later

//! The router: agent sessions on one side, backend instances on the other.
//!
//! * Agent sessions are broker-side objects with their own negotiated
//!   protocol version; every backend sees one synthetic client, the broker.
//! * Tools and prompts are exposed as `<prefix>__<name>` ([`naming`]);
//!   resource URIs are mapped reversibly to `mcpdoze://<prefix>/...`
//!   ([`uri`]). Resource subscriptions are shared per backend instance
//!   (`subs.rs`, `docs/SUBSCRIPTIONS.md`).
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
//! has its own lifecycle. At most `max_instances` scoped instances exist per
//! server. The catalog is keyed by `(server, cwd key)`: a `per-cwd`
//! server's scoped instance lists into its directory's entry, and a
//! session's lists use the entry of its directory, falling back to the
//! server's shared entry. List changes go only to sessions reading the
//! changed entry.
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
mod events;
mod forward;
mod lists;
pub mod modern;
pub mod naming;
pub mod optimize;
mod session;
mod subs;
pub mod uri;
mod usage;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use serde_json::{json, Value};

use crate::backend::jsonrpc;
use crate::backend::stdio::lock;
use crate::backend::{BackendSpec, ClientSide, Conn, Instance, StateKind, Tuning};
use crate::config::DebugSettings;
use crate::config::Scope;
use crate::diag::{Diag, DiagConfig, Level, ScrubContext, TapIdent, Transport};
pub use catalog::{
    fingerprint, Catalog, CatalogEntry, DiskCatalog, ListChanges, MemoryCatalog, CATALOG_VERSION,
};
use events::{ProgressTable, RouterEvents};
use lists::expired;
#[cfg(test)]
use session::random_id;
use session::RootsState;
pub use session::{Era, Session, StreamPoll};
pub use subs::SubscriptionStats;

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
    /// Per instance (scope key, "" for the shared one): the connection
    /// generation whose lists are known.
    listed: Mutex<HashMap<String, u64>>,
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

    /// The catalog key of instance scope_key: its directory for a
    /// per-cwd server's scoped instance, else None (the shared entry).
    pub fn catalog_key_of(&self, scope_key: &str) -> Option<String> {
        (self.scope.scope == Scope::PerCwd && !scope_key.is_empty()).then(|| scope_key.to_owned())
    }

    /// A running connection whose lists belong to catalog key ckey, with
    /// its instance's scope key: the scoped instance of that directory, or
    /// for None the shared instance (and, unless the server is per-cwd,
    /// any scoped instance).
    pub(crate) fn running_for(&self, ckey: Option<&str>) -> Option<(String, Arc<Conn>)> {
        match ckey {
            Some(k) => self
                .scoped_instance(k)
                .and_then(|i| i.running_conn())
                .map(|c| (k.to_owned(), c)),
            None => {
                if let Some(c) = self.instance.running_conn() {
                    return Some((String::new(), c));
                }
                if self.scope.scope == Scope::PerCwd {
                    return None;
                }
                lock(&self.scoped)
                    .iter()
                    .find_map(|(k, i)| i.running_conn().map(|c| (k.clone(), c)))
            }
        }
    }

    /// Running scoped instances of a per-cwd server, by directory key.
    pub(crate) fn running_keyed(&self) -> Vec<(String, Arc<Conn>)> {
        if self.scope.scope != Scope::PerCwd {
            return Vec::new();
        }
        lock(&self.scoped)
            .iter()
            .filter_map(|(k, i)| i.running_conn().map(|c| (k.clone(), c)))
            .collect()
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
            !(idle && Arc::strong_count(i) == 1 && i.in_flight() == 0 && i.pins() == 0)
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

/// The recorder and how deep each server records.
#[derive(Clone)]
struct DiagState {
    diag: Arc<Diag>,
    /// Level for servers without a resolved setting (set by
    /// [`Router::set_diag`] / [`Router::set_diag_level`]).
    level: Level,
    /// Per-server levels from `[debug]` (see [`Router::apply_debug`]).
    settings: Option<Arc<DebugSettings>>,
}

impl DiagState {
    fn level_for(&self, server: &str) -> Level {
        self.settings
            .as_ref()
            .map_or(self.level, |s| s.level_for(server))
    }
}

fn same_diag_config(a: &DiagConfig, b: &DiagConfig) -> bool {
    a.dir == b.dir
        && a.max_file_bytes == b.max_file_bytes
        && a.keep == b.keep
        && a.frames == b.frames
        && a.stderr_lines == b.stderr_lines
        && a.slow_call == b.slow_call
        && a.tool_errors == b.tool_errors
        && a.per_minute == b.per_minute
        && a.queue == b.queue
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
    subs: subs::Subscriptions,
    /// 2026-07-28 sessions of requests and listen streams in progress.
    live: Mutex<HashMap<String, Weak<Session>>>,
    /// Statistics log, when enabled.
    stats: RwLock<Option<Arc<crate::stats::Stats>>>,
    /// Failure diagnostics recorder and level, when set.
    diag: RwLock<Option<DiagState>>,
    /// Per-process request counter shared by the statistics log and the
    /// diagnostics records.
    next_req: AtomicU64,
    /// Compiled `optimize` policies by server name (absent: untouched).
    policies: RwLock<HashMap<String, Arc<optimize::ServerPolicy>>>,
    /// Broker-served meta-tools (`mcpdoze__*`).
    meta: RwLock<optimize::meta::MetaRegistry>,
    /// Truncated results for `mcpdoze__result_get`.
    results: Arc<optimize::store::ResultStore>,
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
            subs: subs::Subscriptions::default(),
            live: Mutex::new(HashMap::new()),
            stats: RwLock::new(None),
            diag: RwLock::new(None),
            next_req: AtomicU64::new(1),
            policies: RwLock::new(HashMap::new()),
            meta: RwLock::new({
                let mut reg = optimize::meta::MetaRegistry::default();
                reg.register(Arc::new(optimize::meta::ResultGet::new(me.clone())));
                reg
            }),
            results: Arc::default(),
        })
    }

    /// Sets (or with `None` clears) the optimisation policy of `server`.
    /// Backends are not touched; callers announce a changed tool list with
    /// [`notify_tools_changed`](Router::notify_tools_changed).
    pub fn set_policy(&self, server: &str, policy: Option<optimize::ServerPolicy>) {
        let mut m = self.policies.write().unwrap_or_else(|e| e.into_inner());
        match policy {
            Some(p) => m.insert(server.to_owned(), Arc::new(p)),
            None => m.remove(server),
        };
    }

    /// The optimisation policy of `server`, if it has one.
    pub fn policy(&self, server: &str) -> Option<Arc<optimize::ServerPolicy>> {
        self.policies
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(server)
            .cloned()
    }

    /// The result store (`mcpdoze__result_get`).
    pub fn result_store(&self) -> &Arc<optimize::store::ResultStore> {
        &self.results
    }

    /// Whether any server keeps results for `mcpdoze__result_get`.
    pub fn result_store_enabled(&self) -> bool {
        self.policies
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|p| p.stores_results())
    }

    /// Registers a meta-tool (`mcpdoze__<name>`), listed while it is enabled.
    pub fn register_meta_tool(&self, tool: Arc<dyn optimize::meta::MetaTool>) {
        self.meta
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .register(tool);
    }

    /// Sends `notifications/tools/list_changed` to every streaming session
    /// that takes it (an optimisation policy changed the served list).
    pub fn notify_tools_changed(&self) {
        let method = "notifications/tools/list_changed";
        let msg = json!({"jsonrpc": "2.0", "method": method});
        for s in self.streaming_sessions() {
            if s.wants_notification(method) {
                s.push(msg.clone());
            }
        }
    }

    /// The next request number. One counter serves both the statistics log
    /// (`req_id`) and the diagnostics records (`req`), so they correlate.
    pub fn next_request_id(&self) -> u64 {
        self.next_req.fetch_add(1, Ordering::Relaxed)
    }

    /// Enables failure diagnostics: every instance (existing and future)
    /// gets a tap on `diag` at `level`.
    pub fn set_diag(&self, diag: Arc<Diag>, level: Level) {
        *self.diag.write().unwrap_or_else(|e| e.into_inner()) = Some(DiagState {
            diag,
            level,
            settings: None,
        });
        self.reattach_diag();
    }

    fn reattach_diag(&self) {
        for server in self.servers() {
            for inst in server.all_instances() {
                self.attach_diag(&server.name, &inst);
            }
        }
    }

    /// Applies the effective `[debug]` settings: builds (or rebuilds, when
    /// its configuration changed) the recorder and sets every instance's
    /// level from [`DebugSettings::level_for`]. Never restarts a backend.
    pub fn apply_debug(&self, settings: &Arc<DebugSettings>) {
        let rebuilt = {
            let mut g = self.diag.write().unwrap_or_else(|e| e.into_inner());
            let keep = match (g.as_ref(), &settings.diag) {
                (Some(st), Some(cfg)) => same_diag_config(st.diag.config(), cfg),
                (Some(_), None) => true,
                (None, _) => false,
            };
            match (&settings.diag, g.as_mut()) {
                (Some(_), Some(st)) if keep => {
                    st.settings = Some(settings.clone());
                    false
                }
                (Some(cfg), _) => {
                    *g = Some(DiagState {
                        diag: Diag::new(cfg.clone()),
                        level: Level::Off,
                        settings: Some(settings.clone()),
                    });
                    true
                }
                (None, Some(st)) => {
                    st.settings = Some(settings.clone());
                    false
                }
                (None, None) => return,
            }
        };
        if rebuilt {
            self.reattach_diag();
            return;
        }
        for server in self.servers() {
            let level = settings.level_for(&server.name);
            for inst in server.all_instances() {
                inst.tap().set_level(level);
            }
        }
    }

    /// Follows the effective debug settings (see
    /// `Control::debug_settings`): applies the current value now and every
    /// later change from a background task. Needs a Tokio runtime.
    pub fn follow_debug(&self, mut rx: tokio::sync::watch::Receiver<Arc<DebugSettings>>) {
        let current = rx.borrow_and_update().clone();
        self.apply_debug(&current);
        let me = self.me.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let next = rx.borrow_and_update().clone();
                let Some(r) = me.upgrade() else { return };
                r.apply_debug(&next);
            }
        });
    }

    /// The diagnostics recorder, when set.
    pub fn diag(&self) -> Option<Arc<Diag>> {
        self.diag
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|st| st.diag.clone())
    }

    /// Changes the diagnostics level of every instance without restarting
    /// anything (no effect before [`set_diag`](Router::set_diag)).
    pub fn set_diag_level(&self, level: Level) {
        {
            let mut g = self.diag.write().unwrap_or_else(|e| e.into_inner());
            match g.as_mut() {
                Some(d) => {
                    d.level = level;
                    d.settings = None;
                }
                None => return,
            }
        }
        for server in self.servers() {
            for inst in server.all_instances() {
                inst.tap().set_level(level);
            }
        }
    }

    /// Gives `inst` a tap when diagnostics are set up.
    pub(super) fn attach_diag(&self, server: &str, inst: &Arc<Instance>) {
        let Some(st) = self.diag.read().unwrap_or_else(|e| e.into_inner()).clone() else {
            return;
        };
        let (diag, level) = (st.diag.clone(), st.level_for(server));
        let spec = inst.spec();
        let http = spec.http.is_some();
        let mut secrets: Vec<(String, String)> = spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Some(h) = &spec.http {
            secrets.extend(h.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        if let Ok(t) = std::env::var(crate::backend::TOKEN_ENV_VAR) {
            secrets.push((crate::backend::TOKEN_ENV_VAR.to_owned(), t));
        }
        let mut scrub = ScrubContext::new(secrets).with_system();
        if let Some(c) = spec.cwd.as_deref().and_then(Path::to_str) {
            scrub = scrub.with_cwd(c);
        }
        let key = &spec.client.scope_key;
        let scope = self.server(server).map_or(Scope::Shared, |s| s.scope.scope);
        let ident = TapIdent {
            server: server.to_owned(),
            transport: if http {
                Transport::Http
            } else {
                Transport::Stdio
            },
            launcher: (!http).then(|| spec.command.clone()),
            scope: match scope {
                Scope::Shared => crate::diag::Scope::Shared,
                Scope::PerCwd => crate::diag::Scope::PerCwd,
                Scope::PerSession => crate::diag::Scope::PerSession,
            },
            instance_key: (!key.is_empty()).then(|| key.clone()),
        };
        inst.set_tap(diag.tap(ident, level, Arc::new(scrub)));
    }

    /// The tap of server `server`'s instance with scope key `scope_key`
    /// (a tap that is always off when there is none).
    pub(super) fn tap_for(&self, server: &str, scope_key: &str) -> Arc<crate::diag::DiagTap> {
        self.instance_for_scope(server, scope_key)
            .map_or_else(crate::diag::DiagTap::disabled, |i| i.tap())
    }

    /// Server `server`'s instance with scope key `scope_key`.
    pub(super) fn instance_for_scope(
        &self,
        server: &str,
        scope_key: &str,
    ) -> Option<Arc<Instance>> {
        self.server(server)?
            .all_instances()
            .into_iter()
            .find(|i| i.spec().client.scope_key == scope_key)
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
        if !naming::valid_prefix(prefix) || prefix == optimize::meta::PREFIX {
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
                listed: Mutex::new(HashMap::new()),
                bg_refresh: AtomicBool::new(false),
            });
            reg.by_name.insert(name.to_owned(), server.clone());
            reg.by_prefix.insert(prefix.to_owned(), server.clone());
            server
        };
        self.hook_instance(name, &server.instance);
        self.attach_diag(name, &server.instance);
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
        let mut all: Vec<_> = lock(&self.sessions).values().cloned().collect();
        all.extend(self.live_sessions());
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
            "capabilities": self.capabilities(),
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
            self.record_session(false);
        }
        self.record_session(true);
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
                self.record_session(false);
                true
            }
            None => false,
        }
    }

    /// Queues `msg` for every stored session with an open stream.
    pub fn broadcast(&self, msg: &Value) {
        let method = msg["method"].as_str().unwrap_or_default();
        for s in self.streaming_sessions() {
            if s.wants_notification(method) {
                s.push(msg.clone());
            }
        }
    }

    /// Sessions with an open notification stream: stored ones and live
    /// 2026-07-28 ones (each filters by [Session::wants_notification]).
    fn streaming_sessions(&self) -> Vec<Arc<Session>> {
        let mut out: Vec<Arc<Session>> = lock(&self.sessions)
            .values()
            .filter(|s| s.has_stream())
            .cloned()
            .collect();
        out.extend(self.live_sessions().into_iter().filter(|s| s.has_stream()));
        out
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
                        ..ClientSide::default()
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
        let mut spec = make();
        spec.client.scope_key = key.clone();
        let inst = Instance::with_tuning(&server.name, spec, self.events.clone(), self.tuning);
        self.hook_instance(&server.name, &inst);
        self.attach_diag(&server.name, &inst);
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

    fn tools_entry(fp: &str, tools: &[&str]) -> CatalogEntry {
        CatalogEntry {
            fingerprint: fp.to_owned(),
            tools: tools.iter().map(|t| json!({"name": t})).collect(),
            ..CatalogEntry::default()
        }
    }

    #[tokio::test]
    async fn per_cwd_catalog_resolves_and_notifies_by_key() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (ka, kb) = (cwd_key(a.path()), cwd_key(b.path()));
        let c = Arc::new(MemoryCatalog::default());
        let r = Router::with(c.clone(), Tuning::default());
        let spec = BackendSpec::new("example-never-run", ["--dir", "${cwd}"]);
        let fp = fingerprint(&spec);
        c.put("srv", tools_entry(&fp, &["shared"]));
        c.put_at("srv", Some(&ka), tools_entry(&fp, &["in_a"]));
        // A stale per-directory entry (other launch spec) is ignored.
        c.put_at("srv", Some(&kb), tools_entry("v1-other", &["stale"]));
        r.register_scoped(
            "srv",
            "s",
            spec,
            CatalogOptions::default(),
            ScopeOptions {
                scope: Scope::PerCwd,
                max_instances: 4,
            },
        )
        .unwrap();
        let sa = r.create_session("2025-11-25");
        r.note_client_cwd(&sa, &a.path().to_string_lossy(), false);
        let sb = r.create_session("2025-11-25");
        r.note_client_cwd(&sb, &b.path().to_string_lossy(), false);
        let sn = r.create_session("2025-11-25");
        let names = |v: Vec<Value>| -> Vec<String> {
            v.iter()
                .map(|t| t["name"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(names(r.list_tools(&sa).await), ["s__in_a"]);
        assert_eq!(names(r.list_tools(&sb).await), ["s__shared"]);
        assert_eq!(names(r.list_tools(&sn).await), ["s__shared"]);
        let server = r.server("srv").unwrap();
        assert_eq!(server.instance.spawn_count(), 0);
        assert_eq!(server.scoped_count(), 0);

        for s in [&sa, &sb, &sn] {
            s.open_stream().unwrap();
        }
        let changed = |s: &Session| {
            s.take_notifications()
                .iter()
                .any(|n| n["method"] == "notifications/tools/list_changed")
        };
        // A change in directory A reaches only the session in A.
        r.apply_entry(&server, Some(&ka), tools_entry("", &["in_a", "more"]));
        assert!(changed(&sa));
        assert!(!changed(&sb));
        assert!(!changed(&sn));
        assert!(c.entry_at("srv", Some(&ka)).unwrap().has_tool("more"));
        // A shared change reaches the sessions without their own entry.
        r.apply_entry(&server, None, tools_entry("", &["shared2"]));
        assert!(!changed(&sa));
        assert!(changed(&sb));
        assert!(changed(&sn));
        assert_eq!(names(r.list_tools(&sa).await), ["s__in_a", "s__more"]);
        assert_eq!(names(r.list_tools(&sn).await), ["s__shared2"]);
        // Removing the server drops every key.
        r.forget_catalog("srv");
        assert!(c.entry_at("srv", Some(&ka)).is_none());
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
        assert_eq!(resp["error"]["code"], jsonrpc::INVALID_PARAMS);
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
