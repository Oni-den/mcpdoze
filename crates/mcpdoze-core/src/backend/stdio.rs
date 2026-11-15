// SPDX-License-Identifier: GPL-3.0-or-later

//! One spawned stdio backend process and its JSON-RPC plumbing.
//!
//! Tasks per connection:
//! * writer: owns the child's stdin, writes queued lines. Ends (closing stdin,
//!   the polite stop signal) once every sender is dropped.
//! * reader: demultiplexes stdout lines. Responses go to the pending call
//!   with that broker id (the original agent id is restored); notifications
//!   and server-to-client requests go to [`BackendEvents`]. Non-JSON lines are
//!   logged at debug and dropped; lines over [`MAX_LINE`] are dropped.
//! * stderr: keeps the last [`STDERR_LINES`] lines in a ring buffer.
//!
//! The reader holds only a weak sender, so it never keeps stdin open.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use super::jsonrpc::{self, Kind};
use super::{BackendEvents, BackendSpec};
use crate::proctree::{resolve_command, ProcessTree};

/// Largest stdout line accepted from a backend.
pub const MAX_LINE: usize = 16 * 1024 * 1024;
/// Lines of backend stderr kept for diagnostics.
pub const STDERR_LINES: usize = 500;
/// Longest stderr line kept (longer lines are truncated).
const MAX_STDERR_LINE: usize = 8 * 1024;
/// Outgoing line queue depth per backend.
const WRITE_QUEUE: usize = 256;
/// Protocol version the broker requests from backends.
pub const BACKEND_PROTOCOL_VERSION: &str = "2025-11-25";

/// Locks a std mutex, ignoring poisoning (state stays consistent because
/// every critical section is panic-free bookkeeping).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Why a call to a backend did not produce a response.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// The backend exited or its pipes closed.
    #[error("backend connection closed")]
    Closed,
    /// The call was cancelled by the agent.
    #[error("request cancelled")]
    Cancelled,
    /// The backend is not available (start failure or crash backoff).
    #[error("backend unavailable: {0}")]
    Unavailable(String),
}

/// Who a backend call belongs to; kept in the id map until the response.
#[derive(Debug, Clone, Default)]
pub struct CallMeta {
    /// Agent session id, `None` for broker-internal calls.
    pub session: Option<String>,
    /// The agent's original request id, restored on the response.
    pub orig_id: Value,
    /// The agent's original progress token, if it sent one.
    pub progress_token: Option<Value>,
}

struct Pending {
    tx: oneshot::Sender<Result<Value, CallError>>,
    meta: CallMeta,
}

#[derive(Default)]
struct PendingMap {
    calls: HashMap<u64, Pending>,
    closed: bool,
}

/// Ring buffer of the most recent stderr lines of a backend.
#[derive(Debug, Default)]
pub struct StderrRing(Mutex<VecDeque<String>>);

impl StderrRing {
    fn push(&self, line: String) {
        let mut q = lock(&self.0);
        if q.len() == STDERR_LINES {
            q.pop_front();
        }
        q.push_back(line);
    }

    /// Snapshot of the buffered lines, oldest first.
    pub fn lines(&self) -> Vec<String> {
        lock(&self.0).iter().cloned().collect()
    }
}

/// A running stdio backend process.
pub struct StdioConn {
    generation: u64,
    pid: u32,
    started: Instant,
    tx: Mutex<Option<mpsc::Sender<String>>>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: AtomicU64,
    tree: Mutex<Option<ProcessTree>>,
    closed: watch::Receiver<bool>,
    init: Mutex<Value>,
}

