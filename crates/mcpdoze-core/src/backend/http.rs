// SPDX-License-Identifier: GPL-3.0-or-later

//! Upstream Streamable HTTP MCP backend (HTTP passthrough, milestone M6).
//!
//! One [`HttpConn`] is the broker's single upstream session with a remote MCP
//! server: it performs the broker's own `initialize` handshake, keeps the
//! upstream `Mcp-Session-Id`, and POSTs JSON-RPC messages that agents send
//! through the broker. Request ids are remapped exactly like the stdio path.
//!
//! * Responses may be plain JSON or `text/event-stream`. SSE is parsed
//!   incrementally from response chunks (never buffering the whole body); the
//!   event whose id matches the request completes the call, other events are
//!   forwarded to [`BackendEvents`].
//! * An upstream `404` for a known session, or a dropped connection, makes the
//!   connection re-initialise (with short exponential backoff for transport
//!   errors) and retry the call once. If reconnecting fails the connection
//!   reports itself closed so the lifecycle enters its normal backoff.
//! * Configured headers are applied to every request. They are marked
//!   sensitive and never logged; errors are stripped of URLs.
//! * A `headers_helper` (see [`super::headers_helper`]) runs before every
//!   `initialize` and once more after a 401/403 on a call; its headers
//!   override the static ones.
//! * One shared [`reqwest::Client`] serves every HTTP backend.

#[path = "http_listen.rs"]
mod listen;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::headers_helper;
use super::jsonrpc::{self, Kind};
use super::stdio::{lock, BACKEND_PROTOCOL_VERSION};
use super::{
    handle_server_request, BackendEvents, BackendSpec, CallError, CallMeta, ClientSide, Reply,
};

/// Largest upstream response body accepted (JSON body or whole SSE stream).
pub const MAX_RESPONSE: usize = 16 * 1024 * 1024;
/// Delays between reconnect attempts after a transport failure.
const RECONNECT_DELAYS_MS: [u64; 4] = [0, 100, 200, 400];

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Some(Duration::from_secs(60)))
            .build()
            .unwrap_or_default()
    })
}

type CallResult = Result<Value, CallError>;

struct Pending {
    tx: oneshot::Sender<CallResult>,
    meta: CallMeta,
    abort: Option<AbortHandle>,
}

#[derive(Default)]
struct PendingMap {
    calls: HashMap<u64, Pending>,
    closed: bool,
}

