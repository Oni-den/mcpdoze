// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-server catalogs, so `tools/list`, `prompts/list`, `resources/list`
//! and `resources/templates/list` can be answered without spawning backends.
//!
//! A [`CatalogEntry`] holds what a backend lists (backend names, not
//! prefixed), its `initialize` capabilities and `serverInfo`, and a
//! [`fingerprint`] of its launch spec. The router discards an entry whose
//! fingerprint no longer matches the spec.
//!
//! [`DiskCatalog`] keeps one JSON file per server at
//! `<dir>/<server>.json`, written atomically (temporary file plus rename).
//! Missing, unreadable, corrupt, oversized or wrong-version files count as
//! "no catalog": the server is probed again and the file rewritten.
//!
//! The fingerprint covers transport, command, arguments, working directory,
//! URL, environment variable NAMES and header NAMES. Values of environment
//! variables and headers are never hashed, stored or logged.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::stdio::lock;
use crate::backend::BackendSpec;

/// On-disk format version; files with another version are ignored.
pub const CATALOG_VERSION: u32 = 1;
/// Largest catalog file read from disk.
pub const MAX_CATALOG_FILE: u64 = 32 * 1024 * 1024;

/// Everything the broker remembers about one backend's lists.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Format version ([`CATALOG_VERSION`]).
    #[serde(default)]
    pub version: u32,
    /// [`fingerprint`] of the launch spec the lists came from; empty for
    /// snapshots stored without one (accepted for any spec).
    #[serde(default)]
    pub fingerprint: String,
    /// When the lists were taken, in seconds since the Unix epoch.
    #[serde(default)]
    pub updated_at: u64,
    /// The backend's `serverInfo`.
    #[serde(default)]
    pub server_info: Value,
    /// The backend's `capabilities`.
    #[serde(default)]
    pub capabilities: Value,
    /// `tools/list` result items.
    #[serde(default)]
    pub tools: Vec<Value>,
    /// `prompts/list` result items.
    #[serde(default)]
    pub prompts: Vec<Value>,
    /// `resources/list` result items.
    #[serde(default)]
    pub resources: Vec<Value>,
    /// `resources/templates/list` result items.
    #[serde(default)]
    pub resource_templates: Vec<Value>,
}

/// Which lists differ between two entries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListChanges {
    /// Tools differ.
    pub tools: bool,
    /// Prompts differ.
    pub prompts: bool,
    /// Resources or resource templates differ.
    pub resources: bool,
}

impl ListChanges {
    /// True if any list differs.
    pub fn any(&self) -> bool {
        self.tools || self.prompts || self.resources
    }
}

impl CatalogEntry {
    /// The lists of `new` compared with `self`.
    pub fn diff(&self, new: &CatalogEntry) -> ListChanges {
        ListChanges {
            tools: self.tools != new.tools,
            prompts: self.prompts != new.prompts,
            resources: self.resources != new.resources
                || self.resource_templates != new.resource_templates,
        }
    }

    /// True if a tool named `name` (backend name) is listed.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t["name"] == name)
    }
}

/// Storage for per-server catalog entries.
pub trait Catalog: Send + Sync + 'static {
    /// The entry for `server`, if a valid one exists.
    fn entry(&self, server: &str) -> Option<Arc<CatalogEntry>>;
    /// Stores the entry for `server`.
    fn put(&self, server: &str, entry: CatalogEntry);
    /// Forgets the entry for `server` (server removed or changed).
    fn remove(&self, server: &str) {
        let _ = server;
    }

    /// The tool snapshot for `server`, if one exists.
    fn get(&self, server: &str) -> Option<Arc<Vec<Value>>> {
        self.entry(server).map(|e| Arc::new(e.tools.clone()))
    }

    /// Replaces the tool snapshot for `server`, keeping the rest of its
    /// entry (a new entry has an empty fingerprint).
    fn set(&self, server: &str, tools: Vec<Value>) {
        let mut e = self.entry(server).map(|e| (*e).clone()).unwrap_or_default();
        e.version = CATALOG_VERSION;
        e.tools = tools;
        self.put(server, e);
    }
}

