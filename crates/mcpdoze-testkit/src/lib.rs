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
    elicitation_request: bool,
    roots_request: bool,
    stderr_noise: bool,
    startup_delay_ms: Option<u64>,
    prompts: Option<Vec<String>>,
    resources: Option<Vec<String>>,
    resource_templates: Option<Vec<String>>,
    log_messages: bool,
    echo_env: Vec<String>,
    subscribe: bool,
    exit_code: Option<i32>,
    exit_after: Option<u64>,
    oversize_response: bool,
    string_ids: bool,
    content_length_framing: bool,
    init_error: bool,
    init_version: Option<String>,
    pretty_json: bool,
    stderr_leak: bool,
    script: Option<PathBuf>,
    big_items: Option<u64>,
    structured: bool,
    read_only: Option<Vec<String>>,
    verbose_descriptions: bool,
}

impl MockServerSpec {
    /// Tool `exit` exits the process with `code` without replying. With
    /// `exit_after`, every call from that index on exits instead.
    pub fn exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }
    /// Exit (code from `exit_code`, default 3) on call index `n` (0-based)
    /// and later, like `crash_after`.
    pub fn exit_after(mut self, n: u64) -> Self {
        self.exit_after = Some(n);
        self
    }
    /// Every tool reply is one line of more than 16 MiB.
    pub fn oversize_response(mut self) -> Self {
        self.oversize_response = true;
        self
    }
    /// Response ids are JSON strings (`"1"` for request id `1`).
    pub fn string_ids(mut self) -> Self {
        self.string_ids = true;
        self
    }
    /// Frame output with LSP-style `Content-Length` headers.
    pub fn content_length_framing(mut self) -> Self {
        self.content_length_framing = true;
        self
    }
    /// `initialize` replies with a JSON-RPC error.
    pub fn init_error(mut self) -> Self {
        self.init_error = true;
        self
    }
    /// `initialize` always reports protocol version `v`.
    pub fn init_version(mut self, v: &str) -> Self {
        self.init_version = Some(v.into());
        self
    }
    /// Output multi-line (pretty-printed) JSON.
    pub fn pretty_json(mut self) -> Self {
        self.pretty_json = true;
        self
    }
    /// Print `MCPDOZE_TEST_*` env values and each call's arguments to stderr.
    pub fn stderr_leak(mut self) -> Self {
        self.stderr_leak = true;
        self
    }
    /// Run the scripted steps in the JSON file at `path` (see the
    /// `mock-mcp-stdio` binary for the format).
    pub fn script(mut self, path: impl Into<PathBuf>) -> Self {
        self.script = Some(path.into());
        self
    }
    /// Tool `big` returns a pretty-printed JSON array of `n` uniform objects.
    pub fn big_items(mut self, n: u64) -> Self {
        self.big_items = Some(n);
        self
    }
    /// Tool `structured` has an output schema and `structuredContent`
    /// (`{"tool":"structured","value":42}`).
    pub fn structured(mut self) -> Self {
        self.structured = true;
        self
    }
    /// Set `annotations.readOnlyHint` to true for the named tools and false
    /// for the others.
    pub fn read_only(mut self, tools: &[&str]) -> Self {
        self.read_only = Some(tools.iter().map(|s| s.to_string()).collect());
        self
    }
    /// Long tool descriptions (over 500 characters).
    pub fn verbose_descriptions(mut self) -> Self {
        self.verbose_descriptions = true;
        self
    }

    /// Prompt names offered (default `greet`); `prompts/get` echoes the
    /// arguments.
    pub fn prompts(mut self, p: &[&str]) -> Self {
        self.prompts = Some(p.iter().map(|s| s.to_string()).collect());
        self
    }
    /// Resource URIs offered (default `mock://readme`); `resources/read`
    /// returns `contents of <uri>`.
    pub fn resources(mut self, r: &[&str]) -> Self {
        self.resources = Some(r.iter().map(|s| s.to_string()).collect());
        self
    }
    /// Resource templates offered (default none). `resources/read` accepts
    /// any URI starting with a template's text before its first `{`.
    pub fn resource_templates(mut self, t: &[&str]) -> Self {
        self.resource_templates = Some(t.iter().map(|s| s.to_string()).collect());
        self
    }
    /// Also report variable `name` (when set) in the `info` tool's `env`.
    pub fn echo_env(mut self, name: &str) -> Self {
        self.echo_env.push(name.to_string());
        self
    }
    /// Advertise `resources.subscribe` and serve `resources/subscribe` /
    /// `unsubscribe`. Tool `touch` (argument `uri`) emits
    /// `notifications/resources/updated`; tool `sub_stats` reports the
    /// counters and subscribed URIs.
    pub fn subscribe(mut self) -> Self {
        self.subscribe = true;
        self
    }
    /// Emit a `notifications/message` log notification at each tool call.
    pub fn log_messages(mut self) -> Self {
        self.log_messages = true;
        self
    }
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
    /// Tool `ask` sends `sampling/createMessage` and returns the reply.
    pub fn sampling_request(mut self) -> Self {
        self.sampling_request = true;
        self
    }
    /// Tool `elicit` sends `elicitation/create` and returns the reply.
    pub fn elicitation_request(mut self) -> Self {
        self.elicitation_request = true;
        self
    }
    /// Tool `roots` sends `roots/list` and returns the reply.
    pub fn roots_request(mut self) -> Self {
        self.roots_request = true;
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
        if let Some(v) = &self.prompts {
            kv("--prompts", v.join(","));
        }
        if let Some(v) = &self.resources {
            kv("--resources", v.join(","));
        }
        if let Some(v) = &self.resource_templates {
            kv("--resource-templates", v.join(","));
        }
        for n in &self.echo_env {
            kv("--echo-env", n.clone());
        }
        if let Some(v) = self.exit_code {
            kv("--exit-code", v.to_string());
        }
        if let Some(v) = self.exit_after {
            kv("--exit-after", v.to_string());
        }
        if let Some(v) = &self.init_version {
            kv("--init-version", v.clone());
        }
        if let Some(v) = &self.script {
            kv("--script", v.to_string_lossy().into_owned());
        }
        if let Some(v) = self.big_items {
            kv("--big-items", v.to_string());
        }
        if let Some(v) = &self.read_only {
            kv("--read-only", v.join(","));
        }
        for (on, flag) in [
            (self.garbage_on_start, "--garbage-on-start"),
            (self.sampling_request, "--sampling-request"),
            (self.elicitation_request, "--elicitation-request"),
            (self.roots_request, "--roots-request"),
            (self.stderr_noise, "--stderr-noise"),
            (self.log_messages, "--log-messages"),
            (self.subscribe, "--subscribe"),
            (self.oversize_response, "--oversize-response"),
            (self.string_ids, "--string-ids"),
            (self.content_length_framing, "--content-length-framing"),
            (self.init_error, "--init-error"),
            (self.pretty_json, "--pretty-json"),
            (self.stderr_leak, "--stderr-leak"),
            (self.structured, "--structured"),
            (self.verbose_descriptions, "--verbose-descriptions"),
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

/// Builder for the mock Streamable HTTP server (`mock-mcp-http`).
#[derive(Debug, Clone, Default)]
pub struct MockHttpSpec {
    program: Option<PathBuf>,
    args: Vec<String>,
}

impl MockHttpSpec {
    pub fn new() -> Self {
        Self::default()
    }
    /// Explicit binary path (e.g. `env!("CARGO_BIN_EXE_mock-mcp-http")`).
    pub fn program(mut self, p: impl Into<PathBuf>) -> Self {
        self.program = Some(p.into());
        self
    }
    /// Extra tool names.
    pub fn tools(mut self, t: &[&str]) -> Self {
        self.args.extend(["--tools".into(), t.join(",")]);
        self
    }
    /// Stream responses as SSE with a progress notification first.
    pub fn sse(mut self) -> Self {
        self.args.push("--sse".into());
        self
    }
    /// Require header `name: value`; 401 otherwise.
    pub fn require_header(mut self, name: &str, value: &str) -> Self {
        self.args
            .extend(["--require-header".into(), format!("{name}={value}")]);
        self
    }
    /// Drop a session after `n` requests (later ones get 404).
    pub fn drop_session_after(mut self, n: u64) -> Self {
        self.args
            .extend(["--drop-session-after".into(), n.to_string()]);
        self
    }
    /// Answer the first `n` `tools/call` requests with 401.
    pub fn reject_calls(mut self, n: u64) -> Self {
        self.args.extend(["--reject-calls".into(), n.to_string()]);
        self
    }
    /// Advertise `resources.subscribe`, serve subscribe/unsubscribe, keep
    /// `GET` event streams open; tool `touch` (argument `uri`) sends
    /// `notifications/resources/updated` on every open stream, tool
    /// `sub_stats` reports counters and the number of open streams.
    pub fn subscribe(mut self) -> Self {
        self.args.push("--subscribe".into());
        self
    }
    /// Delay every non-initialize response by `ms`.
    pub fn slow_ms(mut self, ms: u64) -> Self {
        self.args.extend(["--slow-ms".into(), ms.to_string()]);
        self
    }
    /// Answer every `tools/call` with an HTML error page (502 unless
    /// `status` is also set).
    pub fn html_error(mut self) -> Self {
        self.args.push("--html-error".into());
        self
    }
    /// Answer every `tools/call` with HTTP status `code` and a plain-text body.
    pub fn status(mut self, code: u16) -> Self {
        self.args.extend(["--status".into(), code.to_string()]);
        self
    }

    /// Path of the mock binary (explicit, `MCPDOZE_MOCK_MCP_HTTP`, or a
    /// sibling of the current executable).
    pub fn program_path(&self) -> PathBuf {
        if let Some(p) = &self.program {
            return p.clone();
        }
        if let Some(p) = std::env::var_os("MCPDOZE_MOCK_MCP_HTTP") {
            return p.into();
        }
        let mut dir = std::env::current_exe().unwrap_or_default();
        dir.pop();
        if dir.ends_with("deps") {
            dir.pop();
        }
        dir.join(format!("mock-mcp-http{}", std::env::consts::EXE_SUFFIX))
    }

    /// Command-line arguments for the configured behaviours.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Starts the server and waits (bounded) for its port line. The process
    /// is killed when the returned handle is dropped.
    pub async fn spawn(&self) -> std::io::Result<MockHttpServer> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut child = tokio::process::Command::new(self.program_path())
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;
        let mut line = String::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            BufReader::new(stdout).read_line(&mut line),
        )
        .await
        .map_err(|_| std::io::Error::other("mock-mcp-http did not print a port"))??;
        let port = line
            .trim()
            .parse::<u16>()
            .map_err(|_| std::io::Error::other(format!("bad port line ({read} bytes)")))?;
        Ok(MockHttpServer { child, port })
    }
}

/// A running `mock-mcp-http` process.
#[derive(Debug)]
pub struct MockHttpServer {
    child: tokio::process::Child,
    port: u16,
}

impl MockHttpServer {
    /// Endpoint URL of the MCP resource.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }
    /// Listening port.
    pub fn port(&self) -> u16 {
        self.port
    }
    /// Kills the process and waits for it.
    pub async fn stop(&mut self) {
        let _ = self.child.kill().await;
    }
}
