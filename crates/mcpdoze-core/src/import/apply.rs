// SPDX-License-Identifier: GPL-3.0-or-later
//! Applying a plan: backups, manifest, broker registration, agent rewrite;
//! and rolling it back.
//!
//! Agent entry written per agent (the token itself is never written; each
//! agent expands an environment variable in its own syntax):
//!
//! * Claude Code: `{"type":"http","url":..,"headers":{"Authorization":"Bearer ${VAR}"}}`
//! * Cursor: `{"url":..,"headers":{"Authorization":"Bearer ${env:VAR}"}}`
//! * VS Code: `{"type":"http","url":..,"headers":{"Authorization":"Bearer ${env:VAR}"}}`
//! * OpenCode: `{"type":"remote","url":..,"headers":{"Authorization":"Bearer {env:VAR}"}}`
//! * Codex: `[mcp_servers.mcpdoze] url = ..; bearer_token_env_var = "VAR"`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;

use super::parse::json_container_key;
use super::plan::{Plan, PlanItem, Source};
use super::util::{
    atomic_write, now_secs, read_capped, sha256_hex, to_text, utc_iso, utc_timestamp,
};
use super::{Agent, ImportError, BACKUP_INFIX, BROKER_ENTRY_NAME};

/// Options for [`apply`].
#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Fixed clock for tests.
    pub now_secs: Option<u64>,
}

/// One file recorded in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: PathBuf,
    /// `agent` (restored on rollback) or `broker_config` (servers removed).
    pub role: String,
    pub existed: bool,
    pub backup: Option<PathBuf>,
    pub original_sha256: Option<String>,
    pub new_sha256: String,
}

/// The record written to `<data_dir>/import/<timestamp>.json`. Holds names,
/// paths and hashes only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub created_utc: String,
    pub agent: Agent,
    pub broker_url: String,
    pub broker_config: PathBuf,
    pub token_env_var: String,
    pub files: Vec<ManifestFile>,
    /// Servers this import added to the broker config.
    pub registered_servers: Vec<String>,
    /// Names migrated out of the agent config.
    pub migrated: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back_utc: Option<String>,
}

/// Outcome of [`apply`].
#[derive(Debug, Clone)]
pub struct ApplyResult {
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
}

/// Outcome of [`rollback`].
#[derive(Debug, Clone)]
pub struct RollbackResult {
    pub restored: Vec<PathBuf>,
    pub removed_servers: Vec<String>,
}

// --------------------------------------------------------------- JSON edit

/// The single broker entry for a JSON-based agent.
pub(crate) fn broker_entry_json(agent: Agent, url: &str, var: &str) -> Value {
    match agent {
        Agent::ClaudeCode => json!({
            "type": "http", "url": url,
            "headers": { "Authorization": format!("Bearer ${{{var}}}") }
        }),
        Agent::Cursor => json!({
            "url": url,
            "headers": { "Authorization": format!("Bearer ${{env:{var}}}") }
        }),
        Agent::VsCode => json!({
            "type": "http", "url": url,
            "headers": { "Authorization": format!("Bearer ${{env:{var}}}") }
        }),
        Agent::OpenCode => json!({
            "type": "remote", "url": url, "enabled": true,
            "headers": { "Authorization": format!("Bearer {{env:{var}}}") }
        }),
        Agent::Codex => Value::Null,
    }
}

