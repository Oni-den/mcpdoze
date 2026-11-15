// SPDX-License-Identifier: GPL-3.0-or-later

//! Admin API wire types (`/admin/v1`), shared by the broker and CLI clients.
//!
//! Every type is plain JSON via serde; see `docs/ADMIN_API.md` for the
//! endpoints. Unknown response fields must be ignored by clients so the API
//! can grow without a version bump.

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

/// Error body of every non-2xx admin response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Short description.
    pub error: String,
    /// Details such as validation errors (`field: message`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}
