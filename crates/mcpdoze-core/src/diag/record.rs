// SPDX-License-Identifier: GPL-3.0-or-later

//! Failure record schema, `v: 1`, one JSON object per line.
//!
//! Sanitised by construction: no type here can hold a raw JSON value or an
//! arbitrary string. Text is one of
//!
//! * [`Name`]: a public identifier (server, tool, method, `serverInfo`),
//!   reduced to a short printable token;
//! * [`Scrubbed`]: text that went through the scrubber (or was composed by
//!   this module from schema-declared names), constructible only inside
//!   `diag`;
//! * a `&'static str` or an enum.
//!
//! Argument and result content appears only as a [`ShapeRoot`]
//! (type, length, class, digit count; see `shape.rs`).

use serde::Serialize;

use super::classify::{BodyClass, Cause, Signature};
use super::ring::{Dir, FrameType};
use super::shape::{SchemaDiff, ShapeRoot};

/// Schema version written in every record.
pub const SCHEMA_VERSION: u8 = 1;

const NAME_MAX: usize = 80;

/// A public identifier: letters, digits and `_ - . : / @ + #` only, at most
/// 80 characters; anything else becomes `_`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Name(String);

impl Name {
    /// Sanitises `s`.
    pub fn new(s: &str) -> Name {
        Name(
            s.chars()
                .take(NAME_MAX)
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "_-.:/@+#".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .collect(),
        )
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Free text that was scrubbed (see `redact.rs`) or composed from safe
/// fragments. Only code inside `diag` can construct it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Scrubbed(String);

impl Scrubbed {
    /// Output of the scrubber.
    pub(super) fn from_scrubber(s: String) -> Self {
        Scrubbed(s)
    }

    /// Text composed from schema-declared names and static words.
    pub(super) fn from_safe(s: String) -> Self {
        Scrubbed(s)
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The backend process could not be started.
    SpawnFailed,
    /// No `initialize` reply in time.
    InitTimeout,
    /// No `initialize` reply and no stdout byte at all.
    InitNoOutput,
    /// `initialize` returned an error.
    InitError,
    /// Protocol version the broker does not speak.
    VersionMismatch,
    /// The backend exited while it should run.
    ExitUnexpected,
    /// An in-flight call died with the backend.
    CrashInFlight,
    /// A line over the frame limit.
    FrameOversize,
    /// Non-JSON on stdout.
    StdoutPollution,
    /// JSON that is not JSON-RPC.
    FrameInvalid,
    /// Response id of another JSON type.
    IdTypeMismatch,
    /// Response for an abandoned call.
    LateResponse,
    /// JSON-RPC error (or `isError` with `tool_errors`).
    RpcError,
    /// No answer for too long.
    Stalled,
    /// Slower than the `slow_call` threshold.
    SlowCall,
    /// The agent cancelled.
    AgentCancelled,
    /// The agent went away.
    AgentDisconnected,
    /// Call raced with an unload.
    RaceUnload,
    /// Start refused during backoff.
    BackoffRejected,
    /// Unexpected HTTP status.
    HttpStatus,
    /// 401/403.
    HttpAuth,
    /// Body is not MCP.
    HttpNonMcpBody,
    /// The upstream lost the session.
    HttpSessionLost,
    /// HTTP request timed out.
    HttpTimeout,
    /// Event stream ended early.
    SseTruncated,
    /// Connection failed.
    ConnectFailed,
    /// Server-to-client request not served.
    ClientRequestUnserved,
    /// A list call failed.
    ListFailed,
    /// A list cursor repeated.
    ListCursorLoop,
    /// Flood control summary.
    Suppressed,
}

impl Kind {
    /// Every kind.
    pub const ALL: &'static [Kind] = &[
        Kind::SpawnFailed,
        Kind::InitTimeout,
        Kind::InitNoOutput,
        Kind::InitError,
        Kind::VersionMismatch,
        Kind::ExitUnexpected,
        Kind::CrashInFlight,
        Kind::FrameOversize,
        Kind::StdoutPollution,
        Kind::FrameInvalid,
        Kind::IdTypeMismatch,
        Kind::LateResponse,
        Kind::RpcError,
        Kind::Stalled,
        Kind::SlowCall,
        Kind::AgentCancelled,
        Kind::AgentDisconnected,
        Kind::RaceUnload,
        Kind::BackoffRejected,
        Kind::HttpStatus,
        Kind::HttpAuth,
        Kind::HttpNonMcpBody,
        Kind::HttpSessionLost,
        Kind::HttpTimeout,
        Kind::SseTruncated,
        Kind::ConnectFailed,
        Kind::ClientRequestUnserved,
        Kind::ListFailed,
        Kind::ListCursorLoop,
        Kind::Suppressed,
    ];

    /// Snake-case name, as written.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::SpawnFailed => "spawn_failed",
            Kind::InitTimeout => "init_timeout",
            Kind::InitNoOutput => "init_no_output",
            Kind::InitError => "init_error",
            Kind::VersionMismatch => "version_mismatch",
            Kind::ExitUnexpected => "exit_unexpected",
            Kind::CrashInFlight => "crash_in_flight",
            Kind::FrameOversize => "frame_oversize",
            Kind::StdoutPollution => "stdout_pollution",
            Kind::FrameInvalid => "frame_invalid",
            Kind::IdTypeMismatch => "id_type_mismatch",
            Kind::LateResponse => "late_response",
            Kind::RpcError => "rpc_error",
            Kind::Stalled => "stalled",
            Kind::SlowCall => "slow_call",
            Kind::AgentCancelled => "agent_cancelled",
            Kind::AgentDisconnected => "agent_disconnected",
            Kind::RaceUnload => "race_unload",
            Kind::BackoffRejected => "backoff_rejected",
            Kind::HttpStatus => "http_status",
            Kind::HttpAuth => "http_auth",
            Kind::HttpNonMcpBody => "http_non_mcp_body",
            Kind::HttpSessionLost => "http_session_lost",
            Kind::HttpTimeout => "http_timeout",
            Kind::SseTruncated => "sse_truncated",
            Kind::ConnectFailed => "connect_failed",
            Kind::ClientRequestUnserved => "client_request_unserved",
            Kind::ListFailed => "list_failed",
            Kind::ListCursorLoop => "list_cursor_loop",
            Kind::Suppressed => "suppressed",
        }
    }

