// SPDX-License-Identifier: GPL-3.0-or-later

//! Admin API client: finds the broker (URL and token file) the same way
//! `serve` does and wraps the `/admin/v1` endpoints. The token is held in a
//! type that never prints it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use mcpdoze_core::admin::api::{
    ErrorResponse, LogsResponse, RegisterRequest, RegisterResponse, ReloadResponse, RemoveResponse,
    ServersResponse, StatusResponse, StopResponse, BASE,
};
use mcpdoze_core::config::{self, Config};
use mcpdoze_core::daemon::DEFAULT_TOKEN_FILE;
use serde::de::DeserializeOwned;

use crate::cli::ConnArgs;

/// Default broker address when nothing else is known.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:7878";
/// Largest response body read from the broker.
const MAX_BODY: usize = 4 * 1024 * 1024;
/// Largest token file read.
const MAX_TOKEN_FILE: u64 = 4096;

/// Where the bearer token comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// A token file.
    File(PathBuf),
    /// The environment variable named by `broker.token_env`.
    Env(String),
}

/// Resolved connection settings (no secrets).
#[derive(Debug)]
pub struct Conn {
    /// Broker base URL without trailing slash.
    pub base: String,
    /// Where the token lives.
    pub token: TokenSource,
    /// Config file path, when one is known.
    pub config_path: Option<PathBuf>,
    /// Parse result of the config file (`None` when it does not exist).
    pub config: Option<Result<Config, String>>,
}

/// Keeps only the lines of a TOML error that do not quote file content.
pub fn sanitize_parse_error(msg: &str) -> String {
    let lines: Vec<&str> = msg
        .lines()
        .filter(|l| !l.contains('|') && !l.trim().is_empty())
        .collect();
    lines.join("; ")
}

fn normalize_base(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix("/mcp").unwrap_or(u);
    u.trim_end_matches('/').to_owned()
}

fn base_from_bind(bind: &str) -> String {
    let (host, port) = bind.rsplit_once(':').unwrap_or((bind, "7878"));
    let host = match host {
        "0.0.0.0" | "" | "[::]" | "::" => "127.0.0.1",
        h => h,
    };
    format!("http://{host}:{port}")
}

impl Conn {
    /// Resolves options, environment (already merged by clap) and config.
    pub fn resolve(args: &ConnArgs) -> Conn {
        let config_path = args.config.clone().or_else(config::default_config_path);
        let config = config_path.as_deref().and_then(|p| match Config::load(p) {
            Ok(c) => Some(Ok(c)),
            Err(mcpdoze_core::config::ConfigError::Io(e))
                if e.kind() == std::io::ErrorKind::NotFound =>
            {
                None
            }
            Err(e) => Some(Err(sanitize_parse_error(&e.to_string()))),
        });
        let broker = config
            .as_ref()
            .and_then(|c| c.as_ref().ok())
            .map(|c| &c.broker);
        let base = match (&args.url, broker) {
            (Some(u), _) => normalize_base(u),
            (None, Some(b)) => base_from_bind(&b.bind),
            _ => DEFAULT_BASE_URL.to_owned(),
        };
        let token = if let Some(p) = &args.token_file {
            TokenSource::File(p.clone())
        } else if let Some(var) = broker.and_then(|b| b.token_env.clone()) {
            TokenSource::Env(var)
        } else {
            let rel = broker
                .and_then(|b| b.token_file.clone())
                .unwrap_or_else(|| PathBuf::from(DEFAULT_TOKEN_FILE));
            let path = if rel.is_absolute() {
                rel
            } else {
                match config::default_data_dir() {
                    Some(d) => d.join(rel),
                    None => rel,
                }
            };
            TokenSource::File(path)
        };
        Conn {
            base,
            token,
            config_path,
            config,
        }
    }

    /// Reads the token (never printed by this module).
    pub fn read_token(&self) -> Result<String> {
        match &self.token {
            TokenSource::Env(var) => {
                let v = std::env::var(var).map_err(|_| {
                    anyhow!("environment variable {var} (broker.token_env) is not set")
                })?;
                clean_token(v)
            }
            TokenSource::File(p) => read_token_file(p),
        }
    }
}

fn clean_token(v: String) -> Result<String> {
    let t = v.trim().to_owned();
    if t.is_empty() || t.chars().any(char::is_whitespace) {
        bail!("the token is empty or contains whitespace");
    }
    Ok(t)
}

/// Reads a token file (size-capped).
pub fn read_token_file(p: &Path) -> Result<String> {
    use std::io::Read;
    let f = std::fs::File::open(p).with_context(|| {
        format!(
            "cannot read the token file {} (start the broker once with `mcpdoze serve` to create it, or pass --token-file)",
            p.display()
        )
    })?;
    let mut s = String::new();
    f.take(MAX_TOKEN_FILE)
        .read_to_string(&mut s)
        .with_context(|| format!("cannot read the token file {}", p.display()))?;
    clean_token(s).with_context(|| format!("token file {}", p.display()))
}

/// HTTP client for the admin API.
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// Outcome of a request that failed before or at the HTTP level.
fn not_running(base: &str, e: &reqwest::Error) -> anyhow::Error {
    if e.is_connect() || e.is_timeout() {
        anyhow!(
            "cannot reach the broker at {base}; is it running? Start it with `mcpdoze serve`, or set it up to start at login with `mcpdoze autostart install`"
        )
    } else {
        anyhow!(
            "request to {base} failed: {}",
            e.to_string().split(" for url").next().unwrap_or_default()
        )
    }
}

