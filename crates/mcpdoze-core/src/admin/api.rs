// SPDX-License-Identifier: GPL-3.0-or-later

//! Admin API wire types (`/admin/v1`), shared by the broker and CLI clients.
//!
//! Every type is plain JSON via serde; see `docs/ADMIN_API.md` for the
//! endpoints. Unknown response fields must be ignored by clients so the API
//! can grow without a version bump.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{ServerConfig, Transport};

/// Path prefix of every admin endpoint.
pub const BASE: &str = "/admin/v1";

/// One backend in `GET /status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerStatus {
    /// Server name.
    pub name: String,
    /// Tool prefix.
    pub prefix: String,
    /// `stdio` or `http`.
    pub transport: Transport,
    /// Lifecycle state: `stopped`, `starting`, `running`, `draining`,
    /// `stopping` or `failed`.
    pub state: String,
    /// Pids of the live process tree (empty unless running a stdio backend).
    pub pids: Vec<u32>,
    /// Resident memory summed over the tree, in bytes.
    pub rss_bytes: u64,
    /// Private memory summed over the tree, in bytes.
    pub private_bytes: u64,
    /// Calls currently in flight.
    pub in_flight: usize,
    /// Seconds until the idle unload (running and idle only).
    pub idle_remaining_secs: Option<u64>,
    /// Calls served since registration.
    pub calls: u64,
    /// Processes or sessions started after the first one (respawn after an
    /// idle unload or a crash).
    pub restarts: u64,
    /// Consecutive failures counted for the crash backoff.
    pub failures: u32,
    /// Most recent start failure or crash.
    pub last_error: Option<String>,
    /// Instance scope: `shared`, `per-cwd` or `per-session`. For scoped
    /// servers the fields above aggregate every instance.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// One entry per backend instance `(server, scope_key)`.
    #[serde(default)]
    pub instances: Vec<InstanceStatus>,
}

fn default_scope() -> String {
    "shared".into()
}

/// One backend instance in [`ServerStatus::instances`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InstanceStatus {
    /// `""` for the shared instance (or the fallback of a scoped server),
    /// the directory for `per-cwd`, `session:<id prefix>` for `per-session`.
    pub scope_key: String,
    /// Lifecycle state (as [`ServerStatus::state`]).
    pub state: String,
    /// Pids of the live process tree.
    pub pids: Vec<u32>,
    /// Resident memory summed over the tree, in bytes.
    pub rss_bytes: u64,
    /// Private memory summed over the tree, in bytes.
    pub private_bytes: u64,
    /// Calls currently in flight.
    pub in_flight: usize,
    /// Seconds until the idle unload (running and idle only).
    pub idle_remaining_secs: Option<u64>,
    /// Calls served by this instance.
    pub calls: u64,
    /// Processes started after the first one.
    pub restarts: u64,
    /// Consecutive failures counted for the crash backoff.
    pub failures: u32,
    /// Most recent start failure or crash.
    pub last_error: Option<String>,
    /// Failure diagnostics mode of this instance (`off`, `failures` or
    /// `trace`).
    #[serde(default = "default_debug")]
    pub debug: String,
    /// Newest classified failure of this instance in the failure log
    /// (`None` while the log has none, or diagnostics never ran).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<LastFailure>,
}

fn default_debug() -> String {
    "off".into()
}

/// A failure record reference in [`InstanceStatus::last_failure`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastFailure {
    /// Record id in the failure log (`f-NNNNNN`).
    pub id: String,
    /// Failure kind (snake case).
    pub kind: String,
    /// Unix time in ms.
    pub ts: u64,
}

/// `GET /status` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    /// Broker version.
    pub version: String,
    /// Stored agent sessions.
    pub sessions: usize,
    /// Registered backends, ordered by name.
    pub servers: Vec<ServerStatus>,
}

/// One server in `GET /servers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerEntry {
    /// Server name.
    pub name: String,
    /// Effective tool prefix.
    pub prefix: String,
    /// True while the router serves it.
    pub registered: bool,
    /// Its configuration, when it comes from the config (file or admin API).
    pub config: Option<ServerConfig>,
    /// Why a configured server is not registered (disabled, missing
    /// environment variable, ...).
    pub error: Option<String>,
}

/// `GET /servers` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServersResponse {
    /// Every configured or registered server, ordered by name.
    pub servers: Vec<ServerEntry>,
}

/// `POST /servers` body: register one server. `server` has exactly the shape
/// of a `[servers.<name>]` table of the config file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterRequest {
    /// Unique server name (`[A-Za-z0-9_.-]`, at most 64 characters).
    pub name: String,
    /// Server configuration.
    pub server: ServerConfig,
}

/// `POST /servers` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterResponse {
    /// The new entry.
    pub server: ServerEntry,
    /// True if the config file was updated; false when the broker runs
    /// without a config file (the server lasts until restart).
    pub persisted: bool,
}

