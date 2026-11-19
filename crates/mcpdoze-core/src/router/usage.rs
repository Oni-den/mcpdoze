// SPDX-License-Identifier: GPL-3.0-or-later

//! Feeds the statistics log ([`crate::stats`]): lifecycle hooks, call and
//! session events, and the periodic memory sampler. Everything here only
//! queues events; nothing blocks the request path.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::{scope_name, Era, Router, Server, Session};
use crate::backend::jsonrpc;
use crate::backend::stdio::lock;
use crate::backend::{Conn, Instance, LifecycleEvent};
use crate::stats::{
    self, CallEvent, CallKind, Event, InstanceMeasure, SampleInput, SessionEvent, SpawnEvent,
    Stats, UnloadEvent,
};

/// What [`Router::forward`](super::Router) learned about one call.
#[derive(Debug, Default)]
pub(super) struct CallTrace {
    /// The backend was not running when the call arrived.
    pub(super) cold_start: bool,
    /// Time to obtain a running backend.
    pub(super) acquire: Duration,
    /// Time from sending to the backend to its response.
    pub(super) backend: Duration,
    /// Instance key ("" for the shared instance).
    pub(super) scope_key: Option<String>,
    /// Request number shared by the statistics log and diagnostics.
    pub(super) req: u64,
}

/// Facts about a call known before it is forwarded.
pub(super) struct CallStart {
    pub(super) kind: CallKind,
    pub(super) name: String,
    pub(super) bytes_in: u64,
    pub(super) started: std::time::Instant,
    /// In-memory hash of the arguments (tool calls); never logged.
    pub(super) args_hash: Option<u64>,
}

/// What later transforms (optimize phase) learned about a call, to be
/// recorded with it. `Default` means "untouched".
#[derive(Debug, Default, Clone)]
pub(super) struct CallExtras {
    /// Response size as sent to the agent; `None`: same as the backend's.
    pub(super) bytes_sent: Option<u64>,
    /// Tags of the transforms applied (empty: none).
    pub(super) opt: Vec<String>,
    /// Served from the result cache.
    pub(super) cached: bool,
}

/// A call being measured: created before forwarding, finished once the
/// final response (after every transform) is known.
#[allow(dead_code)] // used by the optimize phase
pub(super) struct CallRecord {
    pub(super) st: Arc<Stats>,
    pub(super) start: CallStart,
    pub(super) trace: CallTrace,
}

#[allow(dead_code)]
impl CallRecord {
    /// Writes the call event. `resp` is the response as received from the
    /// backend (`bytes_out`); `extras` describes what was changed after.
    pub(super) fn finish(
        self,
        router: &Router,
        session: &Session,
        server: &Server,
        resp: &Value,
        extras: CallExtras,
    ) {
        router.record_call_with(
            &self.st,
            session,
            server,
            self.start,
            &self.trace,
            resp,
            extras,
        );
    }
}

fn instance_tag(scope_key: &str) -> Option<String> {
    (!scope_key.is_empty()).then(|| stats::short_hash(scope_key))
}

/// The failure kind of a call, from its response (the broker's own error
/// shapes and backend JSON-RPC errors); `None` for successes and for tool
/// results that merely carry `isError`. Kinds are those of `diag::Kind`
/// (snake case), so the statistics log and the failure log agree.
fn classify_fail(resp: &Value) -> Option<String> {
    let err = resp.get("error").filter(|e| e.is_object())?;
    let code = err["code"].as_i64().unwrap_or(0);
    let msg = err["message"].as_str().unwrap_or("");
    let kind = if code == jsonrpc::REQUEST_CANCELLED {
        "agent_cancelled"
    } else if code == jsonrpc::BACKEND_UNAVAILABLE {
        if msg.contains("(call_timeout)") {
            "stalled"
        } else if msg.contains("connection closed") {
            "crash_in_flight"
        } else if msg.contains("; retry in ") {
            "backoff_rejected"
        } else {
            "backend_unavailable"
        }
    } else {
        "rpc_error"
    };
    Some(kind.to_owned())
}

impl CallStart {
    /// `None` for methods that are not logged.
    pub(super) fn new(method: &str, params: &Value) -> Option<Self> {
        let kind = CallKind::of_method(method)?;
        let name = match kind {
            CallKind::Tool | CallKind::Prompt => params["name"].as_str().unwrap_or("").to_owned(),
            CallKind::Resource => stats::short_hash(params["uri"].as_str().unwrap_or("")),
        };
        Some(Self {
            kind,
            name,
            bytes_in: stats::json_len(params),
            started: std::time::Instant::now(),
            args_hash: (kind == CallKind::Tool)
                .then(|| stats::call_hash("", "", params.get("arguments").unwrap_or(&Value::Null))),
        })
    }
}

