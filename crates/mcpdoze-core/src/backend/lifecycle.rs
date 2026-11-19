// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-instance lifecycle:
//!
//! ```text
//! Stopped -> Starting -> Running -> Draining -> Stopping -> Stopped
//!               |           |
//!               +-----------+--> Failed(backoff) -> (next call) Starting
//! ```
//!
//! * Calls hold a [`CallGuard`] (RAII in-flight count). When the count drops
//!   to zero an idle deadline is armed; any new call clears it.
//! * The idle check and the move `Running -> Draining` happen in one critical
//!   section under the instance lock, the same lock [`Instance::acquire`]
//!   takes. A racing call therefore either finds `Running` (and clears the
//!   deadline, so the unload is abandoned) or finds `Draining`/`Stopping`, waits
//!   for the stop to finish and starts a fresh process.
//! * Concurrent callers during `Starting` share one start (a watch channel).
//! * A crash (stdout closes while `Running`) fails in-flight calls, moves to
//!   `Failed` with exponential backoff, and the next call after the backoff
//!   respawns. Calls during the backoff fail fast.
//!
//! All timing uses `tokio::time`, so pure state-machine tests can run under
//! `tokio::time::pause`. Tests with real processes should use short real
//! durations instead (auto-advance would fire timeouts while waiting on
//! process I/O).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use super::http::HttpConn;
use super::stdio::{lock, InstCounters, StderrRing, StdioConn, EXIT_DRAIN, STATE_NAMES};
use super::{BackendEvents, BackendSpec, CallError, Conn, SubscriptionPolicy};
use crate::diag::{BackendView, DiagTap, ExitView, StderrView};
use crate::proctree::ExitInfo;

/// Timing knobs of the lifecycle; defaults follow the development plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// Grace between closing stdin and killing the tree on unload.
    pub stop_grace: Duration,
    /// First crash backoff.
    pub backoff_base: Duration,
    /// Backoff ceiling.
    pub backoff_max: Duration,
    /// Uptime after which the failure count resets.
    pub healthy_reset: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            stop_grace: Duration::from_secs(3),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            healthy_reset: Duration::from_secs(30),
        }
    }
}

impl Tuning {
    /// Backoff after `failures` consecutive failures (`failures >= 1`):
    /// `base * 2^(failures-1)`, capped at `backoff_max`.
    pub fn backoff(&self, failures: u32) -> Duration {
        let exp = failures.saturating_sub(1).min(20);
        self.backoff_base
            .saturating_mul(1u32 << exp)
            .min(self.backoff_max)
    }
}

/// Why a backend process went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnloadReason {
    /// Idle TTL expired.
    Idle,
    /// The process exited on its own while running.
    Crash,
    /// Stopped on request (admin `stop`, server removed, broker shutdown).
    Stop,
    /// Stopped because a config reload changed or removed the server.
    Reload,
}

impl UnloadReason {
    /// Lower-case name (`idle`, `crash`, `stop`, `reload`).
    pub fn as_str(self) -> &'static str {
        match self {
            UnloadReason::Idle => "idle",
            UnloadReason::Crash => "crash",
            UnloadReason::Stop => "stop",
            UnloadReason::Reload => "reload",
        }
    }
}

/// A process-level lifecycle event of one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// A new process is up and initialised.
    Spawned {
        /// Lifecycle generation of the process.
        generation: u64,
        /// From the start decision to `Running`.
        startup: Duration,
    },
    /// A process went away.
    Unloaded {
        /// Lifecycle generation of the process.
        generation: u64,
        /// Why.
        reason: UnloadReason,
        /// How long the process ran.
        uptime: Duration,
        /// How the root process ended, when known (stdio backends only).
        exit: Option<ExitInfo>,
    },
}

/// Callback run (outside the instance lock) on every [`LifecycleEvent`].
pub type LifecycleHook = Arc<dyn Fn(&LifecycleEvent) + Send + Sync>;

/// Externally visible lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateKind {
    /// No process.
    Stopped,
    /// Spawning and handshaking.
    Starting,
    /// Serving calls.
    Running,
    /// Idle TTL expired; no new calls accepted on this process.
    Draining,
    /// Stdin closed, waiting for exit / killing the tree.
    Stopping,
    /// Crashed or failed to start; backing off.
    Failed,
}