/// `DELETE /servers/{name}` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoveResponse {
    /// Removed server.
    pub name: String,
    /// True if the config file was updated.
    pub persisted: bool,
}

/// `POST /servers/{name}/stop` query.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StopQuery {
    /// Kill immediately (failing calls in flight) instead of draining.
    #[serde(default)]
    pub force: bool,
}

/// `POST /servers/{name}/stop` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopResponse {
    /// Server name.
    pub name: String,
    /// State after the stop.
    pub state: String,
}

/// `POST /reload` response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReloadResponse {
    /// Servers newly registered.
    pub added: Vec<String>,
    /// Servers unregistered and unloaded.
    pub removed: Vec<String>,
    /// Servers whose configuration changed (unloaded and re-registered).
    pub changed: Vec<String>,
    /// Per-server problems (`name: message`); those servers are skipped.
    pub errors: Vec<String>,
    /// Broker-section changes that only take effect after a restart.
    pub restart_required: Vec<String>,
}

/// `POST /servers/{name}/refresh` response, and one item of
/// `POST /refresh`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RefreshResult {
    /// Server name.
    pub name: String,
    /// True if any list differed from the stored catalog (it was updated and
    /// streaming sessions were notified).
    #[serde(default)]
    pub changed: bool,
    /// Tools listed.
    #[serde(default)]
    pub tools: usize,
    /// Prompts listed.
    #[serde(default)]
    pub prompts: usize,
    /// Resources listed.
    #[serde(default)]
    pub resources: usize,
    /// Resource templates listed.
    #[serde(default)]
    pub resource_templates: usize,
    /// Why listing failed (the catalog is then unchanged).
    #[serde(default)]
    pub error: Option<String>,
}

/// `POST /refresh` response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RefreshResponse {
    /// One result per registered server, ordered by name.
    pub servers: Vec<RefreshResult>,
}

/// `GET /stats` query.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsQuery {
    /// Period to summarise, a duration such as `7d` or `12h` (default
    /// [`DEFAULT_STATS_SINCE`]).
    pub since: Option<String>,
}

/// Default period of `GET /stats` and `mcpdoze stats`.
pub const DEFAULT_STATS_SINCE: &str = "7d";

/// `GET /stats` response (see `docs/STATS.md`).
pub use crate::stats::Summary as StatsResponse;

/// `GET /servers/{name}/logs` query.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LogsQuery {
    /// Most recent lines to return (default 100, capped at the buffer size).
    pub lines: Option<usize>,
}

/// `GET /servers/{name}/logs` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogsResponse {
    /// Server name.
    pub name: String,
    /// Last stderr lines, oldest first.
    pub lines: Vec<String>,
}

/// Runtime debug overrides in [`DebugStatus`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugOverride {
    /// Mode for every server, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<String>,
    /// Modes of named servers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, String>,
}

/// `GET /debug` and `POST /debug` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DebugStatus {
    /// Effective mode for servers without a per-server setting.
    pub mode: String,
    /// Effective mode of every registered server.
    pub servers: BTreeMap<String, String>,
    /// The runtime layer (set by `POST /debug`, lost on restart).
    pub runtime: DebugOverride,
    /// Raw frame capture is on. Always false in this build.
    pub raw: bool,
    /// Seconds until the runtime layer reverts (when `for` was given).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
    /// False when no debug directory can be determined (everything stays
    /// off).
    pub available: bool,
    /// Records written to the failure log by this process.
    pub written: u64,
    /// Records dropped (queue full) by this process.
    pub dropped: u64,
}

/// `POST /debug` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugSetRequest {
    /// `off`, `failures` or `trace`.
    pub mode: String,
    /// Apply to these servers only (default: every server).
    #[serde(default)]
    pub servers: Vec<String>,
    /// Raw capture is not available; `true` is rejected.
    #[serde(default)]
    pub raw: bool,
    /// Revert after this duration (`30m`); default: until restart.
    #[serde(default, rename = "for")]
    pub duration: Option<String>,
}

/// `GET /debug/failures` query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailuresQuery {
    /// Only this server.
    pub server: Option<String>,
    /// Only this kind (snake case).
    pub kind: Option<String>,
    /// Only records newer than this: a duration ago (`1h`) or unix ms.
    pub since: Option<String>,
    /// Newest records to return (default 50, at most 500).
    pub limit: Option<usize>,
}

/// `GET /debug/failures` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailuresResponse {
    /// Matching records, oldest first (the schema of `docs/DEBUG.md`).
    pub records: Vec<serde_json::Value>,
}

/// Error body of every non-2xx admin response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Short description.
    pub error: String,
    /// Details such as validation errors (`field: message`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}
