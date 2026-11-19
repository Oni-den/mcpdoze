// SPDX-License-Identifier: GPL-3.0-or-later

//! Usage statistics: an append-only JSONL log for comparing the broker with
//! per-session (direct) MCP servers over time. See `docs/STATS.md`.
//!
//! * Files: `<dir>/stats-YYYY-MM.jsonl` (UTC month of the event), one JSON
//!   object per line, tagged by `ev` (`call`, `spawn`, `unload`, `session`,
//!   `sample`).
//! * The request path only does a non-blocking `try_send` into a bounded
//!   queue ([`QUEUE`]); when it is full the event is dropped and counted
//!   (`dropped` in every sample). One writer thread owns the file, buffers
//!   writes and flushes about once a second.
//! * Privacy: no tool arguments or results, no resource URIs (a short hash
//!   instead), no directories or session ids (short hashes of the instance
//!   key; session ids are only kept in memory to count concurrency).
//! * Retention: a monthly file is deleted once its whole month is older
//!   than `retention_days`; checked at start and every few hours.
//! * The "direct" counterfactual of each sample: per server, the last
//!   measured private memory of one instance times the sessions that called
//!   it during the sample window. It is an estimate (see `docs/STATS.md`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::StatsConfig;
use crate::proctree::TreeMemory;

/// Events buffered between the request path and the writer thread.
pub const QUEUE: usize = 4096;
/// Longest time an event stays in the write buffer.
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// How often retention is enforced while running.
const PRUNE_EVERY: Duration = Duration::from_secs(6 * 3600);
const MS_PER_DAY: u64 = 86_400_000;
const MB: f64 = 1024.0 * 1024.0;
/// Session key used for stateless (2026-07-28 / sessionless) calls: they
/// count as one session per server and window.
pub const STATELESS_SESSION: u64 = 0;

/// Calls remembered per session for the duplicate-call metric.
pub const DUP_WINDOW: usize = 32;
/// Sessions tracked by the duplicate-call metric before it starts over.
const DUP_SESSIONS: usize = 512;

fn is_false(b: &bool) -> bool {
    !*b
}

/// Estimated tokens for `bytes` of JSON (no tokenizer: bytes / 4).
pub fn est_tokens(bytes: u64) -> u64 {
    bytes / 4
}

