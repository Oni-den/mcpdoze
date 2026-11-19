// SPDX-License-Identifier: GPL-3.0-or-later

//! List transforms (techniques 3a and 3b): applied to the raw catalog tool
//! list every time it is served, never stored.

use serde_json::Value;

use super::ServerPolicy;

impl ServerPolicy {
    /// The tools as listed to agents: denied tools removed, descriptions cut,
    /// schema prose stripped. Names are still the backend's.
    pub fn view_tools(&self, tools: &[Value]) -> Vec<Value> {
        tools
            .iter()
            .filter(|t| t["name"].as_str().is_some_and(|n| self.allows(n)))
            .map(|t| self.view_tool(t))
            .collect()
    }

    // A transform that changes `structuredContent` (the field filter) must
    // remove the tool's `outputSchema` here: the filtered result no longer
    // validates against it. Truncation and minify leave it alone.
    fn view_tool(&self, tool: &Value) -> Value {
        let mut t = tool.clone();
        if t["name"].as_str().is_some_and(|n| self.filters_tool(n)) {
            if let Some(o) = t.as_object_mut() {
                o.remove("outputSchema");
            }
        }
        if let Some(max) = self.description_max_chars {
            if let Some(d) = t["description"].as_str() {
                let cut = trim_description(d, max);
                if cut.len() != d.len() {
                    t["description"] = Value::String(cut);
                }
            }
        }
        if self.strip_schema_descriptions {
            if let Some(s) = t.get_mut("inputSchema") {
                strip_prose(s);
            }
        }
        t
    }
}

/// Cuts `text` to at most `max` characters, at the last sentence end that
/// fits (else at the last space, else hard).
pub fn trim_description(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let head_len: usize = text.chars().take(max).map(char::len_utf8).sum();
    let head = &text[..head_len];
    let mut best = None;
    for (i, c) in head.char_indices() {
        if matches!(c, '.' | '!' | '?') {
            let end = i + c.len_utf8();
            if end == head.len() && text[end..].starts_with(char::is_whitespace)
                || head[end..].starts_with(char::is_whitespace)
            {
                best = Some(end);
            }
        }
    }
    let end = best
        .or_else(|| head.rfind(char::is_whitespace))
        .unwrap_or(head.len());
    head[..end].trim_end().to_owned()
}

/// Removes `description`, `title` and `examples` keywords inside a schema.
/// Property names (even one called `title`) are kept: `properties` is
/// walked by value only.
fn strip_prose(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for k in ["description", "title", "examples"] {
                if m.get(k).is_some_and(|x| !x.is_object()) {
                    m.remove(k);
                }
            }
            for (k, sub) in m.iter_mut() {
                if matches!(k.as_str(), "properties" | "$defs" | "definitions") {
                    if let Value::Object(props) = sub {
                        props.iter_mut().for_each(|(_, s)| strip_prose(s));
                    }
                } else {
                    strip_prose(sub);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(strip_prose),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn trims_at_sentence() {
        let t = "First sentence. Second one is long. Third.";
        assert_eq!(trim_description(t, 20), "First sentence.");
        assert_eq!(trim_description(t, 100), t);
        assert_eq!(trim_description("no break here at all", 9), "no break");
    }

    #[test]
    fn strips_prose_keeps_names() {
        let mut s = json!({"type":"object","description":"d","properties":{
            "title":{"type":"string","description":"x","title":"T"},
            "n":{"type":"integer","examples":[1]}},"required":["title"]});
        strip_prose(&mut s);
        assert_eq!(
            s,
            json!({"type":"object","properties":{"title":{"type":"string"},
            "n":{"type":"integer"}},"required":["title"]})
        );
    }
}
