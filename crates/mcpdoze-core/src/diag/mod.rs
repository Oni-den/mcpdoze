// SPDX-License-Identifier: GPL-3.0-or-later

//! Failure diagnostics: sanitised-by-construction records of why a backend
//! or a call failed (JSONL, schema v1). Off by default. See `docs/DEBUG.md`.
//!
//! # Pieces
//!
//! * [`Diag`]: the recorder, one per broker. Cheap to create (no I/O). On
//!   the first record it creates the debug directory (owner-only), the
//!   per-install salt and one writer thread (the `stats.rs` pattern: a
//!   bounded queue, `try_send` on the request path, dropped and counted when
//!   full). The writer applies flood control and rotation.
//! * [`DiagTap`]: one per backend instance (`Diag::tap`). It holds the
//!   [`Level`] in an `AtomicU8`, the frame and transition rings, and the
//!   capture API below.
//! * `record.rs` (schema), `shape.rs` (argument shapes and schema diff),
//!   `redact.rs` (scrubber, salt, URL reduction), `classify.rs` (cause),
//!   `ring.rs` (rings).
//!
//! # Cost
//!
//! * `Off`: every `on_*` method starts with one relaxed atomic load and
//!   returns. No allocation, no lock, no directory, no thread. Build the
//!   borrowed views ([`BackendView`], [`RequestView`], ...) as plain
//!   structs (they hold only primitives and borrows), but take a stderr
//!   snapshot or any other allocating input only after
//!   [`DiagTap::enabled`].
//! * `Failures`: a fixed-size [`ring::FrameMeta`] per frame; a shape walk
//!   (200 nodes, depth 6) per failed call; disk only on a failure record,
//!   off the request path.
//! * `Trace`: also the trace-only kinds (slow calls, noise, late responses,
//!   agent cancel or disconnect).
//!
//! # Capture points
//!
//! Wiring is one call per row. Every method is cheap to call when off.
//!
//! | Plan capture point | Call | Kind(s) |
//! |---|---|---|
//! | Spawn failed (`run_start`) | [`DiagTap::on_spawn_failed`] | `spawn_failed` |
//! | Start timeout | [`DiagTap::on_init_timeout`] | `init_timeout`, `init_no_output` |
//! | Handshake error or version | [`DiagTap::on_init_error`] | `init_error`, `version_mismatch` |
//! | Exit (`exit_watch`, `on_closed`) | [`DiagTap::on_exit`] | `exit_unexpected` |
//! | Crash mid-call (`fail_all`, one per in-flight call) | [`DiagTap::new_incident`] then [`DiagTap::on_crash_in_flight`] | `crash_in_flight` |
//! | Every frame sent or read | [`DiagTap::on_frame`] | ring only |
//! | Non-JSON stdout line | [`DiagTap::on_stdout_noise`] | `stdout_pollution` (trace) |
//! | Oversize line | [`DiagTap::on_frame_oversize`] | `frame_oversize` |
//! | JSON that is not JSON-RPC | [`DiagTap::on_frame_invalid`] | `frame_invalid` |
//! | `dispatch` id handling | [`DiagTap::on_id_type_mismatch`], [`DiagTap::on_late_response`] | `id_type_mismatch`, `late_response` (trace) |
//! | JSON-RPC error or `isError` | [`DiagTap::on_rpc_error`], [`DiagTap::on_tool_error`] | `rpc_error` |
//! | Slow, stalled, cancelled, disconnected | [`DiagTap::on_stalled`], [`DiagTap::on_slow_call`], [`DiagTap::on_agent_cancelled`], [`DiagTap::on_agent_disconnected`] | `stalled`, `slow_call`, `agent_cancelled`, `agent_disconnected` |
//! | Lifecycle transition | [`DiagTap::on_transition`] | ring only |
//! | Lifecycle race | [`DiagTap::on_race_unload`], [`DiagTap::on_backoff_rejected`] | `race_unload`, `backoff_rejected` |
//! | HTTP exchange | [`DiagTap::on_http`] | `http_status`, `http_auth`, `http_non_mcp_body`, `http_session_lost`, `http_timeout`, `sse_truncated`, `connect_failed` |
//! | Server-to-client request | [`DiagTap::on_client_request_unserved`] | `client_request_unserved` |
//! | `list_all` | [`DiagTap::on_list_failed`] | `list_failed`, `list_cursor_loop` |
//!
//! Anything else (a failure kind without a dedicated method) goes through
//! [`DiagTap::on_call_failed`].
//!
//! # Rules for callers
//!
//! * Pass borrowed data only; the tap copies what it needs, after the level
//!   check. Pass the tool arguments and the catalog `inputSchema` in
//!   [`RequestView`] at failure time; the tap reduces them to a shape and
//!   never stores them.
//! * Pass backend env and header values, and the broker token, once, in the
//!   [`ScrubContext`] given to [`Diag::tap`].
//! * A crash with several calls in flight: call [`DiagTap::new_incident`]
//!   once and pass the id to [`DiagTap::on_crash_in_flight`] for each call
//!   (set `suspect` on the last request sent).
//! * `Diag::new` takes the configuration from `[debug]`; mode changes at
//!   runtime go through [`DiagTap::set_level`] (no unload).

pub mod classify;
pub mod export;
pub mod record;
pub mod redact;
pub mod ring;
pub mod shape;

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

pub use classify::{classify, BodyClass, Cause, ClassifyInput, Confidence, Phase, Signature};
pub use record::{
    BackendRec, Era, ErrorRec, ExitRec, FrameRec, HttpRec, IdType, Identity, Kind, Name, Origin,
    Record, RepeatLine, RequestRec, Scope, Scrubbed, StderrRec, Transport, UrlInfo, SCHEMA_VERSION,
};
pub use redact::{launcher_basename, reduce_url, url_secret_parts, Salt, ScrubContext, Scrubber};
pub use ring::{Dir, FrameRing, FrameType, MethodTag, TransitionRing};
pub use shape::{analyse, schema_diff, ArgSet, SchemaDiff, ShapeNode, ShapeRoot};

use crate::stats::now_ms;

/// File the failure records go to, inside the debug directory.
pub const FILE_NAME: &str = "failures.jsonl";
/// Name of the salt file inside the debug directory.
pub const SALT_NAME: &str = ".salt";
const FLUSH_EVERY: Duration = Duration::from_secs(1);
const WINDOW_MS: u64 = 60_000;
const SIG_TTL_MS: u64 = 10 * 60_000;

