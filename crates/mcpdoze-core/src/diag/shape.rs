// SPDX-License-Identifier: GPL-3.0-or-later

//! Shape of a JSON value: type, length, class and digit count, never the
//! value. A bounded walk (at most [`MAX_NODES`] nodes, depth
//! [`MAX_DEPTH`]) so a hostile or huge argument tree costs a fixed amount.
//!
//! * strings: `{"t":"string","len":N,"cls":"path|url|json|base64|multiline|ascii|unicode"}`;
//! * integers: `{"t":"int","digits":N}`; floats `{"t":"float"}`;
//! * booleans keep their value (`"v"`), schema-declared `enum` members keep
//!   theirs (`"en"`);
//! * object keys are kept only when the tool's `inputSchema` declares them;
//!   any other key becomes `k#<hash8>` (salted HMAC), so a key that is
//!   itself data (an id, a path) never reaches the log;
//! * arrays record their length and the shapes of the first three items.
//!
//! The same walk fills an [`ArgSet`]: a bounded in-memory set of the string
//! values of the call (at most 32 values and 4 KiB), keyed by a *safe*
//! pointer built from the kept keys. The scrubber uses it to replace any
//! argument that shows up in stderr or an error message. It has no
//! `Serialize` impl and is never written.
//!
//! [`schema_diff`] compares arguments with the catalog `inputSchema`: missing
//! required keys, undeclared keys, and null or wrongly typed values. It is
//! the most diagnostic field of a record.

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;
use serde_json::{Map, Value};

use super::record::{Name, Scrubbed};
use super::redact::Salt;

/// Nodes visited per walk.
pub const MAX_NODES: usize = 200;
/// Nesting depth walked.
pub const MAX_DEPTH: usize = 6;
/// Values kept in an [`ArgSet`].
pub const ARGSET_MAX_VALUES: usize = 32;
/// Bytes kept in an [`ArgSet`].
pub const ARGSET_MAX_BYTES: usize = 4096;
const ARG_VALUE_CAP: usize = 256;
const ARG_MIN_LEN: usize = 4;
const ARRAY_ITEMS: usize = 3;

/// Shape of one JSON node.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShapeNode {
    /// Type: `null`, `bool`, `int`, `float`, `string`, `array`, `object`,
    /// `elided` (budget exhausted).
    pub t: &'static str,
    /// String length in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub len: Option<usize>,
    /// Decimal digits of an integer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digits: Option<u8>,
    /// String class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cls: Option<&'static str>,
    /// Array length or object key count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<usize>,
    /// Boolean value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v: Option<bool>,
    /// Schema-declared enum member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub en: Option<Name>,
    /// Object members.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys: Option<BTreeMap<Name, ShapeNode>>,
    /// Shapes of the first array items.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ShapeNode>,
    /// Children were cut by the depth or node limit.
    #[serde(skip_serializing_if = "is_false")]
    pub trunc: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl ShapeNode {
    fn of(t: &'static str) -> Self {
        ShapeNode {
            t,
            len: None,
            digits: None,
            cls: None,
            n: None,
            v: None,
            en: None,
            keys: None,
            items: Vec::new(),
            trunc: false,
        }
    }
}

/// Shape of a whole argument value. An object (the usual case) is written
/// as a plain map of key to node.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ShapeRoot {
    /// Members of a top-level object.
    Object(BTreeMap<Name, ShapeNode>),
    /// Any other top-level value.
    Node(ShapeNode),
}

/// Bounded in-memory set of a call's string values, with safe pointers.
/// Never serialised; `Debug` shows only the count.
#[derive(Default)]
pub struct ArgSet {
    items: Vec<(String, String)>,
    bytes: usize,
}

impl fmt::Debug for ArgSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArgSet")
            .field("values", &self.items.len())
            .finish()
    }
}