/// In-memory hash of a call's identity (server, tool, arguments) for the
/// duplicate-call metric. Never written anywhere.
pub fn call_hash(server: &str, tool: &str, args: &serde_json::Value) -> u64 {
    use std::hash::Hasher;
    struct H(std::collections::hash_map::DefaultHasher);
    impl Write for H {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut h = H(std::collections::hash_map::DefaultHasher::new());
    h.0.write(server.as_bytes());
    h.0.write_u8(0);
    h.0.write(tool.as_bytes());
    h.0.write_u8(0);
    let _ = serde_json::to_writer(&mut h, args);
    h.0.finish()
}

/// Per-session ring of the last [`DUP_WINDOW`] call hashes; memory only.
#[derive(Debug, Default)]
pub struct DupTracker {
    rings: Mutex<HashMap<u64, std::collections::VecDeque<u64>>>,
}

impl DupTracker {
    /// Records `hash` for `session` and says whether it was already among
    /// the session's last [`DUP_WINDOW`] calls.
    pub fn check(&self, session: u64, hash: u64) -> bool {
        let mut rings = self.rings.lock().unwrap_or_else(|e| e.into_inner());
        if rings.len() >= DUP_SESSIONS && !rings.contains_key(&session) {
            rings.clear();
        }
        let ring = rings.entry(session).or_default();
        let dup = ring.contains(&hash);
        if ring.len() >= DUP_WINDOW {
            ring.pop_front();
        }
        ring.push_back(hash);
        dup
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 12 hex digits of SHA-256: a stable pseudonym for a directory, session
/// key or resource URI. Not reversible from the log alone, but a guessable
/// input can be confirmed by hashing it; treat it as pseudonymous.
pub fn short_hash(s: &str) -> String {
    let d = Sha256::digest(format!("mcpdoze-stats\0{s}").as_bytes());
    d.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// In-memory session key (never written).
pub fn session_key(id: &str) -> u64 {
    // FNV-1a; 0 is reserved for stateless calls.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h.max(1)
}

fn mb(bytes: u64) -> f64 {
    round1(bytes as f64 / MB)
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

// ------------------------------------------------------------------ events

/// What a forwarded call was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    /// `tools/call`.
    Tool,
    /// `prompts/get`.
    Prompt,
    /// `resources/read`.
    Resource,
}

impl CallKind {
    /// The kind of a forwarded method, if it is one that is logged.
    pub fn of_method(method: &str) -> Option<Self> {
        match method {
            "tools/call" => Some(CallKind::Tool),
            "prompts/get" => Some(CallKind::Prompt),
            "resources/read" => Some(CallKind::Resource),
            _ => None,
        }
    }
}

/// One forwarded agent request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallEvent {
    /// Unix time in ms (completion).
    pub ts: u64,
    /// Server name.
    pub server: String,
    /// Request kind.
    pub kind: CallKind,
    /// Tool or prompt name (without prefix); resource: [`short_hash`] of
    /// the URI.
    pub name: String,
    /// Broker-side duration.
    pub duration_ms: u64,
    /// No JSON-RPC error and no `isError` tool result.
    pub ok: bool,
    /// JSON-RPC error code, when the response is an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<i64>,
    /// Classified failure kind (a `diag` kind such as `stalled`), when the
    /// call failed in a way the broker can classify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail: Option<String>,
    /// The backend was not running when the call arrived.
    pub cold_start: bool,
    /// Time spent waiting for the backend to start (0 when warm).
    pub spawn_wait_ms: u64,
    /// Size of the request params as JSON.
    pub bytes_in: u64,
    /// Size of the response as JSON.
    pub bytes_out: u64,
    /// `legacy` or `modern` (2026-07-28) agent protocol.
    pub era: String,
    /// `shared`, `per-cwd` or `per-session`.
    pub scope: String,
    /// [`short_hash`] of the instance key (scoped instances only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Debug level: per-process request number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req_id: Option<u64>,
    /// Debug level: time to obtain a running backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquire_ms: Option<u64>,
    /// Debug level: time from sending to the backend to its response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_ms: Option<u64>,
    /// Size of the response as sent to the agent, when it differs from
    /// `bytes_out` (an optimisation changed it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_sent: Option<u64>,
    /// Tags of the optimisations applied to the response (empty: none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub opt: Vec<String>,
    /// Served from the result cache.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cached: bool,
    /// The session already made an identical call (same tool and
    /// arguments) among its last [`DUP_WINDOW`] calls. Only this flag is
    /// logged, never the arguments or their hash.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dup: bool,
    /// In memory only: the calling session (concurrency counting).
    #[serde(skip)]
    pub session: Option<u64>,
}

/// A backend process came up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnEvent {
    /// Unix time in ms.
    pub ts: u64,
    /// Server name.
    pub server: String,
    /// [`short_hash`] of the instance key (scoped instances only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// From the start decision to running.
    pub startup_ms: u64,
    /// Lifecycle generation.
    pub generation: u64,
}

/// A backend process went away.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnloadEvent {
    /// Unix time in ms.
    pub ts: u64,
    /// Server name.
    pub server: String,
    /// [`short_hash`] of the instance key (scoped instances only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// `idle`, `crash`, `stop` or `reload`.
    pub reason: String,
    /// How long the process ran.
    pub uptime_s: u64,
    /// Working set at the last sample, when it was sampled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_mb: Option<f64>,
    /// Exit code of the backend's root process, when it exited normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Terminating signal (Unix), when the root process was killed by one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Lifecycle generation.
    pub generation: u64,
}

/// An agent session opened or closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Unix time in ms.
    pub ts: u64,
    /// `open` or `close`.
    pub action: String,
    /// Stored sessions after the change.
    pub count: usize,
}

/// One server in a [`SampleEvent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerSample {
    /// Server name.
    pub server: String,
    /// Running instances (processes trees).
    pub instances: usize,
    /// Live processes over all instances.
    pub pids: usize,
    /// Private memory over all instances.
    pub private_mb: f64,
    /// Working set over all instances.
    pub working_mb: f64,
    /// Sessions that called the server during the window.
    pub sessions: usize,
    /// Last measured private MB of one instance times `sessions`.
    pub est_direct_mb: f64,
}

