// SPDX-License-Identifier: GPL-3.0-or-later

//! Runtime control of the broker: the desired server set (config file or
//! in-memory), diff-and-apply on reload, and the admin REST API under
//! [`api::BASE`].
//!
//! The config file is the source of truth when a path is set: registering
//! or removing a server edits the file (format-preserving, atomic) and then
//! applies the file, so a later hot reload of the same file is a no-op.
//! Without a path, changes live in memory until the broker exits.
//!
//! Every mutating operation runs under one async lock, so concurrent admin
//! calls and file-watch reloads are applied one at a time.

pub mod api;
pub mod persist;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;

use crate::backend::stdio::STDERR_LINES;
use crate::backend::{BackendSpec, Conn, Instance, StateKind};
use crate::config::{self, Config, ServerConfig, Transport};
use crate::router::{scope_name, CatalogOptions, Router, ScopeOptions, Server};
use api::*;

/// Longest wait for in-flight calls on `stop` before the backend is killed.
pub const STOP_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for in-flight calls of removed or changed servers on reload.
pub const RELOAD_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Log lines returned when `lines` is not given.
pub const DEFAULT_LOG_LINES: usize = 100;
/// Largest admin request body.
pub const MAX_ADMIN_BODY: usize = 256 * 1024;
const MAX_NAME_LEN: usize = 64;

/// True if `peer` may use the admin API: loopback only, whatever the bind
/// address (IPv4-mapped IPv6 loopback included).
pub fn peer_allowed(peer: &SocketAddr) -> bool {
    peer.ip().to_canonical().is_loopback()
}

/// True if `name` is acceptable as a server name for registration.
pub fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// An admin operation failure; maps to an HTTP status and [`ErrorResponse`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    /// Unknown server.
    #[error("no server named {0:?}")]
    NotFound(String),
    /// Name already in use, or the operation needs a config file.
    #[error("{0}")]
    Conflict(String),
    /// The request or resulting config is invalid.
    #[error("{0}")]
    Invalid(String, Vec<String>),
    /// Reading or writing the config file failed.
    #[error("{0}")]
    Io(String),
}

