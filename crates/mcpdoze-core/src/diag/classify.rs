// SPDX-License-Identifier: GPL-3.0-or-later

//! Pure cause classifier: (kind, exit, signature, schema diff, phase, ...)
//! to a [`Cause`]. No I/O, no clock, no user text: every `guess` is a static
//! string, every hint is composed from schema-declared names.

use std::io;

use serde::Serialize;

use super::record::{ExitRec, Kind, Scrubbed};
use super::shape::SchemaDiff;

/// What a non-JSON stdout line looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Signature {
    /// Empty or whitespace only.
    Empty,
    /// `Content-Length:` header (LSP-style framing).
    ContentLength,
    /// UTF-8 byte order mark.
    Bom,
    /// NUL bytes (UTF-16 output, typical of PowerShell).
    Nul,
    /// ANSI escape sequences.
    Ansi,
    /// npm, pip or similar banner.
    Banner,
    /// A lone `{` or `[` (pretty-printed multi-line JSON).
    LoneBrace,
    /// Anything else: a plain text line.
    Text,
}

impl Signature {
    /// Classifies one stdout line (without its newline).
    pub fn of(line: &[u8]) -> Signature {
        let head = &line[..line.len().min(64)];
        if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
            return Signature::Bom;
        }
        if head.contains(&0) {
            return Signature::Nul;
        }
        if head.len() >= 15 && head[..15].eq_ignore_ascii_case(b"content-length:") {
            return Signature::ContentLength;
        }
        if line.contains(&0x1b) {
            return Signature::Ansi;
        }
        let text = String::from_utf8_lossy(&line[..line.len().min(256)]);
        let t = text.trim();
        if t.is_empty() {
            return Signature::Empty;
        }
        if t == "{" || t == "[" {
            return Signature::LoneBrace;
        }
        const BANNERS: &[&str] = &[
            "npm ",
            "pip ",
            "Collecting ",
            "Downloading ",
            "added ",
            "Need to install",
            "Ok to proceed",
            "Installing ",
            "Resolved ",
            "Fetching ",
        ];
        if BANNERS.iter().any(|b| t.starts_with(b)) {
            return Signature::Banner;
        }
        Signature::Text
    }
}

/// How far a start-up got before it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Process spawned, nothing sent.
    Spawned,
    /// `initialize` sent, no stdout byte yet.
    InitSent,
    /// Some stdout bytes seen, no reply.
    StdoutSeen,
    /// `initialize` was answered.
    InitReplied,
}

/// Class of an HTTP response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyClass {
    /// JSON.
    Json,
    /// Server-sent events.
    Sse,
    /// HTML.
    Html,
    /// No body.
    Empty,
    /// Plain text.
    Text,
    /// Anything else.
    Other,
}

/// How sure the guess is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// A hunch.
    Low,
    /// Likely.
    Medium,
    /// The signature is specific.
    High,
}

/// The classifier's answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cause {
    /// One short sentence (static text).
    pub guess: &'static str,
    /// Confidence.
    pub confidence: Confidence,
    /// Supporting facts, composed from schema-declared names only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<Scrubbed>,
}

/// Inputs of [`classify`]; every field is optional context.
#[derive(Debug, Clone, Copy)]
pub struct ClassifyInput<'a> {
    /// Record kind.
    pub kind: Kind,
    /// Launcher basename (`npx`, `uvx`, ...).
    pub launcher: Option<&'a str>,
    /// Exit information.
    pub exit: Option<&'a ExitRec>,
    /// Stdout noise signature.
    pub signature: Option<Signature>,
    /// Arguments versus input schema.
    pub schema_diff: Option<&'a SchemaDiff>,
    /// Start-up phase reached.
    pub phase: Option<Phase>,
    /// `io::ErrorKind` of a failed spawn.
    pub io_kind: Option<io::ErrorKind>,
    /// JSON-RPC error code.
    pub rpc_code: Option<i64>,
    /// HTTP status.
    pub http_status: Option<u16>,
    /// HTTP body class.
    pub body_class: Option<BodyClass>,
}

impl ClassifyInput<'_> {
    /// An input with only the kind set.
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            launcher: None,
            exit: None,
            signature: None,
            schema_diff: None,
            phase: None,
            io_kind: None,
            rpc_code: None,
            http_status: None,
            body_class: None,
        }
    }
}