/// Periodic measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleEvent {
    /// Unix time in ms.
    pub ts: u64,
    /// Seconds covered by this sample (since the previous one).
    pub interval_s: u64,
    /// Broker process working set.
    pub broker_rss_mb: f64,
    /// Broker process private memory.
    pub broker_private_mb: f64,
    /// Stored agent sessions.
    pub sessions: usize,
    /// Running backend instances.
    pub running_instances: usize,
    /// Per server (running or called during the window).
    pub servers: Vec<ServerSample>,
    /// Estimated memory if every calling session ran its own servers.
    pub est_direct_mb: f64,
    /// Broker private memory plus every running backend's private memory.
    pub actual_mb: f64,
    /// `est_direct_mb - actual_mb` (negative when the broker costs more).
    pub saved_mb: f64,
    /// Events dropped so far (queue full).
    pub dropped: u64,
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    /// See [`CallEvent`].
    Call(CallEvent),
    /// See [`SpawnEvent`].
    Spawn(SpawnEvent),
    /// See [`UnloadEvent`].
    Unload(UnloadEvent),
    /// See [`SessionEvent`].
    Session(SessionEvent),
    /// See [`SampleEvent`].
    Sample(SampleEvent),
}

impl Event {
    /// Unix time in ms.
    pub fn ts(&self) -> u64 {
        match self {
            Event::Call(e) => e.ts,
            Event::Spawn(e) => e.ts,
            Event::Unload(e) => e.ts,
            Event::Session(e) => e.ts,
            Event::Sample(e) => e.ts,
        }
    }
}

/// Memory of one running instance, measured by the sampler.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceMeasure {
    /// Server name.
    pub server: String,
    /// [`short_hash`] of the instance key (scoped instances only).
    pub instance: Option<String>,
    /// Measured memory.
    pub memory: TreeMemory,
}

/// What the sampler measured; turned into a [`SampleEvent`] by the writer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SampleInput {
    /// The broker process.
    pub broker: TreeMemory,
    /// Stored agent sessions.
    pub sessions: usize,
    /// Every running (stdio) instance.
    pub instances: Vec<InstanceMeasure>,
}

// -------------------------------------------------------------- aggregator

/// Stateful part of the writer: the sample window and last measurements.
#[derive(Debug, Default)]
pub struct Aggregator {
    /// Server -> sessions that called it since the last sample.
    window: HashMap<String, HashSet<u64>>,
    /// Server -> last measured private MB of one instance.
    per_instance_mb: HashMap<String, f64>,
    /// (server, instance) -> working set MB at the last sample.
    last_rss: HashMap<(String, Option<String>), f64>,
    last_sample_ms: Option<u64>,
}

impl Aggregator {
    /// Updates state from an event and completes it (unload `rss_mb`).
    pub fn on_event(&mut self, ev: &mut Event) {
        match ev {
            Event::Call(c) => {
                if let Some(s) = c.session {
                    self.window.entry(c.server.clone()).or_default().insert(s);
                }
            }
            Event::Unload(u) if u.rss_mb.is_none() => {
                u.rss_mb = self
                    .last_rss
                    .remove(&(u.server.clone(), u.instance.clone()));
            }
            _ => {}
        }
    }

