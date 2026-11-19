// SPDX-License-Identifier: GPL-3.0-or-later

//! The result store: full tool results kept in memory so an agent can page
//! through a truncated one with `mcpdoze__result_get`.
//!
//! * A ref is 128 random bits (hex) and is bound to the agent session that
//!   caused the call. A ref of another session, an expired one and an unknown
//!   one are indistinguishable to the caller (no existence oracle).
//! * Entries live [`DEFAULT_TTL`] after their last use and the store is capped
//!   by entry count and by bytes; beyond a cap the least recently used entry
//!   goes first. An entry larger than the byte cap is not stored at all.
//! * A session's entries are purged when it ends (the router registers a
//!   session end hook on the first entry).
//! * Stateless (2026-07-28) requests run on ephemeral sessions with a fresh id
//!   each, and the protocol carries no stable client identity, so the store is
//!   not used for them in v1: the router skips truncation there and
//!   `result_get` says so.
//!
//! Memory is bounded by [`DEFAULT_MAX_BYTES`] (plus bookkeeping); nothing is
//! written to disk.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::stats::json_len;

/// How long an unused entry lives.
pub const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);
/// Most entries kept.
pub const DEFAULT_MAX_ENTRIES: usize = 64;
/// Most bytes kept (sum of the serialised results).
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// What an agent is told for a ref the store cannot serve.
pub const LOST_REF: &str =
    "unknown or expired result ref; call the original tool again to get a fresh result";

/// One stored result.
#[derive(Debug)]
pub struct Stored {
    /// Backend tool the result came from (for messages).
    pub tool: String,
    /// The full `result` object of `tools/call`.
    pub result: Value,
    /// Default page size of `result_get`, bytes.
    pub page: usize,
}

struct Entry {
    session: String,
    data: Arc<Stored>,
    bytes: usize,
    tick: u64,
    last_used: Instant,
}

struct Inner {
    entries: HashMap<String, Entry>,
    bytes: usize,
    tick: u64,
    /// Sessions that already have an end hook.
    hooked: HashSet<String>,
}

/// The in-memory result store. Cheap to share (`Arc`).
pub struct ResultStore {
    inner: Mutex<Inner>,
    ttl: Duration,
    max_entries: usize,
    max_bytes: usize,
}

impl std::fmt::Debug for ResultStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (n, b) = self.usage();
        f.debug_struct("ResultStore")
            .field("entries", &n)
            .field("bytes", &b)
            .finish()
    }
}

impl Default for ResultStore {
    fn default() -> Self {
        Self::with_limits(DEFAULT_TTL, DEFAULT_MAX_ENTRIES, DEFAULT_MAX_BYTES)
    }
}

/// A stored ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Put {
    /// The ref to give the agent.
    pub id: String,
    /// First entry of this session: the caller registers the purge hook.
    pub first_for_session: bool,
}

impl ResultStore {
    /// A store with explicit limits.
    pub fn with_limits(ttl: Duration, max_entries: usize, max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                bytes: 0,
                tick: 0,
                hooked: HashSet::new(),
            }),
            ttl,
            max_entries: max_entries.max(1),
            max_bytes,
        }
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `(entries, bytes)` held now.
    pub fn usage(&self) -> (usize, usize) {
        let g = self.guard();
        (g.entries.len(), g.bytes)
    }

    /// Stores `data` for `session`. `None` when it alone exceeds the byte cap.
    pub fn put(&self, session: &str, data: Stored) -> Option<Put> {
        self.put_at(session, data, Instant::now())
    }

    pub(crate) fn put_at(&self, session: &str, data: Stored, now: Instant) -> Option<Put> {
        let bytes = usize::try_from(json_len(&data.result)).unwrap_or(usize::MAX);
        if bytes > self.max_bytes {
            return None;
        }
        let id = random_ref();
        let mut g = self.guard();
        self.expire(&mut g, now);
        while g.entries.len() >= self.max_entries || g.bytes + bytes > self.max_bytes {
            let Some(victim) = g
                .entries
                .iter()
                .min_by_key(|(_, e)| e.tick)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            Self::remove_locked(&mut g, &victim);
        }
        g.tick += 1;
        let tick = g.tick;
        g.bytes += bytes;
        g.entries.insert(
            id.clone(),
            Entry {
                session: session.to_owned(),
                data: Arc::new(data),
                bytes,
                tick,
                last_used: now,
            },
        );
        let first_for_session = g.hooked.insert(session.to_owned());
        Some(Put {
            id,
            first_for_session,
        })
    }

    /// The entry `id` of `session`; `None` for an unknown, expired or foreign
    /// ref. A hit refreshes its TTL and LRU position.
    pub fn get(&self, session: &str, id: &str) -> Option<Arc<Stored>> {
        self.get_at(session, id, Instant::now())
    }

    pub(crate) fn get_at(&self, session: &str, id: &str, now: Instant) -> Option<Arc<Stored>> {
        let mut g = self.guard();
        self.expire(&mut g, now);
        g.tick += 1;
        let tick = g.tick;
        let e = g.entries.get_mut(id).filter(|e| e.session == session)?;
        e.tick = tick;
        e.last_used = now;
        Some(e.data.clone())
    }

    /// Removes one entry of `session` (a transform that turned out not to
    /// be used).
    pub fn remove(&self, session: &str, id: &str) {
        let mut g = self.guard();
        if g.entries.get(id).is_some_and(|e| e.session == session) {
            Self::remove_locked(&mut g, id);
        }
    }

    /// Drops everything of `session` (session end).
    pub fn purge_session(&self, session: &str) {
        let mut g = self.guard();
        let ids: Vec<String> = g
            .entries
            .iter()
            .filter(|(_, e)| e.session == session)
            .map(|(k, _)| k.clone())
            .collect();
        for id in ids {
            Self::remove_locked(&mut g, &id);
        }
        g.hooked.remove(session);
    }

    fn expire(&self, g: &mut Inner, now: Instant) {
        let dead: Vec<String> = g
            .entries
            .iter()
            .filter(|(_, e)| now.saturating_duration_since(e.last_used) >= self.ttl)
            .map(|(k, _)| k.clone())
            .collect();
        for id in dead {
            Self::remove_locked(g, &id);
        }
    }

    fn remove_locked(g: &mut Inner, id: &str) {
        if let Some(e) = g.entries.remove(id) {
            g.bytes = g.bytes.saturating_sub(e.bytes);
        }
    }
}

