# Claude Code guardrail hook

An optional `PreToolUse` hook that stops an agent from registering MCP servers
natively (which starts one copy per session) and tells it to use
`mcpdoze add` instead.

## What it blocks

- **Bash** commands that run `claude mcp add`, `add-json` or
  `add-from-claude-desktop`, `codex mcp add`, `gemini mcp add`,
  `opencode mcp add`, or `code --add-mcp`. Matching is case-insensitive, allows
  flags and `.exe`/`.cmd` suffixes, and sees commands inside `&&`, `||`, `;`,
  `|` chains and one or more `pwsh -c "..."` / `bash -c '...'` wrappers, after
  `sudo`, `env`, `time`, `npx` and `VAR=value` prefixes.
- **Write / Edit / MultiEdit** of a file named `.mcp.json` or `mcp.json` whose
  new content defines a server with a `command`, or a `url` that is not the
  local broker entry named `mcpdoze`.

Not blocked: `claude mcp list|get|remove|serve`, mentions of these commands
inside `echo`, comments or `grep` arguments, and every other tool call.

When it blocks, the agent receives a fixed message explaining how to use
`mcpdoze add`. The blocked command text is never echoed back.

## Install and remove

```
mcpdoze hook install --scope user --dry-run   # preview
mcpdoze hook install --scope user --yes       # apply
mcpdoze hook remove  --scope user --yes
```

Options: `--scope user|project` (`<home>/.claude/settings.json` or
`./.claude/settings.json`), `--settings PATH` to name a file, `--dry-run`,
`--yes` (otherwise an interactive confirmation is required).

Install adds one `hooks.PreToolUse` entry (matcher `Bash|Write|Edit|MultiEdit`,
command `"<path to mcpdoze>" hook claude-code`). Re-running it updates the path
instead of duplicating. Remove deletes only that entry. Other settings and key
order are preserved, an existing file is copied to
`<file>.mcpdoze-bak-<UTC timestamp>` first, the write is atomic, and a file
that is not valid JSON is refused and left untouched.

## Bypass

Set `MCPDOZE_HOOK_BYPASS=1` in the agent's environment to allow everything.
Use it only if you really want a native install.

## Limits

This is a guardrail, not a security boundary. It looks at command text only: a
command assembled at runtime, run from a script file, or otherwise obscured is
not seen, and it does not watch other agents' config files. Malformed or
unrecognised hook input is always allowed; the hook never fails closed.
