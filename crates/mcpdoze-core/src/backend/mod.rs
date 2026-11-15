// SPDX-License-Identifier: GPL-3.0-or-later

//! Backends: MCP servers the broker launches and talks to on behalf of agents.
//!
//! * [`stdio`]: one spawned process, newline-delimited JSON-RPC on its
//!   stdin/stdout, request-id remapping and the broker's own `initialize`.
//! * [`lifecycle`]: the per-instance state machine (lazy start, idle TTL,
//!   crash backoff) that owns at most one [`stdio`] connection at a time.

pub mod http;
pub mod jsonrpc;
pub mod lifecycle;
pub mod stdio;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

pub use lifecycle::{CallGuard, Instance, StateKind, Tuning};
pub use stdio::{CallError, CallMeta, StdioConn};

/// Default per-request timeout for HTTP backends.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Upstream Streamable HTTP transport settings of a [`BackendSpec`].
#[derive(Clone, PartialEq, Eq)]
pub struct HttpSpec {
    /// Upstream MCP endpoint URL.
    pub url: String,
    /// Extra request headers, already resolved. Values are never logged.
    pub headers: BTreeMap<String, String>,
    /// Timeout for one upstream request (including a streamed response).
    pub request_timeout: Duration,
}

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
    /// When set, this backend is a remote Streamable HTTP server and
    /// `command`/`args`/`env`/`cwd` are unused.
    pub http: Option<HttpSpec>,
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
            http: None,
        }
    }

    /// A remote Streamable HTTP backend at `url` with default TTL, start and
    /// request timeouts and no extra headers.
    pub fn http(url: impl Into<String>) -> Self {
        let mut s = Self::new(String::new(), Vec::<String>::new());
        s.http = Some(HttpSpec {
            url: url.into(),
            headers: BTreeMap::new(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        });
        s
    }

    /// Adds a resolved request header (HTTP backends only; no-op otherwise).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        if let Some(h) = &mut self.http {
            h.headers.insert(name.into(), value.into());
        }
        self
    }

    /// Sets the per-request timeout (HTTP backends only; no-op otherwise).
    pub fn request_timeout(mut self, t: Duration) -> Self {
        if let Some(h) = &mut self.http {
            h.request_timeout = t;
        }
        self
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
            .field("http", &self.http)
            .finish()
    }
}

impl fmt::Debug for HttpSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpSpec")
            .field("url", &"<redacted>")
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

type CallResult = Result<Value, CallError>;

/// A live connection to one backend, whatever the transport. The lifecycle
/// and router only use this type.
#[derive(Debug)]
pub enum Conn {
    /// Spawned local process.
    Stdio(Box<StdioConn>),
    /// Upstream Streamable HTTP session.
    Http(Arc<http::HttpConn>),
}

impl Conn {
    /// Lifecycle generation.
    pub fn generation(&self) -> u64 {
        match self {
            Conn::Stdio(c) => c.generation(),
            Conn::Http(c) => c.generation(),
        }
    }

    /// OS pid of the backend root process; `None` for HTTP backends.
    pub fn pid(&self) -> Option<u32> {
        match self {
            Conn::Stdio(c) => Some(c.pid()),
            Conn::Http(_) => None,
        }
    }

    /// When the connection was established.
    pub fn started(&self) -> Instant {
        match self {
            Conn::Stdio(c) => c.started(),
            Conn::Http(c) => c.started(),
        }
    }

    /// The backend's `initialize` result.
    pub fn init_result(&self) -> Value {
        match self {
            Conn::Stdio(c) => c.init_result(),
            Conn::Http(c) => c.init_result(),
        }
    }

    /// Calls awaiting a response.
    pub fn pending_len(&self) -> usize {
        match self {
            Conn::Stdio(c) => c.pending_len(),
            Conn::Http(c) => c.pending_len(),
        }
    }

    pub(crate) async fn initialize(&self) -> Result<Value, String> {
        match self {
            Conn::Stdio(c) => c.initialize().await,
            Conn::Http(c) => c.initialize().await,
        }
    }

    pub(crate) fn closed(&self) -> watch::Receiver<bool> {
        match self {
            Conn::Stdio(c) => c.closed(),
            Conn::Http(c) => c.closed(),
        }
    }

    /// Sends a request with a fresh broker id.
    pub async fn begin(
        &self,
        method: &str,
        params: Value,
        meta: CallMeta,
    ) -> Result<(u64, oneshot::Receiver<CallResult>), CallError> {
        match self {
            Conn::Stdio(c) => c.begin(method, params, meta).await,
            Conn::Http(c) => c.begin(method, params, meta).await,
        }
    }

    /// Sends a request and waits for its response.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        meta: CallMeta,
    ) -> Result<Value, CallError> {
        match self {
            Conn::Stdio(c) => c.call(method, params, meta).await,
            Conn::Http(c) => c.call(method, params, meta).await,
        }
    }

    /// Sends a notification.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), CallError> {
        match self {
            Conn::Stdio(c) => c.notify(method, params).await,
            Conn::Http(c) => c.notify(method, params).await,
        }
    }

    /// Cancels pending call `id` and tells the backend.
    pub fn cancel(&self, id: u64, reason: &str) {
        match self {
            Conn::Stdio(c) => c.cancel(id, reason),
            Conn::Http(c) => c.cancel(id, reason),
        }
    }

    pub(crate) async fn shutdown(&self, grace: Duration) {
        match self {
            Conn::Stdio(c) => c.shutdown(grace).await,
            Conn::Http(c) => c.shutdown(grace).await,
        }
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
