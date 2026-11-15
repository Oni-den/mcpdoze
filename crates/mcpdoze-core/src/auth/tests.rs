// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

fn tok() -> Token {
    Token::from_string("ab".repeat(32)).unwrap()
}

#[test]
fn token_is_redacted() {
    let t = tok();
    let secret = t.expose_secret().to_string();
    assert!(!format!("{t:?}").contains(&secret));
    assert!(!format!("{t}").contains(&secret));
    assert!(!format!("{t:#?}").contains(&secret));
    assert!(!t.to_string().contains(&secret));
    // nested in another Debug type
    #[derive(Debug)]
    #[allow(dead_code)]
    struct Wrap(Token);
    assert!(!format!("{:?}", Wrap(t)).contains(&secret));
}

#[test]
fn generate_and_create_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("sub").join("token");
    let a = load_or_create_token(&p).unwrap();
    assert_eq!(a.expose_secret().len(), 64);
    assert!(a.expose_secret().bytes().all(|b| b.is_ascii_hexdigit()));
    let b = load_or_create_token(&p).unwrap();
    assert_eq!(a.expose_secret(), b.expose_secret());
    let c = Token::generate().unwrap();
    assert_ne!(a.expose_secret(), c.expose_secret());
}

#[test]
fn invalid_token_file_errors() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("token");
    std::fs::write(&p, "not a token").unwrap();
    let e = load_or_create_token(&p).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    assert!(!e.to_string().contains("not a token"));
}

#[cfg(unix)]
#[test]
fn unix_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("token");
    load_or_create_token(&p).unwrap();
    let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(warn_if_permissive(&p).is_none());
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(warn_if_permissive(&p).is_some());
}

#[cfg(not(unix))]
#[test]
fn non_unix_never_warns() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("token");
    load_or_create_token(&p).unwrap();
    assert!(warn_if_permissive(&p).is_none());
}

#[test]
fn from_string_validation() {
    assert!(Token::from_string(String::new()).is_none());
    assert!(Token::from_string("  ".into()).is_none());
    assert!(Token::from_string("a b".into()).is_none());
    assert_eq!(
        Token::from_string(" abc\n".into()).unwrap().expose_secret(),
        "abc"
    );
}

#[test]
fn bearer_verification() {
    let t = tok();
    let good = format!("Bearer {}", t.expose_secret());
    assert!(verify_bearer(&good, &t));
    assert!(verify_bearer(&format!("bearer {}", t.expose_secret()), &t));
    assert!(verify_bearer(
        &format!("  BEARER   {}  ", t.expose_secret()),
        &t
    ));
    assert!(!verify_bearer(t.expose_secret(), &t), "no scheme");
    assert!(!verify_bearer("Basic abc", &t));
    assert!(!verify_bearer("Bearer ", &t));
    assert!(!verify_bearer("", &t));
    assert!(!verify_bearer("Bearer wrong", &t));
    let mut off = t.expose_secret().to_string();
    off.pop();
    assert!(!verify_bearer(&format!("Bearer {off}"), &t), "prefix");
    assert!(!verify_bearer(
        &format!("Bearer {}x", t.expose_secret()),
        &t
    ));
}

#[test]
fn host_accepts_loopback() {
    for h in [
        "localhost",
        "localhost:7878",
        "LOCALHOST:80",
        "127.0.0.1",
        "127.0.0.1:7878",
        "[::1]",
        "[::1]:7878",
        "LocalHost",
    ] {
        assert!(check_host(Some(h), &[]), "{h}");
    }
}

#[test]
fn host_rejects_rebinding_and_garbage() {
    assert!(!check_host(None, &[]), "missing host");
    for h in [
        "",
        "evil.com",
        "evil.com:7878",
        "localhost.evil.com",
        "localhost.evil.com:80",
        "127.0.0.1.evil.com",
        "127.0.0.1.evil.com:80",
        "evil.com.localhost.evil.com",
        "localhost@example.com",
        "example.com@localhost",
        "user:pw@localhost",
        "localhost:80@example.com",
        "localhost/",
        "localhost:",
        "localhost:abc",
        "localhost:99999",
        "localhost:80:80",
        "127.0.0.2",
        "0.0.0.0",
        "[::2]",
        "[::1",
        "::1",
        "[::1]x",
        "[::1]:",
        "localhost ",
        " localhost x",
        "local\thost",
        "localhost\\evil.com",
        "localhost.",
        "[]",
        ":80",
    ] {
        assert!(!check_host(Some(h), &[]), "{h:?}");
    }
}

#[test]
fn host_extra_allowed() {
    assert!(check_host(Some("broker.example:7878"), &["broker.example"]));
    assert!(check_host(Some("BROKER.example"), &["Broker.Example"]));
    assert!(!check_host(Some("other.example"), &["broker.example"]));
    assert!(!check_host(
        Some("broker.example.evil.com"),
        &["broker.example"]
    ));
}

#[test]
fn origin_rules() {
    assert!(check_origin(None, &[]), "missing origin allowed");
    for o in [
        "http://localhost",
        "http://localhost:3000",
        "https://localhost:3000",
        "http://127.0.0.1:8080",
        "http://[::1]:8080",
        "HTTP://LOCALHOST:1",
        "https://127.0.0.1",
    ] {
        assert!(check_origin(Some(o), &[]), "{o}");
    }
    for o in [
        "",
        "null",
        "http://evil.com",
        "http://localhost.evil.com",
        "http://127.0.0.1.evil.com:80",
        "http://localhost@example.com",
        "http://example.com@localhost",
        "https://localhost:3000/path",
        "http://localhost/",
        "ftp://localhost",
        "file://",
        "localhost",
        "//localhost",
        "http://",
        "http://[::1",
        "chrome-extension://abc",
        "http://localhost:bad",
    ] {
        assert!(!check_origin(Some(o), &[]), "{o:?}");
    }
    assert!(check_origin(Some("https://app.example"), &["app.example"]));
    assert!(!check_origin(
        Some("https://app.example.evil.com"),
        &["app.example"]
    ));
}
