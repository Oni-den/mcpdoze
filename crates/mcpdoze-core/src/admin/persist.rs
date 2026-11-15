// SPDX-License-Identifier: GPL-3.0-or-later

//! Format-preserving config edits (`toml_edit`) and atomic file replacement.
//!
//! Only the `[servers.<name>]` table being added or removed changes;
//! comments, ordering and formatting elsewhere are kept.

use std::io::{self, Write};
use std::path::Path;

use toml_edit::{DocumentMut, Item, Table};

use crate::config::ServerConfig;

/// Why an edit could not be made.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    /// The current file is not valid TOML.
    #[error("config file is not valid TOML: {0}")]
    Parse(String),
    /// `servers` exists but is not a table.
    #[error("`servers` in the config file is not a table")]
    NotATable,
    /// The server could not be expressed as TOML.
    #[error("cannot serialize server: {0}")]
    Serialize(String),
}

/// Key order of a written `[servers.<name>]` table.
const FIELD_ORDER: &[&str] = &[
    "transport",
    "command",
    "args",
    "env",
    "cwd",
    "url",
    "headers",
    "scope",
    "max_instances",
    "idle_ttl",
    "prefix",
    "enabled",
    "allow_literal_env",
    "probe",
    "catalog_max_age",
];

fn parse(text: &str) -> Result<DocumentMut, PersistError> {
    text.parse::<DocumentMut>()
        .map_err(|e| PersistError::Parse(e.message().to_owned()))
}

/// Returns `text` with `[servers.<name>]` set to `server` (appended when
/// new, replaced in place otherwise). `env` and `headers` are written as
/// inline tables, as in the documented config examples.
pub fn upsert_server(
    text: &str,
    name: &str,
    server: &ServerConfig,
) -> Result<String, PersistError> {
    let mut doc = parse(text)?;
    let rendered = toml::to_string(server).map_err(|e| PersistError::Serialize(e.to_string()))?;
    let parsed = parse(&rendered)?;
    let src = parsed.as_table();
    // Rebuild in the documented field order; maps become inline tables.
    let mut keys: Vec<&str> = FIELD_ORDER
        .iter()
        .copied()
        .filter(|k| src.contains_key(k))
        .collect();
    keys.extend(
        src.iter()
            .map(|(k, _)| k)
            .filter(|k| !FIELD_ORDER.contains(k)),
    );
    let mut table = Table::new();
    for key in keys {
        let Some(item) = src.get(key) else { continue };
        let item = match item.as_table() {
            Some(t) => {
                let mut inline = t.clone().into_inline_table();
                inline.sort_values();
                inline.fmt();
                Item::Value(toml_edit::Value::InlineTable(inline))
            }
            None => {
                let mut v = item.clone();
                if let Some(v) = v.as_value_mut() {
                    v.decor_mut().clear();
                }
                v
            }
        };
        table.insert(key, item);
    }
    table.set_implicit(false);
    table.decor_mut().clear();
    let servers = doc
        .entry("servers")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_mut()
        .ok_or(PersistError::NotATable)?;
    if servers.is_empty() && servers.is_implicit() && !text.trim().is_empty() {
        // Separate the first server table from what precedes it.
        table.decor_mut().set_prefix("\n");
    } else if !servers.contains_key(name) && !servers.is_empty() {
        table.decor_mut().set_prefix("\n");
    }
    servers.insert(name, Item::Table(table));
    Ok(doc.to_string())
}

