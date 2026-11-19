// SPDX-License-Identifier: GPL-3.0-or-later

//! The session result cache (technique 5): strict opt-in, memory only.
//!
//! * Eligible only when the tool matches `cache.tools` AND the catalog says
//!   `annotations.readOnlyHint == true` (the router checks the second part).
//! * Key: (server, instance scope key, session id, tool, canonical-args
//!   hash). Sessions never share entries. Stateless (2026-07-28) requests
//!   have no stable session identity, so there is no cache for them in v1.
//! * Invalidated by: TTL, any call of a tool that is not read-only on the
//!   same server, a backend process unload (respawn), a list change, and a
//!   resource update notification. Errors (JSON-RPC errors and `isError`
//!   results) are never stored.
//! * Bounded by a byte cap with LRU eviction; never written to disk; keys
//!   and values are never logged (the key holds only a hash of the
//!   arguments).

use std::collections::{HashMap, HashSet};
use std::hash::Hasher;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::{CacheConfig, CacheHit, GlobPattern};
use crate::stats::json_len;

/// Tag recorded on a call answered from the cache.
pub const TAG_CACHE: &str = "cache";

/// Most bytes kept (sum of the serialised results).
pub const DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Compiled `cache = { ... }` of one server.
#[derive(Debug)]
pub struct CachePolicy {
    ttl: Duration,
    tools: Vec<GlobPattern>,
    on_hit: CacheHit,
}

impl CachePolicy {
    /// Compiles the configured settings.
    pub fn new(c: &CacheConfig) -> Result<Self, String> {
        let tools = c
            .tools
            .iter()
            .enumerate()
            .map(|(i, p)| GlobPattern::new(p).map_err(|e| format!("cache.tools[{i}]: {e}")))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            ttl: c.ttl,
            tools,
            on_hit: c.on_hit,
        })
    }

    /// Whether the backend tool `name` is listed in `cache.tools`.
    pub fn matches(&self, name: &str) -> bool {
        self.tools.iter().any(|g| g.matches(name))
    }

    /// Entry lifetime.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// What a hit returns.
    pub fn on_hit(&self) -> CacheHit {
        self.on_hit
    }
}

/// Cache key. The arguments are only present as a 128-bit hash.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Key {
    server: String,
    instance: String,
    session: String,
    tool: String,
    args: u128,
}

impl Key {
    /// Builds the key of one call (`arguments` as the backend will see them).
    pub fn new(
        server: &str,
        instance: &str,
        session: &str,
        tool: &str,
        arguments: Option<&Value>,
    ) -> Self {
        Self {
            server: server.to_owned(),
            instance: instance.to_owned(),
            session: session.to_owned(),
            tool: tool.to_owned(),
            args: canonical_hash(arguments.unwrap_or(&Value::Null)),
        }
    }

    /// Session the key belongs to.
    pub fn session(&self) -> &str {
        &self.session
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key").finish_non_exhaustive()
    }
}

/// Hash of `v` that ignores object key order. `null` and a missing value
/// hash alike.
fn canonical_hash(v: &Value) -> u128 {
    fn walk<H: Hasher>(h: &mut H, v: &Value) {
        match v {
            Value::Null => h.write_u8(0),
            Value::Bool(b) => h.write(&[1, u8::from(*b)]),
            Value::Number(n) => {
                h.write_u8(2);
                h.write(n.to_string().as_bytes());
                h.write_u8(0xff);
            }
            Value::String(s) => {
                h.write_u8(3);
                h.write_usize(s.len());
                h.write(s.as_bytes());
            }
            Value::Array(a) => {
                h.write_u8(4);
                h.write_usize(a.len());
                for x in a {
                    walk(h, x);
                }
            }
            Value::Object(m) => {
                h.write_u8(5);
                h.write_usize(m.len());
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                for k in keys {
                    h.write_usize(k.len());
                    h.write(k.as_bytes());
                    walk(h, &m[k]);
                }
            }
        }
    }
    let mut a = std::collections::hash_map::DefaultHasher::new();
    let mut b = std::collections::hash_map::DefaultHasher::new();
    b.write(b"mcpdoze-cache-salt");
    walk(&mut a, v);
    walk(&mut b, v);
    (u128::from(a.finish()) << 64) | u128::from(b.finish())
}