impl ControlError {
    fn status(&self) -> StatusCode {
        match self {
            ControlError::NotFound(_) => StatusCode::NOT_FOUND,
            ControlError::Conflict(_) => StatusCode::CONFLICT,
            ControlError::Invalid(..) => StatusCode::BAD_REQUEST,
            ControlError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let details = match &self {
            ControlError::Invalid(_, d) => d.clone(),
            _ => Vec::new(),
        };
        let body = ErrorResponse {
            error: self.to_string(),
            details,
        };
        (self.status(), Json(body)).into_response()
    }
}

/// What a configured server was registered as.
#[derive(Debug, Clone, PartialEq)]
struct Applied {
    prefix: String,
    spec: BackendSpec,
    catalog: CatalogOptions,
    scope: ScopeOptions,
}

#[derive(Default)]
struct ControlState {
    config: Config,
    applied: BTreeMap<String, Applied>,
    /// Configured but not registered: name -> reason.
    skipped: BTreeMap<String, String>,
    /// Initial config, applied by `apply_initial`.
    pending: Option<Config>,
}

/// Broker control plane shared by the admin API and the config watcher.
pub struct Control {
    router: Arc<Router>,
    path: Option<PathBuf>,
    state: tokio::sync::Mutex<ControlState>,
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Builds the launch spec of one configured server. Errors never contain
/// environment values.
fn build_spec(server: &ServerConfig, broker: &config::BrokerConfig) -> Result<BackendSpec, String> {
    let ttl = server.idle_ttl_or(broker);
    let spec = match server.transport {
        Transport::Stdio => {
            let command = server
                .command
                .clone()
                .ok_or("stdio transport requires `command`")?;
            let mut spec = BackendSpec::new(command, server.args.clone());
            spec.env = config::resolve_env(server).map_err(|e| e.to_string())?;
            spec.ref_env = server
                .env
                .iter()
                .filter(|(_, v)| matches!(v, config::EnvValue::Ref { .. }))
                .map(|(k, _)| k.clone())
                .collect();
            spec.cwd = server.cwd.clone();
            if let Some(var) = &broker.token_env {
                spec.strip_env.insert(var.clone());
            }
            spec
        }
        Transport::Http => {
            let url = server.url.clone().ok_or("http transport requires `url`")?;
            let mut spec = BackendSpec::http(url);
            for (k, v) in config::resolve_headers(server).map_err(|e| e.to_string())? {
                spec = spec.header(k, v);
            }
            spec
        }
    };
    Ok(spec.idle_ttl(ttl).start_timeout(broker.start_timeout))
}

fn broker_restart_fields(old: &config::BrokerConfig, new: &config::BrokerConfig) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |name: &str, same: bool| {
        if !same {
            out.push(format!("broker.{name}"));
        }
    };
    check("bind", old.bind == new.bind);
    check("token_file", old.token_file == new.token_file);
    check("token_env", old.token_env == new.token_env);
    check("allow_remote", old.allow_remote == new.allow_remote);
    check("allowed_hosts", old.allowed_hosts == new.allowed_hosts);
    check("log_dir", old.log_dir == new.log_dir);
    check("catalog_dir", old.catalog_dir == new.catalog_dir);
    out
}

fn state_name(s: StateKind) -> String {
    match s {
        StateKind::Stopped => "stopped",
        StateKind::Starting => "starting",
        StateKind::Running => "running",
        StateKind::Draining => "draining",
        StateKind::Stopping => "stopping",
        StateKind::Failed => "failed",
    }
    .to_owned()
}

fn validation_details(errs: Vec<config::ValidationError>) -> Vec<String> {
    errs.into_iter().map(|e| e.to_string()).collect()
}

/// Reads the config file; a missing file is an empty config.
async fn read_config_text(path: &Path) -> Result<String, ControlError> {
    let p = path.to_path_buf();
    let res = tokio::task::spawn_blocking(move || std::fs::read_to_string(&p))
        .await
        .map_err(|e| ControlError::Io(e.to_string()))?;
    match res {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(ControlError::Io(format!("cannot read config: {e}"))),
    }
}

async fn write_config_text(path: &Path, text: String) -> Result<(), ControlError> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || persist::write_atomic(&p, &text))
        .await
        .map_err(|e| ControlError::Io(e.to_string()))?
        .map_err(|e| ControlError::Io(format!("cannot write config: {e}")))
}

fn parse_config(text: &str) -> Result<Config, ControlError> {
    Config::parse(text)
        .map_err(|e| ControlError::Invalid("config file is invalid".into(), vec![e.to_string()]))
}

fn refresh_result(name: &str, res: Result<crate::router::RefreshOutcome, String>) -> RefreshResult {
    match res {
        Ok(o) => RefreshResult {
            name: name.to_owned(),
            changed: o.changed,
            tools: o.tools,
            prompts: o.prompts,
            resources: o.resources,
            resource_templates: o.resource_templates,
            error: None,
        },
        Err(e) => RefreshResult {
            name: name.to_owned(),
            error: Some(e),
            ..RefreshResult::default()
        },
    }
}

/// Status of one instance (memory scanned in a blocking task).
async fn instance_status(key: String, inst: Arc<Instance>) -> InstanceStatus {
    let (pids, mem) = match inst.running_conn() {
        Some(conn) => tokio::task::spawn_blocking(move || match &*conn {
            Conn::Stdio(c) => (c.pids(), c.memory()),
            _ => (Vec::new(), Default::default()),
        })
        .await
        .unwrap_or_default(),
        None => (Vec::new(), Default::default()),
    };
    InstanceStatus {
        scope_key: key,
        state: state_name(inst.state()),
        pids,
        rss_bytes: mem.rss_bytes,
        private_bytes: mem.private_bytes,
        in_flight: inst.in_flight(),
        idle_remaining_secs: inst.idle_remaining().map(|d| d.as_secs()),
        calls: inst.calls(),
        restarts: inst.spawn_count().saturating_sub(1),
        failures: inst.failures(),
        last_error: inst.last_error(),
    }
}

