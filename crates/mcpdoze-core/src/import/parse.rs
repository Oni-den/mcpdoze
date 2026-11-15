// SPDX-License-Identifier: GPL-3.0-or-later
//! Agent config parsers. Each yields [`RawServer`] entries with the exact
//! location they came from so the rewrite step can remove them.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::plan::Source;
use super::util::{read_capped, to_text};
use super::{Agent, ImportEnv, ImportError};

/// Whether a config file is user-wide or belongs to the current project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileRole {
    User,
    Project,
}

/// A config file an agent reads.
#[derive(Debug, Clone)]
pub struct AgentFile {
    pub path: PathBuf,
    pub role: FileRole,
}

/// A value as written in an agent config.
#[derive(Clone, PartialEq, Eq)]
pub enum RawValue {
    Literal(String),
    /// Whole value is a reference to an environment variable.
    EnvRef(String),
    /// `<prefix><ref>`, e.g. `Bearer ${TOKEN}`.
    PrefixedRef {
        prefix: String,
        var: String,
    },
}

// Debug never prints literals.
impl std::fmt::Debug for RawValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RawValue::Literal(_) => f.write_str("Literal(<hidden>)"),
            RawValue::EnvRef(v) => write!(f, "EnvRef({v})"),
            RawValue::PrefixedRef { var, .. } => write!(f, "PrefixedRef({var})"),
        }
    }
}

/// What a raw entry describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawSpec {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, RawValue)>,
    },
    Http {
        url: String,
        headers: Vec<(String, RawValue)>,
        /// Legacy SSE transport.
        sse: bool,
        /// `headersHelper` command (Claude Code). Never run by the import.
        headers_helper: Option<String>,
    },
    /// Cannot be migrated; the string says why.
    Unsupported(String),
}

/// One server entry found in an agent config.
#[derive(Debug, Clone)]
pub struct RawServer {
    pub name: String,
    pub spec: RawSpec,
    pub disabled: bool,
    /// Entry mentions inputs/prompts the broker cannot resolve.
    pub uses_inputs: bool,
    pub source: Source,
}

/// Result of parsing one file.
#[derive(Debug, Clone, Default)]
pub struct ParsedFile {
    pub servers: Vec<RawServer>,
    /// File used comments or trailing commas; rewriting would drop them, so
    /// the planner skips its entries.
    pub lenient: bool,
}

/// Config files each agent reads (existing or not).
pub fn agent_files(agent: Agent, env: &ImportEnv) -> Vec<AgentFile> {
    let user = |path: PathBuf| AgentFile {
        path,
        role: FileRole::User,
    };
    let proj = |path: PathBuf| AgentFile {
        path,
        role: FileRole::Project,
    };
    let mut v = match agent {
        Agent::ClaudeCode => vec![
            user(env.home.join(".claude.json")),
            proj(env.cwd.join(".mcp.json")),
        ],
        Agent::Codex => vec![user(env.codex_home.join("config.toml"))],
        Agent::OpenCode => vec![
            user(env.xdg_config.join("opencode").join("opencode.json")),
            proj(env.cwd.join("opencode.json")),
        ],
        Agent::Cursor => vec![
            user(env.home.join(".cursor").join("mcp.json")),
            proj(env.cwd.join(".cursor").join("mcp.json")),
        ],
        Agent::VsCode => vec![
            user(env.config_dir.join("Code").join("User").join("mcp.json")),
            proj(env.cwd.join(".vscode").join("mcp.json")),
        ],
    };
    let mut seen = Vec::new();
    v.retain(|f| {
        if seen.contains(&f.path) {
            false
        } else {
            seen.push(f.path.clone());
            true
        }
    });
    v
}

/// The JSON object key that holds servers when a broker entry is added.
pub(crate) fn json_container_key(agent: Agent) -> &'static str {
    match agent {
        Agent::ClaudeCode | Agent::Cursor => "mcpServers",
        Agent::VsCode => "servers",
        Agent::OpenCode => "mcp",
        Agent::Codex => "mcp_servers",
    }
}