enum State {
    Stopped,
    Starting { done: watch::Receiver<bool> },
    Running { conn: Arc<Conn>, idle: Arc<Notify> },
    Draining { done: watch::Receiver<bool> },
    Stopping { done: watch::Receiver<bool> },
    Failed { until: Instant, error: String },
}

impl StateKind {
    /// Lower-case state name (as written in diagnostics records).
    pub fn name(self) -> &'static str {
        STATE_NAMES[self.idx()]
    }

    fn idx(self) -> usize {
        match self {
            StateKind::Stopped => 0,
            StateKind::Starting => 1,
            StateKind::Running => 2,
            StateKind::Draining => 3,
            StateKind::Stopping => 4,
            StateKind::Failed => 5,
        }
    }
}

impl State {
    fn kind(&self) -> StateKind {
        match self {
            State::Stopped => StateKind::Stopped,
            State::Starting { .. } => StateKind::Starting,
            State::Running { .. } => StateKind::Running,
            State::Draining { .. } => StateKind::Draining,
            State::Stopping { .. } => StateKind::Stopping,
            State::Failed { .. } => StateKind::Failed,
        }
    }

    fn done(&self) -> Option<&watch::Receiver<bool>> {
        match self {
            State::Starting { done } | State::Draining { done } | State::Stopping { done } => {
                Some(done)
            }
            _ => None,
        }
    }
}

struct Inner {
    state: State,
    in_flight: usize,
    idle_deadline: Option<Instant>,
    failures: u32,
    generation: u64,
    /// Stop as soon as the in-flight count reaches zero.
    drain: bool,
    last_error: Option<String>,
    /// Live resource subscriptions held on this instance.
    pins: usize,
    /// Reported reason of the stop `drain` arms.
    drain_reason: UnloadReason,
}

/// One backend instance: at most one process at a time, started lazily and
/// unloaded when idle.
pub struct Instance {
    name: String,
    spec: BackendSpec,
    tuning: Tuning,
    events: Arc<dyn BackendEvents>,
    stderr: Arc<StderrRing>,
    inner: Mutex<Inner>,
    /// Spawn and call counts, state and start phase, shared with the
    /// connections for diagnostics views.
    counters: Arc<InstCounters>,
    tap: RwLock<Arc<DiagTap>>,
    spawn_hooks: Mutex<Vec<SpawnHook>>,
    lifecycle_hooks: Mutex<Vec<LifecycleHook>>,
}

/// Callback run each time an instance reaches `Running` on a new process,
/// with that process's lifecycle generation.
pub type SpawnHook = Arc<dyn Fn(u64) + Send + Sync>;

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance")
            .field("name", &self.name)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

enum Next {
    Wait(watch::Receiver<bool>),
    Start,
}

impl Instance {
    /// A stopped instance with default [`Tuning`].
    pub fn new(name: &str, spec: BackendSpec, events: Arc<dyn BackendEvents>) -> Arc<Self> {
        Self::with_tuning(name, spec, events, Tuning::default())
    }