    /// Kinds recorded only in `trace` mode: slow or odd-but-successful
    /// calls and noise.
    pub fn trace_only(self) -> bool {
        matches!(
            self,
            Kind::SlowCall
                | Kind::StdoutPollution
                | Kind::LateResponse
                | Kind::AgentCancelled
                | Kind::AgentDisconnected
        )
    }
}

/// Backend transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Child process on stdio.
    Stdio,
    /// Streamable HTTP.
    Http,
}

/// Instance scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// One instance for everyone.
    Shared,
    /// One per working directory.
    PerCwd,
    /// One per session.
    PerSession,
}

/// Agent protocol era.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Era {
    /// Session-based protocol.
    Legacy,
    /// 2026-07-28 stateless protocol.
    Modern,
}

/// JSON type of a request id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdType {
    /// A number.
    Number,
    /// A string.
    String,
    /// `null`.
    Null,
    /// Anything else.
    Other,
}

/// Who produced the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The broker itself.
    Broker,
    /// The backend server.
    Backend,
    /// The agent (client).
    Agent,
    /// The HTTP upstream.
    Http,
}

/// Identity of the instance; flattened into every record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Identity {
    /// Server name.
    pub server: Name,
    /// Transport.
    pub transport: Transport,
    /// Launcher basename (stdio).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launcher: Option<Name>,
    /// Scope.
    pub scope: Scope,
    /// Salted hash of the instance key (scoped instances).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

/// One lifecycle transition as written.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TransitionRec {
    /// State entered.
    pub to: &'static str,
    /// Milliseconds ago.
    pub age_ms: u64,
    /// Why.
    pub reason: &'static str,
    /// Calls in flight then.
    pub in_flight: u32,
}

