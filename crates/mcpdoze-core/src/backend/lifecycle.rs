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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use super::http::HttpConn;
use super::stdio::{lock, StderrRing, StdioConn};
use super::{BackendEvents, BackendSpec, CallError, Conn};

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
    spawns: AtomicU64,
    calls: AtomicU64,
    spawn_hooks: Mutex<Vec<SpawnHook>>,
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
            }),
            spawns: AtomicU64::new(0),
            calls: AtomicU64::new(0),
            spawn_hooks: Mutex::new(Vec::new()),
        })
    }

    /// Registers `hook`, run (outside the instance lock) every time a new
    /// process of this instance is up and initialised, with its lifecycle
    /// generation, before callers waiting on the start resume. Hooks are
    /// kept for the life of the instance and must not block.
    pub fn on_spawn(&self, hook: impl Fn(u64) + Send + Sync + 'static) {
        lock(&self.spawn_hooks).push(Arc::new(hook));
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
        self.spawns.load(Ordering::Relaxed)
    }

    /// Last lines of backend stderr (across restarts).
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr.lines()
    }

    /// Calls handed a [`CallGuard`] so far.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
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
        let graceful = tokio::time::timeout(timeout, async {
            loop {
                let done = {
                    let g = &mut *lock(&self.inner);
                    match &g.state {
                        State::Stopped | State::Failed { .. } => return,
                        State::Running { .. } if g.in_flight == 0 => Some(self.begin_stop(g)),
                        State::Running { .. } => {
                            // `release` begins the stop when the count hits zero.
                            g.drain = true;
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
            self.shutdown().await;
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
                        self.calls.fetch_add(1, Ordering::Relaxed);
                        return Ok(CallGuard {
                            inst: self.clone(),
                            conn,
                        });
                    }
                    State::Failed { until, error } => {
                        let now = Instant::now();
                        if now < *until {
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
                    Next::Wait(rx) => rx,
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
        loop {
            let mut done = {
                let g = &mut *lock(&self.inner);
                match &g.state {
                    State::Stopped | State::Failed { .. } => return,
                    State::Running { .. } => self.begin_stop(g),
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
        }
    }

    fn begin_start(self: &Arc<Self>, g: &mut Inner) -> watch::Receiver<bool> {
        g.generation += 1;
        let generation = g.generation;
        let (tx, rx) = watch::channel(false);
        g.state = State::Starting { done: rx.clone() };
        self.spawns.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(self.clone().run_start(generation, tx));
        rx
    }

    async fn run_start(self: Arc<Self>, generation: u64, done: watch::Sender<bool>) {
        let start = async {
            let conn = if self.spec.http.is_some() {
                Conn::Http(HttpConn::new(
                    &self.name,
                    generation,
                    &self.spec,
                    self.events.clone(),
                )?)
            } else {
                Conn::Stdio(Box::new(
                    StdioConn::spawn(
                        &self.name,
                        generation,
                        &self.spec,
                        self.events.clone(),
                        self.stderr.clone(),
                    )
                    .map_err(|e| format!("spawn failed: {e}"))?,
                ))
            };
            let conn = Arc::new(conn);
            conn.initialize().await?;
            Ok::<_, String>(conn)
        };
        let res = match tokio::time::timeout(self.spec.start_timeout, start).await {
            Ok(r) => r,
            Err(_) => Err(format!(
                "start timed out after {} ms",
                self.spec.start_timeout.as_millis()
            )),
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
                    }
                    None
                }
            }
        };
        if let Some(conn) = leftover {
            conn.shutdown(Duration::ZERO).await;
        }
        if let Some(generation) = spawned {
            let hooks = lock(&self.spawn_hooks).clone();
            for hook in hooks {
                hook(generation);
            }
        }
        let _ = done.send(true);
    }

    /// `Running -> Draining`; caller holds the lock and has checked the state.
    fn begin_stop(self: &Arc<Self>, g: &mut Inner) -> watch::Receiver<bool> {
        let State::Running { conn, idle } = std::mem::replace(&mut g.state, State::Stopped) else {
            unreachable!("begin_stop outside Running");
        };
        idle.notify_one();
        let (tx, rx) = watch::channel(false);
        g.state = State::Draining { done: rx.clone() };
        g.idle_deadline = None;
        g.drain = false;
        tokio::spawn(self.clone().run_stop(g.generation, conn, tx));
        rx
    }

    async fn run_stop(
        self: Arc<Self>,
        generation: u64,
        conn: Arc<Conn>,
        done: watch::Sender<bool>,
    ) {
        {
            let g = &mut *lock(&self.inner);
            if g.generation == generation {
                if let State::Draining { done } = &g.state {
                    g.state = State::Stopping { done: done.clone() };
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
            }
        }
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
        // The atomic step: still Running, nothing in flight, deadline passed.
        self.begin_stop(g);
        true
    }

    fn idle_deadline(&self, generation: u64) -> Option<Option<Instant>> {
        let g = lock(&self.inner);
        if g.generation != generation || !matches!(g.state, State::Running { .. }) {
            return None;
        }
        Some(g.idle_deadline)
    }

    fn on_closed(&self, generation: u64) {
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
            g.last_error = Some("backend exited unexpectedly".into());
            g.state = State::Failed {
                until: Instant::now() + wait,
                error: "backend exited unexpectedly".into(),
            };
            tracing::warn!(server = %self.name, pid = ?conn.pid(), backoff_ms = wait.as_millis() as u64, "backend crashed");
            conn
        };
        // Reap whatever is left of the tree (grandchildren).
        tokio::spawn(async move { conn.shutdown(Duration::ZERO).await });
    }

    fn release(self: &Arc<Self>, conn: &Conn) {
        let g = &mut *lock(&self.inner);
        g.in_flight = g.in_flight.saturating_sub(1);
        if g.in_flight != 0 || g.generation != conn.generation() {
            return;
        }
        if g.drain && matches!(g.state, State::Running { .. }) {
            self.begin_stop(g);
            return;
        }
        if let State::Running { idle, .. } = &g.state {
            let idle = idle.clone();
            g.idle_deadline = Some(Instant::now() + self.spec.idle_ttl);
            idle.notify_one();
        }
    }
}

async fn monitor(inst: Weak<Instance>, generation: u64, mut closed: watch::Receiver<bool>) {
    let _ = closed.wait_for(|c| *c).await;
    if let Some(inst) = inst.upgrade() {
        inst.on_closed(generation);
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
