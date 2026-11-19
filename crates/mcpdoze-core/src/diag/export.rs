// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdoze debug export`: turns the failure log into one pasteable
//! markdown text.
//!
//! The records on disk are sanitised by construction (`record.rs`). The
//! export adds three more layers, because the text is meant for a public
//! issue:
//!
//! 1. a second scrub pass over the only free-text fields (`error.message`,
//!    `stderr.tail`, `cause.hints`) with the scrubber, using the exporting
//!    machine's home, user and host names;
//! 2. the salted hashes (`instance`, `http.url.host`, `k#<hash8>`,
//!    `<host#..>`) are re-keyed with a fresh random salt, so the output
//!    cannot be linked to the install's salt file;
//! 3. a self-check over the finished text with the rule set of
//!    `scripts/pii-lint.ps1` (profile paths, emails, token prefixes, private
//!    IPs) plus JWTs, bearer values, long hex runs and this machine's user,
//!    host and home names. Any hit fails closed ([`ExportError::SelfCheck`]);
//!    the findings name the rule and line, never the matched text.
//!
//! `strict` drops all free text (error messages, stderr tails, cause hints).
//! The launch shape lists names only: launcher basename, flag names,
//! argument count, environment and header names; the package argument only
//! with `identify`.
//!
//! The functions are pure over their inputs (the CLI reads the files and the
//! admin API), so they work with the broker down.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{json, Map, Value};

use super::record::Name;
use super::redact::{launcher_basename, Salt, ScrubContext, Scrubber};
use super::shape::ArgSet;
use crate::config::{EnvValue, ServerConfig, Transport};
use crate::logfile::rotated_path;
use crate::stats::Summary;

/// Most records one export carries (the newest).
pub const MAX_RECORDS: usize = 100;
/// Export format version.
pub const FORMAT: u32 = 1;

// ------------------------------------------------------------- reading

/// Which records [`read_records`] returns.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Only this server.
    pub server: Option<String>,
    /// Only this kind (snake case).
    pub kind: Option<String>,
    /// Only records at or after this unix time in ms.
    pub since_ms: u64,
}

impl Filter {
    fn matches(&self, rec: &Value) -> bool {
        self.server
            .as_deref()
            .is_none_or(|s| rec["server"].as_str() == Some(s))
            && self
                .kind
                .as_deref()
                .is_none_or(|k| rec["kind"].as_str() == Some(k))
            && rec["ts"].as_u64().unwrap_or(0) >= self.since_ms
    }
}

/// Parses a `--since` value: a duration ago (`24h`) or unix ms.
pub fn parse_since(s: &str, now_ms: u64) -> Result<u64, String> {
    let s = s.trim();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse::<u64>().map_err(|e| e.to_string());
    }
    let d = crate::config::parse_duration(s)?;
    Ok(now_ms.saturating_sub(d.as_millis() as u64))
}

/// The newest `limit` matching records of `file` and its `keep` rotated
/// files, oldest first. Repeat lines (`{"of": id, "repeat": n}`) are folded
/// into their record. Reads the files directly, so it works with the broker
/// down. Blocking.
pub fn read_records(file: &Path, keep: usize, f: &Filter, limit: usize) -> Vec<Value> {
    let limit = limit.max(1);
    let mut recs: VecDeque<Value> = VecDeque::new();
    let mut repeats: BTreeMap<String, u64> = BTreeMap::new();
    let paths = (1..=keep)
        .rev()
        .map(|n| rotated_path(file, n))
        .chain(std::iter::once(file.to_path_buf()));
    for p in paths {
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            let Ok(rec) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(of) = rec["of"].as_str() {
                if let Some(n) = rec["repeat"].as_u64() {
                    let e = repeats.entry(of.to_owned()).or_insert(0);
                    *e = (*e).max(n);
                }
                continue;
            }
            if rec["id"].is_string() && f.matches(&rec) {
                recs.push_back(rec);
                if recs.len() > limit {
                    recs.pop_front();
                }
            }
        }
    }
    let mut out: Vec<Value> = recs.into();
    for r in &mut out {
        let n = r["id"].as_str().and_then(|id| repeats.get(id)).copied();
        if let Some(n) = n {
            if n > r["repeat"].as_u64().unwrap_or(1) {
                r["repeat"] = n.into();
            }
        }
    }
    out
}

// -------------------------------------------------------- launch shape

/// What the export says about how a server is launched: names and counts,
/// never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchShape {
    /// Server name.
    pub server: String,
    /// `stdio` or `http`.
    pub transport: &'static str,
    /// Launcher basename (stdio).
    pub launcher: Option<String>,
    /// Names of the arguments that start with `-` (the part before `=`).
    pub flags: Vec<String>,
    /// Number of arguments.
    pub arg_count: usize,
    /// Environment variable names.
    pub env_names: Vec<String>,
    /// Header names.
    pub header_names: Vec<String>,
    /// A `headers_helper` is configured.
    pub headers_helper: bool,
    /// A working directory is configured.
    pub cwd_set: bool,
    /// The package argument of a package launcher; exported only with
    /// `identify`.
    pub package: Option<String>,
}

