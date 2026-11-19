// SPDX-License-Identifier: GPL-3.0-or-later

//! Backends: MCP servers the broker launches and talks to on behalf of agents.
//!
//! * [`stdio`]: one spawned process, newline-delimited JSON-RPC on its
//!   stdin/stdout, request-id remapping and the broker's own `initialize`.
//! * [`lifecycle`]: the per-instance state machine (lazy start, idle TTL,
//!   crash backoff) that owns at most one [`stdio`] connection at a time.

pub mod headers_helper;
pub mod http;
pub mod jsonrpc;
pub mod lifecycle;
pub mod stdio;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

pub use crate::config::SubscriptionPolicy;
pub use lifecycle::{
    CallGuard, Instance, LifecycleEvent, LifecycleHook, SpawnHook, StateKind, Tuning, UnloadReason,
};
pub use stdio::{CallError, CallMeta, StdioConn, WireCounts};

/// Default per-request timeout for HTTP backends.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Upstream Streamable HTTP transport settings of a [`BackendSpec`].
#[derive(Clone, PartialEq, Eq)]
pub struct HttpSpec {
    /// Upstream MCP endpoint URL.
    pub url: String,
    /// Extra request headers, already resolved. Values are never logged.
    pub headers: BTreeMap<String, String>,
    /// Shell command whose JSON-object output adds or overrides headers
    /// (see [`headers_helper`]). Run on connect, re-initialise and once
    /// after a 401/403. Its output is never logged.
    pub headers_helper: Option<String>,
    /// Timeout for one upstream request (including a streamed response).
    pub request_timeout: Duration,
}

/// Placeholder expanded to the agent's working directory in scoped specs.
pub const CWD_VAR: &str = "${cwd}";

/// Default idle time before an unused backend is unloaded.
pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(600);
/// Default time a backend gets to spawn and answer `initialize`.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(60);

/// What the broker offers a backend as its (single, synthetic) client, and
/// how it answers the backend's server-to-client requests. Not part of the
/// catalog fingerprint.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ClientSide {
    /// Advertise the `sampling` capability (only where the owning agent
    /// session is unambiguous: per-session instances).
    pub sampling: bool,
    /// Advertise the `elicitation` capability (same rule as `sampling`).
    pub elicitation: bool,
    /// The root answered for `roots/list` (`file://` URI); an empty list
    /// when `None`. The broker answers `roots/list` itself, always.
    pub root: Option<String>,
    /// The agent session that owns this instance (per-session scope).
    /// Server-to-client requests go there even with no call in flight.
    pub owner: Option<String>,
    /// Scope key of the instance (`""` for the shared instance, the session
    /// id for `per-session`, the normalised directory for `per-cwd`).
    /// Handed to [`BackendEvents`] with every notification.
    pub scope_key: String,
}

impl ClientSide {
    /// The `capabilities` object of the broker's `initialize` request.
    pub fn capabilities(&self) -> Value {
        let mut caps = serde_json::Map::new();
        caps.insert("roots".into(), serde_json::json!({"listChanged": false}));
        if self.sampling {
            caps.insert("sampling".into(), serde_json::json!({}));
        }
        if self.elicitation {
            caps.insert("elicitation".into(), serde_json::json!({}));
        }
        Value::Object(caps)
    }

    /// The `roots/list` result.
    pub fn roots_result(&self) -> Value {
        let roots: Vec<Value> = self
            .root
            .iter()
            .map(|u| {
                let name = u
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                serde_json::json!({"uri": u, "name": name})
            })
            .collect();
        serde_json::json!({ "roots": roots })
    }
}

impl fmt::Debug for ClientSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientSide")
            .field("sampling", &self.sampling)
            .field("elicitation", &self.elicitation)
            .field("root", &self.root)
            .field("owned", &self.owner.is_some())
            .field("scoped", &!self.scope_key.is_empty())
            .finish()
    }
}

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
    /// Names of `env` entries resolved from `{ from_env = ... }` references.
    /// `${cwd}` is never expanded inside them.
    pub ref_env: BTreeSet<String>,
    /// Client capabilities and server-to-client answers for this instance.
    pub client: ClientSide,
    /// Inherited variables removed from the backend's environment (the
    /// broker token). An entry of the same name in `env`, which the user
    /// configured explicitly, is still passed.
    pub strip_env: BTreeSet<String>,
    /// Idle behaviour while the instance holds resource subscriptions.
    pub subscriptions: SubscriptionPolicy,
}

/// Variable agents carry the broker token in; never inherited by backends.
pub const TOKEN_ENV_VAR: &str = "MCPDOZE_TOKEN";

