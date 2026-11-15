// SPDX-License-Identifier: GPL-3.0-or-later

//! Tool snapshots per server, so `tools/list` can be answered without
//! spawning backends. The on-disk catalog (fingerprints, refresh,
//! `list_changed`) is milestone M4; it implements the same trait.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::backend::stdio::lock;

/// Storage for per-server tool snapshots (backend names, not prefixed).
pub trait Catalog: Send + Sync + 'static {
    /// The snapshot for `server`, if one exists.
    fn get(&self, server: &str) -> Option<Arc<Vec<Value>>>;
    /// Replaces the snapshot for `server`.
    fn set(&self, server: &str, tools: Vec<Value>);
    /// Forgets the snapshot for `server` (server removed or changed).
    fn remove(&self, server: &str) {
        let _ = server;
    }
}

/// In-memory [`Catalog`].
#[derive(Debug, Default)]
pub struct MemoryCatalog {
    tools: Mutex<HashMap<String, Arc<Vec<Value>>>>,
}

impl Catalog for MemoryCatalog {
    fn get(&self, server: &str) -> Option<Arc<Vec<Value>>> {
        lock(&self.tools).get(server).cloned()
    }

    fn set(&self, server: &str, tools: Vec<Value>) {
        lock(&self.tools).insert(server.to_owned(), Arc::new(tools));
    }

    fn remove(&self, server: &str) {
        lock(&self.tools).remove(server);
    }
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
    }
}
