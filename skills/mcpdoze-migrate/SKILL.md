---
name: mcpdoze-migrate
description: Route all MCP server installs through the MCPDoze broker and migrate existing ones. Use whenever the user asks to install, add, set up, or configure an MCP server for an agent (even if they name the agent's own command such as `claude mcp add`, `codex mcp add`, or say "add this to .mcp.json"), and when they want to stop duplicate MCP processes across sessions, reduce MCP memory use, move existing MCP servers behind MCPDoze, or roll a migration back.
---

# Migrate MCP servers behind MCPDoze

MCPDoze is a local broker: agents talk to one HTTP endpoint and the broker
starts each MCP server on first use, shares one instance between all agent
sessions, and unloads it after an idle period. This skill moves an agent's
existing MCP configuration behind it, safely and reversibly. The logic lives in
the `mcpdoze` binary (`mcpdoze import`); this skill drives it.

## Rules

- Never print, copy or write the broker token or any secret value. Refer to
  the token file by path and to secrets by environment-variable name.
- Always run a dry run first, show the plan, and get the user's confirmation
  before applying. Apply only with the user's explicit yes.
- Applying changes agent config files. The tool backs each one up and writes a
  rollback manifest; report the manifest path to the user.
- Do not edit agent config files by hand when `mcpdoze import` supports the
  agent.

## Re-route "install an MCP server" requests

When the user asks to install, add or configure any MCP server for an agent
(`claude mcp add ...`, `codex mcp add ...`, editing `.mcp.json`, `mcp.json`,
`config.toml` or `opencode.json`, "add the X MCP server", pasting an
`npx`/`uvx` MCP command or a server URL), do NOT register it with the agent
directly. A per-agent registration spawns a private copy of the server for every
session, which is what MCPDoze exists to prevent. Instead:

1. Tell the user in one line that you are registering it with MCPDoze so it
   stays a single shared instance.
2. Make sure the broker is set up: `mcpdoze doctor`, and that the agent has the
   single `mcpdoze` entry (`mcpdoze import <agent> --dry-run` reports nothing to
   migrate and `mcpdoze status` answers). If not, run the migration steps below
   first.
3. Register the server with the broker:
   - stdio: `mcpdoze add <name> [--env KEY=ENVVAR] [--cwd DIR] -- <command> [args...]`
   - remote HTTP: `mcpdoze add <name> --transport http --url <url> [--header NAME=ENVVAR]`
   - Secrets are passed as the *name* of an environment variable (`KEY=ENVVAR`),
     never as a value. If the user gives a literal key, ask them to export it
     as an environment variable and use that name; do not repeat the value.
   - Pick a short prefix (`--prefix`) if the name is long; use `--scope per-cwd`
     for servers that take a project directory argument and `--ttl` to override
     the idle period.
4. Confirm with `mcpdoze status`, tell the user the tools appear as
   `mcp__mcpdoze__<prefix>__<tool>` (the agent may need a restart or a tool-list
   refresh), and that the server starts on first use.
5. Only fall back to the agent's native command if the user explicitly insists,
   or the server cannot run behind a broker (for example it needs per-session
   sampling); say why.

## Steps (migration)

1. **Check the install.** Run `mcpdoze --version`. If missing, tell the user
   how to install it (release binary from the repository's Releases page, or
   `cargo install --git https://github.com/Oni-den/mcpdoze mcpdoze`) and stop
   until it is available.
2. **Check health.** Run `mcpdoze doctor`. Fix reported problems with the
   user. If the broker is not running, offer `mcpdoze autostart install`
   (a per-user logon task / systemd user unit / LaunchAgent, no admin rights)
   and `mcpdoze serve` for a one-off run.
3. **Pick the agent.** One of `claude-code`, `codex`, `opencode`, `cursor`,
   `vscode`. Ask the user if unclear.
4. **Dry run.** `mcpdoze import <agent> --dry-run` (add `--json` if you need to
   parse it). Present the plan to the user: each server, transport, proposed
   prefix and scope, and warnings. Explain:
   - Tool names gain a prefix, so permission allowlists that name tools such
     as `mcp__<server>__<tool>` must be updated to the new
     `mcp__mcpdoze__<prefix>__<tool>` form.
   - Servers marked `per-cwd` depend on the working directory (arguments such
     as `--dir .`); confirm that is right.
   - Entries reported as `needs_env_var` contained literal secrets. These are
     NOT migrated automatically. Ask the user to put the secret in an
     environment variable they choose and re-run; never ask them to paste the
     secret into the chat.
5. **Token environment variable.** Agents read the broker token from the
   `MCPDOZE_TOKEN` environment variable. `mcpdoze token` prints the token file
   *path* only. Tell the user to set `MCPDOZE_TOKEN` from that file in their
   own shell or profile (they run it; you do not read the file), then restart
   the agent so it picks the variable up.
6. **Apply.** After the user confirms: `mcpdoze import <agent> --yes`. Report
   the backup files and manifest path from the output.
7. **Verify.** `mcpdoze status` lists the registered servers (all idle until
   first use). Ask the user to restart the agent and call one tool. Then
   `mcpdoze status` again should show one running backend with a process id,
   and after the idle period (default 10 minutes) it returns to zero.
8. **Roll back if needed.** `mcpdoze import --rollback <manifest>` restores
   the original files and removes the registered servers. It refuses if a
   file changed since the migration; explain that and let the user decide.

## Reference

- `references/agents.md`: where each agent keeps its MCP config, what the tool
  reads and writes, and how the token header is expressed per agent.
- `references/rollback.md`: manifest format and manual recovery.
- `scripts/discover.ps1`, `scripts/discover.sh`: run a dry run for every
  supported agent and summarise, without changing anything.

## Optional guardrail hook

Claude Code can be told to block native MCP installs (`claude mcp add`,
`codex mcp add`, `.mcp.json` edits that define servers) and point the agent at
`mcpdoze add` instead. Offer it once; never install it unasked:

1. Preview: `mcpdoze hook install --scope user --dry-run` and show the output.
2. After the user's explicit yes: `mcpdoze hook install --scope user --yes`.
   Report the backup path it prints.
3. To undo: `mcpdoze hook remove --scope user --yes`.

It is a guardrail on command text, not a security boundary; the user can
bypass it with `MCPDOZE_HOOK_BYPASS=1`. Details: `docs/HOOK.md`.
