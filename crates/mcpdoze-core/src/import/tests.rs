// SPDX-License-Identifier: GPL-3.0-or-later
use std::fs;
use std::path::{Path, PathBuf};

use super::parse::{classify, strip_jsonc};
use super::*;
use crate::config::{Config, Scope, Transport};

const URL: &str = "http://127.0.0.1:7878/mcp";
const SECRET: &str = "not-a-real-value-123";

struct Fx {
    _tmp: tempfile::TempDir,
    env: ImportEnv,
}

fn fx() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    let env = ImportEnv {
        home: r.join("home"),
        cwd: r.join("proj"),
        config_dir: r.join("cfg"),
        xdg_config: r.join("xdg"),
        codex_home: r.join("home").join(".codex"),
        data_dir: r.join("data"),
        broker_config: r.join("broker").join("config.toml"),
        broker_url: URL.into(),
        token_env_var: DEFAULT_TOKEN_ENV.into(),
    };
    fs::create_dir_all(&env.home).unwrap();
    fs::create_dir_all(&env.cwd).unwrap();
    Fx { _tmp: tmp, env }
}

fn write(p: &Path, s: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, s).unwrap();
}

fn claude_user_json() -> String {
    format!(
        r#"{{
  "numStartups": 3,
  "mcpServers": {{
    "files": {{ "type": "stdio", "command": "npx", "args": ["-y", "example-files", "--dir", "/home/user/data"],
               "env": {{ "API_KEY": "{SECRET}", "MODE": "fast", "FROM_OUTER": "${{OUTER_VAR}}" }} }},
    "remote one": {{ "type": "http", "url": "https://example.com/mcp",
               "headers": {{ "Authorization": "Bearer {SECRET}", "X-Team": "alpha" }} }},
    "legacy": {{ "type": "sse", "url": "https://example.com/sse" }},
    "mcpdoze": {{ "type": "http", "url": "{URL}/" }}
  }},
  "projects": {{
    "/home/user/proj": {{ "mcpServers": {{ "local": {{ "command": "mock", "args": [] }} }} }}
  }}
}}
"#
    )
}

fn plan_for(f: &Fx, agent: Agent) -> Plan {
    build_plan(agent, &f.env, None).unwrap()
}

fn item<'a>(p: &'a Plan, name: &str) -> &'a PlanItem {
    p.items
        .iter()
        .find(|i| i.name == name)
        .unwrap_or_else(|| panic!("no item {name}"))
}

#[test]
fn classify_refs() {
    assert_eq!(classify("${A_B}"), RawValue::EnvRef("A_B".into()));
    assert_eq!(classify("${env:A}"), RawValue::EnvRef("A".into()));
    assert_eq!(classify("{env:A}"), RawValue::EnvRef("A".into()));
    assert_eq!(
        classify("Bearer ${T}"),
        RawValue::PrefixedRef {
            prefix: "Bearer ".into(),
            var: "T".into()
        }
    );
    assert_eq!(classify("plain"), RawValue::Literal("plain".into()));
    assert_eq!(classify("${a b}"), RawValue::Literal("${a b}".into()));
}

#[test]
fn jsonc_stripping() {
    let s = strip_jsonc("{ // c\n \"a\": \"x//y\", /* b */ \"b\": [1,2,], }");
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["a"], "x//y");
    assert_eq!(v["b"][1], 2);
}

