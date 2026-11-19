// SPDX-License-Identifier: GPL-3.0-or-later

//! Meta-tool framework: tools the broker itself serves under the reserved
//! prefix [`PREFIX`] (`mcpdoze__<name>`). They are intercepted in
//! `Router::call_tool` before server routing and listed only while their
//! feature is on. Nothing is registered by default; later features add
//! implementations of [`MetaTool`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::router::Session;

/// Reserved server prefix of meta-tools.
pub const PREFIX: &str = "mcpdoze";

/// Boxed future returned by [`MetaTool::call`].
pub type MetaFuture<'a> = Pin<Box<dyn Future<Output = Value> + Send + 'a>>;

/// A broker-served tool.
pub trait MetaTool: Send + Sync {
    /// Bare name (the agent sees `mcpdoze__<name>`).
    fn name(&self) -> &str;
    /// The `tools/list` entry; the name is replaced by the prefixed one.
    fn definition(&self) -> Value;
    /// Whether the feature enabling it is on (listed and callable).
    fn enabled(&self) -> bool;
    /// Runs the tool; returns the `result` object of `tools/call`.
    fn call<'a>(&'a self, session: &'a Arc<Session>, arguments: Value) -> MetaFuture<'a>;
}

/// The registered meta-tools.
#[derive(Default)]
pub struct MetaRegistry {
    tools: Vec<Arc<dyn MetaTool>>,
}

impl std::fmt::Debug for MetaRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetaRegistry")
            .field("tools", &self.tools.len())
            .finish()
    }
}

impl MetaRegistry {
    /// Adds a meta-tool, replacing one of the same name.
    pub fn register(&mut self, tool: Arc<dyn MetaTool>) {
        self.tools.retain(|t| t.name() != tool.name());
        self.tools.push(tool);
    }

    /// Definitions (prefixed names) of the enabled tools.
    pub fn listed(&self) -> Vec<Value> {
        self.tools
            .iter()
            .filter(|t| t.enabled())
            .map(|t| {
                let mut d = t.definition();
                d["name"] = Value::String(format!("{PREFIX}__{}", t.name()));
                d
            })
            .collect()
    }

    /// The enabled tool called `name` (bare).
    pub fn find(&self, name: &str) -> Option<Arc<dyn MetaTool>> {
        self.tools
            .iter()
            .find(|t| t.name() == name && t.enabled())
            .cloned()
    }
}

/// The bare meta-tool name when `agent_name` is `mcpdoze__<name>`.
pub fn bare(agent_name: &str) -> Option<&str> {
    agent_name
        .strip_prefix(PREFIX)?
        .strip_prefix(crate::router::naming::SEP)
        .filter(|n| !n.is_empty())
}

/// `mcpdoze__result_get`: pages through a result the broker truncated.
/// Listed while any server has `max_result_bytes` (or, later, the cache) on.
pub struct ResultGet {
    router: std::sync::Weak<crate::router::Router>,
}

/// Largest page one call returns.
const MAX_PAGE: usize = 1 << 20;

impl ResultGet {
    /// A tool reading the store of `router`.
    pub fn new(router: std::sync::Weak<crate::router::Router>) -> Self {
        Self { router }
    }
}