/// In-memory [`Catalog`].
#[derive(Debug, Default)]
pub struct MemoryCatalog {
    entries: Mutex<HashMap<String, Arc<CatalogEntry>>>,
}

impl Catalog for MemoryCatalog {
    fn entry(&self, server: &str) -> Option<Arc<CatalogEntry>> {
        lock(&self.entries).get(server).cloned()
    }

    fn put(&self, server: &str, entry: CatalogEntry) {
        lock(&self.entries).insert(server.to_owned(), Arc::new(entry));
    }

    fn remove(&self, server: &str) {
        lock(&self.entries).remove(server);
    }
}

/// [`Catalog`] backed by one JSON file per server, with an in-memory cache.
#[derive(Debug)]
pub struct DiskCatalog {
    dir: PathBuf,
    /// `None` caches "no valid file".
    cache: Mutex<HashMap<String, Option<Arc<CatalogEntry>>>>,
}

impl DiskCatalog {
    /// A catalog in `dir` (created on first write).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The catalog directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file of `server`.
    pub fn path_of(&self, server: &str) -> PathBuf {
        self.dir.join(format!("{}.json", file_stem(server)))
    }

    fn load(&self, server: &str) -> Option<Arc<CatalogEntry>> {
        let path = self.path_of(server);
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(server, error = %e, "cannot read catalog file; ignoring it");
                return None;
            }
        };
        let mut text = Vec::new();
        if let Err(e) = file.take(MAX_CATALOG_FILE + 1).read_to_end(&mut text) {
            tracing::warn!(server, error = %e, "cannot read catalog file; ignoring it");
            return None;
        }
        if text.len() as u64 > MAX_CATALOG_FILE {
            tracing::warn!(server, "catalog file too large; ignoring it");
            return None;
        }
        match serde_json::from_slice::<CatalogEntry>(&text) {
            Ok(e) if e.version == CATALOG_VERSION => Some(Arc::new(e)),
            Ok(e) => {
                tracing::info!(
                    server,
                    version = e.version,
                    "catalog file has another version; ignoring it"
                );
                None
            }
            Err(_) => {
                tracing::warn!(server, "catalog file is corrupt; ignoring it");
                None
            }
        }
    }
}

impl Catalog for DiskCatalog {
    fn entry(&self, server: &str) -> Option<Arc<CatalogEntry>> {
        if let Some(e) = lock(&self.cache).get(server) {
            return e.clone();
        }
        let loaded = self.load(server);
        lock(&self.cache).insert(server.to_owned(), loaded.clone());
        loaded
    }

    fn put(&self, server: &str, mut entry: CatalogEntry) {
        entry.version = CATALOG_VERSION;
        let path = self.path_of(server);
        match serde_json::to_string_pretty(&entry) {
            Ok(text) => {
                if let Err(e) = crate::admin::persist::write_atomic(&path, &text) {
                    tracing::warn!(server, error = %e, "cannot write catalog file");
                }
            }
            Err(e) => tracing::warn!(server, error = %e, "cannot serialize catalog"),
        }
        lock(&self.cache).insert(server.to_owned(), Some(Arc::new(entry)));
    }

    fn remove(&self, server: &str) {
        lock(&self.cache).insert(server.to_owned(), None);
        match std::fs::remove_file(self.path_of(server)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(server, error = %e, "cannot remove catalog file"),
        }
    }
}