    /// Builds the sample at `ts` and starts a new window.
    /// `default_interval` covers the first sample.
    pub fn sample(
        &mut self,
        input: SampleInput,
        ts: u64,
        default_interval: Duration,
        dropped: u64,
    ) -> SampleEvent {
        let interval_s = match self.last_sample_ms {
            Some(prev) => ts.saturating_sub(prev) / 1000,
            None => default_interval.as_secs(),
        };
        self.last_sample_ms = Some(ts);
        #[derive(Default)]
        struct Acc {
            instances: usize,
            pids: usize,
            private: u64,
            working: u64,
        }
        let mut by_server: BTreeMap<String, Acc> = BTreeMap::new();
        self.last_rss.clear();
        for m in &input.instances {
            let a = by_server.entry(m.server.clone()).or_default();
            a.instances += 1;
            a.pids += m.memory.process_count;
            a.private += m.memory.private_bytes;
            a.working += m.memory.rss_bytes;
            self.last_rss.insert(
                (m.server.clone(), m.instance.clone()),
                mb(m.memory.rss_bytes),
            );
        }
        for (name, a) in &by_server {
            if a.instances > 0 && a.private > 0 {
                let per = a.private as f64 / MB / a.instances as f64;
                self.per_instance_mb.insert(name.clone(), per);
            }
        }
        let window = std::mem::take(&mut self.window);
        let mut names: Vec<String> = by_server.keys().cloned().collect();
        for k in window.keys() {
            if !by_server.contains_key(k) {
                names.push(k.clone());
            }
        }
        names.sort();
        let mut servers = Vec::new();
        let mut est_total = 0.0;
        let mut backends = 0.0;
        let mut running = 0;
        for name in names {
            let a = by_server.remove(&name).unwrap_or_default();
            let sessions = window.get(&name).map_or(0, HashSet::len);
            let per = self.per_instance_mb.get(&name).copied().unwrap_or(0.0);
            let est = per * sessions as f64;
            est_total += est;
            let private = a.private as f64 / MB;
            backends += private;
            running += a.instances;
            servers.push(ServerSample {
                server: name,
                instances: a.instances,
                pids: a.pids,
                private_mb: round1(private),
                working_mb: mb(a.working),
                sessions,
                est_direct_mb: round1(est),
            });
        }
        let actual = input.broker.private_bytes as f64 / MB + backends;
        SampleEvent {
            ts,
            interval_s,
            broker_rss_mb: mb(input.broker.rss_bytes),
            broker_private_mb: mb(input.broker.private_bytes),
            sessions: input.sessions,
            running_instances: running,
            servers,
            est_direct_mb: round1(est_total),
            actual_mb: round1(actual),
            saved_mb: round1(est_total - actual),
            dropped,
        }
    }
}

// ------------------------------------------------------------------- dates

/// (year, month, day) of a day count since 1970-01-01 (proleptic
/// Gregorian; H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days since 1970-01-01 of a civil date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM` of a Unix time in ms (UTC).
pub fn month_of(ts_ms: u64) -> String {
    let (y, m, _) = civil_from_days((ts_ms / MS_PER_DAY) as i64);
    format!("{y:04}-{m:02}")
}

/// File name of the month of `ts_ms`.
pub fn file_name_for(ts_ms: u64) -> String {
    format!("stats-{}.jsonl", month_of(ts_ms))
}

/// Unix ms of the first instant after the month named by a stats file
/// name, if it is one.
fn month_end_ms(file_name: &str) -> Option<u64> {
    let ym = file_name.strip_prefix("stats-")?.strip_suffix(".jsonl")?;
    let (y, m) = ym.split_once('-')?;
    if y.len() != 4 || m.len() != 2 {
        return None;
    }
    let y: i64 = y.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if !(1..=12).contains(&m) {
        return None;
    }
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let days = days_from_civil(ny, nm, 1);
    u64::try_from(days).ok().map(|d| d * MS_PER_DAY)
}

/// Unix ms of the start of the month named by a stats file name.
fn month_start_ms(file_name: &str) -> Option<u64> {
    let ym = file_name.strip_prefix("stats-")?.strip_suffix(".jsonl")?;
    let (y, m) = ym.split_once('-')?;
    let days = days_from_civil(y.parse().ok()?, m.parse().ok()?, 1);
    u64::try_from(days).ok().map(|d| d * MS_PER_DAY)
}

/// Deletes stats files whose whole month ended more than `retention_days`
/// before `now_ms`. Returns the names removed.
pub fn prune(dir: &Path, retention_days: u32, now_ms: u64) -> io::Result<Vec<String>> {
    let cutoff = now_ms.saturating_sub(u64::from(retention_days) * MS_PER_DAY);
    let mut removed = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(end) = month_end_ms(&name) else {
            continue;
        };
        if end <= cutoff && entry.file_type().is_ok_and(|t| t.is_file()) {
            fs::remove_file(entry.path())?;
            removed.push(name);
        }
    }
    removed.sort();
    Ok(removed)
}

// ------------------------------------------------------------------ writer

enum Msg {
    Event(Box<Event>),
    Sample(SampleInput),
    Flush(SyncSender<()>),
}

