// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

fn cfg(text: &str) -> Config {
    Config::parse(text).expect("parse")
}

fn errs(text: &str) -> Vec<ValidationError> {
    cfg(text).validate().expect_err("should be invalid")
}

fn has(e: &[ValidationError], field: &str) -> bool {
    e.iter().any(|x| x.field == field)
}

const GOOD: &str = r#"
[broker]
bind = "127.0.0.1:7878"
token_file = "token"
default_idle_ttl = "10m"

[servers.example]
transport = "stdio"
command = "npx"
args = ["-y", "some-mcp-server"]
env = { API_KEY = { from_env = "EXAMPLE_API_KEY" }, MODE = "fast" }
scope = "per-cwd"
idle_ttl = "5m"
prefix = "example"

[servers.remote]
transport = "http"
url = "https://example.com/mcp"
headers = { Authorization = { from_env = "EXAMPLE_AUTH" } }
"#;

#[test]
fn parses_and_validates_good_config() {
    let c = cfg(GOOD);
    c.validate().unwrap();
    let s = &c.servers["example"];
    assert_eq!(s.scope, Scope::PerCwd);
    assert_eq!(s.idle_ttl, Some(Duration::from_secs(300)));
    assert!(s.enabled);
    assert_eq!(c.servers["remote"].scope, Scope::Shared);
    assert_eq!(c.servers["remote"].effective_prefix("remote"), "remote");
}

#[test]
fn defaults() {
    let c = cfg("");
    assert_eq!(c.broker.bind, "127.0.0.1:7878");
    assert_eq!(c.broker.default_idle_ttl, Duration::from_secs(600));
    assert_eq!(c.broker.start_timeout, Duration::from_secs(60));
    assert!(c.broker.log_dir.is_none());
    c.validate().unwrap();
}

#[test]
fn unknown_fields_rejected() {
    assert!(Config::parse("[broker]\nbogus = 1").is_err());
    assert!(Config::parse("[servers.a]\ntransport='stdio'\ncommand='x'\nbogus=1").is_err());
}

#[test]
fn load_from_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.toml");
    std::fs::write(&p, GOOD).unwrap();
    assert_eq!(Config::load(&p).unwrap().servers.len(), 2);
    assert!(matches!(
        Config::load(&dir.path().join("missing.toml")),
        Err(ConfigError::Io(_))
    ));
}

#[test]
fn duration_parsing() {
    assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
    assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
    assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
    assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
    assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172_800));
    for bad in ["", "m", "5", "5x", "-5s", "5 s s", "99999999999999999999s"] {
        assert!(parse_duration(bad).is_err(), "{bad}");
    }
    assert!(parse_duration("18446744073709551615h").is_err());
}

#[test]
fn bad_duration_in_config() {
    assert!(Config::parse("[broker]\ndefault_idle_ttl = \"soon\"").is_err());
}

#[test]
fn duplicate_prefixes_reported() {
    let e = errs(
        r#"
[servers.a]
transport = "stdio"
command = "x"
prefix = "same"
[servers.b]
transport = "stdio"
command = "x"
prefix = "same"
[servers.My-Tool]
transport = "stdio"
command = "x"
[servers.my_tool]
transport = "stdio"
command = "x"
"#,
    );
    assert!(has(&e, "servers.b.prefix"));
    assert!(has(&e, "servers.my_tool.prefix"));
    assert!(!has(&e, "servers.a.prefix"));
}

#[test]
fn prefix_rules() {
    assert_eq!(sanitize_prefix("My-Tool.v2"), "my_tool_v2");
    assert_eq!(sanitize_prefix("--a__b--"), "a_b");
    assert_eq!(sanitize_prefix("***"), "");
    assert!(sanitize_prefix(&"a".repeat(100)).len() <= MAX_PREFIX_LEN);
    let e = errs(
        r#"
[servers.a]
transport = "stdio"
command = "x"
prefix = "bad__one"
[servers.b]
transport = "stdio"
command = "x"
prefix = "Upper"
[servers.c]
transport = "stdio"
command = "x"
prefix = "_lead"
[servers."***"]
transport = "stdio"
command = "x"
"#,
    );
    assert!(has(&e, "servers.a.prefix"));
    assert!(has(&e, "servers.b.prefix"));
    assert!(has(&e, "servers.c.prefix"));
    assert!(has(&e, "servers.***"));
}

#[test]
fn transport_requirements_and_all_errors_collected() {
    let e = errs(
        r#"
[servers.a]
transport = "stdio"
[servers.b]
transport = "http"
[servers.c]
transport = "http"
url = "ftp://example.com"
[servers.d]
transport = "http"
url = "http://"
[servers.e]
transport = "stdio"
command = "x"
idle_ttl = "0s"
"#,
    );
    assert!(has(&e, "servers.a.command"));
    assert!(has(&e, "servers.b.url"));
    assert!(has(&e, "servers.c.url"));
    assert!(has(&e, "servers.d.url"));
    assert!(has(&e, "servers.e.idle_ttl"));
    assert!(e.len() >= 5);
}

