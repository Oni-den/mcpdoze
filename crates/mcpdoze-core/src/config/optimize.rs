// SPDX-License-Identifier: GPL-3.0-or-later
//! `[servers.<name>.optimize]` and `[servers.<name>.tools.<tool>]`: the
//! token-optimisation settings (`docs/OPTIMIZE.md`).
//!
//! Everything here is opt-in. A server without an `optimize` table has
//! `optimize == None` and no `tools` entries, which the rest of the broker
//! treats exactly as before.
//!
//! This module holds the config types, their validation, the compiled
//! matchers the router will use (tool-name globs and JSON-pointer globs) and
//! the reload comparison. Neither table enters the backend registration
//! (`admin::Applied`) or the catalog fingerprint, so a change never unloads a
//! backend; [`optimize_changed`] tells the admin apply whether a
//! `notifications/tools/list_changed` is due.
//!
//! Runtime-only checks are not done here: `default_args` keys are validated
//! against the live catalog `inputSchema`, and `cache.on_hit = "reference"`
//! is refused for tools with an `outputSchema`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize, Serializer};

use super::{de_duration, format_duration, is_false, Config, ServerConfig, ValidationError};

/// Smallest accepted non-zero `max_result_bytes`.
pub const MIN_MAX_RESULT_BYTES: u64 = 512;

/// Output encoding of text results (`encode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Encode {
    /// Results are passed through unchanged (default).
    #[default]
    None,
    /// TOON-style text encoding, used only when it round-trips losslessly and
    /// is at least 20% smaller.
    Toon,
}

fn is_default_encode(e: &Encode) -> bool {
    *e == Encode::None
}

/// Batch meta-tool (`mcpdoze__batch`) availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BatchMode {
    /// No batch meta-tool (default).
    #[default]
    Off,
    /// Only tools annotated `readOnlyHint = true` may be batched.
    ReadOnly,
    /// Any tool may be batched. Bypasses per-tool approval; explicit only.
    All,
}

fn is_default_batch(b: &BatchMode) -> bool {
    *b == BatchMode::Off
}

/// What a cache hit returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheHit {
    /// The full cached result (default).
    #[default]
    Full,
    /// A stub plus a result ref (`mcpdoze__result_get`).
    Reference,
}

/// `cache = { ttl = "60s", tools = ["get_*"], on_hit = "full" }`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// How long an entry lives. Must be greater than zero.
    #[serde(deserialize_with = "de_duration", serialize_with = "ser_duration")]
    pub ttl: Duration,
    /// Tool-name globs that may be cached. Must not be empty. A tool is cached
    /// only if it matches here and is annotated `readOnlyHint = true`.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Cache hit shape.
    #[serde(default, skip_serializing_if = "is_default_hit")]
    pub on_hit: CacheHit,
}

fn is_default_hit(h: &CacheHit) -> bool {
    *h == CacheHit::Full
}

fn ser_duration<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format_duration(*d))
}

/// `[servers.<name>.optimize]`. Unknown keys are rejected.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizeConfig {
    /// Tool-name globs (on backend names) that are listed. Empty = all tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_allow: Vec<String>,
    /// Tool-name globs that are hidden and refused. Wins over `tools_allow`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_deny: Vec<String>,
    /// Cut listed descriptions to this many characters at a sentence boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_max_chars: Option<u32>,
    /// Remove `description`, `title` and `examples` inside `inputSchema` only.
    #[serde(default, skip_serializing_if = "is_false")]
    pub strip_schema_descriptions: bool,
    /// Re-serialise pretty-printed JSON text blocks compactly.
    #[serde(default, skip_serializing_if = "is_false")]
    pub minify_json: bool,
    /// Text result encoding.
    #[serde(default, skip_serializing_if = "is_default_encode")]
    pub encode: Encode,
    /// Results larger than this are truncated and stored for
    /// `mcpdoze__result_get`. `0` = off; otherwise at least
    /// [`MIN_MAX_RESULT_BYTES`].
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub max_result_bytes: u64,
    /// Session result cache for listed read-only tools (off when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheConfig>,
    /// Batch meta-tool availability.
    #[serde(default, skip_serializing_if = "is_default_batch")]
    pub batch: BatchMode,
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

