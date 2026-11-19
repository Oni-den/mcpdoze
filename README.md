<p align="center">
  <img src="assets/icon.png" alt="MCPDoze: a sleepy robot dozing on top of a server" width="160">
</p>

# MCPDoze

**One shared, lazy, idle-unloading instance of every MCP server.**

MCPDoze is a small Rust broker that sits between your AI agents and your
[Model Context Protocol](https://modelcontextprotocol.io) servers.

> Status: **v0.2.1, early release.** The data path, lifecycle, catalog,
> admin API, CLI, HTTP passthrough, `shared` / `per-cwd` / `per-session`
> scopes, routing of server-to-client requests (sampling, elicitation,
> roots), migration skill with rollback and the optional guardrail hook work.
> CI tests Windows (x64 and ARM64), Linux and macOS, and a release workflow
> builds the binaries. On main for v0.2: the stateless MCP 2026-07-28
> protocol alongside 2025-11-25 (see
> [decision 0002](docs/decisions/0002-stateless-protocol-support.md)) and
> shared resource subscriptions ([SUBSCRIPTIONS.md](docs/SUBSCRIPTIONS.md)). See
> [docs/DEVELOPMENT_PLAN.md](docs/DEVELOPMENT_PLAN.md) and the measured
> [case study](docs/CASE_STUDY.md): two sessions, 741 MB without MCPDoze
> versus 394 MB with it, and about 6 MB after the idle unload.

## The problem

Stdio MCP servers are launched per agent session. Open three agent sessions and
you get three copies of every server, and some of them (Node-based servers
started through `npx`, browsers, language servers) use a lot of memory. The
memory stays allocated even when the tools are idle.

## What MCPDoze does

- **One HTTP endpoint.** Agents connect to a single local Streamable HTTP MCP
  endpoint and see the combined tool list of every server registered behind it.
- **One instance per server.** Any number of agent sessions share a single
  backend process.
- **Lazy start.** A backend is spawned the first time one of its tools is
  called. The tool list is served from a cache, so listing tools starts nothing.
- **Idle unload (TTL).** After a configurable idle period with no calls the
  whole process tree is terminated and its memory returned. The next call
  starts it again.
- **HTTP passthrough.** Remote (HTTP) MCP servers can be registered too, so
  connections are shared as well.
- **Migration skill.** An agent skill moves your existing MCP configuration
  behind the broker, with backup and rollback.
- **Failure diagnostics.** Off by default. When enabled, `mcpdoze debug`
  records why a brokered server failed, as shape and size data with no
  argument or result values, and exports a report that is designed to be safe
  to attach to an issue. See [docs/DEBUG.md](docs/DEBUG.md).
- **Opt-in token optimisation.** Per-server settings can trim tool lists,
  shrink results, cache read-only calls and cut oversize output with on-demand
  retrieval. Nothing changes unless configured. See
  [docs/OPTIMIZE.md](docs/OPTIMIZE.md).

## Results

Measured with [`scripts/case-study.ps1`](scripts/case-study.ps1): two agent
sessions, each with its own copy of two MCP servers (a Node.js server started
through `npx`, and a small native server), versus the same two sessions sharing
one MCPDoze broker. Total private memory of the whole process tree, one
read-only tool call per server per session, idle TTL set to 20 s for the test:

| Scenario | Processes | Private memory |
|---|---:|---:|
| Two sessions, each with its own copies | 14 | 741 MB |
| Two sessions through MCPDoze | 9 | 394 MB |
| After the idle TTL | 2 | 6 MB |

That is about 47% less memory with two sessions, no extra processes for the
second session, and the memory handed back once the servers go idle. The
saving grows with every additional session and with heavier servers. One run
on one machine; see [docs/CASE_STUDY.md](docs/CASE_STUDY.md) for method and
caveats and run the script with your own servers.

## Quick start

**1. Install.** Download the archive for your platform from the repository's
Releases page (it contains the `mcpdoze` binary, on Windows also the
console-less `mcpdozed.exe` used for autostart, `LICENSE` and the migration
skill) and put the binaries on your `PATH`, or build from source (Rust
stable; on Linux also `pkg-config` and `libssl-dev`):

```text
cargo install --git https://github.com/Oni-den/mcpdoze mcpdoze
```

**2. Run the broker.** It listens on `127.0.0.1:7878` and creates an
owner-only token file on first start:

```text
mcpdoze serve
```

To start it at login instead (Windows logon task, systemd user unit on Linux,
LaunchAgent on macOS; no administrator rights): `mcpdoze autostart install`.
On Windows the task runs `mcpdozed.exe`, the same broker without a console
window, which logs to `<data dir>/logs/mcpdozed.log` (`mcpdoze doctor` shows
where). Remove it with `mcpdoze autostart remove`.

**3. Register a server.** Everything after `--` is the server's command. Pass
secrets as the *name* of an environment variable, never the value:

```text
mcpdoze add example --env API_KEY=EXAMPLE_API_KEY -- npx -y some-mcp-server
mcpdoze add remote --transport http --url https://example.com/mcp --header Authorization=REMOTE_AUTH
```

The broker's config file is updated and the server is registered without a
restart. `mcpdoze list` shows what is configured, `mcpdoze remove <name>`
removes a server.

**4. Check it.**

```text
mcpdoze status       # state, memory and idle countdown per backend
mcpdoze doctor       # checks config, token, port, commands, env vars
mcpdoze stop <name>  # unload a backend now (starts again on the next call)
mcpdoze logs <name>  # last stderr lines of a backend
mcpdoze stats        # calls, latency, cold starts, memory saved (last 7 days)
```

`mcpdoze stats` summarises the broker's statistics log (on by default, no
arguments or paths recorded): it compares the broker's real memory with an
estimate of running every server inside each session. See
[docs/STATS.md](docs/STATS.md).