/// Parse one agent file. A missing file is an error; callers check existence.
pub fn parse_file(agent: Agent, file: &AgentFile) -> Result<ParsedFile, ImportError> {
    let text = to_text(&file.path, read_capped(&file.path)?)?;
    parse_text(agent, file, &text)
}

pub(crate) fn parse_text(
    agent: Agent,
    file: &AgentFile,
    text: &str,
) -> Result<ParsedFile, ImportError> {
    if agent == Agent::Codex {
        return parse_codex(file, text);
    }
    let perr = |m: String| ImportError::Parse {
        path: file.path.clone(),
        message: m,
    };
    let (root, lenient) = match serde_json::from_str::<Value>(text) {
        Ok(v) => (v, false),
        Err(_) => {
            let cleaned = strip_jsonc(text);
            match serde_json::from_str::<Value>(&cleaned) {
                Ok(v) => (v, true),
                Err(e) => return Err(perr(e.to_string())),
            }
        }
    };
    let mut out = ParsedFile {
        servers: Vec::new(),
        lenient,
    };
    // (container path, project scoped)
    let mut containers: Vec<(Vec<String>, bool)> = Vec::new();
    let project_file = file.role == FileRole::Project;
    match agent {
        Agent::ClaudeCode => {
            containers.push((vec!["mcpServers".into()], project_file));
            if !project_file {
                if let Some(projects) = root.get("projects").and_then(Value::as_object) {
                    for p in projects.keys() {
                        containers.push((
                            vec!["projects".into(), p.clone(), "mcpServers".into()],
                            true,
                        ));
                    }
                }
            }
        }
        Agent::Cursor | Agent::VsCode => {
            containers.push((vec!["mcpServers".into()], project_file));
            containers.push((vec!["servers".into()], project_file));
        }
        Agent::OpenCode => containers.push((vec!["mcp".into()], project_file)),
        Agent::Codex => unreachable!(),
    }
    for (path, project_scoped) in containers {
        let Some(map) = path
            .iter()
            .try_fold(&root, |v, k| v.get(k))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (key, entry) in map {
            let (spec, disabled) = if agent == Agent::OpenCode {
                opencode_spec(entry)
            } else {
                (json_spec(entry), false)
            };
            let uses_inputs = entry.to_string().contains("${input:");
            out.servers.push(RawServer {
                name: key.clone(),
                spec,
                disabled,
                uses_inputs,
                source: Source {
                    file: file.path.clone(),
                    container: path.clone(),
                    key: key.clone(),
                    project_scoped,
                },
            });
        }
    }
    Ok(out)
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().map(value_to_string).collect())
        .unwrap_or_default()
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn kv_map(v: Option<&Value>) -> Vec<(String, RawValue)> {
    v.and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), classify(&value_to_string(v))))
                .collect()
        })
        .unwrap_or_default()
}