    /// A stopped instance with explicit timing.
    pub fn with_tuning(
        name: &str,
        spec: BackendSpec,
        events: Arc<dyn BackendEvents>,
        tuning: Tuning,
    ) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            spec,
            tuning,
            events,
            stderr: Arc::new(StderrRing::default()),
            inner: Mutex::new(Inner {
                state: State::Stopped,
                in_flight: 0,
                idle_deadline: None,
                failures: 0,
                generation: 0,
                drain: false,
                last_error: None,
                pins: 0,
                drain_reason: UnloadReason::Stop,
            }),
            counters: Arc::new(InstCounters::default()),
            tap: RwLock::new(DiagTap::disabled()),
            spawn_hooks: Mutex::new(Vec::new()),
            lifecycle_hooks: Mutex::new(Vec::new()),
        })
    }

    /// Sets the diagnostics tap of this instance (the router makes one per
    /// instance; the default is a tap that is always off). Connections
    /// started afterwards use it.
    pub fn set_tap(&self, tap: Arc<DiagTap>) {
        *self.tap.write().unwrap_or_else(|e| e.into_inner()) = tap;
    }

    /// The diagnostics tap.
    pub fn tap(&self) -> Arc<DiagTap> {
        self.tap.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn tap_ref(&self) -> std::sync::RwLockReadGuard<'_, Arc<DiagTap>> {
        self.tap.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a state change in the shared counters and the tap's
    /// transition ring. Call right after assigning `g.state`.
    fn trans(&self, g: &Inner, reason: &'static str) {
        let kind = g.state.kind();
        self.counters
            .state
            .store(u8::try_from(kind.idx()).unwrap_or(0), Ordering::Relaxed);
        self.counters
            .failures
            .store(u64::from(g.failures), Ordering::Relaxed);
        self.tap_ref()
            .on_transition(kind.name(), reason, g.in_flight);
    }

    fn view_locked(&self, g: &Inner) -> BackendView<'static> {
        let mut v = BackendView::new(g.state.kind().name(), g.generation);
        v.in_flight = g.in_flight as u64;
        v.calls_served = self.calls();
        v.restarts = self.spawn_count().saturating_sub(1);
        v.failures = u64::from(g.failures);
        v
    }

    /// Runs `f` with a view of this instance's backend state, handshake
    /// facts included when it is running. Allocates: call it only when the
    /// tap is enabled.
    pub fn with_view<R>(&self, f: impl FnOnce(&BackendView<'_>) -> R) -> R {
        let conn = self.running_conn();
        self.view_for(conn.as_deref(), None, f)
    }

    fn view_for<R>(
        &self,
        conn: Option<&Conn>,
        state: Option<&'static str>,
        f: impl FnOnce(&BackendView<'_>) -> R,
    ) -> R {
        let mut base = self.view_locked(&lock(&self.inner));
        if let Some(s) = state {
            base.state = s;
        }
        let Some(conn) = conn else { return f(&base) };
        let init = conn.init_result();
        let caps: Vec<&str> = init["capabilities"]
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let mut v = base;
        v.uptime_ms = u64::try_from(conn.started().elapsed().as_millis()).unwrap_or(u64::MAX);
        v.pending = conn.pending_len() as u64;
        v.protocol = init["protocolVersion"].as_str();
        v.server_info = init["serverInfo"]["name"]
            .as_str()
            .map(|n| (n, init["serverInfo"]["version"].as_str()));
        v.caps = &caps;
        f(&v)
    }

    /// Runs `f` with the stderr tail (call only when the tap is enabled).
    pub fn with_stderr<R>(&self, f: impl FnOnce(&StderrView<'_>) -> R) -> R {
        let (total, lines, ts) = self.stderr.snapshot();
        f(&StderrView {
            total,
            lines: &lines,
            ts: &ts,
        })
    }

    /// Registers `hook`, run (outside the instance lock) on every process
    /// spawn and unload of this instance. Hooks must not block.
    pub fn on_lifecycle(&self, hook: impl Fn(&LifecycleEvent) + Send + Sync + 'static) {
        lock(&self.lifecycle_hooks).push(Arc::new(hook));
    }

    fn emit(&self, ev: LifecycleEvent) {
        let hooks = lock(&self.lifecycle_hooks).clone();
        for h in hooks {
            h(&ev);
        }
    }

    /// Registers `hook`, run (outside the instance lock) every time a new
    /// process of this instance is up and initialised, with its lifecycle
    /// generation, before callers waiting on the start resume. Hooks are
    /// kept for the life of the instance and must not block.
    pub fn on_spawn(&self, hook: impl Fn(u64) + Send + Sync + 'static) {
        lock(&self.spawn_hooks).push(Arc::new(hook));
    }

    /// Adds one resource-subscription pin. With the `keep-alive` policy a
    /// pinned instance is not unloaded at its idle TTL and is respawned
    /// eagerly after a crash.
    pub fn pin(&self) {
        lock(&self.inner).pins += 1;
    }

    /// Removes one pin; the idle timer restarts when the last goes.
    pub fn unpin(&self) {
        let g = &mut *lock(&self.inner);
        g.pins = g.pins.saturating_sub(1);
        if g.pins == 0 && g.in_flight == 0 {
            if let State::Running { idle, .. } = &g.state {
                let idle = idle.clone();
                g.idle_deadline = Some(Instant::now() + self.spec.idle_ttl);
                idle.notify_one();
            }
        }
    }

    /// Resource-subscription pins held.
    pub fn pins(&self) -> usize {
        lock(&self.inner).pins
    }

    fn keeps_alive(&self, g: &Inner) -> bool {
        g.pins > 0 && self.spec.subscriptions == SubscriptionPolicy::KeepAlive
    }

    /// Server name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Launch spec.
    pub fn spec(&self) -> &BackendSpec {
        &self.spec
    }

    /// Current state.
    pub fn state(&self) -> StateKind {
        lock(&self.inner).state.kind()
    }

    /// Pid of the running process, if `Running`.
    pub fn pid(&self) -> Option<u32> {
        match &lock(&self.inner).state {
            State::Running { conn, .. } => conn.pid(),
            _ => None,
        }
    }

    /// Calls currently holding a [`CallGuard`].
    pub fn in_flight(&self) -> usize {
        lock(&self.inner).in_flight
    }

    /// Calls awaiting a backend response on the running process.
    pub fn pending_calls(&self) -> usize {
        match &lock(&self.inner).state {
            State::Running { conn, .. } => conn.pending_len(),
            _ => 0,
        }
    }

    /// Consecutive failures counted for backoff.
    pub fn failures(&self) -> u32 {
        lock(&self.inner).failures
    }

    /// Number of processes spawned so far.
    pub fn spawn_count(&self) -> u64 {
        self.counters.spawns.load(Ordering::Relaxed)
    }

    /// Last lines of backend stderr (across restarts).
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr.lines()
    }

    /// Calls handed a [`CallGuard`] so far.
    pub fn calls(&self) -> u64 {
        self.counters.calls.load(Ordering::Relaxed)
    }

    /// The most recent start failure or crash, if any.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.inner).last_error.clone()
    }

    /// Time left before the idle unload, when `Running` with nothing in
    /// flight.
    pub fn idle_remaining(&self) -> Option<Duration> {
        let g = lock(&self.inner);
        match (&g.state, g.idle_deadline) {
            (State::Running { .. }, Some(d)) if g.in_flight == 0 => {
                Some(d.saturating_duration_since(Instant::now()))
            }
            _ => None,
        }
    }

    /// The live connection, if `Running` (for status: pids, memory).
    pub fn running_conn(&self) -> Option<Arc<Conn>> {
        match &lock(&self.inner).state {
            State::Running { conn, .. } => Some(conn.clone()),
            _ => None,
        }
    }

    /// Unloads the backend once no call is in flight, waiting at most
    /// `timeout` before stopping it like [`shutdown`](Instance::shutdown)
    /// (which fails the calls still running). Calls arriving meanwhile are
    /// served; a later call starts a fresh process as usual.
    pub async fn drain(self: &Arc<Self>, timeout: Duration) {
        self.drain_for(timeout, UnloadReason::Stop).await;
    }

    /// [`drain`](Instance::drain), reporting `reason` for the unload.
    pub async fn drain_for(self: &Arc<Self>, timeout: Duration, reason: UnloadReason) {
        let graceful = tokio::time::timeout(timeout, async {
            loop {
                let done = {
                    let g = &mut *lock(&self.inner);
                    match &g.state {
                        State::Stopped | State::Failed { .. } => return,
                        State::Running { .. } if g.in_flight == 0 => {
                            Some(self.begin_stop(g, reason))
                        }
                        State::Running { .. } => {
                            // `release` begins the stop when the count hits zero.
                            g.drain = true;
                            g.drain_reason = reason;
                            None
                        }
                        s => s.done().cloned(),
                    }
                };
                match done {
                    Some(mut done) => {
                        if done.wait_for(|d| *d).await.is_err() {
                            self.abandon(&done);
                        }
                    }
                    None => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await;
        if graceful.is_err() {
            lock(&self.inner).drain = false;
            self.shutdown_for(reason).await;
        }
    }

    /// Gets a running connection for one call, starting the backend if needed.
    /// The returned guard counts as in flight until dropped.
    pub async fn acquire(self: &Arc<Self>) -> Result<CallGuard, CallError> {
        loop {
            let mut done = {
                let g = &mut *lock(&self.inner);
                let next = match &g.state {
                    State::Running { conn, .. } => {
                        let conn = conn.clone();
                        g.in_flight += 1;
                        g.idle_deadline = None;
                        self.counters.calls.fetch_add(1, Ordering::Relaxed);
                        return Ok(CallGuard {
                            inst: self.clone(),
                            conn,
                        });
                    }
                    State::Failed { until, error } => {
                        let now = Instant::now();
                        if now < *until {
                            let tap = self.tap_ref();
                            if tap.enabled() {
                                tap.on_backoff_rejected(&self.view_locked(g));
                            }
                            return Err(CallError::Unavailable(format!(
                                "{error}; retry in {} ms",
                                (*until - now).as_millis()
                            )));
                        }
                        Next::Start
                    }
                    State::Stopped => Next::Start,
                    s => Next::Wait(s.done().cloned().expect("transitional state")),
                };
                match next {
                    Next::Wait(rx) => {
                        if matches!(g.state, State::Draining { .. } | State::Stopping { .. }) {
                            let tap = self.tap_ref();
                            if tap.enabled() {
                                tap.on_race_unload("acquire", &self.view_locked(g));
                            }
                        }
                        rx
                    }
                    // Same critical section as the decision: one start only.
                    Next::Start => self.begin_start(g),
                }
            };
            if done.wait_for(|d| *d).await.is_err() {
                self.abandon(&done);
            }
        }
    }

    /// Stops the backend now (fails calls still in flight) and waits until
    /// it is `Stopped` or `Failed`. Used on broker shutdown.
    pub async fn shutdown(self: &Arc<Self>) {
        self.shutdown_for(UnloadReason::Stop).await;
    }

    /// [`shutdown`](Instance::shutdown), reporting `reason` for the unload.
    pub async fn shutdown_for(self: &Arc<Self>, reason: UnloadReason) {
        loop {
            let mut done = {
                let g = &mut *lock(&self.inner);
                match &g.state {
                    State::Stopped | State::Failed { .. } => return,
                    State::Running { .. } => self.begin_stop(g, reason),
                    s => s.done().cloned().expect("transitional state"),
                }
            };
            if done.wait_for(|d| *d).await.is_err() {
                self.abandon(&done);
            }
        }
    }

    /// A lifecycle task vanished without signalling (runtime shutdown or a
    /// panic): mark the instance failed so waiters do not spin.
    fn abandon(&self, done: &watch::Receiver<bool>) {
        let g = &mut *lock(&self.inner);
        if g.state.done().is_some_and(|d| d.same_channel(done)) {
            g.state = State::Failed {
                until: Instant::now(),
                error: "lifecycle task ended unexpectedly".into(),
            };
            self.trans(g, "abandoned");
        }
    }

    fn begin_start(self: &Arc<Self>, g: &mut Inner) -> watch::Receiver<bool> {
        g.generation += 1;
        let generation = g.generation;
        let (tx, rx) = watch::channel(false);
        g.state = State::Starting { done: rx.clone() };
        self.trans(g, "call");
        self.counters.spawns.fetch_add(1, Ordering::Relaxed);
        self.counters.reset_phase();
        tokio::spawn(self.clone().run_start(generation, tx));
        rx
    }

    async fn run_start(self: Arc<Self>, generation: u64, done: watch::Sender<bool>) {
        let t0 = Instant::now();
        let tap = self.tap();
        let start = async {
            let conn = if self.spec.http.is_some() {
                let c = HttpConn::new(&self.name, generation, &self.spec, self.events.clone())?;
                c.set_diag(tap.clone());
                Conn::Http(c)
            } else {
                match StdioConn::spawn(
                    &self.name,
                    generation,
                    &self.spec,
                    self.events.clone(),
                    self.stderr.clone(),
                    tap.clone(),
                    self.counters.clone(),
                ) {
                    Ok(c) => Conn::Stdio(Box::new(c)),
                    Err(e) => {
                        if tap.enabled() {
                            let cmd = self.spec.command.to_ascii_lowercase();
                            let shim = cmd.ends_with(".cmd") || cmd.ends_with(".bat");
                            self.view_for(None, Some("starting"), |v| {
                                tap.on_spawn_failed(e.kind(), shim, v);
                            });
                        }
                        return Err(format!("spawn failed: {e}"));
                    }
                }
            };
            let conn = Arc::new(conn);
            if let Err(e) = conn.initialize().await {
                // The process went away before answering: an exit, not a
                // timeout or a protocol error.
                if let Conn::Stdio(c) = &*conn {
                    if tap.enabled() && *c.closed().borrow() {
                        let exit = c.exit_info_wait(EXIT_DRAIN * 3).await;
                        let ev = ExitView {
                            code: exit.and_then(|x| x.code),
                            signal: exit.and_then(|x| x.signal),
                        };
                        self.view_for(Some(&conn), Some("starting"), |v| {
                            self.with_stderr(|s| tap.on_exit(ev, v, Some(s)));
                        });
                    }
                }
                return Err(e);
            }
            Ok::<_, String>(conn)
        };
        let res = match tokio::time::timeout(self.spec.start_timeout, start).await {
            Ok(r) => r,
            Err(_) => {
                if tap.enabled() && self.spec.http.is_none() {
                    let phase = self.counters.phase();
                    self.view_for(None, Some("starting"), |v| {
                        self.with_stderr(|s| tap.on_init_timeout(phase, v, Some(s)));
                    });
                }
                Err(format!(
                    "start timed out after {} ms",
                    self.spec.start_timeout.as_millis()
                ))
            }
        };
        let mut spawned = None;
        let leftover = {
            let g = &mut *lock(&self.inner);
            let current = g.generation == generation && matches!(g.state, State::Starting { .. });
            match res {
                Ok(conn) if current => {
                    let idle = Arc::new(Notify::new());
                    g.state = State::Running {
                        conn: conn.clone(),
                        idle: idle.clone(),
                    };
                    self.trans(g, "started");
                    let weak = Arc::downgrade(&self);
                    tokio::spawn(monitor(weak.clone(), generation, conn.closed()));
                    tokio::spawn(idle_task(weak, generation, idle.clone()));
                    if g.in_flight == 0 {
                        g.idle_deadline = Some(Instant::now() + self.spec.idle_ttl);
                        idle.notify_one();
                    }
                    tracing::info!(server = %self.name, pid = ?conn.pid(), "backend running");
                    spawned = Some(generation);
                    None
                }
                Ok(conn) => Some(conn),
                Err(error) => {
                    g.failures += 1;
                    let wait = self.tuning.backoff(g.failures);
                    tracing::warn!(server = %self.name, %error, backoff_ms = wait.as_millis() as u64, "backend start failed");
                    g.last_error = Some(error.clone());
                    if current {
                        g.state = State::Failed {
                            until: Instant::now() + wait,
                            error,
                        };
                        self.trans(g, "start_failed");
                    } else {
                        self.counters
                            .failures
                            .store(u64::from(g.failures), Ordering::Relaxed);
                    }
                    None
                }
            }
        };
        if let Some(conn) = leftover {
            conn.shutdown(Duration::ZERO).await;
        }
        if let Some(generation) = spawned {
            self.emit(LifecycleEvent::Spawned {
                generation,
                startup: t0.elapsed(),
            });
            let hooks = lock(&self.spawn_hooks).clone();
            for hook in hooks {
                hook(generation);
            }
        }
        let _ = done.send(true);
    }

    /// `Running -> Draining`; caller holds the lock and has checked the state.
    fn begin_stop(self: &Arc<Self>, g: &mut Inner, reason: UnloadReason) -> watch::Receiver<bool> {
        let State::Running { conn, idle } = std::mem::replace(&mut g.state, State::Stopped) else {
            unreachable!("begin_stop outside Running");
        };
        idle.notify_one();
        let (tx, rx) = watch::channel(false);
        g.state = State::Draining { done: rx.clone() };
        self.trans(g, reason.as_str());
        g.idle_deadline = None;
        g.drain = false;
        tokio::spawn(self.clone().run_stop(g.generation, conn, tx, reason));
        rx
    }

    async fn run_stop(
        self: Arc<Self>,
        generation: u64,
        conn: Arc<Conn>,
        done: watch::Sender<bool>,
        reason: UnloadReason,
    ) {
        let uptime = conn.started().elapsed();
        {
            let g = &mut *lock(&self.inner);
            if g.generation == generation {
                if let State::Draining { done } = &g.state {
                    g.state = State::Stopping { done: done.clone() };
                    self.trans(g, "stop");
                }
            }
        }
        tracing::info!(server = %self.name, pid = ?conn.pid(), "unloading backend");
        conn.shutdown(self.tuning.stop_grace).await;
        {
            let g = &mut *lock(&self.inner);
            if g.generation == generation && matches!(g.state, State::Stopping { .. }) {
                g.state = State::Stopped;
                if conn.started().elapsed() >= self.tuning.healthy_reset {
                    g.failures = 0;
                }
                self.trans(g, "stopped");
            }
        }
        self.emit(LifecycleEvent::Unloaded {
            generation,
            reason,
            uptime,
            exit: stdio_exit(&conn),
        });
        let _ = done.send(true);
    }

    /// Idle check; returns true when the idle task should exit.
    fn try_idle_unload(self: &Arc<Self>, generation: u64) -> bool {
        let g = &mut *lock(&self.inner);
        if g.generation != generation || !matches!(g.state, State::Running { .. }) {
            return true;
        }
        let expired = g.idle_deadline.is_some_and(|d| Instant::now() >= d);
        if g.in_flight != 0 || !expired {
            return false;
        }
        // Checked in the same critical section: a subscription pinned the
        // instance (keep-alive policy). Re-arm instead of unloading.
        if self.keeps_alive(g) {
            g.idle_deadline = Some(Instant::now() + self.spec.idle_ttl);
            return false;
        }
        // The atomic step: still Running, nothing in flight, deadline passed.
        self.begin_stop(g, UnloadReason::Idle);
        true
    }

    fn idle_deadline(&self, generation: u64) -> Option<Option<Instant>> {
        let g = lock(&self.inner);
        if g.generation != generation || !matches!(g.state, State::Running { .. }) {
            return None;
        }
        Some(g.idle_deadline)
    }

    fn running_conn_gen(&self, generation: u64) -> Option<Arc<Conn>> {
        let g = lock(&self.inner);
        match &g.state {
            State::Running { conn, .. } if g.generation == generation => Some(conn.clone()),
            _ => None,
        }
    }

    fn on_closed(self: &Arc<Self>, generation: u64, exit: Option<ExitInfo>) {
        let eager;
        let conn = {
            let g = &mut *lock(&self.inner);
            if g.generation != generation || !matches!(g.state, State::Running { .. }) {
                return;
            }
            let State::Running { conn, idle } = std::mem::replace(&mut g.state, State::Stopped)
            else {
                return;
            };
            idle.notify_one();
            if conn.started().elapsed() >= self.tuning.healthy_reset {
                g.failures = 0;
            }
            g.failures += 1;
            let wait = self.tuning.backoff(g.failures);
            g.idle_deadline = None;
            g.drain = false;
            let error = exit.map_or_else(
                || "backend exited unexpectedly".to_owned(),
                |e| e.describe(),
            );
            g.last_error = Some(error.clone());
            g.state = State::Failed {
                until: Instant::now() + wait,
                error,
            };
            self.trans(g, "crash");
            tracing::warn!(server = %self.name, pid = ?conn.pid(), backoff_ms = wait.as_millis() as u64, "backend crashed");
            eager = self.keeps_alive(g);
            conn
        };
        {
            let tap = self.tap();
            if tap.enabled() {
                let ev = ExitView {
                    code: exit.and_then(|x| x.code),
                    signal: exit.and_then(|x| x.signal),
                };
                self.view_for(Some(&conn), Some("running"), |v| {
                    self.with_stderr(|s| tap.on_exit(ev, v, Some(s)));
                });
            }
        }
        self.emit(LifecycleEvent::Unloaded {
            generation,
            reason: UnloadReason::Crash,
            uptime: conn.started().elapsed(),
            exit,
        });
        // Reap whatever is left of the tree (grandchildren).
        tokio::spawn(async move { conn.shutdown(Duration::ZERO).await });
        if eager {
            tokio::spawn(respawn_pinned(Arc::downgrade(self)));
        }
    }

    fn release(self: &Arc<Self>, conn: &Conn) {
        let g = &mut *lock(&self.inner);
        g.in_flight = g.in_flight.saturating_sub(1);
        if g.in_flight != 0 || g.generation != conn.generation() {
            return;
        }
        if g.drain && matches!(g.state, State::Running { .. }) {
            let reason = g.drain_reason;
            self.begin_stop(g, reason);
            return;
        }
        if let State::Running { idle, .. } = &g.state {
            let idle = idle.clone();
            g.idle_deadline = Some(Instant::now() + self.spec.idle_ttl);
            idle.notify_one();
        }
    }
}

/// Restarts a crashed instance that holds subscriptions (keep-alive), after
/// each backoff, until it runs again or nothing pins it any more. The spawn
/// hooks then replay the subscriptions.
async fn respawn_pinned(inst: Weak<Instance>) {
    loop {
        let wait = {
            let Some(i) = inst.upgrade() else { return };
            let g = lock(&i.inner);
            if !i.keeps_alive(&g) {
                return;
            }
            match &g.state {
                State::Failed { until, .. } => until.saturating_duration_since(Instant::now()),
                State::Stopped => Duration::ZERO,
                // Running, or a start / stop is under way.
                _ => return,
            }
        };
        tokio::time::sleep(wait).await;
        let Some(i) = inst.upgrade() else { return };
        if !i.keeps_alive(&lock(&i.inner)) {
            return;
        }
        match i.acquire().await {
            Ok(guard) => {
                tracing::info!(server = %i.name, "pinned backend respawned");
                drop(guard);
                return;
            }
            Err(e) => {
                tracing::debug!(server = %i.name, error = %e, "pinned backend respawn failed");
                // Back to `Failed` with a longer backoff; loop.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

async fn monitor(inst: Weak<Instance>, generation: u64, mut closed: watch::Receiver<bool>) {
    let _ = closed.wait_for(|c| *c).await;
    let Some(inst) = inst.upgrade() else { return };
    // The stdout EOF can arrive a moment before the exit status is reaped.
    let conn = inst.running_conn_gen(generation);
    let exit = match conn.as_deref() {
        Some(Conn::Stdio(c)) => c.exit_info_wait(EXIT_DRAIN).await,
        _ => None,
    };
    inst.on_closed(generation, exit);
}

/// Exit status of a stdio connection's root process, if it has ended.
fn stdio_exit(conn: &Conn) -> Option<ExitInfo> {
    match conn {
        Conn::Stdio(c) => c.exit_info(),
        Conn::Http(_) => None,
    }
}

async fn idle_task(inst: Weak<Instance>, generation: u64, idle: Arc<Notify>) {
    loop {
        let Some(deadline) = inst.upgrade().and_then(|i| i.idle_deadline(generation)) else {
            return;
        };
        match deadline {
            None => idle.notified().await,
            Some(d) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(d) => {
                        let Some(i) = inst.upgrade() else { return };
                        if i.try_idle_unload(generation) {
                            return;
                        }
                    }
                    _ = idle.notified() => {}
                }
            }
        }
    }
}

/// An in-flight call on a running backend. Dropping it ends the call's
/// claim on the instance (and may arm the idle timer).
pub struct CallGuard {
    inst: Arc<Instance>,
    conn: Arc<Conn>,
}

impl std::fmt::Debug for CallGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallGuard")
            .field("server", &self.inst.name)
            .field("pid", &self.conn.pid())
            .finish()
    }
}

impl CallGuard {
    /// The connection this call uses.
    pub fn conn(&self) -> &Arc<Conn> {
        &self.conn
    }

    /// The instance this call runs on.
    pub fn instance(&self) -> &Arc<Instance> {
        &self.inst
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.inst.release(&self.conn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let t = Tuning::default();
        let secs: Vec<u64> = (1..=8).map(|f| t.backoff(f).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(t.backoff(u32::MAX), Duration::from_secs(60));
    }

    /// A missing program fails the start, enters Failed with backoff, fails
    /// fast during the backoff and retries after it. Runs with paused time
    /// (no real process I/O is awaited).
    #[tokio::test(start_paused = true)]
    async fn start_failure_backs_off() {
        let spec = BackendSpec::new("mcpdoze-test-no-such-program-xyz", Vec::<String>::new());
        let inst = Instance::new("missing", spec, Arc::new(super::super::NoEvents));
        let e = inst.acquire().await.unwrap_err();
        assert!(matches!(e, CallError::Unavailable(_)), "{e:?}");
        assert_eq!(inst.state(), StateKind::Failed);
        assert_eq!(inst.failures(), 1);
        assert_eq!(inst.spawn_count(), 1);
        // Within the backoff: fails fast without spawning.
        assert!(inst.acquire().await.is_err());
        assert_eq!(inst.spawn_count(), 1);
        tokio::time::advance(Duration::from_millis(1100)).await;
        assert!(inst.acquire().await.is_err());
        assert_eq!(inst.spawn_count(), 2);
        assert_eq!(inst.failures(), 2);
    }
}