const PACKAGE_LAUNCHERS: &[&str] = &[
    "npx", "bunx", "pnpx", "uvx", "pipx", "npm", "pnpm", "yarn", "bun", "deno", "uv", "dnx",
];
const SUBCOMMANDS: &[&str] = &["run", "dlx", "exec", "x", "tool", "start", "mcp"];

fn is_flag_name(s: &str) -> bool {
    let body = s.trim_start_matches('-');
    let dashes = s.len() - body.len();
    // A single dash takes one letter: `-p` is a flag, `-ptoken` is a flag
    // with its value attached.
    (dashes == 2 || (dashes == 1 && body.len() == 1))
        && body.len() <= 40
        && body.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_package_arg(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with(['.', '/', '~', '-'])
        && !s.contains("//")
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/@[],~^".contains(c))
}

impl LaunchShape {
    /// Builds the shape of the server `name` from its configuration.
    pub fn from_config(name: &str, cfg: &ServerConfig) -> LaunchShape {
        let stdio = cfg.transport == Transport::Stdio;
        let launcher = stdio
            .then(|| cfg.command.as_deref().map(launcher_basename))
            .flatten()
            .map(|n| n.as_str().to_owned());
        let mut flags: Vec<String> = Vec::new();
        let mut package = None;
        if stdio {
            for a in &cfg.args {
                if a.starts_with('-') {
                    let head = a.split('=').next().unwrap_or(a);
                    let f = if is_flag_name(head) { head } else { "-?" };
                    if !flags.iter().any(|x| x == f) {
                        flags.push(f.to_owned());
                    }
                }
            }
            if launcher
                .as_deref()
                .is_some_and(|l| PACKAGE_LAUNCHERS.contains(&l))
            {
                package = cfg
                    .args
                    .iter()
                    .filter(|a| !a.starts_with('-'))
                    .find(|a| !SUBCOMMANDS.contains(&a.as_str()))
                    .filter(|a| is_package_arg(a))
                    .cloned();
            }
        }
        let names = |m: &BTreeMap<String, EnvValue>| -> Vec<String> {
            m.keys().map(|k| Name::new(k).as_str().to_owned()).collect()
        };
        LaunchShape {
            server: Name::new(name).as_str().to_owned(),
            transport: if stdio { "stdio" } else { "http" },
            launcher,
            flags,
            arg_count: cfg.args.len(),
            env_names: names(&cfg.env),
            header_names: names(&cfg.headers),
            headers_helper: cfg.headers_helper.is_some(),
            cwd_set: cfg.cwd.is_some(),
            package,
        }
    }
}

// ------------------------------------------------------------- self-check

/// A self-check hit: which rule, on which line of the export. Never the
/// matched text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Rule name.
    pub rule: &'static str,
    /// 1-based line of the export text.
    pub line: usize,
}

/// This machine's identity for the self-check.
#[derive(Debug, Clone, Default)]
pub struct LintContext {
    secrets: Vec<String>,
    homes: Vec<String>,
    user: Option<String>,
    host: Option<String>,
}

impl LintContext {
    /// Home directories, user and host names from the environment.
    pub fn from_env() -> Self {
        let mut c = Self::default();
        for var in ["USERPROFILE", "HOME", "HOMEPATH"] {
            if let Ok(h) = std::env::var(var) {
                c = c.with_home(&h);
            }
        }
        for var in ["USERNAME", "USER", "LOGNAME"] {
            if let Ok(u) = std::env::var(var) {
                c = c.with_user(&u);
            }
        }
        for var in ["COMPUTERNAME", "HOSTNAME"] {
            if let Ok(h) = std::env::var(var) {
                c = c.with_host(&h);
            }
        }
        c
    }

    /// Adds a home directory (ignored when shorter than 4 characters).
    pub fn with_home(mut self, home: &str) -> Self {
        let h = norm_path(home);
        let h = h.trim_end_matches('/').to_owned();
        if h.len() >= 4 && !self.homes.contains(&h) {
            self.homes.push(h);
        }
        self
    }

    /// Adds literal secret values (resolved env and header values, URL
    /// parts, the broker token) that must not appear verbatim, either raw
    /// or JSON-escaped. Values shorter than 4 bytes are ignored.
    pub fn with_secrets(mut self, values: impl IntoIterator<Item = String>) -> Self {
        for v in values {
            if v.len() < 4 {
                continue;
            }
            let esc = serde_json::to_string(&v).unwrap_or_default();
            let esc = esc.trim_matches('"').to_owned();
            for x in [v, esc] {
                if x.len() >= 4 && !self.secrets.contains(&x) {
                    self.secrets.push(x);
                }
            }
        }
        self
    }

