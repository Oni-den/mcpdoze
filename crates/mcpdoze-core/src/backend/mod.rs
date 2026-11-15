// SPDX-License-Identifier: GPL-3.0-or-later

//! Backends: MCP servers the broker launches and talks to on behalf of agents.
//!
//! * [`stdio`]: one spawned process, newline-delimited JSON-RPC on its
//!   stdin/stdout, request-id remapping and the broker's own `initialize`.
//! * [`lifecycle`]: the per-instance state machine (lazy start, idle TTL,
//!   crash backoff) that owns at most one [`stdio`] connection at a time.

pub mod jsonrpc;
pub mod lifecycle;
pub mod stdio;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

pub use lifecycle::{CallGuard, Instance, StateKind, Tuning};
pub use stdio::{CallError, CallMeta};

/// Default idle time before an unused backend is unloaded.
pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(600);
/// Default time a backend gets to spawn and answer `initialize`.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(60);

/// How to launch one stdio backend.
///
/// Deliberately independent of the configuration module: the caller resolves
/// secret references and passes final values. `Debug` never prints `env`
/// values.
#[derive(Clone, PartialEq, Eq)]
pub struct BackendSpec {
    /// Program to run (looked up on `PATH`; `.cmd` shims handled on Windows).
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Extra environment variables, already resolved. Added on top of the
    /// broker's inherited environment.
    pub env: BTreeMap<String, String>,
    /// Working directory; the broker's when `None`.
    pub cwd: Option<PathBuf>,
    /// Unload after this long with no call in flight.
    pub idle_ttl: Duration,
    /// Spawn plus `initialize` must finish within this.
    pub start_timeout: Duration,
}

impl BackendSpec {
    /// A spec with default TTL and start timeout, no extra env, no cwd.
    pub fn new<I, S>(command: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
            cwd: None,
            idle_ttl: DEFAULT_IDLE_TTL,
            start_timeout: DEFAULT_START_TIMEOUT,
        }
    }

    /// Sets the idle TTL.
    pub fn idle_ttl(mut self, ttl: Duration) -> Self {
        self.idle_ttl = ttl;
        self
    }

    /// Sets the start timeout.
    pub fn start_timeout(mut self, t: Duration) -> Self {
        self.start_timeout = t;
        self
    }
}

impl fmt::Debug for BackendSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendSpec")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("cwd", &self.cwd)
            .field("idle_ttl", &self.idle_ttl)
            .field("start_timeout", &self.start_timeout)
            .finish()
    }
}

/// Hooks the backend reader calls for messages that are not responses to
/// broker requests. The router implements this.
pub trait BackendEvents: Send + Sync + 'static {
    /// A notification from backend `server` (progress, list_changed, log).
    fn notification(&self, server: &str, msg: Value) {
        let _ = (server, msg);
    }

    /// A server-to-client request (sampling, elicitation, roots) from
    /// backend `server`. Return the full JSON-RPC response to send back, or
    /// `None` to answer "method not supported".
    ///
    /// Hook for M7: route to the session that owns the in-flight call.
    fn server_request(&self, server: &str, msg: &Value) -> Option<Value> {
        let _ = (server, msg);
        None
    }
}

/// Events sink that ignores everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoEvents;

impl BackendEvents for NoEvents {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_env_values() {
        let mut s = BackendSpec::new("prog", ["a"]);
        s.env.insert("EXAMPLE_KEY".into(), "hidden-value".into());
        let d = format!("{s:?}");
        assert!(d.contains("EXAMPLE_KEY"));
        assert!(!d.contains("hidden-value"));
    }
}
