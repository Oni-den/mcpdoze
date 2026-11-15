# Case study: two agent sessions, duplicate MCP servers

## Setup

Two AI agent sessions were open at the same time on a Windows 11 laptop. Each
session had started its own copy of two MCP servers:

- a Node.js MCP server launched through `npx` (a `cmd.exe` wrapper, `npm`/`npx`
  and a `node` process per copy), and
- a small native (Rust) MCP server.

`scripts/case-study.ps1` reproduces the comparison without touching any agent
or MCPDoze configuration: it runs everything in a temporary directory, makes
one real read-only tool call per server per session, discards the tool output,
and sums the **private memory** of the whole process tree.

```text
pwsh -File scripts/case-study.ps1 -Binary <path to mcpdoze> `
     -ServersFile servers.json -Sessions 2 -TtlSeconds 20
```

`servers.json` is a list of `{ name, command, args, tool, arguments }` entries.

## Result

Idle TTL 20 s (the default is 10 minutes), two sessions.

| Scenario | Processes | Private memory (MB) |
|---|---:|---:|
| Baseline: 2 sessions, each with its own copy of both servers | 14 | 740.7 |
| MCPDoze: broker started, no calls yet | 2 | 4.1 |
| MCPDoze: 2 sessions active, 4 tool calls made | 9 | 394.1 |
| MCPDoze: after the idle TTL | 2 | 5.7 |

- With two sessions, MCPDoze used about **47% less memory** (394 MB vs 741 MB)
  and half the processes (7 backend processes instead of 14).
- Each session added **no** processes: the second session's calls were served
  by the instance the first session started.
- After the idle TTL the backends were unloaded and the whole tree dropped to
  **about 6 MB**, the broker itself. A baseline session keeps its copies for
  as long as the session lives.
- The saving grows with the number of sessions: the baseline scales linearly
  with sessions, MCPDoze stays flat.

The "2 processes" at idle are the broker plus its console host.

## Caveats

- One machine, one run, small sample of servers. Memory figures vary by
  server version and workload; run the script with your own servers.
- Private bytes were summed per process tree, so shared pages are not
  double-counted away; working-set figures would look smaller for both
  scenarios.
- The tool calls were deliberately light. Servers that grow with use (browsers,
  language servers) save more.
