// SPDX-License-Identifier: GPL-3.0-or-later
//! Import planning: pure, read-only.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use serde::Serialize;

use crate::config::{sanitize_prefix, Config, EnvValue, Scope, ServerConfig, Transport};

use super::parse::{agent_files, exists, parse_file, RawServer, RawSpec, RawValue};
use super::plugins::CWD_MARKER;
use super::{Agent, ImportEnv, ImportError};

/// Where an entry lives in an agent file (used to remove it again).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Source {
    pub file: PathBuf,
    /// Path of object/table keys leading to the map holding the entry.
    pub container: Vec<String>,
    pub key: String,
    pub project_scoped: bool,
}

/// The plugin a server comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginRef {
    /// `<plugin>@<marketplace>`.
    pub id: String,
    pub plugin: String,
    pub server: String,
    /// How the agent names the server to disable it: `plugin:<plugin>:<server>`.
    pub disable_as: String,
    /// Plugin install directory (read only).
    pub root: PathBuf,
    /// Uses `${CLAUDE_PROJECT_DIR}`.
    pub per_cwd: bool,
}

/// A secret the user must provide through the environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NeedsEnvVar {
    /// Original key (env var or header name). Never the value.
    pub key: String,
    pub suggested_env_var: String,
    /// `env` or `header`.
    pub location: String,
}

/// One server to migrate.
#[derive(Debug, Clone, Serialize)]
pub struct PlanItem {
    pub name: String,
    pub prefix: String,
    pub transport: Transport,
    pub command: Option<String>,
    pub args: Vec<String>,
    /// Environment KEY names only.
    pub env_keys: Vec<String>,
    pub url: Option<String>,
    pub header_names: Vec<String>,
    /// A `headersHelper` command is migrated (never its output).
    pub headers_helper: bool,
    pub scope: Scope,
    pub scope_reason: Option<String>,
    pub needs_env_var: Vec<NeedsEnvVar>,
    pub warnings: Vec<String>,
    pub sources: Vec<Source>,
    /// An identical server is already in the broker config.
    pub already_registered: bool,
    /// Provided by an agent plugin: registered with the broker, but the
    /// plugin is never changed (`sources` is empty).
    pub plugin: Option<PluginRef>,
    #[serde(skip)]
    pub config: ServerConfig,
}

/// An entry left untouched.
#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub name: String,
    pub file: PathBuf,
    pub reason: String,
}

/// The full import plan.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub agent: Agent,
    pub broker_url: String,
    pub broker_config: PathBuf,
    pub token_env_var: String,
    pub items: Vec<PlanItem>,
    pub skipped: Vec<Skipped>,
    pub warnings: Vec<String>,
    /// Agent files that will be rewritten.
    pub files: Vec<PathBuf>,
}

impl Plan {
    /// True if applying would change anything: an agent file to rewrite or
    /// a server not yet in the broker config. (Plugin servers stay in their
    /// plugin, so a repeated import lists them as already registered.)
    pub fn has_changes(&self) -> bool {
        !self.files.is_empty() || self.items.iter().any(|i| !i.already_registered)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// Human-readable plan. Contains key names only, never values.
    pub fn render_text(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "Import plan: {} -> broker {}",
            self.agent.as_str(),
            self.broker_url
        );
        let _ = writeln!(s, "Broker config: {}", self.broker_config.display());
        if self.items.is_empty() {
            let _ = writeln!(s, "Nothing to migrate.");
        }
        for it in &self.items {
            let t = match it.transport {
                Transport::Stdio => "stdio",
                Transport::Http => "http",
            };
            let scope = match it.scope {
                Scope::Shared => "shared",
                Scope::PerCwd => "per-cwd",
                Scope::PerSession => "per-session",
            };
            let _ = write!(
                s,
                "\n  {} [{t}] prefix={} scope={scope}",
                it.name, it.prefix
            );
            if it.already_registered {
                let _ = write!(s, " (already in broker config)");
            }
            s.push('\n');
            if let Some(r) = &it.scope_reason {
                let _ = writeln!(s, "      scope reason: {r}");
            }
            if let Some(c) = &it.command {
                let _ = writeln!(s, "      command: {c} {}", it.args.join(" "));
            }
            if let Some(u) = &it.url {
                let _ = writeln!(s, "      url: {u}");
            }
            if !it.env_keys.is_empty() {
                let _ = writeln!(s, "      env keys: {}", it.env_keys.join(", "));
            }
            if !it.header_names.is_empty() {
                let _ = writeln!(s, "      header names: {}", it.header_names.join(", "));
            }
            if it.headers_helper {
                let _ = writeln!(s, "      headers helper: yes (command run by the broker)");
            }
            for n in &it.needs_env_var {
                let _ = writeln!(
                    s,
                    "      NEEDS ENV VAR: {} `{}` is a literal secret and is not migrated; set {} before use",
                    n.location, n.key, n.suggested_env_var
                );
            }
            for w in &it.warnings {
                let _ = writeln!(s, "      warning: {w}");
            }
            for src in &it.sources {
                let _ = writeln!(s, "      from: {}", src.file.display());
            }
            if let Some(p) = &it.plugin {
                let _ = writeln!(
                    s,
                    "      from plugin: {} (not changed; disable in agent as {})",
                    p.id, p.disable_as
                );
            }
        }
        for sk in &self.skipped {
            let _ = writeln!(
                s,
                "\n  skipped {} ({}): {}",
                sk.name,
                sk.file.display(),
                sk.reason
            );
        }
        if !self.files.is_empty() {
            s.push_str("\nFiles to be rewritten (backed up first):\n");
            for f in &self.files {
                let _ = writeln!(s, "  {}", f.display());
            }
        }
        for w in &self.warnings {
            let _ = writeln!(s, "\nWarning: {w}");
        }
        s
    }
}