fn cap_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl ArgSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `(pointer, value)` pairs; the pointer must be safe (kept keys
    /// only). Used by tests; the walk uses it internally.
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut a = ArgSet::new();
        for (p, v) in pairs {
            a.offer(p, v);
        }
        a
    }

    fn push(&mut self, ptr: &str, value: &str) {
        let v = cap_str(value, ARG_VALUE_CAP);
        if v.len() < ARG_MIN_LEN
            || self.items.len() >= ARGSET_MAX_VALUES
            || self.bytes + v.len() > ARGSET_MAX_BYTES
        {
            return;
        }
        self.bytes += v.len();
        self.items.push((v.to_string(), ptr.to_string()));
    }

    fn offer(&mut self, ptr: &str, value: &str) {
        self.push(ptr, value);
        if value.contains('\n') {
            for line in value.lines() {
                let l = line.trim();
                if l.len() >= 8 && l.len() < value.len() {
                    self.push(ptr, l);
                }
            }
        }
    }

    /// `(value, safe pointer)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.items.iter().map(|(v, p)| (v.as_str(), p.as_str()))
    }

    /// Number of values held.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Class of a string value.
pub fn string_class(s: &str) -> &'static str {
    let t = s.trim();
    let b = t.as_bytes();
    if let Some(i) = t.find("://") {
        if i > 0
            && i <= 12
            && b[..i]
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || b"+-.".contains(c))
        {
            return "url";
        }
    }
    let drive = b.len() > 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/');
    let seps = t.bytes().filter(|c| *c == b'/' || *c == b'\\').count();
    if !t.contains('\n')
        && (drive
            || t.starts_with('/')
            || t.starts_with("~/")
            || t.starts_with("./")
            || t.starts_with("..")
            || t.starts_with("\\\\")
            || (seps >= 2 && !t.contains(' ')))
    {
        return "path";
    }
    if (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')) {
        return "json";
    }
    if t.len() >= 24
        && t.len().is_multiple_of(4)
        && !t.contains(char::is_whitespace)
        && t.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"+/=_-".contains(&c))
    {
        return "base64";
    }
    if s.contains('\n') {
        return "multiline";
    }
    if s.is_ascii() {
        "ascii"
    } else {
        "unicode"
    }
}

fn digits(n: u64) -> u8 {
    (n.checked_ilog10().unwrap_or(0) + 1) as u8
}

struct Walker<'a> {
    salt: &'a Salt,
    budget: usize,
    args: ArgSet,
}

fn schema_props(schema: Option<&Value>) -> Option<&Map<String, Value>> {
    schema?.get("properties")?.as_object()
}

fn enum_member(schema: Option<&Value>, v: &Value) -> bool {
    schema
        .and_then(|s| s.get("enum"))
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|m| m == v))
}

