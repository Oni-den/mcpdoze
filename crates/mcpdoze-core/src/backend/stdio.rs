// SPDX-License-Identifier: GPL-3.0-or-later

//! One spawned stdio backend process and its JSON-RPC plumbing.
//!
//! Tasks per connection:
//! * writer: owns the child's stdin, writes queued lines. Ends (closing stdin,
//!   the polite stop signal) once every sender is dropped.
//! * reader: demultiplexes stdout lines. Responses go to the pending call
//!   with that broker id (the original agent id is restored); notifications
//!   and server-to-client requests go to [`BackendEvents`]. Non-JSON lines are
//!   counted ([`WireStats`]) and dropped without logging their content; a line
//!   over [`MAX_LINE`] is dropped and fails the pending calls (its response
//!   cannot be attributed).
//! * stderr: keeps the last [`STDERR_LINES`] lines in a ring buffer.
//! * exit watch: waits for the root process to exit. Exit is a crash signal
//!   on its own (a grandchild may keep stdout open): after a short drain
//!   grace ([`EXIT_DRAIN`]) pending calls fail and the connection reports
//!   itself closed.
//!
//! The reader holds only a weak sender, so it never keeps stdin open.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use super::jsonrpc::{self, Kind};
use super::{handle_server_request, BackendEvents, BackendSpec, ClientSide, Reply};
use crate::diag::{BackendView, DiagTap, Dir, ExitView, FrameType, IdType, Phase, StderrView};
use crate::proctree::{resolve_command, ExitInfo, ProcessTree, TreeMemory};

/// Largest stdout line accepted from a backend.
pub const MAX_LINE: usize = 16 * 1024 * 1024;
/// Lines of backend stderr kept for diagnostics.
pub const STDERR_LINES: usize = 500;
/// Newest stderr lines copied into one diagnostics event.
pub const STDERR_SNAPSHOT_LINES: usize = 64;
/// Longest stderr line copied into one diagnostics event.
pub const STDERR_SNAPSHOT_BYTES: usize = 2048;
/// Longest stderr line kept (longer lines are truncated).
const MAX_STDERR_LINE: usize = 8 * 1024;
/// Outgoing line queue depth per backend.
const WRITE_QUEUE: usize = 256;
/// After the root process exits, how long the reader may still drain stdout
/// before pending calls are failed.
pub const EXIT_DRAIN: Duration = Duration::from_millis(200);
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
    /// The backend is not available (start failure, crash backoff, or a
    /// response the broker could not accept such as an oversize line).
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

impl PendingMap {
    /// Distinct agent sessions with a call pending.
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

/// Counters of unusual traffic on a backend's stdout. Counts only; the
/// content of such lines is never kept or logged.
#[derive(Debug, Default)]
pub struct WireStats {
    non_json: AtomicU64,
    invalid: AtomicU64,
    oversize: AtomicU64,
    foreign_ids: AtomicU64,
}

/// Snapshot of [`WireStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WireCounts {
    /// Lines that were not valid JSON (stdout pollution).
    pub non_json_lines: u64,
    /// Valid JSON that was not a JSON-RPC message.
    pub invalid_messages: u64,
    /// Lines over [`MAX_LINE`].
    pub oversize_lines: u64,
    /// Responses whose id matched no broker id format.
    pub foreign_ids: u64,
}

impl WireStats {
    /// Current counts.
    pub fn snapshot(&self) -> WireCounts {
        WireCounts {
            non_json_lines: self.non_json.load(Ordering::Relaxed),
            invalid_messages: self.invalid.load(Ordering::Relaxed),
            oversize_lines: self.oversize.load(Ordering::Relaxed),
            foreign_ids: self.foreign_ids.load(Ordering::Relaxed),
        }
    }
}

/// One stderr line with its sequence number and arrival time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StderrEntry {
    /// 1-based number of the line since the ring was created.
    pub seq: u64,
    /// Arrival time (Unix milliseconds).
    pub ts_ms: u64,
    /// The line.
    pub line: String,
}

