// SPDX-License-Identifier: GPL-3.0-or-later
//! Configuration types, validation, environment resolution and tool-name
//! namespacing.
//!
//! # Namespacing and truncation
//!
//! Tools are exposed as `<prefix>__<tool>`. Prefixes are sanitised to
//! `[a-z0-9_]`, never contain `__`, never start or end with `_` and are at most
//! [`MAX_PREFIX_LEN`] characters, so the *first* `__` in an exposed name always
//! separates prefix from tool. When the full name exceeds [`MAX_TOOL_NAME_LEN`]
//! the tool part is cut and a `_` plus 8 hex digits of a stable FNV-1a hash of
//! the full untruncated name is appended. A truncated tool part cannot be
//! recovered from the exposed name alone, so [`split_namespaced`] returns the
//! (correct) prefix with the (possibly truncated) tool part, and [`NameMap`]
//! keeps the exact reverse table built from the catalog. Routers should use
//! [`NameMap::resolve`], which is exact for every registered tool and rejects
//! hash collisions at insert time.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

/// Maximum length of an exposed tool name.
pub const MAX_TOOL_NAME_LEN: usize = 64;
/// Maximum length of a server prefix.
pub const MAX_PREFIX_LEN: usize = 32;
const SEP: &str = "__";
const DEFAULT_BIND: &str = "127.0.0.1:7878";
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

/// Errors loading a config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config file is larger than {MAX_CONFIG_BYTES} bytes")]
    TooLarge,
}

/// One validation problem, located by a field path such as `servers.foo.command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl ValidationError {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ValidationError {}

/// Failure resolving an environment reference. Never carries a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("environment variable {var} (needed for {key}) is not set")]
    Missing { var: String, key: String },
    #[error("environment variable {var} (needed for {key}) is not valid unicode")]
    NotUnicode { var: String, key: String },
}

/// Top-level configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub broker: BrokerConfig,
    #[serde(default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

/// Broker settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    pub token_file: Option<PathBuf>,
    pub token_env: Option<String>,
    /// Explicit opt-in for a non-loopback bind.
    #[serde(default)]
    pub allow_remote: bool,
    #[serde(default = "default_idle_ttl", deserialize_with = "de_duration")]
    pub default_idle_ttl: Duration,
    #[serde(default = "default_start_timeout", deserialize_with = "de_duration")]
    pub start_timeout: Duration,
    pub log_dir: Option<PathBuf>,
    /// Extra `Host` / `Origin` host names accepted besides `localhost`,
    /// `127.0.0.1` and `[::1]` (needed for non-loopback binds).
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            token_file: None,
            token_env: None,
            allow_remote: false,
            default_idle_ttl: default_idle_ttl(),
            start_timeout: default_start_timeout(),
            log_dir: None,
            allowed_hosts: Vec::new(),
        }
    }
}

fn default_bind() -> String {
    DEFAULT_BIND.to_string()
}
fn default_idle_ttl() -> Duration {
    Duration::from_secs(600)
}
fn default_start_timeout() -> Duration {
    Duration::from_secs(60)
}
fn default_true() -> bool {
    true
}

/// Backend transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Stdio,
    Http,
}

/// Backend instance scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    #[default]
    Shared,
    PerCwd,
    PerSession,
}

/// A literal value or a reference to the process environment.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum EnvValue {
    Literal(String),
    Ref {
        #[serde(deserialize_with = "de_deny_empty")]
        from_env: String,
    },
}

// Debug must not print literal values (they might be secrets).
impl fmt::Debug for EnvValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvValue::Literal(_) => f.write_str("Literal(<hidden>)"),
            EnvValue::Ref { from_env } => write!(f, "Ref({from_env})"),
        }
    }
}

fn de_deny_empty<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    if s.is_empty() {
        Err(de::Error::custom("from_env must not be empty"))
    } else {
        Ok(s)
    }
}

