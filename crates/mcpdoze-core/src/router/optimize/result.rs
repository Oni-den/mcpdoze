// SPDX-License-Identifier: GPL-3.0-or-later

//! Result transforms. Global rules: an `isError` result and a JSON-RPC error
//! are never touched, non-text blocks are never touched, and a transform
//! that does not make the response smaller is discarded.

use std::sync::Arc;

use serde_json::Value;

use super::store::{ResultStore, Stored};
use super::ServerPolicy;
use crate::config::PointerGlob;
use crate::router::Session;
use crate::stats::json_len;

/// Tag recorded for `minify_json`.
pub const TAG_MINIFY: &str = "minify";
/// Tag recorded for `encode = "toon"`.
pub const TAG_TOON: &str = "toon";
/// Tag recorded for `max_result_bytes`.
pub const TAG_TRUNCATE: &str = "truncate";
/// Tag recorded for the per-tool field filter.
pub const TAG_FILTER: &str = "filter";
/// Marker block appended when fields were filtered.
pub const FILTER_MARKER: &str = "[mcpdoze: fields filtered]";
/// Text blocks shorter than this are never cut.
const MIN_CUT: usize = 256;

/// Where truncated results are kept.
pub struct StoreCtx<'a> {
    /// The router's store.
    pub store: &'a Arc<ResultStore>,
    /// The (stored, non-ephemeral) agent session.
    pub session: &'a Arc<Session>,
}

/// `keep` or `drop` pointer-globs of one tool.
#[derive(Debug)]
pub struct FieldFilter {
    keep: Vec<PointerGlob>,
    drop: Vec<PointerGlob>,
}

impl FieldFilter {
    /// Compiles the globs; `None` when both lists are empty.
    pub fn new(keep: &[String], drop: &[String]) -> Result<Option<Self>, String> {
        if keep.is_empty() && drop.is_empty() {
            return Ok(None);
        }
        let c = |l: &[String]| {
            l.iter()
                .map(|g| PointerGlob::new(g))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Some(Self {
            keep: c(keep)?,
            drop: c(drop)?,
        }))
    }

    /// Projects `v`; returns whether anything was removed.
    pub fn apply(&self, v: &mut Value) -> bool {
        let new = self.walk(v, &mut String::new()).unwrap_or_else(|| {
            // The root itself is never removed.
            match v {
                Value::Array(_) => Value::Array(Vec::new()),
                _ => Value::Object(serde_json::Map::new()),
            }
        });
        let changed = new != *v;
        *v = new;
        changed
    }

    fn walk(&self, v: &Value, path: &mut String) -> Option<Value> {
        if !self.keep.is_empty() {
            if self.keep.iter().any(|g| g.matches(path)) {
                return Some(v.clone());
            }
            if !self.keep.iter().any(|g| g.matches_or_ancestor(path)) {
                return None;
            }
        } else if !path.is_empty() && self.drop.iter().any(|g| g.matches(path)) {
            return None;
        }
        let len = path.len();
        match v {
            Value::Object(m) => {
                let mut o = serde_json::Map::new();
                for (k, c) in m {
                    push_seg(path, k);
                    if let Some(n) = self.walk(c, path) {
                        o.insert(k.clone(), n);
                    }
                    path.truncate(len);
                }
                Some(Value::Object(o))
            }
            Value::Array(a) => {
                let mut o = Vec::new();
                for (i, c) in a.iter().enumerate() {
                    push_seg(path, &i.to_string());
                    if let Some(n) = self.walk(c, path) {
                        o.push(n);
                    }
                    path.truncate(len);
                }
                Some(Value::Array(o))
            }
            // A scalar that is not itself matched is dropped in keep mode.
            _ if !self.keep.is_empty() => None,
            _ => Some(v.clone()),
        }
    }
}

fn push_seg(path: &mut String, seg: &str) {
    path.push('/');
    for c in seg.chars() {
        match c {
            '~' => path.push_str("~0"),
            '/' => path.push_str("~1"),
            c => path.push(c),
        }
    }
}