/// How much is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    /// Nothing (default).
    Off = 0,
    /// Failures.
    Failures = 1,
    /// Failures plus slow calls, noise and other odd-but-successful events.
    Trace = 2,
}

impl Level {
    fn from_u8(v: u8) -> Level {
        match v {
            0 => Level::Off,
            1 => Level::Failures,
            _ => Level::Trace,
        }
    }

    /// `off`, `failures` or `trace`.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Off => "off",
            Level::Failures => "failures",
            Level::Trace => "trace",
        }
    }

    /// Parses `off`, `failures` or `trace`.
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Level::Off),
            "failures" => Some(Level::Failures),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }
}

/// Settings of the recorder (mapped from `[debug]` by the config layer).
#[derive(Debug, Clone)]
pub struct DiagConfig {
    /// Directory of the log, salt and (later) raw capture. Absolute.
    pub dir: PathBuf,
    /// Size cap of one file.
    pub max_file_bytes: u64,
    /// Rotated files kept.
    pub keep: usize,
    /// Frames kept per instance.
    pub frames: usize,
    /// Stderr lines attached to a record.
    pub stderr_lines: usize,
    /// Calls slower than this are `slow_call` (trace).
    pub slow_call: Duration,
    /// Also record `isError` tool results.
    pub tool_errors: bool,
    /// Records per minute and server before suppression.
    pub per_minute: u32,
    /// Queue length between the request path and the writer.
    pub queue: usize,
}

impl DiagConfig {
    /// Defaults with `dir` as the debug directory.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            max_file_bytes: 5 * 1024 * 1024,
            keep: 5,
            frames: ring::DEFAULT_FRAMES,
            stderr_lines: 40,
            slow_call: Duration::from_secs(30),
            tool_errors: false,
            per_minute: 20,
            queue: 1024,
        }
    }
}

// ------------------------------------------------------------ flood control

/// Identity of a failure for repeat detection (never written).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SigKey {
    kind: Kind,
    tool: Option<String>,
    shape: Option<u64>,
    exit: (Option<i32>, Option<i32>),
}

struct SigState {
    id: String,
    count: u32,
    last_ts: u64,
}

struct ServerFlood {
    ident: Identity,
    window: u64,
    count: u32,
    suppressed: u64,
    sigs: HashMap<SigKey, SigState>,
}

/// What to do with a record.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admit {
    /// Write it in full.
    Full,
    /// Write a compact repeat line for the record with id `of`.
    Repeat { of: String, n: u32 },
    /// Count it as suppressed.
    Skip,
}

/// Per-server rate limit and repeat detection. Time is the record's own
/// timestamp, so it is deterministic in tests.
pub(crate) struct Flood {
    per_minute: u32,
    servers: HashMap<String, ServerFlood>,
}

impl Flood {
    pub(crate) fn new(per_minute: u32) -> Self {
        Self {
            per_minute: per_minute.max(1),
            servers: HashMap::new(),
        }
    }

    fn summary(sf: &mut ServerFlood, ts: u64) -> Option<Record> {
        if sf.suppressed == 0 {
            return None;
        }
        let mut r = Record::new(Kind::Suppressed, sf.ident.clone(), ts);
        r.suppressed = Some(sf.suppressed);
        r.cause = Some(classify(&ClassifyInput::new(Kind::Suppressed)));
        sf.suppressed = 0;
        Some(r)
    }

    /// Decides for `rec`; may return a `suppressed` summary of the previous
    /// window to write first.
    pub(crate) fn admit(&mut self, rec: &Record, sig: &SigKey) -> (Option<Record>, Admit) {
        let per_minute = self.per_minute;
        let sf = self
            .servers
            .entry(rec.ident.server.as_str().to_string())
            .or_insert_with(|| ServerFlood {
                ident: rec.ident.clone(),
                window: rec.ts / WINDOW_MS,
                count: 0,
                suppressed: 0,
                sigs: HashMap::new(),
            });
        let mut summary = None;
        let window = rec.ts / WINDOW_MS;
        if window != sf.window {
            summary = Self::summary(sf, rec.ts);
            sf.window = window;
            sf.count = 0;
            sf.sigs
                .retain(|_, s| rec.ts.saturating_sub(s.last_ts) < SIG_TTL_MS);
        }
        let allowed = sf.count < per_minute;
        if let Some(s) = sf.sigs.get_mut(sig) {
            if rec.ts.saturating_sub(s.last_ts) < SIG_TTL_MS {
                s.count += 1;
                s.last_ts = rec.ts;
                if allowed {
                    sf.count += 1;
                    return (
                        summary,
                        Admit::Repeat {
                            of: s.id.clone(),
                            n: s.count,
                        },
                    );
                }
                sf.suppressed += 1;
                return (summary, Admit::Skip);
            }
        }
        if allowed {
            sf.count += 1;
            (summary, Admit::Full)
        } else {
            sf.suppressed += 1;
            (summary, Admit::Skip)
        }
    }

    /// Remembers the id of a full record, for later repeat lines.
    pub(crate) fn remember(&mut self, server: &str, sig: SigKey, id: &str, ts: u64) {
        if let Some(sf) = self.servers.get_mut(server) {
            sf.sigs.insert(
                sig,
                SigState {
                    id: id.to_string(),
                    count: 1,
                    last_ts: ts,
                },
            );
        }
    }

    /// Summaries for servers whose window has passed (`all`: every server
    /// with suppressed records, for flush and shutdown).
    pub(crate) fn expire(&mut self, now: u64, all: bool) -> Vec<Record> {
        let mut out = Vec::new();
        for sf in self.servers.values_mut() {
            if all || now / WINDOW_MS > sf.window {
                if let Some(r) = Self::summary(sf, now) {
                    out.push(r);
                }
            }
        }
        out
    }
}

// ------------------------------------------------------------------- writer

enum Msg {
    Record(Box<Record>, SigKey),
    Flush(SyncSender<()>),
}

#[derive(Default)]
struct Counters {
    dropped: AtomicU64,
    written: AtomicU64,
    write_errors: AtomicU64,
    active: AtomicU64,
}