struct Writer {
    dir: PathBuf,
    retention_days: u32,
    interval: Duration,
    dropped: Arc<AtomicU64>,
    agg: Aggregator,
    file: Option<(String, BufWriter<File>)>,
    dirty: bool,
    last_flush: Instant,
    last_prune: Option<Instant>,
    warned: bool,
}

impl Writer {
    fn warn(&mut self, what: &str, e: &io::Error) {
        if !self.warned {
            self.warned = true;
            tracing::warn!(error = %e, "statistics log: {what}; further errors are not reported");
        }
    }

    fn write(&mut self, ev: &Event) {
        let name = file_name_for(ev.ts());
        if self.file.as_ref().map(|(n, _)| n) != Some(&name) {
            self.flush();
            self.file = None;
            let opened = fs::create_dir_all(&self.dir).and_then(|()| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.dir.join(&name))
            });
            match opened {
                Ok(f) => self.file = Some((name, BufWriter::with_capacity(64 * 1024, f))),
                Err(e) => {
                    self.warn("cannot open the log file", &e);
                    return;
                }
            }
        }
        let Some((_, w)) = self.file.as_mut() else {
            return;
        };
        let res = serde_json::to_writer(&mut *w, ev)
            .map_err(io::Error::other)
            .and_then(|()| w.write_all(b"\n"));
        match res {
            Ok(()) => self.dirty = true,
            Err(e) => self.warn("write failed", &e),
        }
    }

    fn flush(&mut self) {
        self.last_flush = Instant::now();
        if !self.dirty {
            return;
        }
        self.dirty = false;
        if let Some((_, w)) = self.file.as_mut() {
            if let Err(e) = w.flush() {
                self.warn("flush failed", &e);
            }
        }
    }

    fn maybe_prune(&mut self) {
        if self.last_prune.is_some_and(|t| t.elapsed() < PRUNE_EVERY) {
            return;
        }
        self.last_prune = Some(Instant::now());
        match prune(&self.dir, self.retention_days, now_ms()) {
            Ok(r) if !r.is_empty() => tracing::info!(files = ?r, "statistics: old months removed"),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => self.warn("retention check failed", &e),
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        self.maybe_prune();
        loop {
            match rx.recv_timeout(FLUSH_EVERY) {
                Ok(Msg::Event(mut ev)) => {
                    self.agg.on_event(&mut ev);
                    self.write(&ev);
                }
                Ok(Msg::Sample(input)) => {
                    let dropped = self.dropped.load(Ordering::Relaxed);
                    let ev = self.agg.sample(input, now_ms(), self.interval, dropped);
                    self.write(&Event::Sample(ev));
                }
                Ok(Msg::Flush(ack)) => {
                    self.flush();
                    let _ = ack.try_send(());
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.flush();
                    self.maybe_prune();
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.flush();
                    return;
                }
            }
            if self.last_flush.elapsed() >= FLUSH_EVERY {
                self.flush();
            }
        }
    }
}

/// Handle to the statistics log. Dropping it flushes and stops the writer.
pub struct Stats {
    tx: Option<SyncSender<Msg>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    dropped: Arc<AtomicU64>,
    dir: PathBuf,
    debug: bool,
    interval: Duration,
    dups: DupTracker,
}

impl std::fmt::Debug for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stats")
            .field("dir", &self.dir)
            .field("debug", &self.debug)
            .finish_non_exhaustive()
    }
}

impl Stats {
    /// Starts the writer thread for `dir` with `cfg`'s retention, interval
    /// and debug level (`cfg.enabled` and `cfg.dir` are the caller's).
    pub fn start(cfg: &StatsConfig, dir: PathBuf) -> io::Result<Arc<Stats>> {
        Self::with_queue(cfg, dir, QUEUE)
    }

