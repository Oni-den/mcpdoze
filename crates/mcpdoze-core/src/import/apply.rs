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

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;

use super::parse::{classify, json_container_key};
use super::plan::{Plan, PlanItem, Source};
use super::util::{
    atomic_write, now_secs, read_capped, sha256_hex, to_text, utc_iso, utc_timestamp,
};
use super::{Agent, ImportError, RawValue, BACKUP_INFIX, BROKER_ENTRY_NAME};

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
    /// What the import changed inside a JSON agent config, so a rollback can
    /// undo exactly that even after the agent rewrote the file. Absent for
    /// TOML agents and in manifests written before version 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_edit: Option<JsonEdit>,
}

/// Marker stored in the manifest in place of a literal value that may be a
/// secret. Rollback reads the real value from the verified backup.
pub const REDACTED: &str = "<redacted: restored from backup>";

/// The edits an import made to one JSON agent config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JsonEdit {
    /// Object keys leading to the map that received the broker entry.
    pub broker_container: Vec<String>,
    /// The import created `broker_container` (it did not exist before).
    pub created_container: bool,
    /// The `mcpdoze` entry that existed before the import, if any. It is
    /// MCPDoze's own entry and holds only a URL and an env reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_broker_entry: Option<Value>,
    /// Entries removed by the import, in their original order.
    pub entries: Vec<SavedEntry>,
}

/// One migrated entry as it was in the agent config. Literal values under
/// `env` / `environment` / `headers` (and URLs with credentials or a query)
/// are replaced by [`REDACTED`]; `redacted` lists their paths inside `value`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedEntry {
    pub container: Vec<String>,
    pub key: String,
    /// Position of the entry in its container before the import.
    pub index: usize,
    pub value: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redacted: Vec<Vec<String>>,
}

/// The record written to `<data_dir>/import/<timestamp>.json`. Holds names,
/// paths, hashes and the migrated JSON entries with literal values redacted;
/// never a secret value.
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
) -> Result<(String, JsonEdit), ImportError> {
    let mut root: Value = serde_json::from_str(text).map_err(|e| ImportError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let mut entries = Vec::new();
    for s in sources {
        let Some(map) = get_path(&root, &s.container).and_then(Value::as_object) else {
            continue;
        };
        if let Some(index) = map.keys().position(|k| k == &s.key) {
            let (value, redacted) = redact_entry(&map[&s.key]);
            entries.push(SavedEntry {
                container: s.container.clone(),
                key: s.key.clone(),
                index,
                value,
                redacted,
            });
        }
    }
    entries.sort_by_key(|e| e.index);
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
    let created_container = !obj.contains_key(&key);
    let container = obj
        .entry(key.clone())
        .or_insert_with(|| Value::Object(Default::default()));
    let map = container
        .as_object_mut()
        .ok_or_else(|| ImportError::Parse {
            path: path.to_path_buf(),
            message: format!("`{key}` is not an object"),
        })?;
    let previous_broker_entry =
        map.insert(BROKER_ENTRY_NAME.into(), broker_entry_json(agent, url, var));
    let out = to_json_text(path, &root, text.ends_with('\n'))?;
    Ok((
        out,
        JsonEdit {
            broker_container: vec![key],
            created_container,
            previous_broker_entry,
            entries,
        },
    ))
}

fn to_json_text(path: &Path, root: &Value, newline: bool) -> Result<String, ImportError> {
    let mut out = serde_json::to_string_pretty(root).map_err(|e| ImportError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    if newline {
        out.push('\n');
    }
    Ok(out)
}

/// Replace literal values that may be secrets with [`REDACTED`]; returns the
/// redacted copy and the paths replaced.
fn redact_entry(v: &Value) -> (Value, Vec<Vec<String>>) {
    let mut out = v.clone();
    let mut paths = Vec::new();
    if let Some(o) = out.as_object_mut() {
        for (k, field) in o.iter_mut() {
            match (k.as_str(), field) {
                ("env" | "environment" | "headers", Value::Object(m)) => {
                    for (name, val) in m.iter_mut() {
                        let literal = match val {
                            Value::String(s) => matches!(classify(s), RawValue::Literal(_)),
                            Value::Null => false,
                            _ => true,
                        };
                        if literal {
                            *val = Value::String(REDACTED.into());
                            paths.push(vec![k.clone(), name.clone()]);
                        }
                    }
                }
                ("url", Value::String(u)) if u.contains('?') || u.contains('@') => {
                    *u = REDACTED.into();
                    paths.push(vec![k.clone()]);
                }
                _ => {}
            }
        }
    }
    (out, paths)
}

fn get_path<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(root, |v, k| v.get(k))
}

fn get_path_mut<'a>(root: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    path.iter().try_fold(root, |v, k| v.get_mut(k))
}