/// Snapshot of the recorder's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagStats {
    /// Records dropped because the queue was full.
    pub dropped: u64,
    /// Lines written (full, repeat and suppressed).
    pub written: u64,
    /// Lines that could not be written.
    pub write_errors: u64,
    /// Capture calls that did work (zero while every tap is off).
    pub active: u64,
}

struct Sink<W: Write> {
    out: W,
    flood: Flood,
    next_id: u64,
    counters: Arc<Counters>,
}

impl<W: Write> Sink<W> {
    fn new(out: W, per_minute: u32, counters: Arc<Counters>) -> Self {
        Self {
            out,
            flood: Flood::new(per_minute),
            next_id: 1,
            counters,
        }
    }

    fn write_json<T: serde::Serialize>(&mut self, v: &T) {
        let Ok(mut line) = serde_json::to_vec(v) else {
            return;
        };
        line.push(b'\n');
        match self.out.write_all(&line) {
            Ok(()) => self.counters.written.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.counters.write_errors.fetch_add(1, Ordering::Relaxed),
        };
    }

    fn write_full(&mut self, mut rec: Record) -> String {
        rec.id = format!("f-{:06}", self.next_id);
        self.next_id += 1;
        self.write_json(&rec);
        rec.id
    }

    fn handle(&mut self, rec: Record, sig: SigKey) {
        let (summary, action) = self.flood.admit(&rec, &sig);
        if let Some(s) = summary {
            self.write_full(s);
        }
        match action {
            Admit::Full => {
                let server = rec.ident.server.as_str().to_string();
                let ts = rec.ts;
                let id = self.write_full(rec);
                self.flood.remember(&server, sig, &id, ts);
            }
            Admit::Repeat { of, n } => {
                let line = RepeatLine {
                    v: SCHEMA_VERSION,
                    ts: rec.ts,
                    kind: rec.kind,
                    server: rec.ident.server.clone(),
                    repeat: n,
                    of,
                };
                self.write_json(&line);
            }
            Admit::Skip => {}
        }
    }

    fn expire(&mut self, all: bool) {
        for r in self.flood.expire(now_ms(), all) {
            self.write_full(r);
        }
        if all {
            let _ = self.out.flush();
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        loop {
            match rx.recv_timeout(FLUSH_EVERY) {
                Ok(Msg::Record(r, s)) => self.handle(*r, s),
                Ok(Msg::Flush(ack)) => {
                    self.expire(true);
                    let _ = ack.send(());
                }
                Err(RecvTimeoutError::Timeout) => self.expire(false),
                Err(RecvTimeoutError::Disconnected) => {
                    self.expire(true);
                    return;
                }
            }
        }
    }
}

// --------------------------------------------------------------------- Diag

/// The failure recorder. Creating it does no I/O; the directory, salt and
/// writer thread appear on the first record.
pub struct Diag {
    cfg: DiagConfig,
    counters: Arc<Counters>,
    tx: OnceLock<Option<SyncSender<Msg>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    salt: OnceLock<Salt>,
}

impl std::fmt::Debug for Diag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Diag")
            .field("dir", &self.cfg.dir)
            .finish_non_exhaustive()
    }
}

impl Diag {
    /// A recorder for `cfg`. No I/O.
    pub fn new(cfg: DiagConfig) -> Arc<Diag> {
        Arc::new(Diag {
            cfg,
            counters: Arc::new(Counters::default()),
            tx: OnceLock::new(),
            thread: Mutex::new(None),
            salt: OnceLock::new(),
        })
    }

    /// The configuration.
    pub fn config(&self) -> &DiagConfig {
        &self.cfg
    }

    /// The debug directory.
    pub fn dir(&self) -> &Path {
        &self.cfg.dir
    }

    /// The failure log file.
    pub fn file(&self) -> PathBuf {
        self.cfg.dir.join(FILE_NAME)
    }

    /// Counters.
    pub fn stats(&self) -> DiagStats {
        DiagStats {
            dropped: self.counters.dropped.load(Ordering::Relaxed),
            written: self.counters.written.load(Ordering::Relaxed),
            write_errors: self.counters.write_errors.load(Ordering::Relaxed),
            active: self.counters.active.load(Ordering::Relaxed),
        }
    }

    /// The per-install salt (created on first use; a random in-memory salt
    /// when the file cannot be written).
    pub fn salt(&self) -> &Salt {
        self.salt.get_or_init(|| {
            Salt::load_or_create(&self.cfg.dir.join(SALT_NAME))
                .unwrap_or_else(|_| Salt::ephemeral())
        })
    }

    /// A tap for one backend instance. `scrub` holds the resolved env and
    /// header values and the broker token. The tap starts at `level`.
    pub fn tap(
        self: &Arc<Self>,
        ident: TapIdent,
        level: Level,
        scrub: Arc<ScrubContext>,
    ) -> Arc<DiagTap> {
        Arc::new(DiagTap::build(Some(self.clone()), ident, level, scrub))
    }

    fn start_writer(&self) -> Option<SyncSender<Msg>> {
        // The directory may pre-date us with looser permissions; the log is
        // sensitive even after scrubbing.
        for w in crate::auth::permissions_warnings(self.dir()) {
            tracing::warn!("{w}");
        }
        let log =
            RotatingLog::open_private(&self.file(), self.cfg.max_file_bytes, self.cfg.keep).ok()?;
        let (tx, rx) = mpsc::sync_channel(self.cfg.queue.max(1));
        let sink = Sink::new(log, self.cfg.per_minute, self.counters.clone());
        let t = std::thread::Builder::new()
            .name("mcpdoze-diag".into())
            .spawn(move || sink.run(rx))
            .ok()?;
        *self.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
        Some(tx)
    }

    fn sender(&self) -> Option<&SyncSender<Msg>> {
        self.tx.get_or_init(|| self.start_writer()).as_ref()
    }