impl Walker<'_> {
    fn children(
        &mut self,
        map: &Map<String, Value>,
        schema: Option<&Value>,
        ptr: &str,
        depth: usize,
    ) -> (BTreeMap<Name, ShapeNode>, bool) {
        let props = schema_props(schema);
        let mut out = BTreeMap::new();
        let mut trunc = false;
        for (k, v) in map {
            if self.budget == 0 {
                trunc = true;
                break;
            }
            let declared = props.and_then(|p| p.get(k));
            let safe = match declared {
                Some(_) => Name::new(k).as_str().to_string(),
                None => format!("k#{}", self.salt.hash8("key", k)),
            };
            let child_ptr = format!("{ptr}/{safe}");
            let node = self.walk(v, declared, &child_ptr, depth + 1);
            out.insert(Name::new(&safe), node);
        }
        (out, trunc)
    }

    fn walk(&mut self, v: &Value, schema: Option<&Value>, ptr: &str, depth: usize) -> ShapeNode {
        if self.budget == 0 {
            return ShapeNode::of("elided");
        }
        self.budget -= 1;
        match v {
            Value::Null => ShapeNode::of("null"),
            Value::Bool(b) => {
                let mut n = ShapeNode::of("bool");
                n.v = Some(*b);
                n
            }
            Value::Number(num) => {
                if let Some(u) = num.as_u64() {
                    let mut n = ShapeNode::of("int");
                    n.digits = Some(digits(u));
                    if enum_member(schema, v) {
                        n.en = Some(Name::new(&u.to_string()));
                    } else if digits(u) >= 6 {
                        self.args.offer(ptr, &u.to_string());
                    }
                    n
                } else if let Some(i) = num.as_i64() {
                    let mut n = ShapeNode::of("int");
                    n.digits = Some(digits(i.unsigned_abs()));
                    if enum_member(schema, v) {
                        n.en = Some(Name::new(&i.to_string()));
                    }
                    n
                } else {
                    ShapeNode::of("float")
                }
            }
            Value::String(s) => {
                let mut n = ShapeNode::of("string");
                n.len = Some(s.len());
                n.cls = Some(string_class(s));
                if enum_member(schema, v) {
                    n.en = Some(Name::new(s));
                } else {
                    self.args.offer(ptr, s);
                }
                n
            }
            Value::Array(a) => {
                let mut n = ShapeNode::of("array");
                n.n = Some(a.len());
                if depth >= MAX_DEPTH {
                    n.trunc = !a.is_empty();
                    return n;
                }
                let item_schema = schema
                    .and_then(|s| s.get("items"))
                    .filter(|i| i.is_object());
                for (i, item) in a.iter().take(ARRAY_ITEMS).enumerate() {
                    if self.budget == 0 {
                        n.trunc = true;
                        break;
                    }
                    let p = format!("{ptr}/{i}");
                    n.items.push(self.walk(item, item_schema, &p, depth + 1));
                }
                n
            }
            Value::Object(m) => {
                let mut n = ShapeNode::of("object");
                n.n = Some(m.len());
                if depth >= MAX_DEPTH {
                    n.trunc = !m.is_empty();
                    return n;
                }
                let (keys, trunc) = self.children(m, schema, ptr, depth);
                n.keys = Some(keys);
                n.trunc = trunc;
                n
            }
        }
    }
}

/// Result of [`analyse`].
#[derive(Debug)]
pub struct Analysis {
    /// Shape of the value.
    pub shape: ShapeRoot,
    /// String values for the scrubber (in memory only).
    pub args: ArgSet,
    /// Arguments versus schema (when a schema was given).
    pub diff: Option<SchemaDiff>,
    /// Stable hash of the shape, for repeat detection (never written).
    pub shape_hash: u64,
}

/// Walks `value` (tool arguments, or an error `data`) against the optional
/// public `schema`.
pub fn analyse(value: &Value, schema: Option<&Value>, salt: &Salt) -> Analysis {
    let mut w = Walker {
        salt,
        budget: MAX_NODES,
        args: ArgSet::new(),
    };
    let shape = match value {
        Value::Object(m) => {
            w.budget = w.budget.saturating_sub(1);
            ShapeRoot::Object(w.children(m, schema, "", 1).0)
        }
        other => ShapeRoot::Node(w.walk(other, schema, "", 0)),
    };
    let diff = schema.map(|s| schema_diff(value, s));
    let shape_hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(&shape)
            .unwrap_or_default()
            .hash(&mut h);
        h.finish()
    };
    Analysis {
        shape,
        args: w.args,
        diff,
        shape_hash,
    }
}

/// Differences between arguments and the tool's `inputSchema`.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct SchemaDiff {
    /// Required keys that are absent.
    pub missing_required: Vec<Name>,
    /// Keys the schema does not declare.
    pub extra_keys: usize,
    /// `"/key: <actual>, schema says <expected>"`, composed from declared
    /// keys and type names only.
    pub type_mismatch: Vec<Scrubbed>,
}