impl BackendSpec {
    /// A spec with default TTL and start timeout, no extra env, no cwd. The
    /// broker token variable [`TOKEN_ENV_VAR`] is stripped.
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
            ref_env: BTreeSet::new(),
            client: ClientSide::default(),
            strip_env: BTreeSet::from([TOKEN_ENV_VAR.to_string()]),
            subscriptions: SubscriptionPolicy::default(),
        }
    }

    /// This spec with `${cwd}` replaced by `cwd` in the args, the literal
    /// env values (never in `from_env` values) and the configured working
    /// directory. With `spawn_in` the process is started in `cwd` unless a
    /// working directory is configured.
    pub fn expand_cwd(&self, cwd: &std::path::Path, spawn_in: bool) -> Self {
        let c = cwd.to_string_lossy();
        let mut s = self.clone();
        for a in &mut s.args {
            *a = a.replace(CWD_VAR, &c);
        }
        for (k, v) in &mut s.env {
            if !self.ref_env.contains(k) {
                *v = v.replace(CWD_VAR, &c);
            }
        }
        s.cwd = match &self.cwd {
            Some(p) => Some(PathBuf::from(p.to_string_lossy().replace(CWD_VAR, &c))),
            None if spawn_in => Some(cwd.to_path_buf()),
            None => None,
        };
        s
    }

    /// A remote Streamable HTTP backend at `url` with default TTL, start and
    /// request timeouts and no extra headers.
    pub fn http(url: impl Into<String>) -> Self {
        let mut s = Self::new(String::new(), Vec::<String>::new());
        s.http = Some(HttpSpec {
            url: url.into(),
            headers: BTreeMap::new(),
            headers_helper: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        });
        s
    }

    /// Sets the headers helper command (HTTP backends only; no-op otherwise).
    pub fn headers_helper(mut self, command: impl Into<String>) -> Self {
        if let Some(h) = &mut self.http {
            h.headers_helper = Some(command.into());
        }
        self
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
            .field("client", &self.client)
            .finish()
    }
}

impl fmt::Debug for HttpSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpSpec")
            .field("url", &"<redacted>")
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("headers_helper", &self.headers_helper.is_some())
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

    /// HTTP backends: keep an upstream `GET` event stream open while `on`,
    /// so server-initiated notifications (resource updates) arrive. Stdio
    /// backends deliver those on stdout anyway; no-op there.
    pub(crate) fn set_listening(&self, on: bool) {
        if let Conn::Http(c) = self {
            c.set_listening(on);
        }
    }

    pub(crate) async fn shutdown(&self, grace: Duration) {
        match self {
            Conn::Stdio(c) => c.shutdown(grace).await,
            Conn::Http(c) => c.shutdown(grace).await,
        }
    }
}

/// Sends the broker's answer to one server-to-client request back to the
/// backend that asked.
pub struct Reply {
    id: Value,
    send: Box<dyn FnOnce(Value) + Send>,
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply").field("id", &self.id).finish()
    }
}

impl Reply {
    pub(crate) fn new(id: Value, send: impl FnOnce(Value) + Send + 'static) -> Self {
        Self {
            id,
            send: Box::new(send),
        }
    }

    /// The backend's request id.
    pub fn id(&self) -> &Value {
        &self.id
    }

    /// Sends a success result.
    pub fn result(self, result: Value) {
        let msg = jsonrpc::result(self.id.clone(), result);
        (self.send)(msg);
    }

    /// Sends an error.
    pub fn error(self, code: i64, message: impl Into<String>) {
        let msg = jsonrpc::error(self.id.clone(), code, message);
        (self.send)(msg);
    }

    /// Sends an agent's response (`result` or `error`) under the backend's id.
    pub fn forward(self, response: Value) {
        let msg = if let Some(r) = response.get("result") {
            jsonrpc::result(self.id.clone(), r.clone())
        } else if let Some(e) = response.get("error").filter(|e| e.is_object()) {
            serde_json::json!({"jsonrpc": "2.0", "id": self.id.clone(), "error": e})
        } else {
            jsonrpc::error(
                self.id.clone(),
                jsonrpc::INTERNAL_ERROR,
                "agent sent an invalid response",
            )
        };
        (self.send)(msg);
    }
}

/// A server-to-client request (sampling, elicitation, ...) from a backend
/// that the broker does not answer itself (`ping` and `roots/list` are
/// answered locally).
#[derive(Debug)]
pub struct ServerRequest {
    /// Backend server name.
    pub server: String,
    /// The request as the backend sent it.
    pub msg: Value,
    /// Distinct agent sessions with a call in flight on this connection.
    pub sessions: Vec<String>,
    /// The session owning the instance (per-session scope), if any.
    pub owner: Option<String>,
    /// Where the answer goes.
    pub reply: Reply,
}

/// Hooks the backend reader calls for messages that are not responses to
/// broker requests. The router implements this.
pub trait BackendEvents: Send + Sync + 'static {
    /// A notification from backend `server` (progress, list_changed, log),
    /// from the instance with scope key `scope_key` (see
    /// [`ClientSide::scope_key`]).
    fn notification(&self, server: &str, scope_key: &str, msg: Value) {
        let _ = (server, scope_key, msg);
    }

    /// A notification plus the agent sessions with calls in flight on the
    /// connection it arrived on. Defaults to [`notification`](Self::notification).
    fn notification_from(&self, server: &str, scope_key: &str, msg: Value, sessions: Vec<String>) {
        let _ = sessions;
        self.notification(server, scope_key, msg);
    }

    /// A server-to-client request the broker does not answer itself. The
    /// default answers "method not supported".
    fn server_request(&self, req: ServerRequest) {
        req.reply.error(
            jsonrpc::METHOD_NOT_FOUND,
            "method not supported by the mcpdoze broker",
        );
    }
}

/// Answers `ping` and `roots/list` locally and hands everything else to
/// `events`. Shared by the stdio and HTTP connections.
pub(crate) fn handle_server_request(
    events: &dyn BackendEvents,
    server: &str,
    client: &ClientSide,
    msg: Value,
    sessions: Vec<String>,
    reply: Reply,
) {
    match msg["method"].as_str().unwrap_or("") {
        "ping" => reply.result(serde_json::json!({})),
        "roots/list" => reply.result(client.roots_result()),
        _ => events.server_request(ServerRequest {
            server: server.to_owned(),
            msg,
            sessions,
            owner: client.owner.clone(),
            reply,
        }),
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
