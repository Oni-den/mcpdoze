// SPDX-License-Identifier: GPL-3.0-or-later
//! Migration of agent MCP configurations onto the broker (`mcpdoze import`).
//!
//! The module is pure data plus local file I/O; it never touches the network.
//!
//! * `parse`: read an agent's config files into raw server entries.
//! * `plan`: turn entries into a [`Plan`] (names, prefixes, scopes, secret
//!   handling). Planning never modifies anything.
//! * `apply`: back up, write a manifest, register servers in the broker
//!   config, rewrite the agent config; and [`rollback`].
//!
//! # Secrets
//!
//! Literal secrets are never copied: the broker entry gets a `from_env`
//! reference to a suggested variable and the plan records `needs_env_var`
//! (key name only). The broker bearer token is never written anywhere; agent
//! entries reference it through an environment variable
//! ([`DEFAULT_TOKEN_ENV`]) using each agent's own expansion syntax.
//!
//! # Agent formats handled
//!
//! | Agent | Files | Shape |
//! |---|---|---|
//! | Claude Code | `~/.claude.json` (`mcpServers`, `projects.<path>.mcpServers`), `./.mcp.json` | `type` stdio/http/sse, `command/args/env`, `url/headers` |
//! | Codex | `$CODEX_HOME/config.toml` | `[mcp_servers.<n>]` `command/args/env`, `url`, `http_headers`, `env_http_headers`, `bearer_token_env_var` |
//! | OpenCode | `<xdg config>/opencode/opencode.json`, `./opencode.json` | `mcp` map, `type` local/remote, `command[]`, `environment`, `url`, `headers` |
//! | Cursor | `~/.cursor/mcp.json`, `./.cursor/mcp.json` | `mcpServers` (also `servers`) |
//! | VS Code | `<user config>/Code/User/mcp.json`, `./.vscode/mcp.json` | `servers` (also `mcpServers`) |

mod apply;
mod parse;
mod plan;
mod util;

pub use apply::{
    apply, rollback, ApplyOptions, ApplyResult, JsonEdit, Manifest, ManifestFile, RollbackResult,
    SavedEntry, REDACTED,
};
pub use parse::{
    agent_files, parse_file, AgentFile, FileRole, ParsedFile, RawServer, RawSpec, RawValue,
};
pub use plan::{build_plan, NeedsEnvVar, Plan, PlanItem, Skipped, Source};
pub use util::{sha256_hex, utc_timestamp};

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Environment variable agents use to carry the broker token.
pub const DEFAULT_TOKEN_ENV: &str = "MCPDOZE_TOKEN";
/// Name of the single entry written into agent configs.
pub const BROKER_ENTRY_NAME: &str = "mcpdoze";
/// Infix of backup files (followed by a UTC timestamp).
pub const BACKUP_INFIX: &str = ".mcpdoze-bak-";

/// Supported agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Agent {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "opencode")]
    OpenCode,
    #[serde(rename = "cursor")]
    Cursor,
    #[serde(rename = "vscode")]
    VsCode,
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
            Agent::OpenCode => "opencode",
            Agent::Cursor => "cursor",
            Agent::VsCode => "vscode",
        }
    }
}

impl FromStr for Agent {
    type Err = ImportError;
    fn from_str(s: &str) -> Result<Self, ImportError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude-code" | "claude" => Ok(Agent::ClaudeCode),
            "codex" => Ok(Agent::Codex),
            "opencode" => Ok(Agent::OpenCode),
            "cursor" => Ok(Agent::Cursor),
            "vscode" | "vs-code" => Ok(Agent::VsCode),
            other => Err(ImportError::Invalid(format!(
                "unknown agent `{other}` (use claude-code, codex, opencode, cursor or vscode)"
            ))),
        }
    }
}

/// Import errors. Messages never contain secret values.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("{path}: cannot parse: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("rollback refused: file(s) changed since the import: {}", .0.join(", "))]
    Changed(Vec<String>),
    #[error("rollback refused: this manifest was already rolled back")]
    AlreadyRolledBack,
}

impl ImportError {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        ImportError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Where things live. All paths are explicit so tests can use temp dirs.
#[derive(Debug, Clone)]
pub struct ImportEnv {
    pub home: PathBuf,
    /// Current directory (project-scoped configs live here).
    pub cwd: PathBuf,
    /// Platform user config dir (VS Code keeps `Code/User` below it).
    pub config_dir: PathBuf,
    /// `$XDG_CONFIG_HOME` or `<home>/.config` (OpenCode).
    pub xdg_config: PathBuf,
    /// `$CODEX_HOME` or `<home>/.codex`.
    pub codex_home: PathBuf,
    /// Broker data dir; manifests go to `<data_dir>/import/`.
    pub data_dir: PathBuf,
    /// Broker config file to register servers in.
    pub broker_config: PathBuf,
    /// Broker MCP endpoint, e.g. `http://127.0.0.1:7878/mcp`.
    pub broker_url: String,
    /// Environment variable that will hold the broker token.
    pub token_env_var: String,
}

impl ImportEnv {
    /// Detect from the real environment.
    pub fn detect(broker_url: &str, broker_config: Option<PathBuf>) -> Result<Self, ImportError> {
        let base = directories::BaseDirs::new()
            .ok_or_else(|| ImportError::Invalid("cannot determine the home directory".into()))?;
        let home = base.home_dir().to_path_buf();
        let cwd = std::env::current_dir().map_err(|e| {
            ImportError::Invalid(format!("cannot determine current directory: {e}"))
        })?;
        let non_empty = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let xdg_config = non_empty("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let codex_home = non_empty("CODEX_HOME").unwrap_or_else(|| home.join(".codex"));
        let data_dir = crate::config::default_data_dir()
            .ok_or_else(|| ImportError::Invalid("cannot determine the data directory".into()))?;
        let broker_config = match broker_config {
            Some(p) => p,
            None => crate::config::default_config_path()
                .ok_or_else(|| ImportError::Invalid("cannot determine the config path".into()))?,
        };
        Ok(Self {
            config_dir: base.config_dir().to_path_buf(),
            home,
            cwd,
            xdg_config,
            codex_home,
            data_dir,
            broker_config,
            broker_url: broker_url.to_string(),
            token_env_var: DEFAULT_TOKEN_ENV.to_string(),
        })
    }
}

#[cfg(test)]
mod tests;
