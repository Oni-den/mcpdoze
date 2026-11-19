// SPDX-License-Identifier: GPL-3.0-or-later

//! Token optimisation (`docs/OPTIMIZE.md`): the per-server policy compiled
//! from `[servers.<name>.optimize]` / `[servers.<name>.tools.<tool>]`.
//!
//! The router holds one [`ServerPolicy`] per server and swaps it on reload
//! without touching backends. Everything here is a view over the raw catalog
//! and over results: the disk catalog keeps what the backend said, so a
//! catalog refresh re-derives every list transform. A server without an
//! `optimize` table gets no policy and behaves as before.
//!
//! Global rules for result transforms (see [`result`]): never touch an
//! `isError` result or a JSON-RPC error, never a non-text block; when the
//! transformed result is not smaller the original is sent.

pub mod meta;
pub mod result;
pub mod store;
pub mod view;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::config::{ServerConfig, ToolFilter};

/// Tag recorded when `default_args` added an argument.
pub const TAG_ARGS: &str = "args";

/// Compiled settings of one server.
#[derive(Debug, Default)]
pub struct ServerPolicy {
    filter: ToolFilter,
    description_max_chars: Option<usize>,
    strip_schema_descriptions: bool,
    minify_json: bool,
    max_result_bytes: u64,
    filters: HashMap<String, result::FieldFilter>,
    default_args: HashMap<String, Map<String, Value>>,
    /// `tool/key` pairs already warned about (one warning each).
    warned: Mutex<HashSet<String>>,
}

impl ServerPolicy {
    /// Compiles the settings of `server`. `None` when nothing is configured
    /// (the router then keeps no policy, i.e. the old behaviour).
    pub fn compile(server: &ServerConfig) -> Result<Option<Self>, String> {
        let default_args: HashMap<String, Map<String, Value>> = server
            .tools
            .iter()
            .filter(|(_, t)| !t.default_args.is_empty())
            .map(|(n, t)| (n.clone(), t.default_args.clone()))
            .collect();
        let mut filters = HashMap::new();
        for (n, t) in &server.tools {
            if let Some(f) = result::FieldFilter::new(&t.keep, &t.drop)? {
                filters.insert(n.clone(), f);
            }
        }
        let Some(o) = &server.optimize else {
            if default_args.is_empty() && filters.is_empty() {
                return Ok(None);
            }
            return Ok(Some(Self {
                default_args,
                filters,
                ..Self::default()
            }));
        };
        Ok(Some(Self {
            filter: o.tool_filter()?,
            description_max_chars: o.description_max_chars.map(|n| n as usize),
            strip_schema_descriptions: o.strip_schema_descriptions,
            minify_json: o.minify_json,
            max_result_bytes: o.max_result_bytes,
            filters,
            default_args,
            warned: Mutex::default(),
        }))
    }

    /// Whether the backend tool is listed and callable.
    pub fn allows(&self, tool: &str) -> bool {
        self.filter.allows(tool)
    }

    /// Whether any result transform is on.
    pub fn transforms_results(&self) -> bool {
        self.minify_json || self.max_result_bytes > 0 || !self.filters.is_empty()
    }

    /// Whether results may be stored for `mcpdoze__result_get` (truncation;
    /// the result cache will add itself here).
    pub fn stores_results(&self) -> bool {
        self.max_result_bytes > 0
    }

    /// Whether `tool` has a field filter.
    pub fn filters_tool(&self, tool: &str) -> bool {
        self.filters.contains_key(tool)
    }

    /// Merges the configured `default_args` of `tool` into `params`
    /// (`tools/call` params), only for keys the agent omitted. A key the
    /// catalog `schema` does not know is skipped with one warning. Returns
    /// whether anything was added.
    pub fn apply_default_args(
        &self,
        server: &str,
        tool: &str,
        params: &mut Value,
        schema: Option<&Value>,
    ) -> bool {
        let Some(defaults) = self.default_args.get(tool) else {
            return false;
        };
        if !params["arguments"].is_object() && !params["arguments"].is_null() {
            return false; // malformed arguments: leave them to the backend
        }
        let mut added = false;
        for (k, v) in defaults {
            if !key_known(schema, k) {
                let id = format!("{tool}/{k}");
                let first = self
                    .warned
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id);
                if first {
                    tracing::warn!(
                        server,
                        tool,
                        key = %k,
                        "default_args key is not in the tool's inputSchema; skipped"
                    );
                }
                continue;
            }
            if params["arguments"].is_null() {
                params["arguments"] = Value::Object(Map::new());
            }
            if let Some(args) = params["arguments"].as_object_mut() {
                if !args.contains_key(k) {
                    args.insert(k.clone(), v.clone());
                    added = true;
                }
            }
        }
        added
    }
}

/// Whether `key` may be an argument of a tool with `schema`. An unknown
/// schema or one without a `properties` map accepts anything.
fn key_known(schema: Option<&Value>, key: &str) -> bool {
    let Some(s) = schema else { return true };
    let Some(props) = s["properties"].as_object() else {
        return true;
    };
    if props.contains_key(key) {
        return true;
    }
    match &s["additionalProperties"] {
        Value::Bool(b) => *b,
        Value::Object(_) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_keys() {
        let closed = json!({"properties":{"a":{}}});
        assert!(key_known(Some(&closed), "a"));
        assert!(!key_known(Some(&closed), "b"));
        assert!(key_known(Some(&json!({"type":"object"})), "b"));
        assert!(key_known(
            Some(&json!({"properties":{},"additionalProperties":true})),
            "b"
        ));
        assert!(key_known(None, "b"));
    }
}