impl std::fmt::Debug for StdioConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioConn")
            .field("generation", &self.generation)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl StdioConn {
    /// Spawns the backend and starts its I/O tasks. Does not handshake.
    pub(crate) fn spawn(
        server: &str,
        generation: u64,
        spec: &BackendSpec,
        events: Arc<dyn BackendEvents>,
        stderr: Arc<StderrRing>,
    ) -> io::Result<Self> {
        let mut cmd = resolve_command(&spec.command, &spec.args)?;
        cmd.envs(&spec.env);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        let mut tree = ProcessTree::spawn(cmd)?;
        let stdio = tree
            .take_stdio()
            .ok_or_else(|| io::Error::other("backend stdio missing"))?;
        let pid = tree.pid();
        let (tx, rx) = mpsc::channel::<String>(WRITE_QUEUE);
        let (closed_tx, closed) = watch::channel(false);
        let pending = Arc::new(Mutex::new(PendingMap::default()));

        tokio::spawn(write_loop(stdio.stdin, rx));
        tokio::spawn(read_loop(ReadCtx {
            server: server.to_owned(),
            stdout: BufReader::new(stdio.stdout),
            pending: pending.clone(),
            tx: tx.downgrade(),
            events,
            closed: closed_tx,
        }));
        tokio::spawn(stderr_loop(BufReader::new(stdio.stderr), stderr));
        tracing::debug!(server, pid, "backend spawned");
        Ok(Self {
            generation,
            pid,
            started: Instant::now(),
            tx: Mutex::new(Some(tx)),
            pending,
            next_id: AtomicU64::new(1),
            tree: Mutex::new(Some(tree)),
            closed,
            init: Mutex::new(Value::Null),
        })
    }

    /// Performs the broker's `initialize` handshake as one synthetic client,
    /// then sends `notifications/initialized`. Returns the backend's
    /// `initialize` result.
    pub(crate) async fn initialize(&self) -> Result<Value, String> {
        let params = json!({
            "protocolVersion": BACKEND_PROTOCOL_VERSION,
            // Conservative: the broker answers server-to-client requests with
            // "not supported" until M7 routes them to agent sessions.
            "capabilities": {},
            "clientInfo": {"name": "mcpdoze", "version": env!("CARGO_PKG_VERSION")},
        });
        let resp = self
            .call("initialize", params, CallMeta::default())
            .await
            .map_err(|e| e.to_string())?;
        let Some(result) = resp.get("result").cloned() else {
            return Err(format!(
                "initialize failed: {}",
                resp.get("error").cloned().unwrap_or(Value::Null)
            ));
        };
        self.notify("notifications/initialized", json!({}))
            .await
            .map_err(|e| e.to_string())?;
        *lock(&self.init) = result.clone();
        Ok(result)
    }

    /// Lifecycle generation this connection belongs to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// OS pid of the backend root process.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// When the process was spawned.
    pub fn started(&self) -> Instant {
        self.started
    }

    /// The backend's `initialize` result (`Null` before the handshake).
    pub fn init_result(&self) -> Value {
        lock(&self.init).clone()
    }

    /// Number of calls awaiting a response.
    pub fn pending_len(&self) -> usize {
        lock(&self.pending).calls.len()
    }

    /// Resolves once the backend's stdout has closed (exit or crash).
    pub(crate) fn closed(&self) -> watch::Receiver<bool> {
        self.closed.clone()
    }

    fn sender(&self) -> Result<mpsc::Sender<String>, CallError> {
        lock(&self.tx).clone().ok_or(CallError::Closed)
    }

    /// Sends a request with a fresh broker id. Returns that id and a
    /// receiver for the response (with the original id restored from `meta`).
    pub(crate) async fn begin(
        &self,
        method: &str,
        params: Value,
        meta: CallMeta,
    ) -> Result<(u64, oneshot::Receiver<Result<Value, CallError>>), CallError> {
        let sender = self.sender()?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let line = jsonrpc::request(id, method, params).to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut p = lock(&self.pending);
            if p.closed {
                return Err(CallError::Closed);
            }
            p.calls.insert(id, Pending { tx, meta });
        }
        if sender.send(line).await.is_err() {
            lock(&self.pending).calls.remove(&id);
            return Err(CallError::Closed);
        }
        Ok((id, rx))
    }

    /// Sends a request and waits for its response.
    pub(crate) async fn call(
        &self,
        method: &str,
        params: Value,
        meta: CallMeta,
    ) -> Result<Value, CallError> {
        let (_, rx) = self.begin(method, params, meta).await?;
        rx.await.unwrap_or(Err(CallError::Closed))
    }

    /// Sends a notification.
    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<(), CallError> {
        let line = jsonrpc::notification(method, params).to_string();
        self.sender()?
            .send(line)
            .await
            .map_err(|_| CallError::Closed)
    }

    /// Forgets the pending call `id` and tells the backend it was cancelled.
    /// Safe to call from `Drop`; a no-op if the call already finished.
    pub(crate) fn cancel(&self, id: u64, reason: &str) {
        if lock(&self.pending).calls.remove(&id).is_none() {
            return;
        }
        let Ok(sender) = self.sender() else { return };
        let line = jsonrpc::notification(
            "notifications/cancelled",
            json!({"requestId": id, "reason": reason}),
        )
        .to_string();
        if let Err(mpsc::error::TrySendError::Full(line)) = sender.try_send(line) {
            if let Ok(h) = tokio::runtime::Handle::try_current() {
                h.spawn(async move {
                    let _ = sender.send(line).await;
                });
            }
        }
    }

    /// Stops the backend: closes stdin, waits up to `grace`, then kills the
    /// whole process tree. Idempotent.
    pub(crate) async fn shutdown(&self, grace: Duration) {
        lock(&self.tx).take();
        let tree = lock(&self.tree).take();
        if let Some(mut tree) = tree {
            if let Err(e) = tree.terminate(grace).await {
                tracing::warn!(pid = self.pid, error = %e, "backend tree did not terminate");
            }
        }
        fail_all(&self.pending);
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

async fn write_loop(mut stdin: tokio::process::ChildStdin, mut rx: mpsc::Receiver<String>) {
    while let Some(line) = rx.recv().await {
        let ok = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        }
        .await;
        if ok.is_err() {
            break;
        }
    }
    // Dropping stdin closes the pipe.
}