#[test]
fn claude_plan() {
    let f = fx();
    write(&f.env.home.join(".claude.json"), &claude_user_json());
    write(
        &f.env.cwd.join(".mcp.json"),
        r#"{ "mcpServers": { "local": { "command": "mock", "args": [] } } }"#,
    );
    let p = plan_for(&f, Agent::ClaudeCode);
    assert_eq!(p.items.len(), 3, "{}", p.render_text());

    let files = item(&p, "files");
    assert_eq!(files.scope, Scope::PerCwd);
    assert_eq!(files.transport, Transport::Stdio);
    assert_eq!(files.env_keys, ["API_KEY", "FROM_OUTER", "MODE"]);
    assert_eq!(files.needs_env_var.len(), 1);
    assert_eq!(files.needs_env_var[0].key, "API_KEY");
    assert_eq!(
        files.needs_env_var[0].suggested_env_var,
        "MCPDOZE_FILES_API_KEY"
    );
    assert_eq!(files.prefix, "files");

    let remote = item(&p, "remote-one");
    assert_eq!(remote.prefix, "remote_one");
    assert_eq!(remote.scope, Scope::Shared);
    assert_eq!(remote.header_names, ["Authorization", "X-Team"]);
    assert_eq!(remote.needs_env_var[0].location, "header");

    // `local` appears in the project entry and .mcp.json: merged, per-cwd.
    let local = item(&p, "local");
    assert_eq!(local.scope, Scope::PerCwd);
    assert_eq!(local.sources.len(), 2);

    let reasons: Vec<_> = p.skipped.iter().map(|s| s.reason.as_str()).collect();
    assert!(reasons.iter().any(|r| r.contains("SSE")));
    assert!(reasons.iter().any(|r| r.contains("already points")));

    // Output carries no secret values.
    for out in [p.to_json(), p.render_text()] {
        assert!(!out.contains(SECRET), "secret leaked");
    }
    assert!(p
        .render_text()
        .contains("mcp__mcpdoze__<prefix>__<tool>; update permission allowlists"));
    // Config for the broker validates and holds refs, not secrets.
    let mut cfg = Config::default();
    for i in &p.items {
        cfg.servers.insert(i.name.clone(), i.config.clone());
    }
    cfg.validate().unwrap();
    assert!(!format!("{:?}", files.config).contains(SECRET));
}

#[test]
fn codex_plan() {
    let f = fx();
    write(
        &f.env.codex_home.join("config.toml"),
        r#"model = "x"
[mcp_servers.alpha]
command = "mock"
args = ["--flag"]
env = { TOKEN_X = "not-a-real-value-123", PLAIN = "1" }

[mcp_servers.beta]
url = "https://example.com/mcp"
bearer_token_env_var = "BETA_TOKEN"
http_headers = { "X-A" = "b" }
env_http_headers = { "X-C" = "C_VAR" }

[mcp_servers.off]
command = "x"
enabled = false
"#,
    );
    let p = plan_for(&f, Agent::Codex);
    assert_eq!(p.items.len(), 2);
    assert_eq!(item(&p, "alpha").needs_env_var[0].key, "TOKEN_X");
    let beta = item(&p, "beta");
    assert_eq!(beta.header_names, ["Authorization", "X-A", "X-C"]);
    assert!(beta.needs_env_var.is_empty());
    assert!(p.skipped.iter().any(|s| s.reason == "disabled"));
}

#[test]
fn opencode_plan() {
    let f = fx();
    write(
        &f.env.xdg_config.join("opencode").join("opencode.json"),
        &format!(
            r#"{{ "$schema": "x", "mcp": {{
  "loc": {{ "type": "local", "command": ["npx", "-y", "pkg"], "environment": {{ "A_SECRET": "{SECRET}" }} }},
  "rem": {{ "type": "remote", "url": "https://example.com/mcp", "headers": {{ "X-Y": "{{env:YV}}" }} }},
  "dis": {{ "type": "local", "command": ["x"], "enabled": false }}
}} }}"#
        ),
    );
    let p = plan_for(&f, Agent::OpenCode);
    assert_eq!(p.items.len(), 2);
    let loc = item(&p, "loc");
    assert_eq!(loc.command.as_deref(), Some("npx"));
    assert_eq!(loc.args, ["-y", "pkg"]);
    assert_eq!(loc.needs_env_var[0].key, "A_SECRET");
    assert_eq!(item(&p, "rem").transport, Transport::Http);
    assert!(item(&p, "rem").needs_env_var.is_empty());
}