/// `[servers.<name>.tools.<tool>]`: per-tool argument and result settings,
/// keyed by the backend tool name.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOptConfig {
    /// Arguments merged in only for keys the agent omitted. Keys are checked
    /// against the catalog `inputSchema` at runtime, not here.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub default_args: serde_json::Map<String, serde_json::Value>,
    /// JSON-pointer globs of the result to keep. Mutually exclusive with `drop`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep: Vec<String>,
    /// JSON-pointer globs of the result to remove. Mutually exclusive with `keep`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drop: Vec<String>,
}

// ------------------------------------------------------------ glob matching

/// One compiled glob token.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// A literal character.
    Lit(char),
    /// `?`: exactly one character.
    One,
    /// `*`: any run of characters (including none).
    Star,
    /// `[...]` / `[!...]`: one character from (or outside) the ranges.
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

impl Tok {
    fn accepts(&self, c: char) -> bool {
        match self {
            Tok::Lit(l) => *l == c,
            Tok::One => true,
            Tok::Star => false,
            Tok::Class { negated, ranges } => {
                let inside = ranges.iter().any(|(lo, hi)| *lo <= c && c <= *hi);
                inside != *negated
            }
        }
    }
}

/// Compile glob syntax: `*`, `?`, `[abc]`, `[a-z]`, `[!x]`. Other characters
/// are literal. Allows the empty pattern (it matches only the empty string).
fn compile_tokens(pat: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let mut chars = pat.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                // Collapse runs of `*` so the matcher stays linear.
                if out.last() != Some(&Tok::Star) {
                    out.push(Tok::Star);
                }
            }
            '?' => out.push(Tok::One),
            '[' => {
                let negated = matches!(chars.peek(), Some('!' | '^'));
                if negated {
                    chars.next();
                }
                let mut ranges = Vec::new();
                let mut closed = false;
                // A `]` straight after `[` (or `[!`) is a literal.
                let mut first = true;
                while let Some(x) = chars.next() {
                    if x == ']' && !first {
                        closed = true;
                        break;
                    }
                    first = false;
                    if chars.peek() == Some(&'-') {
                        let mut look = chars.clone();
                        look.next();
                        if let Some(hi) = look.next() {
                            if hi != ']' {
                                chars.next();
                                chars.next();
                                if hi < x {
                                    return Err(format!("range `{x}-{hi}` is reversed in `{pat}`"));
                                }
                                ranges.push((x, hi));
                                continue;
                            }
                        }
                    }
                    ranges.push((x, x));
                }
                if !closed {
                    return Err(format!("unclosed `[` in `{pat}`"));
                }
                out.push(Tok::Class { negated, ranges });
            }
            _ => out.push(Tok::Lit(c)),
        }
    }
    Ok(out)
}

/// Match compiled tokens against a whole string.
fn glob_match(p: &[Tok], s: &[char]) -> bool {
    let (mut pi, mut si) = (0usize, 0usize);
    // Position just after the last `*`, and the input position it began at.
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if let Some(tok) = p.get(pi) {
            match tok {
                Tok::Star => {
                    star = Some((pi + 1, si));
                    pi += 1;
                    continue;
                }
                t if t.accepts(s[si]) => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some((sp, ss)) => {
                pi = sp;
                si = ss + 1;
                star = Some((sp, si));
            }
            None => return false,
        }
    }
    p[pi..].iter().all(|t| *t == Tok::Star)
}

/// A validated glob over backend tool names.
///
/// Syntax: `*` any run, `?` one character, `[abc]`, `[a-z]`, `[!x]` classes.
/// Matching is case-sensitive and against the whole name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobPattern {
    source: String,
    toks: Vec<Tok>,
}

impl GlobPattern {
    /// Compile one pattern. The empty pattern is refused.
    pub fn new(pattern: &str) -> Result<Self, String> {
        if pattern.is_empty() {
            return Err("glob must not be empty".into());
        }
        Ok(Self {
            source: pattern.to_string(),
            toks: compile_tokens(pattern)?,
        })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Whether `name` matches the whole pattern.
    pub fn matches(&self, name: &str) -> bool {
        let chars: Vec<char> = name.chars().collect();
        glob_match(&self.toks, &chars)
    }
}

/// Allow and deny lists over tool names, as configured.
///
/// Deny always wins. An empty allow list means every tool not denied is
/// listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolFilter {
    allow: Vec<GlobPattern>,
    deny: Vec<GlobPattern>,
}

