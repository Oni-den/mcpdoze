// SPDX-License-Identifier: GPL-3.0-or-later
//! MCP servers provided by Claude Code plugins. Read-only: plugin files are
//! never rewritten, backed up or restored.
//!
//! * Enabled plugins: `enabledPlugins` (`"<plugin>@<marketplace>": bool`) in
//!   `~/.claude/settings.json`, then `./.claude/settings.json` and
//!   `./.claude/settings.local.json` (later files win).
//! * Install locations: `~/.claude/plugins/installed_plugins.json`, version 1
//!   (`plugins.<id> = {installPath}`) or 2 (`plugins.<id> = [{installPath,
//!   scope, projectPath}]`). Any other layout is skipped with a warning.
//! * Servers of a plugin root: `.mcp.json` (`mcpServers` or a bare map),
//!   `.claude-plugin/plugin.json` `mcpServers` (a map, a file path or a list
//!   of paths) and the plugin's entry in `.claude-plugin/marketplace.json`.
//!   The first definition of a server name wins.
//! * `${CLAUDE_PLUGIN_ROOT}` becomes the install path, `${CLAUDE_PLUGIN_DATA}`
//!   `~/.claude/plugins/data/<plugin>-<marketplace>`, and
//!   `${CLAUDE_PROJECT_DIR}` the broker's `${cwd}` (the server becomes
//!   `per-cwd`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::parse::{json_spec, RawSpec};
use super::util::{read_capped, to_text};
use super::ImportEnv;

/// Stands in for `${cwd}` until the server config is built, so the agent
/// value classifier does not mistake it for an environment reference.
pub(crate) const CWD_MARKER: &str = "\u{1}mcpdoze-cwd\u{1}";

/// One server a plugin provides.
#[derive(Debug, Clone)]
pub(crate) struct PluginServer {
    /// `<plugin>@<marketplace>`.
    pub id: String,
    /// `<plugin>`.
    pub plugin: String,
    pub server: String,
    pub root: PathBuf,
    pub enabled: bool,
    pub spec: RawSpec,
    /// Uses `${CLAUDE_PROJECT_DIR}`.
    pub per_cwd: bool,
    /// Cannot be migrated, and why.
    pub skip: Option<String>,
}

/// Result of a plugin scan.
#[derive(Debug, Clone, Default)]
pub(crate) struct PluginScan {
    pub servers: Vec<PluginServer>,
    pub warnings: Vec<String>,
}

fn read_json(path: &Path) -> Option<Result<Value, String>> {
    if !path.is_file() {
        return None;
    }
    Some(
        read_capped(path)
            .and_then(|b| to_text(path, b))
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str(&t).map_err(|e| format!("{}: {e}", path.display()))),
    )
}

/// `enabledPlugins` merged over the settings files.
fn enabled_plugins(env: &ImportEnv, warnings: &mut Vec<String>) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    for f in [
        env.home.join(".claude").join("settings.json"),
        env.cwd.join(".claude").join("settings.json"),
        env.cwd.join(".claude").join("settings.local.json"),
    ] {
        match read_json(&f) {
            None => {}
            Some(Err(e)) => warnings.push(format!("plugin settings not read: {e}")),
            Some(Ok(v)) => {
                if let Some(m) = v.get("enabledPlugins").and_then(Value::as_object) {
                    for (k, on) in m {
                        out.insert(k.clone(), on.as_bool() == Some(true));
                    }
                }
            }
        }
    }
    out
}

/// Install path of each plugin id, or `Err` for an unrecognised layout.
fn installs(env: &ImportEnv, registry: &Value) -> Result<Vec<(String, PathBuf)>, String> {
    let version = match registry.get("version") {
        None => None,
        Some(v) => match v.as_u64() {
            Some(n @ 1..=2) => Some(n),
            _ => return Err(format!("registry version {v}")),
        },
    };
    let plugins = registry
        .get("plugins")
        .and_then(Value::as_object)
        .ok_or("no `plugins` object")?;
    let here = crate::router::cwd_key(&env.cwd);
    let mut out = Vec::new();
    for (id, v) in plugins {
        let pick = match (version, v) {
            (Some(1) | None, Value::Object(_)) => Some(v),
            (Some(2) | None, Value::Array(list)) => list
                .iter()
                .find(|e| {
                    e["scope"] == "user"
                        || e["projectPath"]
                            .as_str()
                            .is_some_and(|p| crate::router::cwd_key(Path::new(p)) == here)
                })
                .or_else(|| list.first()),
            _ => return Err(format!("unexpected entry shape for `{id}`")),
        };
        if let Some(p) = pick
            .and_then(|e| e.get("installPath"))
            .and_then(Value::as_str)
        {
            out.push((id.clone(), PathBuf::from(p)));
        }
    }
    Ok(out)
}