    /// Sets the user name (ignored when shorter than 3 characters).
    pub fn with_user(mut self, user: &str) -> Self {
        if user.len() >= 3 {
            self.user = Some(user.to_ascii_lowercase());
        }
        self
    }

    /// Sets the host name (ignored when shorter than 3 characters).
    pub fn with_host(mut self, host: &str) -> Self {
        if host.len() >= 3 {
            self.host = Some(host.to_ascii_lowercase());
        }
        self
    }
}

/// Lowercase text with `\` and JSON-escaped `\\` turned into `/`.
fn norm_path(s: &str) -> String {
    s.replace("\\\\", "/")
        .replace('\\', "/")
        .to_ascii_lowercase()
}

const ALLOWED_USERS: &[&str] = &["example", "user", "runner", "username"];

fn profile_path_hit(line: &str) -> bool {
    let norm = line.replace("\\\\", "/").replace('\\', "/");
    let lower = norm.to_ascii_lowercase();
    for marker in ["/users/", "/home/"] {
        let mut from = 0;
        while let Some(p) = lower[from..].find(marker) {
            let start = from + p + marker.len();
            let seg: String = norm[start..]
                .chars()
                .take_while(|c| !(c.is_whitespace() || "/\"'`<>".contains(*c)))
                .collect();
            let allowed =
                seg.is_empty() || ALLOWED_USERS.contains(&seg.to_ascii_lowercase().as_str());
            if !allowed {
                return true;
            }
            from = start;
        }
    }
    false
}

fn is_local_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"._%+-".contains(&c)
}

fn email_hit(line: &str) -> bool {
    let b = line.as_bytes();
    for (i, _) in line.match_indices('@') {
        let mut s = i;
        while s > 0 && is_local_char(b[s - 1]) {
            s -= 1;
        }
        if s == i {
            continue;
        }
        let local = line[s..i].to_ascii_lowercase();
        let mut e = i + 1;
        while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'.' || b[e] == b'-') {
            e += 1;
        }
        let dom = line[i + 1..e].trim_end_matches(['.', '-']);
        let d = dom.as_bytes();
        // A dot after a non-empty prefix, followed by two letters.
        let tld = (1..d.len().saturating_sub(2)).any(|p| {
            d[p] == b'.' && d[p + 1].is_ascii_alphabetic() && d[p + 2].is_ascii_alphabetic()
        });
        if !tld {
            continue;
        }
        let dom = dom.to_ascii_lowercase();
        if local.contains("noreply") || dom == "example.com" || dom == "users.noreply.github.com" {
            continue;
        }
        return true;
    }
    false
}

fn run_len(b: &[u8], from: usize, ok: impl Fn(u8) -> bool) -> usize {
    b[from..].iter().take_while(|c| ok(**c)).count()
}

fn token_hit(line: &str) -> bool {
    let b = line.as_bytes();
    let boundary = |at: usize| at == 0 || !b[at - 1].is_ascii_alphanumeric();
    let prefixed = |p: &str, min: usize, ok: &dyn Fn(u8) -> bool, need_boundary: bool| -> bool {
        line.match_indices(p)
            .any(|(at, _)| (!need_boundary || boundary(at)) && run_len(b, at + p.len(), ok) >= min)
    };
    let alnum = |c: u8| c.is_ascii_alphanumeric();
    let alnum_us = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let alnum_us_dash = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    let upper_alnum = |c: u8| c.is_ascii_uppercase() || c.is_ascii_digit();
    for p in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
        if prefixed(p, 20, &alnum, false) {
            return true;
        }
    }
    prefixed("github_pat_", 20, &alnum_us, false)
        || prefixed("sk-", 20, &alnum_us_dash, true)
        || prefixed("AKIA", 16, &upper_alnum, false)
        || xox_hit(line)
        || jwt_hit(line)
        || bearer_hit(line)
        || long_hex_hit(b)
}

/// `<env:X>:pass`, `<user>:pass`, `<arg:/p>tail`: a placeholder at the start
/// of a token with more text attached (the scrubber must have removed it).
fn placeholder_glue_hit(line: &str) -> bool {
    let b = line.as_bytes();
    for p in ["<env:", "<arg:", "<user>", "<host>", "<host#"] {
        for (at, _) in line.match_indices(p) {
            if at > 0 && b[at - 1].is_ascii_alphanumeric() {
                continue;
            }
            let Some(close) = line[at..].find('>') else {
                continue;
            };
            let end = at + close + 1;
            let Some(&c) = b.get(end) else {
                continue;
            };
            let name = p == "<user>" || p == "<host>" || p == "<host#";
            let next = b.get(end + 1).copied().unwrap_or(b' ');
            let hit = if name {
                c == b':' && next.is_ascii_alphabetic()
            } else {
                c.is_ascii_alphanumeric()
                    || ((c == b':' || c == b'.') && next.is_ascii_alphanumeric())
            };
            if hit {
                return true;
            }
        }
    }
    false
}