/// Windows NTSTATUS exit codes worth naming.
pub fn ntstatus_name(code: i32) -> Option<&'static str> {
    Some(match code as u32 {
        0xC000_0005 => "ACCESS_VIOLATION",
        0xC000_00FD => "STACK_OVERFLOW",
        0xC000_0135 => "DLL_NOT_FOUND",
        0xC000_0142 => "DLL_INIT_FAILED",
        0xC000_013A => "CONTROL_C_EXIT",
        0xC000_0374 => "HEAP_CORRUPTION",
        0xC000_0409 => "STACK_BUFFER_OVERRUN",
        0xC000_001D => "ILLEGAL_INSTRUCTION",
        _ => return None,
    })
}

const FETCHING_LAUNCHERS: &[&str] = &[
    "npx", "uvx", "bunx", "pnpx", "pnpm", "yarn", "dlx", "pipx", "uv", "deno", "bun", "npm",
];

fn is_fetching(launcher: Option<&str>) -> bool {
    launcher.is_some_and(|l| FETCHING_LAUNCHERS.iter().any(|f| l.eq_ignore_ascii_case(f)))
}

fn c(guess: &'static str, confidence: Confidence) -> Cause {
    Cause {
        guess,
        confidence,
        hints: Vec::new(),
    }
}

fn exit_guess(exit: Option<&ExitRec>, diff: Option<&SchemaDiff>) -> (&'static str, Confidence) {
    use Confidence::*;
    if let Some(d) = diff {
        if d.has_null_mismatch() {
            return ("server crashed on a null optional argument", Medium);
        }
        if !d.is_empty_issues() {
            return (
                "server failed on arguments that do not match its schema",
                Medium,
            );
        }
    }
    let Some(e) = exit else {
        return ("server stopped; exit status unavailable", Low);
    };
    match (e.signal, e.status_name, e.code) {
        (Some(9), _, _) => ("killed, possibly out of memory", Medium),
        (Some(11), _, _) | (_, Some("ACCESS_VIOLATION"), _) => {
            ("server crashed with a memory access violation", Medium)
        }
        (Some(6), _, _) | (_, Some("STACK_BUFFER_OVERRUN" | "HEAP_CORRUPTION"), _) => {
            ("server aborted (runtime fault)", Medium)
        }
        (Some(15), _, _) => ("server was terminated by a signal", Low),
        (_, Some("STACK_OVERFLOW"), _) => ("server overflowed its stack", Medium),
        (_, Some("CONTROL_C_EXIT"), _) => ("server was interrupted (Ctrl+C)", Medium),
        (_, Some("DLL_NOT_FOUND" | "DLL_INIT_FAILED"), _) => (
            "a library the server executable needs is missing or failed to load",
            High,
        ),
        (_, Some(_), _) => ("server crashed with a Windows exception status", Medium),
        (_, _, Some(0)) => (
            "server exited cleanly while the broker expected it to keep running",
            Medium,
        ),
        (_, _, Some(127 | 9009)) => ("a command inside the launcher script was not found", Medium),
        (_, _, Some(_)) => (
            "server exited with an error status; see the stderr tail",
            Low,
        ),
        _ => ("server stopped; exit status unavailable", Low),
    }
}