impl PendingMap {
    fn sessions(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .calls
            .values()
            .filter_map(|p| p.meta.session.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    protocol: Option<String>,
    epoch: u64,
}

enum XErr {
    SessionGone,
    Transport(String),
    Status(u16),
    Other(String),
}

impl XErr {
    fn into_call(self) -> CallError {
        CallError::Unavailable(match self {
            XErr::SessionGone => "upstream session lost".into(),
            XErr::Transport(m) => format!("upstream connection failed: {m}"),
            XErr::Status(401 | 403) => {
                "upstream rejected the request (HTTP 401/403); check the configured headers".into()
            }
            XErr::Status(c) => format!("upstream returned HTTP {c}"),
            XErr::Other(m) => m,
        })
    }
}

struct Exchange {
    response: Option<Value>,
    session: Option<String>,
}

/// A live upstream Streamable HTTP session.
pub struct HttpConn {
    server: String,
    generation: u64,
    url: String,
    /// Configured (static) headers.
    static_headers: Vec<(HeaderName, HeaderValue)>,
    /// `headers_helper` command, if any.
    helper: Option<String>,
    /// Headers sent: static ones overridden by the helper's latest output.
    headers: Mutex<Vec<(HeaderName, HeaderValue)>>,
    timeout: Duration,
    events: Arc<dyn BackendEvents>,
    started: Instant,
    session: Mutex<Session>,
    init_lock: tokio::sync::Mutex<()>,
    pending: Mutex<PendingMap>,
    next_id: AtomicU64,
    closed_tx: watch::Sender<bool>,
    closed: watch::Receiver<bool>,
    init: Mutex<Value>,
    client: ClientSide,
    /// The upstream `GET` event stream task (see `http_listen.rs`).
    listen: Mutex<Option<AbortHandle>>,
}

impl std::fmt::Debug for HttpConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpConn")
            .field("server", &self.server)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl HttpConn {
    /// Prepares a connection (no network traffic until [`Self::initialize`]).
    pub(crate) fn new(
        server: &str,
        generation: u64,
        spec: &BackendSpec,
        events: Arc<dyn BackendEvents>,
    ) -> Result<Arc<Self>, String> {
        let http = spec.http.as_ref().ok_or("not an http backend")?;
        let mut headers = Vec::new();
        for (k, v) in &http.headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| format!("invalid header name {k:?}"))?;
            let mut value =
                HeaderValue::from_str(v).map_err(|_| format!("invalid value for header {k:?}"))?;
            value.set_sensitive(true);
            headers.push((name, value));
        }
        let (closed_tx, closed) = watch::channel(false);
        Ok(Arc::new(Self {
            server: server.to_owned(),
            generation,
            url: http.url.clone(),
            headers: Mutex::new(headers.clone()),
            static_headers: headers,
            helper: http.headers_helper.clone(),
            timeout: http.request_timeout,
            events,
            started: Instant::now(),
            session: Mutex::new(Session::default()),
            init_lock: tokio::sync::Mutex::new(()),
            pending: Mutex::new(PendingMap::default()),
            next_id: AtomicU64::new(1),
            closed_tx,
            closed,
            init: Mutex::new(Value::Null),
            client: spec.client.clone(),
            listen: Mutex::new(None),
        }))
    }

    /// Lifecycle generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// When this connection object was created.
    pub fn started(&self) -> Instant {
        self.started
    }

    /// The upstream `initialize` result (`Null` before the handshake).
    pub fn init_result(&self) -> Value {
        lock(&self.init).clone()
    }

    /// Calls awaiting an upstream response.
    pub fn pending_len(&self) -> usize {
        lock(&self.pending).calls.len()
    }

    pub(crate) fn closed(&self) -> watch::Receiver<bool> {
        self.closed.clone()
    }

    fn snapshot(&self) -> (Option<String>, u64) {
        let s = lock(&self.session);
        (s.id.clone(), s.epoch)
    }

    /// The broker's own `initialize` handshake with the upstream.
    pub(crate) async fn initialize(self: &Arc<Self>) -> Result<Value, String> {
        let _g = self.init_lock.lock().await;
        self.do_init().await
    }

    /// Re-runs the headers helper (no-op without one) and installs its
    /// headers over the static ones. The output is never logged.
    async fn refresh_headers(&self) -> Result<(), String> {
        let Some(cmd) = &self.helper else {
            return Ok(());
        };
        match headers_helper::run(cmd).await {
            Ok(extra) => {
                tracing::debug!(server = %self.server, count = extra.len(), "headers helper ran");
                *lock(&self.headers) = headers_helper::merge(&self.static_headers, extra);
                Ok(())
            }
            Err(e) => {
                tracing::warn!(server = %self.server, error = %e, "headers helper failed");
                Err(e.to_string())
            }
        }
    }

    fn current_headers(&self) -> Vec<(HeaderName, HeaderValue)> {
        lock(&self.headers).clone()
    }

