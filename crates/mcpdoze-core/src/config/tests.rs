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

#[test]
fn server_config_round_trips_through_toml_and_json() {
    let c = cfg(GOOD);
    let s = &c.servers["example"];
    let text = toml::to_string(s).expect("toml");
    assert!(text.contains("idle_ttl = \"5m\""), "{text}");
    assert!(!text.contains("enabled"), "defaults are omitted: {text}");
    let back: ServerConfig = toml::from_str(&text).expect("re-parse");
    assert_eq!(&back, s);
    let json = serde_json::to_string(s).expect("json");
    let back: ServerConfig = serde_json::from_str(&json).expect("json re-parse");
    assert_eq!(&back, s);
}

#[test]
fn durations_format_and_reparse() {
    for (d, s) in [
        (Duration::from_millis(250), "250ms"),
        (Duration::from_secs(30), "30s"),
        (Duration::from_secs(300), "5m"),
        (Duration::from_secs(7200), "2h"),
        (Duration::from_secs(90), "90s"),
        (Duration::ZERO, "0s"),
    ] {
        assert_eq!(format_duration(d), s);
        assert_eq!(parse_duration(s).unwrap(), d);
    }
}

#[test]
fn allowed_hosts_are_validated() {
    let ok = cfg("[broker]\nallowed_hosts = [\"broker.example.com\", \"[fd00::1]\"]\n");
    assert!(ok.validate().is_ok());
    for bad in [
        "\"\"",
        "\"a.example.com:80\"",
        "\"*.example.com\"",
        "\"a b\"",
    ] {
        let e = errs(&format!("[broker]\nallowed_hosts = [{bad}]\n"));
        assert!(has(&e, "broker.allowed_hosts[0]"), "{bad}: {e:?}");
    }
}

#[test]
fn max_instances_is_validated() {
    for (n, ok) in [(0, false), (1, true), (8, true), (257, false)] {
        let c = Config::parse(&format!(
            "[servers.a]
transport = \"stdio\"
command = \"prog\"
scope = \"per-session\"
max_instances = {n}
"
        ))
        .unwrap();
        assert_eq!(c.validate().is_ok(), ok, "{n}");
        assert_eq!(c.servers["a"].max_instances_or_default(), n);
    }
    let c = Config::parse(
        "[servers.a]
transport = \"stdio\"
command = \"prog\"
",
    )
    .unwrap();
    assert_eq!(
        c.servers["a"].max_instances_or_default(),
        DEFAULT_MAX_INSTANCES
    );
    assert_eq!(
        c.broker.client_request_timeout,
        std::time::Duration::from_secs(120)
    );
    assert_eq!(
        c.broker.session_idle_timeout,
        std::time::Duration::from_secs(1800)
    );
    let c = Config::parse(
        "[broker]
client_request_timeout = \"5s\"
session_idle_timeout = \"1m\"
",
    )
    .unwrap();
    assert_eq!(
        c.broker.client_request_timeout,
        std::time::Duration::from_secs(5)
    );
    assert_eq!(
        c.broker.session_idle_timeout,
        std::time::Duration::from_secs(60)
    );
}

#[test]
fn headers_helper_validation() {
    let ok = cfg(
        "[servers.r]\ntransport = \"http\"\nurl = \"https://example.com/mcp\"\nheaders_helper = \"example-helper --json\"\n",
    );
    ok.validate().unwrap();
    assert_eq!(
        ok.servers["r"].headers_helper.as_deref(),
        Some("example-helper --json")
    );
    let e = errs("[servers.s]\ntransport = \"stdio\"\ncommand = \"x\"\nheaders_helper = \"h\"\n");
    assert!(has(&e, "servers.s.headers_helper"), "{e:?}");
    let e = errs(
        "[servers.r]\ntransport = \"http\"\nurl = \"https://example.com/mcp\"\nheaders_helper = \"  \"\n",
    );
    assert!(has(&e, "servers.r.headers_helper"), "{e:?}");
}

#[test]
fn subscriptions_policy_parses() {
    let c = cfg("[servers.a]\ntransport = \"stdio\"\ncommand = \"x\"\n");
    assert_eq!(c.servers["a"].subscriptions, SubscriptionPolicy::KeepAlive);
    let c = cfg(
        "[servers.a]\ntransport = \"stdio\"\ncommand = \"x\"\nsubscriptions = \"allow-unload\"\n",
    );
    assert_eq!(
        c.servers["a"].subscriptions,
        SubscriptionPolicy::AllowUnload
    );
    assert!(Config::parse(
        "[servers.a]\ntransport = \"stdio\"\ncommand = \"x\"\nsubscriptions = \"never\"\n"
    )
    .is_err());
    // The default is not written back.
    assert!(!toml::to_string(&c.servers["a"])
        .unwrap()
        .contains("keep-alive"));
}

