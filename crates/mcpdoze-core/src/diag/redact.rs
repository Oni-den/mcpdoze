// SPDX-License-Identifier: GPL-3.0-or-later

//! Redaction: everything that turns free text or identifiers into something
//! safe to write.
//!
//! * [`Salt`]: a per-install random key (file `.salt` in the debug
//!   directory, owner-only, never exported) and HMAC-SHA256 hashing with a
//!   domain tag. Used for object keys, hosts, resource URIs, session ids and
//!   directories.
//! * [`reduce_url`]: scheme, salted host hash, path depth; never userinfo,
//!   query or fragment.
//! * [`ScrubContext`] and [`Scrubber`]: the free-text scrubber for stderr
//!   lines and error messages. In order:
//!   1. exact match of resolved env and header values and the broker token
//!      (`<env:NAME>`; values shorter than 4 bytes are ignored);
//!   2. exact match of the call's string arguments (`<arg:/pointer>`);
//!   3. home directories, the user name, the host name and the working
//!      directory (`<home>`, `<user>`, `<host>`, `<cwd>`), ASCII
//!      case-insensitive, either slash direction;
//!   4. a token pass: `Bearer`/`Basic` and `Authorization:`-like values,
//!      `key=value` with a sensitive key, URLs (reduced), JWTs, known token
//!      prefixes (`sk-`, `ghp_`, `github_pat_`, `xoxb-`, `AKIA`, ...), emails,
//!      IPv4 and IPv6 addresses, query strings, hex runs of 24+ and mixed
//!      base64 runs of 24+ (`<redacted:len>`), and every path with two or
//!      more separators (`<path>/basename`);
//!   5. truncation to 300 characters.
//!
//!   [`Scrubber::stderr_line`] first drops lines that do not look like errors
//!   (`<line len=N>`).
//!
//! The scrubber errs on the side of removing too much. Encodings of a
//! secret (URL-encoded, base64 of a value, split across lines) are not
//! matched by the exact-value step; the token pass catches the common
//! shapes.

use std::io::{self, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::record::{Name, Scrubbed, UrlInfo};
use super::shape::ArgSet;

/// Longest scrubbed text, in characters.
pub const MAX_TEXT: usize = 300;
const MIN_SECRET_LEN: usize = 4;
const BLOCK: usize = 64;

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let ih = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(ih);
    outer.finalize().into()
}

/// Per-install secret used for salted hashing.
#[derive(Clone)]
pub struct Salt {
    key: [u8; 32],
}