/// Undo `edit` on the current content of a JSON agent config. `backup` is
/// the verified original, needed only for redacted values. Returns the new
/// text, or `None` when nothing needs changing.
fn surgical_json_rollback(
    path: &Path,
    current: &str,
    edit: &JsonEdit,
    backup: Option<&[u8]>,
) -> Result<Option<String>, ImportError> {
    let refuse = |m: String| ImportError::Parse {
        path: path.to_path_buf(),
        message: format!("{m}; refusing to roll back"),
    };
    let mut root: Value =
        serde_json::from_str(current).map_err(|e| refuse(format!("not valid JSON ({e})")))?;
    if !root.is_object() {
        return Err(refuse("top level is not an object".into()));
    }
    let needs_backup = edit.entries.iter().any(|e| !e.redacted.is_empty());
    let original: Option<Value> = match (needs_backup, backup) {
        (false, _) => None,
        (true, Some(b)) => Some(
            serde_json::from_slice(b)
                .map_err(|e| refuse(format!("backup is not valid JSON ({e})")))?,
        ),
        (true, None) => {
            return Err(refuse(
                "the backup needed to restore redacted values is missing".into(),
            ))
        }
    };
    let before = root.clone();
    let not_object = |p: &[String]| refuse(format!("`{}` is not an object", p.join(".")));

    // 1. Our broker entry: put back what was there, or remove it.
    if let Some(c) = get_path_mut(&mut root, &edit.broker_container) {
        let map = c
            .as_object_mut()
            .ok_or_else(|| not_object(&edit.broker_container))?;
        match &edit.previous_broker_entry {
            Some(prev) => {
                if map.contains_key(BROKER_ENTRY_NAME) {
                    map.insert(BROKER_ENTRY_NAME.into(), prev.clone());
                }
            }
            None => {
                map.shift_remove(BROKER_ENTRY_NAME);
            }
        }
    }

    // 2. Re-insert the migrated entries (an entry the user re-added is kept).
    for e in &edit.entries {
        let mut value = e.value.clone();
        if let Some(orig) = &original {
            let src = get_path(orig, &e.container)
                .and_then(|c| c.get(&e.key))
                .ok_or_else(|| refuse(format!("backup lacks entry `{}`", e.key)))?;
            for rp in &e.redacted {
                let real = get_path(src, rp)
                    .cloned()
                    .ok_or_else(|| refuse(format!("backup lacks a value of `{}`", e.key)))?;
                if let Some(slot) = get_path_mut(&mut value, rp) {
                    *slot = real;
                }
            }
        }
        let mut cur = &mut root;
        for (i, k) in e.container.iter().enumerate() {
            let map = cur
                .as_object_mut()
                .ok_or_else(|| not_object(&e.container[..i]))?;
            cur = map
                .entry(k.clone())
                .or_insert_with(|| Value::Object(Default::default()));
        }
        let map = cur
            .as_object_mut()
            .ok_or_else(|| not_object(&e.container))?;
        if !map.contains_key(&e.key) {
            let at = e.index.min(map.len());
            map.shift_insert(at, e.key.clone(), value);
        }
    }

    // 3. Drop a container the import created if it is now empty.
    if edit.created_container {
        if let ([k], Some(obj)) = (edit.broker_container.as_slice(), root.as_object_mut()) {
            if obj
                .get(k)
                .and_then(Value::as_object)
                .is_some_and(|m| m.is_empty())
            {
                obj.shift_remove(k);
            }
        }
    }
    if root == before {
        return Ok(None);
    }
    to_json_text(path, &root, current.ends_with('\n')).map(Some)
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
    json_edit: Option<JsonEdit>,
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
            json_edit: None,
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
        let (new, json_edit) = if plan.agent == Agent::Codex {
            (
                rewrite_codex(file, &text, &sources, &plan.broker_url, &plan.token_env_var)?,
                None,
            )
        } else {
            let (t, e) = rewrite_json(
                plan.agent,
                file,
                &text,
                &sources,
                &plan.broker_url,
                &plan.token_env_var,
            )?;
            (t, Some(e))
        };
        pending.push(Pending {
            path: file.clone(),
            role: "agent",
            original: Some(bytes),
            new,
            json_edit,
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
            json_edit: p.json_edit.clone(),
        });
    }

    let manifest = Manifest {
        version: 2,
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

/// Undo an import.
///
/// For each agent file: if it is byte-identical to what the import wrote, the
/// backup is restored whole. Otherwise a JSON agent config recorded with a
/// [`JsonEdit`] is rolled back surgically: the current file is parsed, the
/// broker entry the import added is removed (or the previous one put back),
/// the migrated entries are re-inserted at their old positions, and every
/// other key is kept. A file that is not valid JSON, a changed TOML agent
/// file, or a changed file under an older manifest without saved entries is
/// refused, and then nothing is touched. Servers the import registered are
/// finally removed from the broker config.
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
    // Per agent file: `Some(bytes)` = write these, `None` = remove the file.
    let mut writes: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    for f in m.files.iter().filter(|f| f.role == "agent") {
        let backup = match &f.backup {
            Some(b) => {
                let data = read_capped(b)?;
                if Some(sha256_hex(&data)) != f.original_sha256 {
                    return Err(ImportError::Invalid(format!(
                        "{}: backup does not match the manifest; refusing to restore",
                        b.display()
                    )));
                }
                Some(data)
            }
            None => None,
        };
        let Ok(cur) = read_capped(&f.path) else {
            changed.push(format!("{} (missing)", f.path.display()));
            continue;
        };
        if sha256_hex(&cur) == f.new_sha256 {
            writes.push((f.path.clone(), backup));
            continue;
        }
        let Some(edit) = &f.json_edit else {
            changed.push(format!("{} (modified)", f.path.display()));
            continue;
        };
        let cur_text = to_text(&f.path, cur)?;
        if let Some(new) = surgical_json_rollback(&f.path, &cur_text, edit, backup.as_deref())? {
            writes.push((f.path.clone(), Some(new.into_bytes())));
        }
    }
    if !changed.is_empty() {
        return Err(ImportError::Changed(changed));
    }
    let mut restored = Vec::new();
    for (path, data) in &writes {
        match data {
            Some(data) => atomic_write(path, data)?,
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
        restored.push(path.clone());
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