/// Status of one server: its instances plus the aggregate.
async fn server_status(server: &Server) -> ServerStatus {
    let mut instances = Vec::new();
    for (key, inst) in server.instances() {
        instances.push(instance_status(key, inst).await);
    }
    let transport = if server.instance.spec().http.is_some() {
        Transport::Http
    } else {
        Transport::Stdio
    };
    let mut st = ServerStatus {
        name: server.name.clone(),
        prefix: server.prefix.clone(),
        transport,
        state: state_name(server.state()),
        pids: Vec::new(),
        rss_bytes: 0,
        private_bytes: 0,
        in_flight: 0,
        idle_remaining_secs: None,
        calls: 0,
        restarts: 0,
        failures: 0,
        last_error: None,
        scope: scope_name(server.scope()).to_owned(),
        instances: Vec::new(),
    };
    for i in &instances {
        st.pids.extend(&i.pids);
        st.rss_bytes += i.rss_bytes;
        st.private_bytes += i.private_bytes;
        st.in_flight += i.in_flight;
        st.idle_remaining_secs = st.idle_remaining_secs.max(i.idle_remaining_secs);
        st.calls += i.calls;
        st.restarts += i.restarts;
        st.failures = st.failures.max(i.failures);
        if st.last_error.is_none() {
            st.last_error = i.last_error.clone();
        }
    }
    st.instances = instances;
    st
}

/// Unloads `instances` concurrently, each draining for at most `timeout`.
async fn unload_all(instances: Vec<Arc<Instance>>, timeout: Duration) {
    let mut set = tokio::task::JoinSet::new();
    for inst in instances {
        set.spawn(async move { inst.drain(timeout).await });
    }
    while set.join_next().await.is_some() {}
}