impl ToolFilter {
    /// Compile both lists; the first bad pattern is reported with its index.
    pub fn new<S: AsRef<str>>(allow: &[S], deny: &[S]) -> Result<Self, String> {
        let compile = |list: &[S]| {
            list.iter()
                .enumerate()
                .map(|(i, p)| GlobPattern::new(p.as_ref()).map_err(|e| format!("[{i}]: {e}")))
                .collect::<Result<Vec<_>, String>>()
        };
        Ok(Self {
            allow: compile(allow)?,
            deny: compile(deny)?,
        })
    }

    /// Whether a backend tool is listed and callable through the broker.
    pub fn allows(&self, tool: &str) -> bool {
        if self.deny.iter().any(|g| g.matches(tool)) {
            return false;
        }
        self.allow.is_empty() || self.allow.iter().any(|g| g.matches(tool))
    }
}

impl OptimizeConfig {
    /// The compiled tool filter for this server's lists.
    pub fn tool_filter(&self) -> Result<ToolFilter, String> {
        ToolFilter::new(&self.tools_allow, &self.tools_deny)
    }
}

// ------------------------------------------------------ JSON-pointer globs

/// One segment of a pointer glob.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PSeg {
    /// `**`: zero or more whole segments.
    AnyDepth,
    /// One segment, matched with the glob syntax of [`GlobPattern`].
    One(Vec<Tok>),
}

