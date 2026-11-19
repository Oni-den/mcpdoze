// SPDX-License-Identifier: GPL-3.0-or-later
//! Bearer token handling and Host/Origin validation (DNS-rebinding defence).
//!
//! All checks are pure functions over header values so they can be called from
//! any HTTP framework's middleware.
//!
//! # Token file permissions
//!
//! On Unix the token file is created with mode 0600. On Windows this module
//! uses only `std` and does not edit ACLs: files created under the user
//! profile directory (where the default data dir lives) inherit an ACL that
//! already restricts access to the owner, SYSTEM and administrators. Do not
//! point `token_file` at a shared directory on Windows.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;

use subtle::ConstantTimeEq;

const TOKEN_BYTES: usize = 32;
const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;

/// A bearer token. `Debug` and `Display` redact the value.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Generate a fresh random token (32 bytes, hex).
    pub fn generate() -> io::Result<Token> {
        let mut buf = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut buf).map_err(|e| io::Error::other(e.to_string()))?;
        let mut s = String::with_capacity(TOKEN_HEX_LEN);
        for b in buf {
            s.push(char::from(HEX[usize::from(b >> 4)]));
            s.push(char::from(HEX[usize::from(b & 0xf)]));
        }
        Ok(Token(s))
    }

    /// Wrap an existing token string (e.g. from `token_env`). Rejects empty
    /// or whitespace/control-containing values.
    pub fn from_string(s: String) -> Option<Token> {
        let t = s.trim();
        if t.is_empty() || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
            None
        } else {
            Some(Token(t.to_string()))
        }
    }

    /// The raw secret. Only for deliberate output (e.g. `mcpdoze token`) or
    /// sending to a client; never log it.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token([redacted])")
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Read the token at `path`, creating it (owner-only) if it does not exist.
pub fn load_or_create_token(path: &Path) -> io::Result<Token> {
    match read_token(path) {
        Ok(t) => return Ok(t),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let token = Token::generate()?;
    match create_private(path) {
        Ok(mut f) => {
            f.write_all(token.0.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            Ok(token)
        }
        // Lost a creation race: use the winner's token.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => read_token(path),
        Err(e) => Err(e),
    }
}

fn read_token(path: &Path) -> io::Result<Token> {
    let f = std::fs::File::open(path)?;
    let mut s = String::new();
    f.take(1024).read_to_string(&mut s)?;
    let t = s.trim();
    if t.len() == TOKEN_HEX_LEN && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(Token(t.to_ascii_lowercase()))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "token file does not contain a 64-character hex token",
        ))
    }
}

/// `OpenOptions` for a new owner-only file: mode 0600 on Unix. On Windows
/// the file inherits the ACL of its directory (see the module docs). Callers
/// add `create_new`, `append`, ... themselves.
pub fn private_options() -> std::fs::OpenOptions {
    let mut o = std::fs::OpenOptions::new();
    o.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o
}

/// Creates `path` (new, owner-only) and fails if it exists.
pub fn create_private(path: &Path) -> io::Result<std::fs::File> {
    private_options().create_new(true).open(path)
}

/// Creates `dir` and its parents; a directory this call creates is mode 0700
/// on Unix (an existing one is left alone).
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    let existed = dir.is_dir();
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = existed;
    Ok(())
}

/// Returns a warning message if `path` is readable by group or others.
/// Always `None` on non-Unix platforms (see module docs).
pub fn warn_if_permissive(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Some(format!(
                "{} is accessible by group/others (mode {mode:o}); run chmod 600",
                path.display()
            ));
        }
        None
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Warnings about a directory and the regular files directly in it that
/// pre-date this process (a debug directory created earlier or by another
/// tool). Unix: group/other access bits. Elsewhere: one note that access
/// follows the inherited ACL (see the module docs).
pub fn permissions_warnings(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut check = |p: &Path, want: &str| {
            if let Ok(m) = std::fs::metadata(p) {
                let mode = m.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    out.push(format!(
                        "{} is accessible by group/others (mode {mode:o}); run chmod {want}",
                        p.display()
                    ));
                }
            }
        };
        check(dir, "700");
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_file()) {
                    check(&e.path(), "600");
                }
            }
        }
    }
    #[cfg(not(unix))]
    if dir.is_dir() {
        out.push(format!(
            "{} gets its access rules from the parent folder's ACL; keep it out of shared or synced folders",
            dir.display()
        ));
    }
    out
}

/// Validate an `Authorization` header value (`Bearer <token>`, scheme
/// case-insensitive) against `token` in constant time.
pub fn verify_bearer(header_value: &str, token: &Token) -> bool {
    let v = header_value.trim();
    let Some((scheme, rest)) = v.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    let presented = rest.trim();
    bool::from(presented.as_bytes().ct_eq(token.0.as_bytes()))
}

/// Parsed `host[:port]` authority.
fn parse_authority(a: &str) -> Option<String> {
    if a.is_empty()
        || a.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '@' | '/' | '\\' | '?' | '#')
        })
    {
        return None;
    }
    let (host, port) = if let Some(rest) = a.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &a[..end + 2];
        let tail = &rest[end + 1..];
        let port = match tail {
            "" => None,
            t => Some(t.strip_prefix(':')?),
        };
        (host, port)
    } else {
        match a.split_once(':') {
            None => (a, None),
            Some((h, p)) => {
                if p.contains(':') {
                    return None;
                }
                (h, Some(p))
            }
        }
    };
    if let Some(p) = port {
        if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        p.parse::<u16>().ok()?;
    }
    if host.is_empty() || host == "[]" {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

fn host_allowed(host: &str, allowed: &[&str]) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
        || allowed.iter().any(|a| a.eq_ignore_ascii_case(host))
}

/// Validate a `Host` header. A missing header is rejected. `allowed` lists
/// extra host names (no port) accepted besides loopback names.
pub fn check_host(host_header: Option<&str>, allowed: &[&str]) -> bool {
    match host_header.and_then(parse_authority) {
        Some(h) => host_allowed(&h, allowed),
        None => false,
    }
}

/// Validate an `Origin` header. A missing header is allowed (non-browser
/// clients); `null`, empty, non-http(s) or unparsable origins are rejected.
pub fn check_origin(origin_header: Option<&str>, allowed: &[&str]) -> bool {
    let Some(o) = origin_header else {
        return true;
    };
    let o = o.trim();
    let Some(rest) = o
        .strip_prefix("http://")
        .or_else(|| o.strip_prefix("https://"))
        .or_else(|| {
            // case-insensitive scheme
            let l = o.to_ascii_lowercase();
            if l.starts_with("http://") {
                Some(&o[7..])
            } else if l.starts_with("https://") {
                Some(&o[8..])
            } else {
                None
            }
        })
    else {
        return false;
    };
    match parse_authority(rest) {
        Some(h) => host_allowed(&h, allowed),
        None => false,
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn warns_about_a_preexisting_directory() {
        let d = tempfile::tempdir().unwrap();
        let w = permissions_warnings(d.path());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            let f = d.path().join("failures.jsonl");
            std::fs::write(&f, b"x").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
            let w = permissions_warnings(d.path());
            assert_eq!(w.len(), 2, "{w:?}");
        }
        #[cfg(not(unix))]
        assert_eq!(w.len(), 1, "{w:?}");
        #[cfg(unix)]
        let _ = w;
    }
}