/// One backend server. Serializes to the same shape it is read from
/// (defaults omitted), so it round-trips through TOML and JSON.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, EnvValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "is_default_scope")]
    pub scope: Scope,
    #[serde(
        default,
        deserialize_with = "de_opt_duration",
        serialize_with = "ser_opt_duration",
        skip_serializing_if = "Option::is_none"
    )]
    pub idle_ttl: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, EnvValue>,
    /// Permit secret-looking literal values in `env` / `headers`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_literal_env: bool,
}

fn is_default_scope(s: &Scope) -> bool {
    *s == Scope::Shared
}
fn is_true(b: &bool) -> bool {
    *b
}
fn is_false(b: &bool) -> bool {
    !*b
}

impl ServerConfig {
    /// The effective prefix for server `name`: the explicit prefix, else the
    /// sanitised name.
    pub fn effective_prefix(&self, name: &str) -> String {
        match &self.prefix {
            Some(p) => p.clone(),
            None => sanitize_prefix(name),
        }
    }

    /// Idle TTL, falling back to the broker default.
    pub fn idle_ttl_or(&self, broker: &BrokerConfig) -> Duration {
        self.idle_ttl.unwrap_or(broker.default_idle_ttl)
    }
}

impl Config {
    /// Read and parse a config file (does not validate).
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        let mut text = String::new();
        file.take(MAX_CONFIG_BYTES + 1).read_to_string(&mut text)?;
        if text.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        Self::parse(&text)
    }

    /// Parse config text (does not validate).
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// Check the whole config and return every problem found.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errs = Vec::new();
        self.validate_broker(&mut errs);
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for (name, s) in &self.servers {
            let f = |k: &str| format!("servers.{name}.{k}");
            if sanitize_prefix(name).is_empty() && s.prefix.is_none() {
                errs.push(ValidationError::new(
                    format!("servers.{name}"),
                    "name has no usable characters for a prefix; set `prefix`",
                ));
            }
            let prefix = s.effective_prefix(name);
            match check_prefix(&prefix) {
                Err(m) => errs.push(ValidationError::new(f("prefix"), m)),
                Ok(()) => {
                    if let Some(other) = seen.insert(prefix.clone(), name.clone()) {
                        errs.push(ValidationError::new(
                            f("prefix"),
                            format!("prefix `{prefix}` is already used by server `{other}`"),
                        ));
                    }
                }
            }
            match s.transport {
                Transport::Stdio => {
                    match s.command.as_deref() {
                        None => errs.push(ValidationError::new(
                            f("command"),
                            "stdio transport requires `command`",
                        )),
                        Some(c) if c.trim().is_empty() => errs.push(ValidationError::new(
                            f("command"),
                            "`command` must not be empty",
                        )),
                        _ => {}
                    }
                    if s.url.is_some() {
                        errs.push(ValidationError::new(f("url"), "`url` is only for http"));
                    }
                    if !s.headers.is_empty() {
                        errs.push(ValidationError::new(
                            f("headers"),
                            "`headers` is only for http",
                        ));
                    }
                }
                Transport::Http => {
                    match s.url.as_deref() {
                        None => errs.push(ValidationError::new(
                            f("url"),
                            "http transport requires `url`",
                        )),
                        Some(u) if !valid_http_url(u) => errs.push(ValidationError::new(
                            f("url"),
                            "`url` must be an http:// or https:// URL with a host",
                        )),
                        _ => {}
                    }
                    if s.command.is_some() {
                        errs.push(ValidationError::new(
                            f("command"),
                            "`command` is only for stdio",
                        ));
                    }
                }
            }
            if s.idle_ttl == Some(Duration::ZERO) {
                errs.push(ValidationError::new(
                    f("idle_ttl"),
                    "must be greater than 0",
                ));
            }
            if !s.allow_literal_env {
                for (k, v) in &s.env {
                    if let EnvValue::Literal(val) = v {
                        if let Some(why) = secret_like(k, val, false) {
                            errs.push(ValidationError::new(
                                format!("servers.{name}.env.{k}"),
                                format!(
                                    "literal value looks like a secret ({why}); use \
                                     {{ from_env = \"NAME\" }} or set allow_literal_env = true"
                                ),
                            ));
                        }
                    }
                }
                for (k, v) in &s.headers {
                    if let EnvValue::Literal(val) = v {
                        if let Some(why) = secret_like(k, val, true) {
                            errs.push(ValidationError::new(
                                format!("servers.{name}.headers.{k}"),
                                format!(
                                    "literal value looks like a secret ({why}); use \
                                     {{ from_env = \"NAME\" }} or set allow_literal_env = true"
                                ),
                            ));
                        }
                    }
                }
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    fn validate_broker(&self, errs: &mut Vec<ValidationError>) {
        let b = &self.broker;
        if b.default_idle_ttl.is_zero() {
            errs.push(ValidationError::new(
                "broker.default_idle_ttl",
                "must be greater than 0",
            ));
        }
        if b.start_timeout.is_zero() {
            errs.push(ValidationError::new(
                "broker.start_timeout",
                "must be greater than 0",
            ));
        }
        if b.token_file.is_some() && b.token_env.is_some() {
            errs.push(ValidationError::new(
                "broker.token_env",
                "set only one of token_file and token_env",
            ));
        }
        for (i, h) in b.allowed_hosts.iter().enumerate() {
            let bad = h.is_empty()
                || h.chars().any(|c| {
                    c.is_whitespace() || c.is_control() || matches!(c, '/' | '@' | '\\' | '*')
                })
                || (h.contains(':') && !(h.starts_with('[') && h.ends_with(']')));
            if bad {
                errs.push(ValidationError::new(
                    format!("broker.allowed_hosts[{i}]"),
                    "must be a host name or IP without port or wildcard (IPv6 in brackets)",
                ));
            }
        }
        match bind_is_loopback(&b.bind) {
            None => errs.push(ValidationError::new(
                "broker.bind",
                "must be `host:port` with an IP address or `localhost`",
            )),
            Some(true) => {}
            Some(false) => {
                if !b.allow_remote {
                    errs.push(ValidationError::new(
                        "broker.allow_remote",
                        "non-loopback bind requires allow_remote = true",
                    ));
                }
                if b.token_file.is_none() && b.token_env.is_none() {
                    errs.push(ValidationError::new(
                        "broker.token_file",
                        "non-loopback bind requires token_file or token_env",
                    ));
                }
            }
        }
    }

    /// Prefix -> server name table for enabled servers (after validation).
    pub fn prefixes(&self) -> BTreeMap<String, String> {
        self.servers
            .iter()
            .map(|(n, s)| (s.effective_prefix(n), n.clone()))
            .collect()
    }
}

/// `Some(true)` loopback, `Some(false)` non-loopback, `None` unparsable.
fn bind_is_loopback(bind: &str) -> Option<bool> {
    if let Ok(a) = bind.parse::<std::net::SocketAddr>() {
        return Some(a.ip().is_loopback());
    }
    let port = bind.strip_prefix("localhost:")?;
    port.parse::<u16>().ok().map(|_| true)
}

fn valid_http_url(u: &str) -> bool {
    let rest = match u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
    {
        Some(r) => r,
        None => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    !host.is_empty()
        && !host.starts_with(':')
        && !u.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Resolve `env` into concrete values from the process environment.
pub fn resolve_env(server: &ServerConfig) -> Result<BTreeMap<String, String>, ResolveError> {
    resolve_map(&server.env, |v| std::env::var_os(v))
}

/// Resolve `headers` into concrete values from the process environment.
pub fn resolve_headers(server: &ServerConfig) -> Result<BTreeMap<String, String>, ResolveError> {
    resolve_map(&server.headers, |v| std::env::var_os(v))
}

/// Resolution against an arbitrary lookup (used by tests).
pub fn resolve_map(
    map: &BTreeMap<String, EnvValue>,
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<BTreeMap<String, String>, ResolveError> {
    let mut out = BTreeMap::new();
    for (k, v) in map {
        let val = match v {
            EnvValue::Literal(s) => s.clone(),
            EnvValue::Ref { from_env } => match lookup(from_env) {
                None => {
                    return Err(ResolveError::Missing {
                        var: from_env.clone(),
                        key: k.clone(),
                    })
                }
                Some(o) => o.into_string().map_err(|_| ResolveError::NotUnicode {
                    var: from_env.clone(),
                    key: k.clone(),
                })?,
            },
        };
        out.insert(k.clone(), val);
    }
    Ok(out)
}

impl ServerConfig {
    /// Method form of [`resolve_env`].
    pub fn resolve_env(&self) -> Result<BTreeMap<String, String>, ResolveError> {
        resolve_env(self)
    }
}

// ---------------------------------------------------------------- secrets

const SECRET_KEY_WORDS: [&str; 4] = ["KEY", "TOKEN", "SECRET", "PASSWORD"];
const SECRET_VALUE_PREFIXES: [&str; 14] = [
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xapp-",
    "AKIA",
    "AIza",
    "npm_",
    "Bearer ",
];

/// Why a literal looks like a secret, if it does. Never includes the value.
fn secret_like(key: &str, value: &str, header: bool) -> Option<&'static str> {
    if value.is_empty() {
        return None;
    }
    let upper = key.to_ascii_uppercase();
    if SECRET_KEY_WORDS.iter().any(|w| upper.contains(w)) {
        return Some("secret-like name");
    }
    if header && (upper.contains("AUTHORIZATION") || upper.contains("COOKIE")) {
        return Some("credential header");
    }
    if SECRET_VALUE_PREFIXES.iter().any(|p| value.starts_with(p)) {
        return Some("known token prefix");
    }
    let v = value.trim();
    if v.starts_with("eyJ") && v.matches('.').count() == 2 {
        return Some("JWT-like value");
    }
    if v.starts_with("-----BEGIN ") && v.contains("PRIVATE KEY") {
        return Some("private key");
    }
    None
}

// ------------------------------------------------------------------ paths

/// Config file path: `MCPDOZE_CONFIG` if set, else `<config dir>/config.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    config_path_from(std::env::var_os("MCPDOZE_CONFIG"))
}

fn config_path_from(over: Option<std::ffi::OsString>) -> Option<PathBuf> {
    match over {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => project_dirs().map(|d| d.config_dir().join("config.toml")),
    }
}

/// Platform data directory (catalog, logs, token).
pub fn default_data_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.data_dir().to_path_buf())
}

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "mcpdoze")
}