impl SchemaDiff {
    /// No missing key and no type mismatch (extra keys are not an issue by
    /// themselves).
    pub fn is_empty_issues(&self) -> bool {
        self.missing_required.is_empty() && self.type_mismatch.is_empty()
    }

    /// Some declared, non-nullable key was sent as `null`.
    pub fn has_null_mismatch(&self) -> bool {
        self.type_mismatch
            .iter()
            .any(|m| m.as_str().contains(": null,"))
    }

    /// One hint per issue.
    pub fn hints(&self) -> Vec<Scrubbed> {
        let mut h: Vec<Scrubbed> = self
            .missing_required
            .iter()
            .map(|n| Scrubbed::from_safe(format!("missing required: {}", n.as_str())))
            .collect();
        h.extend(self.type_mismatch.iter().cloned());
        h
    }
}

const TYPE_NAMES: &[&str] = &[
    "null", "boolean", "integer", "number", "string", "array", "object",
];
const MAX_MISMATCHES: usize = 8;
const DIFF_DEPTH: usize = 3;

fn actual_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_ok(v: &Value, expected: &str) -> bool {
    match (expected, v) {
        ("number", Value::Number(_)) => true,
        ("integer", Value::Number(n)) => {
            !n.is_f64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        (e, v) => e == actual_type(v),
    }
}

fn expected_types(schema: &Value) -> Vec<&'static str> {
    let names: Vec<&str> = match schema.get("type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => return Vec::new(),
    };
    names
        .into_iter()
        .filter_map(|n| TYPE_NAMES.iter().find(|t| **t == n).copied())
        .collect()
}

fn diff_object(
    args: &Map<String, Value>,
    schema: &Value,
    ptr: &str,
    depth: usize,
    out: &mut SchemaDiff,
) {
    if let Some(req) = schema.get("required").and_then(Value::as_array) {
        for r in req.iter().filter_map(Value::as_str) {
            if !args.contains_key(r) && out.missing_required.len() < MAX_MISMATCHES {
                out.missing_required.push(Name::new(&format!("{ptr}/{r}")));
            }
        }
    }
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (k, v) in args {
        let Some(ps) = props.get(k) else {
            out.extra_keys += 1;
            continue;
        };
        let key = Name::new(k);
        let child = format!("{ptr}/{}", key.as_str());
        let want = expected_types(ps);
        if !want.is_empty() && !want.iter().any(|t| type_ok(v, t)) {
            if out.type_mismatch.len() < MAX_MISMATCHES {
                out.type_mismatch.push(Scrubbed::from_safe(format!(
                    "{child}: {}, schema says {}",
                    actual_type(v),
                    want.join("|")
                )));
            }
            continue;
        }
        if depth < DIFF_DEPTH {
            if let Value::Object(m) = v {
                diff_object(m, ps, &child, depth + 1, out);
            }
        }
    }
}