#[test]
fn opencode_with_comments_is_skipped() {
    let f = fx();
    write(
        &f.env.cwd.join("opencode.json"),
        "{ // note\n \"mcp\": { \"a\": { \"type\": \"local\", \"command\": [\"x\"] } } }",
    );
    let p = plan_for(&f, Agent::OpenCode);
    assert!(p.items.is_empty());
    assert!(p.skipped[0].reason.contains("comments"));
}

#[test]
fn cursor_and_vscode_plan() {
    let f = fx();
    write(
        &f.env.home.join(".cursor").join("mcp.json"),
        r#"{ "mcpServers": { "c1": { "command": "mock", "args": ["${workspaceFolder}"] },
            "c2": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${env:C2_TOKEN}" } } } }"#,
    );
    let p = plan_for(&f, Agent::Cursor);
    assert_eq!(item(&p, "c1").scope, Scope::PerCwd);
    let c2 = item(&p, "c2");
    assert!(c2.needs_env_var.is_empty());
    assert!(c2.warnings.iter().any(|w| w.contains("full value")));

    write(
        &f.env.cwd.join(".vscode").join("mcp.json"),
        r#"{ "inputs": [], "servers": {
            "v1": { "type": "stdio", "command": "mock", "env": { "K": "${input:k}" } },
            "v2": { "type": "http", "url": "https://example.com/mcp" } } }"#,
    );
    let p = plan_for(&f, Agent::VsCode);
    assert_eq!(p.items.len(), 1);
    assert_eq!(item(&p, "v2").scope, Scope::PerCwd);
    assert!(p.skipped.iter().any(|s| s.reason.contains("input")));
}

#[test]
fn secret_in_args_is_skipped() {
    let f = fx();
    write(
        &f.env.home.join(".claude.json"),
        &format!(
            r#"{{ "mcpServers": {{ "a": {{ "command": "x", "args": ["--api-key", "{SECRET}"] }},
                "b": {{ "command": "x", "args": ["--token=abc"] }},
                "c": {{ "command": "x", "args": ["--api-key", "${{KEY_VAR}}"] }} }} }}"#
        ),
    );
    let p = plan_for(&f, Agent::ClaudeCode);
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].name, "c");
    assert_eq!(p.skipped.len(), 2);
    assert!(!p.render_text().contains(SECRET));
}

#[test]
fn name_conflict_with_existing_broker_server() {
    let f = fx();
    write(
        &f.env.broker_config,
        "[servers.files]\ntransport = \"stdio\"\ncommand = \"other\"\n",
    );
    write(&f.env.home.join(".claude.json"), &claude_user_json());
    let p = plan_for(&f, Agent::ClaudeCode);
    let renamed = item(&p, "files-2");
    assert_eq!(renamed.prefix, "files_2");
    assert!(p.warnings.iter().any(|w| w.contains("already used")));
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap()
}