fn xox_hit(line: &str) -> bool {
    ["xoxb-", "xoxp-", "xoxa-", "xoxs-"].iter().any(|p| {
        line.match_indices(p).any(|(at, _)| {
            run_len(line.as_bytes(), at + p.len(), |c| {
                c.is_ascii_alphanumeric() || c == b'-'
            }) >= 10
        })
    })
}

fn jwt_hit(line: &str) -> bool {
    let b = line.as_bytes();
    line.match_indices("eyJ").any(|(at, _)| {
        let n = run_len(b, at, |c| c.is_ascii_alphanumeric() || b"._-".contains(&c));
        n >= 20 && line[at..at + n].contains('.')
    })
}

fn bearer_hit(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let b = line.as_bytes();
    lower.match_indices("bearer ").any(|(at, _)| {
        let from = at + 7;
        run_len(b, from, |c| {
            c.is_ascii_alphanumeric() || b"._~+/=-".contains(&c)
        }) >= 8
    })
}

fn long_hex_hit(b: &[u8]) -> bool {
    let mut run = 0;
    for &c in b {
        if c.is_ascii_hexdigit() {
            run += 1;
            if run >= 32 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

fn private_ip_hit(line: &str) -> bool {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let n = run_len(b, i, |c| c.is_ascii_digit() || c == b'.');
        let run = line[i..i + n].trim_end_matches('.');
        i += n;
        let parts: Vec<&str> = run.split('.').collect();
        if parts.len() != 4 || parts.iter().any(|p| p.is_empty() || p.len() > 3) {
            continue;
        }
        let o: Vec<u16> = parts.iter().filter_map(|p| p.parse().ok()).collect();
        if o.len() == 4
            && (o[0] == 10
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 172 && (16..=31).contains(&o[1])))
        {
            return true;
        }
    }
    false
}

fn word_hit(lower: &str, needle: &str) -> bool {
    let b = lower.as_bytes();
    lower.match_indices(needle).any(|(at, _)| {
        let before = at == 0 || !b[at - 1].is_ascii_alphanumeric();
        let end = at + needle.len();
        let after = end >= b.len() || !b[end].is_ascii_alphanumeric();
        before && after
    })
}

/// Runs the self-check over `text`: the rules of `scripts/pii-lint.ps1`
/// (profile paths, emails, token prefixes, private IPs), JWTs, bearer
/// values, long hex runs, and this machine's user name, host name and home
/// directory. Returns the hits (rule and line only).
pub fn self_check(text: &str, ctx: &LintContext) -> Vec<Finding> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line_no = n + 1;
        let mut hit = |rule: &'static str| {
            out.push(Finding {
                rule,
                line: line_no,
            })
        };
        if profile_path_hit(line) {
            hit("user-profile path");
        }
        if email_hit(line) {
            hit("email address");
        }
        if token_hit(line) {
            hit("secret-looking token");
        }
        if placeholder_glue_hit(line) {
            hit("text glued to a placeholder");
        }
        if ctx.secrets.iter().any(|v| line.contains(v.as_str())) {
            hit("configured secret value");
        }
        if private_ip_hit(line) {
            hit("private IP address");
        }
        let lower = line.to_ascii_lowercase();
        // Our own placeholders are not the user name.
        let plain = lower
            .replace("<user>", "")
            .replace("<host>", "")
            .replace("<home>", "");
        if ctx.user.as_deref().is_some_and(|u| word_hit(&plain, u)) {
            hit("user name");
        }
        if ctx.host.as_deref().is_some_and(|h| word_hit(&plain, h)) {
            hit("host name");
        }
        if !ctx.homes.is_empty() {
            let np = norm_path(line);
            if ctx.homes.iter().any(|h| np.contains(h.as_str())) {
                hit("home directory");
            }
        }
    }
    out
}

// ----------------------------------------------------------------- build

/// Everything [`build`] needs.
#[derive(Debug, Default)]
pub struct ExportInput {
    /// Failure records (see [`read_records`]).
    pub records: Vec<Value>,
    /// Launch shapes of the servers to describe.
    pub servers: Vec<LaunchShape>,
    /// Statistics for the same period (per-kind counts, p50 and p95).
    pub stats: Option<Summary>,
    /// The broker answered (the log was flushed first).
    pub broker_running: bool,
}

