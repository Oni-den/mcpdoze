// SPDX-License-Identifier: GPL-3.0-or-later

//! Catalog upkeep (refresh, probe, re-list) and the aggregated list methods.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};

use super::catalog;
use super::{
    naming, uri, CatalogEntry, RefreshOutcome, Router, Server, Session, CATALOG_VERSION,
    LIST_TIMEOUT, MAX_LIST_ITEMS, MAX_LIST_PAGES,
};
use crate::backend::stdio::lock;
use crate::backend::{CallError, CallMeta, Conn, Instance, NoEvents};

/// Clears a server's deduplication flag when a background refresh ends.
pub(super) struct BgFlag(Arc<Server>);

impl Drop for BgFlag {
    fn drop(&mut self) {
        self.0.bg_refresh.store(false, Ordering::SeqCst);
    }
}

pub(super) enum ListError {
    Call(CallError),
    Rpc(Value),
}

/// One list request with a timeout (cancelled at the backend on timeout).
pub(super) async fn call_bounded(
    conn: &Conn,
    method: &str,
    params: Value,
) -> Result<Value, CallError> {
    let (id, rx) = conn.begin(method, params, CallMeta::default()).await?;
    match tokio::time::timeout(LIST_TIMEOUT, rx).await {
        Ok(r) => r.unwrap_or(Err(CallError::Closed)),
        Err(_) => {
            conn.cancel(id, "timed out");
            Err(CallError::Unavailable(format!("{method} timed out")))
        }
    }
}

/// Every page of a list method (bounded in pages and items).
pub(super) async fn list_paged(
    conn: &Conn,
    method: &str,
    key: &str,
) -> Result<Vec<Value>, ListError> {
    let mut out = Vec::new();
    let mut cursor: Option<Value> = None;
    for _ in 0..MAX_LIST_PAGES {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let resp = call_bounded(conn, method, params)
            .await
            .map_err(ListError::Call)?;
        let Some(result) = resp.get("result") else {
            return Err(ListError::Rpc(
                resp.get("error").cloned().unwrap_or(Value::Null),
            ));
        };
        if let Some(list) = result[key].as_array() {
            let room = MAX_LIST_ITEMS.saturating_sub(out.len());
            out.extend(list.iter().take(room).cloned());
        }
        cursor = result.get("nextCursor").filter(|c| !c.is_null()).cloned();
        if cursor.is_none() || out.len() >= MAX_LIST_ITEMS {
            break;
        }
    }
    Ok(out)
}

/// A list the backend may not implement: JSON-RPC errors mean empty.
pub(super) async fn list_optional(
    conn: &Conn,
    method: &str,
    key: &str,
) -> Result<Vec<Value>, CallError> {
    match list_paged(conn, method, key).await {
        Ok(v) => Ok(v),
        Err(ListError::Rpc(_)) => Ok(Vec::new()),
        Err(ListError::Call(e)) => Err(e),
    }
}