impl Control {
    /// A control plane over `router` with `config` as the desired state
    /// (not applied until [`apply_initial`](Control::apply_initial)).
    /// `path` enables persistence and reload.
    pub fn new(router: Arc<Router>, config: Config, path: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            router,
            path,
            state: tokio::sync::Mutex::new(ControlState {
                // Same broker baseline as `config`, so applying it reports
                // no restart-only changes.
                config: Config {
                    broker: config.broker.clone(),
                    servers: BTreeMap::new(),
                },
                pending: Some(config),
                ..ControlState::default()
            }),
        })
    }

    /// The router this controls.
    pub fn router(&self) -> &Arc<Router> {
        &self.router
    }

    /// The config file, if any.
    pub fn config_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Registers the initial config's servers.
    pub async fn apply_initial(&self) -> ReloadResponse {
        let mut st = self.state.lock().await;
        let pending = st.pending.take().unwrap_or_default();
        self.apply(&mut st, pending).await
    }

    /// Diffs `new` against what is registered and applies it.
    async fn apply(&self, st: &mut ControlState, new: Config) -> ReloadResponse {
        let mut resp = ReloadResponse {
            restart_required: broker_restart_fields(&st.config.broker, &new.broker),
            ..ReloadResponse::default()
        };
        let mut desired = BTreeMap::new();
        let mut skipped = BTreeMap::new();
        for (name, s) in &new.servers {
            if !s.enabled {
                skipped.insert(name.clone(), "disabled".to_owned());
                continue;
            }
            match build_spec(s, &new.broker) {
                Ok(spec) => {
                    desired.insert(
                        name.clone(),
                        Applied {
                            prefix: s.effective_prefix(name),
                            spec,
                            catalog: CatalogOptions {
                                probe: s.probe,
                                max_age: s.catalog_max_age_or(&new.broker),
                            },
                            scope: ScopeOptions {
                                scope: s.scope,
                                max_instances: s.max_instances_or_default(),
                            },
                        },
                    );
                }
                Err(e) => {
                    resp.errors.push(format!("{name}: {e}"));
                    skipped.insert(name.clone(), e);
                }
            }
        }
        let mut unload = Vec::new();
        for (name, old) in &st.applied {
            let removed = match desired.get(name) {
                None => {
                    resp.removed.push(name.clone());
                    true
                }
                Some(d) if d != old => {
                    resp.changed.push(name.clone());
                    false
                }
                Some(_) => continue,
            };
            if let Some(server) = self.router.unregister(name) {
                unload.extend(server.all_instances());
            }
            // A changed server keeps its catalog; registration checks the
            // fingerprint and re-probes if the launch spec differs.
            if removed {
                self.router.forget_catalog(name);
            }
        }
        unload_all(unload, RELOAD_DRAIN_TIMEOUT).await;
        let mut applied = BTreeMap::new();
        for (name, d) in desired {
            let unchanged = st.applied.get(&name) == Some(&d);
            if !unchanged {
                if let Err(e) = self.router.register_scoped(
                    &name,
                    &d.prefix,
                    d.spec.clone(),
                    d.catalog,
                    d.scope,
                ) {
                    resp.errors.push(format!("{name}: {e}"));
                    skipped.insert(name, e.to_string());
                    continue;
                }
                if !resp.changed.contains(&name) {
                    resp.added.push(name.clone());
                }
            }
            applied.insert(name, d);
        }
        for f in &resp.restart_required {
            tracing::warn!(field = %f, "config change takes effect after a restart");
        }
        if !resp.added.is_empty() || !resp.removed.is_empty() || !resp.changed.is_empty() {
            tracing::info!(
                added = ?resp.added,
                removed = ?resp.removed,
                changed = ?resp.changed,
                "servers updated"
            );
        }
        for e in &resp.errors {
            tracing::warn!(error = %e, "server skipped");
        }
        self.router
            .set_client_request_timeout(new.broker.client_request_timeout);
        self.router
            .set_session_idle_timeout(new.broker.session_idle_timeout);
        st.applied = applied;
        st.skipped = skipped;
        st.config = new;
        resp
    }

    /// Re-reads the config file and applies it. On a parse or validation
    /// error nothing changes.
    pub async fn reload(&self) -> Result<ReloadResponse, ControlError> {
        let path = self.path.as_deref().ok_or_else(|| {
            ControlError::Conflict("the broker runs without a config file".into())
        })?;
        let mut st = self.state.lock().await;
        let p = path.to_path_buf();
        let text = tokio::task::spawn_blocking(move || Config::load(&p))
            .await
            .map_err(|e| ControlError::Io(e.to_string()))?
            .map_err(|e| match e {
                config::ConfigError::Io(e) => ControlError::Io(format!("cannot read config: {e}")),
                other => {
                    ControlError::Invalid("config file is invalid".into(), vec![other.to_string()])
                }
            })?;
        text.validate().map_err(|e| {
            ControlError::Invalid("config file is invalid".into(), validation_details(e))
        })?;
        Ok(self.apply(&mut st, text).await)
    }

    /// Registers a server (persisted when a config file is set).
    pub async fn register(&self, req: RegisterRequest) -> Result<RegisterResponse, ControlError> {
        let RegisterRequest { name, server } = req;
        if !valid_server_name(&name) {
            return Err(ControlError::Invalid(
                format!("invalid server name {name:?}"),
                vec![format!(
                    "names use [A-Za-z0-9_.-], at most {MAX_NAME_LEN} characters"
                )],
            ));
        }
        let mut st = self.state.lock().await;
        let (mut candidate, text) = match &self.path {
            Some(p) => {
                let text = read_config_text(p).await?;
                (parse_config(&text)?, Some(text))
            }
            None => (st.config.clone(), None),
        };
        if candidate.servers.contains_key(&name)
            || st.config.servers.contains_key(&name)
            || self.router.instance(&name).is_some()
        {
            return Err(ControlError::Conflict(format!(
                "server {name:?} already exists"
            )));
        }
        candidate.servers.insert(name.clone(), server.clone());
        candidate.validate().map_err(|e| {
            ControlError::Invalid("invalid server configuration".into(), validation_details(e))
        })?;
        if server.enabled {
            build_spec(&server, &candidate.broker).map_err(|e| {
                ControlError::Invalid("invalid server configuration".into(), vec![e])
            })?;
        }
        let persisted = match (&self.path, text) {
            (Some(p), Some(text)) => {
                let new_text = persist::upsert_server(&text, &name, &server)
                    .map_err(|e| ControlError::Io(e.to_string()))?;
                write_config_text(p, new_text).await?;
                true
            }
            _ => false,
        };
        self.apply(&mut st, candidate).await;
        let entry = self.entry(&st, &name);
        if let Some(e) = &entry.error {
            if server.enabled {
                return Err(ControlError::Invalid(
                    format!("server {name:?} not registered"),
                    vec![e.clone()],
                ));
            }
        }
        Ok(RegisterResponse {
            server: entry,
            persisted,
        })
    }

    /// Removes a server: from the config (file or memory) if it is there,
    /// else from the router; its backend is drained and stopped.
    pub async fn remove(&self, name: &str) -> Result<RemoveResponse, ControlError> {
        let mut st = self.state.lock().await;
        if let Some(p) = &self.path {
            let text = read_config_text(p).await?;
            let mut candidate = parse_config(&text)?;
            if candidate.servers.remove(name).is_some() {
                let new_text = persist::remove_server(&text, name)
                    .map_err(|e| ControlError::Io(e.to_string()))?
                    .ok_or_else(|| ControlError::NotFound(name.to_owned()))?;
                write_config_text(p, new_text).await?;
                self.apply(&mut st, candidate).await;
                return Ok(RemoveResponse {
                    name: name.to_owned(),
                    persisted: true,
                });
            }
        } else if st.config.servers.contains_key(name) {
            let mut candidate = st.config.clone();
            candidate.servers.remove(name);
            self.apply(&mut st, candidate).await;
            return Ok(RemoveResponse {
                name: name.to_owned(),
                persisted: false,
            });
        }
        if st.applied.contains_key(name) {
            // Registered from a config the file no longer has: unregister it.
            st.applied.remove(name);
            st.config.servers.remove(name);
        }
        match self.router.unregister(name) {
            Some(server) => {
                self.router.forget_catalog(name);
                unload_all(server.all_instances(), STOP_DRAIN_TIMEOUT).await;
                Ok(RemoveResponse {
                    name: name.to_owned(),
                    persisted: false,
                })
            }
            None => Err(ControlError::NotFound(name.to_owned())),
        }
    }

    /// Stops a server's backend: drains it (or kills it with `force`). The
    /// server stays registered; the next call starts it again.
    pub async fn stop(&self, name: &str, force: bool) -> Result<StopResponse, ControlError> {
        let server = self
            .router
            .server(name)
            .ok_or_else(|| ControlError::NotFound(name.to_owned()))?;
        let all = server.all_instances();
        if force {
            for inst in &all {
                inst.shutdown().await;
            }
        } else {
            unload_all(all, STOP_DRAIN_TIMEOUT).await;
        }
        Ok(StopResponse {
            name: name.to_owned(),
            state: state_name(server.state()),
        })
    }

    /// Re-lists one server now (on its running process, else by probing)
    /// and stores the catalog if it changed. A listing failure is reported
    /// in `error`, not as an HTTP error.
    pub async fn refresh(&self, name: &str) -> Result<RefreshResult, ControlError> {
        let res = self
            .router
            .refresh(name)
            .await
            .ok_or_else(|| ControlError::NotFound(name.to_owned()))?;
        Ok(refresh_result(name, res))
    }

    /// Re-lists every registered server, concurrently.
    pub async fn refresh_all(&self) -> RefreshResponse {
        let mut set = tokio::task::JoinSet::new();
        for s in self.router.servers() {
            let router = self.router.clone();
            set.spawn(async move {
                let res = router
                    .refresh(&s.name)
                    .await
                    .unwrap_or_else(|| Err("server is no longer registered".into()));
                refresh_result(&s.name, res)
            });
        }
        let mut servers = Vec::new();
        while let Some(r) = set.join_next().await {
            if let Ok(r) = r {
                servers.push(r);
            }
        }
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        RefreshResponse { servers }
    }

    /// The last `lines` stderr lines of a server.
    pub fn logs(&self, name: &str, lines: Option<usize>) -> Result<LogsResponse, ControlError> {
        let server = self
            .router
            .server(name)
            .ok_or_else(|| ControlError::NotFound(name.to_owned()))?;
        let n = lines.unwrap_or(DEFAULT_LOG_LINES).clamp(1, STDERR_LINES);
        let mut all = Vec::new();
        for (key, inst) in server.instances() {
            if key.is_empty() {
                all.extend(inst.stderr_tail());
            } else {
                all.extend(
                    inst.stderr_tail()
                        .into_iter()
                        .map(|l| format!("[{key}] {l}")),
                );
            }
        }
        let skip = all.len().saturating_sub(n);
        Ok(LogsResponse {
            name: name.to_owned(),
            lines: all.into_iter().skip(skip).collect(),
        })
    }

    /// Runtime status of every registered backend.
    pub async fn status(&self) -> StatusResponse {
        let mut servers = Vec::new();
        for server in self.router.servers() {
            servers.push(server_status(&server).await);
        }
        StatusResponse {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            sessions: self.router.session_count(),
            servers,
        }
    }

    fn entry(&self, st: &ControlState, name: &str) -> ServerEntry {
        let cfg = st.config.servers.get(name);
        let registered = self.router.instance(name).is_some();
        ServerEntry {
            name: name.to_owned(),
            prefix: match (st.applied.get(name), cfg) {
                (Some(a), _) => a.prefix.clone(),
                (None, Some(c)) => c.effective_prefix(name),
                (None, None) => self
                    .router
                    .servers()
                    .into_iter()
                    .find(|s| s.name == name)
                    .map(|s| s.prefix.clone())
                    .unwrap_or_default(),
            },
            registered,
            config: cfg.cloned(),
            error: st.skipped.get(name).cloned(),
        }
    }

    /// Every configured or registered server.
    pub async fn servers(&self) -> ServersResponse {
        let st = self.state.lock().await;
        let mut names: BTreeSet<String> = st.config.servers.keys().cloned().collect();
        names.extend(self.router.servers().into_iter().map(|s| s.name.clone()));
        ServersResponse {
            servers: names.iter().map(|n| self.entry(&st, n)).collect(),
        }
    }

    /// Stops every backend (broker shutdown).
    pub async fn shutdown(&self) {
        self.router.shutdown().await;
    }
}