/// How the export is produced.
#[derive(Debug)]
pub struct ExportOptions {
    /// Drop all free text.
    pub strict: bool,
    /// Include the package argument of package launchers.
    pub identify: bool,
    /// Export time, unix ms.
    pub now_ms: u64,
    /// The `--since` bound, unix ms.
    pub since_ms: u64,
    /// Second-pass scrub context (this machine's names).
    pub scrub: ScrubContext,
    /// Self-check context (this machine's names).
    pub lint: LintContext,
    /// Salt used to re-key the hashes.
    pub salt: Salt,
}

impl ExportOptions {
    /// Standard options for this machine: system names, a fresh random
    /// salt.
    pub fn new(now_ms: u64) -> Self {
        Self {
            strict: false,
            identify: false,
            now_ms,
            since_ms: 0,
            scrub: ScrubContext::default().with_system(),
            lint: LintContext::from_env(),
            salt: Salt::ephemeral(),
        }
    }
}

/// Literal secret values of a server's configuration: env and header
/// values (literals and the resolved `from_env` ones), credential parts of
/// the URL, and values attached to flags (`--k=v`, `-kv`). Names only label
/// the placeholders.
pub fn config_secrets(cfg: &ServerConfig) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let resolve = |v: &EnvValue| match v {
        EnvValue::Literal(l) => Some(l.clone()),
        EnvValue::Ref { from_env } => std::env::var(from_env).ok(),
    };
    for (k, v) in cfg.env.iter().chain(cfg.headers.iter()) {
        if let Some(val) = resolve(v) {
            out.push((k.clone(), val));
        }
    }
    if let Some(u) = &cfg.url {
        out.extend(
            super::url_secret_parts(u)
                .into_iter()
                .map(|v| ("URL".to_owned(), v)),
        );
    }
    for a in &cfg.args {
        if let Some(rest) = a.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                out.push((k.to_owned(), v.to_owned()));
            }
        } else if let Some(rest) = a.strip_prefix('-') {
            if rest.len() > 1 {
                let v = rest.split_once('=').map_or(&rest[1..], |(_, v)| v);
                out.push(("ARG".to_owned(), v.to_owned()));
            }
        }
    }
    out
}

impl ExportOptions {
    /// Teaches the second scrub pass and the self-check the secret values
    /// of the configuration (see [`config_secrets`]) plus any extra ones
    /// (such as the broker token). The self-check only takes values of 8+
    /// bytes, so short common values cannot make every export fail.
    pub fn with_secrets(mut self, secrets: Vec<(String, String)>) -> Self {
        let lint: Vec<String> = secrets
            .iter()
            .filter(|(_, v)| v.len() >= 8)
            .map(|(_, v)| v.clone())
            .collect();
        self.scrub = self.scrub.with_secrets(secrets);
        self.lint = self.lint.with_secrets(lint);
        self
    }
}

/// Counts for the CLI summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportCounts {
    /// Records in the export.
    pub records: usize,
    /// Free-text fields the second scrub pass changed.
    pub rescrubbed: u64,
    /// Hashes re-keyed.
    pub rekeyed: u64,
    /// Free-text fields dropped (strict).
    pub dropped: u64,
}

/// A finished export.
#[derive(Debug)]
pub struct Export {
    /// The markdown text.
    pub text: String,
    /// Counts.
    pub counts: ExportCounts,
}

/// Why no export was produced.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExportError {
    /// The self-check found something that must not be shared.
    #[error("the export failed the privacy self-check ({})", .0.iter().map(|f| format!("{} on line {}", f.rule, f.line)).collect::<Vec<_>>().join("; "))]
    SelfCheck(Vec<Finding>),
}