/// Lists everything a backend offers, per its advertised capabilities.
pub(super) async fn list_all(conn: &Conn) -> Result<CatalogEntry, CallError> {
    let init = conn.init_result();
    let caps = init
        .get("capabilities")
        .cloned()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let tools = match list_paged(conn, "tools/list", "tools").await {
        Ok(v) => v,
        Err(ListError::Rpc(_)) if caps.get("tools").is_none() => Vec::new(),
        Err(ListError::Rpc(e)) => {
            return Err(CallError::Unavailable(format!("tools/list error: {e}")));
        }
        Err(ListError::Call(e)) => return Err(e),
    };
    let prompts = if caps.get("prompts").is_some() {
        list_optional(conn, "prompts/list", "prompts").await?
    } else {
        Vec::new()
    };
    let (resources, resource_templates) = if caps.get("resources").is_some() {
        (
            list_optional(conn, "resources/list", "resources").await?,
            list_optional(conn, "resources/templates/list", "resourceTemplates").await?,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    Ok(CatalogEntry {
        version: CATALOG_VERSION,
        server_info: init.get("serverInfo").cloned().unwrap_or(Value::Null),
        capabilities: caps,
        tools,
        prompts,
        resources,
        resource_templates,
        ..CatalogEntry::default()
    })
}

pub(super) fn expired(entry: &CatalogEntry, server: &Server) -> bool {
    match server.options.max_age {
        Some(age) => catalog::now_secs().saturating_sub(entry.updated_at) >= age.as_secs().max(1),
        None => false,
    }
}

impl Router {
    /// Re-lists server `name` now: on its running process when it has one,
    /// else by probing. `None` if unknown.
    pub async fn refresh(&self, name: &str) -> Option<Result<RefreshOutcome, String>> {
        let server = self.server(name)?;
        Some(self.refresh_server(&server, None, true, "requested").await)
    }

    pub(super) fn spawn_refresh(
        &self,
        server: &Arc<Server>,
        conn: Option<Arc<Conn>>,
        allow_probe: bool,
        why: &'static str,
    ) {
        let (Some(me), Ok(rt)) = (self.me.upgrade(), tokio::runtime::Handle::try_current()) else {
            return;
        };
        let server = server.clone();
        rt.spawn(async move {
            let _ = me.refresh_server(&server, conn, allow_probe, why).await;
        });
    }

    /// Like `spawn_refresh`, but at most one queued per server.
    pub(super) fn spawn_dedup_refresh(
        &self,
        server: &Arc<Server>,
        conn: Option<Arc<Conn>>,
        allow_probe: bool,
        why: &'static str,
    ) {
        if server.bg_refresh.swap(true, Ordering::SeqCst) {
            return;
        }
        let flag = BgFlag(server.clone());
        let (Some(me), Ok(rt)) = (self.me.upgrade(), tokio::runtime::Handle::try_current()) else {
            return;
        };
        let server = server.clone();
        rt.spawn(async move {
            let _flag = flag;
            let _ = me.refresh_server(&server, conn, allow_probe, why).await;
        });
    }

    pub(super) async fn refresh_server(
        &self,
        server: &Arc<Server>,
        conn: Option<Arc<Conn>>,
        allow_probe: bool,
        why: &'static str,
    ) -> Result<RefreshOutcome, String> {
        let _g = server.refresh_lock.lock().await;
        if !self.is_current(server) {
            return Err("server is no longer registered".into());
        }
        let conn = conn.or_else(|| server.any_running_conn());
        let listed = match conn {
            Some(c) => {
                server.listed_gen.store(c.generation(), Ordering::SeqCst);
                list_all(&c).await.map_err(|e| e.to_string())
            }
            None if allow_probe => self.probe_list(server).await,
            None => return Err("backend not running".into()),
        };
        match listed {
            Ok(entry) => {
                let (outcome, _) = self.apply_entry(server, entry);
                tracing::debug!(server = %server.name, why, changed = outcome.changed, "catalog refreshed");
                Ok(outcome)
            }
            Err(e) => {
                tracing::warn!(server = %server.name, why, error = %e, "catalog refresh failed");
                Err(e)
            }
        }
    }

    /// Starts a throwaway instance of `server`, lists it and stops it.
    pub(super) async fn probe_list(&self, server: &Server) -> Result<CatalogEntry, String> {
        if self.is_closing() {
            return Err("broker is shutting down".into());
        }
        let inst = Instance::with_tuning(
            &server.name,
            server.instance.spec().clone(),
            Arc::new(NoEvents),
            self.tuning,
        );
        let id = self.next_probe.fetch_add(1, Ordering::Relaxed);
        lock(&self.probes).insert(id, inst.clone());
        self.probe_count.fetch_add(1, Ordering::Relaxed);
        tracing::info!(server = %server.name, "probing backend for its catalog");
        let res = async {
            let guard = inst.acquire().await.map_err(|e| e.to_string())?;
            let r = list_all(guard.conn()).await.map_err(|e| e.to_string());
            drop(guard);
            r
        }
        .await;
        inst.shutdown().await;
        lock(&self.probes).remove(&id);
        res
    }

    /// Stores `new` as the entry of `server` when it differs and tells
    /// streaming sessions about changed lists.
    pub(super) fn apply_entry(
        &self,
        server: &Arc<Server>,
        mut new: CatalogEntry,
    ) -> (RefreshOutcome, Arc<CatalogEntry>) {
        new.version = CATALOG_VERSION;
        new.fingerprint = server.fingerprint.clone();
        new.updated_at = catalog::now_secs();
        let old = self.catalog.entry(&server.name);
        let changes = match &old {
            Some(o) => o.diff(&new),
            None => CatalogEntry::default().diff(&new),
        };
        let meta_changed = old.as_deref().is_none_or(|o| {
            o.fingerprint != new.fingerprint
                || o.capabilities != new.capabilities
                || o.server_info != new.server_info
        });
        let outcome = RefreshOutcome {
            changed: changes.any(),
            tools: new.tools.len(),
            prompts: new.prompts.len(),
            resources: new.resources.len(),
            resource_templates: new.resource_templates.len(),
        };
        self.refresh_count.fetch_add(1, Ordering::Relaxed);
        let new = Arc::new(new);
        if !self.is_current(server) {
            return (outcome, new);
        }
        if changes.any() || meta_changed || server.options.max_age.is_some() {
            self.catalog.put(&server.name, (*new).clone());
        }
        if changes.any() {
            tracing::info!(
                server = %server.name,
                tools = changes.tools,
                prompts = changes.prompts,
                resources = changes.resources,
                "catalog changed"
            );
            for (on, method) in [
                (changes.tools, "notifications/tools/list_changed"),
                (changes.prompts, "notifications/prompts/list_changed"),
                (changes.resources, "notifications/resources/list_changed"),
            ] {
                if on {
                    self.broadcast(&json!({"jsonrpc": "2.0", "method": method}));
                }
            }
        }
        (outcome, new)
    }

    pub(super) fn on_list_changed(&self, server: &str) {
        let Some(server) = self.server(server) else {
            return;
        };
        if let Some(conn) = server.any_running_conn() {
            self.spawn_refresh(&server, Some(conn), false, "list_changed");
        }
    }

    /// Called with every connection a forwarded call uses: a new process of
    /// a catalogued server is re-listed in the background.
    pub(super) fn note_conn(&self, server: &Arc<Server>, conn: &Arc<Conn>) {
        let generation = conn.generation();
        if server.listed_gen.load(Ordering::SeqCst) == generation
            || self.catalog.entry(&server.name).is_none()
        {
            return;
        }
        if server.listed_gen.swap(generation, Ordering::SeqCst) != generation {
            self.spawn_refresh(server, Some(conn.clone()), false, "spawn");
        }
    }

    /// Lists a server without an entry from its real instance (starting it
    /// if needed) and stores the entry.
    pub(super) async fn list_uncatalogued(
        &self,
        server: &Arc<Server>,
    ) -> Result<Arc<CatalogEntry>, String> {
        let _g = server.refresh_lock.lock().await;
        if let Some(e) = self.catalog.entry(&server.name) {
            return Ok(e);
        }
        let guard = server.instance.acquire().await.map_err(|e| e.to_string())?;
        server
            .listed_gen
            .store(guard.conn().generation(), Ordering::SeqCst);
        let entry = list_all(guard.conn()).await.map_err(|e| e.to_string())?;
        drop(guard);
        Ok(self.apply_entry(server, entry).1)
    }

    /// Catalog entries of every server; servers without one are listed now,
    /// failures are logged and skipped. `session` is the asking agent
    /// session (not used yet: every session sees the same catalog).
    pub async fn catalog_entries(
        &self,
        _session: &Session,
    ) -> Vec<(Arc<Server>, Arc<CatalogEntry>)> {
        let mut out = Vec::new();
        for server in self.servers() {
            let entry = match self.catalog.entry(&server.name) {
                Some(e) => {
                    if expired(&e, &server) {
                        self.spawn_dedup_refresh(
                            &server,
                            None,
                            server.options.probe,
                            "catalog expired",
                        );
                    }
                    e
                }
                None => match self.list_uncatalogued(&server).await {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(server = %server.name, error = %e, "list failed");
                        continue;
                    }
                },
            };
            out.push((server, entry));
        }
        out
    }

    /// Aggregated, prefixed tool list.
    pub async fn list_tools(&self, session: &Session) -> Vec<Value> {
        let mut out = Vec::new();
        for (server, entry) in self.catalog_entries(session).await {
            for tool in &entry.tools {
                let Some(name) = tool["name"].as_str() else {
                    continue;
                };
                let mut t = tool.clone();
                t["name"] = Value::String(naming::join(&server.prefix, name));
                out.push(t);
            }
        }
        out
    }

    /// Aggregated, prefixed prompt list.
    pub async fn list_prompts(&self, session: &Session) -> Vec<Value> {
        let mut out = Vec::new();
        for (server, entry) in self.catalog_entries(session).await {
            for p in &entry.prompts {
                let Some(name) = p["name"].as_str() else {
                    continue;
                };
                let mut p = p.clone();
                p["name"] = Value::String(naming::join(&server.prefix, name));
                out.push(p);
            }
        }
        out
    }

    /// Aggregated resource list with mapped URIs.
    pub async fn list_resources(&self, session: &Session) -> Vec<Value> {
        let mut out = Vec::new();
        for (server, entry) in self.catalog_entries(session).await {
            for r in &entry.resources {
                let Some(u) = r["uri"].as_str() else {
                    continue;
                };
                let mut r = r.clone();
                r["uri"] = Value::String(uri::map_uri(&server.prefix, u));
                out.push(r);
            }
        }
        out
    }

    /// Aggregated resource template list with mapped URI templates.
    pub async fn list_resource_templates(&self, session: &Session) -> Vec<Value> {
        let mut out = Vec::new();
        for (server, entry) in self.catalog_entries(session).await {
            for t in &entry.resource_templates {
                let Some(u) = t["uriTemplate"].as_str() else {
                    continue;
                };
                let mut t = t.clone();
                t["uriTemplate"] = Value::String(uri::map_template(&server.prefix, u));
                out.push(t);
            }
        }
        out
    }
}