impl Client {
    /// Builds a client for `conn`, reading the token.
    pub fn new(conn: &Conn) -> Result<Client> {
        let token = conn.read_token()?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(40))
            .build()
            .context("cannot create the HTTP client")?;
        Ok(Client {
            http,
            base: conn.base.clone(),
            token,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{BASE}{path}", self.base)
    }

    async fn read_body(&self, mut resp: reqwest::Response) -> Result<(u16, Vec<u8>)> {
        let status = resp.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| not_running(&self.base, &e))?
        {
            if body.len() + chunk.len() > MAX_BODY {
                bail!("response from the broker is too large");
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<(u16, Vec<u8>)> {
        let resp = req
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| not_running(&self.base, &e))?;
        let (status, body) = self.read_body(resp).await?;
        if (200..300).contains(&status) {
            return Ok((status, body));
        }
        Err(api_error(status, &body))
    }

    async fn json<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T> {
        let (_, body) = self.send(req).await?;
        serde_json::from_slice(&body).context("unexpected response from the broker")
    }

    /// `GET /status`.
    pub async fn status(&self) -> Result<StatusResponse> {
        self.json(self.http.get(self.url("/status"))).await
    }

    /// `GET /servers`.
    pub async fn servers(&self) -> Result<ServersResponse> {
        self.json(self.http.get(self.url("/servers"))).await
    }

    /// `POST /servers`.
    pub async fn register(&self, req: &RegisterRequest) -> Result<RegisterResponse> {
        self.json(self.http.post(self.url("/servers")).json(req))
            .await
    }

    /// `DELETE /servers/{name}`.
    pub async fn remove(&self, name: &str) -> Result<RemoveResponse> {
        self.json(
            self.http
                .delete(self.url(&format!("/servers/{}", enc(name)))),
        )
        .await
    }

    /// `POST /servers/{name}/stop`.
    pub async fn stop(&self, name: &str, force: bool) -> Result<StopResponse> {
        let url = self.url(&format!("/servers/{}/stop", enc(name)));
        let mut req = self.http.post(url);
        if force {
            req = req.query(&[("force", "true")]);
        }
        self.json(req).await
    }

    /// `POST /reload`.
    pub async fn reload(&self) -> Result<ReloadResponse> {
        self.json(self.http.post(self.url("/reload"))).await
    }

    /// `GET /servers/{name}/logs`.
    pub async fn logs(&self, name: &str, lines: usize) -> Result<LogsResponse> {
        let url = self.url(&format!("/servers/{}/logs", enc(name)));
        self.json(self.http.get(url).query(&[("lines", lines)]))
            .await
    }

    /// `POST /servers/{name}/refresh` or `POST /refresh`; the response shape
    /// is returned as loosely typed JSON.
    pub async fn refresh(&self, name: Option<&str>) -> Result<serde_json::Value> {
        let path = match name {
            Some(n) => format!("/servers/{}/refresh", enc(n)),
            None => "/refresh".to_owned(),
        };
        let (_, body) = self
            .send(self.http.post(self.url(&path)))
            .await
            .map_err(|e| {
                let m = e.to_string();
                if m.starts_with("HTTP 404") {
                    anyhow!("this broker does not support refresh yet (upgrade mcpdoze); {m}")
                } else {
                    e
                }
            })?;
        Ok(serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null))
    }

    /// `GET /healthz` (no token). Returns whether the broker answered `ok`.
    pub async fn healthz(&self) -> Result<bool> {
        let resp = self
            .http
            .get(format!("{}/healthz", self.base))
            .send()
            .await
            .map_err(|e| not_running(&self.base, &e))?;
        let (status, body) = self.read_body(resp).await?;
        Ok(status == 200 && String::from_utf8_lossy(&body).trim() == "ok")
    }
}

/// Percent-encodes a path segment.
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Turns a non-2xx response into a readable error.
fn api_error(status: u16, body: &[u8]) -> anyhow::Error {
    if status == 401 {
        return anyhow!(
            "the broker rejected the token (HTTP 401); check --token-file or the broker's config"
        );
    }
    match serde_json::from_slice::<ErrorResponse>(body) {
        Ok(e) if e.details.is_empty() => anyhow!("HTTP {status}: {}", e.error),
        Ok(e) => anyhow!("HTTP {status}: {}\n  {}", e.error, e.details.join("\n  ")),
        Err(_) => anyhow!("HTTP {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_normalisation() {
        assert_eq!(
            normalize_base("http://127.0.0.1:7878/"),
            "http://127.0.0.1:7878"
        );
        assert_eq!(normalize_base("http://h:1/mcp"), "http://h:1");
        assert_eq!(base_from_bind("0.0.0.0:9"), "http://127.0.0.1:9");
        assert_eq!(base_from_bind("[::1]:9"), "http://[::1]:9");
    }

    #[test]
    fn path_encoding() {
        assert_eq!(enc("a.b-c_d"), "a.b-c_d");
        assert_eq!(enc("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn parse_errors_drop_quoted_source() {
        let m =
            "TOML parse error at line 2, column 1\n  |\n2 | secret = \"x\"\n  | ^\nunknown field";
        let s = sanitize_parse_error(m);
        assert!(!s.contains("secret"));
        assert!(s.contains("line 2"));
    }

    #[test]
    fn api_errors() {
        let e = api_error(401, b"");
        assert!(e.to_string().contains("rejected the token"));
        let e = api_error(400, br#"{"error":"bad","details":["a: b"]}"#);
        assert!(e.to_string().contains("a: b"));
        assert_eq!(api_error(404, b"").to_string(), "HTTP 404");
    }
}