#[test]
fn http_accepts_plain_and_tls() {
    assert!(valid_http_url("http://127.0.0.1:9/mcp"));
    assert!(valid_http_url("https://example.com"));
    assert!(!valid_http_url("https:///path"));
    assert!(!valid_http_url("https://exa mple.com"));
    assert!(!valid_http_url("example.com"));
}

#[test]
fn bind_rules() {
    let e = errs("[broker]\nbind = \"0.0.0.0:7878\"");
    assert!(has(&e, "broker.allow_remote"));
    assert!(has(&e, "broker.token_file"));
    cfg("[broker]\nbind = \"0.0.0.0:7878\"\nallow_remote = true\ntoken_env = \"EXAMPLE_TOKEN\"")
        .validate()
        .unwrap();
    // allow_remote alone is not enough
    let e = errs("[broker]\nbind = \"[::]:1\"\nallow_remote = true");
    assert!(has(&e, "broker.token_file") && !has(&e, "broker.allow_remote"));
    // token alone is not enough
    let e = errs("[broker]\nbind = \"192.0.2.1:1\"\ntoken_file = \"t\"");
    assert!(has(&e, "broker.allow_remote"));
    // loopback forms
    for b in ["127.0.0.1:1", "[::1]:1", "localhost:80"] {
        cfg(&format!("[broker]\nbind = \"{b}\""))
            .validate()
            .unwrap();
    }
    assert!(has(&errs("[broker]\nbind = \"nonsense\""), "broker.bind"));
    assert!(has(
        &errs("[broker]\nbind = \"evil.example:80\""),
        "broker.bind"
    ));
    assert!(has(
        &errs("[broker]\ntoken_file=\"a\"\ntoken_env=\"B\""),
        "broker.token_env"
    ));
}

#[test]
fn secret_detection() {
    assert!(secret_like("API_KEY", "x", false).is_some());
    assert!(secret_like("github_token", "x", false).is_some());
    assert!(secret_like("DB_PASSWORD", "x", false).is_some());
    assert!(secret_like("CLIENT_SECRET", "x", false).is_some());
    assert!(secret_like("API_KEY", "", false).is_none());
    assert!(secret_like("NAME", "sk-abcdef", false).is_some());
    assert!(secret_like("NAME", "ghp_abcdef", false).is_some());
    assert!(secret_like("NAME", "github_pat_abc", false).is_some());
    assert!(secret_like("NAME", "xoxb-1-2", false).is_some());
    assert!(secret_like("NAME", "AKIAABCDEFGH", false).is_some());
    assert!(secret_like("NAME", "eyJhbGci.eyJzdWIi.sig", false).is_some());
    assert!(secret_like("NAME", "-----BEGIN RSA PRIVATE KEY-----", false).is_some());
    assert!(secret_like("NAME", "fast", false).is_none());
    assert!(secret_like("LOG_LEVEL", "debug", false).is_none());
    assert!(secret_like("Authorization", "x", true).is_some());
    assert!(secret_like("Authorization", "x", false).is_none());
    assert!(secret_like("X-Trace", "Bearer abc", true).is_some());
}

#[test]
fn literal_secret_env_rejected_unless_allowed() {
    let base = r#"
[servers.a]
transport = "stdio"
command = "x"
env = { MY_TOKEN = "abc", PLAIN = "ok", OTHER = "sk-live-1" }
"#;
    let e = errs(base);
    assert!(has(&e, "servers.a.env.MY_TOKEN"));
    assert!(has(&e, "servers.a.env.OTHER"));
    assert!(!has(&e, "servers.a.env.PLAIN"));
    // error text never includes the value
    assert!(e.iter().all(|x| !x.message.contains("sk-live-1")));
    cfg(&format!("{base}allow_literal_env = true"))
        .validate()
        .unwrap();
    // from_env never flagged
    cfg("[servers.a]\ntransport='stdio'\ncommand='x'\nenv={ MY_TOKEN = { from_env = \"EXAMPLE\" } }")
        .validate()
        .unwrap();
}

#[test]
fn literal_secret_header_rejected() {
    let e = errs(
        "[servers.a]\ntransport='http'\nurl='https://example.com'\nheaders={ Authorization = \"Bearer abc\" }",
    );
    assert!(has(&e, "servers.a.headers.Authorization"));
}

#[test]
fn env_value_debug_hides_literals() {
    let c = cfg("[servers.a]\ntransport='stdio'\ncommand='x'\nallow_literal_env=true\nenv={ MY_TOKEN = \"hunter2hunter2\" }");
    assert!(!format!("{c:?}").contains("hunter2"));
}