/// Returns `text` without `[servers.<name>]`, or `None` if it is absent.
pub fn remove_server(text: &str, name: &str) -> Result<Option<String>, PersistError> {
    let mut doc = parse(text)?;
    let Some(servers) = doc.get_mut("servers") else {
        return Ok(None);
    };
    let servers = servers.as_table_like_mut().ok_or(PersistError::NotATable)?;
    if servers.remove(name).is_none() {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

/// Replaces `path` with `contents` atomically: write a temporary file in the
/// same directory, flush it to disk, then rename it over the target. Keeps
/// the existing file's permissions (owner-only for a new file on Unix).
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "config path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let mut rnd = [0u8; 8];
    getrandom::fill(&mut rnd).map_err(|e| io::Error::other(e.to_string()))?;
    let suffix: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = dir.join(format!(".{file_name}.{suffix}.tmp"));
    let result = (|| {
        let mut f = create_tmp(&tmp)?;
        if let Ok(meta) = std::fs::metadata(path) {
            f.set_permissions(meta.permissions())?;
        }
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        drop(f);
        rename_retry(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn create_tmp(p: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(p)
}

#[cfg(not(unix))]
fn create_tmp(p: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(p)
}

/// `rename` replaces the target on every platform; on Windows it can fail
/// briefly while another process (an editor, a scanner) has the file open.
fn rename_retry(from: &Path, to: &Path) -> io::Result<()> {
    let mut last = None;
    for _ in 0..5 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const ORIGINAL: &str = r#"# MCPDoze config (example)
[broker]
bind = "127.0.0.1:7878" # loopback only

# The first server.
[servers.alpha]
transport = "stdio"
command = "alpha-mcp" # keep this comment
"#;

    fn server(json: &str) -> ServerConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn add_and_remove_preserve_comments() {
        let s = server(
            r#"{"transport":"stdio","command":"beta-mcp","args":["--flag"],
                "env":{"API_KEY":{"from_env":"EXAMPLE_KEY"},"MODE":"fast"},"idle_ttl":"5m"}"#,
        );
        let added = upsert_server(ORIGINAL, "beta", &s).unwrap();
        for keep in [
            "# MCPDoze config (example)",
            "# loopback only",
            "# The first server.",
            "# keep this comment",
        ] {
            assert!(added.contains(keep), "{keep} lost:\n{added}");
        }
        assert!(
            added.starts_with(ORIGINAL),
            "existing text changed:\n{added}"
        );
        assert!(added.contains("[servers.beta]"), "{added}");
        assert!(
            added.contains(r#"env = { API_KEY = { from_env = "EXAMPLE_KEY" }, MODE = "fast" }"#),
            "{added}"
        );
        let cfg = Config::parse(&added).unwrap();
        assert_eq!(cfg.servers["beta"], s);
        assert_eq!(cfg.servers.len(), 2);

        let removed = remove_server(&added, "beta").unwrap().unwrap();
        assert_eq!(removed.trim_end(), ORIGINAL.trim_end());
        assert!(remove_server(&removed, "beta").unwrap().is_none());
    }

    #[test]
    fn add_to_empty_and_serverless_files() {
        let s = server(r#"{"transport":"http","url":"https://mcp.example.com/mcp"}"#);
        let out = upsert_server("", "remote", &s).unwrap();
        assert_eq!(
            Config::parse(&out).unwrap().servers["remote"]
                .url
                .as_deref(),
            Some("https://mcp.example.com/mcp")
        );
        assert!(!out.contains("[servers]\n"), "{out}");
        let out = upsert_server("[broker]\nbind = \"127.0.0.1:1\"\n", "remote", &s).unwrap();
        assert!(
            out.starts_with("[broker]\nbind = \"127.0.0.1:1\"\n"),
            "{out}"
        );
        assert!(Config::parse(&out).unwrap().servers.contains_key("remote"));
    }

    #[test]
    fn rejects_broken_files() {
        let s = server(r#"{"transport":"stdio","command":"x"}"#);
        assert!(matches!(
            upsert_server("[broken", "x", &s),
            Err(PersistError::Parse(_))
        ));
        assert!(matches!(
            upsert_server("servers = 3\n", "x", &s),
            Err(PersistError::NotATable)
        ));
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_atomic(&path, "a = 1\n").unwrap();
        write_atomic(&path, "a = 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 2\n");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }
}