**5. Point your agent at it.** Agents authenticate with the broker token, read
from the `MCPDOZE_TOKEN` environment variable. `mcpdoze token` prints the
token file's path (never the token); load that file into the variable in your
own shell profile, then add the single entry. For Claude Code:

```text
claude mcp add --transport http mcpdoze http://127.0.0.1:7878/mcp --header "Authorization: Bearer ${MCPDOZE_TOKEN}"
```

Tools appear as `mcp__mcpdoze__<prefix>__<tool>` in Claude Code, where the
prefix defaults to the server name (see `--prefix`). The backend starts on the
first call and is unloaded after the idle TTL (10 minutes by default).

**Already have MCP servers configured?** The `mcpdoze-migrate` skill in
[`skills/mcpdoze-migrate`](skills/mcpdoze-migrate/SKILL.md) moves them behind
the broker and routes later "install an MCP server" requests to `mcpdoze add`.
It drives `mcpdoze import <agent> --dry-run`, then `--yes`, with a backup and
a rollback (`mcpdoze import --rollback <manifest>`). Supported: `claude-code`,
`codex`, `opencode`, `cursor`, `vscode`.

**Optional guardrail.** `mcpdoze hook install --scope user --dry-run` previews
a Claude Code hook that blocks native `claude mcp add` style installs and points
the agent at `mcpdoze add`. See [docs/HOOK.md](docs/HOOK.md).

More: [configuration](docs/CONFIGURATION.md),
[troubleshooting](docs/TROUBLESHOOTING.md),
[architecture](docs/ARCHITECTURE.md), [admin API](docs/ADMIN_API.md),
[statistics](docs/STATS.md).

## Security model (summary)

Loopback bind by default, `Host`/`Origin` validation against DNS rebinding,
bearer-token authentication, and backend secrets referenced by environment
variable name rather than stored in configuration. Details in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). Contributions are accepted under the
same license with a DCO sign-off; see [CONTRIBUTING.md](CONTRIBUTING.md).