// ----------------------------------------------------------------- helpers

/// Does a literal look like a secret? Reuses the broker's own validation so
/// the plan never produces a config the broker would reject.
fn secret_literal(key: &str, value: &str, header: bool) -> bool {
    let mut cfg = Config::default();
    let mut s = blank(Transport::Stdio);
    s.command = Some("x".into());
    if header {
        s.transport = Transport::Http;
        s.command = None;
        s.url = Some("http://example.com/".into());
        s.headers
            .insert(key.to_string(), EnvValue::Literal(value.to_string()));
    } else {
        s.env
            .insert(key.to_string(), EnvValue::Literal(value.to_string()));
    }
    cfg.servers.insert("probe".into(), s);
    match cfg.validate() {
        Ok(()) => false,
        Err(errs) => errs
            .iter()
            .any(|e| e.field.contains(".env.") || e.field.contains(".headers.")),
    }
}

/// A default server of `transport`, built through the config's own
/// deserialisation so new config fields never break this module.
fn blank(transport: Transport) -> ServerConfig {
    let t = match transport {
        Transport::Stdio => "stdio",
        Transport::Http => "http",
    };
    serde_json::from_value(serde_json::json!({ "transport": t }))
        .expect("transport alone is a valid server table")
}

fn sanitize_name(raw: &str) -> String {
    let mut n: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    n.truncate(64);
    if n.is_empty() {
        "server".into()
    } else {
        n
    }
}

fn unique_name(base: &str, taken: &BTreeSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    for i in 2.. {
        let suffix = format!("-{i}");
        let mut stem = base.to_string();
        stem.truncate(64 - suffix.len());
        let cand = format!("{stem}{suffix}");
        if !taken.contains(&cand) {
            return cand;
        }
    }
    unreachable!()
}

fn suggest_var(server: &str, key: &str) -> String {
    let up = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    format!("MCPDOZE_{}_{}", up(server), up(key))
}

fn norm_url(u: &str) -> String {
    u.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn url_has_credentials(u: &str) -> bool {
    let rest = u.split_once("://").map(|x| x.1).unwrap_or(u);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return true;
    }
    if let Some((_, q)) = u.split_once('?') {
        for pair in q.split('&') {
            let k = pair.split('=').next().unwrap_or("").to_ascii_uppercase();
            if ["KEY", "TOKEN", "SECRET", "PASSWORD", "AUTH"]
                .iter()
                .any(|w| k.contains(w))
            {
                return true;
            }
        }
    }
    false
}

fn arg_is_secret(args: &[String]) -> bool {
    let mut prev_flag_secret = false;
    for a in args {
        let flag_secret = |f: &str| {
            let up = f.to_ascii_uppercase();
            ["KEY", "TOKEN", "SECRET", "PASSWORD"]
                .iter()
                .any(|w| up.contains(w))
        };
        if let Some(rest) = a.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                if flag_secret(k) && !v.is_empty() && !v.starts_with("${") {
                    return true;
                }
                if secret_literal("ARG", v, false) {
                    return true;
                }
                prev_flag_secret = false;
            } else {
                prev_flag_secret = flag_secret(rest);
            }
            continue;
        }
        if prev_flag_secret && !a.starts_with("${") {
            return true;
        }
        prev_flag_secret = false;
        if secret_literal("ARG", a, false) {
            return true;
        }
    }
    false
}