/// The server maps a value names: a map, a file path or a list of paths.
fn maps_from(
    root: &Path,
    v: &Value,
    out: &mut Vec<Map<String, Value>>,
    warnings: &mut Vec<String>,
) {
    match v {
        Value::Object(m) => out.push(m.clone()),
        Value::String(p) => {
            let p = p.replace("${CLAUDE_PLUGIN_ROOT}", &root.to_string_lossy());
            let path = root.join(p);
            match read_json(&path) {
                Some(Ok(v)) => out.push(server_map(v)),
                Some(Err(e)) => warnings.push(format!("plugin server file not read: {e}")),
                None => {}
            }
        }
        Value::Array(list) => {
            for x in list {
                maps_from(root, x, out, warnings);
            }
        }
        _ => {}
    }
}

/// `{"mcpServers": {...}}` or a bare map of servers.
fn server_map(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(mut o) => match o.remove("mcpServers") {
            Some(Value::Object(m)) => m,
            Some(_) => Map::new(),
            None => o.into_iter().filter(|(_, v)| v.is_object()).collect(),
        },
        _ => Map::new(),
    }
}

fn plugin_servers(root: &Path, plugin: &str, warnings: &mut Vec<String>) -> Vec<(String, Value)> {
    let mut maps = Vec::new();
    match read_json(&root.join(".mcp.json")) {
        Some(Ok(v)) => maps.push(server_map(v)),
        Some(Err(e)) => warnings.push(format!("plugin server file not read: {e}")),
        None => {}
    }
    let meta = root.join(".claude-plugin");
    if let Some(Ok(pj)) = read_json(&meta.join("plugin.json")) {
        if let Some(v) = pj.get("mcpServers") {
            maps_from(root, v, &mut maps, warnings);
        }
    }
    if let Some(Ok(mk)) = read_json(&meta.join("marketplace.json")) {
        let entry = mk["plugins"]
            .as_array()
            .and_then(|l| l.iter().find(|p| p["name"] == plugin));
        if let Some(v) = entry.and_then(|e| e.get("mcpServers")) {
            maps_from(root, v, &mut maps, warnings);
        }
    }
    let mut out: Vec<(String, Value)> = Vec::new();
    for m in maps {
        for (k, v) in m {
            if !out.iter().any(|(n, _)| n == &k) {
                out.push((k, v));
            }
        }
    }
    out
}

/// Replaces the plugin variables in every string of `v`.
fn expand(v: &mut Value, root: &str, data: &str, per_cwd: &mut bool) {
    match v {
        Value::String(s) => {
            if s.contains("${CLAUDE_PROJECT_DIR}") {
                *per_cwd = true;
            }
            *s = s
                .replace("${CLAUDE_PLUGIN_ROOT}", root)
                .replace("${CLAUDE_PLUGIN_DATA}", data)
                .replace("${CLAUDE_PROJECT_DIR}", CWD_MARKER);
        }
        Value::Array(a) => a.iter_mut().for_each(|x| expand(x, root, data, per_cwd)),
        Value::Object(o) => o.values_mut().for_each(|x| expand(x, root, data, per_cwd)),
        _ => {}
    }
}

fn data_dir_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Every server of every installed plugin (enabled or not).
pub(crate) fn scan(env: &ImportEnv) -> PluginScan {
    let mut scan = PluginScan::default();
    let plugins_dir = env.home.join(".claude").join("plugins");
    let registry = match read_json(&plugins_dir.join("installed_plugins.json")) {
        None => return scan,
        Some(Err(e)) => {
            scan.warnings.push(format!(
                "plugin registry not read ({e}); plugin servers not scanned"
            ));
            return scan;
        }
        Some(Ok(v)) => v,
    };
    let installed = match installs(env, &registry) {
        Ok(i) => i,
        Err(why) => {
            scan.warnings.push(format!(
                "unrecognised plugin registry layout ({why}); plugin servers not scanned"
            ));
            return scan;
        }
    };
    let enabled = enabled_plugins(env, &mut scan.warnings);
    for (id, root) in installed {
        let plugin = id.split('@').next().unwrap_or(&id).to_owned();
        let on = enabled.get(&id).copied().unwrap_or(false);
        let data = plugins_dir.join("data").join(data_dir_name(&id));
        let (root_s, data_s) = (
            root.to_string_lossy().into_owned(),
            data.to_string_lossy().into_owned(),
        );
        for (server, mut v) in plugin_servers(&root, &plugin, &mut scan.warnings) {
            let mut per_cwd = false;
            expand(&mut v, &root_s, &data_s, &mut per_cwd);
            let skip = (v.to_string().contains("${user_config."))
                .then(|| "uses plugin user_config values the broker cannot read".to_owned());
            scan.servers.push(PluginServer {
                id: id.clone(),
                plugin: plugin.clone(),
                server,
                root: root.clone(),
                enabled: on,
                spec: json_spec(&v),
                per_cwd,
                skip,
            });
        }
    }
    scan
}