#[test]
fn debug_defaults_to_off() {
    let c = cfg(GOOD);
    assert_eq!(c.debug.mode, DebugMode::Off);
    assert_eq!(c.debug, DebugConfig::default());
    assert_eq!(c.servers["example"].debug, None);
    assert_eq!(c.servers["example"].call_timeout, None);
    c.validate().unwrap();
}

#[test]
fn debug_table_parses() {
    let c = cfg(r#"
[debug]
mode = "failures"
dir = "diag"
max_file_bytes = 1048576
keep = 3
frames = 16
stderr_lines = 10
slow_call = "5s"
tool_errors = true
raw = true
raw_for = "10m"
"#);
    let d = &c.debug;
    assert_eq!(d.mode, DebugMode::Failures);
    assert_eq!(d.dir, PathBuf::from("diag"));
    assert_eq!(d.max_file_bytes, 1_048_576);
    assert_eq!(d.keep, 3);
    assert_eq!(d.frames, 16);
    assert_eq!(d.stderr_lines, 10);
    assert_eq!(d.slow_call, Duration::from_secs(5));
    assert!(d.tool_errors);
    assert!(d.raw);
    assert_eq!(d.raw_for, Duration::from_secs(600));
    c.validate().unwrap();
}

#[test]
fn debug_rejects_unknown_keys_and_modes() {
    assert!(Config::parse("[debug]\nmode = \"verbose\"\n").is_err());
    assert!(Config::parse("[debug]\nmodee = \"off\"\n").is_err());
    assert!(Config::parse("[debug]\nslow_call = \"soon\"\n").is_err());
    assert!(Config::parse("[debug]\nkeep = -1\n").is_err());
}

#[test]
fn debug_validation_reports_each_field() {
    let e = errs(
        r#"
[debug]
dir = ""
max_file_bytes = 10
keep = 0
frames = 0
stderr_lines = 5000
slow_call = "0s"
raw_for = "0s"
"#,
    );
    for f in [
        "debug.dir",
        "debug.max_file_bytes",
        "debug.keep",
        "debug.frames",
        "debug.stderr_lines",
        "debug.slow_call",
        "debug.raw_for",
    ] {
        assert!(has(&e, f), "missing {f}: {e:?}");
    }
}

#[test]
fn server_debug_and_call_timeout_parse() {
    let c = cfg(r#"
[servers.a]
transport = "stdio"
command = "prog"
debug = "trace"
call_timeout = "90s"

[servers.b]
transport = "stdio"
command = "prog"
debug = "off"
"#);
    c.validate().unwrap();
    assert_eq!(c.servers["a"].debug, Some(DebugMode::Trace));
    assert_eq!(c.servers["a"].call_timeout, Some(Duration::from_secs(90)));
    assert_eq!(c.servers["b"].debug, Some(DebugMode::Off));
    let back = toml::to_string(&c.servers["a"]).unwrap();
    assert!(back.contains("debug = \"trace\""), "{back}");
    assert!(back.contains("call_timeout = \"90s\""), "{back}");
}

#[test]
fn server_debug_and_call_timeout_validation() {
    let e = errs(
        r#"
[servers.a]
transport = "stdio"
command = "prog"
call_timeout = "0s"
"#,
    );
    assert!(has(&e, "servers.a.call_timeout"), "{e:?}");
    assert!(Config::parse(
        "[servers.a]\ntransport = \"stdio\"\ncommand = \"p\"\ndebug = \"loud\"\n"
    )
    .is_err());
    assert!(Config::parse(
        "[servers.a]\ntransport = \"stdio\"\ncommand = \"p\"\ncall_timeout = \"5x\"\n"
    )
    .is_err());
}

#[test]
fn env_layer_parses_mode_and_names() {
    assert_eq!(
        DebugLayer::from_env_value("").unwrap(),
        DebugLayer::default()
    );
    let l = DebugLayer::from_env_value("trace").unwrap();
    assert_eq!(l.all, Some(DebugMode::Trace));
    assert!(l.servers.is_empty());
    let l = DebugLayer::from_env_value(" example, other-1 ").unwrap();
    assert_eq!(l.all, None);
    assert_eq!(l.servers.get("example"), Some(&DebugMode::Failures));
    assert_eq!(l.servers.get("other-1"), Some(&DebugMode::Failures));
    let l = DebugLayer::from_env_value("failures,example").unwrap();
    assert_eq!(l.all, Some(DebugMode::Failures));
    assert!(DebugLayer::from_env_value("trace,failures").is_err());
    assert!(DebugLayer::from_env_value("a/b").is_err());
    let e = DebugLayer::from_env_value("bad name").unwrap_err();
    assert!(
        !e.contains("bad name"),
        "error must not echo the value: {e}"
    );
}

#[test]
fn cli_layer_scopes_by_server_name() {
    let all = DebugLayer::from_cli(Some(DebugMode::Trace), &[]);
    assert_eq!(all.all, Some(DebugMode::Trace));
    let some = DebugLayer::from_cli(None, &["a".to_owned()]);
    assert_eq!(some.all, None);
    assert_eq!(some.servers.get("a"), Some(&DebugMode::Failures));
    assert_eq!(DebugLayer::from_cli(None, &[]), DebugLayer::default());
}

fn settings(text: &str, layers: &DebugLayers) -> DebugSettings {
    resolve_debug(&cfg(text), layers)
}

const DEBUG_SERVERS: &str = r#"
[debug]
mode = "failures"
dir = "/var/mcpdoze-test/debug"
keep = 2

[servers.pinned_off]
transport = "stdio"
command = "prog"
debug = "off"

[servers.traced]
transport = "stdio"
command = "prog"
debug = "trace"

[servers.plain]
transport = "stdio"
command = "prog"
"#;

#[test]
fn resolution_precedence_runtime_cli_env_server_global() {
    let none = DebugLayers::default();
    let s = settings(DEBUG_SERVERS, &none);
    assert_eq!(s.mode_for("plain"), DebugMode::Failures, "global");
    assert_eq!(
        s.mode_for("traced"),
        DebugMode::Trace,
        "per-server beats global"
    );
    assert_eq!(
        s.mode_for("pinned_off"),
        DebugMode::Off,
        "per-server off beats global"
    );
    assert_eq!(s.mode_for("unknown"), DebugMode::Failures);
    assert_eq!(s.level_for("traced"), crate::diag::Level::Trace);

    let env = DebugLayers {
        env: DebugLayer::from_env_value("trace").unwrap(),
        ..DebugLayers::default()
    };
    let s = settings(DEBUG_SERVERS, &env);
    assert_eq!(
        s.mode_for("pinned_off"),
        DebugMode::Trace,
        "env beats per-server"
    );
    assert_eq!(s.mode_for("plain"), DebugMode::Trace);

    let env_name = DebugLayers {
        env: DebugLayer::from_env_value("pinned_off").unwrap(),
        ..DebugLayers::default()
    };
    assert_eq!(
        settings(DEBUG_SERVERS, &env_name).mode_for("pinned_off"),
        DebugMode::Failures
    );
    assert_eq!(
        settings(DEBUG_SERVERS, &env_name).mode_for("plain"),
        DebugMode::Failures
    );

    let cli = DebugLayers {
        cli: DebugLayer::from_cli(Some(DebugMode::Off), &[]),
        env: DebugLayer::from_env_value("trace").unwrap(),
        ..DebugLayers::default()
    };
    assert_eq!(
        settings(DEBUG_SERVERS, &cli).mode_for("plain"),
        DebugMode::Off,
        "cli beats env"
    );

    let runtime = DebugLayers {
        runtime: DebugLayer::from_cli(Some(DebugMode::Trace), &["plain".to_owned()]),
        cli: DebugLayer::from_cli(Some(DebugMode::Off), &[]),
        ..DebugLayers::default()
    };
    let s = settings(DEBUG_SERVERS, &runtime);
    assert_eq!(s.mode_for("plain"), DebugMode::Trace, "runtime beats cli");
    assert_eq!(
        s.mode_for("traced"),
        DebugMode::Off,
        "cli applies to the rest"
    );
}

#[test]
fn resolution_builds_recorder_config() {
    let dir = std::env::temp_dir().join("mcpdoze-test-debug");
    let mut c = cfg(DEBUG_SERVERS);
    c.debug.dir = dir.clone();
    let s = resolve_debug(&c, &DebugLayers::default());
    let diag = s.diag.expect("absolute dir resolves");
    assert_eq!(diag.dir, dir);
    assert_eq!(diag.keep, 2);
    assert_eq!(diag.max_file_bytes, DebugConfig::default().max_file_bytes);
    assert!(!s.raw);
}

#[test]
fn off_by_default_and_runtime_layer_overrides_global() {
    let s = settings(GOOD, &DebugLayers::default());
    assert_eq!(s.mode_for("example"), DebugMode::Off);
    let forced = DebugLayers {
        runtime: DebugLayer::from_cli(Some(DebugMode::Trace), &[]),
        ..DebugLayers::default()
    };
    assert_eq!(
        settings(GOOD, &forced).mode_for("example"),
        DebugMode::Trace
    );
}