#[derive(Debug, Default)]
struct StderrInner {
    q: VecDeque<StderrEntry>,
    total: u64,
}

/// Ring buffer of the most recent stderr lines of a backend. Entries carry
/// a sequence number and a timestamp; the count of every line ever seen
/// survives eviction.
#[derive(Debug, Default)]
pub struct StderrRing(Mutex<StderrInner>);

impl StderrRing {
    fn push(&self, line: String) {
        let mut g = lock(&self.0);
        if g.q.len() == STDERR_LINES {
            g.q.pop_front();
        }
        g.total += 1;
        let seq = g.total;
        g.q.push_back(StderrEntry {
            seq,
            ts_ms: crate::stats::now_ms(),
            line,
        });
    }

    /// Snapshot of the buffered lines, oldest first.
    pub fn lines(&self) -> Vec<String> {
        lock(&self.0).q.iter().map(|e| e.line.clone()).collect()
    }

    /// Snapshot of the buffered entries, oldest first.
    pub fn entries(&self) -> Vec<StderrEntry> {
        lock(&self.0).q.iter().cloned().collect()
    }

    /// Lines written since the ring was created, and the newest
    /// [`STDERR_SNAPSHOT_LINES`] buffered lines (oldest first, each capped
    /// at [`STDERR_SNAPSHOT_BYTES`]) with their arrival times. Bounded so a
    /// flood of failures cannot make every event copy the whole ring.
    pub fn snapshot(&self) -> (u64, Vec<String>, Vec<u64>) {
        let g = lock(&self.0);
        let skip = g.q.len().saturating_sub(STDERR_SNAPSHOT_LINES);
        let mut lines = Vec::with_capacity(g.q.len() - skip);
        let mut ts = Vec::with_capacity(g.q.len() - skip);
        for e in g.q.iter().skip(skip) {
            let mut end = e.line.len().min(STDERR_SNAPSHOT_BYTES);
            while !e.line.is_char_boundary(end) {
                end -= 1;
            }
            lines.push(e.line[..end].to_string());
            ts.push(e.ts_ms);
        }
        (g.total, lines, ts)
    }
}

/// Names of the lifecycle states, indexed like [`InstCounters::state`].
pub(crate) const STATE_NAMES: [&str; 6] = [
    "stopped", "starting", "running", "draining", "stopping", "failed",
];

/// Facts an instance shares with its connections so that diagnostics
/// records built in the I/O tasks carry the instance's state. Plain
/// atomics; reading them costs nothing when diagnostics are off.
#[derive(Debug, Default)]
pub struct InstCounters {
    /// Index into [`STATE_NAMES`].
    pub(crate) state: AtomicU8,
    /// Consecutive failures counted for backoff.
    pub(crate) failures: AtomicU64,
    /// Processes spawned so far.
    pub(crate) spawns: AtomicU64,
    /// Calls handed out so far.
    pub(crate) calls: AtomicU64,
    /// How far the current start got (see [`Phase`]).
    pub(crate) phase: AtomicU8,
}

impl InstCounters {
    pub(crate) fn state_name(&self) -> &'static str {
        STATE_NAMES[usize::from(self.state.load(Ordering::Relaxed)).min(STATE_NAMES.len() - 1)]
    }

    /// Moves the start phase forward (never back).
    pub(crate) fn reach(&self, p: Phase) {
        self.phase.fetch_max(phase_idx(p), Ordering::Relaxed);
    }

    /// Starts a new start attempt.
    pub(crate) fn reset_phase(&self) {
        self.phase.store(0, Ordering::Relaxed);
    }

    /// The phase reached so far.
    pub(crate) fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Relaxed) {
            0 => Phase::Spawned,
            1 => Phase::InitSent,
            2 => Phase::StdoutSeen,
            _ => Phase::InitReplied,
        }
    }
}