/// A file name for `server`: the name itself when it only uses
/// `[A-Za-z0-9_.-]` (not starting with `.`), else `_` plus its hex bytes.
fn file_stem(server: &str) -> String {
    let plain = !server.is_empty()
        && server.len() <= 64
        && !server.starts_with('.')
        && server
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if plain {
        return server.to_owned();
    }
    let mut out = String::from("_");
    for b in server.bytes().take(100) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn fnv1a64(seed: u64, data: &[u8]) -> u64 {
    let mut h = seed;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Stable hash of what determines a backend's lists: transport, command,
/// arguments, working directory, URL, environment variable names and header
/// names. Never values. Format: `v1-` plus 32 hex digits.
pub fn fingerprint(spec: &BackendSpec) -> String {
    let mut canon = String::new();
    let mut field = |k: &str, v: &str| {
        // Length-prefixed, so field boundaries are unambiguous.
        canon.push_str(&format!("{k}:{}:{v};", v.len()));
    };
    match &spec.http {
        Some(h) => {
            field("transport", "http");
            field("url", &h.url);
            for name in h.headers.keys() {
                field("header", &name.to_ascii_lowercase());
            }
        }
        None => {
            field("transport", "stdio");
            field("command", &spec.command);
            for a in &spec.args {
                field("arg", a);
            }
            if let Some(cwd) = &spec.cwd {
                field("cwd", &cwd.to_string_lossy());
            }
            for name in spec.env.keys() {
                field("env", name);
            }
        }
    }
    let a = fnv1a64(0xcbf2_9ce4_8422_2325, canon.as_bytes());
    let b = fnv1a64(0x6c62_272e_07bb_0142, canon.as_bytes());
    format!("v1-{a:016x}{b:016x}")
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_then_get() {
        let c = MemoryCatalog::default();
        assert!(c.get("a").is_none());
        c.set("a", vec![json!({"name": "t"})]);
        assert_eq!(c.get("a").unwrap()[0]["name"], "t");
        assert!(c.entry("a").unwrap().has_tool("t"));
    }

    #[test]
    fn fingerprint_uses_names_not_values() {
        let mut a = BackendSpec::new("prog", ["x"]);
        a.env.insert("EXAMPLE_KEY".into(), "value-one".into());
        let mut b = a.clone();
        b.env.insert("EXAMPLE_KEY".into(), "value-two".into());
        assert_eq!(fingerprint(&a), fingerprint(&b));
        b.env.insert("OTHER".into(), "v".into());
        assert_ne!(fingerprint(&a), fingerprint(&b));
        let c = BackendSpec::new("prog", ["y"]);
        assert_ne!(fingerprint(&a), fingerprint(&c));
        // Field boundaries: ["ab"] differs from ["a", "b"].
        assert_ne!(
            fingerprint(&BackendSpec::new("p", ["ab"])),
            fingerprint(&BackendSpec::new("p", ["a", "b"]))
        );
        let h1 = BackendSpec::http("https://example.com/mcp").header("X-Key", "one");
        let h2 = BackendSpec::http("https://example.com/mcp").header("X-Key", "two");
        assert_eq!(fingerprint(&h1), fingerprint(&h2));
        assert!(fingerprint(&h1).starts_with("v1-"));
    }

    #[test]
    fn disk_round_trip_and_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let c = DiskCatalog::new(dir.path().join("catalog"));
        assert!(c.entry("srv").is_none());
        let e = CatalogEntry {
            fingerprint: "v1-x".into(),
            tools: vec![json!({"name": "t"})],
            ..CatalogEntry::default()
        };
        c.put("srv", e.clone());
        let fresh = DiskCatalog::new(dir.path().join("catalog"));
        let got = fresh.entry("srv").unwrap();
        assert_eq!(got.tools, e.tools);
        assert_eq!(got.version, CATALOG_VERSION);
        // Corrupt and wrong-version files are ignored.
        std::fs::write(c.path_of("srv"), b"{not json").unwrap();
        assert!(DiskCatalog::new(c.dir()).entry("srv").is_none());
        std::fs::write(c.path_of("srv"), br#"{"version": 99, "tools": []}"#).unwrap();
        assert!(DiskCatalog::new(c.dir()).entry("srv").is_none());
        c.remove("srv");
        assert!(!c.path_of("srv").exists());
        assert!(c.entry("srv").is_none());
    }

    #[test]
    fn odd_names_get_safe_files() {
        assert_eq!(file_stem("ex-1.v2_x"), "ex-1.v2_x");
        assert_eq!(file_stem("../x"), "_2e2e2f78");
        assert_eq!(file_stem("a b"), "_612062");
    }
}