    async fn do_init(self: &Arc<Self>) -> Result<Value, String> {
        self.refresh_headers().await?;
        {
            let mut s = lock(&self.session);
            s.id = None;
            s.protocol = None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let params = json!({
            "protocolVersion": BACKEND_PROTOCOL_VERSION,
            "capabilities": self.client.capabilities(),
            "clientInfo": {"name": "mcpdoze", "version": env!("CARGO_PKG_VERSION")},
        });
        let body = jsonrpc::request(id, "initialize", params);
        let ex = self
            .exchange(&body, Some(id), None, None)
            .await
            .map_err(|e| e.into_call().to_string())?;
        let resp = ex.response.ok_or("initialize returned no response")?;
        let Some(result) = resp.get("result").cloned() else {
            return Err(format!(
                "initialize failed: {}",
                resp.get("error").cloned().unwrap_or(Value::Null)
            ));
        };
        let protocol = result["protocolVersion"].as_str().map(str::to_owned);
        {
            let mut s = lock(&self.session);
            s.id = ex.session;
            s.protocol = protocol;
            s.epoch += 1;
        }
        self.notify("notifications/initialized", json!({}))
            .await
            .map_err(|e| e.to_string())?;
        *lock(&self.init) = result.clone();
        Ok(result)
    }

    /// Re-initialises unless another task already did since `epoch`.
    async fn reinit(self: &Arc<Self>, epoch: u64) -> Result<(), String> {
        let _g = self.init_lock.lock().await;
        if lock(&self.session).epoch != epoch {
            return Ok(());
        }
        tracing::info!(server = %self.server, "re-initialising upstream session");
        self.do_init().await.map(|_| ())
    }

    async fn reconnect(self: &Arc<Self>, epoch: u64) -> Result<(), CallError> {
        let mut last = String::new();
        for ms in RECONNECT_DELAYS_MS {
            if ms > 0 {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            match self.reinit(epoch).await {
                Ok(()) => return Ok(()),
                Err(e) => last = e,
            }
        }
        self.mark_closed();
        Err(CallError::Unavailable(last))
    }

    fn mark_closed(&self) {
        if let Some(h) = lock(&self.listen).take() {
            h.abort();
        }
        fail_all(&self.pending);
        let _ = self.closed_tx.send(true);
    }

    /// One POST. `want` is the request id whose response completes the call.
    async fn exchange(
        self: &Arc<Self>,
        body: &Value,
        want: Option<u64>,
        session: Option<String>,
        protocol: Option<String>,
    ) -> Result<Exchange, XErr> {
        let mut req = client()
            .post(&self.url)
            .timeout(self.timeout)
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json");
        for (k, v) in self.current_headers() {
            req = req.header(k, v);
        }
        let had_session = session.is_some();
        if let Some(s) = session {
            req = req.header("Mcp-Session-Id", s);
        }
        if let Some(p) = protocol {
            req = req.header("MCP-Protocol-Version", p);
        }
        let mut resp = req.body(body.to_string()).send().await.map_err(map_err)?;
        let status = resp.status();
        if status.as_u16() == 404 && had_session {
            return Err(XErr::SessionGone);
        }
        if !status.is_success() {
            return Err(XErr::Status(status.as_u16()));
        }
        let session_hdr = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let is_sse = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
        let mut found = None;
        let mut total = 0usize;
        if is_sse {
            let mut parser = SseParser::default();
            'outer: while let Some(chunk) = resp.chunk().await.map_err(map_err)? {
                total += chunk.len();
                if total > MAX_RESPONSE {
                    return Err(XErr::Other("upstream response over 16 MiB".into()));
                }
                for data in parser.feed(&chunk) {
                    if let Ok(msg) = serde_json::from_str::<Value>(&data) {
                        if let Some(r) = self.dispatch(msg, want) {
                            found = Some(r);
                            break 'outer;
                        }
                    }
                }
            }
        } else {
            let mut buf = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(map_err)? {
                total += chunk.len();
                if total > MAX_RESPONSE {
                    return Err(XErr::Other("upstream response over 16 MiB".into()));
                }
                buf.extend_from_slice(&chunk);
            }
            if !buf.iter().all(u8::is_ascii_whitespace) {
                let v: Value = serde_json::from_slice(&buf)
                    .map_err(|_| XErr::Other("upstream sent invalid JSON".into()))?;
                let msgs = match v {
                    Value::Array(a) => a,
                    other => vec![other],
                };
                for m in msgs {
                    if let Some(r) = self.dispatch(m, want) {
                        found = Some(r);
                    }
                }
            }
        }
        if want.is_some() && found.is_none() {
            return Err(XErr::Other(
                "upstream ended the response without a result".into(),
            ));
        }
        Ok(Exchange {
            response: found,
            session: session_hdr,
        })
    }

    /// Routes one upstream message; returns it when it answers `want`.
    fn dispatch(self: &Arc<Self>, msg: Value, want: Option<u64>) -> Option<Value> {
        match jsonrpc::kind(&msg) {
            Kind::Response => {
                (want.is_some() && msg.get("id").and_then(Value::as_u64) == want).then_some(msg)
            }
            Kind::Notification => {
                let sessions = lock(&self.pending).sessions();
                self.events
                    .notification_from(&self.server, &self.client.scope_key, msg, sessions);
                None
            }
            Kind::Request => {
                let sessions = lock(&self.pending).sessions();
                let this = self.clone();
                let reply = Reply::new(msg["id"].clone(), move |resp: Value| {
                    let Ok(h) = tokio::runtime::Handle::try_current() else {
                        return;
                    };
                    h.spawn(async move {
                        let (sid, _) = this.snapshot();
                        let proto = lock(&this.session).protocol.clone();
                        let _ = this.exchange(&resp, None, sid, proto).await;
                    });
                });
                handle_server_request(
                    self.events.as_ref(),
                    &self.server,
                    &self.client,
                    msg,
                    sessions,
                    reply,
                );
                None
            }
            Kind::Invalid => None,
        }
    }

    async fn send_request(self: &Arc<Self>, id: u64, method: &str, params: Value) -> CallResult {
        let body = jsonrpc::request(id, method, params);
        let mut attempt = 0;
        let mut helper_rerun = false;
        loop {
            let (sid, epoch) = self.snapshot();
            let proto = lock(&self.session).protocol.clone();
            match self.exchange(&body, Some(id), sid, proto).await {
                Ok(ex) => {
                    return ex
                        .response
                        .ok_or_else(|| CallError::Unavailable("upstream sent no response".into()))
                }
                Err(XErr::SessionGone) if attempt == 0 => {
                    attempt += 1;
                    self.reinit(epoch).await.map_err(CallError::Unavailable)?;
                }
                Err(XErr::Transport(_)) if attempt == 0 => {
                    attempt += 1;
                    self.reconnect(epoch).await?;
                }
                // Credentials may have expired: ask the helper once more.
                Err(XErr::Status(401 | 403)) if self.helper.is_some() && !helper_rerun => {
                    helper_rerun = true;
                    self.refresh_headers()
                        .await
                        .map_err(CallError::Unavailable)?;
                }
                Err(e) => return Err(e.into_call()),
            }
        }
    }

    /// Sends a request with a fresh broker id; see `StdioConn::begin`.
    pub(crate) async fn begin(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        meta: CallMeta,
    ) -> Result<(u64, oneshot::Receiver<CallResult>), CallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut p = lock(&self.pending);
            if p.closed {
                return Err(CallError::Closed);
            }
            p.calls.insert(
                id,
                Pending {
                    tx,
                    meta,
                    abort: None,
                },
            );
        }
        let this = self.clone();
        let method = method.to_owned();
        let handle = tokio::spawn(async move {
            let res = this.send_request(id, &method, params).await;
            let call = lock(&this.pending).calls.remove(&id);
            if let Some(call) = call {
                let res = res.map(|mut v| {
                    v["id"] = call.meta.orig_id.clone();
                    v
                });
                let _ = call.tx.send(res);
            }
        });
        if let Some(p) = lock(&self.pending).calls.get_mut(&id) {
            p.abort = Some(handle.abort_handle());
        }
        Ok((id, rx))
    }

    /// Sends a request and waits for its response.
    pub(crate) async fn call(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        meta: CallMeta,
    ) -> CallResult {
        let (_, rx) = self.begin(method, params, meta).await?;
        rx.await.unwrap_or(Err(CallError::Closed))
    }

    /// Sends a notification.
    pub(crate) async fn notify(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<(), CallError> {
        let (sid, _) = self.snapshot();
        let proto = lock(&self.session).protocol.clone();
        self.exchange(&jsonrpc::notification(method, params), None, sid, proto)
            .await
            .map(|_| ())
            .map_err(XErr::into_call)
    }

    /// Forgets pending call `id`, aborts its POST and tells the upstream.
    pub(crate) fn cancel(self: &Arc<Self>, id: u64, reason: &str) {
        let Some(call) = lock(&self.pending).calls.remove(&id) else {
            return;
        };
        if let Some(h) = call.abort {
            h.abort();
        }
        let this = self.clone();
        let reason = reason.to_owned();
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(async move {
                let _ = this
                    .notify(
                        "notifications/cancelled",
                        json!({"requestId": id, "reason": reason}),
                    )
                    .await;
            });
        }
    }

    /// Ends the upstream session: aborts in-flight POSTs, fails pending calls
    /// and DELETEs the session. Idempotent.
    pub(crate) async fn shutdown(self: &Arc<Self>, grace: Duration) {
        self.set_listening(false);
        let aborts: Vec<AbortHandle> = lock(&self.pending)
            .calls
            .values_mut()
            .filter_map(|p| p.abort.take())
            .collect();
        for a in aborts {
            a.abort();
        }
        fail_all(&self.pending);
        let (sid, proto) = {
            let mut s = lock(&self.session);
            (s.id.take(), s.protocol.take())
        };
        let Some(sid) = sid else { return };
        let mut req = client()
            .delete(&self.url)
            .timeout(grace.max(Duration::from_secs(2)))
            .header("Mcp-Session-Id", sid);
        for (k, v) in self.current_headers() {
            req = req.header(k, v);
        }
        if let Some(p) = proto {
            req = req.header("MCP-Protocol-Version", p);
        }
        if let Err(e) = req.send().await {
            tracing::debug!(server = %self.server, error = %e.without_url(), "upstream session delete failed");
        }
    }
}