fn phase_idx(p: Phase) -> u8 {
    match p {
        Phase::Spawned => 0,
        Phase::InitSent => 1,
        Phase::StdoutSeen => 2,
        Phase::InitReplied => 3,
    }
}

/// The JSON type of a JSON-RPC id.
pub(crate) fn id_type_of(v: &Value) -> IdType {
    match v {
        Value::Number(_) => IdType::Number,
        Value::String(_) => IdType::String,
        Value::Null => IdType::Null,
        _ => IdType::Other,
    }
}

#[derive(Debug, Default)]
struct Handshake {
    protocol: Option<String>,
    server_name: Option<String>,
    server_version: Option<String>,
    caps: Vec<String>,
}

#[derive(Debug, Clone)]
struct CrashInfo {
    incident: String,
    /// Backend ids that were pending when the connection died.
    bids: Vec<u64>,
    /// The last request sent among them.
    suspect: u64,
}

/// What a call that died with its backend needs for its `crash_in_flight`
/// record.
#[derive(Debug, Clone)]
pub struct CrashReport {
    /// Incident id shared by every call lost in the same crash.
    pub incident: String,
    /// This was the last request sent before the crash.
    pub suspect: bool,
    /// How the process ended (as far as known).
    pub exit: ExitView,
}

/// Diagnostics context of one connection, shared with its I/O tasks.
pub(crate) struct ConnDiag {
    pub(crate) tap: Arc<DiagTap>,
    pub(crate) counters: Arc<InstCounters>,
    stderr: Arc<StderrRing>,
    generation: u64,
    started: Instant,
    pending: Arc<Mutex<PendingMap>>,
    info: Mutex<Handshake>,
    crash: Mutex<Option<CrashInfo>>,
    /// Set when the broker itself stops the process (not a crash).
    stopping: AtomicBool,
}

