// SPDX-License-Identifier: GPL-3.0-or-later

//! A small TOON-style text encoding of JSON (`encode = "toon"`).
//!
//! Objects are `key: value` lines with two-space indentation. A uniform array
//! of flat objects becomes a table: `items[3]{id,name}:` followed by one
//! comma-delimited row per element. An array of scalars is inline
//! (`tags[2]: a,b`); any other array is a list of `- ` items. Strings are
//! bare unless that would be ambiguous (delimiters, quotes, control
//! characters, edge whitespace, or text that reads as a number, boolean or
//! null), in which case they are JSON-quoted.
//!
//! The decoder exists only so the router can prove `decode(encode(v)) == v`
//! (numbers exactly, key order included) before it sends TOON to an agent.

use serde_json::{Map, Number, Value};

/// Encodes `v`. A root scalar is not supported (callers only pass JSON
/// objects and arrays); an empty root object encodes as the empty string.
pub fn encode(v: &Value) -> String {
    let mut out = String::new();
    match v {
        Value::Object(m) => enc_obj(m, 0, &mut out),
        _ => enc_value(v, 0, "", &mut out),
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Encodes `v` and returns the text only when it decodes back to exactly
/// `v` (same numbers, same key order) and is at least 20% smaller than
/// `original_len` bytes. Empty root containers are never encoded.
pub fn encode_if_better(v: &Value, original_len: usize) -> Option<String> {
    let nonempty = match v {
        Value::Object(m) => !m.is_empty(),
        Value::Array(a) => !a.is_empty(),
        _ => false,
    };
    if !nonempty {
        return None;
    }
    let t = encode(v);
    if t.len() * 5 > original_len * 4 {
        return None;
    }
    let back = decode(&t).ok()?;
    (same(v, &back)).then_some(t)
}

/// Equality that also compares object key order.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|((k1, v1), (k2, v2))| k1 == k2 && same(v1, v2))
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        _ => a == b,
    }
}

fn is_scalar(v: &Value) -> bool {
    !matches!(v, Value::Object(_) | Value::Array(_))
}

fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if c.is_control() => {
                let mut b = [0u16; 2];
                for u in c.encode_utf16(&mut b) {
                    o.push_str(&format!("\\u{u:04x}"));
                }
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn bare_string_ok(s: &str) -> bool {
    let (Some(first), Some(last)) = (s.chars().next(), s.chars().next_back()) else {
        return false;
    };
    !(first.is_whitespace()
        || last.is_whitespace()
        || first == '-'
        || s.chars()
            .any(|c| c.is_control() || matches!(c, ',' | ':' | '"' | '\\' | '[' | ']' | '{' | '}'))
        || matches!(s, "true" | "false" | "null")
        || serde_json::from_str::<Number>(s).is_ok())
}

fn scalar(v: &Value) -> String {
    match v {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) if bare_string_ok(s) => s.clone(),
        Value::String(s) => quote(s),
        _ => String::new(),
    }
}

fn key_str(k: &str) -> String {
    let ok = k
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    if ok {
        k.to_owned()
    } else {
        quote(k)
    }
}