fn random_ref() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("OS random number generator unavailable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(n: usize) -> Stored {
        Stored {
            tool: "t".into(),
            result: json!({"content":[{"type":"text","text":"x".repeat(n)}]}),
            page: 100,
        }
    }

    #[test]
    fn refs_are_session_bound() {
        let s = ResultStore::default();
        let p = s.put("a", item(10)).unwrap();
        assert!(p.first_for_session);
        assert_eq!(p.id.len(), 32);
        assert!(!s.put("a", item(10)).unwrap().first_for_session);
        assert!(s.get("a", &p.id).is_some());
        assert!(s.get("b", &p.id).is_none());
        assert!(s.get("a", "nope").is_none());
        s.purge_session("a");
        assert!(s.get("a", &p.id).is_none());
        assert_eq!(s.usage(), (0, 0));
        assert!(s.put("a", item(10)).unwrap().first_for_session);
    }

    #[test]
    fn expires_after_ttl_and_use_refreshes() {
        let s = ResultStore::with_limits(Duration::from_secs(10), 8, 1 << 20);
        let t0 = Instant::now();
        let p = s.put_at("a", item(10), t0).unwrap();
        assert!(s.get_at("a", &p.id, t0 + Duration::from_secs(9)).is_some());
        assert!(s.get_at("a", &p.id, t0 + Duration::from_secs(18)).is_some());
        assert!(s.get_at("a", &p.id, t0 + Duration::from_secs(40)).is_none());
        assert_eq!(s.usage(), (0, 0));
    }

    #[test]
    fn evicts_least_recently_used_by_count() {
        let s = ResultStore::with_limits(DEFAULT_TTL, 2, 1 << 20);
        let a = s.put("s", item(10)).unwrap().id;
        let b = s.put("s", item(10)).unwrap().id;
        assert!(s.get("s", &a).is_some()); // a is now the newest
        let c = s.put("s", item(10)).unwrap().id;
        assert!(s.get("s", &b).is_none());
        assert!(s.get("s", &a).is_some());
        assert!(s.get("s", &c).is_some());
        assert_eq!(s.usage().0, 2);
    }

    #[test]
    fn evicts_by_bytes_and_refuses_oversize() {
        let one = usize::try_from(json_len(&item(400).result)).unwrap();
        let s = ResultStore::with_limits(DEFAULT_TTL, 100, one * 2 + 10);
        let a = s.put("s", item(400)).unwrap().id;
        let b = s.put("s", item(400)).unwrap().id;
        let c = s.put("s", item(400)).unwrap().id;
        assert!(s.get("s", &a).is_none());
        assert!(s.get("s", &b).is_some() && s.get("s", &c).is_some());
        assert!(s.usage().1 <= one * 2 + 10);
        assert!(s.put("s", item(one * 3)).is_none());
        assert!(s.get("s", &b).is_some(), "refusal must not evict");
    }

    #[test]
    fn remove_is_session_checked() {
        let s = ResultStore::default();
        let id = s.put("a", item(5)).unwrap().id;
        s.remove("b", &id);
        assert!(s.get("a", &id).is_some());
        s.remove("a", &id);
        assert!(s.get("a", &id).is_none());
    }
}