// ------------------------------------------------------------------ routes

/// Admin routes, relative to [`api::BASE`]. Authentication and the loopback
/// check are applied by the HTTP layer.
pub fn routes(control: Arc<Control>) -> axum::Router {
    axum::Router::new()
        .route("/status", get(get_status))
        .route("/servers", get(get_servers).post(post_server))
        .route("/servers/{name}", axum::routing::delete(delete_server))
        .route("/servers/{name}/stop", post(post_stop))
        .route("/servers/{name}/logs", get(get_logs))
        .route("/servers/{name}/refresh", post(post_refresh_one))
        .route("/reload", post(post_reload))
        .route("/refresh", post(post_refresh_all))
        .with_state(control)
}

async fn post_refresh_one(
    State(c): State<Arc<Control>>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<RefreshResult>, ControlError> {
    c.refresh(&name).await.map(Json)
}

async fn post_refresh_all(State(c): State<Arc<Control>>) -> Json<RefreshResponse> {
    Json(c.refresh_all().await)
}

async fn get_status(State(c): State<Arc<Control>>) -> Json<StatusResponse> {
    Json(c.status().await)
}

async fn get_servers(State(c): State<Arc<Control>>) -> Json<ServersResponse> {
    Json(c.servers().await)
}

async fn post_server(State(c): State<Arc<Control>>, body: Bytes) -> Response {
    if body.len() > MAX_ADMIN_BODY {
        return ControlError::Invalid("request body too large".into(), Vec::new()).into_response();
    }
    let req: RegisterRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return ControlError::Invalid("invalid request body".into(), vec![e.to_string()])
                .into_response()
        }
    };
    match c.register(req).await {
        Ok(r) => (StatusCode::CREATED, Json(r)).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn delete_server(
    State(c): State<Arc<Control>>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<RemoveResponse>, ControlError> {
    c.remove(&name).await.map(Json)
}

async fn post_stop(
    State(c): State<Arc<Control>>,
    UrlPath(name): UrlPath<String>,
    Query(q): Query<StopQuery>,
) -> Result<Json<StopResponse>, ControlError> {
    c.stop(&name, q.force).await.map(Json)
}

async fn get_logs(
    State(c): State<Arc<Control>>,
    UrlPath(name): UrlPath<String>,
    Query(q): Query<LogsQuery>,
) -> Result<Json<LogsResponse>, ControlError> {
    c.logs(&name, q.lines).map(Json)
}

async fn post_reload(State(c): State<Arc<Control>>) -> Result<Json<ReloadResponse>, ControlError> {
    c.reload().await.map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_peer_must_be_loopback() {
        for ok in [
            "127.0.0.1:1",
            "127.8.9.10:2",
            "[::1]:3",
            "[::ffff:127.0.0.1]:4",
        ] {
            assert!(peer_allowed(&ok.parse().unwrap()), "{ok}");
        }
        for bad in [
            "203.0.113.5:1",
            "0.0.0.0:2",
            "[2001:db8::1]:3",
            "[::ffff:203.0.113.5]:4",
            "[::]:5",
        ] {
            assert!(!peer_allowed(&bad.parse().unwrap()), "{bad}");
        }
    }

    #[test]
    fn server_names() {
        assert!(valid_server_name("example-1.v2_x"));
        for bad in ["", "a b", "a/b", "ä", &"a".repeat(65)] {
            assert!(!valid_server_name(bad), "{bad}");
        }
    }

    #[test]
    fn spec_resolution_never_leaks_values() {
        let s: ServerConfig = serde_json::from_str(
            r#"{"transport":"stdio","command":"prog","env":{"K":{"from_env":"MCPDOZE_TEST_UNSET_VAR_XYZ"}}}"#,
        )
        .unwrap();
        let e = build_spec(&s, &config::BrokerConfig::default()).unwrap_err();
        assert!(e.contains("MCPDOZE_TEST_UNSET_VAR_XYZ"), "{e}");
    }

    #[test]
    fn spec_strips_broker_token_variables() {
        let s: ServerConfig =
            serde_json::from_str(r#"{"transport":"stdio","command":"prog"}"#).unwrap();
        let spec = build_spec(&s, &config::BrokerConfig::default()).unwrap();
        assert!(spec.strip_env.contains("MCPDOZE_TOKEN"));
        let broker = config::BrokerConfig {
            token_env: Some("EXAMPLE_BROKER_TOKEN".into()),
            ..config::BrokerConfig::default()
        };
        let spec = build_spec(&s, &broker).unwrap();
        assert!(spec.strip_env.contains("MCPDOZE_TOKEN"));
        assert!(spec.strip_env.contains("EXAMPLE_BROKER_TOKEN"));
    }
}