fn is_hex_run(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn rekey_hex(salt: &Salt, old: &str) -> String {
    let m = salt.mac("export", old);
    let n = if old.len() == 12 { 6 } else { 4 };
    m[..n].iter().map(|b| format!("{b:02x}")).collect()
}

/// Replaces `#<8 or 12 hex>` with the same shape under the export salt.
fn rekey_text(salt: &Salt, s: &str, n: &mut u64) -> String {
    if !s.contains('#') {
        return s.to_owned();
    }
    rekey_hashes(salt, s, n)
}

fn rekey_hashes(salt: &Salt, s: &str, n: &mut u64) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('#') {
        out.push_str(&rest[..=p]);
        rest = &rest[p + 1..];
        let hexn = rest
            .bytes()
            .take_while(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
            .count();
        let boundary = rest[hexn..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        if (hexn == 8 || hexn == 12) && boundary {
            out.push_str(&rekey_hex(salt, &rest[..hexn]));
            *n += 1;
            rest = &rest[hexn..];
        }
    }
    out.push_str(rest);
    out
}

fn rekey_value(salt: &Salt, v: &mut Value, n: &mut u64) {
    match v {
        Value::String(s) => *s = rekey_text(salt, s, n),
        Value::Array(a) => a.iter_mut().for_each(|x| rekey_value(salt, x, n)),
        Value::Object(m) => {
            let old = std::mem::take(m);
            for (k, mut x) in old {
                rekey_value(salt, &mut x, n);
                m.insert(rekey_text(salt, &k, n), x);
            }
        }
        _ => {}
    }
}

/// Re-keys the bare-hash fields of one record, then every `#hash` in it.
fn rekey_record(salt: &Salt, rec: &mut Value, n: &mut u64) {
    let mut bare = |v: &mut Value| {
        if let Some(s) = v.as_str() {
            if (s.len() == 8 || s.len() == 12) && is_hex_run(s) {
                *v = Value::String(rekey_hex(salt, s));
                *n += 1;
            }
        }
    };
    if let Some(v) = rec.get_mut("instance") {
        bare(v);
    }
    if let Some(v) = rec.pointer_mut("/http/url/host") {
        bare(v);
    }
    rekey_value(salt, rec, n);
}

fn scrub_field(sc: &Scrubber<'_>, v: &mut Value, changed: &mut u64) {
    if let Some(s) = v.as_str() {
        let new = sc.scrub(s);
        if new.as_str() != s {
            *changed += 1;
            *v = Value::String(new.as_str().to_owned());
        }
    }
}

/// Second scrub pass over (or removal of) the free-text fields.
fn clean_record(rec: &mut Value, strict: bool, sc: &Scrubber<'_>, counts: &mut ExportCounts) {
    if strict {
        if let Some(e) = rec.get_mut("error").and_then(Value::as_object_mut) {
            if e.remove("message").is_some() {
                counts.dropped += 1;
            }
        }
        if let Some(s) = rec.get_mut("stderr").and_then(Value::as_object_mut) {
            if let Some(Value::Array(t)) = s.remove("tail") {
                counts.dropped += t.len() as u64;
            }
        }
        if let Some(c) = rec.get_mut("cause").and_then(Value::as_object_mut) {
            if let Some(Value::Array(h)) = c.remove("hints") {
                counts.dropped += h.len() as u64;
            }
        }
        return;
    }
    if let Some(m) = rec.pointer_mut("/error/message") {
        scrub_field(sc, m, &mut counts.rescrubbed);
    }
    for ptr in ["/stderr/tail", "/cause/hints"] {
        if let Some(Value::Array(a)) = rec.pointer_mut(ptr) {
            for x in a {
                scrub_field(sc, x, &mut counts.rescrubbed);
            }
        }
    }
}

fn count_placeholders(text: &str) -> BTreeMap<&'static str, u64> {
    let mut m = BTreeMap::new();
    for (name, pat) in [
        ("home", "<home>"),
        ("cwd", "<cwd>"),
        ("user", "<user>"),
        ("host", "<host>"),
        ("env", "<env:"),
        ("arg", "<arg:"),
        ("path", "<path>"),
        ("redacted", "<redacted:"),
    ] {
        m.insert(name, text.matches(pat).count() as u64);
    }
    m
}

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(Value::as_str).map(str::to_owned)
}

fn catalog_summary(records: &[Value]) -> Value {
    let mut per: BTreeMap<String, (u64, Value, BTreeSet<String>)> = BTreeMap::new();
    for r in records {
        let Some(server) = r["server"].as_str() else {
            continue;
        };
        let e = per
            .entry(server.to_owned())
            .or_insert_with(|| (0, Value::Null, BTreeSet::new()));
        if let Some(t) = s(r, "/request/tool") {
            if e.2.len() < 20 {
                e.2.insert(t);
            }
        }
        if let Some(b) = r.get("backend") {
            let ts = r["ts"].as_u64().unwrap_or(0);
            if ts >= e.0 && (b.get("protocol").is_some() || b.get("server_info").is_some()) {
                e.0 = ts;
                e.1 = json!({
                    "protocol": b.get("protocol").cloned().unwrap_or(Value::Null),
                    "server_info": b.get("server_info").cloned().unwrap_or(Value::Null),
                    "capabilities": b.get("caps").cloned().unwrap_or(json!([])),
                });
            }
        }
    }
    Value::Array(
        per.into_iter()
            .map(|(server, (_, info, tools))| {
                json!({
                    "server": server,
                    "backend": info,
                    "tools_in_failures": tools,
                })
            })
            .collect(),
    )
}

fn frame_sig(rec: &Value) -> Option<String> {
    rec["frames"]
        .as_array()?
        .iter()
        .rev()
        .find_map(|f| f["sig"].as_str().map(str::to_owned))
}