    fn submit(&self, rec: Record, sig: SigKey) {
        let Some(tx) = self.sender() else {
            self.counters.write_errors.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match tx.try_send(Msg::Record(Box::new(rec), sig)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Flushes pending records (and any `suppressed` summaries) and waits up
    /// to `timeout`; blocking. True when confirmed. A recorder that never
    /// wrote has nothing to flush and returns true without creating
    /// anything.
    pub fn flush(&self, timeout: Duration) -> bool {
        let Some(Some(tx)) = self.tx.get() else {
            return true;
        };
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        let deadline = std::time::Instant::now() + timeout;
        let mut msg = Msg::Flush(ack_tx);
        loop {
            match tx.try_send(msg) {
                Ok(()) => break,
                Err(TrySendError::Full(m)) => {
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    msg = m;
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        ack_rx
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .is_ok()
    }
}

impl Drop for Diag {
    fn drop(&mut self) {
        drop(self.tx.take());
        let t = self
            .thread
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(t) = t {
            let _ = t.join();
        }
    }
}

// ------------------------------------------------------------------- inputs

/// Static identity of the instance a tap belongs to.
#[derive(Debug, Clone)]
pub struct TapIdent {
    /// Server name.
    pub server: String,
    /// Transport.
    pub transport: Transport,
    /// The launcher command as configured (reduced to its basename).
    pub launcher: Option<String>,
    /// Instance scope.
    pub scope: Scope,
    /// Raw instance key (directory, session id): hashed, never written.
    pub instance_key: Option<String>,
}

/// Backend state at the time of a capture (borrowed; no allocation).
#[derive(Debug, Clone, Copy)]
pub struct BackendView<'a> {
    /// Lifecycle state name.
    pub state: &'static str,
    /// Previous state name.
    pub prev_state: Option<&'static str>,
    /// Respawn count.
    pub generation: u64,
    /// Process uptime.
    pub uptime_ms: u64,
    /// Calls in flight.
    pub in_flight: u64,
    /// Calls awaiting a backend response.
    pub pending: u64,
    /// Calls served.
    pub calls_served: u64,
    /// Restarts.
    pub restarts: u64,
    /// Failures.
    pub failures: u64,
    /// Negotiated protocol version.
    pub protocol: Option<&'a str>,
    /// `serverInfo` name and version.
    pub server_info: Option<(&'a str, Option<&'a str>)>,
    /// Capability keys.
    pub caps: &'a [&'a str],
}

impl BackendView<'_> {
    /// A view with only the state and generation set.
    pub fn new(state: &'static str, generation: u64) -> Self {
        Self {
            state,
            prev_state: None,
            generation,
            uptime_ms: 0,
            in_flight: 0,
            pending: 0,
            calls_served: 0,
            restarts: 0,
            failures: 0,
            protocol: None,
            server_info: None,
            caps: &[],
        }
    }
}

/// The request being served (borrowed).
#[derive(Debug, Clone, Copy)]
pub struct RequestView<'a> {
    /// Per-process request number (same as the stats `req_id`).
    pub req: u64,
    /// Protocol era.
    pub era: Era,
    /// Method.
    pub method: &'a str,
    /// Tool name (without prefix).
    pub tool: Option<&'a str>,
    /// JSON type of the request id.
    pub id_type: Option<IdType>,
    /// The agent asked for progress notifications.
    pub progress: bool,
    /// Request size in bytes.
    pub bytes: u64,
    /// Time since the request arrived.
    pub elapsed_ms: u64,
    /// Time spent obtaining a backend.
    pub acquire_ms: u64,
    /// The backend was cold.
    pub cold_start: bool,
    /// The last request sent before a crash.
    pub suspect: bool,
    /// Tool arguments (reduced to a shape; never stored).
    pub arguments: Option<&'a Value>,
    /// The tool's catalog `inputSchema`.
    pub input_schema: Option<&'a Value>,
    /// Params of a non-`tools/call` request (string leaves only feed the
    /// scrubber; never stored).
    pub params: Option<&'a Value>,
}

impl<'a> RequestView<'a> {
    /// A view with the identifying fields set and everything else zero.
    pub fn new(req: u64, era: Era, method: &'a str) -> Self {
        Self {
            req,
            era,
            method,
            tool: None,
            id_type: None,
            progress: false,
            bytes: 0,
            elapsed_ms: 0,
            acquire_ms: 0,
            cold_start: false,
            suspect: false,
            arguments: None,
            input_schema: None,
            params: None,
        }
    }
}

/// Process exit.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExitView {
    /// Exit code.
    pub code: Option<i32>,
    /// Unix signal.
    pub signal: Option<i32>,
}

/// The stderr tail (lines oldest first; scrubbed by the tap).
#[derive(Debug, Clone, Copy)]
pub struct StderrView<'a> {
    /// Lines written since spawn.
    pub total: u64,
    /// The newest lines.
    pub lines: &'a [String],
    /// Arrival time of each line, unix ms (same length as `lines`). Lines
    /// are dropped when the lengths differ: the tap cannot tell whose they
    /// are.
    pub ts: &'a [u64],
}

/// An error to attach to a record.
#[derive(Debug, Clone, Copy)]
pub struct ErrorView<'a> {
    /// Source.
    pub origin: Origin,
    /// JSON-RPC code.
    pub code: Option<i64>,
    /// Message (scrubbed by the tap).
    pub message: Option<&'a str>,
    /// Error `data` (reduced to a shape).
    pub data: Option<&'a Value>,
}

/// An HTTP exchange (borrowed).
#[derive(Debug, Clone, Copy, Default)]
pub struct HttpView<'a> {
    /// Request URL (reduced to scheme, host hash, path depth).
    pub url: Option<&'a str>,
    /// Status code.
    pub status: Option<u16>,
    /// `content-type` header.
    pub content_type: Option<&'a str>,
    /// Body length.
    pub body_len: Option<u64>,
    /// Body class.
    pub body_class: Option<BodyClass>,
    /// `www-authenticate` scheme (first word only is kept).
    pub www_authenticate: Option<&'a str>,
    /// `retry-after` in seconds.
    pub retry_after_s: Option<u64>,
    /// Whether a session id was held.
    pub session_present: Option<bool>,
    /// Transport error class (`timeout`, `connect`, `tls`, ...).
    pub transport_error: Option<&'static str>,
    /// The auth helper was re-run.
    pub helper_rerun: bool,
}

/// Why a backend-to-client request was not served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unserved {
    /// The agent does not support the method.
    Unsupported,
    /// The agent did not answer in time.
    TimedOut,
    /// No agent session to route it to.
    NotRouted,
}

impl Unserved {
    fn as_str(self) -> &'static str {
        match self {
            Unserved::Unsupported => "unsupported",
            Unserved::TimedOut => "timed out",
            Unserved::NotRouted => "not routed",
        }
    }
}

// ------------------------------------------------------------------- DiagTap