/// Result of reading one bounded line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
    /// A complete line (without the newline) is in the buffer.
    Line,
    /// The line exceeded the limit and was discarded.
    TooLong,
    /// End of stream.
    Eof,
}

/// Reads one `\n`-terminated line into `buf` (cleared first), keeping at
/// most `max` bytes. Longer lines are consumed and reported as `TooLong`.
pub(crate) async fn read_frame<R: AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> io::Result<Frame> {
    buf.clear();
    let mut overflow = false;
    let mut any = false;
    loop {
        let avail = r.fill_buf().await?;
        if avail.is_empty() {
            return Ok(match (any, overflow) {
                (false, _) => Frame::Eof,
                (true, true) => Frame::TooLong,
                (true, false) => Frame::Line,
            });
        }
        any = true;
        let (chunk, used, done) = match avail.iter().position(|&b| b == b'\n') {
            Some(i) => (&avail[..i], i + 1, true),
            None => (avail, avail.len(), false),
        };
        if !overflow {
            if buf.len() + chunk.len() > max {
                overflow = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        r.consume(used);
        if done {
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(if overflow {
                Frame::TooLong
            } else {
                Frame::Line
            });
        }
    }
}

struct ReadCtx {
    server: String,
    stdout: BufReader<tokio::process::ChildStdout>,
    pending: Arc<Mutex<PendingMap>>,
    tx: mpsc::WeakSender<String>,
    events: Arc<dyn BackendEvents>,
    closed: watch::Sender<bool>,
}

async fn read_loop(mut ctx: ReadCtx) {
    let mut buf = Vec::new();
    loop {
        match read_frame(&mut ctx.stdout, &mut buf, MAX_LINE).await {
            Ok(Frame::Line) => {}
            Ok(Frame::TooLong) => {
                tracing::warn!(server = %ctx.server, "backend line over 16 MiB dropped");
                continue;
            }
            Ok(Frame::Eof) | Err(_) => break,
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(&buf) {
            Ok(msg) => dispatch(&ctx, msg),
            Err(_) => tracing::debug!(server = %ctx.server, "non-JSON backend line dropped"),
        }
    }
    fail_all(&ctx.pending);
    let _ = ctx.closed.send(true);
    tracing::debug!(server = %ctx.server, "backend stdout closed");
}

fn dispatch(ctx: &ReadCtx, mut msg: Value) {
    match jsonrpc::kind(&msg) {
        Kind::Response => {
            let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                tracing::debug!(server = %ctx.server, "response with foreign id dropped");
                return;
            };
            let Some(call) = lock(&ctx.pending).calls.remove(&id) else {
                tracing::debug!(server = %ctx.server, id, "late or unknown response dropped");
                return;
            };
            msg["id"] = call.meta.orig_id;
            let _ = call.tx.send(Ok(msg));
        }
        Kind::Notification => ctx.events.notification(&ctx.server, msg),
        Kind::Request => {
            let reply = ctx
                .events
                .server_request(&ctx.server, &msg)
                .unwrap_or_else(|| {
                    jsonrpc::error(
                        msg["id"].clone(),
                        jsonrpc::METHOD_NOT_FOUND,
                        "method not supported by the mcpdoze broker",
                    )
                });
            if let Some(tx) = ctx.tx.upgrade() {
                let _ = tx.try_send(reply.to_string());
            }
        }
        Kind::Invalid => tracing::debug!(server = %ctx.server, "invalid JSON-RPC line dropped"),
    }
}

async fn stderr_loop(mut r: BufReader<tokio::process::ChildStderr>, ring: Arc<StderrRing>) {
    let mut buf = Vec::new();
    loop {
        match read_frame(&mut r, &mut buf, MAX_STDERR_LINE).await {
            Ok(Frame::Line) => ring.push(String::from_utf8_lossy(&buf).into_owned()),
            Ok(Frame::TooLong) => ring.push("<line too long>".into()),
            Ok(Frame::Eof) | Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn frames(input: &[u8], max: usize) -> Vec<(Frame, Vec<u8>)> {
        let mut r = BufReader::with_capacity(4, input);
        let mut buf = Vec::new();
        let mut out = Vec::new();
        loop {
            let f = read_frame(&mut r, &mut buf, max).await.unwrap();
            if f == Frame::Eof {
                return out;
            }
            out.push((f, buf.clone()));
        }
    }

    #[tokio::test]
    async fn frames_split_on_newline_and_cap_length() {
        let got = frames(b"ab\r\n0123456789\nxyz", 5).await;
        assert_eq!(
            got,
            vec![
                (Frame::Line, b"ab".to_vec()),
                (Frame::TooLong, vec![]),
                (Frame::Line, b"xyz".to_vec()),
            ]
        );
    }

    #[test]
    fn stderr_ring_is_bounded() {
        let r = StderrRing::default();
        for i in 0..(STDERR_LINES + 10) {
            r.push(i.to_string());
        }
        let l = r.lines();
        assert_eq!(l.len(), STDERR_LINES);
        assert_eq!(l[0], "10");
    }
}