impl ConnDiag {
    /// Runs `f` with a view of the backend. Call only after checking
    /// [`DiagTap::enabled`]: it locks and allocates.
    pub(crate) fn with_view<R>(&self, f: impl FnOnce(&BackendView<'_>) -> R) -> R {
        let pending = lock(&self.pending).calls.len() as u64;
        let h = lock(&self.info);
        let caps: Vec<&str> = h.caps.iter().map(String::as_str).collect();
        let mut v = BackendView::new(self.counters.state_name(), self.generation);
        v.uptime_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        v.in_flight = pending;
        v.pending = pending;
        v.calls_served = self.counters.calls.load(Ordering::Relaxed);
        v.restarts = self
            .counters
            .spawns
            .load(Ordering::Relaxed)
            .saturating_sub(1);
        v.failures = self.counters.failures.load(Ordering::Relaxed);
        v.protocol = h.protocol.as_deref();
        v.server_info = h
            .server_name
            .as_deref()
            .map(|n| (n, h.server_version.as_deref()));
        v.caps = &caps;
        f(&v)
    }

    fn with_stderr<R>(&self, f: impl FnOnce(&StderrView<'_>) -> R) -> R {
        let (total, lines, ts) = self.stderr.snapshot();
        f(&StderrView {
            total,
            lines: &lines,
            ts: &ts,
        })
    }

    fn frame(&self, dir: Dir, ftype: FrameType, method: &str, bid: u64, bytes: usize) {
        self.tap.on_frame(dir, ftype, method, bid, bytes);
    }

    fn noise(&self, line: &[u8]) {
        if self.tap.enabled() {
            self.with_view(|v| self.tap.on_stdout_noise(line, v));
        }
    }

    fn oversize(&self, consumed: u64) {
        if self.tap.enabled() {
            self.with_view(|v| self.tap.on_frame_oversize(consumed, v));
        }
    }

    fn invalid(&self, bytes: usize) {
        if self.tap.enabled() {
            self.with_view(|v| self.tap.on_frame_invalid(bytes, v));
        }
    }

    fn id_mismatch(&self, ty: IdType) {
        if self.tap.enabled() {
            self.with_view(|v| self.tap.on_id_type_mismatch(ty, v));
        }
    }

    fn late(&self, bid: u64) {
        if self.tap.enabled() {
            self.with_view(|v| self.tap.on_late_response(bid, v));
        }
    }

    fn init_error(&self, returned: Option<&str>, rpc: Option<(i64, &str)>) {
        if self.tap.enabled() {
            self.with_view(|v| {
                self.with_stderr(|s| {
                    self.tap
                        .on_init_error(BACKEND_PROTOCOL_VERSION, returned, rpc, v, Some(s));
                });
            });
        }
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
    client: ClientSide,
    wire: Arc<WireStats>,
    exit: watch::Receiver<Option<ExitInfo>>,
    protocol: Mutex<Option<String>>,
    diag: Arc<ConnDiag>,
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
        tap: Arc<DiagTap>,
        counters: Arc<InstCounters>,
    ) -> io::Result<Self> {
        let mut cmd = resolve_command(&spec.command, &spec.args)?;
        // Never hand the broker token to a backend through inheritance;
        // an explicit `env` entry of the same name (set by the user) wins.
        for name in spec.strip_env.iter().filter(|n| !spec.env.contains_key(*n)) {
            cmd.env_remove(name);
        }
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
        let closed_tx = Arc::new(closed_tx);
        let pending = Arc::new(Mutex::new(PendingMap::default()));
        let wire = Arc::new(WireStats::default());
        let (exit_tx, exit) = watch::channel(None);
        let diag = Arc::new(ConnDiag {
            tap,
            counters,
            stderr: stderr.clone(),
            generation,
            started: Instant::now(),
            pending: pending.clone(),
            info: Mutex::new(Handshake::default()),
            crash: Mutex::new(None),
            stopping: AtomicBool::new(false),
        });

        tokio::spawn(write_loop(stdio.stdin, rx));
        tokio::spawn(read_loop(ReadCtx {
            server: server.to_owned(),
            stdout: BufReader::new(stdio.stdout),
            pending: pending.clone(),
            tx: tx.downgrade(),
            events,
            closed: closed_tx.clone(),
            client: spec.client.clone(),
            wire: wire.clone(),
            diag: diag.clone(),
        }));
        let info = tree.exited_info();
        tokio::spawn(async move {
            if let Some(i) = info.await {
                let _ = exit_tx.send(Some(i));
            }
        });
        tokio::spawn(exit_watch(
            server.to_owned(),
            pid,
            tree.exited(),
            pending.clone(),
            closed_tx,
            diag.clone(),
        ));
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
            client: spec.client.clone(),
            wire,
            exit,
            protocol: Mutex::new(None),
            diag,
        })
    }