#[test]
fn claude_round_trip_and_rollback() {
    let f = fx();
    let cj = f.env.home.join(".claude.json");
    let original = claude_user_json();
    write(&cj, &original);
    write(
        &f.env.broker_config,
        "# my broker\n[broker]\nbind = \"127.0.0.1:7878\"\n",
    );
    let before_broker = read(&f.env.broker_config);
    let p = plan_for(&f, Agent::ClaudeCode);
    let n = p.items.len();
    let res = apply(&p, &f.env.data_dir, &ApplyOptions { now_secs: Some(0) })
        .unwrap()
        .unwrap();
    assert_eq!(
        res.manifest_path,
        f.env.data_dir.join("import").join("19700101T000000Z.json")
    );

    // Agent file: migrated entries gone, single broker entry, SSE kept.
    let new: serde_json::Value = serde_json::from_str(&read(&cj)).unwrap();
    let servers = new["mcpServers"].as_object().unwrap();
    assert!(servers.contains_key("legacy"));
    assert!(!servers.contains_key("files"));
    let b = &servers["mcpdoze"];
    assert_eq!(b["url"], URL);
    assert_eq!(b["headers"]["Authorization"], "Bearer ${MCPDOZE_TOKEN}");
    assert_eq!(new["numStartups"], 3);
    assert!(new["projects"]["/home/user/proj"]["mcpServers"]
        .as_object()
        .unwrap()
        .is_empty());

    // Broker config: comment kept, servers valid, no secret value anywhere.
    let bc = read(&f.env.broker_config);
    assert!(bc.starts_with("# my broker"));
    let cfg = Config::parse(&bc).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.servers.len(), n);
    for path in [&cj, &f.env.broker_config, &res.manifest_path] {
        let t = read(path);
        assert!(!t.contains(SECRET), "secret in {}", path.display());
    }
    // Backups and manifest.
    let m = &res.manifest;
    assert_eq!(m.registered_servers.len(), n);
    let agent_file = m.files.iter().find(|x| x.role == "agent").unwrap();
    assert_eq!(
        agent_file.original_sha256.as_deref(),
        Some(sha256_hex(original.as_bytes()).as_str())
    );
    assert_eq!(read(agent_file.backup.as_ref().unwrap()), original);
    assert!(agent_file
        .backup
        .as_ref()
        .unwrap()
        .to_string_lossy()
        .contains(".mcpdoze-bak-19700101T000000Z"));

    // Idempotent re-import: nothing left to migrate.
    let p2 = plan_for(&f, Agent::ClaudeCode);
    assert!(p2.items.is_empty(), "{}", p2.render_text());
    assert!(apply(&p2, &f.env.data_dir, &ApplyOptions::default())
        .unwrap()
        .is_none());

    // Rollback.
    let r = rollback(&res.manifest_path).unwrap();
    assert_eq!(r.removed_servers.len(), n);
    assert_eq!(read(&cj), original);
    let after = read(&f.env.broker_config);
    assert!(Config::parse(&after).unwrap().servers.is_empty());
    assert!(after.contains("# my broker"));
    assert_eq!(after.trim(), before_broker.trim());
    assert!(matches!(
        rollback(&res.manifest_path),
        Err(ImportError::AlreadyRolledBack)
    ));
}

#[test]
fn rollback_refused_when_json_invalid() {
    let f = fx();
    let cj = f.env.home.join(".claude.json");
    write(&cj, &claude_user_json());
    let p = plan_for(&f, Agent::ClaudeCode);
    let res = apply(&p, &f.env.data_dir, &ApplyOptions::default())
        .unwrap()
        .unwrap();
    let edited = format!("{} {{ broken", read(&cj));
    fs::write(&cj, &edited).unwrap();
    let broker_before = read(&f.env.broker_config);
    let err = rollback(&res.manifest_path).unwrap_err();
    assert!(matches!(err, ImportError::Parse { .. }), "{err}");
    assert!(err.to_string().contains(".claude.json"));
    assert!(err.to_string().contains("refusing"));
    // Nothing touched; the manifest is still usable.
    assert_eq!(read(&cj), edited);
    assert_eq!(read(&f.env.broker_config), broker_before);
    assert!(!read(&res.manifest_path).contains("rolled_back_utc"));
}

