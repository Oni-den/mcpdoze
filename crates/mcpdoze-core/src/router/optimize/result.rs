// SPDX-License-Identifier: GPL-3.0-or-later

//! Result transforms. Global rules: an `isError` result and a JSON-RPC error
//! are never touched, non-text blocks are never touched, and a transform
//! that does not make the response smaller is discarded.

use serde_json::Value;

use super::ServerPolicy;
use crate::stats::json_len;

/// Tag recorded for `minify_json`.
pub const TAG_MINIFY: &str = "minify";

/// Re-serialises pretty-printed JSON text blocks compactly (lossless: the
/// compact text must parse back to the same value). `structuredContent` is
/// not changed, so it stays consistent with the text. Returns whether `resp`
/// was changed.
pub fn minify_json(resp: &mut Value) -> bool {
    if resp.get("error").is_some() || resp["result"]["isError"] == Value::Bool(true) {
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

impl ServerPolicy {
    /// Applies the configured result transforms to `resp` (a full JSON-RPC
    /// response of `tools/call`) and returns the tags applied. `resp` is left
    /// as it was unless the result is smaller.
    pub fn transform_result(&self, resp: &mut Value) -> Vec<String> {
        if !self.transforms_results() {
            return Vec::new();
        }
        let before = json_len(resp);
        let mut work = resp.clone();
        let mut tags = Vec::new();
        if self.minify_json && minify_json(&mut work) {
            tags.push(TAG_MINIFY.to_owned());
        }
        if tags.is_empty() || json_len(&work) >= before {
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
}