fn is_untouchable(resp: &Value) -> bool {
    resp.get("error").is_some() || resp["result"]["isError"] == Value::Bool(true)
}

/// Applies `f` to `structuredContent` and to JSON text blocks; appends the
/// marker block when something changed. Returns whether `resp` changed.
pub fn filter_fields(resp: &mut Value, f: &FieldFilter) -> bool {
    if is_untouchable(resp) {
        return false;
    }
    let mut changed = false;
    if let Some(sc) = resp.pointer_mut("/result/structuredContent") {
        changed |= f.apply(sc);
    }
    if let Some(blocks) = resp
        .pointer_mut("/result/content")
        .and_then(Value::as_array_mut)
    {
        for b in blocks.iter_mut() {
            if b["type"] != "text" {
                continue;
            }
            let Some(text) = b["text"].as_str() else {
                continue;
            };
            let t = text.trim_start();
            if !(t.starts_with('{') || t.starts_with('[')) {
                continue;
            }
            let Ok(mut v) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if f.apply(&mut v) {
                if let Ok(s) = serde_json::to_string(&v) {
                    b["text"] = Value::String(s);
                    changed = true;
                }
            }
        }
        if changed {
            blocks.push(serde_json::json!({"type":"text","text":FILTER_MARKER}));
        }
    }
    changed
}

/// The text of a `tools/call` result as `result_get` pages it: the text
/// blocks joined with a newline.
pub fn text_view(result: &Value) -> String {
    let mut out = String::new();
    let mut first = true;
    for b in result["content"].as_array().into_iter().flatten() {
        if let (Some("text"), Some(t)) = (b["type"].as_str(), b["text"].as_str()) {
            if !first {
                out.push('\n');
            }
            out.push_str(t);
            first = false;
        }
    }
    out
}

fn text_bytes(result: &Value) -> usize {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .map(str::len)
        .sum()
}

/// Cuts `text` to about `budget` bytes: 80% head, 20% tail.
fn cut(text: &str, budget: usize) -> String {
    let tail = budget / 5;
    let mut head = budget - tail;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut ts = text.len() - tail;
    while !text.is_char_boundary(ts) {
        ts += 1;
    }
    format!("{}\n...\n{}", &text[..head], &text[ts..])
}

/// Shrinks the text blocks of `result` to about `max` bytes in total (each
/// block to its share; short blocks and non-text blocks stay) and appends the
/// marker block naming `reference`. Returns `(kept, total)` text bytes.
pub fn truncate_texts(result: &mut Value, max: usize, reference: &str) -> (usize, usize) {
    let total = text_bytes(result);
    let mut kept = 0usize;
    if let Some(blocks) = result["content"].as_array_mut() {
        for b in blocks.iter_mut() {
            if b["type"] != "text" {
                continue;
            }
            let Some(t) = b["text"].as_str() else {
                continue;
            };
            let share = max * t.len() / total.max(1);
            if t.len() > MIN_CUT && t.len() > share && share > 16 {
                let c = cut(t, share);
                kept += c.len();
                b["text"] = Value::String(c);
            } else {
                kept += t.len();
            }
        }
        blocks.push(serde_json::json!({"type":"text","text":format!(
            "[mcpdoze: truncated, {kept} of {total} bytes; call mcpdoze__result_get with ref={reference}]"
        )}));
    }
    (kept, total)
}

/// Re-serialises pretty-printed JSON text blocks compactly (lossless: the
/// compact text must parse back to the same value). `structuredContent` is
/// not changed, so it stays consistent with the text. Returns whether `resp`
/// was changed.
pub fn minify_json(resp: &mut Value) -> bool {
    if is_untouchable(resp) {
        return false;
    }
    let Some(blocks) = resp
        .pointer_mut("/result/content")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    for b in blocks {
        if b["type"] != "text" {
            continue;
        }
        let Some(text) = b["text"].as_str() else {
            continue;
        };
        let t = text.trim_start();
        if !(t.starts_with('{') || t.starts_with('[')) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        let Ok(compact) = serde_json::to_string(&v) else {
            continue;
        };
        let lossless = serde_json::from_str::<Value>(&compact).is_ok_and(|w| w == v);
        if lossless && compact.len() < text.len() {
            b["text"] = Value::String(compact);
            changed = true;
        }
    }
    changed
}