#[test]
fn surgical_rollback_after_agent_rewrite() {
    let f = fx();
    let cj = f.env.home.join(".claude.json");
    let original = claude_user_json();
    write(&cj, &original);
    let p = plan_for(&f, Agent::ClaudeCode);
    let res = apply(&p, &f.env.data_dir, &ApplyOptions { now_secs: Some(0) })
        .unwrap()
        .unwrap();
    // The manifest carries the entries but never the literal secret.
    let mt = read(&res.manifest_path);
    assert!(!mt.contains(SECRET));
    assert!(mt.contains(REDACTED));
    assert!(mt.contains("${OUTER_VAR}"));

    // The agent rewrites its file: new unrelated keys, a changed value, a
    // new server of its own.
    let mut cur: serde_json::Value = serde_json::from_str(&read(&cj)).unwrap();
    cur["numStartups"] = 4.into();
    cur["tipsHistory"] = serde_json::json!({ "a": 1 });
    cur["mcpServers"]["added-later"] = serde_json::json!({ "command": "mock2" });
    fs::write(&cj, serde_json::to_string(&cur).unwrap()).unwrap();

    let r = rollback(&res.manifest_path).unwrap();
    assert_eq!(r.restored, vec![cj.clone()]);
    assert_eq!(r.removed_servers.len(), p.items.len());
    let after: serde_json::Value = serde_json::from_str(&read(&cj)).unwrap();
    let orig: serde_json::Value = serde_json::from_str(&original).unwrap();
    // Unrelated keys survive.
    assert_eq!(after["numStartups"], 4);
    assert_eq!(after["tipsHistory"]["a"], 1);
    assert_eq!(after["mcpServers"]["added-later"]["command"], "mock2");
    // Migrated entries are back, secrets included, in their old order.
    for k in ["files", "remote one", "legacy", "mcpdoze"] {
        assert_eq!(after["mcpServers"][k], orig["mcpServers"][k], "{k}");
    }
    let keys: Vec<&String> = after["mcpServers"].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["files", "remote one", "legacy", "mcpdoze", "added-later"]
    );
    assert_eq!(
        after["projects"]["/home/user/proj"],
        orig["projects"]["/home/user/proj"]
    );
    assert!(Config::parse(&read(&f.env.broker_config))
        .unwrap()
        .servers
        .is_empty());

    // Second rollback: refused, nothing changes.
    let snapshot = read(&cj);
    assert!(matches!(
        rollback(&res.manifest_path),
        Err(ImportError::AlreadyRolledBack)
    ));
    assert_eq!(read(&cj), snapshot);
}

#[test]
fn surgical_rollback_removes_created_container() {
    let f = fx();
    let cj = f.env.home.join(".claude.json");
    // Only a project-scoped server: the import creates top-level mcpServers.
    write(
        &cj,
        "{\"projects\": {\"/p\": {\"mcpServers\": {\"local\": {\"command\": \"mock\"}}}}}\n",
    );
    let p = plan_for(&f, Agent::ClaudeCode);
    let res = apply(&p, &f.env.data_dir, &ApplyOptions::default())
        .unwrap()
        .unwrap();
    let mut cur: serde_json::Value = serde_json::from_str(&read(&cj)).unwrap();
    cur["other"] = true.into();
    fs::write(&cj, serde_json::to_string(&cur).unwrap()).unwrap();
    rollback(&res.manifest_path).unwrap();
    let after: serde_json::Value = serde_json::from_str(&read(&cj)).unwrap();
    assert!(after.get("mcpServers").is_none(), "{after}");
    assert_eq!(after["other"], true);
    assert_eq!(
        after["projects"]["/p"]["mcpServers"]["local"]["command"],
        "mock"
    );
}

#[test]
fn old_manifest_without_saved_entries_needs_identical_file() {
    let f = fx();
    let cj = f.env.home.join(".claude.json");
    write(&cj, &claude_user_json());
    let p = plan_for(&f, Agent::ClaudeCode);
    let res = apply(&p, &f.env.data_dir, &ApplyOptions::default())
        .unwrap()
        .unwrap();
    // Simulate a version 1 manifest.
    let mut m: serde_json::Value = serde_json::from_str(&read(&res.manifest_path)).unwrap();
    m["version"] = 1.into();
    for file in m["files"].as_array_mut().unwrap() {
        file.as_object_mut().unwrap().remove("json_edit");
    }
    fs::write(&res.manifest_path, m.to_string()).unwrap();
    let edited = format!("{} ", read(&cj));
    fs::write(&cj, &edited).unwrap();
    let err = rollback(&res.manifest_path).unwrap_err();
    assert!(matches!(err, ImportError::Changed(_)), "{err}");
    assert_eq!(read(&cj), edited);
}