/// `serverInfo` as reported.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServerInfoRec {
    /// Name.
    pub name: Name,
    /// Version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<Name>,
}

/// Backend state at the time of the record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BackendRec {
    /// State name.
    pub state: &'static str,
    /// Previous state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_state: Option<&'static str>,
    /// Process uptime.
    pub uptime_ms: u64,
    /// Calls in flight.
    pub in_flight: u64,
    /// Calls awaiting a backend response.
    pub pending: u64,
    /// Calls served so far.
    pub calls_served: u64,
    /// Restarts so far.
    pub restarts: u64,
    /// Failures so far.
    pub failures: u64,
    /// Newest transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_transition: Option<TransitionRec>,
    /// Negotiated protocol version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Name>,
    /// `serverInfo`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_info: Option<ServerInfoRec>,
    /// Capability keys.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<Name>,
    /// Non-JSON stdout lines seen since spawn.
    #[serde(skip_serializing_if = "is_zero")]
    pub stdout_noise: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// The request being served.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestRec {
    /// Per-process request number (same as the stats `req_id`).
    pub req: u64,
    /// Protocol era.
    pub era: Era,
    /// Method.
    pub method: Name,
    /// Tool name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<Name>,
    /// JSON type of the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_type: Option<IdType>,
    /// The agent asked for progress.
    pub progress: bool,
    /// Request size.
    pub bytes: u64,
    /// Time since the request arrived.
    pub elapsed_ms: u64,
    /// Time spent obtaining a backend.
    pub acquire_ms: u64,
    /// The backend was cold.
    pub cold_start: bool,
    /// Last request sent before a crash.
    pub suspect: bool,
    /// Argument shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape: Option<ShapeRoot>,
    /// Arguments versus input schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_diff: Option<SchemaDiff>,
}

/// Process exit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExitRec {
    /// Exit code.
    pub code: Option<i32>,
    /// Unix signal.
    pub signal: Option<i32>,
    /// Windows NTSTATUS name.
    pub status_name: Option<&'static str>,
}

/// The error behind the record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorRec {
    /// Source.
    pub origin: Origin,
    /// JSON-RPC code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    /// Scrubbed message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Scrubbed>,
    /// Shape of the error `data`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_shape: Option<ShapeRoot>,
}

/// One frame as written.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FrameRec {
    /// Milliseconds relative to the record (negative: before).
    pub dt_ms: i64,
    /// Direction.
    pub dir: Dir,
    /// Class.
    #[serde(rename = "type")]
    pub ftype: FrameType,
    /// Method (fixed table).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<Name>,
    /// Backend request id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bid: Option<u64>,
    /// Size.
    pub bytes: u32,
    /// Noise signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<Signature>,
}

/// Scrubbed stderr tail.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StderrRec {
    /// Lines written since spawn.
    pub total: u64,
    /// The newest lines, scrubbed or reduced to `<line len=N>`.
    pub tail: Vec<Scrubbed>,
}

/// A reduced URL: scheme, salted host hash, path depth. Never a query.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UrlInfo {
    /// Scheme.
    pub scheme: Name,
    /// 8 hex digits of a salted hash of the host.
    pub host: String,
    /// Number of non-empty path segments.
    pub path_depth: usize,
}

/// HTTP exchange metadata.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HttpRec {
    /// Reduced URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<UrlInfo>,
    /// Status code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Media type without parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<Name>,
    /// Body length.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_len: Option<u64>,
    /// Body class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_class: Option<BodyClass>,
    /// `www-authenticate` scheme only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub www_authenticate: Option<Name>,
    /// `retry-after` in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_s: Option<u64>,
    /// A session id was held (the id itself is never recorded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_present: Option<bool>,
    /// Transport error class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_error: Option<&'static str>,
    /// The auth helper was re-run.
    pub helper_rerun: bool,
}

