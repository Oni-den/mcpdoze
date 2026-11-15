// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal tool namespacing: `<prefix>__<tool>`.
//!
//! Temporary local version for the data path. The full helper (sanitising,
//! 64-character cap with hash suffix) belongs to the configuration work and
//! can replace this module; the router only uses [`join`], [`split`] and
//! [`valid_prefix`].

/// Separator between prefix and tool name.
pub const SEP: &str = "__";
/// Longest accepted prefix.
pub const MAX_PREFIX: usize = 32;

/// A prefix is 1..=32 chars of `[a-z0-9_]`, without `__`, so that
/// [`split`] is unambiguous.
pub fn valid_prefix(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= MAX_PREFIX
        && !p.contains(SEP)
        && p.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The agent-visible name of `tool` on the server with `prefix`.
pub fn join(prefix: &str, tool: &str) -> String {
    format!("{prefix}{SEP}{tool}")
}

/// Splits an agent-visible name into `(prefix, tool)`.
pub fn split(name: &str) -> Option<(&str, &str)> {
    name.split_once(SEP)
        .filter(|(p, t)| !p.is_empty() && !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        assert_eq!(split(&join("ex_1", "do__it")), Some(("ex_1", "do__it")));
        assert_eq!(split("noprefix"), None);
        assert_eq!(split("__x"), None);
        assert_eq!(split("x__"), None);
    }

    #[test]
    fn prefixes() {
        assert!(valid_prefix("example_2"));
        for bad in ["", "Ex", "a__b", "a-b", &"a".repeat(33)] {
            assert!(!valid_prefix(bad), "{bad}");
        }
    }
}
