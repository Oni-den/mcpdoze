// SPDX-License-Identifier: GPL-3.0-or-later

//! Resource subscriptions (see `docs/SUBSCRIPTIONS.md`).
//!
//! The registry maps `(server, scope_key, backend URI)` to the agent
//! sessions subscribed to it. The backend sees one subscriber, the broker:
//!
//! * the first session (0 -> 1) makes the broker send `resources/subscribe`
//!   to the instance that serves that session, and pins the instance;
//! * the last one leaving (1 -> 0: `resources/unsubscribe`, session end,
//!   reaper, `DELETE`, a closed `subscriptions/listen` stream) makes it send
//!   `resources/unsubscribe` and unpin;
//! * `notifications/resources/updated` from an instance goes only to the
//!   sessions subscribed to that URI on that instance, with the URI mapped
//!   to its `mcpdoze://` form.
//!
//! A pinned instance stays loaded past its idle TTL under the `keep-alive`
//! policy (the default) and is respawned eagerly after a crash; with
//! `allow-unload` it unloads normally. Either way, when a new process of
//! the instance comes up, its spawn hook replays every subscription made on
//! an older process and then sends one synthetic `updated` per URI, since
//! changes may have been missed meanwhile.
//!
//! Backend transitions and replays are serialised by one async lock, so a
//! subscribe racing an unsubscribe (or a respawn) never leaves the backend
//! and the registry out of step.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::{json, Value};

use super::lists::call_bounded;
use super::{uri, Router, Server, Session};
use crate::backend::jsonrpc;
use crate::backend::stdio::lock;
use crate::backend::{Conn, Instance};

/// One subscribed resource on one backend instance.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct SubKey {
    pub(super) server: String,
    /// Scope key of the instance (`""` for the shared one).
    pub(super) scope_key: String,
    /// URI as the backend knows it.
    pub(super) uri: String,
}

struct Entry {
    sessions: HashMap<String, Weak<Session>>,
    /// Lifecycle generation of the process the backend subscription was
    /// made on (`None` while it could not be made).
    generation: Option<u64>,
}

/// The subscription registry of a router.
#[derive(Default)]
pub(super) struct Subscriptions {
    map: Mutex<BTreeMap<SubKey, Entry>>,
    op: tokio::sync::Mutex<()>,
    backend_subscribes: AtomicU64,
    backend_unsubscribes: AtomicU64,
}

/// Counters of the subscription registry (status and tests).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscriptionStats {
    /// Subscribed `(server, instance, URI)` triples.
    pub active: usize,
    /// `resources/subscribe` requests sent to backends (including replays).
    pub backend_subscribes: u64,
    /// `resources/unsubscribe` requests sent to backends.
    pub backend_unsubscribes: u64,
}

type RpcErr = (i64, String);

fn rpc_failure(resp: &Value) -> Option<RpcErr> {
    let e = resp.get("error")?;
    Some((
        e["code"].as_i64().unwrap_or(jsonrpc::INTERNAL_ERROR),
        e["message"].as_str().unwrap_or("backend error").to_owned(),
    ))
}

impl Router {
    /// Registry counters.
    pub fn subscription_stats(&self) -> SubscriptionStats {
        SubscriptionStats {
            active: lock(&self.subs.map).len(),
            backend_subscribes: self.subs.backend_subscribes.load(Ordering::Relaxed),
            backend_unsubscribes: self.subs.backend_unsubscribes.load(Ordering::Relaxed),
        }
    }

    /// True if any registered backend advertises `resources.subscribe`.
    pub(super) fn any_subscribe(&self) -> bool {
        self.servers()
            .iter()
            .any(|s| self.server_subscribes(s, None))
    }

    /// True if `server` advertises `resources.subscribe` (catalog entry
    /// for `ckey`, else a running connection's `initialize` result).
    pub(super) fn server_subscribes(&self, server: &Server, ckey: Option<&str>) -> bool {
        let yes = |caps: &Value| caps["resources"]["subscribe"] == true;
        if let Some((e, _)) = self.entry_for(server, ckey) {
            if yes(&e.capabilities) {
                return true;
            }
        }
        server
            .any_running_conn()
            .is_some_and(|c| yes(&c.init_result()["capabilities"]))
    }

    /// Registers the spawn hook that replays subscriptions on a new
    /// process of `inst` (scope key `scope_key` of `server`).
    pub(super) fn hook_instance(&self, server: &str, inst: &Arc<Instance>) {
        let me = self.me.clone();
        let server = server.to_owned();
        let scope_key = inst.spec().client.scope_key.clone();
        let weak = Arc::downgrade(inst);
        inst.on_spawn(move |generation| {
            let (Some(r), Some(inst), Ok(rt)) = (
                me.upgrade(),
                weak.upgrade(),
                tokio::runtime::Handle::try_current(),
            ) else {
                return;
            };
            let (server, scope_key) = (server.clone(), scope_key.clone());
            rt.spawn(async move { r.replay(&server, &scope_key, &inst, generation).await });
        });
    }