/// Decode one RFC 6901 segment (`~1` is `/`, `~0` is `~`). `None` on a bad
/// escape.
fn decode_segment(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '~' {
            match chars.next() {
                Some('0') => out.push('~'),
                Some('1') => out.push('/'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Split a concrete JSON pointer into decoded segments. The root (`""`) has
/// none. `None` if the pointer is malformed.
fn split_pointer(p: &str) -> Option<Vec<String>> {
    if p.is_empty() {
        return Some(Vec::new());
    }
    let rest = p.strip_prefix('/')?;
    rest.split('/').map(decode_segment).collect()
}

/// A validated JSON-pointer glob, e.g. `/items/*/id`.
///
/// Each segment is a glob over one decoded key or index (`*`, `?`, classes),
/// and a segment that is exactly `**` matches any number of segments,
/// including none. `~0` and `~1` escapes work as in RFC 6901. The pattern
/// must start with `/`; the root is written as `""` in a path but never as a
/// pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerGlob {
    source: String,
    segs: Vec<PSeg>,
}

impl PointerGlob {
    /// Compile one pointer glob.
    pub fn new(pattern: &str) -> Result<Self, String> {
        let rest = pattern
            .strip_prefix('/')
            .ok_or_else(|| "JSON pointer must start with `/`".to_string())?;
        let mut segs = Vec::new();
        for raw in rest.split('/') {
            if raw == "**" {
                segs.push(PSeg::AnyDepth);
                continue;
            }
            let decoded =
                decode_segment(raw).ok_or_else(|| format!("invalid `~` escape in `{pattern}`"))?;
            segs.push(PSeg::One(compile_tokens(&decoded)?));
        }
        Ok(Self {
            source: pattern.to_string(),
            segs,
        })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Whether the concrete pointer `path` is matched by the whole pattern.
    /// A malformed `path` never matches.
    pub fn matches(&self, path: &str) -> bool {
        match split_pointer(path) {
            Some(s) => full_match(&self.segs, &s),
            None => false,
        }
    }

    /// Whether `path` matches, or is an ancestor of something that matches.
    /// A filter that keeps matches must also keep their ancestors, so this is
    /// the test used while walking a result.
    pub fn matches_or_ancestor(&self, path: &str) -> bool {
        match split_pointer(path) {
            Some(s) => prefix_match(&self.segs, &s),
            None => false,
        }
    }
}

fn seg_matches(toks: &[Tok], key: &str) -> bool {
    let chars: Vec<char> = key.chars().collect();
    glob_match(toks, &chars)
}

fn full_match(p: &[PSeg], s: &[String]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((PSeg::AnyDepth, rest)) => {
            full_match(rest, s) || (!s.is_empty() && full_match(p, &s[1..]))
        }
        Some((PSeg::One(t), rest)) => {
            !s.is_empty() && seg_matches(t, &s[0]) && full_match(rest, &s[1..])
        }
    }
}

fn prefix_match(p: &[PSeg], s: &[String]) -> bool {
    if s.is_empty() {
        // Whatever the pattern still needs can be supplied below this node.
        return true;
    }
    match p.split_first() {
        None => false,
        Some((PSeg::AnyDepth, rest)) => prefix_match(rest, s) || prefix_match(p, &s[1..]),
        Some((PSeg::One(t), rest)) => seg_matches(t, &s[0]) && prefix_match(rest, &s[1..]),
    }
}

// ------------------------------------------------------------ validation

/// Validate one server's `optimize` and `tools` tables.
pub(super) fn validate_server(name: &str, s: &ServerConfig, errs: &mut Vec<ValidationError>) {
    if let Some(o) = &s.optimize {
        validate_optimize(name, o, errs);
    }
    for (tool, t) in &s.tools {
        validate_tool(name, tool, t, errs);
    }
}

fn validate_optimize(name: &str, o: &OptimizeConfig, errs: &mut Vec<ValidationError>) {
    let f = |k: &str| format!("servers.{name}.optimize.{k}");
    check_globs(&f("tools_allow"), &o.tools_allow, errs);
    check_globs(&f("tools_deny"), &o.tools_deny, errs);
    if o.description_max_chars == Some(0) {
        errs.push(ValidationError::new(
            f("description_max_chars"),
            "must be greater than 0",
        ));
    }
    if o.max_result_bytes != 0 && o.max_result_bytes < MIN_MAX_RESULT_BYTES {
        errs.push(ValidationError::new(
            f("max_result_bytes"),
            format!("must be 0 (off) or at least {MIN_MAX_RESULT_BYTES}"),
        ));
    }
    if let Some(c) = &o.cache {
        if c.ttl.is_zero() {
            errs.push(ValidationError::new(
                f("cache.ttl"),
                "must be greater than 0",
            ));
        }
        if c.tools.is_empty() {
            errs.push(ValidationError::new(
                f("cache.tools"),
                "must list at least one tool when `cache` is set",
            ));
        }
        check_globs(&f("cache.tools"), &c.tools, errs);
    }
}

fn check_globs(field: &str, list: &[String], errs: &mut Vec<ValidationError>) {
    for (i, p) in list.iter().enumerate() {
        if let Err(m) = GlobPattern::new(p) {
            errs.push(ValidationError::new(format!("{field}[{i}]"), m));
        }
    }
}

fn validate_tool(name: &str, tool: &str, t: &ToolOptConfig, errs: &mut Vec<ValidationError>) {
    let f = |k: &str| format!("servers.{name}.tools.{tool}.{k}");
    if tool.is_empty() {
        errs.push(ValidationError::new(
            format!("servers.{name}.tools"),
            "tool name must not be empty",
        ));
        return;
    }
    if !t.keep.is_empty() && !t.drop.is_empty() {
        errs.push(ValidationError::new(
            f("drop"),
            "`keep` and `drop` are mutually exclusive; set only one",
        ));
    }
    for (key, list) in [("keep", &t.keep), ("drop", &t.drop)] {
        for (i, p) in list.iter().enumerate() {
            if let Err(m) = PointerGlob::new(p) {
                errs.push(ValidationError::new(format!("{}[{i}]", f(key)), m));
            }
        }
    }
}

// ----------------------------------------------------------------- reload

fn optimize_parts(
    s: Option<&ServerConfig>,
) -> (Option<OptimizeConfig>, BTreeMap<String, ToolOptConfig>) {
    match s {
        Some(s) => (s.optimize.clone(), s.tools.clone()),
        None => (None, BTreeMap::new()),
    }
}

/// Names of servers whose `optimize` or `tools` tables differ between `old`
/// and `new`, sorted. A server missing on one side counts as having neither
/// table, so adding or removing a server with no optimisation settings is
/// not a change here.
///
/// The admin apply should set `ReloadResponse::optimize_changed` from this and
/// use it to send `notifications/tools/list_changed`. These tables never
/// cause a respawn.
pub fn changed_servers(old: &Config, new: &Config) -> Vec<String> {
    let names: BTreeSet<&str> = old
        .servers
        .keys()
        .chain(new.servers.keys())
        .map(String::as_str)
        .collect();
    names
        .into_iter()
        .filter(|n| optimize_parts(old.servers.get(*n)) != optimize_parts(new.servers.get(*n)))
        .map(str::to_owned)
        .collect()
}

/// Whether any server's `optimize` or `tools` table changed; see
/// [`changed_servers`].
pub fn optimize_changed(old: &Config, new: &Config) -> bool {
    !changed_servers(old, new).is_empty()
}
