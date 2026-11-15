// SPDX-License-Identifier: GPL-3.0-or-later

//! Pure decision logic of the Claude Code `PreToolUse` hook: does this tool
//! call register an MCP server natively instead of through the broker?
//!
//! This is a guardrail on command *text*, not a security boundary. A command
//! built at runtime, read from a script file, or otherwise obfuscated is not
//! seen. The input text is never echoed back in the decision.

use serde_json::Value;

/// Outcome of inspecting one hook event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Let the tool call proceed (the hook prints nothing).
    Allow,
    /// Block the tool call with [`DENY_REASON`].
    Deny,
}

/// Message shown to the agent when a call is denied. Fixed text.
pub const DENY_REASON: &str = "MCP servers must be registered with the MCPDoze broker so only one instance of each runs, \
shared by all sessions. Do not use the agent's native MCP install. Instead run: \
`mcpdoze add <name> [--env KEY=ENVVAR] -- <command> [args...]` (or `mcpdoze add <name> --transport http --url <url>`). \
See the mcpdoze-migrate skill. Secrets are passed by environment variable name, never by value. \
Set MCPDOZE_HOOK_BYPASS=1 only if the user insists on a native install.";

/// Environment variable that disables the hook.
pub const BYPASS_ENV: &str = "MCPDOZE_HOOK_BYPASS";

/// Inspects a `PreToolUse` event. Unknown shapes are allowed.
pub fn check_event(event: &Value) -> Decision {
    let Some(tool) = event.get("tool_name").and_then(Value::as_str) else {
        return Decision::Allow;
    };
    let Some(input) = event.get("tool_input") else {
        return Decision::Allow;
    };
    match tool {
        // Claude Code's shell tool is `Bash`, or `PowerShell` on Windows.
        "Bash" | "PowerShell" => match input.get("command").and_then(Value::as_str) {
            Some(cmd) if command_registers_mcp(cmd) => Decision::Deny,
            _ => Decision::Allow,
        },
        "Write" | "Edit" | "MultiEdit" => {
            let path = input.get("file_path").and_then(Value::as_str).unwrap_or("");
            if !is_mcp_config_path(path) {
                return Decision::Allow;
            }
            let mut texts: Vec<&str> = Vec::new();
            for key in ["content", "new_string"] {
                if let Some(s) = input.get(key).and_then(Value::as_str) {
                    texts.push(s);
                }
            }
            if let Some(edits) = input.get("edits").and_then(Value::as_array) {
                texts.extend(
                    edits
                        .iter()
                        .filter_map(|e| e.get("new_string").and_then(Value::as_str)),
                );
            }
            if texts.iter().any(|t| defines_native_server(t)) {
                Decision::Deny
            } else {
                Decision::Allow
            }
        }
        _ => Decision::Allow,
    }
}

// ------------------------------------------------------------- Bash text

/// True if any command position in `cmd` is a native MCP registration.
pub fn command_registers_mcp(cmd: &str) -> bool {
    scan(cmd, 0)
}

const MAX_DEPTH: u8 = 3;

fn scan(cmd: &str, depth: u8) -> bool {
    let segs = split_segments(cmd);
    if segs.iter().any(|words| segment_registers(words, depth)) {
        return true;
    }
    // `"claude mcp add x" | iex` or `$c = '...'; iex $c`: when the text
    // evaluates strings, treat every multi-word string as a command.
    if depth < MAX_DEPTH
        && segs
            .iter()
            .flatten()
            .any(|w| matches!(base_name(w).as_str(), "iex" | "invoke-expression"))
    {
        return segs
            .iter()
            .flatten()
            .filter(|w| w.contains(char::is_whitespace))
            .any(|w| scan(w, depth + 1));
    }
    false
}

/// Splits into command segments (on `&&`, `||`, `;`, `|`, `&`, newline) of
/// words, honouring single and double quotes. Comments are dropped.
#[allow(unused_assignments)]
fn split_segments(cmd: &str) -> Vec<Vec<String>> {
    let mut segs: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = cmd.chars().peekable();
    macro_rules! end_word {
        () => {
            if in_word {
                words.push(std::mem::take(&mut cur));
                in_word = false;
            }
        };
    }
    macro_rules! end_seg {
        () => {
            end_word!();
            if !words.is_empty() {
                segs.push(std::mem::take(&mut words));
            }
        };
    }
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    cur.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' if chars.peek() == Some(&'"') => {
                            cur.push('"');
                            chars.next();
                        }
                        _ => cur.push(q),
                    }
                }
            }
            '#' if !in_word => {
                for q in chars.by_ref() {
                    if q == '\n' {
                        break;
                    }
                }
                end_seg!();
            }
            '&' | '|' | ';' | '\n' | '\r' => {
                end_seg!();
            }
            c if c.is_whitespace() => end_word!(),
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    end_seg!();
    segs
}

