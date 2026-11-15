// SPDX-License-Identifier: GPL-3.0-or-later

//! Reversible resource URI mapping.
//!
//! A backend resource URI `<uri>` on the server with prefix `<prefix>` is
//! exposed as `mcpdoze://<prefix>/<enc>`, where `<enc>` is `<uri>` with every
//! byte outside the RFC 3986 unreserved set (`A-Z a-z 0-9 - . _ ~`) written
//! as `%XX` (upper-case hex). The whole original URI is one opaque path
//! segment, so the mapping is unambiguous and survives clients that treat
//! URIs as strings.
//!
//! Resource templates (RFC 6570) are mapped the same way, except that
//! `{...}` expressions are kept verbatim so clients can still expand them.
//! The reverse mapping percent-decodes the segment once; a value expanded
//! into a template therefore reaches the backend decoded once (for example
//! `{path}` with `a/b` arrives as `a/b`, not `a%2Fb`).

/// Scheme and authority marker of mapped URIs.
pub const SCHEME: &str = "mcpdoze://";

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn encode_into(out: &mut String, s: &str) {
    for b in s.bytes() {
        if unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

/// The agent-visible URI of backend resource `uri`.
pub fn map_uri(prefix: &str, uri: &str) -> String {
    let mut out = format!("{SCHEME}{prefix}/");
    encode_into(&mut out, uri);
    out
}

/// The agent-visible form of resource template `template`.
pub fn map_template(prefix: &str, template: &str) -> String {
    let mut out = format!("{SCHEME}{prefix}/");
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        encode_into(&mut out, &rest[..open]);
        out.push_str(&rest[open..open + close + 1]);
        rest = &rest[open + close + 1..];
    }
    encode_into(&mut out, rest);
    out
}

/// Splits an agent-visible URI into `(prefix, original URI)`. `None` if it
/// is not a mapped URI or its encoding is invalid.
pub fn unmap(mapped: &str) -> Option<(&str, String)> {
    let rest = mapped.strip_prefix(SCHEME)?;
    let (prefix, enc) = rest.split_once('/')?;
    if prefix.is_empty() || enc.is_empty() {
        return None;
    }
    let bytes = enc.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = enc.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some((prefix, String::from_utf8(out).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for uri in [
            "file:///home/example/a b.txt",
            "https://example.com/x?q=1#f",
            "mock://readme",
            "custom:äö/%41",
        ] {
            let m = map_uri("ex", uri);
            assert!(m.starts_with("mcpdoze://ex/"), "{m}");
            assert!(!m["mcpdoze://ex/".len()..].contains('/'), "{m}");
            assert_eq!(unmap(&m), Some(("ex", uri.to_owned())));
        }
        assert_eq!(map_uri("p", "a:b/c"), "mcpdoze://p/a%3Ab%2Fc");
    }

    #[test]
    fn templates_keep_expressions() {
        let t = map_template("p", "mock://items/{id}{?q}");
        assert_eq!(t, "mcpdoze://p/mock%3A%2F%2Fitems%2F{id}{?q}");
        let expanded = t.replace("{id}", "42").replace("{?q}", "");
        assert_eq!(unmap(&expanded).unwrap().1, "mock://items/42");
    }

    #[test]
    fn rejects_foreign_or_broken() {
        assert_eq!(unmap("file:///x"), None);
        assert_eq!(unmap("mcpdoze://p"), None);
        assert_eq!(unmap("mcpdoze://p/"), None);
        assert_eq!(unmap("mcpdoze://p/%4"), None);
        assert_eq!(unmap("mcpdoze://p/%zz"), None);
    }
}
