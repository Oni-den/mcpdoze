# MCPDoze development plan

Decisions: name `mcpdoze`; default idle TTL 10 minutes (per-server override);
accept both the 2025-11-25 stateful and 2026-07-28 stateless MCP protocols from
agents if cheap (decided at the M1 spike, otherwise stateful first); DCO
contributions; GPL-3.0-or-later.

## Architecture

**Crates.** `rmcp` (official Rust MCP SDK: types, child-process and Streamable
HTTP transports) underneath a hand-written router that works on raw JSON-RPC
(a proxy must forward unknown methods and remap ids). `tokio`, `axum`, `serde`,
`toml`/`toml_edit`, `clap`, `tracing`, `windows-sys` (Job Objects), `nix`
(process groups), `sysinfo`, `notify`, `subtle`. Spike gate in M1: if `rmcp`
cannot pass raw messages cleanly, use it for types only and hand-roll stdio
framing (newline-delimited JSON).

**Workspace.** `crates/mcpdoze-core` (config, registry, catalog, backend
{stdio, http, lifecycle, proctree}, router, auth), `crates/mcpdoze` (binary:
serve, cli, admin, autostart), `crates/mcpdoze-testkit` (mock stdio/HTTP MCP
servers), `skills/mcpdoze-migrate/`, `docs/`.

**Config** (TOML in the platform config dir; `--config` / `MCPDOZE_CONFIG`):

```toml
[broker]
bind = "127.0.0.1:7878"
token_file = "token"            # or token_env
default_idle_ttl = "10m"

[servers.example]
transport = "stdio"             # stdio | http
command = "npx"
args = ["-y", "some-mcp-server"]
env = { API_KEY = { from_env = "EXAMPLE_API_KEY" } }
scope = "shared"                # shared | per-cwd | per-session
idle_ttl = "5m"
prefix = "example"
```

The file is the source of truth and is hot-reloaded. Runtime registration goes
through an admin REST API (`/admin/v1/servers`) guarded by the same token; the
CLI is a thin client of it.

**Namespacing.** Tools are exposed as `<prefix>__<tool>`; prefixes are unique
(enforced at `add`), sanitised to `[a-z0-9_]`, total length capped at 64 with a
short hash suffix on overflow (mapping kept in the catalog).

**Tool list without spawning.** A per-server on-disk catalog (tools, prompts,
resource metadata, capabilities, fingerprint of command/args/env names/URL).
`add` probes once (spawn, list, stop). Refresh on fingerprint change, on every
spawn (background diff), on backend `list_changed`, or `mcpdoze refresh`.
A non-empty diff sends `list_changed` to stateful agent sessions. Prompts and
resources are listed/fetched through the same prefixing in v1; resource
subscriptions came in v0.2 (`SUBSCRIPTIONS.md`).

**Sessions and scopes.** Agent sessions are broker-side objects. Backend
instances are keyed by `(server, scope_key)`: `shared` (default, one instance
for everyone), `per-cwd` (key from the agent's roots, or `X-Mcpdoze-Cwd`;
expands `${cwd}` in args, covering servers launched with a directory argument),
`per-session` (stateful servers; still lazy and TTL-managed). The backend sees
one synthetic client, the broker.

**Concurrency.** One writer task and one reader task per backend. Each agent
request gets a fresh broker id; a map `backend_id -> (session, original_id,
progress_token)` is dropped on response. Progress tokens are rewritten and
routed by token. Agent `cancelled` notifications are translated; a disconnect
cancels that agent's in-flight calls. Server-to-client requests (sampling,
elicitation, roots) route to the session that owns the in-flight call, and
return "not supported" when ambiguous.

**Lifecycle** per instance: `Stopped -> Starting -> Running -> Draining ->
Stopping -> Stopped`, plus `Failed(backoff)`. An RAII guard counts in-flight
calls; the idle timer arms at zero and any new call cancels it; the TTL check and
the move to Draining are one atomic step under the instance lock, so a racing
call either keeps it Running or waits and triggers a respawn. Start timeout 60 s
(cold `npx`). Stop: close stdin, 3 s grace, then kill the tree. Crashes fail
in-flight calls and respawn lazily with exponential backoff (1 s to 60 s).

**Process trees.** Windows: spawn suspended, assign to a Job Object with
`KILL_ON_JOB_CLOSE`, resume; unload with `TerminateJobObject`; resolve `.cmd`
shims explicitly. Unix: new session/process group, `killpg` SIGTERM then
SIGKILL, `PR_SET_PDEATHSIG` on Linux. Broker exit leaves no orphans.

**Security.** Bind 127.0.0.1 by default (other binds need `--allow-remote` and a
token). Validate `Host` and `Origin` on every request (DNS rebinding). Bearer
token generated on first run into an owner-only file, constant-time compare,
never logged. Backend secrets are `{ from_env = "NAME" }` references; `add`
refuses literal secret-looking values without `--allow-literal-env`. Admin API
is loopback-only regardless of bind. Warn on group/world-readable config.

**HTTP passthrough.** One Streamable HTTP client per remote with a shared
pool; header injection from env references; SSE streamed unbuffered; reconnect
with backoff and re-initialise on upstream 404. Idle TTL means dropping the
upstream session. OAuth-flow remotes are out of scope for v1.

**CLI.** `serve`, `status` (state, process-tree RSS/private bytes, in-flight,
idle countdown, restarts), `list`, `add`, `remove`, `reload`, `stop`,
`refresh`, `logs`, `token`, `doctor`, `autostart install|remove`, `import`.