fn tool_error(msg: impl Into<String>) -> Value {
    serde_json::json!({"content":[{"type":"text","text":msg.into()}],"isError":true})
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// One page of `text`: `(start, end)` byte offsets, both on character
/// boundaries, `end - start` about `limit` (at least one character).
pub fn page_bounds(text: &str, offset: usize, limit: usize) -> (usize, usize) {
    let start = floor_boundary(text, offset);
    let mut end = floor_boundary(text, start.saturating_add(limit.max(1)));
    if end <= start && start < text.len() {
        end = start + 1;
        while !text.is_char_boundary(end) {
            end += 1;
        }
    }
    (start, end)
}

impl MetaTool for ResultGet {
    fn name(&self) -> &str {
        "result_get"
    }

    fn definition(&self) -> Value {
        serde_json::json!({
            "description": "Read a result the broker truncated (its text ends with a mcpdoze marker naming a ref). Returns a slice of the full text by byte offset and limit; pass the returned next_offset to continue. With pointer (JSON Pointer such as /structuredContent/items/3) only that part is paged.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "description": "The ref from the truncation marker."},
                    "pointer": {"type": "string", "description": "Optional JSON Pointer into the stored result."},
                    "offset": {"type": "integer", "minimum": 0, "description": "Start byte (default 0)."},
                    "limit": {"type": "integer", "minimum": 1, "description": "Bytes to return (default: the server's max_result_bytes)."}
                },
                "required": ["ref"]
            },
            "annotations": {"readOnlyHint": true, "idempotentHint": true}
        })
    }

    fn enabled(&self) -> bool {
        self.router
            .upgrade()
            .is_some_and(|r| r.result_store_enabled())
    }

    fn call<'a>(&'a self, session: &'a Arc<Session>, arguments: Value) -> MetaFuture<'a> {
        Box::pin(async move {
            let Some(router) = self.router.upgrade() else {
                return tool_error("broker is shutting down");
            };
            if session.is_ephemeral() {
                return tool_error(
                    "result refs are not available for stateless requests; call the original tool again",
                );
            }
            let Some(id) = arguments["ref"].as_str() else {
                return tool_error("result_get needs a string argument `ref`");
            };
            let num = |k: &str| match &arguments[k] {
                Value::Null => Ok(None),
                v => v
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .map(Some)
                    .ok_or(format!("`{k}` must be a non-negative integer")),
            };
            let (offset, limit) = match (num("offset"), num("limit")) {
                (Ok(o), Ok(l)) => (o.unwrap_or(0), l),
                (Err(e), _) | (_, Err(e)) => return tool_error(e),
            };
            let Some(stored) = router.result_store().get(session.id(), id) else {
                return tool_error(crate::router::optimize::store::LOST_REF);
            };
            let text = match arguments["pointer"].as_str() {
                None => super::result::text_view(&stored.result),
                Some(p) => match stored.result.pointer(p) {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => return tool_error(format!("pointer {p} not found in the result")),
                },
            };
            let limit = limit.unwrap_or(stored.page).clamp(1, MAX_PAGE);
            let (start, end) = page_bounds(&text, offset, limit);
            let total = text.len();
            let more = end < total;
            let note = if more {
                format!("[mcpdoze: bytes {start}-{end} of {total}; next offset={end}]")
            } else {
                format!("[mcpdoze: bytes {start}-{end} of {total}; end]")
            };
            serde_json::json!({
                "content": [
                    {"type":"text","text": &text[start..end]},
                    {"type":"text","text": note},
                ],
                "structuredContent": {
                    "offset": start,
                    "end": end,
                    "total": total,
                    "next_offset": if more { Value::from(end) } else { Value::Null },
                },
                "isError": false
            })
        })
    }
}

/// `mcpdoze__batch`: several tool calls in one request. Listed while any
/// server has `batch` other than `off`; see `router::batch`.
pub struct BatchTool {
    router: std::sync::Weak<crate::router::Router>,
}

impl BatchTool {
    /// A tool batching calls through `router`.
    pub fn new(router: std::sync::Weak<crate::router::Router>) -> Self {
        Self { router }
    }
}

impl MetaTool for BatchTool {
    fn name(&self) -> &str {
        "batch"
    }

    fn definition(&self) -> Value {
        serde_json::json!({
            "description": "Run several tool calls in one request (at most 16, four at a time). Each call is {name, arguments} with the tool's full listed name. Returns one text block per call, in order, starting with its index, name and isError flag. Servers decide which tools may be batched; refused calls report an error and the others still run. mcpdoze__ tools cannot be batched.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "calls": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": crate::router::batch::MAX_CALLS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "arguments": {"type": "object"}
                            },
                            "required": ["name"]
                        }
                    }
                },
                "required": ["calls"]
            }
        })
    }

    fn enabled(&self) -> bool {
        self.router.upgrade().is_some_and(|r| r.batch_enabled())
    }

    fn call<'a>(&'a self, session: &'a Arc<Session>, arguments: Value) -> MetaFuture<'a> {
        Box::pin(async move {
            let Some(router) = self.router.upgrade() else {
                return tool_error("broker is shutting down");
            };
            crate::router::batch::run(router, session, arguments).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_cover_text_exactly() {
        let t = "aé€😀b".repeat(50);
        let mut out = String::new();
        let mut at = 0;
        while at < t.len() {
            let (s, e) = page_bounds(&t, at, 7);
            assert!(e > s);
            out.push_str(&t[s..e]);
            at = e;
        }
        assert_eq!(out, t);
        assert_eq!(page_bounds(&t, t.len() + 5, 3), (t.len(), t.len()));
    }
}