impl std::fmt::Debug for Salt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Salt(..)")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Salt {
    /// A salt from fixed bytes (tests).
    pub fn from_bytes(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// A random salt that is not persisted.
    pub fn ephemeral() -> Self {
        let mut key = [0u8; 32];
        if getrandom::fill(&mut key).is_err() {
            // Extremely unlikely; fall back to time-derived bytes rather
            // than an all-zero key.
            let t = crate::stats::now_ms().to_le_bytes();
            key[..8].copy_from_slice(&t);
            key[8..16].copy_from_slice(&std::process::id().to_le_bytes().repeat(2));
        }
        Self { key }
    }

    /// Reads the salt from `path` (64 hex digits) or creates it owner-only.
    /// The directory is created (0700 on Unix) when missing.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        fn read(path: &Path) -> io::Result<Salt> {
            let s = std::fs::read_to_string(path)?;
            let s = s.trim();
            if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "bad salt file"));
            }
            let mut key = [0u8; 32];
            for (i, b) in key.iter_mut().enumerate() {
                *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad salt file"))?;
            }
            Ok(Salt { key })
        }
        match read(path) {
            Ok(s) => return Ok(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Some(dir) = path.parent() {
            crate::auth::create_private_dir(dir)?;
        }
        let salt = Salt::ephemeral();
        match crate::auth::create_private(path) {
            Ok(mut f) => {
                f.write_all(hex(&salt.key).as_bytes())?;
                f.write_all(b"\n")?;
                f.sync_all()?;
                Ok(salt)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => read(path),
            Err(e) => Err(e),
        }
    }

    /// HMAC-SHA256 of `domain || 0 || data`.
    pub fn mac(&self, domain: &str, data: &str) -> [u8; 32] {
        let mut m = Vec::with_capacity(domain.len() + 1 + data.len());
        m.extend_from_slice(domain.as_bytes());
        m.push(0);
        m.extend_from_slice(data.as_bytes());
        hmac_sha256(&self.key, &m)
    }

    /// First 4 bytes of the MAC as 8 hex digits.
    pub fn hash8(&self, domain: &str, data: &str) -> String {
        hex(&self.mac(domain, data)[..4])
    }

    /// First 6 bytes of the MAC as 12 hex digits.
    pub fn hash12(&self, domain: &str, data: &str) -> String {
        hex(&self.mac(domain, data)[..6])
    }
}

/// The launcher's file name without directory and `.exe`/`.cmd`/`.bat`.
pub fn launcher_basename(command: &str) -> Name {
    let base = command.rsplit(['/', '\\']).next().unwrap_or(command);
    let lower = base.to_ascii_lowercase();
    let stem = [".exe", ".cmd", ".bat", ".com"]
        .iter()
        .find(|e| lower.ends_with(**e))
        .map_or(base, |e| &base[..base.len() - e.len()]);
    Name::new(stem)
}

/// Splits a URL into scheme, host and path depth: `None` when it has no
/// `scheme://`.
pub fn reduce_url(salt: &Salt, url: &str) -> Option<UrlInfo> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
    {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .to_ascii_lowercase();
    let after = &rest[end..];
    let path_end = after.find(['?', '#']).unwrap_or(after.len());
    let depth = after[..path_end]
        .split('/')
        .filter(|s| !s.is_empty())
        .count();
    Some(UrlInfo {
        scheme: Name::new(&scheme.to_ascii_lowercase()),
        host: salt.hash8("host", &host),
        path_depth: depth,
    })
}

/// What the scrubber knows about this machine and this backend.
#[derive(Debug, Clone, Default)]
pub struct ScrubContext {
    secrets: Vec<(String, String)>,
    homes: Vec<String>,
    user: Option<String>,
    host: Option<String>,
    cwd: Option<String>,
}

impl ScrubContext {
    /// A context holding `(name, value)` secrets: resolved env values,
    /// header values and the broker token. Names appear in the placeholder
    /// (`<env:NAME>`); values never leave memory.
    pub fn new(secrets: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut secrets: Vec<(String, String)> = secrets
            .into_iter()
            .filter(|(_, v)| v.len() >= MIN_SECRET_LEN)
            .map(|(n, v)| (Name::new(&n).as_str().to_string(), v))
            .collect();
        secrets.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        Self {
            secrets,
            ..Self::default()
        }
    }

    /// Adds the home directory, user name and host name of this machine
    /// (from the environment).
    pub fn with_system(mut self) -> Self {
        for var in ["USERPROFILE", "HOME", "HOMEPATH"] {
            if let Ok(h) = std::env::var(var) {
                self = self.with_home(&h);
            }
        }
        for var in ["USERNAME", "USER", "LOGNAME"] {
            if let Ok(u) = std::env::var(var) {
                self = self.with_user(&u);
            }
        }
        for var in ["COMPUTERNAME", "HOSTNAME"] {
            if let Ok(h) = std::env::var(var) {
                self = self.with_host(&h);
            }
        }
        self
    }

    /// Adds a home directory prefix.
    pub fn with_home(mut self, home: &str) -> Self {
        let h = home.trim_end_matches(['/', '\\']);
        if h.len() > 2 && !self.homes.iter().any(|x| x == h) {
            self.homes.push(h.to_string());
        }
        self
    }

    /// Sets the user name (ignored when shorter than 3 characters).
    pub fn with_user(mut self, user: &str) -> Self {
        if user.len() >= 3 {
            self.user = Some(user.to_string());
        }
        self
    }

    /// Sets the host name (ignored when shorter than 3 characters).
    pub fn with_host(mut self, host: &str) -> Self {
        if host.len() >= 3 {
            self.host = Some(host.to_string());
        }
        self
    }

    /// Sets the backend's working directory.
    pub fn with_cwd(mut self, cwd: &str) -> Self {
        let c = cwd.trim_end_matches(['/', '\\']);
        if c.len() > 2 {
            self.cwd = Some(c.to_string());
        }
        self
    }
}

/// Case-insensitive (ASCII) replace of every occurrence of `needle`.
fn replace_ci(text: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    let hay = text.to_ascii_lowercase();
    let nd = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(p) = hay[i..].find(&nd) {
        out.push_str(&text[i..i + p]);
        out.push_str(with);
        i += p + nd.len();
    }
    out.push_str(&text[i..]);
    out
}

/// A path in the spellings it can take in text: as given, with the other
/// slash, and JSON-escaped backslashes.
fn path_variants(p: &str) -> Vec<String> {
    let fwd = p.replace('\\', "/");
    let back = p.replace('/', "\\");
    let dbl = back.replace('\\', "\\\\");
    let mut v = vec![p.to_string(), fwd, back, dbl];
    v.sort();
    v.dedup();
    v
}

const PLACEHOLDER_STARTS: &[&str] = &[
    "<env:",
    "<arg:",
    "<path>",
    "<redacted:",
    "<user>",
    "<host>",
    "<line len=",
    "<host#",
];

const KNOWN_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "AKIA",
    "ASIA",
    "AIza",
    "glpat-",
    "npm_",
    "pypi-",
    "ya29.",
    "rk_live_",
    "pk_live_",
    "shpat_",
    "hf_",
];