/// Claude Code / Cursor / VS Code entry.
fn json_spec(v: &Value) -> RawSpec {
    let Some(o) = v.as_object() else {
        return RawSpec::Unsupported("entry is not an object".into());
    };
    let ty = o.get("type").and_then(Value::as_str).unwrap_or("");
    if let Some(cmd) = o.get("command").and_then(Value::as_str) {
        if ty.is_empty() || ty == "stdio" {
            return RawSpec::Stdio {
                command: cmd.to_string(),
                args: str_list(o.get("args")),
                env: kv_map(o.get("env")),
            };
        }
    }
    if let Some(url) = o.get("url").and_then(Value::as_str) {
        return RawSpec::Http {
            url: url.to_string(),
            headers: kv_map(o.get("headers")),
            sse: ty == "sse",
            headers_helper: o
                .get("headersHelper")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
    }
    RawSpec::Unsupported("entry has neither `command` nor `url`".into())
}

/// OpenCode entry; the bool is `disabled`.
fn opencode_spec(v: &Value) -> (RawSpec, bool) {
    let Some(o) = v.as_object() else {
        return (RawSpec::Unsupported("entry is not an object".into()), false);
    };
    let disabled = o.get("enabled").and_then(Value::as_bool) == Some(false);
    let ty = o.get("type").and_then(Value::as_str).unwrap_or("");
    let spec = match ty {
        "local" => {
            let mut cmd = str_list(o.get("command")).into_iter();
            match cmd.next() {
                Some(command) => RawSpec::Stdio {
                    command,
                    args: cmd.collect(),
                    env: kv_map(o.get("environment")),
                },
                None => RawSpec::Unsupported("`command` array is empty".into()),
            }
        }
        "remote" => match o.get("url").and_then(Value::as_str) {
            Some(url) => RawSpec::Http {
                url: url.to_string(),
                headers: kv_map(o.get("headers")),
                sse: false,
                headers_helper: None,
            },
            None => RawSpec::Unsupported("remote entry has no `url`".into()),
        },
        _ => RawSpec::Unsupported("unknown `type`".into()),
    };
    (spec, disabled)
}

fn parse_codex(file: &AgentFile, text: &str) -> Result<ParsedFile, ImportError> {
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| ImportError::Parse {
            path: file.path.clone(),
            message: e.to_string(),
        })?;
    let mut out = ParsedFile::default();
    let Some(servers) = table.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Ok(out);
    };
    let strs = |v: Option<&toml::Value>| -> Vec<String> {
        v.and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|x| match x {
                        toml::Value::String(s) => s.clone(),
                        o => o.to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let map = |v: Option<&toml::Value>, as_ref: bool| -> Vec<(String, RawValue)> {
        v.and_then(toml::Value::as_table)
            .map(|t| {
                t.iter()
                    .map(|(k, v)| {
                        let s = match v {
                            toml::Value::String(s) => s.clone(),
                            o => o.to_string(),
                        };
                        let val = if as_ref {
                            RawValue::EnvRef(s)
                        } else {
                            classify(&s)
                        };
                        (k.clone(), val)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    for (name, entry) in servers {
        let Some(t) = entry.as_table() else {
            continue;
        };
        let disabled = t.get("enabled").and_then(toml::Value::as_bool) == Some(false);
        let spec = if let Some(cmd) = t.get("command").and_then(toml::Value::as_str) {
            RawSpec::Stdio {
                command: cmd.to_string(),
                args: strs(t.get("args")),
                env: map(t.get("env"), false),
            }
        } else if let Some(url) = t.get("url").and_then(toml::Value::as_str) {
            let mut headers = map(t.get("http_headers"), false);
            headers.extend(map(t.get("env_http_headers"), true));
            if let Some(var) = t.get("bearer_token_env_var").and_then(toml::Value::as_str) {
                headers.push((
                    "Authorization".into(),
                    RawValue::PrefixedRef {
                        prefix: "Bearer ".into(),
                        var: var.to_string(),
                    },
                ));
            }
            RawSpec::Http {
                url: url.to_string(),
                headers,
                sse: false,
                headers_helper: None,
            }
        } else {
            RawSpec::Unsupported("entry has neither `command` nor `url`".into())
        };
        out.servers.push(RawServer {
            name: name.clone(),
            spec,
            disabled,
            uses_inputs: false,
            source: Source {
                file: file.path.clone(),
                container: vec!["mcp_servers".into()],
                key: name.clone(),
                project_scoped: file.role == FileRole::Project,
            },
        });
    }
    Ok(out)
}

fn valid_var(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
        && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Recognise `${NAME}`, `${env:NAME}` and `{env:NAME}` at the end of a value.
pub(crate) fn classify(s: &str) -> RawValue {
    for marker in ["${env:", "${", "{env:"] {
        if let Some(i) = s.find(marker) {
            let rest = &s[i + marker.len()..];
            if let Some(name) = rest.strip_suffix('}') {
                if valid_var(name) {
                    let prefix = &s[..i];
                    return if prefix.is_empty() {
                        RawValue::EnvRef(name.to_string())
                    } else {
                        RawValue::PrefixedRef {
                            prefix: prefix.to_string(),
                            var: name.to_string(),
                        }
                    };
                }
            }
        }
    }
    RawValue::Literal(s.to_string())
}

/// Remove `//` and `/* */` comments and trailing commas (outside strings).
pub(crate) fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some('}') | Some(']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// Existing-path helper for callers.
pub(crate) fn exists(p: &Path) -> bool {
    p.is_file()
}