/// Maps the facts of a failure to a cause. Pure and total.
pub fn classify(i: &ClassifyInput<'_>) -> Cause {
    use Confidence::*;
    use Kind::*;
    let mut cause = match i.kind {
        SpawnFailed => match i.io_kind {
            Some(io::ErrorKind::NotFound) => {
                c("launcher not found; check the command and PATH", High)
            }
            Some(io::ErrorKind::PermissionDenied) => {
                c("launcher is not executable or is blocked by policy", High)
            }
            _ => c("the server process could not be started", Low),
        },
        InitTimeout | InitNoOutput => {
            let silent = matches!(i.phase, None | Some(Phase::Spawned | Phase::InitSent))
                || i.kind == InitNoOutput;
            if silent && is_fetching(i.launcher) {
                c("package download; raise start_timeout", High)
            } else if i.phase == Some(Phase::StdoutSeen) {
                c(
                    "server wrote output but never answered initialize (non-MCP output, or too slow)",
                    Medium,
                )
            } else if silent {
                c(
                    "server wrote nothing to stdout; check that it speaks MCP over stdio",
                    Medium,
                )
            } else {
                c("server did not answer initialize in time", Low)
            }
        }
        InitError => c("server rejected initialize", Medium),
        VersionMismatch => c(
            "server answered with a protocol version the broker does not speak",
            High,
        ),
        ExitUnexpected | CrashInFlight => {
            let (g, conf) = exit_guess(i.exit, i.schema_diff);
            c(g, conf)
        }
        FrameOversize => c(
            "server wrote one message over the frame limit; return less data or page results",
            High,
        ),
        StdoutPollution => match i.signature.unwrap_or(Signature::Text) {
            Signature::ContentLength => c("server uses LSP-style Content-Length framing", High),
            Signature::Bom => c("server writes a UTF-8 byte order mark before JSON", High),
            Signature::Nul => c(
                "server writes UTF-16 (NUL bytes), typical of PowerShell",
                High,
            ),
            Signature::Ansi => c("server writes colour codes to stdout", High),
            Signature::Banner => c(
                "a package manager or start-up banner was printed on stdout",
                High,
            ),
            Signature::LoneBrace => c(
                "server pretty-prints JSON over several lines; MCP needs one message per line",
                High,
            ),
            Signature::Empty => c("server writes blank lines to stdout", Medium),
            Signature::Text => c(
                "server prints log text on stdout; logs must go to stderr",
                Medium,
            ),
        },
        FrameInvalid => c(
            "a message is JSON but not a valid JSON-RPC 2.0 message",
            Medium,
        ),
        IdTypeMismatch => c(
            "server echoes request ids as a different JSON type than it received",
            High,
        ),
        LateResponse => c("a response arrived after its call was abandoned", Medium),
        RpcError => match i.rpc_code {
            Some(-32602) => {
                let conf = if i.schema_diff.is_some_and(|d| !d.is_empty_issues()) {
                    High
                } else {
                    Medium
                };
                c("server rejected the arguments as invalid params", conf)
            }
            Some(-32601) => c("method or tool not found on the server", Medium),
            Some(-32603) => c("internal error inside the server", Medium),
            Some(_) | None => c("server returned a JSON-RPC error", Low),
        },
        Stalled => c("server did not answer in time", Medium),
        SlowCall => c("call took longer than the slow_call threshold", Low),
        AgentCancelled => c("the agent cancelled the call while it waited", Medium),
        AgentDisconnected => c("the agent disconnected while its call was running", Medium),
        RaceUnload => c(
            "a call raced with an idle unload or restart of the backend",
            Medium,
        ),
        BackoffRejected => c(
            "start rejected during restart backoff after earlier failures",
            Medium,
        ),
        HttpStatus => match i.http_status {
            Some(401 | 403) => c("credentials missing or rejected", Medium),
            Some(404) => c("the URL path is not the MCP endpoint", Medium),
            Some(429) => c("the upstream rate-limited the broker", Medium),
            Some(500..=599) => c("the upstream server failed", Medium),
            _ => c("the upstream answered with an unexpected HTTP status", Low),
        },
        HttpAuth => c(
            "credentials missing or rejected; check headers or the auth helper",
            High,
        ),
        HttpNonMcpBody => match i.body_class {
            Some(BodyClass::Html) => c(
                "URL is not the MCP endpoint, or a proxy or login page answered",
                High,
            ),
            Some(BodyClass::Empty) => c("the upstream answered with an empty body", Medium),
            Some(BodyClass::Json) => c(
                "the upstream answered with JSON that is not a JSON-RPC message",
                Medium,
            ),
            _ => c("the upstream answered with a body that is not MCP", Medium),
        },
        HttpSessionLost => c(
            "the upstream forgot the session (restart or expiry)",
            Medium,
        ),
        HttpTimeout => c(
            "the upstream did not answer within the request timeout",
            Medium,
        ),
        SseTruncated => c("the event stream ended before the response arrived", Medium),
        ConnectFailed => c(
            "could not connect to the upstream; check the URL, DNS and network",
            Medium,
        ),
        ClientRequestUnserved => c(
            "the backend asked the agent for something the agent cannot serve",
            Medium,
        ),
        ListFailed => c("the backend did not list its items in time", Medium),
        ListCursorLoop => c("the backend repeats a pagination cursor", High),
        Suppressed => c(
            "further records of this server were suppressed (flood control)",
            High,
        ),
    };
    if let Some(d) = i.schema_diff {
        cause.hints.extend(d.hints());
    }
    cause
}

#[cfg(test)]
mod tests {
    use super::super::record::Name;
    use super::*;

    fn exit(code: Option<i32>, signal: Option<i32>) -> ExitRec {
        ExitRec {
            code,
            signal,
            status_name: code.and_then(ntstatus_name),
        }
    }