fn arg_marks_cwd(a: &str) -> bool {
    a == "--dir"
        || a.starts_with("--dir=")
        || a == "."
        || a.contains("${workspaceFolder}")
        || a.contains("${workspaceRoot}")
}

struct Candidate {
    orig_name: String,
    config: ServerConfig,
    needs: Vec<NeedsEnvVar>,
    warnings: Vec<String>,
    sources: Vec<Source>,
    plugin: Option<PluginRef>,
}

/// Puts the broker's `${cwd}` where a plugin used `${CLAUDE_PROJECT_DIR}`.
fn restore_cwd(c: &mut ServerConfig) {
    let fix = |s: &mut String| {
        if s.contains(CWD_MARKER) {
            *s = s.replace(CWD_MARKER, crate::backend::CWD_VAR);
        }
    };
    if let Some(cmd) = &mut c.command {
        fix(cmd);
    }
    c.args.iter_mut().for_each(fix);
    for v in c.env.values_mut().chain(c.headers.values_mut()) {
        if let EnvValue::Literal(s) = v {
            fix(s);
        }
    }
    if let Some(u) = &mut c.url {
        fix(u);
    }
    if let Some(d) = &mut c.cwd {
        let mut s = d.to_string_lossy().into_owned();
        fix(&mut s);
        *d = PathBuf::from(s);
    }
}

/// Convert key/value pairs into broker env values.
fn convert_map(
    server: &str,
    pairs: &[(String, RawValue)],
    header: bool,
    needs: &mut Vec<NeedsEnvVar>,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, EnvValue> {
    let mut out = BTreeMap::new();
    for (k, v) in pairs {
        let val = match v {
            RawValue::EnvRef(var) => EnvValue::Ref {
                from_env: var.clone(),
            },
            RawValue::PrefixedRef { var, .. } => {
                warnings.push(format!(
                    "{} `{k}` uses a prefix before ${{{var}}}; the broker needs variable {var} to hold the full value",
                    if header { "header" } else { "env" }
                ));
                EnvValue::Ref {
                    from_env: var.clone(),
                }
            }
            RawValue::Literal(s) => {
                if secret_literal(k, s, header) {
                    let var = suggest_var(server, k);
                    needs.push(NeedsEnvVar {
                        key: k.clone(),
                        suggested_env_var: var.clone(),
                        location: if header { "header" } else { "env" }.into(),
                    });
                    EnvValue::Ref { from_env: var }
                } else {
                    if s.contains("${") {
                        warnings.push(format!(
                            "{} `{k}` contains a variable expansion that the broker will not expand",
                            if header { "header" } else { "env" }
                        ));
                    }
                    EnvValue::Literal(s.clone())
                }
            }
        };
        out.insert(k.clone(), val);
    }
    out
}

fn skip(raw: &RawServer, reason: &str) -> Skipped {
    Skipped {
        name: raw.name.clone(),
        file: raw.source.file.clone(),
        reason: reason.into(),
    }
}

// ------------------------------------------------------------------- plan

/// Build a plan for `agent`. Reads agent files and the broker config; writes
/// nothing. `scope_override` forces one scope for every item.
pub fn build_plan(
    agent: Agent,
    env: &ImportEnv,
    scope_override: Option<Scope>,
) -> Result<Plan, ImportError> {
    build_plan_with(
        agent,
        env,
        &PlanOptions {
            scope: scope_override,
            ..PlanOptions::default()
        },
    )
}

/// Planning options.
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// Force one scope for every item.
    pub scope: Option<Scope>,
    /// Import `headersHelper` commands from project-scoped configs too
    /// (user-scoped ones are always imported). Helpers are never run while
    /// planning.
    pub allow_helpers: bool,
}