    /// Performs the broker's `initialize` handshake as one synthetic client,
    /// then sends `notifications/initialized`. Returns the backend's
    /// `initialize` result.
    pub(crate) async fn initialize(&self) -> Result<Value, String> {
        let params = json!({
            "protocolVersion": BACKEND_PROTOCOL_VERSION,
            // Only what the broker can serve for this instance's scope.
            "capabilities": self.client.capabilities(),
            "clientInfo": {"name": "mcpdoze", "version": env!("CARGO_PKG_VERSION")},
        });
        self.diag.counters.reach(Phase::InitSent);
        let resp = self
            .call("initialize", params, CallMeta::default())
            .await
            .map_err(|e| e.to_string())?;
        self.diag.counters.reach(Phase::InitReplied);
        let Some(result) = resp.get("result").cloned() else {
            if self.diag.tap.enabled() {
                let err = resp.get("error");
                let code = err.and_then(|e| e["code"].as_i64()).unwrap_or(0);
                let msg = err.and_then(|e| e["message"].as_str()).unwrap_or("");
                self.diag.init_error(None, Some((code, msg)));
            }
            return Err(format!(
                "initialize failed: {}",
                resp.get("error").cloned().unwrap_or(Value::Null)
            ));
        };
        self.notify("notifications/initialized", json!({}))
            .await
            .map_err(|e| e.to_string())?;
        // Accepted whatever it is; kept so diagnostics can show a mismatch.
        *lock(&self.protocol) = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .map(|v| v.chars().take(64).collect());
        *lock(&self.init) = result.clone();
        {
            let mut h = lock(&self.diag.info);
            h.protocol = lock(&self.protocol).clone();
            h.server_name = result["serverInfo"]["name"]
                .as_str()
                .map(|n| n.chars().take(64).collect());
            h.server_version = result["serverInfo"]["version"]
                .as_str()
                .map(|n| n.chars().take(64).collect());
            h.caps = result["capabilities"]
                .as_object()
                .map(|o| o.keys().take(32).cloned().collect())
                .unwrap_or_default();
        }
        if self.protocol_mismatch() {
            let returned = self.protocol_version();
            self.diag.init_error(returned.as_deref(), None);
        }
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

    /// The protocol version the backend answered `initialize` with
    /// (`None` before the handshake or when it sent none).
    pub fn protocol_version(&self) -> Option<String> {
        lock(&self.protocol).clone()
    }

    /// True when the backend negotiated a version other than the one the
    /// broker requested ([`BACKEND_PROTOCOL_VERSION`]). The connection is
    /// kept; this only feeds diagnostics.
    pub fn protocol_mismatch(&self) -> bool {
        lock(&self.protocol)
            .as_deref()
            .is_some_and(|v| v != BACKEND_PROTOCOL_VERSION)
    }

    /// Counts of non-JSON, invalid, oversize and foreign-id traffic.
    pub fn wire_counts(&self) -> WireCounts {
        self.wire.snapshot()
    }

    /// How the root process ended, if it has.
    pub fn exit_info(&self) -> Option<ExitInfo> {
        *self.exit.borrow()
    }

    /// Waits up to `grace` for the root process exit status (the stdout EOF
    /// can be seen a moment before the reaper reports the exit).
    pub(crate) async fn exit_info_wait(&self, grace: Duration) -> Option<ExitInfo> {
        let mut rx = self.exit.clone();
        let got = tokio::time::timeout(grace, rx.wait_for(Option::is_some)).await;
        match got {
            Ok(Ok(v)) => *v,
            _ => None,
        }
    }

    /// Memory summed over the process tree (zero once stopped). May scan the
    /// process table; call from a blocking context.
    pub fn memory(&self) -> TreeMemory {
        lock(&self.tree)
            .as_ref()
            .map(ProcessTree::memory)
            .unwrap_or_default()
    }

    /// Pids of the live processes of the tree (empty once stopped). May scan
    /// the process table; call from a blocking context.
    pub fn pids(&self) -> Vec<u32> {
        lock(&self.tree)
            .as_ref()
            .map(ProcessTree::pids)
            .unwrap_or_default()
    }

    /// For a call (backend id `bid`) that failed with [`CallError::Closed`]:
    /// the crash it was lost in, when the connection died on its own while
    /// it was pending. Waits briefly for the exit status.
    pub(crate) async fn crash_for(&self, bid: u64) -> Option<CrashReport> {
        let c = lock(&self.diag.crash).clone()?;
        if !c.bids.contains(&bid) {
            return None;
        }
        let exit = self.exit_info_wait(EXIT_DRAIN * 3).await;
        Some(CrashReport {
            incident: c.incident,
            suspect: c.suspect == bid,
            exit: ExitView {
                code: exit.and_then(|e| e.code),
                signal: exit.and_then(|e| e.signal),
            },
        })
    }

    /// Number of calls awaiting a response.
    pub fn pending_len(&self) -> usize {
        lock(&self.pending).calls.len()
    }

    /// Resolves once the backend's stdout has closed or its root process
    /// exited (exit or crash).
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
        self.diag
            .frame(Dir::Out, FrameType::Request, method, id, line.len());
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
        self.diag
            .frame(Dir::Out, FrameType::Notification, method, 0, line.len());
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
        self.diag.frame(
            Dir::Out,
            FrameType::Notification,
            "notifications/cancelled",
            id,
            line.len(),
        );
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
        self.diag.stopping.store(true, Ordering::Relaxed);
        lock(&self.tx).take();
        let tree = lock(&self.tree).take();
        if let Some(mut tree) = tree {
            if let Err(e) = tree.terminate(grace).await {
                tracing::warn!(pid = self.pid, error = %e, "backend tree did not terminate");
            }
        }
        fail_all(&self.pending, None);
    }
}