#[test]
fn resolve_env_from_lookup() {
    let c = cfg(GOOD);
    let s = &c.servers["example"];
    let ok = resolve_map(&s.env, |v| {
        (v == "EXAMPLE_API_KEY").then(|| "s3cret-value".into())
    })
    .unwrap();
    assert_eq!(ok["API_KEY"], "s3cret-value");
    assert_eq!(ok["MODE"], "fast");
    let err = resolve_map(&s.env, |_| None).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("EXAMPLE_API_KEY") && msg.contains("API_KEY"));
    assert!(matches!(err, ResolveError::Missing { .. }));
}

#[test]
fn resolve_env_process_environment() {
    // Variable intentionally unset; unique name avoids touching real env.
    let c = cfg("[servers.a]\ntransport='stdio'\ncommand='x'\nenv={ K = { from_env = \"MCPDOZE_TEST_SURELY_UNSET_VAR_X1\" } }");
    let e = resolve_env(&c.servers["a"]).unwrap_err();
    assert!(e.to_string().contains("MCPDOZE_TEST_SURELY_UNSET_VAR_X1"));
    assert!(c.servers["a"].resolve_env().is_err());
}

#[test]
fn config_path_override() {
    assert_eq!(
        config_path_from(Some("custom.toml".into())),
        Some(PathBuf::from("custom.toml"))
    );
    if let Some(p) = config_path_from(None) {
        assert!(p.ends_with("config.toml"));
    }
    assert!(default_data_dir().is_none_or(|p| p.to_string_lossy().contains("mcpdoze")));
}

#[test]
fn namespacing_basic() {
    assert_eq!(namespaced_tool_name("fs", "read_file"), "fs__read_file");
    assert_eq!(split_namespaced("fs__read_file"), Some(("fs", "read_file")));
    assert_eq!(split_namespaced("fs__a__b"), Some(("fs", "a__b")));
    assert_eq!(split_namespaced("nosep"), None);
    assert_eq!(split_namespaced("__x"), None);
    assert_eq!(split_namespaced("fs__"), None);
}

#[test]
fn namespacing_truncation_and_roundtrip() {
    let long_a = format!("{}A", "t".repeat(100));
    let long_b = format!("{}B", "t".repeat(100));
    let a = namespaced_tool_name("srv", &long_a);
    let b = namespaced_tool_name("srv", &long_b);
    assert_eq!(a.len(), MAX_TOOL_NAME_LEN);
    assert_eq!(b.len(), MAX_TOOL_NAME_LEN);
    assert_ne!(a, b, "same stem, different hash");
    assert_eq!(a, namespaced_tool_name("srv", &long_a), "stable");
    assert_eq!(split_namespaced(&a).unwrap().0, "srv");

    let mut m = NameMap::new();
    assert_eq!(m.insert("srv", &long_a).unwrap(), a);
    assert_eq!(m.insert("srv", &long_b).unwrap(), b);
    assert_eq!(m.insert("srv", &long_a).unwrap(), a, "idempotent");
    m.insert("srv", "short").unwrap();
    assert_eq!(m.resolve(&a), Some(("srv", long_a.as_str())));
    assert_eq!(m.resolve(&b), Some(("srv", long_b.as_str())));
    assert_eq!(m.resolve("srv__short"), Some(("srv", "short")));
    assert_eq!(m.resolve("srv__nope"), None);
    assert_eq!(m.len(), 3);
    m.remove_prefix("srv");
    assert!(m.is_empty());
}

#[test]
fn truncation_boundaries_and_unicode() {
    let exact = "x".repeat(MAX_TOOL_NAME_LEN - "ab__".len());
    assert_eq!(namespaced_tool_name("ab", &exact).len(), MAX_TOOL_NAME_LEN);
    assert_eq!(namespaced_tool_name("ab", &exact), format!("ab__{exact}"));
    let over = format!("{exact}y");
    assert_ne!(namespaced_tool_name("ab", &over), format!("ab__{over}"));
    let uni = "é".repeat(80);
    let n = namespaced_tool_name("srv", &uni);
    assert!(n.len() <= MAX_TOOL_NAME_LEN);
    // maximal prefix still fits
    let p = "p".repeat(MAX_PREFIX_LEN);
    assert!(namespaced_tool_name(&p, &"z".repeat(200)).len() <= MAX_TOOL_NAME_LEN);
}

#[test]
fn name_collision_detected() {
    let mut m = NameMap::new();
    m.insert("srv", "tool").unwrap();
    // forge a collision directly
    m.map
        .insert("srv__other".into(), ("srv".into(), "different".into()));
    let err = m.insert("srv", "other").unwrap_err();
    assert_eq!(err.exposed, "srv__other");
}

#[test]
fn prefixes_table() {
    let c = cfg(GOOD);
    let p = c.prefixes();
    assert_eq!(p["example"], "example");
    assert_eq!(p["remote"], "remote");
}