#[test]
fn codex_rollback_refused_when_file_changed() {
    let f = fx();
    let cfg = f.env.codex_home.join("config.toml");
    write(&cfg, "[mcp_servers.alpha]\ncommand = \"mock\"\n");
    let p = plan_for(&f, Agent::Codex);
    let res = apply(&p, &f.env.data_dir, &ApplyOptions::default())
        .unwrap()
        .unwrap();
    let edited = format!("{}# edited\n", read(&cfg));
    fs::write(&cfg, &edited).unwrap();
    let broker_before = read(&f.env.broker_config);
    let err = rollback(&res.manifest_path).unwrap_err();
    assert!(matches!(err, ImportError::Changed(_)), "{err}");
    assert_eq!(read(&cfg), edited);
    assert_eq!(read(&f.env.broker_config), broker_before);
}

#[test]
fn codex_round_trip() {
    let f = fx();
    let cfg = f.env.codex_home.join("config.toml");
    let original = format!(
        "# codex\nmodel = \"x\"\n\n[mcp_servers.alpha]\ncommand = \"mock\"\nenv = {{ MY_TOKEN = \"{SECRET}\" }}\n\n[mcp_servers.keep]\nurl = \"{URL}\"\n"
    );
    write(&cfg, &original);
    let p = plan_for(&f, Agent::Codex);
    assert_eq!(p.items.len(), 1);
    let res = apply(&p, &f.env.data_dir, &ApplyOptions::default())
        .unwrap()
        .unwrap();
    let t = read(&cfg);
    assert!(t.contains("# codex"));
    assert!(!t.contains("alpha"));
    assert!(!t.contains(SECRET));
    let doc: toml::Table = t.parse().unwrap();
    let b = &doc["mcp_servers"]["mcpdoze"];
    assert_eq!(b["url"].as_str(), Some(URL));
    assert_eq!(b["bearer_token_env_var"].as_str(), Some("MCPDOZE_TOKEN"));
    assert!(doc["mcp_servers"]["keep"].is_table());
    let bc = read(&f.env.broker_config);
    assert!(bc.contains("MCPDOZE_ALPHA_MY_TOKEN"));
    assert!(!bc.contains(SECRET));
    assert_eq!(p.items[0].needs_env_var[0].key, "MY_TOKEN");

    assert!(plan_for(&f, Agent::Codex).items.is_empty());
    rollback(&res.manifest_path).unwrap();
    assert_eq!(read(&cfg), original);
}

#[test]
fn other_agents_write_env_expansion_syntax() {
    let f = fx();
    for (agent, rel, body, needle) in [
        (
            Agent::OpenCode,
            PathBuf::from("proj/opencode.json"),
            r#"{ "mcp": { "a": { "type": "local", "command": ["x"] } } }"#,
            "Bearer {env:MCPDOZE_TOKEN}",
        ),
        (
            Agent::Cursor,
            PathBuf::from("proj/.cursor/mcp.json"),
            r#"{ "mcpServers": { "a": { "command": "x" } } }"#,
            "Bearer ${env:MCPDOZE_TOKEN}",
        ),
        (
            Agent::VsCode,
            PathBuf::from("proj/.vscode/mcp.json"),
            r#"{ "servers": { "a": { "command": "x" } } }"#,
            "Bearer ${env:MCPDOZE_TOKEN}",
        ),
    ] {
        let path = f.env.cwd.parent().unwrap().join(rel);
        write(&path, body);
        let p = plan_for(&f, agent);
        assert_eq!(p.items.len(), 1, "{agent:?}");
        apply(&p, &f.env.data_dir, &ApplyOptions::default()).unwrap();
        let t = read(&path);
        assert!(t.contains(needle), "{agent:?}: {t}");
        assert!(t.contains(URL));
        assert!(plan_for(&f, agent).items.is_empty());
        let _ = fs::remove_dir_all(f.env.data_dir.join("import"));
        // Reset broker config so the next agent's `a` does not collide.
        let _ = fs::remove_file(&f.env.broker_config);
        let _ = fs::remove_file(&path);
    }
}

#[test]
fn agent_names_parse() {
    assert_eq!("claude-code".parse::<Agent>().unwrap(), Agent::ClaudeCode);
    assert_eq!("VSCode".parse::<Agent>().unwrap(), Agent::VsCode);
    assert!("nope".parse::<Agent>().is_err());
}
