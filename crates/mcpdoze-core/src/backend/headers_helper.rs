// SPDX-License-Identifier: GPL-3.0-or-later

//! `headers_helper`: a command whose output supplies HTTP request headers.
//!
//! The command runs through the platform shell (`cmd /d /s /c` on Windows,
//! `sh -c` elsewhere) with stdin and stderr closed, the broker token
//! variable removed from its environment, a [`HELPER_TIMEOUT`] deadline and
//! stdout capped at [`MAX_HELPER_OUTPUT`] bytes. It must exit 0 and print a
//! JSON object whose values are strings, e.g.
//! `{"Authorization": "Bearer <token>"}`. Names and values are validated as
//! HTTP header names and values; values are marked sensitive.
//!
//! The output is a secret: it is never logged, stored or echoed in errors.
//! Error messages describe what went wrong without quoting the output (not
//! even header names from it).

use std::process::Stdio;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;
use tokio::io::AsyncReadExt;

use super::TOKEN_ENV_VAR;

/// How long one helper run may take.
pub const HELPER_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest accepted helper output (stdout).
pub const MAX_HELPER_OUTPUT: usize = 64 * 1024;
/// Most headers one helper may return.
pub const MAX_HELPER_HEADERS: usize = 64;

/// Resolved request headers.
pub type Headers = Vec<(HeaderName, HeaderValue)>;

/// Why a helper run failed. Never carries any of the helper's output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HelperError {
    #[error("headers helper could not be started ({0})")]
    Spawn(String),
    #[error("headers helper timed out after {}s", .0.as_secs_f32())]
    Timeout(Duration),
    #[error("headers helper failed ({0})")]
    Exit(String),
    #[error("headers helper output is larger than {MAX_HELPER_OUTPUT} bytes")]
    TooLarge,
    #[error("headers helper output is not a JSON object of string values")]
    BadJson,
    #[error("headers helper returned more than {MAX_HELPER_HEADERS} headers")]
    TooMany,
    #[error("headers helper returned an invalid header name")]
    BadName,
    #[error("headers helper returned an invalid header value")]
    BadValue,
}

/// Runs `command` with the default [`HELPER_TIMEOUT`].
pub async fn run(command: &str) -> Result<Headers, HelperError> {
    run_with_timeout(command, HELPER_TIMEOUT).await
}

/// Runs `command` and parses its output; see the module documentation.
pub async fn run_with_timeout(command: &str, timeout: Duration) -> Result<Headers, HelperError> {
    let mut cmd = shell(command);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove(TOKEN_ENV_VAR)
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| HelperError::Spawn(e.kind().to_string()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| HelperError::Spawn("no stdout".into()))?;
    let work = async {
        let mut buf = Vec::new();
        (&mut stdout)
            .take(MAX_HELPER_OUTPUT as u64 + 1)
            .read_to_end(&mut buf)
            .await
            .map_err(|e| HelperError::Exit(format!("cannot read output: {}", e.kind())))?;
        if buf.len() > MAX_HELPER_OUTPUT {
            return Err(HelperError::TooLarge);
        }
        let status = child
            .wait()
            .await
            .map_err(|e| HelperError::Exit(e.kind().to_string()))?;
        Ok((buf, status))
    };
    let (buf, status) = match tokio::time::timeout(timeout, work).await {
        Ok(r) => r?,
        Err(_) => return Err(HelperError::Timeout(timeout)),
    };
    if !status.success() {
        return Err(HelperError::Exit(match status.code() {
            Some(c) => format!("exit status {c}"),
            None => "terminated by a signal".into(),
        }));
    }
    parse(&buf)
}

/// Parses helper output: a JSON object of string values.
pub fn parse(output: &[u8]) -> Result<Headers, HelperError> {
    let v: Value = serde_json::from_slice(output).map_err(|_| HelperError::BadJson)?;
    let Value::Object(map) = v else {
        return Err(HelperError::BadJson);
    };
    if map.len() > MAX_HELPER_HEADERS {
        return Err(HelperError::TooMany);
    }
    let mut out = Vec::with_capacity(map.len());
    for (k, v) in map {
        let Value::String(v) = v else {
            return Err(HelperError::BadJson);
        };
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| HelperError::BadName)?;
        let mut value = HeaderValue::from_str(&v).map_err(|_| HelperError::BadValue)?;
        value.set_sensitive(true);
        out.push((name, value));
    }
    Ok(out)
}