    /// `resources/subscribe` from a stored session.
    pub(super) async fn subscribe_request(
        &self,
        session: &Arc<Session>,
        id: Value,
        params: Value,
    ) -> Value {
        if session.is_ephemeral() {
            return jsonrpc::error(
                id,
                jsonrpc::INVALID_REQUEST,
                "resources/subscribe needs an agent session (initialize first)",
            );
        }
        let Some(u) = params["uri"].as_str() else {
            return jsonrpc::error(
                id,
                jsonrpc::INVALID_PARAMS,
                "resources/subscribe needs a uri",
            );
        };
        match self.subscribe_uri(session, u).await {
            Ok(()) => jsonrpc::result(id, json!({})),
            Err((code, msg)) => jsonrpc::error(id, code, msg),
        }
    }

    /// `resources/unsubscribe` from a stored session (unknown URIs are fine).
    pub(super) async fn unsubscribe_request(
        &self,
        session: &Arc<Session>,
        id: Value,
        params: Value,
    ) -> Value {
        let Some(u) = params["uri"].as_str() else {
            return jsonrpc::error(
                id,
                jsonrpc::INVALID_PARAMS,
                "resources/unsubscribe needs a uri",
            );
        };
        let Some((server, original)) = self.unmap_server(u) else {
            return jsonrpc::error(id, jsonrpc::INVALID_PARAMS, "unknown resource URI");
        };
        let keys: Vec<SubKey> = lock(&self.subs.map)
            .iter()
            .filter(|(k, e)| {
                k.server == server.name && k.uri == original && e.sessions.contains_key(&session.id)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            self.release(&session.id, &k).await;
        }
        jsonrpc::result(id, json!({}))
    }

    fn unmap_server(&self, mapped: &str) -> Option<(Arc<Server>, String)> {
        uri::unmap(mapped).and_then(|(p, u)| self.by_prefix(p).map(|s| (s, u)))
    }

    /// Subscribes `session` to the agent-visible URI `mapped`.
    pub(super) async fn subscribe_uri(
        &self,
        session: &Arc<Session>,
        mapped: &str,
    ) -> Result<(), RpcErr> {
        let Some((server, original)) = self.unmap_server(mapped) else {
            return Err((jsonrpc::INVALID_PARAMS, "unknown resource URI".into()));
        };
        let ckey = self
            .session_key_now(session)
            .and_then(|k| server.catalog_key_of(&k));
        if !self.server_subscribes(&server, ckey.as_deref()) {
            return Err((
                jsonrpc::METHOD_NOT_FOUND,
                format!(
                    "server {:?} does not support resource subscriptions",
                    server.name
                ),
            ));
        }
        let inst = self.instance_for(session, &server).await?;
        let key = SubKey {
            server: server.name.clone(),
            scope_key: inst.spec().client.scope_key.clone(),
            uri: original,
        };
        self.ensure_end_hook(session);
        let _op = self.subs.op.lock().await;
        {
            let mut map = lock(&self.subs.map);
            if let Some(e) = map.get_mut(&key) {
                e.sessions
                    .insert(session.id.clone(), Arc::downgrade(session));
                return Ok(());
            }
        }
        if session.is_ended() {
            return Err((jsonrpc::BACKEND_UNAVAILABLE, "agent session ended".into()));
        }
        // 0 -> 1: subscribe at the backend while holding a call guard, and
        // pin before the guard goes so the idle timer never sees a gap.
        let guard = inst
            .acquire()
            .await
            .map_err(|e| (jsonrpc::BACKEND_UNAVAILABLE, e.to_string()))?;
        let conn = guard.conn().clone();
        self.backend_subscribe(&conn, &key.uri).await?;
        inst.pin();
        conn.set_listening(true);
        let mut sessions = HashMap::new();
        sessions.insert(session.id.clone(), Arc::downgrade(session));
        lock(&self.subs.map).insert(
            key,
            Entry {
                sessions,
                generation: Some(conn.generation()),
            },
        );
        drop(guard);
        Ok(())
    }

    async fn backend_subscribe(&self, conn: &Conn, uri: &str) -> Result<(), RpcErr> {
        self.subs.backend_subscribes.fetch_add(1, Ordering::Relaxed);
        let resp = call_bounded(conn, "resources/subscribe", json!({ "uri": uri }))
            .await
            .map_err(|e| (jsonrpc::BACKEND_UNAVAILABLE, e.to_string()))?;
        match rpc_failure(&resp) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Releases every subscription of `session` once it ends.
    fn ensure_end_hook(&self, session: &Arc<Session>) {
        if session.subs_hooked.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.me.clone();
        let id = session.id.clone();
        session.on_end(move || {
            let (Some(r), Ok(rt)) = (me.upgrade(), tokio::runtime::Handle::try_current()) else {
                return;
            };
            rt.spawn(async move { r.release_session(&id).await });
        });
    }

    /// Drops session `id` from every subscription it holds.
    pub(super) async fn release_session(&self, id: &str) {
        let keys: Vec<SubKey> = lock(&self.subs.map)
            .iter()
            .filter(|(_, e)| e.sessions.contains_key(id))
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            self.release(id, &k).await;
        }
    }

    /// Drops session `id` from `key`; the last one unsubscribes at the
    /// backend and unpins the instance.
    async fn release(&self, id: &str, key: &SubKey) {
        let _op = self.subs.op.lock().await;
        let generation = {
            let mut map = lock(&self.subs.map);
            let Some(e) = map.get_mut(key) else { return };
            e.sessions.remove(id);
            // Sessions dropped without ending count as gone too.
            e.sessions.retain(|_, w| w.strong_count() > 0);
            if !e.sessions.is_empty() {
                return;
            }
            map.remove(key).and_then(|e| e.generation)
        };
        let Some(inst) = self.instance_of(&key.server, &key.scope_key) else {
            return;
        };
        if let Some(conn) = inst.running_conn() {
            if generation == Some(conn.generation()) {
                self.subs
                    .backend_unsubscribes
                    .fetch_add(1, Ordering::Relaxed);
                match call_bounded(&conn, "resources/unsubscribe", json!({ "uri": key.uri })).await
                {
                    Ok(r) if r.get("error").is_some() => {
                        tracing::debug!(server = %key.server, "backend refused resources/unsubscribe")
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(server = %key.server, error = %e, "resources/unsubscribe failed")
                    }
                }
            }
            if inst.pins() <= 1 {
                conn.set_listening(false);
            }
        }
        inst.unpin();
    }

    /// The instance with scope key `scope_key` of server `name`.
    fn instance_of(&self, name: &str, scope_key: &str) -> Option<Arc<Instance>> {
        let server = self.server(name)?;
        if scope_key.is_empty() {
            Some(server.instance.clone())
        } else {
            server.scoped_instance(scope_key)
        }
    }

    /// A new process of `inst` is up: re-subscribe what was subscribed on
    /// older processes, then tell subscribers each URI may have changed.
    async fn replay(&self, server: &str, scope_key: &str, inst: &Arc<Instance>, generation: u64) {
        let _op = self.subs.op.lock().await;
        let todo: Vec<SubKey> = lock(&self.subs.map)
            .iter()
            .filter(|(k, e)| {
                k.server == server && k.scope_key == scope_key && e.generation != Some(generation)
            })
            .map(|(k, _)| k.clone())
            .collect();
        if todo.is_empty() {
            return;
        }
        let Ok(guard) = inst.acquire().await else {
            return;
        };
        let conn = guard.conn().clone();
        if conn.generation() != generation {
            // An even newer process; its own hook replays.
            return;
        }
        let mut done = Vec::new();
        for key in todo {
            match self.backend_subscribe(&conn, &key.uri).await {
                Ok(()) => {
                    if let Some(e) = lock(&self.subs.map).get_mut(&key) {
                        e.generation = Some(generation);
                    }
                    done.push(key);
                }
                Err((_, msg)) => {
                    tracing::warn!(server, error = %msg, "resubscribe after respawn failed")
                }
            }
        }
        if !done.is_empty() {
            conn.set_listening(true);
            tracing::info!(
                server,
                count = done.len(),
                "subscriptions replayed on new backend process"
            );
        }
        drop(guard);
        let prefix = self.server(server).map(|s| s.prefix.clone());
        if let Some(prefix) = prefix {
            for key in done {
                let msg = jsonrpc::notification(
                    "notifications/resources/updated",
                    json!({ "uri": key.uri }),
                );
                self.fan_out(&prefix, &key, msg);
            }
        }
    }

    /// `notifications/resources/updated` from an instance of `server`.
    pub(super) fn on_resource_updated(&self, server: &str, scope_key: &str, msg: Value) {
        let Some(u) = msg["params"]["uri"].as_str() else {
            return;
        };
        let Some(s) = self.server(server) else { return };
        let key = SubKey {
            server: server.to_owned(),
            scope_key: scope_key.to_owned(),
            uri: u.to_owned(),
        };
        self.fan_out(&s.prefix, &key, msg);
    }

    /// Sends `msg` (an `updated` notification with a backend URI) to the
    /// subscribers of `key`, the URI mapped.
    fn fan_out(&self, prefix: &str, key: &SubKey, mut msg: Value) {
        let targets: Vec<Arc<Session>> = match lock(&self.subs.map).get(key) {
            Some(e) => e.sessions.values().filter_map(Weak::upgrade).collect(),
            None => return,
        };
        msg["params"]["uri"] = Value::String(uri::map_uri(prefix, &key.uri));
        for s in targets {
            if s.wants_notification("notifications/resources/updated") {
                s.push(msg.clone());
            }
        }
    }
}
