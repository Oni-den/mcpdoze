// SPDX-License-Identifier: GPL-3.0-or-later

//! Agent sessions: negotiated version, notification outbox and stream,
//! in-flight calls, roots and server-to-client requests.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::task::Waker;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{MAX_CLIENT_PENDING, SESSION_OUTBOX};
use crate::backend::jsonrpc;
use crate::backend::stdio::lock;

/// Protocol generation a session speaks with the broker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Era {
    /// Today's session model (the only one served so far).
    #[default]
    Legacy,
    /// The newer session model (planned for v0.2).
    Modern,
}

/// Callback run once when a session ends.
type EndHook = Box<dyn FnOnce() + Send>;

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

/// What the broker learnt from the agent's `roots/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RootsState {
    /// Not asked yet (or the agent said its roots changed).
    NotAsked,
    /// Asked; the answer (first `file://` root) or `None`.
    Known(Option<PathBuf>),
}

/// One agent session.
pub struct Session {
    pub(super) id: String,
    protocol_version: String,
    ephemeral: bool,
    era: Mutex<Era>,
    pub(super) inflight: Mutex<HashMap<String, oneshot::Sender<()>>>,
    outbox: Mutex<Outbox>,
    next_stream: AtomicU64,
    pub(super) last_used: Mutex<Instant>,
    /// Directory from `X-Mcpdoze-Cwd` / `?cwd=`.
    declared_cwd: Mutex<Option<PathBuf>>,
    /// The agent advertised the `roots` client capability.
    roots_capable: AtomicBool,
    pub(super) roots: Mutex<RootsState>,
    pub(super) roots_lock: tokio::sync::Mutex<()>,
    /// Server-to-client requests sent to this agent, by broker id key.
    client_pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// Logged that the directory is unknown (once per session).
    pub(super) cwd_fallback_logged: AtomicBool,
    /// Run once when the session ends.
    end_hooks: Mutex<Vec<EndHook>>,
    /// The subscription release hook is registered.
    pub(super) subs_hooked: AtomicBool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("protocol_version", &self.protocol_version)
            .field("ephemeral", &self.ephemeral)
            .field("era", &self.era())
            .finish_non_exhaustive()
    }
}

impl Session {
    pub(super) fn new(protocol_version: &str, ephemeral: bool) -> Self {
        Self {
            id: random_id(),
            protocol_version: protocol_version.to_owned(),
            ephemeral,
            era: Mutex::new(Era::default()),
            end_hooks: Mutex::new(Vec::new()),
            inflight: Mutex::new(HashMap::new()),
            outbox: Mutex::new(Outbox::default()),
            next_stream: AtomicU64::new(1),
            last_used: Mutex::new(Instant::now()),
            declared_cwd: Mutex::new(None),
            roots_capable: AtomicBool::new(false),
            roots: Mutex::new(RootsState::NotAsked),
            roots_lock: tokio::sync::Mutex::new(()),
            client_pending: Mutex::new(HashMap::new()),
            cwd_fallback_logged: AtomicBool::new(false),
            subs_hooked: AtomicBool::new(false),
        }
    }

    /// True if a notification with `method` may be queued for this session.
    pub fn wants_notification(&self, method: &str) -> bool {
        let _ = method;
        true
    }

    /// Protocol generation of this session ([`Era::Legacy`] by default).
    pub fn era(&self) -> Era {
        *lock(&self.era)
    }

    /// Sets the protocol generation of this session.
    pub fn set_era(&self, era: Era) {
        *lock(&self.era) = era;
    }

    /// Registers `hook`, run once when the session ends (`DELETE`,
    /// eviction, idle timeout, broker shutdown). Runs at once if the session
    /// has already ended.
    pub fn on_end(&self, hook: impl FnOnce() + Send + 'static) {
        if self.is_ended() {
            hook();
            return;
        }
        lock(&self.end_hooks).push(Box::new(hook));
        // `end` may have run between the check and the push.
        if self.is_ended() {
            self.run_end_hooks();
        }
    }

    fn run_end_hooks(&self) {
        let hooks = std::mem::take(&mut *lock(&self.end_hooks));
        for hook in hooks {
            hook();
        }
    }