/// One replay scenario: the mock script that reproduces the broker-visible
/// symptom of the record's kind, when one exists.
fn replay_scenario(rec: &Value) -> Value {
    let kind = rec["kind"].as_str().unwrap_or("");
    let method = s(rec, "/request/method");
    let tool = s(rec, "/request/tool");
    let on = |default: &str| {
        let mut o = Map::new();
        o.insert(
            "method".into(),
            Value::String(method.clone().unwrap_or_else(|| default.to_owned())),
        );
        if let (Some(t), Some("tools/call")) = (&tool, method.as_deref()) {
            o.insert("tool".into(), Value::String(t.clone()));
        }
        Value::Object(o)
    };
    let step = |on: Value, actions: Vec<Value>| json!([{ "on": on, "do": actions }]);
    let code = rec["exit"]["code"].as_i64().unwrap_or(1);
    let oversize = rec["frames"]
        .as_array()
        .and_then(|fr| fr.iter().find(|f| f["type"] == "oversize"))
        .and_then(|f| f["bytes"].as_u64())
        .unwrap_or(2 * 1024 * 1024)
        .min(64 * 1024 * 1024);
    let (script, note): (Option<Value>, &str) = match kind {
        "init_timeout" | "init_no_output" => (
            Some(step(json!({"method": "initialize"}), vec![])),
            "initialize is never answered",
        ),
        "exit_unexpected" | "crash_in_flight" => (
            Some(step(
                on("tools/call"),
                vec![json!({"exit": {"code": code}})],
            )),
            "the backend exits while handling the request",
        ),
        "frame_oversize" => (
            Some(step(
                on("tools/call"),
                vec![json!({"oversize": {"bytes": oversize}})],
            )),
            "one stdout line over the frame limit",
        ),
        "stdout_pollution" => {
            let action = match frame_sig(rec).as_deref() {
                Some("content_length") => json!("content_length_framing"),
                Some("lone_brace") => json!("pretty_json"),
                Some("banner") => json!({"stdout_raw": "notice: synthetic banner line"}),
                _ => json!({"stdout_raw": "synthetic text line"}),
            };
            (
                Some(step(on("tools/call"), vec![action, json!("respond")])),
                "non-JSON output on stdout before a normal reply",
            )
        }
        "id_type_mismatch" => (
            Some(step(
                on("tools/call"),
                vec![json!("string_id"), json!("respond")],
            )),
            "the reply carries a string id",
        ),
        "frame_invalid" => (
            Some(step(
                on("tools/call"),
                vec![json!({"stdout_raw": "{}"}), json!("respond")],
            )),
            "JSON that is not JSON-RPC before a normal reply",
        ),
        "stalled" => (
            Some(step(on("tools/call"), vec![])),
            "the request is never answered",
        ),
        "spawn_failed" => (None, "point the server at a command that does not exist"),
        _ => (
            None,
            "not reproducible with the mock; the record is the fixture",
        ),
    };
    json!({
        "expected_kind": kind,
        "server": rec["server"],
        "transport": rec["transport"],
        "request": {
            "method": method,
            "bytes": rec.pointer("/request/bytes"),
            "shape": rec.pointer("/request/shape"),
        },
        "script": script,
        "note": note,
    })
}

fn replay_section(records: &[Value]) -> Value {
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut scenarios = Vec::new();
    for r in records.iter().rev() {
        let key = (
            r["server"].as_str().unwrap_or("").to_owned(),
            r["kind"].as_str().unwrap_or("").to_owned(),
        );
        if r["kind"] == "suppressed" || !seen.insert(key) {
            continue;
        }
        scenarios.push(replay_scenario(r));
    }
    json!({
        "how": "Each script is a mock_mcp_stdio --script file (a JSON array of steps). Synthetic requests of the recorded method, size and shape reproduce the symptom the broker saw (framing, timing, exit), not the server's internal bug.",
        "scenarios": scenarios,
    })
}

fn cell(s: &str) -> String {
    Name::new(s).as_str().to_owned()
}

fn kind_tables(records: &[Value], stats: Option<&Summary>) -> String {
    let mut out = String::new();
    let mut rec_counts: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
    for r in records {
        let e = rec_counts
            .entry((
                cell(r["server"].as_str().unwrap_or("?")),
                cell(r["kind"].as_str().unwrap_or("?")),
            ))
            .or_default();
        e.0 += 1;
        e.1 += r["repeat"].as_u64().unwrap_or(1).max(1);
    }
    out.push_str("| server | kind | records | occurrences |\n|---|---|---|---|\n");
    for ((server, kind), (n, occ)) in &rec_counts {
        let _ = writeln!(out, "| {server} | {kind} | {n} | {occ} |");
    }
    if rec_counts.is_empty() {
        out.push_str("| (none) | | 0 | 0 |\n");
    }
    out.push('\n');
    match stats {
        Some(st) if !st.servers.is_empty() => {
            out.push_str("Call statistics for the period (from the statistics log):\n\n");
            out.push_str("| server | calls | errors | p50 ms | p95 ms | failures by kind |\n|---|---|---|---|---|---|\n");
            for sv in &st.servers {
                let f: Vec<String> = sv
                    .failures
                    .iter()
                    .map(|(k, n)| format!("{}={n}", cell(k)))
                    .collect();
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} | {} |",
                    cell(&sv.server),
                    sv.calls,
                    sv.errors,
                    sv.p50_ms,
                    sv.p95_ms,
                    if f.is_empty() {
                        "-".to_owned()
                    } else {
                        f.join(", ")
                    }
                );
            }
        }
        _ => out.push_str("No call statistics available for the period.\n"),
    }
    out
}