// --------------------------------------------------------------- durations

/// Parse `"30s"`, `"5m"`, `"1h"`, `"500ms"`, `"1d"` and sums such as `"1h30m"`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let mut total = Duration::ZERO;
    let mut rest = s;
    while !rest.is_empty() {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return Err(format!("invalid duration `{s}`"));
        }
        let n: u64 = rest[..digits]
            .parse()
            .map_err(|_| format!("number too large in `{s}`"))?;
        rest = &rest[digits..];
        let unit_len = rest.chars().take_while(char::is_ascii_alphabetic).count();
        let unit = &rest[..unit_len];
        rest = rest[unit_len..].trim_start();
        let part = match unit {
            "ms" => Duration::from_millis(n),
            "s" | "sec" => Duration::from_secs(n),
            "m" | "min" => Duration::from_secs(n.checked_mul(60).ok_or("overflow")?),
            "h" | "hr" => Duration::from_secs(n.checked_mul(3600).ok_or("overflow")?),
            "d" => Duration::from_secs(n.checked_mul(86_400).ok_or("overflow")?),
            _ => {
                return Err(format!(
                    "unknown unit `{unit}` in `{s}` (use ms, s, m, h, d)"
                ))
            }
        };
        total = total
            .checked_add(part)
            .ok_or_else(|| "duration overflow".to_string())?;
    }
    Ok(total)
}

fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let s = String::deserialize(d)?;
    parse_duration(&s).map_err(de::Error::custom)
}

/// Formats a duration in the config syntax (`"500ms"`, `"30s"`, `"5m"`,
/// `"2h"`); [`parse_duration`] reads it back exactly.
pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if !ms.is_multiple_of(1000) {
        return format!("{ms}ms");
    }
    let s = d.as_secs();
    if s != 0 && s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s != 0 && s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

fn ser_opt_duration<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
    match d {
        Some(d) => s.serialize_str(&format_duration(*d)),
        None => s.serialize_none(),
    }
}

fn de_opt_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|s| parse_duration(&s).map_err(de::Error::custom))
        .transpose()
}

// ------------------------------------------------------------- namespacing

/// Map a server name to a prefix: lowercase, non `[a-z0-9]` runs become a
/// single `_`, leading/trailing `_` trimmed, cut to [`MAX_PREFIX_LEN`].
pub fn sanitize_prefix(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let t = out.trim_matches('_');
    let t = &t[..t.len().min(MAX_PREFIX_LEN)];
    t.trim_end_matches('_').to_string()
}

fn check_prefix(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Err("prefix must not be empty".into());
    }
    if p.len() > MAX_PREFIX_LEN {
        return Err(format!("prefix longer than {MAX_PREFIX_LEN} characters"));
    }
    if !p
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("prefix may only contain [a-z0-9_]".into());
    }
    if p.contains(SEP) {
        return Err("prefix must not contain `__`".into());
    }
    if p.starts_with('_') || p.ends_with('_') {
        return Err("prefix must not start or end with `_`".into());
    }
    Ok(())
}

fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Exposed name `prefix__tool`, truncated with a stable hash suffix
/// (`_` + 8 hex digits of FNV-1a over the full name) if longer than
/// [`MAX_TOOL_NAME_LEN`]. Total length never exceeds the cap as long as the
/// prefix is valid (at most [`MAX_PREFIX_LEN`]).
pub fn namespaced_tool_name(prefix: &str, tool: &str) -> String {
    let full = format!("{prefix}{SEP}{tool}");
    if full.len() <= MAX_TOOL_NAME_LEN {
        return full;
    }
    let suffix = format!("_{:08x}", fnv1a64(full.as_bytes()) as u32);
    let mut cut = MAX_TOOL_NAME_LEN
        .saturating_sub(suffix.len())
        .max(prefix.len() + SEP.len());
    cut = cut.min(full.len());
    while !full.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{}", &full[..cut], suffix)
}

/// Split an exposed name at the first `__`. The prefix is always exact; the
/// tool part is the exact tool name unless the name was truncated, in which
/// case use [`NameMap::resolve`]. Returns `None` if either side is empty.
pub fn split_namespaced(name: &str) -> Option<(&str, &str)> {
    let (p, t) = name.split_once(SEP)?;
    if p.is_empty() || t.is_empty() {
        None
    } else {
        Some((p, t))
    }
}

/// Exact reverse table from exposed names to `(prefix, original tool)`.
#[derive(Debug, Default, Clone)]
pub struct NameMap {
    map: HashMap<String, (String, String)>,
}

/// Two different tools mapped to the same exposed name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("exposed name `{exposed}` collides for tools `{a}` and `{b}`")]
pub struct NameCollision {
    pub exposed: String,
    pub a: String,
    pub b: String,
}

impl NameMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool and return its exposed name. Re-inserting the same
    /// tool is idempotent; a different tool with the same exposed name errors.
    pub fn insert(&mut self, prefix: &str, tool: &str) -> Result<String, NameCollision> {
        let exposed = namespaced_tool_name(prefix, tool);
        if let Some((p, t)) = self.map.get(&exposed) {
            if p != prefix || t != tool {
                return Err(NameCollision {
                    exposed,
                    a: format!("{p}/{t}"),
                    b: format!("{prefix}/{tool}"),
                });
            }
        } else {
            self.map
                .insert(exposed.clone(), (prefix.to_string(), tool.to_string()));
        }
        Ok(exposed)
    }

    /// Drop every tool of a prefix (e.g. before re-registering a catalog).
    pub fn remove_prefix(&mut self, prefix: &str) {
        self.map.retain(|_, (p, _)| p != prefix);
    }

    /// Exact `(prefix, tool)` for an exposed name.
    pub fn resolve(&self, exposed: &str) -> Option<(&str, &str)> {
        self.map.get(exposed).map(|(p, t)| (p.as_str(), t.as_str()))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests;