/// A word stripped of PowerShell escapes (backticks) and of leading
/// grouping/call syntax (`(`, `{`, `$(`) and trailing `)` / `}`.
fn clean(word: &str) -> String {
    let w: String = word.chars().filter(|&c| c != '`').collect();
    w.trim_start_matches(['(', '{', '$'])
        .trim_end_matches([')', '}'])
        .to_owned()
}

fn base_name(word: &str) -> String {
    let word = clean(word);
    let word = word.as_str();
    let w = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let lower = w.to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".ps1"] {
        if let Some(stem) = lower.strip_suffix(ext) {
            return stem.to_owned();
        }
    }
    lower
}

fn is_env_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((k, _)) => {
            let mut c = k.chars();
            matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
                && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        }
        None => false,
    }
}

fn segment_registers(words: &[String], depth: u8) -> bool {
    let mut i = 0;
    // Strip prefixes that do not change which program runs.
    while i < words.len() {
        let w = clean(&words[i]);
        let b = base_name(&w);
        if w.is_empty() || is_env_assignment(&w) {
            i += 1;
        } else if matches!(
            b.as_str(),
            "sudo"
                | "command"
                | "time"
                | "env"
                | "nohup"
                | "exec"
                | "call"
                | "start"
                | "npx"
                | "bunx"
                | "pnpx"
                | "."
        ) {
            i += 1;
            while i < words.len() && words[i].starts_with('-') {
                i += 1;
            }
        } else {
            break;
        }
    }
    let Some(first) = words.get(i) else {
        return false;
    };
    let prog = base_name(first);
    let rest = &words[i + 1..];
    match prog.as_str() {
        "pwsh" | "powershell" | "bash" | "sh" | "zsh" | "dash" | "cmd" => {
            if depth >= MAX_DEPTH {
                return false;
            }
            match rest.iter().position(|a| is_command_flag(&prog, a)) {
                Some(p) if p + 1 < rest.len() => scan(&rest[p + 1..].join(" "), depth + 1),
                _ => false,
            }
        }
        // PowerShell wrappers that run their argument text as a command.
        "iex" | "invoke-expression" | "invoke-command" | "icm" | "start-process" | "saps" => {
            if depth >= MAX_DEPTH {
                return false;
            }
            let text: Vec<String> = rest
                .iter()
                .filter(|a| !a.starts_with('-'))
                .map(|a| a.replace(',', " "))
                .collect();
            scan(&text.join(" "), depth + 1)
        }
        "claude" | "codex" | "gemini" | "opencode" => registration_args(rest),
        "code" | "code-insiders" | "codium" | "cursor" => rest
            .iter()
            .any(|a| a == "--add-mcp" || a.starts_with("--add-mcp=")),
        _ => false,
    }
}

fn is_command_flag(prog: &str, a: &str) -> bool {
    let l = a.to_ascii_lowercase();
    match prog {
        "pwsh" | "powershell" => matches!(l.as_str(), "-c" | "-command"),
        "cmd" => matches!(l.as_str(), "/c" | "/k"),
        _ => l.len() >= 2 && l.starts_with('-') && !l.starts_with("--") && l.ends_with('c'),
    }
}

/// Words after `claude`/`codex`/...: `[flags] mcp [flags] add*`.
fn registration_args(rest: &[String]) -> bool {
    let plain: Vec<&str> = rest
        .iter()
        .map(String::as_str)
        .filter(|w| !w.starts_with('-'))
        .collect();
    let Some(m) = plain
        .iter()
        .take(2)
        .position(|w| w.eq_ignore_ascii_case("mcp"))
    else {
        return false;
    };
    let is_add = |w: &str| {
        matches!(
            w.to_ascii_lowercase().as_str(),
            "add" | "add-json" | "add-from-claude-desktop"
        )
    };
    match plain.get(m + 1) {
        Some(w) if is_add(w) => true,
        Some(w)
            if matches!(
                w.to_ascii_lowercase().as_str(),
                "list" | "get" | "remove" | "serve" | "rm"
            ) =>
        {
            false
        }
        // `claude mcp --scope user add x`: one flag value before the verb.
        Some(_) => plain.get(m + 2).is_some_and(|w| is_add(w)),
        None => false,
    }
}

// -------------------------------------------------------- config content

fn is_mcp_config_path(path: &str) -> bool {
    let b = path.rsplit(['/', '\\']).next().unwrap_or("");
    b.eq_ignore_ascii_case(".mcp.json") || b.eq_ignore_ascii_case("mcp.json")
}

fn is_local_url(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| {
            l.strip_prefix(p)
                .is_some_and(|r| r.is_empty() || r.starts_with([':', '/']))
        })
}

fn defines_native_server(text: &str) -> bool {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => value_defines_server(&v, false),
        Err(_) => fragment_defines_server(text),
    }
}

