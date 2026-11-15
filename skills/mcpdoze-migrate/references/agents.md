# Supported agents

`mcpdoze import <agent>` reads the agent's MCP configuration, registers each
server in the MCPDoze config, and replaces the originals with a single
`mcpdoze` HTTP entry. Entries that already point at the broker are skipped, so
re-running is safe.

| Agent | Files read | Token header written |
|---|---|---|
| `claude-code` | `~/.claude.json` (`mcpServers`, `projects.<path>.mcpServers`) and `./.mcp.json` | `Authorization: Bearer ${MCPDOZE_TOKEN}` |
| `codex` | `~/.codex/config.toml` `[mcp_servers.*]` (command, args, env, url, `http_headers`, `env_http_headers`, `bearer_token_env_var`) | `bearer_token_env_var = "MCPDOZE_TOKEN"` |
| `opencode` | `opencode.json` `mcp` map (`local` / `remote`, command array, environment, url, headers) | `Bearer {env:MCPDOZE_TOKEN}` |
| `cursor`, `vscode` | `mcp.json`, `.vscode/mcp.json` (`mcpServers` / `servers`) | `Bearer ${env:MCPDOZE_TOKEN}` |

Notes:

- OpenCode files containing comments are skipped rather than rewritten (a
  rewrite would drop the comments); migrate those by hand using
  `mcpdoze add`.
- Claude Code plugins: servers of enabled plugins (`enabledPlugins` in
  `~/.claude/settings.json` and the project's `.claude/settings*.json`;
  install paths from `~/.claude/plugins/installed_plugins.json`; servers
  from the plugin's `.mcp.json`, `.claude-plugin/plugin.json` `mcpServers`
  or its marketplace entry) are registered with the broker too. Plugin
  files are never changed, backed up or rolled back. `${CLAUDE_PLUGIN_ROOT}`
  and `${CLAUDE_PLUGIN_DATA}` are expanded; `${CLAUDE_PROJECT_DIR}` becomes
  `${cwd}` and the server `per-cwd`. Disabled plugins and servers using
  `${user_config.*}` are skipped. An unknown registry layout is skipped with
  a warning. After the import the user must disable each plugin server in
  the agent (`/mcp`, shown as `plugin:<plugin>:<server>`) or disable the
  plugin; otherwise its tools appear twice (`mcp__plugin_<plugin>_<server>__*`
  and `mcp__mcpdoze__<prefix>__*`). A repeated import lists them as already
  registered.
- Claude Code `headersHelper` (http entries) is copied to the broker's
  `headers_helper` and run by the broker, never by the import (not even with
  `--dry-run`). Helpers from project-scoped files are skipped unless
  `--allow-helpers` is given. See docs/CONFIGURATION.md, "Headers helper".
- Instead of the `MCPDOZE_TOKEN` variable, a Claude Code entry may use
  `"headersHelper": "mcpdoze token --headers-json"`, which prints
  `{"Authorization":"Bearer <token>"}` from the token file or
  `broker.token_env`.
- Literal secret values are never migrated. They are reported by key name so
  the user can move them into an environment variable.
- Agents not listed: register servers manually with
  `mcpdoze add <name> [--env KEY=ENVVAR] -- <command> [args...]` and point the
  agent at `http://127.0.0.1:7878/mcp` with an `Authorization: Bearer` header
  taken from the `MCPDOZE_TOKEN` environment variable.