/// [uild_plan] with all options.
pub fn build_plan_with(
    agent: Agent,
    env: &ImportEnv,
    opts: &PlanOptions,
) -> Result<Plan, ImportError> {
    let scope_override = opts.scope;
    let existing = if exists(&env.broker_config) {
        Config::load(&env.broker_config).map_err(|e| {
            ImportError::Invalid(format!(
                "{}: cannot read broker config: {e}",
                env.broker_config.display()
            ))
        })?
    } else {
        Config::default()
    };
    let broker_norm = norm_url(&env.broker_url);

    let mut warnings = Vec::new();
    let mut skipped = Vec::new();
    let mut cands: Vec<Candidate> = Vec::new();
    let mut found_any_file = false;
    let mut raws: Vec<(RawServer, bool)> = Vec::new();

    for file in agent_files(agent, env) {
        if !exists(&file.path) {
            continue;
        }
        found_any_file = true;
        let parsed = match parse_file(agent, &file) {
            Ok(p) => p,
            Err(e) => {
                warnings.push(format!("skipped unreadable file: {e}"));
                continue;
            }
        };
        let lenient = parsed.lenient;
        raws.extend(parsed.servers.into_iter().map(|r| (r, lenient)));
    }
    if agent == Agent::ClaudeCode {
        let scan = super::plugins::scan(env);
        warnings.extend(scan.warnings);
        for p in scan.servers {
            let raw = RawServer {
                name: p.server.clone(),
                spec: p.spec,
                disabled: false,
                uses_inputs: false,
                source: Source {
                    file: p.root.clone(),
                    container: Vec::new(),
                    key: p.server.clone(),
                    project_scoped: false,
                },
                plugin: Some(PluginRef {
                    disable_as: format!("plugin:{}:{}", p.plugin, p.server),
                    id: p.id.clone(),
                    plugin: p.plugin,
                    server: p.server,
                    root: p.root,
                    per_cwd: p.per_cwd,
                }),
            };
            if !p.enabled {
                skipped.push(skip(&raw, &format!("plugin `{}` is disabled", p.id)));
                continue;
            }
            if let Some(why) = &p.skip {
                skipped.push(skip(&raw, why));
                continue;
            }
            raws.push((raw, false));
        }
    }
    {
        for (raw, lenient) in raws {
            if lenient {
                skipped.push(skip(
                    &raw,
                    "file contains comments or trailing commas; rewriting would drop them (edit by hand or remove them first)",
                ));
                continue;
            }
            if raw.disabled {
                skipped.push(skip(&raw, "disabled"));
                continue;
            }
            if raw.uses_inputs {
                skipped.push(skip(
                    &raw,
                    "uses ${input:...} prompts the broker cannot resolve",
                ));
                continue;
            }
            let name = sanitize_name(&raw.name);
            let mut needs = Vec::new();
            let mut cwarn = Vec::new();
            let config = match &raw.spec {
                RawSpec::Unsupported(why) => {
                    skipped.push(skip(&raw, why));
                    continue;
                }
                RawSpec::Http { sse: true, .. } => {
                    skipped.push(skip(
                        &raw,
                        "legacy SSE transport is not supported by the broker",
                    ));
                    continue;
                }
                RawSpec::Http {
                    url,
                    headers,
                    headers_helper,
                    ..
                } => {
                    if norm_url(url) == broker_norm {
                        skipped.push(skip(&raw, "already points at the broker"));
                        continue;
                    }
                    if url_has_credentials(url) {
                        skipped.push(skip(
                            &raw,
                            "url embeds credentials; move them to a header or environment first",
                        ));
                        continue;
                    }
                    let mut c = blank(Transport::Http);
                    c.url = Some(url.clone());
                    c.headers = convert_map(&name, headers, true, &mut needs, &mut cwarn);
                    if let Some(h) = headers_helper {
                        if raw.source.project_scoped && !opts.allow_helpers {
                            skipped.push(skip(
                                &raw,
                                "project-scoped headersHelper runs a command from the project; review it and pass --allow-helpers to import it",
                            ));
                            continue;
                        }
                        cwarn.push(
                            "headersHelper is run by the broker (on connect and after a 401/403); its output is never logged".into(),
                        );
                        c.headers_helper = Some(h.clone());
                    }
                    c
                }
                RawSpec::Stdio { command, args, env } => {
                    if arg_is_secret(args) {
                        skipped.push(skip(
                            &raw,
                            "an argument looks like a secret; move it to an environment variable first",
                        ));
                        continue;
                    }
                    let mut c = blank(Transport::Stdio);
                    c.command = Some(command.clone());
                    c.args = args.clone();
                    c.env = convert_map(&name, env, false, &mut needs, &mut cwarn);
                    c
                }
            };
            let mut config = config;
            restore_cwd(&mut config);
            if let Some(p) = &raw.plugin {
                cwarn.push(format!(
                    "provided by plugin `{}`: the plugin's files are not changed; disable it in the agent as {} (or disable the plugin), otherwise its tools are listed twice (mcp__plugin_{}_{}__* and mcp__{}__<prefix>__*)",
                    p.id,
                    p.disable_as,
                    p.plugin,
                    p.server,
                    super::BROKER_ENTRY_NAME
                ));
            }
            // Merge with an identical candidate of the same name.
            if let Some(c) = cands.iter_mut().find(|c| {
                c.orig_name == name
                    && c.config == config
                    && c.needs == needs
                    && c.plugin == raw.plugin
            }) {
                if raw.plugin.is_none() {
                    c.sources.push(raw.source.clone());
                }
                continue;
            }
            cands.push(Candidate {
                orig_name: name,
                config,
                needs,
                warnings: cwarn,
                sources: if raw.plugin.is_none() {
                    vec![raw.source.clone()]
                } else {
                    Vec::new()
                },
                plugin: raw.plugin.clone(),
            });
        }
    }
    if !found_any_file {
        warnings.push(format!("no {} config files found", agent.as_str()));
    }

    // Names, prefixes, scopes.
    let mut taken_names: BTreeSet<String> = existing.servers.keys().cloned().collect();
    let mut taken_prefixes: BTreeSet<String> = existing.prefixes().keys().cloned().collect();
    let mut planned_names: BTreeSet<String> = BTreeSet::new();
    let mut items: Vec<PlanItem> = Vec::new();

    for c in cands {
        let mut name = c.orig_name.clone();
        let mut already = false;
        let mut config = c.config.clone();
        if let Some(ex) = existing.servers.get(&name) {
            let same = {
                let mut e = ex.clone();
                e.prefix = None;
                e.scope = Scope::Shared;
                let mut mine = config.clone();
                mine.scope = Scope::Shared;
                e == mine
            };
            if same {
                already = true;
                config = ex.clone();
            }
        }
        if !already {
            let mut avoid = taken_names.clone();
            avoid.extend(planned_names.iter().cloned());
            let unique = unique_name(&name, &avoid);
            if unique != name {
                warnings.push(format!(
                    "server name `{name}` is already used with a different definition; registering it as `{unique}`"
                ));
                name = unique;
            }
        }
        planned_names.insert(name.clone());
        taken_names.insert(name.clone());

        let prefix = if already {
            config.effective_prefix(&name)
        } else {
            let mut p = sanitize_prefix(&name);
            if p.is_empty() {
                p = "server".into();
            }
            let base = p.clone();
            let mut n = 2;
            while taken_prefixes.contains(&p) {
                let suffix = format!("_{n}");
                let mut stem = base.clone();
                stem.truncate(crate::config::MAX_PREFIX_LEN - suffix.len());
                p = format!("{}{suffix}", stem.trim_end_matches('_'));
                n += 1;
            }
            if p != sanitize_prefix(&name) {
                config.prefix = Some(p.clone());
            }
            taken_prefixes.insert(p.clone());
            p
        };

        let (scope, reason) = if let Some(s) = scope_override {
            (s, Some("set with --scope".to_string()))
        } else if c.sources.iter().any(|s| s.project_scoped) {
            (
                Scope::PerCwd,
                Some("entry comes from a project-scoped config".to_string()),
            )
        } else if c.plugin.as_ref().is_some_and(|p| p.per_cwd) {
            (
                Scope::PerCwd,
                Some("plugin server uses ${CLAUDE_PROJECT_DIR}".to_string()),
            )
        } else if config.args.iter().any(|a| arg_marks_cwd(a)) {
            (
                Scope::PerCwd,
                Some("arguments reference the working directory".to_string()),
            )
        } else {
            (Scope::Shared, None)
        };
        if !already {
            config.scope = scope;
        }
        items.push(PlanItem {
            name,
            prefix,
            transport: config.transport,
            command: config.command.clone(),
            args: config.args.clone(),
            env_keys: config.env.keys().cloned().collect(),
            url: config.url.clone(),
            header_names: config.headers.keys().cloned().collect(),
            headers_helper: config.headers_helper.is_some(),
            scope: if already { config.scope } else { scope },
            scope_reason: if already { None } else { reason },
            needs_env_var: c.needs,
            warnings: c.warnings,
            sources: c.sources,
            already_registered: already,
            plugin: c.plugin,
            config,
        });
    }

    let files: Vec<PathBuf> = items
        .iter()
        .flat_map(|i| i.sources.iter().map(|s| s.file.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    if !items.is_empty() {
        warnings.push(format!(
            "tool names will become mcp__{}__<prefix>__<tool>; update permission allowlists",
            super::BROKER_ENTRY_NAME
        ));
        warnings.push(format!(
            "set environment variable {} to the broker token (`mcpdoze token`) before starting the agent; the token is never written to any file",
            env.token_env_var
        ));
    }

    Ok(Plan {
        agent,
        broker_url: env.broker_url.clone(),
        broker_config: env.broker_config.clone(),
        token_env_var: env.token_env_var.clone(),
        items,
        skipped,
        warnings,
        files,
    })
}