/// Replaces JSON text blocks with their TOON encoding when the round trip
/// is lossless and the saving is at least 20%. `structuredContent` is not
/// changed. Returns whether `resp` was changed.
pub fn toon_encode(resp: &mut Value) -> bool {
    if is_untouchable(resp) {
        return false;
    }
    let Some(blocks) = resp
        .pointer_mut("/result/content")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    for b in blocks {
        if b["type"] != "text" {
            continue;
        }
        let Some(text) = b["text"].as_str() else {
            continue;
        };
        let t = text.trim_start();
        if !(t.starts_with('{') || t.starts_with('[')) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if let Some(enc) = super::toon::encode_if_better(&v, text.len()) {
            b["text"] = Value::String(enc);
            changed = true;
        }
    }
    changed
}

impl ServerPolicy {
    /// Applies the configured result transforms to `resp` (a full JSON-RPC
    /// response of `tools/call` of backend tool `tool`) and returns the tags
    /// applied. `resp` is left as it was unless the result is smaller.
    /// Truncation needs `store` (absent for stateless requests: the result
    /// is then sent whole). The stored copy is the result as it would have
    /// been sent without truncation (after filter and minify), so paging and
    /// pointers match what the agent sees.
    pub fn transform_result(
        &self,
        tool: &str,
        resp: &mut Value,
        store: Option<&StoreCtx<'_>>,
    ) -> Vec<String> {
        if !self.transforms_results() || is_untouchable(resp) {
            return Vec::new();
        }
        let before = json_len(resp);
        let mut work = resp.clone();
        let mut tags = Vec::new();
        if let Some(f) = self.filters.get(tool) {
            if filter_fields(&mut work, f) {
                tags.push(TAG_FILTER.to_owned());
            }
        }
        if self.minify_json && minify_json(&mut work) {
            tags.push(TAG_MINIFY.to_owned());
        }
        if self.encode_toon && toon_encode(&mut work) {
            tags.push(TAG_TOON.to_owned());
        }
        let mut stored = None;
        if let (max @ 1.., Some(ctx)) = (self.max_result_bytes, store) {
            let max = usize::try_from(max).unwrap_or(usize::MAX);
            if text_bytes(&work["result"]) > max {
                let put = ctx.store.put(
                    ctx.session.id(),
                    Stored {
                        tool: tool.to_owned(),
                        result: work["result"].clone(),
                        page: max,
                    },
                );
                if let Some(put) = put {
                    truncate_texts(&mut work["result"], max, &put.id);
                    if put.first_for_session {
                        let st = ctx.store.clone();
                        let sid = ctx.session.id().to_owned();
                        ctx.session.on_end(move || st.purge_session(&sid));
                    }
                    tags.push(TAG_TRUNCATE.to_owned());
                    stored = Some(put.id);
                }
            }
        }
        if tags.is_empty() || json_len(&work) >= before {
            if let (Some(id), Some(ctx)) = (stored, store) {
                ctx.store.remove(ctx.session.id(), &id);
            }
            return Vec::new();
        }
        *resp = work;
        tags
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resp(text: &str, is_error: bool) -> Value {
        json!({"jsonrpc":"2.0","id":1,"result":{
            "content":[{"type":"text","text":text},
                       {"type":"image","data":"x","mimeType":"image/png"}],
            "isError":is_error}})
    }

    #[test]
    fn minifies_and_skips() {
        let pretty = serde_json::to_string_pretty(&json!({"a":[1,2,3],"b":"x"})).unwrap();
        let mut r = resp(&pretty, false);
        assert!(minify_json(&mut r));
        assert_eq!(
            r["result"]["content"][0]["text"],
            "{\"a\":[1,2,3],\"b\":\"x\"}"
        );
        assert_eq!(r["result"]["content"][1]["data"], "x");
        let mut e = resp(&pretty, true);
        assert!(!minify_json(&mut e));
        let mut p = resp("plain text", false);
        assert!(!minify_json(&mut p));
        let mut already = resp("{\"a\":1}", false);
        assert!(!minify_json(&mut already));
    }

    fn f(keep: &[&str], drop: &[&str]) -> FieldFilter {
        let s = |l: &[&str]| l.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        FieldFilter::new(&s(keep), &s(drop)).unwrap().unwrap()
    }

    #[test]
    fn keep_and_drop_project() {
        let v = json!({"items":[{"id":1,"title":"a","body":"x"},{"id":2,"title":"b","body":"y"}],"meta":{"n":2}});
        let mut k = v.clone();
        assert!(f(&["/items/*/id", "/items/*/title"], &[]).apply(&mut k));
        assert_eq!(
            k,
            json!({"items":[{"id":1,"title":"a"},{"id":2,"title":"b"}]})
        );
        let mut d = v.clone();
        assert!(f(&[], &["/items/*/body", "/meta"]).apply(&mut d));
        assert_eq!(
            d,
            json!({"items":[{"id":1,"title":"a"},{"id":2,"title":"b"}]})
        );
        let mut same = json!({"a":1});
        assert!(!f(&[], &["/zzz"]).apply(&mut same));
    }

    #[test]
    fn filter_touches_text_and_structured_but_not_errors() {
        let body = json!({"id":1,"junk":"zzz"});
        let mut r = json!({"result":{"content":[{"type":"text","text":body.to_string()},
            {"type":"image","data":"x","mimeType":"image/png"}],"structuredContent":body}});
        assert!(filter_fields(&mut r, &f(&["/id"], &[])));
        assert_eq!(r["result"]["structuredContent"], json!({"id":1}));
        assert_eq!(r["result"]["content"][0]["text"], "{\"id\":1}");
        assert_eq!(r["result"]["content"][1]["data"], "x");
        assert_eq!(r["result"]["content"][2]["text"], FILTER_MARKER);
        let mut e = resp(&body.to_string(), true);
        assert!(!filter_fields(&mut e, &f(&["/id"], &[])));
    }

    #[test]
    fn truncation_keeps_head_and_tail_and_is_char_safe() {
        let text = "é".repeat(2000);
        let mut r = json!({"content":[{"type":"text","text":text},
            {"type":"image","data":"x","mimeType":"image/png"}]});
        let (kept, total) = truncate_texts(&mut r, 600, "REF");
        assert_eq!(total, 4000);
        assert!(kept < 800, "{kept}");
        let t = r["content"][0]["text"].as_str().unwrap();
        assert!(t.contains("\n...\n") && t.starts_with('é') && t.ends_with('é'));
        assert_eq!(r["content"][1]["data"], "x");
        let m = r["content"][2]["text"].as_str().unwrap();
        assert!(m.contains("of 4000 bytes") && m.contains("ref=REF"), "{m}");
    }

    #[test]
    fn is_error_results_are_never_truncated() {
        let store = Arc::new(ResultStore::default());
        let session = Arc::new(Session::new("2025-11-25", false));
        let ctx = StoreCtx {
            store: &store,
            session: &session,
        };
        let cfg = crate::config::Config::parse(
            "[servers.m]\ntransport = \"stdio\"\ncommand = \"x\"\n[servers.m.optimize]\nmax_result_bytes = 512\n",
        )
        .unwrap();
        let p = ServerPolicy::compile(&cfg.servers["m"]).unwrap().unwrap();
        let big = "y".repeat(5000);
        let mut e = resp(&big, true);
        let orig = e.clone();
        assert!(p.transform_result("t", &mut e, Some(&ctx)).is_empty());
        assert_eq!(e, orig);
        assert_eq!(store.usage().0, 0);
        let mut ok = resp(&big, false);
        assert_eq!(
            p.transform_result("t", &mut ok, Some(&ctx)),
            vec!["truncate"]
        );
        assert_eq!(store.usage().0, 1);
        // No store (stateless request): sent whole.
        let mut s = resp(&big, false);
        assert!(p.transform_result("t", &mut s, None).is_empty());
    }
}