    /// Records the working directory the client declared
    /// (`X-Mcpdoze-Cwd` header or `?cwd=` query). Relative paths are ignored.
    pub fn set_declared_cwd(&self, cwd: PathBuf) {
        if cwd.is_absolute() {
            *lock(&self.declared_cwd) = Some(cwd);
        } else {
            tracing::info!("relative client directory ignored");
        }
    }

    /// The directory declared by header or query, if any.
    pub fn declared_cwd(&self) -> Option<PathBuf> {
        lock(&self.declared_cwd).clone()
    }

    /// Records whether the agent advertised the `roots` capability.
    pub fn set_roots_capable(&self, on: bool) {
        self.roots_capable.store(on, Ordering::SeqCst);
    }

    /// True if the agent advertised the `roots` capability.
    pub fn roots_capable(&self) -> bool {
        self.roots_capable.load(Ordering::SeqCst)
    }

    /// The directory from the agent's roots, once asked and known.
    pub fn roots_cwd(&self) -> Option<PathBuf> {
        match &*lock(&self.roots) {
            RootsState::Known(p) => p.clone(),
            RootsState::NotAsked => None,
        }
    }

    /// True once the session ended (`DELETE`, eviction, idle timeout).
    pub fn is_ended(&self) -> bool {
        lock(&self.outbox).closed
    }

    /// Server-to-client requests awaiting this agent's answer.
    pub fn pending_client_requests(&self) -> usize {
        lock(&self.client_pending).len()
    }

    /// Sends a request to the agent over its event stream and waits for the
    /// answer (the whole JSON-RPC response) for at most `limit`.
    pub async fn client_request(
        &self,
        id: String,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, String> {
        let key = jsonrpc::id_key(&Value::String(id.clone()));
        let (tx, rx) = oneshot::channel();
        {
            let mut p = lock(&self.client_pending);
            if p.len() >= MAX_CLIENT_PENDING {
                return Err("too many requests pending at the agent".into());
            }
            p.insert(key.clone(), tx);
        }
        struct Remove<'a>(&'a Session, String);
        impl Drop for Remove<'_> {
            fn drop(&mut self) {
                lock(&self.0.client_pending).remove(&self.1);
            }
        }
        let _remove = Remove(self, key);
        if self.is_ended() {
            return Err("agent session ended".into());
        }
        self.push(jsonrpc::request(id, method, params));
        match tokio::time::timeout(limit, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err("agent session ended".into()),
            Err(_) => Err(format!(
                "agent did not answer {method} within {} s",
                limit.as_secs()
            )),
        }
    }

    /// Delivers an agent's response to a pending server-to-client request.
    /// False if no request has that id.
    pub(super) fn answer(&self, msg: Value) -> bool {
        let key = jsonrpc::id_key(&msg["id"]);
        match lock(&self.client_pending).remove(&key) {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
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

    /// Stream `id` went away (client disconnected). The idle timeout counts
    /// from here.
    pub fn close_stream(&self, id: u64) {
        let mut o = lock(&self.outbox);
        if o.stream == Some(id) {
            o.stream = None;
            o.waker = None;
            drop(o);
            self.touch();
        }
    }

    /// Ends the session's streams for good and fails requests pending at
    /// the agent.
    pub(super) fn end(&self) {
        let waker = {
            let mut o = lock(&self.outbox);
            o.closed = true;
            o.stream = None;
            o.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
        lock(&self.client_pending).clear();
        self.run_end_hooks();
    }

    /// Cancels every in-flight request of this session.
    pub fn cancel_all(&self) {
        let all: Vec<_> = lock(&self.inflight).drain().map(|(_, tx)| tx).collect();
        for tx in all {
            let _ = tx.send(());
        }
    }

    pub(super) fn cancel(&self, id: &Value) -> bool {
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

    pub(super) fn touch(&self) {
        *lock(&self.last_used) = Instant::now();
    }
}

/// 128-bit session id from the OS CSPRNG, as 32 lowercase hex digits.
pub(super) fn random_id() -> String {
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