fn map_err(e: reqwest::Error) -> XErr {
    let e = e.without_url();
    if e.is_timeout() {
        XErr::Other("upstream request timed out".into())
    } else {
        XErr::Transport(e.to_string())
    }
}

fn fail_all(pending: &Mutex<PendingMap>) {
    let drained: Vec<Pending> = {
        let mut p = lock(pending);
        p.closed = true;
        p.calls.drain().map(|(_, v)| v).collect()
    };
    for call in drained {
        let _ = call.tx.send(Err(CallError::Closed));
    }
}

/// Incremental Server-Sent Events parser: yields the `data` of each event.
#[derive(Default)]
pub(crate) struct SseParser {
    line: Vec<u8>,
    data: String,
    has_data: bool,
}

impl SseParser {
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in chunk {
            if b != b'\n' {
                self.line.push(b);
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            if self.line.is_empty() {
                if self.has_data {
                    out.push(std::mem::take(&mut self.data));
                    self.has_data = false;
                }
            } else if let Some(rest) = self.line.strip_prefix(b"data:") {
                let rest = rest.strip_prefix(b" ").unwrap_or(rest);
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(&String::from_utf8_lossy(rest));
                self.has_data = true;
            }
            self.line.clear();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_events_split_across_chunks() {
        let mut p = SseParser::default();
        assert!(p.feed(b"event: message\r\ndata: {\"a\"").is_empty());
        let e = p.feed(b":1}\n\ndata: x\ndata: y\n\n: comment\n\n");
        assert_eq!(e, vec!["{\"a\":1}".to_string(), "x\ny".to_string()]);
    }
}