fn pad(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn enc_obj(m: &Map<String, Value>, indent: usize, out: &mut String) {
    for (k, v) in m {
        enc_value(v, indent, &key_str(k), out);
    }
}

/// Writes `lead` (a key, `- ` or nothing) plus the value on its own line(s).
fn enc_value(v: &Value, indent: usize, lead: &str, out: &mut String) {
    pad(out, indent);
    match v {
        Value::Object(m) => {
            out.push_str(lead);
            out.push_str(":\n");
            enc_obj(m, indent + 1, out);
        }
        Value::Array(a) => {
            out.push_str(lead);
            enc_array(a, indent, out);
        }
        _ => {
            out.push_str(lead);
            out.push_str(": ");
            out.push_str(&scalar(v));
            out.push('\n');
        }
    }
}

fn tabular_fields(a: &[Value]) -> Option<Vec<&String>> {
    let first = a.first()?.as_object()?;
    if first.is_empty() {
        return None;
    }
    let fields: Vec<&String> = first.keys().collect();
    let uniform = a.iter().all(|e| {
        e.as_object().is_some_and(|m| {
            m.len() == fields.len()
                && m.iter()
                    .zip(&fields)
                    .all(|((k, v), f)| k == *f && is_scalar(v))
        })
    });
    uniform.then_some(fields)
}

fn enc_array(a: &[Value], indent: usize, out: &mut String) {
    out.push_str(&format!("[{}]", a.len()));
    if a.is_empty() {
        out.push_str(":\n");
    } else if let Some(fields) = tabular_fields(a) {
        let names: Vec<String> = fields.iter().map(|f| key_str(f)).collect();
        out.push_str(&format!("{{{}}}:\n", names.join(",")));
        for e in a {
            pad(out, indent + 1);
            let cells: Vec<String> = e
                .as_object()
                .into_iter()
                .flat_map(|m| m.values().map(scalar))
                .collect();
            out.push_str(&cells.join(","));
            out.push('\n');
        }
    } else if a.iter().all(is_scalar) {
        let cells: Vec<String> = a.iter().map(scalar).collect();
        out.push_str(": ");
        out.push_str(&cells.join(","));
        out.push('\n');
    } else {
        out.push_str(":\n");
        for e in a {
            match e {
                Value::Object(m) => {
                    pad(out, indent + 1);
                    out.push_str("-\n");
                    enc_obj(m, indent + 2, out);
                }
                Value::Array(_) => enc_value(e, indent + 1, "- ", out),
                _ => {
                    pad(out, indent + 1);
                    out.push_str("- ");
                    out.push_str(&scalar(e));
                    out.push('\n');
                }
            }
        }
    }
}

// ---------------------------------------------------------------- decoder

/// Decodes text produced by [`encode`]. Strict: anything else is an error.
pub fn decode(text: &str) -> Result<Value, String> {
    let mut lines = Vec::new();
    for l in text.split('\n') {
        let body = l.trim_start_matches(' ');
        let sp = l.len() - body.len();
        if sp % 2 != 0 || body.is_empty() || body.starts_with('\t') {
            if text.is_empty() {
                break;
            }
            return Err("bad indentation or empty line".into());
        }
        lines.push((sp / 2, body));
    }
    let mut d = Dec { lines, pos: 0 };
    let v = match d.lines.first() {
        None => Value::Object(Map::new()),
        Some((_, t)) if t.starts_with('[') => d.entry_value(None, 0)?.1,
        Some(_) => d.object(0)?,
    };
    if d.pos != d.lines.len() {
        return Err("trailing lines".into());
    }
    Ok(v)
}

struct Dec<'a> {
    lines: Vec<(usize, &'a str)>,
    pos: usize,
}

/// Header of one entry: key, optional `[n]` / `{fields}`, and the text
/// after `:` (or after `: `).
struct Header {
    key: Option<String>,
    arr: Option<(usize, Option<Vec<String>>)>,
    rest: Option<String>,
}

struct Cur<'a> {
    s: &'a str,
    i: usize,
}

impl Cur<'_> {
    fn peek(&self) -> Option<char> {
        self.s[self.i..].chars().next()
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.i += c.len_utf8();
            true
        } else {
            false
        }
    }
    fn quoted(&mut self) -> Result<String, String> {
        if !self.eat('"') {
            return Err("expected quote".into());
        }
        let mut o = String::new();
        loop {
            let c = self.peek().ok_or("unterminated string")?;
            self.i += c.len_utf8();
            match c {
                '"' => return Ok(o),
                '\\' => {
                    let e = self.peek().ok_or("bad escape")?;
                    self.i += e.len_utf8();
                    match e {
                        '"' => o.push('"'),
                        '\\' => o.push('\\'),
                        'n' => o.push('\n'),
                        'r' => o.push('\r'),
                        't' => o.push('\t'),
                        'u' => {
                            let h = self.s.get(self.i..self.i + 4).ok_or("bad \\u")?;
                            let mut u = u32::from_str_radix(h, 16).map_err(|e| e.to_string())?;
                            self.i += 4;
                            if (0xD800..0xDC00).contains(&u) {
                                let rest = self.s.get(self.i..self.i + 6).ok_or("bad surrogate")?;
                                let lo = rest
                                    .strip_prefix("\\u")
                                    .and_then(|x| u32::from_str_radix(x, 16).ok())
                                    .filter(|l| (0xDC00..0xE000).contains(l))
                                    .ok_or("bad surrogate")?;
                                self.i += 6;
                                u = 0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00);
                            }
                            o.push(char::from_u32(u).ok_or("bad codepoint")?);
                        }
                        _ => return Err("unknown escape".into()),
                    }
                }
                c => o.push(c),
            }
        }
    }
    /// A key: quoted or bare (`[A-Za-z0-9_.]*`).
    fn key(&mut self) -> Result<String, String> {
        if self.peek() == Some('"') {
            return self.quoted();
        }
        let st = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        {
            self.i += 1;
        }
        Ok(self.s[st..self.i].to_owned())
    }
    /// A scalar token ending at `,` or the end of the text.
    fn token(&mut self) -> Result<Value, String> {
        if self.peek() == Some('"') {
            return self.quoted().map(Value::String);
        }
        let st = self.i;
        while self.peek().is_some_and(|c| c != ',') {
            self.i += self.peek().map_or(0, char::len_utf8);
        }
        let t = &self.s[st..self.i];
        if t.is_empty() {
            return Err("empty token".into());
        }
        Ok(match t {
            "null" => Value::Null,
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => match serde_json::from_str::<Number>(t) {
                Ok(n) => Value::Number(n),
                Err(_) => Value::String(t.to_owned()),
            },
        })
    }
    fn cells(&mut self) -> Result<Vec<Value>, String> {
        let mut v = vec![self.token()?];
        while self.eat(',') {
            v.push(self.token()?);
        }
        if self.i != self.s.len() {
            return Err("trailing text".into());
        }
        Ok(v)
    }
}

