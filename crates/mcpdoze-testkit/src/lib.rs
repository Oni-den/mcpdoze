// SPDX-License-Identifier: GPL-3.0-or-later
//! Test support for MCPDoze: a scriptable mock MCP stdio server
//! (`mock-mcp-stdio`) and helpers to launch it and speak JSON-RPC lines.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Protocol version the mock advertises by default.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Build a JSON-RPC request line (without trailing newline).
pub fn request(id: impl Into<Value>, method: &str, params: Value) -> String {
    json!({"jsonrpc":"2.0","id":id.into(),"method":method,"params":params}).to_string()
}

/// Build a JSON-RPC notification line (without trailing newline).
pub fn notification(method: &str, params: Value) -> String {
    json!({"jsonrpc":"2.0","method":method,"params":params}).to_string()
}

/// Build a JSON-RPC success response line.
pub fn response(id: impl Into<Value>, result: Value) -> String {
    json!({"jsonrpc":"2.0","id":id.into(),"result":result}).to_string()
}

/// Parse one line as JSON; `None` if it is not valid JSON.
pub fn parse_line(line: &str) -> Option<Value> {
    serde_json::from_str(line.trim()).ok()
}

/// Builder describing how to launch the mock server and how it behaves.
#[derive(Debug, Clone, Default)]
pub struct MockServerSpec {
    program: Option<PathBuf>,
    name: Option<String>,
    tools: Option<Vec<String>>,
    delay_ms: Option<u64>,
    crash_after: Option<u64>,
    garbage_on_start: bool,
    list_changed_after: Option<u64>,
    sampling_request: bool,
    stderr_noise: bool,
    startup_delay_ms: Option<u64>,
}

impl MockServerSpec {
    pub fn new() -> Self {
        Self::default()
    }
    /// Use an explicit binary path (e.g. `env!("CARGO_BIN_EXE_mock-mcp-stdio")`).
    pub fn program(mut self, p: impl Into<PathBuf>) -> Self {
        self.program = Some(p.into());
        self
    }
    pub fn name(mut self, n: &str) -> Self {
        self.name = Some(n.into());
        self
    }
    pub fn tools(mut self, t: &[&str]) -> Self {
        self.tools = Some(t.iter().map(|s| s.to_string()).collect());
        self
    }
    pub fn delay_ms(mut self, ms: u64) -> Self {
        self.delay_ms = Some(ms);
        self
    }
    /// Exit with code 3 (no reply) on call number `n + 1`.
    pub fn crash_after(mut self, n: u64) -> Self {
        self.crash_after = Some(n);
        self
    }
    pub fn garbage_on_start(mut self) -> Self {
        self.garbage_on_start = true;
        self
    }
    /// After `n` calls, emit `notifications/tools/list_changed` and add tool `extra`.
    pub fn list_changed_after(mut self, n: u64) -> Self {
        self.list_changed_after = Some(n);
        self
    }
    pub fn sampling_request(mut self) -> Self {
        self.sampling_request = true;
        self
    }
    pub fn stderr_noise(mut self) -> Self {
        self.stderr_noise = true;
        self
    }
    pub fn startup_delay_ms(mut self, ms: u64) -> Self {
        self.startup_delay_ms = Some(ms);
        self
    }

    /// Path of the mock binary: explicit, `MCPDOZE_MOCK_MCP_STDIO`, or a
    /// sibling of the current executable (works from cargo test binaries).
    pub fn program_path(&self) -> PathBuf {
        if let Some(p) = &self.program {
            return p.clone();
        }
        if let Some(p) = std::env::var_os("MCPDOZE_MOCK_MCP_STDIO") {
            return p.into();
        }
        let exe = format!("mock-mcp-stdio{}", std::env::consts::EXE_SUFFIX);
        let mut dir = std::env::current_exe().unwrap_or_default();
        dir.pop();
        if dir.ends_with("deps") {
            dir.pop();
        }
        dir.join(exe)
    }

    /// Command-line arguments for the configured behaviours.
    pub fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        let mut kv = |k: &str, v: String| {
            a.push(k.to_string());
            a.push(v);
        };
        if let Some(v) = &self.name {
            kv("--name", v.clone());
        }
        if let Some(v) = &self.tools {
            kv("--tools", v.join(","));
        }
        if let Some(v) = self.delay_ms {
            kv("--delay-ms", v.to_string());
        }
        if let Some(v) = self.crash_after {
            kv("--crash-after", v.to_string());
        }
        if let Some(v) = self.list_changed_after {
            kv("--list-changed-after", v.to_string());
        }
        if let Some(v) = self.startup_delay_ms {
            kv("--startup-delay-ms", v.to_string());
        }
        for (on, flag) in [
            (self.garbage_on_start, "--garbage-on-start"),
            (self.sampling_request, "--sampling-request"),
            (self.stderr_noise, "--stderr-noise"),
        ] {
            if on {
                a.push(flag.to_string());
            }
        }
        a
    }

    /// A `std::process::Command` with piped stdio, ready to spawn.
    pub fn command(&self) -> Command {
        let mut c = Command::new(self.program_path());
        c.args(self.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        c
    }
}