fn rewrite_json(
    agent: Agent,
    path: &Path,
    text: &str,
    sources: &[&Source],
    url: &str,
    var: &str,
) -> Result<String, ImportError> {
    let mut root: Value = serde_json::from_str(text).map_err(|e| ImportError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    for s in sources {
        let mut cur = Some(&mut root);
        for k in &s.container {
            cur = cur.and_then(|v| v.get_mut(k));
        }
        if let Some(map) = cur.and_then(Value::as_object_mut) {
            map.shift_remove(&s.key);
        }
    }
    let key = sources
        .iter()
        .find(|s| s.container.len() == 1)
        .map(|s| s.container[0].clone())
        .unwrap_or_else(|| json_container_key(agent).to_string());
    let obj = root.as_object_mut().ok_or_else(|| ImportError::Parse {
        path: path.to_path_buf(),
        message: "top level is not an object".into(),
    })?;
    let container = obj
        .entry(key.clone())
        .or_insert_with(|| Value::Object(Default::default()));
    let map = container
        .as_object_mut()
        .ok_or_else(|| ImportError::Parse {
            path: path.to_path_buf(),
            message: format!("`{key}` is not an object"),
        })?;
    map.insert(BROKER_ENTRY_NAME.into(), broker_entry_json(agent, url, var));
    let mut out = serde_json::to_string_pretty(&root).map_err(|e| ImportError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    if text.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

fn rewrite_codex(
    path: &Path,
    text: &str,
    sources: &[&Source],
    url: &str,
    var: &str,
) -> Result<String, ImportError> {
    let mut doc: toml_edit::DocumentMut =
        text.parse()
            .map_err(|e: toml_edit::TomlError| ImportError::Parse {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
    for s in sources {
        if let Some(t) = doc
            .get_mut("mcp_servers")
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            t.remove(&s.key);
        }
    }
    let mut entry = toml_edit::Table::new();
    entry["url"] = toml_edit::value(url);
    entry["bearer_token_env_var"] = toml_edit::value(var);
    if doc.get("mcp_servers").is_none() {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        doc["mcp_servers"] = toml_edit::Item::Table(t);
    }
    doc["mcp_servers"][BROKER_ENTRY_NAME] = toml_edit::Item::Table(entry);
    Ok(doc.to_string())
}

// ------------------------------------------------------- broker config edit

/// Add `items` to the broker config text. Returns the new text and the names
/// added, or `None` when nothing needs adding.
fn register_servers(
    path: &Path,
    items: &[&PlanItem],
) -> Result<Option<(String, Vec<String>)>, ImportError> {
    let text = if path.is_file() {
        to_text(path, read_capped(path)?)?
    } else {
        String::new()
    };
    let mut doc: toml_edit::DocumentMut =
        text.parse()
            .map_err(|e: toml_edit::TomlError| ImportError::Parse {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
    let mut added = Vec::new();
    for it in items.iter().filter(|i| !i.already_registered) {
        let ser = toml::to_string(&it.config)
            .map_err(|e| ImportError::Invalid(format!("cannot serialise `{}`: {e}", it.name)))?;
        let parsed: toml_edit::DocumentMut = ser
            .parse()
            .map_err(|e: toml_edit::TomlError| ImportError::Invalid(e.to_string()))?;
        let mut table = toml_edit::Table::new();
        for (k, v) in parsed.as_table().iter() {
            let mut v = v.clone();
            if v.is_table() {
                v =
                    toml_edit::Item::Value(v.into_value().map_err(|_| {
                        ImportError::Invalid("unexpected server table shape".into())
                    })?);
            }
            table.insert(k, v);
        }
        if doc.get("servers").is_none() {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            doc["servers"] = toml_edit::Item::Table(t);
        }
        doc["servers"][it.name.as_str()] = toml_edit::Item::Table(table);
        added.push(it.name.clone());
    }
    if added.is_empty() {
        return Ok(None);
    }
    let new = doc.to_string();
    let cfg = Config::parse(&new)
        .map_err(|e| ImportError::Invalid(format!("resulting broker config is invalid: {e}")))?;
    if let Err(errs) = cfg.validate() {
        let msgs: Vec<String> = errs.iter().map(|e| e.to_string()).collect();
        return Err(ImportError::Invalid(format!(
            "resulting broker config is invalid: {}",
            msgs.join("; ")
        )));
    }
    Ok(Some((new, added)))
}

fn remove_servers(text: &str, names: &[String]) -> Result<Option<String>, ImportError> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| ImportError::Invalid(format!("broker config: {e}")))?;
    let mut changed = false;
    if let Some(t) = doc
        .get_mut("servers")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        for n in names {
            changed |= t.remove(n).is_some();
        }
    }
    Ok(changed.then(|| doc.to_string()))
}

// ------------------------------------------------------------------ apply

fn backup_path(file: &Path, ts: &str) -> PathBuf {
    let mut name = file
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!("{BACKUP_INFIX}{ts}"));
    let base = file.with_file_name(name);
    if !base.exists() {
        return base;
    }
    for i in 2.. {
        let mut n = base
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        n.push(format!("-{i}"));
        let p = base.with_file_name(n);
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

struct Pending {
    path: PathBuf,
    role: &'static str,
    original: Option<Vec<u8>>,
    new: String,
}

/// Apply `plan`. Order: compute everything in memory, back up, write the
/// manifest, write the broker config, then rewrite the agent files. If a
/// write fails the files already written are restored. Returns `None` when
/// the plan has nothing to do (e.g. a repeated import).
pub fn apply(
    plan: &Plan,
    data_dir: &Path,
    opts: &ApplyOptions,
) -> Result<Option<ApplyResult>, ImportError> {
    if plan.items.is_empty() {
        return Ok(None);
    }
    let secs = opts.now_secs.unwrap_or_else(now_secs);
    let ts = utc_timestamp(secs);

    let mut pending: Vec<Pending> = Vec::new();
    let item_refs: Vec<&PlanItem> = plan.items.iter().collect();
    let mut registered = Vec::new();
    if let Some((text, added)) = register_servers(&plan.broker_config, &item_refs)? {
        let original = plan
            .broker_config
            .is_file()
            .then(|| read_capped(&plan.broker_config))
            .transpose()?;
        registered = added;
        pending.push(Pending {
            path: plan.broker_config.clone(),
            role: "broker_config",
            original,
            new: text,
        });
    }
    for file in &plan.files {
        let bytes = read_capped(file)?;
        let text = to_text(file, bytes.clone())?;
        let sources: Vec<&Source> = plan
            .items
            .iter()
            .flat_map(|i| i.sources.iter())
            .filter(|s| &s.file == file)
            .collect();
        let new = if plan.agent == Agent::Codex {
            rewrite_codex(file, &text, &sources, &plan.broker_url, &plan.token_env_var)?
        } else {
            rewrite_json(
                plan.agent,
                file,
                &text,
                &sources,
                &plan.broker_url,
                &plan.token_env_var,
            )?
        };
        pending.push(Pending {
            path: file.clone(),
            role: "agent",
            original: Some(bytes),
            new,
        });
    }

    // Backups.
    let mut files = Vec::new();
    for p in &pending {
        let backup = match &p.original {
            Some(orig) => {
                let b = backup_path(&p.path, &ts);
                atomic_write(&b, orig)?;
                if sha256_hex(&read_capped(&b)?) != sha256_hex(orig) {
                    return Err(ImportError::Invalid(format!(
                        "{}: backup verification failed",
                        b.display()
                    )));
                }
                Some(b)
            }
            None => None,
        };
        files.push(ManifestFile {
            path: p.path.clone(),
            role: p.role.into(),
            existed: p.original.is_some(),
            backup,
            original_sha256: p.original.as_ref().map(|o| sha256_hex(o)),
            new_sha256: sha256_hex(p.new.as_bytes()),
        });
    }

    let manifest = Manifest {
        version: 1,
        created_utc: utc_iso(secs),
        agent: plan.agent,
        broker_url: plan.broker_url.clone(),
        broker_config: plan.broker_config.clone(),
        token_env_var: plan.token_env_var.clone(),
        files,
        registered_servers: registered,
        migrated: plan.items.iter().map(|i| i.name.clone()).collect(),
        rolled_back_utc: None,
    };
    let dir = data_dir.join("import");
    let mut manifest_path = dir.join(format!("{ts}.json"));
    let mut n = 2;
    while manifest_path.exists() {
        manifest_path = dir.join(format!("{ts}-{n}.json"));
        n += 1;
    }
    write_manifest(&manifest_path, &manifest)?;

    // Write: broker config first so the servers exist before agents lose them.
    let mut written: Vec<usize> = Vec::new();
    for (i, p) in pending.iter().enumerate() {
        if let Err(e) = atomic_write(&p.path, p.new.as_bytes()) {
            for &j in written.iter().rev() {
                match &pending[j].original {
                    Some(orig) => {
                        let _ = atomic_write(&pending[j].path, orig);
                    }
                    None => {
                        let _ = std::fs::remove_file(&pending[j].path);
                    }
                }
            }
            return Err(e);
        }
        written.push(i);
    }
    Ok(Some(ApplyResult {
        manifest_path,
        manifest,
    }))
}

fn write_manifest(path: &Path, m: &Manifest) -> Result<(), ImportError> {
    let text = serde_json::to_string_pretty(m)
        .map_err(|e| ImportError::Invalid(format!("manifest: {e}")))?;
    atomic_write(path, format!("{text}\n").as_bytes())
}

// --------------------------------------------------------------- rollback

/// Undo an import. Agent files are restored only if their current content
/// still hashes to the manifest's `new_sha256`; otherwise nothing is touched.
/// Servers the import registered are removed from the broker config.
pub fn rollback(manifest_path: &Path) -> Result<RollbackResult, ImportError> {
    let text = to_text(manifest_path, read_capped(manifest_path)?)?;
    let mut m: Manifest = serde_json::from_str(&text).map_err(|e| ImportError::Parse {
        path: manifest_path.to_path_buf(),
        message: e.to_string(),
    })?;
    if m.rolled_back_utc.is_some() {
        return Err(ImportError::AlreadyRolledBack);
    }
    let mut changed = Vec::new();
    let mut backups: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    for f in m.files.iter().filter(|f| f.role == "agent") {
        match read_capped(&f.path) {
            Ok(cur) if sha256_hex(&cur) == f.new_sha256 => {}
            Ok(_) => changed.push(format!("{} (modified)", f.path.display())),
            Err(_) => changed.push(format!("{} (missing)", f.path.display())),
        }
        if let Some(b) = &f.backup {
            let data = read_capped(b)?;
            if Some(sha256_hex(&data)) != f.original_sha256 {
                return Err(ImportError::Invalid(format!(
                    "{}: backup does not match the manifest; refusing to restore",
                    b.display()
                )));
            }
            backups.insert(f.path.clone(), data);
        }
    }
    if !changed.is_empty() {
        return Err(ImportError::Changed(changed));
    }
    let mut restored = Vec::new();
    for f in m.files.iter().filter(|f| f.role == "agent") {
        match backups.get(&f.path) {
            Some(data) => atomic_write(&f.path, data)?,
            None => {
                let _ = std::fs::remove_file(&f.path);
            }
        }
        restored.push(f.path.clone());
    }
    let mut removed = Vec::new();
    if !m.registered_servers.is_empty() && m.broker_config.is_file() {
        let cur = to_text(&m.broker_config, read_capped(&m.broker_config)?)?;
        if let Some(new) = remove_servers(&cur, &m.registered_servers)? {
            atomic_write(&m.broker_config, new.as_bytes())?;
            removed = m.registered_servers.clone();
        }
    }
    m.rolled_back_utc = Some(utc_iso(now_secs()));
    write_manifest(manifest_path, &m)?;
    Ok(RollbackResult {
        restored,
        removed_servers: removed,
    })
}