fn value_defines_server(v: &Value, in_servers: bool) -> bool {
    let map = match v {
        Value::Object(m) => m,
        Value::Array(a) => return a.iter().any(|x| value_defines_server(x, false)),
        _ => return false,
    };
    if in_servers {
        // `map` is the servers table: name -> server definition.
        return map
            .iter()
            .any(|(name, def)| server_def_is_native(name, def));
    }
    if map.get("command").is_some_and(Value::is_string) {
        return true;
    }
    map.iter().any(|(k, child)| {
        let servers = matches!(k.as_str(), "mcpServers" | "servers");
        value_defines_server(child, servers && child.is_object())
    })
}

fn server_def_is_native(name: &str, def: &Value) -> bool {
    let Some(o) = def.as_object() else {
        return false;
    };
    if o.get("command").is_some_and(Value::is_string) {
        return true;
    }
    match o.get("url").and_then(Value::as_str) {
        Some(u) => !(name == "mcpdoze" && is_local_url(u)),
        None => false,
    }
}

/// Text that is not valid JSON (an Edit fragment): look for `"command":` or
/// a non-local `"url":` value.
fn fragment_defines_server(text: &str) -> bool {
    let mut rest = text;
    while let Some(p) = rest.find("\"command\"") {
        let after = rest[p + 9..].trim_start();
        if after.starts_with(':') {
            return true;
        }
        rest = &rest[p + 9..];
    }
    let mut rest = text;
    while let Some(p) = rest.find("\"url\"") {
        let after = rest[p + 5..].trim_start();
        if let Some(v) = after.strip_prefix(':') {
            if let Some(v) = v.trim_start().strip_prefix('"') {
                let val = v.split('"').next().unwrap_or("");
                if !is_local_url(val) {
                    return true;
                }
            }
        }
        rest = &rest[p + 5..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(cmd: &str) -> Decision {
        check_event(&json!({"tool_name":"Bash","tool_input":{"command":cmd}}))
    }

    #[test]
    fn bash_table() {
        let deny = [
            "claude mcp add foo -- npx -y bar",
            "claude mcp add-json foo '{\"command\":\"x\"}'",
            "claude mcp add-from-claude-desktop",
            "claude mcp add --scope user --transport http foo https://example.test/mcp",
            "claude mcp --scope user add foo -- x",
            "CLAUDE MCP ADD foo -- x",
            "claude.exe mcp add foo -- x",
            "C:\\tools\\claude.exe mcp add foo -- x",
            "/usr/local/bin/claude mcp add foo -- x",
            "codex mcp add foo -- x",
            "gemini mcp add foo x",
            "opencode mcp add foo",
            "code --add-mcp '{\"name\":\"x\"}'",
            "code.cmd --add-mcp={}",
            "cd /tmp && claude mcp add foo -- x",
            "true; claude mcp add foo -- x",
            "echo hi | claude mcp add foo -- x",
            "false || codex mcp add foo -- x",
            "echo a\nclaude mcp add foo -- x",
            "FOO=1 claude mcp add foo -- x",
            "sudo claude mcp add foo -- x",
            "command claude mcp add foo -- x",
            "time codex mcp add foo -- x",
            "pwsh -c \"claude mcp add foo -- x\"",
            "pwsh -NoProfile -Command claude mcp add foo -- x",
            "bash -c 'claude mcp add foo -- x'",
            "bash -lc \"cd /x && codex mcp add foo -- y\"",
            "sh -c \"bash -c 'claude mcp add foo -- x'\"",
            "cmd /c claude mcp add foo -- x",
            "& claude mcp add foo -- x",
            "npx claude mcp add foo -- x",
            "claude --debug mcp add foo -- x",
        ];
        for c in deny {
            assert_eq!(bash(c), Decision::Deny, "should deny: {c}");
        }
        let allow = [
            "claude mcp list",
            "claude mcp get foo",
            "claude mcp remove foo",
            "claude mcp serve",
            "claude mcp --scope user list",
            "echo \"claude mcp add x\"",
            "echo claude mcp add x",
            "# claude mcp add x",
            "ls # claude mcp add x",
            "grep 'claude mcp add' README.md",
            "git commit -m 'document claude mcp add'",
            "claude -p \"please run claude mcp add foo\"",
            "claude --help",
            "code --install-extension foo",
            "mcpdoze add foo -- npx -y bar",
            "cargo test",
            "",
            "pwsh -c \"echo claude mcp add\"",
            "bash -c 'claude mcp list'",
        ];
        for c in allow {
            assert_eq!(bash(c), Decision::Allow, "should allow: {c}");
        }
    }

    fn write_event(tool: &str, path: &str, key: &str, text: &str) -> Value {
        json!({"tool_name":tool,"tool_input":{"file_path":path,key:text}})
    }

    fn powershell(cmd: &str) -> Decision {
        check_event(&json!({"tool_name":"PowerShell","tool_input":{"command":cmd}}))
    }

    #[test]
    fn powershell_tool_is_inspected_like_bash() {
        let deny = [
            "claude mcp add foo -- npx -y bar",
            "& claude mcp add foo -- x",
            "& 'C:\\Program Files\\x\\claude.exe' mcp add foo -- x",
            "& \"$env:APPDATA\\npm\\claude.cmd\" mcp add foo -- x",
            "Invoke-Expression 'claude mcp add foo -- x'",
            "iex \"claude mcp add foo -- x\"",
            "'claude mcp add foo -- x' | iex",
            "$c = 'codex mcp add foo -- x'; Invoke-Expression $c",
            "Invoke-Command -ScriptBlock { claude mcp add foo -- x }",
            "Start-Process claude -ArgumentList 'mcp','add','foo'",
            "Start-Process -FilePath claude.exe -ArgumentList \"mcp add foo\"",
            "$(claude mcp add foo -- x)",
            "c`laude mcp add foo -- x",
            ". claude mcp add foo -- x",
            "pwsh -NoProfile -Command \"& { claude mcp add foo -- x }\"",
            "Set-Location C:\\p; claude mcp add foo -- x",
        ];
        for c in deny {
            assert_eq!(powershell(c), Decision::Deny, "should deny: {c}");
        }
        let allow = [
            "claude mcp list",
            "Write-Output 'claude mcp add x'",
            "Get-ChildItem",
            "Select-String 'claude mcp add' README.md",
            "iex 'Get-Date'",
            "mcpdoze add foo -- npx -y bar",
            "Start-Process notepad",
            "",
        ];
        for c in allow {
            assert_eq!(powershell(c), Decision::Allow, "should allow: {c}");
        }
    }

    #[test]
    fn mcp_json_content() {
        let only_broker = r#"{"mcpServers":{"mcpdoze":{"type":"http","url":"http://127.0.0.1:7878/mcp","headers":{"Authorization":"Bearer ${MCPDOZE_TOKEN}"}}}}"#;
        let with_cmd = r#"{"mcpServers":{"mcpdoze":{"url":"http://127.0.0.1:7878/mcp"},"foo":{"command":"npx","args":["x"]}}}"#;
        let remote = r#"{"mcpServers":{"foo":{"type":"http","url":"https://example.test/mcp"}}}"#;
        let fake_broker = r#"{"mcpServers":{"mcpdoze":{"url":"https://example.test/mcp"}}}"#;
        let empty = r#"{"mcpServers":{}}"#;
        let allow = [
            write_event("Write", "/p/.mcp.json", "content", only_broker),
            write_event("Write", "C:\\p\\mcp.json", "content", empty),
            write_event("Write", "/p/.mcp.json", "content", "not json at all"),
            write_event("Write", "/p/other.json", "content", with_cmd),
            write_event(
                "Edit",
                "/p/package.json",
                "new_string",
                "\"command\": \"x\"",
            ),
            write_event("Edit", "/p/.mcp.json", "new_string", "\"headers\": {}"),
        ];
        for e in &allow {
            assert_eq!(check_event(e), Decision::Allow, "{e}");
        }
        let deny = [
            write_event("Write", "/p/.mcp.json", "content", with_cmd),
            write_event("Write", "/p/.MCP.json", "content", remote),
            write_event("Write", "/p/mcp.json", "content", fake_broker),
            write_event(
                "Edit",
                "/p/.mcp.json",
                "new_string",
                "\"foo\": {\"command\": \"npx\"}",
            ),
            write_event(
                "Edit",
                "/p/.mcp.json",
                "new_string",
                "\"url\": \"https://example.test/x\"",
            ),
            json!({"tool_name":"MultiEdit","tool_input":{"file_path":"/p/.mcp.json","edits":[{"old_string":"a","new_string":"{\"command\":\"x\"}"}]}}),
        ];
        for e in &deny {
            assert_eq!(check_event(e), Decision::Deny, "{e}");
        }
    }

    #[test]
    fn unrelated_or_malformed_events_allow() {
        for e in [
            json!({}),
            json!(null),
            json!("x"),
            json!({"tool_name":"Read","tool_input":{"file_path":"/p/.mcp.json"}}),
            json!({"tool_name":"Bash"}),
            json!({"tool_name":"Bash","tool_input":{"command":5}}),
            json!({"tool_name":7,"tool_input":{}}),
        ] {
            assert_eq!(check_event(&e), Decision::Allow, "{e}");
        }
    }

    #[test]
    fn deny_reason_mentions_remedy() {
        assert!(DENY_REASON.contains("mcpdoze add"));
        assert!(DENY_REASON.contains("MCPDOZE_HOOK_BYPASS=1"));
    }
}