/// Fails every pending call with `err` but keeps the connection open.
fn fail_pending_with(pending: &Mutex<PendingMap>, err: &CallError) {
    let drained: Vec<Pending> = lock(pending).calls.drain().map(|(_, v)| v).collect();
    for call in drained {
        let _ = call.tx.send(Err(err.clone()));
    }
}

/// The broker id a response `id` refers to: a JSON number, or a string that
/// is the canonical decimal form of one (some servers echo ids as strings).
pub(crate) fn response_id(id: &Value) -> Option<u64> {
    match id {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse::<u64>().ok().filter(|n| n.to_string() == *s),
        _ => None,
    }
}

/// Hands a response to the pending call it answers. False when no call
/// matched (late, unknown or foreign id).
fn deliver_response(pending: &Mutex<PendingMap>, mut msg: Value) -> bool {
    let Some(id) = msg.get("id").and_then(response_id) else {
        return false;
    };
    let Some(call) = lock(pending).calls.remove(&id) else {
        return false;
    };
    msg["id"] = call.meta.orig_id;
    let _ = call.tx.send(Ok(msg));
    true
}

/// Fails every pending call and closes the map. With `crash` (the
/// connection died on its own) and diagnostics on, the lost calls are
/// recorded first so each can report its `crash_in_flight`.
fn fail_all(pending: &Mutex<PendingMap>, crash: Option<&ConnDiag>) {
    let drained: Vec<(u64, Pending)> = {
        let mut p = lock(pending);
        p.closed = true;
        p.calls.drain().collect()
    };
    if let Some(d) = crash {
        if !drained.is_empty() && d.tap.enabled() && !d.stopping.load(Ordering::Relaxed) {
            let bids: Vec<u64> = drained.iter().map(|(b, _)| *b).collect();
            *lock(&d.crash) = Some(CrashInfo {
                incident: d.tap.new_incident(d.generation),
                suspect: bids.iter().copied().max().unwrap_or(0),
                bids,
            });
        }
    }
    for (_, call) in drained {
        let _ = call.tx.send(Err(CallError::Closed));
    }
}