#[derive(Default)]
struct Draft<'a> {
    kind: Option<Kind>,
    backend: Option<&'a BackendView<'a>>,
    request: Option<&'a RequestView<'a>>,
    exit: Option<ExitView>,
    http: Option<&'a HttpView<'a>>,
    error: Option<ErrorView<'a>>,
    stderr: Option<&'a StderrView<'a>>,
    incident: Option<&'a str>,
    frames: bool,
    transitions: bool,
    phase: Option<Phase>,
    signature: Option<Signature>,
    io_kind: Option<io::ErrorKind>,
    hints: Vec<String>,
}

impl<'a> Draft<'a> {
    fn new(kind: Kind) -> Self {
        Draft {
            kind: Some(kind),
            frames: true,
            ..Draft::default()
        }
    }
}

/// Per-instance capture handle: the level, the rings and the capture API.
/// See the module docs for the capture-point table.
pub struct DiagTap {
    level: AtomicU8,
    diag: Option<Arc<Diag>>,
    src: TapIdent,
    ident: OnceLock<Identity>,
    scrub: Mutex<Arc<ScrubContext>>,
    frames: FrameRing,
    transitions: TransitionRing,
    noise: AtomicU64,
    incidents: AtomicU64,
    /// Unix ms of the last good response from the backend.
    last_good_ms: AtomicU64,
    /// Arguments of the most recent calls (leaves only), so a stderr line
    /// caused by one call is scrubbed in the record of another.
    recent_args: Mutex<VecDeque<ArgSet>>,
    /// Cheap per-tap rate gate: (window start ms, records in the window).
    gate: Mutex<(u64, u32)>,
}

const RECENT_CALLS: usize = 8;
const GATE_WINDOW_MS: u64 = 10_000;
const GATE_MAX: u32 = 30;

impl std::fmt::Debug for DiagTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiagTap")
            .field("server", &self.src.server)
            .field("level", &self.level())
            .finish_non_exhaustive()
    }
}

impl DiagTap {
    fn build(
        diag: Option<Arc<Diag>>,
        src: TapIdent,
        level: Level,
        scrub: Arc<ScrubContext>,
    ) -> Self {
        let frames = diag.as_ref().map_or(ring::DEFAULT_FRAMES, |d| d.cfg.frames);
        DiagTap {
            level: AtomicU8::new(if diag.is_some() { level as u8 } else { 0 }),
            diag,
            src,
            ident: OnceLock::new(),
            scrub: Mutex::new(scrub),
            frames: FrameRing::new(frames),
            transitions: TransitionRing::default(),
            noise: AtomicU64::new(0),
            incidents: AtomicU64::new(0),
            last_good_ms: AtomicU64::new(0),
            recent_args: Mutex::new(VecDeque::new()),
            gate: Mutex::new((0, 0)),
        }
    }

    /// A tap that is always off and has no recorder (for tests and code
    /// paths without diagnostics).
    pub fn disabled() -> Arc<DiagTap> {
        Arc::new(DiagTap::build(
            None,
            TapIdent {
                server: String::new(),
                transport: Transport::Stdio,
                launcher: None,
                scope: Scope::Shared,
                instance_key: None,
            },
            Level::Off,
            Arc::new(ScrubContext::default()),
        ))
    }

    /// The current level (one relaxed load).
    #[inline]
    pub fn level(&self) -> Level {
        Level::from_u8(self.level.load(Ordering::Relaxed))
    }

    /// True unless the level is `Off`. Gate any allocating input on this.
    #[inline]
    pub fn enabled(&self) -> bool {
        self.level.load(Ordering::Relaxed) != 0
    }

    /// Changes the level without touching the backend. Ignored on a tap
    /// without a recorder.
    pub fn set_level(&self, level: Level) {
        if self.diag.is_some() {
            self.level.store(level as u8, Ordering::Relaxed);
        }
    }

    /// The `slow_call` threshold, for the caller's timer.
    pub fn slow_call_threshold(&self) -> Duration {
        self.diag
            .as_ref()
            .map_or(Duration::from_secs(30), |d| d.cfg.slow_call)
    }

    /// Whether `isError` tool results are recorded (`tool_errors`).
    pub fn tool_errors(&self) -> bool {
        self.diag.as_ref().is_some_and(|d| d.cfg.tool_errors)
    }

    /// Frames currently in the ring (tests, `status`).
    pub fn frames_held(&self) -> usize {
        self.frames.len()
    }

    fn identity(&self, diag: &Diag) -> &Identity {
        self.ident.get_or_init(|| Identity {
            server: Name::new(&self.src.server),
            transport: self.src.transport,
            launcher: self.src.launcher.as_deref().map(launcher_basename),
            scope: self.src.scope,
            instance: self
                .src
                .instance_key
                .as_deref()
                .map(|k| diag.salt().hash8("instance", k)),
        })
    }

    /// Adds secrets learned after the tap was built (a `headers_helper`
    /// refresh): later records scrub them as exact values.
    pub fn add_secrets(&self, more: impl IntoIterator<Item = (String, String)>) {
        let mut g = self.scrub.lock().unwrap_or_else(|e| e.into_inner());
        let next = (**g).clone().with_secrets(more);
        *g = Arc::new(next);
    }

    /// Remembers the arguments of a call that is starting (leaves only, in
    /// memory), for scrubbing the records of later events.
    pub fn remember_call_args(&self, v: &Value) {
        if !self.enabled() {
            return;
        }
        let mut a = ArgSet::new();
        a.add_all_leaves(v);
        if a.leaves().is_empty() {
            return;
        }
        let mut g = self.recent_args.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() == RECENT_CALLS {
            g.pop_front();
        }
        g.push_back(a);
    }

    /// Cheap rate gate, checked before any record is built.
    fn gate_ok(&self, now: u64) -> bool {
        let mut g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if now.saturating_sub(g.0) >= GATE_WINDOW_MS {
            *g = (now, 0);
        }
        if g.1 >= GATE_MAX {
            return false;
        }
        g.1 += 1;
        true
    }