struct Entry {
    result: Arc<Value>,
    bytes: usize,
    expires: Instant,
    tick: u64,
    /// Result-store ref of this result (reference mode), once made.
    reference: Option<String>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<Key, Entry>,
    bytes: usize,
    tick: u64,
    /// Bumped by every invalidation of a server.
    epochs: HashMap<String, u64>,
    /// Sessions that already have an end hook.
    hooked: HashSet<String>,
}

/// A cache hit.
pub struct Hit {
    /// The cached `result` object.
    pub result: Arc<Value>,
    /// The result-store ref made for it earlier (reference mode).
    pub reference: Option<String>,
}

/// What [`ResultCache::store`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Stored {
    /// Not stored (too large, or the server was invalidated meanwhile).
    No,
    /// Stored; `first_for_session` asks the caller to register the purge
    /// hook of the session.
    Yes {
        /// First entry of this session.
        first_for_session: bool,
    },
}

/// The in-memory result cache. Cheap to share (`Arc`).
pub struct ResultCache {
    inner: Mutex<Inner>,
    max_bytes: usize,
}

impl std::fmt::Debug for ResultCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (n, b) = self.usage();
        f.debug_struct("ResultCache")
            .field("entries", &n)
            .field("bytes", &b)
            .finish()
    }
}

impl Default for ResultCache {
    fn default() -> Self {
        Self::with_max_bytes(DEFAULT_MAX_BYTES)
    }
}

impl ResultCache {
    /// A cache holding at most `max_bytes` of results.
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            inner: Mutex::default(),
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

    /// The invalidation epoch of `server`; pass it to [`store`](Self::store).
    pub fn epoch(&self, server: &str) -> u64 {
        self.guard().epochs.get(server).copied().unwrap_or(0)
    }

    /// The live entry of `key`, refreshing its LRU position. An expired
    /// entry is dropped.
    pub fn lookup(&self, key: &Key, now: Instant) -> Option<Hit> {
        let mut g = self.guard();
        g.tick += 1;
        let tick = g.tick;
        let e = g.entries.get_mut(key)?;
        if e.expires <= now {
            Self::remove_locked(&mut g, key);
            return None;
        }
        e.tick = tick;
        Some(Hit {
            result: e.result.clone(),
            reference: e.reference.clone(),
        })
    }

    /// Remembers the result-store ref made for the entry of `key`.
    pub fn set_reference(&self, key: &Key, id: String) {
        if let Some(e) = self.guard().entries.get_mut(key) {
            e.reference = Some(id);
        }
    }