/// Broker build facts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BrokerRec {
    /// Crate version.
    pub version: &'static str,
    /// `std::env::consts::OS`.
    pub os: &'static str,
    /// `std::env::consts::ARCH`.
    pub arch: &'static str,
}

impl BrokerRec {
    /// This build.
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }
}

/// One failure record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Record {
    /// Schema version.
    pub v: u8,
    /// Unix time in ms.
    pub ts: u64,
    /// `f-NNNNNN`, assigned by the writer.
    pub id: String,
    /// Links records of one incident.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incident: Option<String>,
    /// What failed.
    pub kind: Kind,
    /// 1 for a full record.
    pub repeat: u32,
    /// Instance identity.
    #[serde(flatten)]
    pub ident: Identity,
    /// Backend generation (respawn count).
    pub generation: u64,
    /// Backend state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendRec>,
    /// Request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestRec>,
    /// Exit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitRec>,
    /// HTTP exchange.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpRec>,
    /// Error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorRec>,
    /// Recent frames.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub frames: Vec<FrameRec>,
    /// Recent lifecycle transitions (races only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub transitions: Vec<TransitionRec>,
    /// Stderr tail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<StderrRec>,
    /// Guessed cause.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    /// Build facts.
    pub broker: BrokerRec,
    /// Records suppressed by flood control (`suppressed` kind).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppressed: Option<u64>,
}

impl Record {
    /// A bare record.
    pub fn new(kind: Kind, ident: Identity, ts: u64) -> Self {
        Record {
            v: SCHEMA_VERSION,
            ts,
            id: String::new(),
            incident: None,
            kind,
            repeat: 1,
            ident,
            generation: 0,
            backend: None,
            request: None,
            exit: None,
            http: None,
            error: None,
            frames: Vec::new(),
            transitions: Vec::new(),
            stderr: None,
            cause: None,
            broker: BrokerRec::current(),
            suppressed: None,
        }
    }
}

/// The compact line written instead of a full record when an identical
/// signature (kind, tool, shape hash, exit) repeats.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepeatLine {
    /// Schema version.
    pub v: u8,
    /// Unix time in ms.
    pub ts: u64,
    /// Kind.
    pub kind: Kind,
    /// Server.
    pub server: Name,
    /// Occurrences so far, the first included.
    pub repeat: u32,
    /// Id of the full record.
    pub of: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-level proof by inspection: this file's code (comments
    /// stripped) never names the JSON value type, so no record field can
    /// hold raw user data.
    #[test]
    fn record_types_hold_no_json_value() {
        let src = include_str!("record.rs");
        let code_end = src.find("#[cfg(test)]").unwrap();
        for line in src[..code_end].lines() {
            let l = line.trim_start();
            if l.starts_with("//") {
                continue;
            }
            assert!(!l.contains("Value"), "record.rs names a JSON value: {l}");
            assert!(!l.contains("serde_json"), "record.rs uses serde_json: {l}");
        }
    }

    #[test]
    fn names_are_sanitised_and_bounded() {
        assert_eq!(Name::new("a b\n\"c\"").as_str(), "a_b__c_");
        assert_eq!(Name::new(&"x".repeat(500)).as_str().len(), 80);
        assert_eq!(Name::new("tools/call").as_str(), "tools/call");
    }

    #[test]
    fn a_record_serialises_with_the_documented_fields() {
        let ident = Identity {
            server: Name::new("example"),
            transport: Transport::Stdio,
            launcher: Some(Name::new("npx")),
            scope: Scope::Shared,
            instance: None,
        };
        let mut r = Record::new(Kind::CrashInFlight, ident, 1_767_225_600_123);
        r.id = "f-000123".into();
        r.generation = 4;
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["kind"], "crash_in_flight");
        assert_eq!(v["server"], "example");
        assert_eq!(v["transport"], "stdio");
        assert_eq!(v["scope"], "shared");
        assert_eq!(v["generation"], 4);
        assert_eq!(v["repeat"], 1);
    }
}