    #[test]
    fn signatures() {
        let cases: &[(&[u8], Signature)] = &[
            (b"Content-Length: 12", Signature::ContentLength),
            (b"content-length:12", Signature::ContentLength),
            (b"\xEF\xBB\xBF{\"jsonrpc\"", Signature::Bom),
            (b"{\0\"\0j\0", Signature::Nul),
            (b"\x1b[32mready\x1b[0m", Signature::Ansi),
            (b"npm warn exec The following package", Signature::Banner),
            (b"  {", Signature::LoneBrace),
            (b"   ", Signature::Empty),
            (b"server listening", Signature::Text),
        ];
        for (line, want) in cases {
            assert_eq!(
                Signature::of(line),
                *want,
                "{:?}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn table() {
        let diff_null = SchemaDiff {
            type_mismatch: vec![Scrubbed::from_safe(
                "/cursor: null, schema says string".into(),
            )],
            ..SchemaDiff::default()
        };
        let e1 = exit(Some(1), None);
        let e9 = exit(None, Some(9));
        let e0 = exit(Some(0), None);
        let eav = exit(Some(0xC000_0005u32 as i32), None);
        let mk = ClassifyInput::new;
        let rows: Vec<(ClassifyInput, &str, Confidence)> = vec![
            (
                ClassifyInput {
                    signature: Some(Signature::ContentLength),
                    ..mk(Kind::StdoutPollution)
                },
                "server uses LSP-style Content-Length framing",
                Confidence::High,
            ),
            (
                ClassifyInput {
                    launcher: Some("npx"),
                    phase: Some(Phase::InitSent),
                    ..mk(Kind::InitTimeout)
                },
                "package download; raise start_timeout",
                Confidence::High,
            ),
            (
                ClassifyInput {
                    launcher: Some("node"),
                    phase: Some(Phase::StdoutSeen),
                    ..mk(Kind::InitTimeout)
                },
                "server wrote output but never answered initialize (non-MCP output, or too slow)",
                Confidence::Medium,
            ),
            (
                ClassifyInput {
                    exit: Some(&e9),
                    ..mk(Kind::ExitUnexpected)
                },
                "killed, possibly out of memory",
                Confidence::Medium,
            ),
            (
                ClassifyInput {
                    exit: Some(&eav),
                    ..mk(Kind::ExitUnexpected)
                },
                "server crashed with a memory access violation",
                Confidence::Medium,
            ),
            (
                ClassifyInput {
                    exit: Some(&e0),
                    ..mk(Kind::CrashInFlight)
                },
                "server exited cleanly while the broker expected it to keep running",
                Confidence::Medium,
            ),
            (
                ClassifyInput {
                    exit: Some(&e1),
                    schema_diff: Some(&diff_null),
                    ..mk(Kind::CrashInFlight)
                },
                "server crashed on a null optional argument",
                Confidence::Medium,
            ),
            (
                ClassifyInput {
                    body_class: Some(BodyClass::Html),
                    ..mk(Kind::HttpNonMcpBody)
                },
                "URL is not the MCP endpoint, or a proxy or login page answered",
                Confidence::High,
            ),
            (
                ClassifyInput {
                    io_kind: Some(io::ErrorKind::NotFound),
                    ..mk(Kind::SpawnFailed)
                },
                "launcher not found; check the command and PATH",
                Confidence::High,
            ),
            (
                ClassifyInput {
                    http_status: Some(404),
                    ..mk(Kind::HttpStatus)
                },
                "the URL path is not the MCP endpoint",
                Confidence::Medium,
            ),
            (
                ClassifyInput {
                    rpc_code: Some(-32602),
                    schema_diff: Some(&diff_null),
                    ..mk(Kind::RpcError)
                },
                "server rejected the arguments as invalid params",
                Confidence::High,
            ),
            (
                mk(Kind::ListCursorLoop),
                "the backend repeats a pagination cursor",
                Confidence::High,
            ),
        ];
        for (input, guess, conf) in rows {
            let got = classify(&input);
            assert_eq!(
                (got.guess, got.confidence),
                (guess, conf),
                "{:?}",
                input.kind
            );
        }
    }

    #[test]
    fn hints_come_from_the_schema_diff() {
        let d = SchemaDiff {
            missing_required: vec![Name::new("query")],
            ..SchemaDiff::default()
        };
        let got = classify(&ClassifyInput {
            schema_diff: Some(&d),
            ..ClassifyInput::new(Kind::RpcError)
        });
        assert_eq!(got.hints.len(), 1);
        assert!(got.hints[0].as_str().contains("query"));
    }

    #[test]
    fn every_kind_has_a_guess() {
        for k in Kind::ALL {
            assert!(!classify(&ClassifyInput::new(*k)).guess.is_empty());
        }
    }
}