/// Fails pending calls and reports the connection closed once the root
/// process exits, after giving the reader [`EXIT_DRAIN`] to deliver what the
/// process wrote before exiting.
async fn exit_watch(
    server: String,
    pid: u32,
    exited: impl std::future::Future<Output = ()>,
    pending: Arc<Mutex<PendingMap>>,
    closed: Arc<watch::Sender<bool>>,
    diag: Arc<ConnDiag>,
) {
    let mut rx = closed.subscribe();
    tokio::select! {
        _ = exited => {}
        _ = rx.wait_for(|c| *c) => return,
    }
    if tokio::time::timeout(EXIT_DRAIN, rx.wait_for(|c| *c))
        .await
        .is_ok()
    {
        return;
    }
    tracing::debug!(server, pid, "backend root process exited");
    fail_all(&pending, Some(&diag));
    let _ = closed.send(true);
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
    /// The line exceeded the limit and was discarded (bytes consumed).
    TooLong(u64),
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
    let mut consumed: u64 = 0;
    loop {
        let avail = r.fill_buf().await?;
        if avail.is_empty() {
            return Ok(match (any, overflow) {
                (false, _) => Frame::Eof,
                (true, true) => Frame::TooLong(consumed),
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
        consumed += used as u64;
        r.consume(used);
        if done {
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(if overflow {
                Frame::TooLong(consumed)
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
    closed: Arc<watch::Sender<bool>>,
    client: ClientSide,
    wire: Arc<WireStats>,
    diag: Arc<ConnDiag>,
}

async fn read_loop(mut ctx: ReadCtx) {
    let mut buf = Vec::new();
    loop {
        match read_frame(&mut ctx.stdout, &mut buf, MAX_LINE).await {
            Ok(Frame::Line) => ctx.diag.counters.reach(Phase::StdoutSeen),
            Ok(Frame::TooLong(consumed)) => {
                ctx.diag.counters.reach(Phase::StdoutSeen);
                ctx.wire.oversize.fetch_add(1, Ordering::Relaxed);
                ctx.diag.oversize(consumed);
                tracing::warn!(server = %ctx.server, "backend line over 16 MiB dropped");
                // The dropped line may have been a response; which call it
                // answered is unknowable, so fail the pending ones rather
                // than leave them hanging.
                fail_pending_with(
                    &ctx.pending,
                    &CallError::Unavailable(
                        "backend response exceeded the 16 MiB line limit".into(),
                    ),
                );
                continue;
            }
            Ok(Frame::Eof) | Err(_) => break,
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(&buf) {
            Ok(msg) => dispatch(&ctx, msg, buf.len()),
            Err(_) => {
                ctx.wire.non_json.fetch_add(1, Ordering::Relaxed);
                ctx.diag.noise(&buf);
            }
        }
    }
    fail_all(&ctx.pending, Some(&ctx.diag));
    let _ = ctx.closed.send(true);
    tracing::debug!(server = %ctx.server, "backend stdout closed");
}

fn dispatch(ctx: &ReadCtx, msg: Value, len: usize) {
    match jsonrpc::kind(&msg) {
        Kind::Response => {
            let ftype = if msg.get("error").is_some() {
                FrameType::Error
            } else {
                FrameType::Response
            };
            let bid = msg.get("id").and_then(response_id);
            ctx.diag.frame(Dir::In, ftype, "", bid.unwrap_or(0), len);
            let Some(bid) = bid else {
                ctx.wire.foreign_ids.fetch_add(1, Ordering::Relaxed);
                ctx.diag.id_mismatch(id_type_of(&msg["id"]));
                tracing::debug!(server = %ctx.server, "response with foreign id dropped");
                return;
            };
            // The broker sends numeric ids; a string echo is a mismatch.
            if msg["id"].is_string() {
                ctx.diag.id_mismatch(IdType::String);
            }
            if !deliver_response(&ctx.pending, msg) {
                ctx.diag.late(bid);
                tracing::debug!(server = %ctx.server, "late or unknown response dropped");
            }
        }
        Kind::Notification => {
            ctx.diag.frame(
                Dir::In,
                FrameType::Notification,
                msg["method"].as_str().unwrap_or(""),
                0,
                len,
            );
            let sessions = lock(&ctx.pending).sessions();
            ctx.events
                .notification_from(&ctx.server, &ctx.client.scope_key, msg, sessions);
        }
        Kind::Request => {
            ctx.diag.frame(
                Dir::In,
                FrameType::Request,
                msg["method"].as_str().unwrap_or(""),
                0,
                len,
            );
            let sessions = lock(&ctx.pending).sessions();
            let tx = ctx.tx.clone();
            let reply = Reply::new(msg["id"].clone(), move |resp: Value| {
                let Some(tx) = tx.upgrade() else { return };
                let line = resp.to_string();
                if let Err(mpsc::error::TrySendError::Full(line)) = tx.try_send(line) {
                    if let Ok(h) = tokio::runtime::Handle::try_current() {
                        h.spawn(async move {
                            let _ = tx.send(line).await;
                        });
                    }
                }
            });
            handle_server_request(
                ctx.events.as_ref(),
                &ctx.server,
                &ctx.client,
                msg,
                sessions,
                reply,
            );
        }
        Kind::Invalid => {
            ctx.wire.invalid.fetch_add(1, Ordering::Relaxed);
            ctx.diag.invalid(len);
        }
    }
}

async fn stderr_loop(mut r: BufReader<tokio::process::ChildStderr>, ring: Arc<StderrRing>) {
    let mut buf = Vec::new();
    loop {
        match read_frame(&mut r, &mut buf, MAX_STDERR_LINE).await {
            Ok(Frame::Line) => ring.push(String::from_utf8_lossy(&buf).into_owned()),
            Ok(Frame::TooLong(_)) => ring.push("<line too long>".into()),
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
                (Frame::TooLong(11), vec![]),
                (Frame::Line, b"xyz".to_vec()),
            ]
        );
    }

    type Rx = oneshot::Receiver<Result<Value, CallError>>;

    fn pending_with(id: u64, orig: Value) -> (Mutex<PendingMap>, Rx) {
        let (tx, rx) = oneshot::channel();
        let mut m = PendingMap::default();
        m.calls.insert(
            id,
            Pending {
                tx,
                meta: CallMeta {
                    orig_id: orig,
                    ..CallMeta::default()
                },
            },
        );
        (Mutex::new(m), rx)
    }

    #[test]
    fn response_ids_accept_numbers_and_canonical_numeric_strings() {
        assert_eq!(response_id(&json!(7)), Some(7));
        assert_eq!(response_id(&json!("7")), Some(7));
        assert_eq!(response_id(&json!("007")), None);
        assert_eq!(response_id(&json!("+7")), None);
        assert_eq!(response_id(&json!(" 7")), None);
        assert_eq!(response_id(&json!("abc")), None);
        assert_eq!(response_id(&json!(-1)), None);
        assert_eq!(response_id(&json!(1.5)), None);
        assert_eq!(response_id(&json!(null)), None);
        assert_eq!(response_id(&json!({"a": 1})), None);
    }

    #[tokio::test]
    async fn string_id_response_reaches_pending_call_with_original_id() {
        let (p, rx) = pending_with(3, json!("agent-1"));
        assert!(deliver_response(
            &p,
            json!({"jsonrpc":"2.0","id":"3","result":{}})
        ));
        let v = rx.await.unwrap().unwrap();
        assert_eq!(v["id"], json!("agent-1"));
        assert!(lock(&p).calls.is_empty());
    }

    #[tokio::test]
    async fn unknown_or_foreign_id_leaves_pending_alone() {
        let (p, _rx) = pending_with(3, json!(1));
        assert!(!deliver_response(&p, json!({"id":"x","result":{}})));
        assert!(!deliver_response(&p, json!({"id":4,"result":{}})));
        assert_eq!(lock(&p).calls.len(), 1);
    }

    #[tokio::test]
    async fn oversize_failure_errors_pending_without_closing() {
        let (p, rx) = pending_with(1, json!(1));
        fail_pending_with(&p, &CallError::Unavailable("too big".into()));
        assert_eq!(
            rx.await.unwrap(),
            Err(CallError::Unavailable("too big".into()))
        );
        let g = lock(&p);
        assert!(g.calls.is_empty());
        assert!(!g.closed, "connection stays usable");
    }

    #[test]
    fn wire_stats_snapshot_counts() {
        let w = WireStats::default();
        w.non_json.fetch_add(2, Ordering::Relaxed);
        w.oversize.fetch_add(1, Ordering::Relaxed);
        let c = w.snapshot();
        assert_eq!(
            (c.non_json_lines, c.oversize_lines, c.foreign_ids),
            (2, 1, 0)
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
