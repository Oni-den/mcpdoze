# MCPDoze

**One shared, lazy, idle-unloading instance of every MCP server.**

MCPDoze is a small Rust broker that sits between your AI agents and your
[Model Context Protocol](https://modelcontextprotocol.io) servers.

> Status: **pre-alpha.** The data path, lifecycle, catalog, admin API, CLI,
> HTTP passthrough, migration skill and optional guardrail hook work and are
> tested on Windows. Not yet done: `per-cwd` / `per-session` scopes
> (currently served as `shared`), routing of server-to-client requests such
> as sampling, release binaries, and Linux/macOS verification. See
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

## Planned usage

```text
mcpdoze serve                 # run the broker (127.0.0.1 only by default)
mcpdoze add <name> -- <cmd>   # register a stdio server
mcpdoze status                # state, memory and idle countdown per backend
mcpdoze stop <name>           # unload a backend now
```

Agents then use one entry, for example in Claude Code:

```text
claude mcp add --transport http mcpdoze http://127.0.0.1:7878/mcp
```

## Security model (summary)

Loopback bind by default, `Host`/`Origin` validation against DNS rebinding,
bearer-token authentication, and backend secrets referenced by environment
variable name rather than stored in configuration. Details in the plan.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). Contributions are accepted under the
same license with a DCO sign-off; see [CONTRIBUTING.md](CONTRIBUTING.md).