    /// Stores `result` for `key`, living `ttl`. Refused when the server's
    /// epoch is no longer `epoch` (an invalidation happened during the call)
    /// or the result is larger than a quarter of the byte cap.
    pub fn store(
        &self,
        key: Key,
        result: Value,
        epoch: u64,
        ttl: Duration,
        now: Instant,
    ) -> Stored {
        let bytes = usize::try_from(json_len(&result)).unwrap_or(usize::MAX);
        if bytes > self.max_bytes / 4 {
            return Stored::No;
        }
        let mut g = self.guard();
        if g.epochs.get(&key.server).copied().unwrap_or(0) != epoch {
            return Stored::No;
        }
        Self::remove_locked(&mut g, &key);
        let expired: Vec<Key> = g
            .entries
            .iter()
            .filter(|(_, e)| e.expires <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            Self::remove_locked(&mut g, &k);
        }
        while g.bytes + bytes > self.max_bytes {
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
        let session = key.session.clone();
        g.entries.insert(
            key,
            Entry {
                result: Arc::new(result),
                bytes,
                expires: now + ttl,
                tick,
                reference: None,
            },
        );
        Stored::Yes {
            first_for_session: g.hooked.insert(session),
        }
    }

    /// Drops the entry of `key` (its call ended in an error).
    pub fn remove(&self, key: &Key) {
        let mut g = self.guard();
        Self::remove_locked(&mut g, key);
    }

    /// Drops everything of `server` and makes calls that started earlier
    /// unable to store their result.
    pub fn invalidate_server(&self, server: &str) {
        let mut g = self.guard();
        *g.epochs.entry(server.to_owned()).or_insert(0) += 1;
        let keys: Vec<Key> = g
            .entries
            .keys()
            .filter(|k| k.server == server)
            .cloned()
            .collect();
        for k in keys {
            Self::remove_locked(&mut g, &k);
        }
    }

    /// Drops everything of `session` (session end).
    pub fn purge_session(&self, session: &str) {
        let mut g = self.guard();
        g.hooked.remove(session);
        let keys: Vec<Key> = g
            .entries
            .keys()
            .filter(|k| k.session == session)
            .cloned()
            .collect();
        for k in keys {
            Self::remove_locked(&mut g, &k);
        }
    }

    fn remove_locked(g: &mut Inner, key: &Key) {
        if let Some(e) = g.entries.remove(key) {
            g.bytes = g.bytes.saturating_sub(e.bytes);
        }
    }
}

/// Whether a `tools/call` response may be cached: a plain result, not an
/// `isError` one and not a JSON-RPC error.
pub fn cacheable(resp: &Value) -> bool {
    resp.get("error").is_none()
        && resp["result"].is_object()
        && resp["result"]["isError"] != Value::Bool(true)
}

/// The stub returned for a hit in `reference` mode.
pub fn stub(reference: &str, bytes: u64) -> Value {
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": format!(
                "[mcpdoze: cache hit; the content of this result equals an earlier result of this call in this session ({bytes} bytes). Read it with mcpdoze__result_get, ref={reference}]"
            )
        }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(tool: &str, args: &Value, session: &str) -> Key {
        Key::new("s", "", session, tool, Some(args))
    }

    #[test]
    fn hash_ignores_key_order_but_not_values() {
        let a = canonical_hash(&json!({"a":1,"b":[1,2]}));
        assert_eq!(a, canonical_hash(&json!({"b":[1,2],"a":1})));
        assert_ne!(a, canonical_hash(&json!({"a":1,"b":[2,1]})));
        assert_ne!(a, canonical_hash(&json!({"a":2,"b":[1,2]})));
    }

    #[test]
    fn store_lookup_ttl_and_sessions() {
        let c = ResultCache::default();
        let now = Instant::now();
        let k = key("t", &json!({"x":1}), "s1");
        let ep = c.epoch("s");
        let r = c.store(k.clone(), json!({"v":1}), ep, Duration::from_secs(5), now);
        assert_eq!(
            r,
            Stored::Yes {
                first_for_session: true
            }
        );
        assert!(c.lookup(&k, now).is_some());
        assert!(c.lookup(&key("t", &json!({"x":1}), "s2"), now).is_none());
        assert!(c.lookup(&k, now + Duration::from_secs(6)).is_none());
        assert_eq!(c.usage().0, 0);
    }

    #[test]
    fn invalidation_drops_entries_and_refuses_stale_stores() {
        let c = ResultCache::default();
        let now = Instant::now();
        let k = key("t", &json!({}), "s1");
        let ep = c.epoch("s");
        c.store(k.clone(), json!({"v":1}), ep, Duration::from_secs(5), now);
        c.invalidate_server("s");
        assert!(c.lookup(&k, now).is_none());
        // A call that began before the invalidation cannot store.
        assert_eq!(
            c.store(k, json!({"v":1}), ep, Duration::from_secs(5), now),
            Stored::No
        );
    }

    #[test]
    fn byte_cap_evicts_least_recently_used() {
        let c = ResultCache::with_max_bytes(400);
        let now = Instant::now();
        let big = json!({"t":"x".repeat(60)});
        let ttl = Duration::from_secs(5);
        for i in 0..8 {
            c.store(key("t", &json!(i), "s"), big.clone(), 0, ttl, now);
        }
        assert!(c.usage().1 <= 400);
        assert!(c.lookup(&key("t", &json!(7), "s"), now).is_some());
        assert!(c.lookup(&key("t", &json!(0), "s"), now).is_none());
        // Larger than a quarter of the cap: refused.
        let huge = json!({"t":"x".repeat(200)});
        assert_eq!(
            c.store(key("h", &json!(0), "s"), huge, 0, ttl, now),
            Stored::No
        );
    }

    #[test]
    fn errors_are_not_cacheable() {
        assert!(cacheable(&json!({"result":{"content":[]}})));
        assert!(!cacheable(&json!({"result":{"isError":true}})));
        assert!(!cacheable(&json!({"error":{"code":-1}})));
    }

    #[test]
    fn purge_session_drops_only_that_session() {
        let c = ResultCache::default();
        let now = Instant::now();
        let ttl = Duration::from_secs(5);
        c.store(key("t", &json!(1), "a"), json!({}), 0, ttl, now);
        c.store(key("t", &json!(1), "b"), json!({}), 0, ttl, now);
        c.purge_session("a");
        assert_eq!(c.usage().0, 1);
    }
}
