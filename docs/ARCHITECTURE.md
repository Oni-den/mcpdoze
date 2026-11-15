# Architecture

MCPDoze is a single Rust process that exposes one local Streamable HTTP MCP
endpoint and manages the MCP servers (backends) behind it.

```text
  agent A --+                       +--> stdio backend  (child process tree)
  agent B --+--> HTTP /mcp --> broker --> stdio backend
  agent C --+    (bearer token)     |
                                    +--> HTTP backend   (remote MCP server)

  catalog on disk  <-- tools / prompts / resources lists
  /admin/v1        <-- mcpdoze CLI (status, add, stop, ...)
```

## Crates

- `mcpdoze-core`: config, auth, router, catalog, backends (stdio and HTTP),
  lifecycle, process-tree handling, admin API, import (migration).
- `mcpdoze`: the binary: `serve`, the admin-API client commands, `doctor`,
  `autostart`, `import`, and the optional Claude Code `hook`.
- `mcpdoze-testkit`: scriptable mock stdio and HTTP MCP servers for tests.

The MCP handling is a hand-written router over raw JSON-RPC; the `rmcp` SDK is
not used (see [decisions/0001-rmcp-vs-handrolled.md](decisions/0001-rmcp-vs-handrolled.md)).
A proxy must forward methods it does not know and remap request ids.

## Request path

1. An agent sends JSON-RPC over `POST /mcp` (JSON response mode), carrying an
   `Mcp-Session-Id` after `initialize`. `GET /mcp` opens an optional SSE stream
   for notifications; `DELETE /mcp` ends the session. MCP 2026-07-28
   clients are served statelessly on the same endpoint (per-request
   `_meta`, `server/discover`, `subscriptions/listen`); see
   [decisions/0002](decisions/0002-stateless-protocol-support.md).
2. The broker checks the bearer token, `Host` and `Origin`, then resolves the
   tool name `<prefix>__<tool>` to a backend and the original tool name.
3. Request ids are rewritten so many agents can share one backend and are
   mapped back in the reply. Progress tokens are rewritten the same way.
4. If the backend is not running it is started, the call is forwarded and the
   reply returned. Cancellation and client disconnects are propagated.

## Backend lifecycle

States reported by `mcpdoze status`: `stopped`, `starting`, `running`,
`draining`, `stopping`, `failed`.

```text
stopped -> starting -> running -> draining -> stopping -> stopped
              |
              +-> failed   (start error or timeout; a later call tries again)
```

- A call to a stopped backend starts it, bounded by `start_timeout`.
- The idle timer runs only while no call is in flight. On expiry the whole
  process tree is terminated (Windows Job Object, Unix process group) and its
  memory returned. The next call starts it again.
- `mcpdoze stop` drains in-flight calls (up to 30 s) unless `--force`.
- Reloading config restarts only servers whose launch settings changed.

## Catalog

Lists (`tools/list`, `prompts/list`, `resources/list`,
`resources/templates/list`) are answered from a per-server JSON file, so
listing starts nothing. A one-off probe fills it at registration; it is
refreshed after each spawn, on backend `list_changed` notifications, on
unknown-tool errors, on demand (`mcpdoze refresh`) and optionally by age. A
change reaches agents as `notifications/*/list_changed`. Resource URIs are
mapped to `mcpdoze://<prefix>/<encoded-uri>`. Details: [CATALOG.md](CATALOG.md).
Resource subscriptions are shared per backend instance and pin it against
idle unload: [SUBSCRIPTIONS.md](SUBSCRIPTIONS.md).

## Scopes

Each backend instance is keyed by `(server, scope_key)`: `shared` (one
instance for all agents, the default), `per-cwd` (one per agent working
directory) or `per-session` (one per agent session). Backend requests to the
client (sampling, elicitation, roots) are answered by the broker or routed to
the owning agent session over its event stream. See [SCOPES.md](SCOPES.md).

## Security model

- Binds to loopback by default; any other address needs `allow_remote = true`
  and an explicit token source.
- Every request needs the bearer token (constant-time comparison). The token
  file is created owner-only and the token is never logged.
- `Host` and `Origin` are validated against `localhost`, `127.0.0.1`, `[::1]`
  and `allowed_hosts` (DNS-rebinding defence).
- The admin API accepts loopback peers only, even on a remote bind.
- Backend secrets are referenced by environment variable name; secret-looking
  literals are rejected by default. Values are never logged or written to the
  catalog or admin responses.
- Request bodies are capped at 16 MiB.
- `unsafe` code is confined to process-tree management.

The broker does not sandbox backends: they run with your user's rights.
Report vulnerabilities as described in [../SECURITY.md](../SECURITY.md).