    /// [`start`](Stats::start) with an explicit queue size (tests).
    pub fn with_queue(cfg: &StatsConfig, dir: PathBuf, queue: usize) -> io::Result<Arc<Stats>> {
        fs::create_dir_all(&dir)?;
        let (tx, rx) = mpsc::sync_channel(queue.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let interval = Duration::from_secs(cfg.sample_interval_secs.max(1));
        let writer = Writer {
            dir: dir.clone(),
            retention_days: cfg.retention_days.max(1),
            interval,
            dropped: dropped.clone(),
            agg: Aggregator::default(),
            file: None,
            dirty: false,
            last_flush: Instant::now(),
            last_prune: None,
            warned: false,
        };
        let thread = std::thread::Builder::new()
            .name("mcpdoze-stats".into())
            .spawn(move || writer.run(rx))?;
        Ok(Arc::new(Stats {
            tx: Some(tx),
            thread: Mutex::new(Some(thread)),
            dropped,
            dir,
            debug: cfg.debug,
            interval,
            dups: DupTracker::default(),
        }))
    }

    /// Records a call of `session` and says whether it repeats a recent
    /// identical call (see [`DupTracker`]).
    pub fn note_call(&self, session: u64, hash: u64) -> bool {
        self.dups.check(session, hash)
    }

    /// The log directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Debug level (request ids and timings).
    pub fn debug(&self) -> bool {
        self.debug
    }

    /// Time between samples.
    pub fn sample_interval(&self) -> Duration {
        self.interval
    }

    /// Events dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn send(&self, msg: Msg) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Queues an event; never blocks (dropped and counted when full).
    pub fn record(&self, ev: Event) {
        self.send(Msg::Event(Box::new(ev)));
    }

    /// Queues a measurement; the writer turns it into a `sample` event.
    pub fn sample(&self, input: SampleInput) {
        self.send(Msg::Sample(input));
    }

    /// Asks the writer to flush and waits up to `timeout` (blocking; call
    /// from a blocking context). True when the flush was confirmed.
    pub fn flush(&self, timeout: Duration) -> bool {
        let Some(tx) = &self.tx else { return false };
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        let deadline = Instant::now() + timeout;
        let mut msg = Msg::Flush(ack_tx);
        loop {
            match tx.try_send(msg) {
                Ok(()) => break,
                Err(TrySendError::Full(m)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    msg = m;
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        ack_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_ok()
    }
}

impl Drop for Stats {
    fn drop(&mut self) {
        drop(self.tx.take());
        let thread = self
            .thread
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(t) = thread {
            let _ = t.join();
        }
    }
}

// ----------------------------------------------------------------- summary

/// Per-server part of a [`Summary`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerSummary {
    /// Server name.
    pub server: String,
    /// Logged calls.
    pub calls: u64,
    /// Calls that were not `ok`.
    pub errors: u64,
    /// `errors / calls`.
    pub error_rate: f64,
    /// Median call duration.
    pub p50_ms: u64,
    /// 95th percentile call duration.
    pub p95_ms: u64,
    /// Calls that found the backend not running.
    pub cold_starts: u64,
    /// `cold_starts / calls`.
    pub cold_start_rate: f64,
    /// Processes started.
    pub spawns: u64,
    /// Unloads by reason.
    pub unloads: BTreeMap<String, u64>,
    /// Classified call failures by kind.
    #[serde(default)]
    pub failures: BTreeMap<String, u64>,
}

/// Size of one server's tool list as served to agents (live catalog).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListSize {
    /// Server name.
    pub server: String,
    /// Bytes of the prefixed tool list as JSON.
    pub list_bytes: u64,
    /// `list_bytes / 4`; an estimate, not a tokenizer count.
    pub est_list_tokens: u64,
}

/// Per-tool part of a [`Summary`] (tool calls only).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolSummary {
    /// Server name.
    pub server: String,
    /// Tool name (without prefix).
    pub tool: String,
    /// Logged calls.
    pub calls: u64,
    /// Average response size in bytes.
    pub avg_bytes_out: u64,
    /// 95th percentile response size in bytes.
    pub p95_bytes_out: u64,
    /// Sum of response sizes in bytes.
    pub total_bytes: u64,
    /// `total_bytes / 4`; an estimate, not a tokenizer count.
    pub est_tokens: u64,
    /// `total_bytes` over the server's total tool response bytes.
    pub share: f64,
    /// Calls flagged as repeats of a recent identical call.
    pub dup_calls: u64,
    /// Bytes not sent to the agent thanks to optimisations.
    pub bytes_saved: u64,
}

/// Memory part of a [`Summary`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemorySummary {
    /// Samples in the period.
    pub samples: u64,
    /// Average broker-side memory (broker plus backends).
    pub avg_actual_mb: f64,
    /// Peak broker-side memory.
    pub peak_actual_mb: f64,
    /// Average estimated direct-in-agent memory.
    pub avg_est_direct_mb: f64,
    /// Peak estimated direct-in-agent memory.
    pub peak_est_direct_mb: f64,
    /// Average of `saved_mb`.
    pub avg_saved_mb: f64,
    /// Sum of `saved_mb * interval` in MB-hours.
    pub mb_hours_saved: f64,
}