    /// Builds and submits one record. Everything below runs only at an
    /// active level.
    fn emit(&self, d: Draft<'_>) {
        let Some(kind) = d.kind else { return };
        let lvl = self.level();
        if lvl == Level::Off || (kind.trace_only() && lvl < Level::Trace) {
            return;
        }
        let Some(diag) = &self.diag else { return };
        diag.counters.active.fetch_add(1, Ordering::Relaxed);
        if !self.gate_ok(now_ms()) {
            diag.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let salt = diag.salt();
        let ident = self.identity(diag).clone();
        let scrub = self.scrub.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let no_args = ArgSet::new();
        let sc0 = Scrubber::new(&scrub, salt, &no_args);
        // A name taken from the wire: character-filtered and also checked
        // for user names, host names, secrets and addresses.
        let wire_name = |n: &str| Name::new(sc0.scrub(n).as_str());
        let mut rec = Record::new(kind, ident, now_ms());
        rec.incident = d.incident.map(str::to_string);

        let trans = if d.transitions || d.backend.is_some() {
            self.transitions.snapshot()
        } else {
            Vec::new()
        };
        if let Some(b) = d.backend {
            rec.generation = b.generation;
            rec.backend = Some(BackendRec {
                state: b.state,
                prev_state: b.prev_state,
                uptime_ms: b.uptime_ms,
                in_flight: b.in_flight,
                pending: b.pending,
                calls_served: b.calls_served,
                restarts: b.restarts,
                failures: b.failures,
                last_transition: trans.last().cloned(),
                protocol: b.protocol.map(Name::new),
                server_info: b.server_info.map(|(n, v)| record::ServerInfoRec {
                    name: wire_name(n),
                    version: v.map(wire_name),
                }),
                caps: b.caps.iter().map(|c| Name::new(c)).collect(),
                stdout_noise: self.noise.load(Ordering::Relaxed),
            });
        }
        if d.transitions {
            rec.transitions = trans;
        }

        let mut args = ArgSet::new();
        let mut diff = None;
        let mut sig = SigKey {
            kind,
            tool: None,
            shape: None,
            exit: (d.exit.and_then(|e| e.code), d.exit.and_then(|e| e.signal)),
        };
        if let Some(r) = d.request {
            let mut shape = None;
            if let Some(a) = r.arguments {
                let an = analyse(a, r.input_schema, salt);
                sig.shape = Some(an.shape_hash);
                diff = an.diff;
                shape = Some(an.shape);
                args = an.args;
            }
            if let Some(p) = r.params {
                args.add_all_leaves(p);
            }
            sig.tool = r.tool.map(str::to_string);
            rec.request = Some(RequestRec {
                req: r.req,
                era: r.era,
                method: Name::new(r.method),
                tool: r.tool.map(wire_name),
                id_type: r.id_type,
                progress: r.progress,
                bytes: r.bytes,
                elapsed_ms: r.elapsed_ms,
                acquire_ms: r.acquire_ms,
                cold_start: r.cold_start,
                suspect: r.suspect,
                shape,
                schema_diff: diff.clone(),
            });
        }
        for other in self
            .recent_args
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            args.absorb(other);
        }
        let sc = Scrubber::new(&scrub, salt, &args);

        if let Some(e) = d.exit {
            rec.exit = Some(ExitRec {
                code: e.code,
                signal: e.signal,
                status_name: e.code.and_then(classify::ntstatus_name),
            });
        }
        if let Some(h) = d.http {
            rec.http = Some(HttpRec {
                url: h.url.and_then(|u| reduce_url(salt, u)),
                status: h.status,
                content_type: h
                    .content_type
                    .map(|c| Name::new(c.split(';').next().unwrap_or("").trim())),
                body_len: h.body_len,
                body_class: h.body_class,
                www_authenticate: h
                    .www_authenticate
                    .map(|w| Name::new(w.split_whitespace().next().unwrap_or(""))),
                retry_after_s: h.retry_after_s,
                session_present: h.session_present,
                transport_error: h.transport_error,
                helper_rerun: h.helper_rerun,
            });
        }
        if let Some(e) = d.error {
            rec.error = Some(ErrorRec {
                origin: e.origin,
                code: e.code,
                message: e.message.map(|m| sc.scrub(m)),
                data_shape: e.data.map(|v| analyse(v, None, salt).shape),
            });
        }
        if let Some(s) = d.stderr {
            // Only lines this event can own: since the call started, or for
            // lifecycle events since the last good response. Lines from
            // other calls or sessions are not attached.
            let cutoff = match d.request {
                Some(r) => now_ms().saturating_sub(r.elapsed_ms),
                None => self.last_good_ms.load(Ordering::Relaxed),
            };
            let own: Vec<&String> = if s.ts.len() == s.lines.len() {
                s.lines
                    .iter()
                    .zip(s.ts)
                    .filter(|(_, t)| **t >= cutoff)
                    .map(|(l, _)| l)
                    .collect()
            } else {
                Vec::new()
            };
            let keep = diag.cfg.stderr_lines;
            let skip = own.len().saturating_sub(keep);
            rec.stderr = Some(StderrRec {
                total: s.total,
                tail: own[skip..].iter().map(|l| sc.stderr_line(l)).collect(),
            });
        }
        if d.frames {
            rec.frames = self.frames.snapshot();
        }

        let ident_launcher = rec.ident.launcher.as_ref().map(Name::as_str);
        let mut cause = classify(&ClassifyInput {
            kind,
            launcher: ident_launcher,
            exit: rec.exit.as_ref(),
            signature: d.signature,
            schema_diff: diff.as_ref(),
            phase: d.phase,
            io_kind: d.io_kind,
            rpc_code: d.error.and_then(|e| e.code),
            http_status: d.http.and_then(|h| h.status),
            body_class: d.http.and_then(|h| h.body_class),
        });
        cause
            .hints
            .extend(d.hints.into_iter().map(Scrubbed::from_safe));
        rec.cause = Some(cause);
        diag.submit(rec, sig);
    }

    // ---------------------------------------------------------- lifecycle

    /// The process could not be started. `cmd_shim`: the launcher is a
    /// Windows `.cmd`/`.bat` shim.
    pub fn on_spawn_failed(
        &self,
        io_kind: io::ErrorKind,
        cmd_shim: bool,
        backend: &BackendView<'_>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut d = Draft::new(Kind::SpawnFailed);
        d.backend = Some(backend);
        d.io_kind = Some(io_kind);
        d.frames = false;
        d.hints.push(format!("io error kind: {io_kind:?}"));
        if cmd_shim {
            d.hints.push("launcher is a .cmd or .bat shim".to_string());
        }
        self.emit(d);
    }

