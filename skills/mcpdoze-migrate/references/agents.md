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
- Literal secret values are never migrated. They are reported by key name so
  the user can move them into an environment variable.
- Agents not listed: register servers manually with
  `mcpdoze add <name> [--env KEY=ENVVAR] -- <command> [args...]` and point the
  agent at `http://127.0.0.1:7878/mcp` with an `Authorization: Bearer` header
  taken from the `MCPDOZE_TOKEN` environment variable.