impl Router {
    /// Starts logging statistics to `stats` (and the memory sampler, which
    /// needs a Tokio runtime). Replaces a previous log.
    pub fn set_stats(&self, stats: Arc<Stats>) {
        let interval = stats.sample_interval();
        *self.stats.write().unwrap_or_else(|e| e.into_inner()) = Some(stats);
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let me = self.me.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(r) = me.upgrade() else { return };
                let Some(st) = r.stats() else { return };
                let input = r.measure().await;
                drop(r);
                st.sample(input);
            }
        });
    }

    /// The statistics log, when enabled.
    pub fn stats(&self) -> Option<Arc<Stats>> {
        self.stats.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Measures the broker and every running stdio instance (in a blocking
    /// task: process scans can be slow).
    async fn measure(&self) -> SampleInput {
        let mut conns: Vec<(String, Option<String>, Arc<Conn>)> = Vec::new();
        for server in self.servers() {
            for inst in server.all_instances() {
                if let Some(conn) = inst.running_conn() {
                    let tag = instance_tag(&inst.spec().client.scope_key);
                    conns.push((server.name.clone(), tag, conn));
                }
            }
        }
        let sessions = self.session_count();
        tokio::task::spawn_blocking(move || {
            let instances = conns
                .into_iter()
                .filter_map(|(server, instance, conn)| match &*conn {
                    Conn::Stdio(c) => Some(InstanceMeasure {
                        server,
                        instance,
                        memory: c.memory(),
                    }),
                    // Remote servers use no local memory.
                    _ => None,
                })
                .collect();
            SampleInput {
                broker: crate::proctree::self_memory(),
                sessions,
                instances,
            }
        })
        .await
        .unwrap_or_default()
    }

    /// Logs spawns and unloads of `inst`.
    pub(super) fn hook_stats(&self, server: &str, inst: &Arc<Instance>) {
        let me = self.me.clone();
        let server = server.to_owned();
        let instance = instance_tag(&inst.spec().client.scope_key);
        inst.on_lifecycle(move |ev| {
            let Some(st) = me.upgrade().and_then(|r| r.stats()) else {
                return;
            };
            let ts = stats::now_ms();
            st.record(match ev {
                LifecycleEvent::Spawned {
                    generation,
                    startup,
                } => Event::Spawn(SpawnEvent {
                    ts,
                    server: server.clone(),
                    instance: instance.clone(),
                    startup_ms: startup.as_millis() as u64,
                    generation: *generation,
                }),
                LifecycleEvent::Unloaded {
                    generation,
                    reason,
                    uptime,
                    exit,
                } => Event::Unload(UnloadEvent {
                    ts,
                    server: server.clone(),
                    instance: instance.clone(),
                    reason: reason.as_str().to_owned(),
                    uptime_s: uptime.as_secs(),
                    rss_mb: None,
                    exit_code: exit.and_then(|e| e.code),
                    signal: exit.and_then(|e| e.signal),
                    generation: *generation,
                }),
            });
        });
    }

    /// Logs a session opening or closing.
    pub(super) fn record_session(&self, open: bool) {
        if let Some(st) = self.stats() {
            st.record(Event::Session(SessionEvent {
                ts: stats::now_ms(),
                action: if open { "open" } else { "close" }.to_owned(),
                count: lock(&self.sessions).len(),
            }));
        }
    }

    /// Logs one forwarded call with no later transform.
    pub(super) fn record_call(
        &self,
        st: &Stats,
        session: &Session,
        server: &Server,
        start: CallStart,
        trace: &CallTrace,
        resp: &Value,
    ) {
        self.record_call_with(
            st,
            session,
            server,
            start,
            trace,
            resp,
            CallExtras::default(),
        );
    }

    /// Logs one call with the facts later transforms added.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_call_with(
        &self,
        st: &Stats,
        session: &Session,
        server: &Server,
        start: CallStart,
        trace: &CallTrace,
        resp: &Value,
        extras: CallExtras,
    ) {
        let session_key = if session.is_ephemeral() {
            stats::STATELESS_SESSION
        } else {
            stats::session_key(session.id())
        };
        let bytes_out = stats::json_len(resp);
        let dup = start.args_hash.is_some_and(|h| {
            let h = stats::call_hash(&server.name, &start.name, &Value::from(h));
            st.note_call(session_key, h)
        });
        let error_code = resp["error"]["code"].as_i64();
        let tool_error = resp["result"]["isError"] == Value::Bool(true);
        let debug = st.debug();
        let ms = |d: Duration| d.as_millis() as u64;
        st.record(Event::Call(CallEvent {
            ts: stats::now_ms(),
            server: server.name.clone(),
            kind: start.kind,
            name: start.name,
            duration_ms: ms(start.started.elapsed()),
            ok: resp.get("error").is_none() && !tool_error,
            error_code,
            fail: classify_fail(resp),
            cold_start: trace.cold_start,
            spawn_wait_ms: if trace.cold_start {
                ms(trace.acquire)
            } else {
                0
            },
            bytes_in: start.bytes_in,
            bytes_out,
            era: match session.era() {
                Era::Legacy => "legacy",
                Era::Modern => "modern",
            }
            .to_owned(),
            scope: scope_name(server.scope.scope).to_owned(),
            instance: trace.scope_key.as_deref().and_then(instance_tag),
            req_id: debug.then_some(trace.req),
            acquire_ms: debug.then(|| ms(trace.acquire)),
            backend_ms: debug.then(|| ms(trace.backend)),
            bytes_sent: extras.bytes_sent.filter(|b| *b != bytes_out),
            opt: extras.opt,
            cached: extras.cached,
            dup,
            session: Some(session_key),
        }));
    }
}

impl Router {
    /// Size in bytes of each server's tool list as served to agents (the
    /// shared catalog entry, names prefixed), for `GET /stats`. Servers
    /// without a catalog entry are absent.
    pub fn tool_list_bytes(&self) -> std::collections::HashMap<String, u64> {
        let mut out = std::collections::HashMap::new();
        for server in self.servers() {
            let Some((entry, _)) = self.entry_for(&server, None) else {
                continue;
            };
            let tools: Vec<Value> = entry
                .tools
                .iter()
                .filter_map(|tool| {
                    let name = tool["name"].as_str()?;
                    let mut t = tool.clone();
                    t["name"] = Value::String(super::naming::join(&server.prefix, name));
                    Some(t)
                })
                .collect();
            out.insert(server.name.clone(), stats::json_len(&Value::Array(tools)));
        }
        out
    }
}