    /// `initialize` was not answered in time. `phase` is how far start-up
    /// got; no stdout at all gives `init_no_output`.
    pub fn on_init_timeout(
        &self,
        phase: Phase,
        backend: &BackendView<'_>,
        stderr: Option<&StderrView<'_>>,
    ) {
        if !self.enabled() {
            return;
        }
        let kind = if matches!(phase, Phase::Spawned | Phase::InitSent) {
            Kind::InitNoOutput
        } else {
            Kind::InitTimeout
        };
        let mut d = Draft::new(kind);
        d.backend = Some(backend);
        d.stderr = stderr;
        d.phase = Some(phase);
        self.emit(d);
    }

    /// The handshake failed: a JSON-RPC error (`rpc`) or a protocol version
    /// other than the one requested (`returned`).
    pub fn on_init_error(
        &self,
        requested: &str,
        returned: Option<&str>,
        rpc: Option<(i64, &str)>,
        backend: &BackendView<'_>,
        stderr: Option<&StderrView<'_>>,
    ) {
        if !self.enabled() {
            return;
        }
        let kind = if rpc.is_some() {
            Kind::InitError
        } else {
            Kind::VersionMismatch
        };
        let mut d = Draft::new(kind);
        d.backend = Some(backend);
        d.stderr = stderr;
        d.phase = Some(Phase::InitReplied);
        d.error = rpc.map(|(code, message)| ErrorView {
            origin: Origin::Backend,
            code: Some(code),
            message: Some(message),
            data: None,
        });
        d.hints.push(format!(
            "requested {}, returned {}",
            Name::new(requested).as_str(),
            returned.map_or_else(|| "none".to_string(), |r| Name::new(r).as_str().to_string())
        ));
        self.emit(d);
    }

    /// The backend exited while it should run.
    pub fn on_exit(
        &self,
        exit: ExitView,
        backend: &BackendView<'_>,
        stderr: Option<&StderrView<'_>>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut d = Draft::new(Kind::ExitUnexpected);
        d.backend = Some(backend);
        d.exit = Some(exit);
        d.stderr = stderr;
        self.emit(d);
    }

    /// A new incident id (`i-<generation>-<hex4>`) to link the records of
    /// one crash. Empty when off.
    pub fn new_incident(&self, generation: u64) -> String {
        if !self.enabled() {
            return String::new();
        }
        let n = self.incidents.fetch_add(1, Ordering::Relaxed);
        format!("i-{generation}-{:04x}", (n ^ now_ms()) & 0xffff)
    }

    /// One in-flight call died with the backend (call once per call, with
    /// the same `incident`). Set `request.suspect` on the last request sent.
    pub fn on_crash_in_flight(
        &self,
        incident: &str,
        exit: ExitView,
        request: &RequestView<'_>,
        backend: &BackendView<'_>,
        stderr: Option<&StderrView<'_>>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut d = Draft::new(Kind::CrashInFlight);
        d.incident = Some(incident);
        d.backend = Some(backend);
        d.request = Some(request);
        d.exit = Some(exit);
        d.stderr = stderr;
        d.error = Some(ErrorView {
            origin: Origin::Broker,
            code: Some(-32000),
            message: Some("backend connection closed"),
            data: None,
        });
        self.emit(d);
    }

    /// A lifecycle transition (ring only; shows up as context).
    pub fn on_transition(&self, to: &'static str, reason: &'static str, in_flight: usize) {
        if !self.enabled() {
            return;
        }
        self.transitions.push(to, reason, in_flight);
    }

    // ------------------------------------------------------------- framing

    /// One frame sent or read (ring only). `method` is empty for responses.
    pub fn on_frame(&self, dir: Dir, ftype: FrameType, method: &str, bid: u64, bytes: usize) {
        if !self.enabled() {
            return;
        }
        if dir == Dir::In && ftype == FrameType::Response {
            self.last_good_ms.store(now_ms(), Ordering::Relaxed);
        }
        self.frames
            .push(dir, ftype, MethodTag::of(method), bid, bytes, None);
    }

    /// A stdout line that is not JSON. Counted, put in the ring, and
    /// recorded as `stdout_pollution` at `trace`.
    pub fn on_stdout_noise(&self, line: &[u8], backend: &BackendView<'_>) {
        if !self.enabled() {
            return;
        }
        let sig = Signature::of(line);
        self.noise.fetch_add(1, Ordering::Relaxed);
        self.frames.push(
            Dir::In,
            FrameType::Noise,
            MethodTag::NONE,
            0,
            line.len(),
            Some(sig),
        );
        let mut d = Draft::new(Kind::StdoutPollution);
        d.backend = Some(backend);
        d.signature = Some(sig);
        self.emit(d);
    }

    /// A line over the frame limit was dropped (`bytes_consumed`).
    pub fn on_frame_oversize(&self, bytes_consumed: u64, backend: &BackendView<'_>) {
        if !self.enabled() {
            return;
        }
        self.frames.push(
            Dir::In,
            FrameType::Oversize,
            MethodTag::NONE,
            0,
            usize::try_from(bytes_consumed).unwrap_or(usize::MAX),
            None,
        );
        let mut d = Draft::new(Kind::FrameOversize);
        d.backend = Some(backend);
        d.hints.push(format!(
            "consumed {bytes_consumed} bytes with {} calls pending",
            backend.pending
        ));
        self.emit(d);
    }

    /// JSON that is not a valid JSON-RPC 2.0 message.
    pub fn on_frame_invalid(&self, bytes: usize, backend: &BackendView<'_>) {
        if !self.enabled() {
            return;
        }
        self.frames
            .push(Dir::In, FrameType::Invalid, MethodTag::NONE, 0, bytes, None);
        let mut d = Draft::new(Kind::FrameInvalid);
        d.backend = Some(backend);
        self.emit(d);
    }

    /// A response carried an id of another JSON type than the request's
    /// (`id_type` is the type seen).
    pub fn on_id_type_mismatch(&self, id_type: IdType, backend: &BackendView<'_>) {
        if !self.enabled() {
            return;
        }
        let mut d = Draft::new(Kind::IdTypeMismatch);
        d.backend = Some(backend);
        d.hints.push(format!("response id type: {id_type:?}"));
        self.emit(d);
    }