/// Compares `args` with `schema` (the tool's `inputSchema`).
pub fn schema_diff(args: &Value, schema: &Value) -> SchemaDiff {
    let mut out = SchemaDiff::default();
    if let Value::Object(m) = args {
        diff_object(m, schema, "", 0, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn salt() -> Salt {
        Salt::from_bytes([7; 32])
    }

    #[test]
    fn golden_shape_with_schema() {
        let schema = json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"},
                "cursor": {"type": "string"},
                "mode": {"type": "string", "enum": ["fast", "slow"]},
                "dry": {"type": "boolean"}
            }
        });
        let args = json!({
            "query": "line one\nline two with a secret",
            "limit": 25,
            "cursor": null,
            "mode": "fast",
            "dry": true,
            "extra-id-4711": "abc"
        });
        let a = analyse(&args, Some(&schema), &salt());
        let got = serde_json::to_value(&a.shape).unwrap();
        let extra = format!("k#{}", salt().hash8("key", "extra-id-4711"));
        assert_eq!(extra.len(), 10);
        let want = json!({
            "query": {"t": "string", "len": 31, "cls": "multiline"},
            "limit": {"t": "int", "digits": 2},
            "cursor": {"t": "null"},
            "mode": {"t": "string", "len": 4, "cls": "ascii", "en": "fast"},
            "dry": {"t": "bool", "v": true},
            extra.as_str(): {"t": "string", "len": 3, "cls": "ascii"}
        });
        assert_eq!(got, want);
        let text = got.to_string();
        assert!(!text.contains("secret") && !text.contains("4711"));

        let d = a.diff.unwrap();
        assert!(d.missing_required.is_empty());
        assert_eq!(d.extra_keys, 1);
        assert_eq!(
            d.type_mismatch
                .iter()
                .map(Scrubbed::as_str)
                .collect::<Vec<_>>(),
            vec!["/cursor: null, schema says string"]
        );
        assert!(d.has_null_mismatch());
        // The multi-line value and its lines are held for the scrubber only.
        let held: Vec<_> = a.args.iter().collect();
        assert!(held
            .iter()
            .any(|(v, p)| v.contains("line two") && *p == "/query"));
    }

    #[test]
    fn string_classes() {
        let cases = [
            ("https://example.com/a?b=c", "url"),
            ("C:\\work\\a.txt", "path"),
            ("/var/log/x", "path"),
            ("./rel", "path"),
            ("{\"a\":1}", "json"),
            ("QUJDREVGR0hJSktMTU5PUFFSU1RVVldY", "base64"),
            ("a\nb", "multiline"),
            ("hello world", "ascii"),
            ("h\u{e9}llo", "unicode"),
        ];
        for (s, want) in cases {
            assert_eq!(string_class(s), want, "{s}");
        }
    }

    #[test]
    fn walk_is_bounded() {
        let wide: Map<String, Value> = (0..1000).map(|i| (format!("k{i}"), json!(i))).collect();
        let a = analyse(&Value::Object(wide), None, &salt());
        let ShapeRoot::Object(m) = &a.shape else {
            panic!()
        };
        assert!(m.len() <= MAX_NODES);

        let mut deep = json!("leaf");
        for _ in 0..50 {
            deep = json!({ "a": deep });
        }
        let s = serde_json::to_string(&analyse(&deep, None, &salt()).shape).unwrap();
        assert!(
            s.matches("\"t\":\"object\"").count() <= MAX_DEPTH + 1,
            "{s}"
        );
        assert!(s.contains("\"trunc\":true"));
    }

    #[test]
    fn argset_is_bounded_and_skips_short_values() {
        let big: Map<String, Value> = (0..100)
            .map(|i| {
                (
                    format!("k{i}"),
                    json!(format!("value-number-{i:04}-{}", "x".repeat(300))),
                )
            })
            .collect();
        let a = analyse(&Value::Object(big), None, &salt());
        assert!(a.args.len() <= ARGSET_MAX_VALUES);
        assert!(a.args.iter().map(|(v, _)| v.len()).sum::<usize>() <= ARGSET_MAX_BYTES);
        let a2 = analyse(&json!({"a": "xy"}), None, &salt());
        assert!(a2.args.is_empty());
        assert_eq!(format!("{:?}", a2.args), "ArgSet { values: 0 }");
    }

    #[test]
    fn missing_required_and_nested_types() {
        let schema = json!({
            "required": ["a", "b"],
            "properties": {"a": {"type": ["integer", "null"]}, "o": {"type": "object", "properties": {"n": {"type": "number"}}}}
        });
        let d = schema_diff(&json!({"a": 1.5, "o": {"n": "x"}}), &schema);
        assert_eq!(d.missing_required, vec![Name::new("/b")]);
        let t: Vec<_> = d.type_mismatch.iter().map(Scrubbed::as_str).collect();
        assert_eq!(
            t,
            vec![
                "/a: number, schema says integer|null",
                "/o/n: string, schema says number"
            ]
        );
    }
}