const KEYWORDS: &[&str] = &[
    "bearer",
    "basic",
    "authorization",
    "proxy-authorization",
    "token",
    "password",
    "passwd",
    "secret",
    "api-key",
    "apikey",
    "x-api-key",
    "cookie",
    "set-cookie",
];

const SENSITIVE_KEY_PARTS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "pwd",
    "key",
    "auth",
    "credential",
    "cookie",
    "session",
    "sig",
];

fn redacted(n: usize) -> String {
    format!("<redacted:{n}>")
}

fn is_delim(c: char) -> bool {
    c.is_whitespace() || "\"'`()[]{},;|".contains(c)
}

fn is_run_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"+/_-=".contains(&c)
}

fn secret_like(run: &str) -> bool {
    let hexy = run.len() >= 24 && run.bytes().all(|c| c.is_ascii_hexdigit());
    let has_digit = run.bytes().any(|c| c.is_ascii_digit());
    let has_up = run.bytes().any(|c| c.is_ascii_uppercase());
    let has_lo = run.bytes().any(|c| c.is_ascii_lowercase());
    hexy || (run.len() >= 24 && has_digit && has_up && has_lo)
}

/// Replaces hex runs and mixed base64 runs of 24+ characters: the whole
/// run (base64 may hold `+ / = _ -`), or any alphanumeric segment of it.
fn redact_runs(tok: &str) -> String {
    let b = tok.as_bytes();
    let mut out = String::with_capacity(tok.len());
    let mut i = 0;
    while i < b.len() {
        if is_run_char(b[i]) {
            let mut j = i;
            while j < b.len() && is_run_char(b[j]) {
                j += 1;
            }
            let run = &tok[i..j];
            if secret_like(run) {
                out.push_str(&redacted(run.len()));
            } else {
                let mut k = 0;
                let rb = run.as_bytes();
                while k < rb.len() {
                    if rb[k].is_ascii_alphanumeric() {
                        let mut e = k;
                        while e < rb.len() && rb[e].is_ascii_alphanumeric() {
                            e += 1;
                        }
                        let seg = &run[k..e];
                        if secret_like(seg) {
                            out.push_str(&redacted(seg.len()));
                        } else {
                            out.push_str(seg);
                        }
                        k = e;
                    } else {
                        out.push(rb[k] as char);
                        k += 1;
                    }
                }
            }
            i = j;
        } else {
            let ch = tok[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn redact_prefixed(tok: &str) -> Option<String> {
    let b = tok.as_bytes();
    for p in KNOWN_PREFIXES {
        let mut from = 0;
        while let Some(pos) = tok[from..].find(p) {
            let at = from + pos;
            let boundary = at == 0 || !b[at - 1].is_ascii_alphanumeric();
            let mut end = at + p.len();
            while end < b.len() && (b[end].is_ascii_alphanumeric() || b"_-.".contains(&b[end])) {
                end += 1;
            }
            if boundary && end - at >= p.len() + 6 {
                let mut s = tok[..at].to_string();
                s.push_str(&redacted(end - at));
                s.push_str(&tok[end..]);
                return Some(s);
            }
            from = at + p.len();
        }
    }
    None
}

fn redact_jwt(tok: &str) -> Option<String> {
    let at = tok.find("eyJ")?;
    let rest = &tok[at..];
    let end = rest
        .bytes()
        .position(|c| !(c.is_ascii_alphanumeric() || b"._-".contains(&c)))
        .unwrap_or(rest.len());
    let jwt = &rest[..end];
    if jwt.matches('.').count() >= 2 && jwt.len() >= 20 {
        Some(format!(
            "{}{}{}",
            &tok[..at],
            redacted(jwt.len()),
            &rest[end..]
        ))
    } else {
        None
    }
}

fn redact_emails(tok: &str) -> Option<String> {
    let b = tok.as_bytes();
    let mut out = String::new();
    let mut last = 0;
    let mut found = false;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'@' {
            let mut l = i;
            while l > last && (b[l - 1].is_ascii_alphanumeric() || b"._%+-".contains(&b[l - 1])) {
                l -= 1;
            }
            let mut r = i + 1;
            while r < b.len() && (b[r].is_ascii_alphanumeric() || b"-.<>#".contains(&b[r])) {
                r += 1;
            }
            let domain = tok[i + 1..r].trim_end_matches('.');
            // A placeholder in the domain (a scrubbed user or host name)
            // still marks an address.
            let tld_ok = domain.contains('<')
                || domain.rsplit_once('.').is_some_and(|(_, t)| {
                    t.len() >= 2 && t.bytes().all(|c| c.is_ascii_alphabetic())
                });
            if l < i && tld_ok {
                let end = i + 1 + domain.len();
                out.push_str(&tok[last..l]);
                out.push_str(&redacted(end - l));
                last = end;
                i = end;
                found = true;
                continue;
            }
        }
        i += 1;
    }
    if found {
        out.push_str(&tok[last..]);
        Some(out)
    } else {
        None
    }
}

fn redact_ipv4(tok: &str) -> Option<String> {
    let b = tok.as_bytes();
    let mut out = String::new();
    let mut last = 0;
    let mut found = false;
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit()
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.'))
        {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') {
                j += 1;
            }
            let run = tok[i..j].trim_end_matches('.');
            let parts: Vec<&str> = run.split('.').collect();
            if parts.len() == 4
                && parts.iter().all(|p| {
                    !p.is_empty() && p.len() <= 3 && p.parse::<u16>().is_ok_and(|n| n <= 255)
                })
            {
                out.push_str(&tok[last..i]);
                out.push_str(&redacted(run.len()));
                last = i + run.len();
                found = true;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    if found {
        out.push_str(&tok[last..]);
        Some(out)
    } else {
        None
    }
}

fn looks_ipv6(tok: &str) -> bool {
    let t = tok.trim_matches(|c: char| !c.is_ascii_hexdigit() && c != ':');
    let colons = t.bytes().filter(|c| *c == b':').count();
    t.len() >= 5
        && t.bytes().all(|c| c.is_ascii_hexdigit() || c == b':')
        && (t.contains("::") || colons >= 5)
        && t.bytes()
            .any(|c| c.is_ascii_alphabetic() || t.contains("::"))
}

fn is_abs_or_deep_path(tok: &str) -> bool {
    let seps = tok.bytes().filter(|c| *c == b'/' || *c == b'\\').count();
    let b = tok.as_bytes();
    let drive = b.len() > 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/');
    seps >= 2 || (drive && seps >= 1) || tok.starts_with("\\\\")
}

fn reduce_path(tok: &str) -> String {
    // Keep a trailing :line:col.
    let mut core = tok;
    let mut suffix = "";
    loop {
        match core.rsplit_once(':') {
            Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|c| c.is_ascii_digit()) => {
                core = head;
                suffix = &tok[core.len()..];
            }
            _ => break,
        }
    }
    let base = core.rsplit(['/', '\\']).next().unwrap_or("");
    if base.is_empty() || base.len() > 80 {
        format!("<path>{suffix}")
    } else {
        format!("<path>/{base}{suffix}")
    }
}

fn looks_like_error(line: &str) -> bool {
    const WORDS: &[&str] = &[
        "Error",
        "error",
        "ERR",
        "Traceback",
        "panic",
        "Exception",
        "FATAL",
        "fatal",
        "Fatal",
        "failed",
        "Failed",
    ];
    if WORDS.iter().any(|w| line.contains(w)) {
        return true;
    }
    if line.trim_start().starts_with("at ") {
        return true;
    }
    line.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        w.len() >= 4
            && w.len() <= 12
            && w.starts_with('E')
            && w.bytes().all(|c| c.is_ascii_uppercase())
    })
}

/// Scrubs text for one record: the context plus the call's argument set.
pub struct Scrubber<'a> {
    ctx: &'a ScrubContext,
    salt: &'a Salt,
    args: &'a ArgSet,
}