    /// A response for a call that is no longer pending (trace).
    pub fn on_late_response(&self, bid: u64, backend: &BackendView<'_>) {
        if !self.enabled() {
            return;
        }
        let mut d = Draft::new(Kind::LateResponse);
        d.backend = Some(backend);
        d.hints.push(format!("backend id {bid}"));
        self.emit(d);
    }

    // --------------------------------------------------------------- calls

    /// A JSON-RPC error answered a call.
    pub fn on_rpc_error(
        &self,
        request: &RequestView<'_>,
        code: i64,
        message: &str,
        data: Option<&Value>,
        backend: &BackendView<'_>,
        stderr: Option<&StderrView<'_>>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut d = Draft::new(Kind::RpcError);
        d.backend = Some(backend);
        d.request = Some(request);
        d.stderr = stderr;
        d.error = Some(ErrorView {
            origin: Origin::Backend,
            code: Some(code),
            message: Some(message),
            data,
        });
        self.emit(d);
    }

    /// A tool result had `isError: true`. Recorded only when `tool_errors`
    /// is set. `message` is the first text block, if any.
    pub fn on_tool_error(
        &self,
        request: &RequestView<'_>,
        message: Option<&str>,
        backend: &BackendView<'_>,
        stderr: Option<&StderrView<'_>>,
    ) {
        if !self.enabled() || !self.tool_errors() {
            return;
        }
        let mut d = Draft::new(Kind::RpcError);
        d.backend = Some(backend);
        d.request = Some(request);
        d.stderr = stderr;
        d.error = Some(ErrorView {
            origin: Origin::Backend,
            code: None,
            message,
            data: None,
        });
        d.hints.push("tool result with isError".to_string());
        self.emit(d);
    }

    /// Generic call failure for kinds without a dedicated method.
    pub fn on_call_failed(
        &self,
        kind: Kind,
        request: &RequestView<'_>,
        error: Option<ErrorView<'_>>,
        backend: &BackendView<'_>,
        stderr: Option<&StderrView<'_>>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut d = Draft::new(kind);
        d.backend = Some(backend);
        d.request = Some(request);
        d.stderr = stderr;
        d.error = error;
        if let Some(age) = self.frames.last_age_ms(Dir::In) {
            d.hints
                .push(format!("last frame from the backend {age} ms ago"));
        }
        self.emit(d);
    }

    /// No answer for too long (`stalled`).
    pub fn on_stalled(
        &self,
        request: &RequestView<'_>,
        backend: &BackendView<'_>,
        stderr: Option<&StderrView<'_>>,
    ) {
        self.on_call_failed(Kind::Stalled, request, None, backend, stderr);
    }

    /// A call exceeded the `slow_call` threshold (trace).
    pub fn on_slow_call(&self, request: &RequestView<'_>, backend: &BackendView<'_>) {
        self.on_call_failed(Kind::SlowCall, request, None, backend, None);
    }

    /// The agent cancelled the call (trace).
    pub fn on_agent_cancelled(&self, request: &RequestView<'_>, backend: &BackendView<'_>) {
        self.on_call_failed(Kind::AgentCancelled, request, None, backend, None);
    }

    /// The agent disconnected while its call ran (trace).
    pub fn on_agent_disconnected(&self, request: &RequestView<'_>, backend: &BackendView<'_>) {
        self.on_call_failed(Kind::AgentDisconnected, request, None, backend, None);
    }

    // ---------------------------------------------------------- lifecycle races

    /// A call raced with an unload or restart; carries the last 16
    /// transitions.
    pub fn on_race_unload(&self, reason: &'static str, backend: &BackendView<'_>) {
        if !self.enabled() {
            return;
        }
        let mut d = Draft::new(Kind::RaceUnload);
        d.backend = Some(backend);
        d.transitions = true;
        d.hints.push(format!("raced at: {reason}"));
        self.emit(d);
    }

    /// A start was refused during restart backoff; carries the last 16
    /// transitions.
    pub fn on_backoff_rejected(&self, backend: &BackendView<'_>) {
        if !self.enabled() {
            return;
        }
        let mut d = Draft::new(Kind::BackoffRejected);
        d.backend = Some(backend);
        d.transitions = true;
        self.emit(d);
    }

    // ------------------------------------------------------------------ HTTP

    /// An HTTP exchange failed. `kind` is one of `http_status`,
    /// `http_auth`, `http_non_mcp_body`, `http_session_lost`,
    /// `http_timeout`, `sse_truncated`, `connect_failed`.
    pub fn on_http(
        &self,
        kind: Kind,
        request: Option<&RequestView<'_>>,
        http: &HttpView<'_>,
        backend: &BackendView<'_>,
    ) {
        if !self.enabled() {
            return;
        }
        debug_assert!(matches!(
            kind,
            Kind::HttpStatus
                | Kind::HttpAuth
                | Kind::HttpNonMcpBody
                | Kind::HttpSessionLost
                | Kind::HttpTimeout
                | Kind::SseTruncated
                | Kind::ConnectFailed
        ));
        let mut d = Draft::new(kind);
        d.backend = Some(backend);
        d.request = request;
        d.http = Some(http);
        d.frames = false;
        self.emit(d);
    }

    // ------------------------------------------------- server-to-client, lists

    /// A request from the backend to the agent was not served.
    pub fn on_client_request_unserved(
        &self,
        method: &str,
        reason: Unserved,
        backend: &BackendView<'_>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut d = Draft::new(Kind::ClientRequestUnserved);
        d.backend = Some(backend);
        d.frames = false;
        d.hints.push(format!(
            "method {}, {}",
            Name::new(method).as_str(),
            reason.as_str()
        ));
        self.emit(d);
    }

    /// A list call failed (`list_failed`) or a cursor repeated
    /// (`list_cursor_loop`).
    pub fn on_list_failed(
        &self,
        cursor_loop: bool,
        method: &str,
        elapsed_ms: u64,
        pages: u32,
        backend: &BackendView<'_>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut d = Draft::new(if cursor_loop {
            Kind::ListCursorLoop
        } else {
            Kind::ListFailed
        });
        d.backend = Some(backend);
        d.frames = false;
        d.hints.push(format!(
            "{} after {pages} pages, {elapsed_ms} ms",
            Name::new(method).as_str()
        ));
        self.emit(d);
    }
}

pub use crate::logfile::RotatingLog;

#[cfg(test)]
mod tests;