/// Summary of the log over a period (`mcpdoze stats`, `GET /admin/v1/stats`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Start of the period (Unix ms).
    pub since_ms: u64,
    /// End of the period (Unix ms).
    pub until_ms: u64,
    /// Events read.
    pub events: u64,
    /// Sessions opened.
    pub sessions_opened: u64,
    /// Most stored sessions in one sample.
    pub peak_sessions: usize,
    /// Highest `dropped` counter seen (per broker run).
    pub dropped: u64,
    /// By server name.
    pub servers: Vec<ServerSummary>,
    /// Per tool, largest `total_bytes` first.
    #[serde(default)]
    pub tools: Vec<ToolSummary>,
    /// Tool-list size per server (filled by the admin API from the live
    /// catalog, not read from the log).
    #[serde(default)]
    pub lists: Vec<ListSize>,
    /// Broker vs estimated direct memory.
    pub memory: MemorySummary,
}

/// Nearest-rank percentile of a sorted slice.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        (a as f64 / b as f64 * 1000.0).round() / 1000.0
    }
}

/// Builds a [`Summary`] from events in any order.
#[derive(Debug, Default)]
pub struct SummaryBuilder {
    since_ms: u64,
    until_ms: u64,
    events: u64,
    sessions_opened: u64,
    peak_sessions: usize,
    dropped: u64,
    servers: BTreeMap<String, (ServerSummary, Vec<u64>)>,
    tools: BTreeMap<(String, String), (ToolSummary, Vec<u64>)>,
    samples: u64,
    sum_actual: f64,
    sum_est: f64,
    sum_saved: f64,
    peak_actual: f64,
    peak_est: f64,
    mb_hours: f64,
}

impl SummaryBuilder {
    /// Events with `since_ms <= ts <= until_ms` are counted.
    pub fn new(since_ms: u64, until_ms: u64) -> Self {
        Self {
            since_ms,
            until_ms,
            ..Self::default()
        }
    }

    fn server(&mut self, name: &str) -> &mut (ServerSummary, Vec<u64>) {
        self.servers.entry(name.to_owned()).or_insert_with(|| {
            (
                ServerSummary {
                    server: name.to_owned(),
                    ..ServerSummary::default()
                },
                Vec::new(),
            )
        })
    }

    /// Adds one event (ignored outside the period).
    pub fn add(&mut self, ev: &Event) {
        let ts = ev.ts();
        if ts < self.since_ms || ts > self.until_ms {
            return;
        }
        self.events += 1;
        match ev {
            Event::Call(c) => {
                let (s, d) = self.server(&c.server);
                s.calls += 1;
                if !c.ok {
                    s.errors += 1;
                }
                if let Some(k) = &c.fail {
                    *s.failures.entry(k.clone()).or_default() += 1;
                }
                if c.cold_start {
                    s.cold_starts += 1;
                }
                d.push(c.duration_ms);
                if c.kind == CallKind::Tool {
                    let (t, sizes) = self
                        .tools
                        .entry((c.server.clone(), c.name.clone()))
                        .or_insert_with(|| {
                            (
                                ToolSummary {
                                    server: c.server.clone(),
                                    tool: c.name.clone(),
                                    ..ToolSummary::default()
                                },
                                Vec::new(),
                            )
                        });
                    t.calls += 1;
                    t.total_bytes += c.bytes_out;
                    t.dup_calls += u64::from(c.dup);
                    t.bytes_saved += c
                        .bytes_sent
                        .map_or(0, |sent| c.bytes_out.saturating_sub(sent));
                    sizes.push(c.bytes_out);
                }
            }
            Event::Spawn(e) => self.server(&e.server).0.spawns += 1,
            Event::Unload(e) => {
                *self
                    .server(&e.server)
                    .0
                    .unloads
                    .entry(e.reason.clone())
                    .or_default() += 1;
            }
            Event::Session(e) => {
                if e.action == "open" {
                    self.sessions_opened += 1;
                }
            }
            Event::Sample(e) => {
                self.samples += 1;
                self.sum_actual += e.actual_mb;
                self.sum_est += e.est_direct_mb;
                self.sum_saved += e.saved_mb;
                self.peak_actual = self.peak_actual.max(e.actual_mb);
                self.peak_est = self.peak_est.max(e.est_direct_mb);
                self.mb_hours += e.saved_mb * e.interval_s as f64 / 3600.0;
                self.peak_sessions = self.peak_sessions.max(e.sessions);
                self.dropped = self.dropped.max(e.dropped);
            }
        }
    }