impl<'a> Scrubber<'a> {
    /// A scrubber over `ctx`, `salt` and the arguments of the triggering
    /// call (use `&ArgSet::new()` when there is none).
    pub fn new(ctx: &'a ScrubContext, salt: &'a Salt, args: &'a ArgSet) -> Self {
        Self { ctx, salt, args }
    }

    fn exact(&self, text: &str) -> String {
        let mut s = text.to_string();
        for (name, value) in &self.ctx.secrets {
            if s.contains(value.as_str()) {
                s = s.replace(value.as_str(), &format!("<env:{name}>"));
            }
        }
        let mut args: Vec<(&str, &str)> = self.args.iter().collect();
        args.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));
        for (value, ptr) in args {
            if s.contains(value) {
                s = s.replace(value, &format!("<arg:{ptr}>"));
            }
        }
        s
    }

    fn system(&self, text: String) -> String {
        let mut s = text;
        let mut paths: Vec<(&str, &str)> = self
            .ctx
            .homes
            .iter()
            .map(|h| (h.as_str(), "<home>"))
            .collect();
        if let Some(c) = &self.ctx.cwd {
            paths.push((c.as_str(), "<cwd>"));
        }
        paths.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        for (p, with) in paths {
            for v in path_variants(p) {
                s = replace_ci(&s, &v, with);
            }
        }
        if let Some(u) = &self.ctx.user {
            s = replace_ci(&s, u, "<user>");
        }
        if let Some(h) = &self.ctx.host {
            s = replace_ci(&s, h, "<host>");
        }
        s
    }

    fn url_token(&self, tok: &str) -> Option<String> {
        let at = tok.find("://")?;
        let scheme_start = tok[..at]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || "+-.".contains(c)))
            .map_or(0, |p| p + 1);
        let info = reduce_url(self.salt, &tok[scheme_start..])?;
        Some(format!(
            "{}{}://<host#{}>/<depth:{}>",
            &tok[..scheme_start],
            info.scheme.as_str(),
            info.host,
            info.path_depth
        ))
    }

    fn token(&self, tok: &str) -> String {
        if PLACEHOLDER_STARTS.iter().any(|p| tok.starts_with(p)) {
            return tok.to_string();
        }
        // A path under a scrubbed home or working directory: keep only the
        // file name.
        if (tok.starts_with("<home>") || tok.starts_with("<cwd>")) && tok.contains(['/', '\\']) {
            return reduce_path(tok);
        }
        // key=value: keep the key, scrub the value.
        if let Some((k, v)) = tok.split_once('=') {
            let ident = k.len() <= 32
                && k.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c));
            if ident && !v.is_empty() {
                let kl = k.to_ascii_lowercase();
                // A key with digits may itself be an address or an id.
                let key = if k.bytes().any(|c| c.is_ascii_digit()) {
                    self.token(k)
                } else {
                    k.to_string()
                };
                if SENSITIVE_KEY_PARTS.iter().any(|p| kl.contains(p)) {
                    return format!("{key}={}", redacted(v.len()));
                }
                return format!("{key}={}", self.token(v));
            }
        }
        if let Some(u) = self.url_token(tok) {
            return u;
        }
        if let Some(s) = redact_jwt(tok) {
            return s;
        }
        if let Some(s) = redact_prefixed(tok) {
            return s;
        }
        if let Some(s) = redact_emails(tok) {
            return s;
        }
        if let Some(s) = redact_ipv4(tok) {
            return s;
        }
        if looks_ipv6(tok) {
            return redacted(tok.len());
        }
        if let Some((head, query)) = tok.split_once('?') {
            if query.contains('=') {
                return format!("{}?{}", self.token(head), redacted(query.len()));
            }
        }
        let t = redact_runs(tok);
        if is_abs_or_deep_path(&t) && !t.contains("<redacted:") {
            return reduce_path(&t);
        }
        t
    }

    fn token_pass(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut tok_start: Option<usize> = None;
        let mut redact_next = false;
        let flush = |out: &mut String, tok: &str, redact_next: &mut bool| {
            let lower = tok
                .trim_matches(|c: char| c == ':' || c == '=')
                .to_ascii_lowercase();
            if KEYWORDS.contains(&lower.as_str()) {
                out.push_str(tok);
                *redact_next = true;
            } else if *redact_next {
                out.push_str(&redacted(tok.len()));
                *redact_next = false;
            } else {
                out.push_str(&self.token(tok));
            }
        };
        for (i, c) in text.char_indices() {
            if is_delim(c) {
                if let Some(s) = tok_start.take() {
                    flush(&mut out, &text[s..i], &mut redact_next);
                }
                out.push(c);
            } else if tok_start.is_none() {
                tok_start = Some(i);
            }
        }
        if let Some(s) = tok_start {
            flush(&mut out, &text[s..], &mut redact_next);
        }
        out
    }

    /// Scrubs free text (an error message, a stderr line).
    pub fn scrub(&self, text: &str) -> Scrubbed {
        let s = self.exact(text);
        let s = self.system(s);
        let s = self.token_pass(&s);
        let s = if s.chars().count() > MAX_TEXT {
            let cut: String = s.chars().take(MAX_TEXT).collect();
            format!("{cut}…")
        } else {
            s
        };
        Scrubbed::from_scrubber(s)
    }

    /// A stderr line: scrubbed when it looks like an error, otherwise only
    /// its length (`<line len=N>`).
    pub fn stderr_line(&self, line: &str) -> Scrubbed {
        if looks_like_error(line) {
            self.scrub(line)
        } else {
            Scrubbed::from_scrubber(format!("<line len={}>", line.len()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn salt() -> Salt {
        Salt::from_bytes([9; 32])
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hashes_are_salted_and_domain_separated() {
        let a = Salt::from_bytes([1; 32]);
        let b = Salt::from_bytes([2; 32]);
        assert_eq!(a.hash8("key", "x"), a.hash8("key", "x"));
        assert_ne!(a.hash8("key", "x"), b.hash8("key", "x"));
        assert_ne!(a.hash8("key", "x"), a.hash8("host", "x"));
        assert_eq!(a.hash12("key", "x").len(), 12);
    }

    #[test]
    fn salt_file_is_created_once_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("debug").join(".salt");
        let a = Salt::load_or_create(&p).unwrap();
        let b = Salt::load_or_create(&p).unwrap();
        assert_eq!(a.hash8("k", "v"), b.hash8("k", "v"));
        assert_eq!(std::fs::read_to_string(&p).unwrap().trim().len(), 64);
    }

    #[test]
    fn url_reduction_drops_userinfo_query_and_fragment() {
        let u = reduce_url(
            &salt(),
            "https://user:pw@Example.COM:8443/a/b/c?token=zzz#frag",
        )
        .unwrap();
        assert_eq!(u.scheme.as_str(), "https");
        assert_eq!(u.path_depth, 3);
        let json = serde_json::to_string(&u).unwrap();
        assert!(
            !json.contains("zzz")
                && !json.contains("pw")
                && !json.to_lowercase().contains("example")
        );
        let same = reduce_url(&salt(), "https://example.com:8443/").unwrap();
        assert_eq!(same.host, u.host);
        assert!(reduce_url(&salt(), "no scheme").is_none());
    }

    #[test]
    fn launcher_names() {
        assert_eq!(launcher_basename("C:\\tools\\npx.cmd").as_str(), "npx");
        assert_eq!(launcher_basename("/usr/bin/uvx").as_str(), "uvx");
        assert_eq!(launcher_basename("node.EXE").as_str(), "node");
    }

    fn ctx() -> ScrubContext {
        ScrubContext::new([
            ("API_KEY".to_string(), "hunter2-hunter2".to_string()),
            ("short".to_string(), "ab".to_string()),
        ])
        .with_home("C:\\Users\\example")
        .with_home("/home/example")
        .with_user("example")
        .with_host("devbox01")
        .with_cwd("/work/project-x")
    }

    #[test]
    fn scrub_examples() {
        let c = ctx();
        let s = salt();
        let args = ArgSet::from_pairs([("/query", "needle-in-haystack")]);
        let sc = Scrubber::new(&c, &s, &args);
        let cases = [
            ("key hunter2-hunter2 used", "key <env:API_KEY> used"),
            (
                "bad query needle-in-haystack here",
                "bad query <arg:/query> here",
            ),
            (
                "open C:\\Users\\Example\\notes.txt failed",
                "open <path>/notes.txt failed",
            ),
            ("at <home>/src/index.js:41:17", "at <path>/index.js:41:17"),
            (
                "Authorization: Bearer abc.def.ghi",
                "Authorization: Bearer <redacted:11>",
            ),
            // The ctx user "example" is scrubbed inside the domain first, so the
            // span is "user@<user>.com" (15 chars).
            ("mail user@example.com now", "mail <redacted:15> now"),
            ("peer 192.0.2.40:8080 down", "peer <redacted:10>:8080 down"),
            (
                "GET https://api.example.com/v1/items?token=abc",
                "GET https://<host#",
            ),
            (
                "password=swordfish1 rejected",
                "password=<redacted:10> rejected",
            ),
        ];
        for (input, want) in cases {
            let got = sc.scrub(input);
            assert!(
                got.as_str().starts_with(want) || got.as_str() == want,
                "{input:?} -> {:?}",
                got.as_str()
            );
        }
        assert!(sc.scrub("host devbox01 user EXAMPLE").as_str() == "host <host> user <user>");
        assert!(sc.scrub(&"x".repeat(1000)).as_str().chars().count() <= MAX_TEXT + 1);
    }

    #[test]
    fn stderr_lines_that_are_not_errors_keep_only_their_length() {
        let c = ctx();
        let s = salt();
        let a = ArgSet::new();
        let sc = Scrubber::new(&c, &s, &a);
        assert_eq!(
            sc.stderr_line("listening on stdio, ready").as_str(),
            "<line len=25>"
        );
        assert!(sc
            .stderr_line("TypeError: x is null")
            .as_str()
            .starts_with("TypeError"));
        assert!(sc
            .stderr_line("  at foo (/a/b/c.js:1:2)")
            .as_str()
            .contains("<path>/c.js"));
        assert!(sc.stderr_line("spawn ENOENT").as_str().contains("ENOENT"));
        assert!(sc
            .stderr_line("npm ERR! code E404")
            .as_str()
            .contains("npm ERR!"));
    }

    // ------------------------------------------------------ property-style

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn string(&mut self, alphabet: &str, len: usize) -> String {
            let a: Vec<char> = alphabet.chars().collect();
            (0..len).map(|_| a[self.below(a.len())]).collect()
        }
        /// Alphanumeric with at least one upper, lower and digit.
        fn mixed(&mut self, len: usize) -> String {
            let mut s: Vec<char> = self
                .string(
                    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                    len,
                )
                .chars()
                .collect();
            s[0] = 'Q';
            s[1] = 'z';
            s[2] = '7';
            s.into_iter().collect()
        }
    }

    const ALNUM: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

    #[test]
    fn planted_secrets_do_not_survive_the_scrubber() {
        let words = [
            "failed", "to", "open", "while", "reading", "config", "error", "at", "line", "retry",
            "done",
        ];
        let delims = [" ", ": ", "\n", "=", " (", ", ", " \"", "/"];
        let homes = ["C:\\Users\\example", "/home/example"];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..600 {
            let n = 8 + rng.below(16);
            let env_val = rng.string(ALNUM, n);
            let n = 6 + rng.below(24);
            let arg_val = rng.string(ALNUM, n);
            let home = homes[rng.below(2)];
            let c = ScrubContext::new([("SECRET_ENV".to_string(), env_val.clone())])
                .with_home(home)
                .with_user("example");
            let args = ArgSet::from_pairs([("/arg", arg_val.as_str())]);
            let s = salt();
            let sc = Scrubber::new(&c, &s, &args);

            let hex40 = rng.string("0123456789abcdef", 40);
            let b64 = rng.mixed(44);
            let jwt = format!("eyJ{}.{}.{}", rng.mixed(20), rng.mixed(30), rng.mixed(30));
            let bearer = rng.mixed(30);
            let ghp = format!("ghp_{}", rng.mixed(36));
            let sk = format!("sk-{}", rng.mixed(32));
            let email = format!("{}@example.org", rng.string("abcdefghij", 8));
            let ip = format!(
                "{}.{}.{}.{}",
                10 + rng.below(200),
                rng.below(256),
                rng.below(256),
                1 + rng.below(254)
            );
            let query_secret = rng.mixed(18);
            let home_variant = match rng.below(4) {
                0 => home.to_string(),
                1 => home.replace('\\', "/"),
                2 => home.to_uppercase(),
                _ => home.replace('\\', "\\\\"),
            };
            let secret_path = format!("{home_variant}/proj{}/src/main.rs", rng.below(1000));

            // (planted text, substring that must not survive)
            let plants: Vec<(String, String)> = vec![
                (env_val.clone(), env_val.clone()),
                (arg_val.clone(), arg_val.clone()),
                (hex40.clone(), hex40.clone()),
                (b64.clone(), b64.clone()),
                (jwt.clone(), jwt.clone()),
                (format!("Bearer {bearer}"), bearer.clone()),
                (ghp.clone(), ghp.clone()),
                (sk.clone(), sk.clone()),
                (email.clone(), email.clone()),
                (ip.clone(), ip.clone()),
                (
                    format!("https://api.example.com/v1/x?token={query_secret}"),
                    query_secret.clone(),
                ),
                (
                    secret_path.clone(),
                    home.replace('\\', "/")
                        .trim_start_matches("C:/")
                        .to_string(),
                ),
            ];
            let mut text = String::new();
            for (planted, _) in &plants {
                if rng.below(3) > 0 {
                    text.push_str(words[rng.below(words.len())]);
                    text.push_str(delims[rng.below(5)]);
                }
                text.push_str(planted);
                text.push_str(delims[rng.below(delims.len())]);
                text.push_str(words[rng.below(words.len())]);
                text.push(' ');
            }
            // Test each plant alone as well as in the long text (the long
            // text is truncated, which can only remove more).
            for (planted, must_not) in &plants {
                let alone = format!(
                    "{} {planted} {}",
                    words[case % words.len()],
                    words[(case + 3) % words.len()]
                );
                let out = sc.scrub(&alone);
                let lower = out.as_str().to_ascii_lowercase();
                assert!(
                    !out.as_str().contains(must_not.as_str())
                        && !lower.contains(&must_not.to_ascii_lowercase()),
                    "case {case}: {must_not:?} survived in {:?} (from {alone:?})",
                    out.as_str()
                );
                assert!(
                    !lower.contains("users\\example")
                        && !lower.contains("home/example")
                        && !lower.contains("users/example")
                );
            }
            let out = sc.scrub(&text);
            assert!(out.as_str().chars().count() <= MAX_TEXT + 1);
            for (_, must_not) in &plants {
                assert!(
                    !out.as_str().contains(must_not.as_str()),
                    "case {case}: {must_not:?} in {:?}",
                    out.as_str()
                );
            }
        }
    }
}
