// SPDX-License-Identifier: GPL-3.0-or-later

//! Reads of the failure log for the admin API: the `failures` query and the
//! newest failure of every instance (`last_failure` in `GET /status`).
//!
//! The log is written by [`crate::diag`] and is sanitised by construction;
//! records are passed on as parsed JSON without interpreting free text.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use super::api::LastFailure;
use crate::logfile::rotated_path;

/// How much of the newest file the `last_failure` scan reads.
const TAIL_BYTES: u64 = 512 * 1024;
/// Most records one query returns.
pub(super) const MAX_LIMIT: usize = 500;
/// Records returned when `limit` is not given.
pub(super) const DEFAULT_LIMIT: usize = 50;

/// Filters of a failures query.
#[derive(Debug, Default)]
pub(super) struct Filter {
    pub(super) server: Option<String>,
    pub(super) kind: Option<String>,
    pub(super) since_ms: u64,
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

/// The newest `limit` matching records of `file` and its `keep` rotated
/// files, oldest first. Blocking.
pub(super) fn read_failures(file: &Path, keep: usize, f: &Filter, limit: usize) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let paths =
        std::iter::once(file.to_path_buf()).chain((1..=keep).map(|n| rotated_path(file, n)));
    'files: for p in paths {
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines().rev() {
            if let Ok(rec) = serde_json::from_str::<Value>(line) {
                if f.matches(&rec) {
                    out.push(rec);
                    if out.len() >= limit {
                        break 'files;
                    }
                }
            }
        }
    }
    out.reverse();
    out
}

/// Newest failure per `(server, instance hash)`.
pub(super) type LastMap = HashMap<(String, Option<String>), LastFailure>;

/// Kinds that are not failures of a call or a backend.
fn is_noise(kind: &str) -> bool {
    matches!(kind, "suppressed" | "slow_call")
}

/// Scans the tail of `file` for the newest failure of each instance.
/// Blocking.
fn scan_last(file: &Path) -> LastMap {
    let mut map = LastMap::new();
    let Ok(mut fh) = std::fs::File::open(file) else {
        return map;
    };
    let len = fh.metadata().map_or(0, |m| m.len());
    let start = len.saturating_sub(TAIL_BYTES);
    if fh.seek(SeekFrom::Start(start)).is_err() {
        return map;
    }
    let mut buf = Vec::new();
    if fh.take(TAIL_BYTES).read_to_end(&mut buf).is_err() {
        return map;
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    if start > 0 {
        // The first line is probably cut.
        lines.next();
    }
    for line in lines {
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let (Some(server), Some(kind), Some(id), Some(ts)) = (
            rec["server"].as_str(),
            rec["kind"].as_str(),
            rec["id"].as_str(),
            rec["ts"].as_u64(),
        ) else {
            continue;
        };
        if is_noise(kind) {
            continue;
        }
        let instance = rec["instance"].as_str().map(str::to_owned);
        map.insert(
            (server.to_owned(), instance),
            LastFailure {
                id: id.to_owned(),
                kind: kind.to_owned(),
                ts,
            },
        );
    }
    map
}

/// [`scan_last`] cached on the file's length and modification time.
#[derive(Default)]
pub(super) struct LastCache {
    key: Option<(PathBuf, u64, Option<SystemTime>)>,
    map: LastMap,
}

impl LastCache {
    /// Blocking.
    pub(super) fn get(&mut self, file: &Path) -> &LastMap {
        let key = std::fs::metadata(file)
            .ok()
            .map(|m| (file.to_path_buf(), m.len(), m.modified().ok()));
        if key != self.key {
            self.map = if key.is_some() {
                scan_last(file)
            } else {
                LastMap::new()
            };
            self.key = key;
        }
        &self.map
    }
}

/// Parses the `since` of a query: a duration ago (`1h`) or unix ms.
pub(super) fn parse_since(s: &str, now_ms: u64) -> Result<u64, String> {
    let s = s.trim();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse::<u64>().map_err(|e| e.to_string());
    }
    let d = crate::config::parse_duration(s)?;
    Ok(now_ms.saturating_sub(d.as_millis() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, server: &str, kind: &str, ts: u64, inst: Option<&str>) -> String {
        let mut v = serde_json::json!({"v":1,"id":id,"server":server,"kind":kind,"ts":ts});
        if let Some(i) = inst {
            v["instance"] = i.into();
        }
        v.to_string()
    }

    #[test]
    fn query_filters_and_limits_newest() {
        let dir = std::env::temp_dir().join(format!("mcpdoze-admin-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("failures.jsonl");
        std::fs::write(
            rotated_path(&file, 1),
            format!("{}\n", line("f-000001", "a", "rpc_error", 10, None)),
        )
        .unwrap();
        std::fs::write(
            &file,
            [
                line("f-000002", "a", "stalled", 20, None),
                "not json".to_owned(),
                line("f-000003", "b", "rpc_error", 30, None),
                line("f-000004", "a", "rpc_error", 40, None),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        let all = read_failures(&file, 2, &Filter::default(), 10);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0]["id"], "f-000001");
        let f = Filter {
            server: Some("a".into()),
            kind: Some("rpc_error".into()),
            since_ms: 15,
        };
        let some = read_failures(&file, 2, &f, 10);
        assert_eq!(some.len(), 1);
        assert_eq!(some[0]["id"], "f-000004");
        let last2 = read_failures(&file, 2, &Filter::default(), 2);
        assert_eq!(last2[0]["id"], "f-000003");
        assert_eq!(last2[1]["id"], "f-000004");
        let mut cache = LastCache::default();
        let m = cache.get(&file);
        assert_eq!(m[&("a".to_owned(), None)].id, "f-000004");
        assert_eq!(m[&("b".to_owned(), None)].kind, "rpc_error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn since_accepts_duration_or_ms() {
        assert_eq!(parse_since("1500", 9999).unwrap(), 1500);
        assert_eq!(parse_since("1h", 4_000_000).unwrap(), 400_000);
        assert!(parse_since("soon", 1).is_err());
    }
}