/// `base` with every header also present in `over` replaced by `over`'s
/// (helper headers override static ones; names compare case-insensitively).
pub fn merge(base: &Headers, over: Headers) -> Headers {
    let mut out: Headers = base
        .iter()
        .filter(|(k, _)| !over.iter().any(|(o, _)| o == k))
        .cloned()
        .collect();
    out.extend(over);
    out
}

#[cfg(windows)]
fn shell(command: &str) -> tokio::process::Command {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let comspec = std::env::var_os("ComSpec")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "cmd.exe".into());
    let mut c = tokio::process::Command::new(comspec);
    // `/s` strips exactly the outer quotes, so the command is taken as is.
    c.args(["/d", "/s", "/c"])
        .raw_arg(format!("\"{command}\""))
        .creation_flags(CREATE_NO_WINDOW);
    c
}

#[cfg(not(windows))]
fn shell(command: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(command);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell command printing `json` (as is) on this platform.
    fn print_cmd(json: &str) -> String {
        if cfg!(windows) {
            format!("echo {json}")
        } else {
            format!("printf '%s' '{json}'")
        }
    }

    #[tokio::test]
    async fn runs_and_parses() {
        let h = run(&print_cmd(
            r#"{"X-Example":"one","Authorization":"Bearer x"}"#,
        ))
        .await
        .unwrap();
        assert_eq!(h.len(), 2);
        assert!(h.iter().all(|(_, v)| v.is_sensitive()));
        assert_eq!(h[0].0.as_str(), "x-example");
        assert_eq!(h[0].1.to_str().unwrap(), "one");
    }

    #[tokio::test]
    async fn times_out() {
        let cmd = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >nul"
        } else {
            "sleep 30"
        };
        let t0 = std::time::Instant::now();
        let e = run_with_timeout(cmd, Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(matches!(e, HelperError::Timeout(_)), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn failures_never_echo_output() {
        const SECRET: &str = "helper-secret-value-7c1";
        let bad = [
            print_cmd(&format!("not json {SECRET}")),
            print_cmd(&format!(r#"["{SECRET}"]"#)),
            print_cmd(&format!(r#"{{"X-A":1,"X-B":"{SECRET}"}}"#)),
            print_cmd(&format!(r#"{{"bad name {SECRET}":"v"}}"#)),
        ];
        for cmd in &bad {
            let e = run(cmd).await.unwrap_err();
            assert!(!e.to_string().contains(SECRET), "{e}");
            assert!(!format!("{e:?}").contains(SECRET));
        }
        let e = run("exit 3").await.unwrap_err();
        assert_eq!(e, HelperError::Exit("exit status 3".into()));
    }

    #[test]
    fn parse_validates() {
        assert_eq!(parse(b"{}").unwrap().len(), 0);
        assert_eq!(parse(b"[]").unwrap_err(), HelperError::BadJson);
        assert_eq!(parse(br#"{"a":null}"#).unwrap_err(), HelperError::BadJson);
        assert_eq!(parse(br#"{"a b":"v"}"#).unwrap_err(), HelperError::BadName);
        assert_eq!(
            parse(b"{\"a\":\"line\\nbreak\"}").unwrap_err(),
            HelperError::BadValue
        );
        let many: serde_json::Map<String, Value> = (0..=MAX_HELPER_HEADERS)
            .map(|i| (format!("x-{i}"), Value::from("v")))
            .collect();
        let text = serde_json::to_vec(&many).unwrap();
        assert_eq!(parse(&text).unwrap_err(), HelperError::TooMany);
    }

    #[test]
    fn helper_overrides_static() {
        let base = parse(br#"{"Authorization":"static","X-Keep":"k"}"#).unwrap();
        let over = parse(br#"{"authorization":"helper"}"#).unwrap();
        let m = merge(&base, over);
        assert_eq!(m.len(), 2);
        let auth: Vec<_> = m.iter().filter(|(k, _)| k == "authorization").collect();
        assert_eq!(auth.len(), 1);
        assert_eq!(auth[0].1.to_str().unwrap(), "helper");
        assert!(m.iter().any(|(k, _)| k == "x-keep"));
    }
}