    /// The summary.
    pub fn finish(self) -> Summary {
        let n = self.samples.max(1) as f64;
        let servers = self
            .servers
            .into_values()
            .map(|(mut s, mut d)| {
                d.sort_unstable();
                s.p50_ms = percentile(&d, 50.0);
                s.p95_ms = percentile(&d, 95.0);
                s.error_rate = ratio(s.errors, s.calls);
                s.cold_start_rate = ratio(s.cold_starts, s.calls);
                s
            })
            .collect();
        let mut server_bytes: HashMap<String, u64> = HashMap::new();
        for (t, _) in self.tools.values() {
            *server_bytes.entry(t.server.clone()).or_default() += t.total_bytes;
        }
        let mut tools: Vec<ToolSummary> = self
            .tools
            .into_values()
            .map(|(mut t, mut sizes)| {
                sizes.sort_unstable();
                t.avg_bytes_out = t.total_bytes / t.calls.max(1);
                t.p95_bytes_out = percentile(&sizes, 95.0);
                t.est_tokens = est_tokens(t.total_bytes);
                t.share = ratio(t.total_bytes, server_bytes[&t.server]);
                t
            })
            .collect();
        tools.sort_by(|a, b| {
            b.total_bytes
                .cmp(&a.total_bytes)
                .then_with(|| (&a.server, &a.tool).cmp(&(&b.server, &b.tool)))
        });
        let avg = |v: f64| {
            if self.samples == 0 {
                0.0
            } else {
                round1(v / n)
            }
        };
        Summary {
            since_ms: self.since_ms,
            until_ms: self.until_ms,
            events: self.events,
            sessions_opened: self.sessions_opened,
            peak_sessions: self.peak_sessions,
            dropped: self.dropped,
            servers,
            tools,
            lists: Vec::new(),
            memory: MemorySummary {
                samples: self.samples,
                avg_actual_mb: avg(self.sum_actual),
                peak_actual_mb: round1(self.peak_actual),
                avg_est_direct_mb: avg(self.sum_est),
                peak_est_direct_mb: round1(self.peak_est),
                avg_saved_mb: avg(self.sum_saved),
                mb_hours_saved: round1(self.mb_hours),
            },
        }
    }
}

/// Summarises the files of `dir` between `since_ms` and `until_ms`.
/// Unparseable lines are skipped; a missing directory is an empty summary.
pub fn summarize_dir(dir: &Path, since_ms: u64, until_ms: u64) -> io::Result<Summary> {
    let mut b = SummaryBuilder::new(since_ms, until_ms);
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(b.finish()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let (Some(start), Some(end)) = (month_start_ms(&name), month_end_ms(&name)) else {
            continue;
        };
        if end > since_ms && start <= until_ms {
            files.push(entry.path());
        }
    }
    files.sort();
    for path in files {
        let f = File::open(&path)?;
        for line in BufReader::new(f).lines() {
            let Ok(line) = line else { break };
            if let Ok(ev) = serde_json::from_str::<Event>(&line) {
                b.add(&ev);
            }
        }
    }
    Ok(b.finish())
}

/// Number of bytes `v` takes as compact JSON (no allocation).
pub fn json_len(v: &serde_json::Value) -> u64 {
    struct Count(u64);
    impl Write for Count {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut c = Count(0);
    let _ = serde_json::to_writer(&mut c, v);
    c.0
}

#[cfg(test)]
mod tests;