fn shape_json(l: &LaunchShape, identify: bool) -> Value {
    let mut o = Map::new();
    o.insert("server".into(), json!(l.server));
    o.insert("transport".into(), json!(l.transport));
    if let Some(x) = &l.launcher {
        o.insert("launcher".into(), json!(x));
    }
    o.insert("flags".into(), json!(l.flags));
    o.insert("arg_count".into(), json!(l.arg_count));
    o.insert("env_names".into(), json!(l.env_names));
    o.insert("header_names".into(), json!(l.header_names));
    o.insert("headers_helper".into(), json!(l.headers_helper));
    o.insert("cwd_set".into(), json!(l.cwd_set));
    if identify {
        if let Some(p) = &l.package {
            o.insert("package".into(), json!(p));
        }
    }
    Value::Object(o)
}

fn fence(out: &mut String, lang: &str, body: &str) {
    let _ = writeln!(out, "```{lang}\n{body}\n```\n");
}

/// Builds the export. Fails closed when the self-check finds anything.
pub fn build(input: ExportInput, opts: &ExportOptions) -> Result<Export, ExportError> {
    let mut counts = ExportCounts::default();
    let mut records = input.records;
    let no_args = ArgSet::new();
    {
        let sc = Scrubber::new(&opts.scrub, &opts.salt, &no_args);
        for r in &mut records {
            clean_record(r, opts.strict, &sc, &mut counts);
        }
    }
    for r in &mut records {
        rekey_record(&opts.salt, r, &mut counts.rekeyed);
    }
    counts.records = records.len();

    // Servers named in the records come first; configured servers without
    // failures are described too when the CLI passed them.
    let shapes: Vec<Value> = input
        .servers
        .iter()
        .map(|l| shape_json(l, opts.identify))
        .collect();
    let catalog = catalog_summary(&records);
    let replay = replay_section(&records);
    let versions: BTreeSet<String> = records
        .iter()
        .filter_map(|r| s(r, "/broker/version"))
        .collect();

    let mut body = String::new();
    body.push_str("## Failure records\n\nOne JSON object per line, newest last. Argument values are never recorded, only their shape.\n\n");
    let lines: Vec<String> = records.iter().map(Value::to_string).collect();
    fence(&mut body, "jsonl", &lines.join("\n"));
    let redactions = count_placeholders(&body);

    let manifest = json!({
        "format": "mcpdoze-debug-export",
        "version": FORMAT,
        "mcpdoze": env!("CARGO_PKG_VERSION"),
        "record_versions_seen": versions,
        "broker_running": input.broker_running,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "generated_unix_ms": opts.now_ms,
        "since_unix_ms": opts.since_ms,
        "mode": if opts.strict { "strict" } else { "standard" },
        "identify": opts.identify,
        "records": counts.records,
        "record_cap": MAX_RECORDS,
        "second_scrub_changed": counts.rescrubbed,
        "hashes_rekeyed": counts.rekeyed,
        "free_text_dropped": counts.dropped,
        "redactions": redactions,
    });

    let mut text = String::new();
    text.push_str("# MCPDoze debug export\n\n");
    text.push_str("Built from the failure log. It holds launch shapes (names only), failure records with argument shapes instead of values, call statistics, and a replay description. Review it before sharing.\n\n");
    text.push_str("## Manifest\n\n");
    fence(
        &mut text,
        "json",
        &serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    );
    text.push_str("## Server launch shape\n\n");
    fence(
        &mut text,
        "json",
        &serde_json::to_string_pretty(&shapes).unwrap_or_default(),
    );
    text.push_str("## Catalog summary\n\nFrom what the failure records saw (the tool catalog itself is not read).\n\n");
    fence(
        &mut text,
        "json",
        &serde_json::to_string_pretty(&catalog).unwrap_or_default(),
    );
    text.push_str("## Per-kind counts and latency\n\n");
    text.push_str(&kind_tables(&records, input.stats.as_ref()));
    text.push('\n');
    text.push_str(&body);
    text.push_str("## Replay\n\n");
    fence(
        &mut text,
        "json",
        &serde_json::to_string_pretty(&replay).unwrap_or_default(),
    );

    let findings = self_check(&text, &opts.lint);
    if !findings.is_empty() {
        return Err(ExportError::SelfCheck(findings));
    }
    Ok(Export { text, counts })
}

#[cfg(test)]
mod tests;
