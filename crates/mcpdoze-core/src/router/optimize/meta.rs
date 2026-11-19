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