**Autostart.** User-level service: Task Scheduler logon task (Windows, no
admin), systemd user unit (Linux), LaunchAgent (macOS). A stdio-to-HTTP shim
(`mcpdoze connect`) is a v0.2 fallback for agents without HTTP MCP support.

## Milestones

| M | Deliverables | Acceptance |
|---|---|---|
| M0 | Workspace, LICENSE, SPDX headers, README, CONTRIBUTING, SECURITY, CI (fmt, clippy, test on Windows x64/ARM64, Linux, macOS; `cargo-deny`; DCO check), PII lint | CI green; lint fails on user paths/usernames |
| M1 | `rmcp` spike, config, process-tree module, testkit mock | Raw-passthrough decision recorded; 3-deep tree kill passes on Windows and Unix |
| M2 | HTTP endpoint, one stdio backend, namespacing, id remap, lazy spawn | Two clients, one backend pid |
| M3 | Lifecycle FSM, TTL, backoff, cancellation, progress | Race tests pass; never unloads with a call in flight |
| M4 | Disk catalog, probe, refresh, `list_changed`, prompts/resources | `tools/list` answered with zero backends running |
| M5 | Auth, Host/Origin checks, secret refs, admin API and CLI, memory status | Rebinding attempt rejected; token never in logs |
| M6 | HTTP passthrough | Mock remote: SSE streamed, reconnect works |
| M7 | `per-cwd` and `per-session` scopes, server-to-client routing | Elicitation reaches the right session |
| M8 | Autostart, migration skill, case study, soak, release binaries | Case-study numbers published; v0.1.0 tagged |

Sequence: M0, M1, then M2 and M3 are sequential (core router and lifecycle).
After M2 interfaces freeze, M4, M5, M6, the skill and docs run in parallel.
M7 follows M3. M8 is last.

## Status

Updated from the code and git history. Note: `rmcp` is not used; the router is
hand-written (see `decisions/0001-rmcp-vs-handrolled.md`), so the Architecture
paragraph above is historical.

| M | State | Notes |
|---|---|---|
| M0 | done | Workspace, license, CI, DCO check, PII lint, cargo-deny. |
| M1 | done | Config, process-tree module, testkit mock; raw-passthrough decision recorded. |
| M2 | done | HTTP endpoint, stdio backend, namespacing, id remap, lazy spawn. |
| M3 | done | Lifecycle, idle TTL, cancellation, progress. |
| M4 | done | Disk catalog, probe, refresh, `list_changed`, prompts and resources. |
| M5 | done | Auth, Host/Origin checks, secret refs, admin API and CLI. |
| M6 | done | HTTP passthrough. |
| M7 | done | `per-cwd` / `per-session` scopes and server-to-client routing (sampling, elicitation, roots). |
| M8 | done | Autostart, migration skill (surgical rollback), hook and case study done; release workflow builds the binaries; Linux/macOS covered by CI; version 0.1.0. Open: the nightly soak run. The v0.2 items (the stateless 2026-07-28 protocol, decisions/0002; resource subscriptions, SUBSCRIPTIONS.md) are implemented on main. |


## Hardest problems

1. TTL versus incoming-call race: single lock-guarded transition, RAII guards,
   `loom`/property tests.
2. Windows tree kill through `npx` shims and ARM64 emulation: Job Object before
   resume, CI on both architectures, `doctor` check.
3. Servers that assume a single client (state leaks, server-initiated
   requests): opt-in scopes, conservative capability advertisement, published
   compatibility list.
4. Two protocol generations: terminate the protocol on both sides, pin the
   `rmcp` version, per-version conformance tests.
5. Stale catalogs (tools vary by env/cwd or change under `npx -y`): fingerprint,
   refresh on every spawn, `list_changed`, error-triggered refresh.

Other risks: `rmcp` API churn, agents ignoring `list_changed`, cold start
exceeding agent tool timeouts (`prewarm = true` option).

## Migration skill (`skills/mcpdoze-migrate`)

```text
SKILL.md
scripts/discover.(ps1|sh)    # locate configs, emit JSON inventory
scripts/migrate.(ps1|sh)     # thin wrappers over `mcpdoze import`
references/agents.md         # config locations and formats per agent
references/rollback.md
```

Logic lives in the binary (`mcpdoze import <agent> --dry-run`) so the
skill stays thin and testable. Steps: check the broker is installed and running;
discover user, project and local MCP configs; show a dry-run plan (transport,
prefix, scope, env var names only); confirm; back up every touched file and
write a manifest; register each server with `mcpdoze add`; remove the
originals and add the single broker entry; verify with a test call; offer
rollback (`mcpdoze import --rollback <manifest>`). The skill warns that tool
names gain a prefix and offers to rewrite permission allowlists. Literal secrets
are listed by name for the user to relocate, never echoed or moved silently.
Adapters: Claude Code first, then Codex, OpenCode, Cursor/VS Code.

## Case study (acceptance for v0.1)

Two agent sessions open, each running its own copies of a Node-based MCP server
(started through `npx`, about 75 MB private memory per node process plus shell
wrappers) and a small Rust MCP server. With MCPDoze: both sessions attach to
one broker; exactly one instance of each backend runs; after the idle TTL the
count is zero; a script reports private memory before, during and after.

## Testing

Testkit mock stdio server with scriptable behaviour (delays, crashes,
`list_changed`, sampling requests, grandchild processes, stdout garbage). Unit
and property tests for the id map and state machine. Integration: two clients,
one pid; TTL to zero; no unload mid-call; crash backoff. Tree-kill tests on all
CI targets. Nightly soak of 1,000 spawn/unload cycles asserting no handle or
memory growth in the broker.