fn parse_header(text: &str) -> Result<Header, String> {
    let mut c = Cur { s: text, i: 0 };
    let key = if c.peek() == Some('[') {
        None
    } else {
        Some(c.key()?)
    };
    let mut arr = None;
    if c.eat('[') {
        let st = c.i;
        while c.peek().is_some_and(|x| x.is_ascii_digit()) {
            c.i += 1;
        }
        let n: usize = c.s[st..c.i].parse().map_err(|_| "bad count")?;
        if !c.eat(']') {
            return Err("expected ]".into());
        }
        let mut fields = None;
        if c.eat('{') {
            let mut f = vec![c.key()?];
            while c.eat(',') {
                f.push(c.key()?);
            }
            if !c.eat('}') {
                return Err("expected }".into());
            }
            fields = Some(f);
        }
        arr = Some((n, fields));
    }
    if !c.eat(':') {
        return Err("expected :".into());
    }
    let rest = if c.i == text.len() {
        None
    } else if c.eat(' ') && c.i < text.len() {
        Some(text[c.i..].to_owned())
    } else {
        return Err("expected space after colon".into());
    };
    Ok(Header { key, arr, rest })
}

impl Dec<'_> {
    fn object(&mut self, indent: usize) -> Result<Value, String> {
        let mut m = Map::new();
        while let Some(&(ind, _)) = self.lines.get(self.pos) {
            if ind != indent {
                if ind > indent {
                    return Err("unexpected indent".into());
                }
                break;
            }
            let (_, v) = self.entry_value(None, indent)?;
            let Value::Object(one) = v else {
                return Err("expected entry".into());
            };
            for (k, v) in one {
                if m.insert(k, v).is_some() {
                    return Err("duplicate key".into());
                }
            }
        }
        Ok(Value::Object(m))
    }

    /// Parses the entry at the current line. With `item` unset (an object
    /// entry or the root) a keyed entry returns `{key: value}`; a keyless
    /// array header (root) returns `(None, array)`.
    fn entry_value(
        &mut self,
        item: Option<&str>,
        indent: usize,
    ) -> Result<(Option<String>, Value), String> {
        let (_, line) = self.lines[self.pos];
        let text = item.unwrap_or(line);
        let h = parse_header(text)?;
        self.pos += 1;
        let val = match h.arr {
            None => match h.rest {
                Some(r) => {
                    let mut c = Cur { s: &r, i: 0 };
                    let v = c.token()?;
                    if c.i != r.len() {
                        return Err("trailing text".into());
                    }
                    v
                }
                None => {
                    if self.lines.get(self.pos).is_some_and(|l| l.0 == indent + 1) {
                        self.object(indent + 1)?
                    } else {
                        Value::Object(Map::new())
                    }
                }
            },
            Some((n, fields)) => self.array(n, fields, h.rest, indent)?,
        };
        match (h.key, item) {
            (Some(k), None) => {
                let mut m = Map::new();
                m.insert(k.clone(), val);
                Ok((Some(k), Value::Object(m)))
            }
            (None, _) => Ok((None, val)),
            (Some(_), Some(_)) => Err("keyed list item".into()),
        }
    }

    fn array(
        &mut self,
        n: usize,
        fields: Option<Vec<String>>,
        rest: Option<String>,
        indent: usize,
    ) -> Result<Value, String> {
        let mut out = Vec::new();
        if let Some(f) = fields {
            for _ in 0..n {
                let &(ind, t) = self.lines.get(self.pos).ok_or("missing row")?;
                if ind != indent + 1 {
                    return Err("bad row indent".into());
                }
                self.pos += 1;
                let mut c = Cur { s: t, i: 0 };
                let cells = c.cells()?;
                if cells.len() != f.len() {
                    return Err("row width".into());
                }
                out.push(Value::Object(f.iter().cloned().zip(cells).collect()));
            }
        } else if let Some(r) = rest {
            let cells = Cur { s: &r, i: 0 }.cells()?;
            if cells.len() != n {
                return Err("inline count".into());
            }
            out = cells;
        } else {
            for _ in 0..n {
                let &(ind, t) = self.lines.get(self.pos).ok_or("missing item")?;
                if ind != indent + 1 {
                    return Err("bad item indent".into());
                }
                if t == "-" {
                    self.pos += 1;
                    if self.lines.get(self.pos).is_some_and(|l| l.0 == indent + 2) {
                        out.push(self.object(indent + 2)?);
                    } else {
                        out.push(Value::Object(Map::new()));
                    }
                } else if let Some(r) = t.strip_prefix("- ") {
                    if r.starts_with('[') {
                        out.push(self.entry_value(Some(r), indent + 1)?.1);
                    } else {
                        let mut c = Cur { s: r, i: 0 };
                        let v = c.token()?;
                        if c.i != r.len() {
                            return Err("trailing text".into());
                        }
                        self.pos += 1;
                        out.push(v);
                    }
                } else {
                    return Err("expected list item".into());
                }
            }
        }
        Ok(Value::Array(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rt(v: &Value) -> String {
        let t = encode(v);
        let back = decode(&t).unwrap_or_else(|e| panic!("{e}\n{t}"));
        assert!(same(v, &back), "{v}\n{t}\n{back}");
        t
    }

    #[test]
    fn uniform_array_is_tabular() {
        let v = json!({"items":[
            {"id":1,"name":"a","status":"ok"},
            {"id":2,"name":"b","status":"bad"},
            {"id":3,"name":"c","status":"ok"}]});
        let t = rt(&v);
        assert_eq!(
            t,
            "items[3]{id,name,status}:\n  1,a,ok\n  2,b,bad\n  3,c,ok"
        );
        assert!(!t.contains('{') || t.matches('{').count() == 1);
    }

    #[test]
    fn root_arrays() {
        let t = rt(&json!([{"a":1},{"a":2}]));
        assert_eq!(t, "[2]{a}:\n  1\n  2");
        rt(&json!([1, "x", null, true]));
        rt(&json!([[1, 2], [3], []]));
        rt(&json!([{"a":[1,2]}, {"b":{"c":1}}, 5, {}]));
    }

    #[test]
    fn mixed_and_nested() {
        rt(&json!({"a":{"b":{"c":[1,2,{"d":null}]}},"e":[{"x":1},{"y":2}],"f":[{"x":1,"y":[1]}]}));
        // Non-uniform key sets or order fall back to list form.
        let t = rt(&json!([{"a":1,"b":2},{"b":2,"a":1}]));
        assert!(
            t.contains(
                "
  -
"
            ),
            "{t}"
        );
        let t = rt(&json!({"k":[{"a":1},{"a":2,"b":3}]}));
        assert!(!t.contains("{a}"), "{t}");
    }

    #[test]
    fn key_order_is_kept() {
        let v: Value = serde_json::from_str(r#"{"z":1,"a":2,"m":{"y":1,"b":2}}"#).unwrap();
        let t = rt(&v);
        assert_eq!(decode(&t).unwrap().to_string(), v.to_string());
    }

    #[test]
    fn special_strings_round_trip() {
        let specials = [
            "",
            " ",
            " lead",
            "trail ",
            "a,b",
            "a:b",
            "say \"hi\"",
            "back\\slash",
            "line\nbreak",
            "tab\there",
            "cr\rx",
            "nul\u{0}x",
            "true",
            "false",
            "null",
            "123",
            "-5",
            "1e5",
            "0.5",
            "01",
            "+1",
            "-",
            "- x",
            "[a]",
            "{a}",
            "a{b",
            "x]",
            "#c",
            "héllo wörld",
            "日本語",
            "emoji \u{1F600}",
            "\u{2028}",
            "a  b",
            "Infinity",
            "NaN",
        ];
        for s in specials {
            rt(&json!({"k": s}));
            rt(&json!([s, s]));
            rt(&json!([{"a": s, "b": s}, {"a": s, "b": 1}]));
            rt(&json!({s: 1, "t": [s]}));
            rt(&json!([[s]]));
        }
        let t = encode(&json!({"a":"123","b":"true","c":"","d":"x,y"}));
        assert_eq!(t, "a: \"123\"\nb: \"true\"\nc: \"\"\nd: \"x,y\"");
    }

    #[test]
    fn empties_round_trip() {
        rt(&json!({"a":{},"b":[],"c":[{}],"d":[[]],"e":{"f":{}}}));
        rt(&json!([{}, {}]));
        rt(&json!({"a":"","b":[""]}));
        assert_eq!(decode("").unwrap(), json!({}));
        assert!(encode_if_better(&json!({}), 100).is_none());
        assert!(encode_if_better(&json!([]), 100).is_none());
    }

    #[test]
    fn numbers_are_exact() {
        rt(&json!({
            "i": 9007199254740993u64, "max": u64::MAX, "min": i64::MIN, "neg": -1,
            "f": 0.1, "g": 1.5e300, "h": -2.5e-10, "z": 0.0, "nz": -0.0, "one": 1.0, "big": 1e21,
            "arr": [1, 1.0, 2.5, u64::MAX]
        }));
        let v: Value = serde_json::from_str(r#"{"a":18446744073709551616,"b":1E2}"#).unwrap();
        rt(&v);
        // 1 and 1.0 stay distinct.
        let t = encode(&json!({"a":1,"b":1.0}));
        let back = decode(&t).unwrap();
        assert!(back["a"].is_u64() && back["b"].is_f64());
    }

    #[test]
    fn decoder_rejects_garbage() {
        for bad in [
            "a",
            "a: 1\n b: 2",
            "k[2]: 1",
            "k[1]{a}:\n  1,2",
            "[1]:\n  x",
            "a: 1,2",
            "a:1",
        ] {
            assert!(decode(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn saving_threshold() {
        let big: Vec<Value> = (0..20)
            .map(|i| json!({"id":i,"name":format!("n{i}"),"ok":true}))
            .collect();
        let v = Value::Array(big);
        let pretty = serde_json::to_string_pretty(&v).unwrap();
        let t = encode_if_better(&v, pretty.len()).unwrap();
        assert!(t.len() * 5 <= pretty.len() * 4);
        // Same data against an unrealistic original: not 20% smaller.
        assert!(encode_if_better(&v, t.len() + 1).is_none());
        // Quoted strings make this larger or barely smaller.
        let q = json!({"a":["1","2"]});
        assert!(encode_if_better(&q, q.to_string().len()).is_none());
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn gen_str(r: &mut Rng) -> String {
        const POOL: &[&str] = &[
            "a",
            "b",
            "z",
            "0",
            "9",
            "-",
            ",",
            ":",
            " ",
            "\"",
            "\\",
            "\n",
            "\t",
            "[",
            "]",
            "{",
            "}",
            "é",
            "日",
            "\u{1F600}",
            ".",
            "e",
            "true",
            "null",
            "_",
            "\u{0}",
        ];
        (0..r.below(6))
            .map(|_| POOL[r.below(POOL.len() as u64) as usize])
            .collect()
    }

    fn gen(r: &mut Rng, depth: u32) -> Value {
        let top = if depth == 0 { 6 } else { 9 };
        match r.below(top) {
            0 => Value::Null,
            1 => json!(r.below(2) == 0),
            2 => json!(r.next() as i64),
            3 => json!((r.next() % 100_000) as f64 / 64.0),
            4 | 5 => Value::String(gen_str(r)),
            6 | 7 => {
                // Often a uniform table to exercise the tabular path.
                let n = r.below(4);
                let keys: Vec<String> = (0..1 + r.below(3)).map(|_| gen_str(r)).collect();
                let uniform = r.below(2) == 0;
                Value::Array(
                    (0..n)
                        .map(|_| {
                            if uniform {
                                Value::Object(keys.iter().map(|k| (k.clone(), gen(r, 0))).collect())
                            } else {
                                gen(r, depth - 1)
                            }
                        })
                        .collect(),
                )
            }
            _ => Value::Object(
                (0..r.below(4))
                    .map(|_| (gen_str(r), gen(r, depth - 1)))
                    .collect(),
            ),
        }
    }

    #[test]
    fn seeded_random_round_trips() {
        let mut r = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..3000 {
            let v = gen(&mut r, 4);
            if matches!(v, Value::Object(_) | Value::Array(_)) {
                rt(&v);
            }
        }
    }
}
