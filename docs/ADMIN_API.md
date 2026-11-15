# MCPDoze admin API (v1)

The broker exposes a small JSON API for runtime control. The `mcpdoze` CLI
client commands are thin wrappers over it. Rust clients can reuse the request
and response types in `mcpdoze_core::admin::api`.

## Access

- Base path: `/admin/v1`, on the same address as `/mcp`
  (default `127.0.0.1:7878`).
- Every request needs `Authorization: Bearer <token>`, the same token as
  `/mcp`. The token lives in the token file (`broker.token_file`, default
  `token` in MCPDoze's platform data directory, created owner-only on first
  run; a relative path is resolved against that directory) or in the environment variable named by `broker.token_env`.
  `mcpdoze serve` logs the token file path, never the token.
- The admin API is loopback-only: requests from a non-loopback peer get
  `403` even when the broker is bound to another address.
- `Host` must be `localhost`, `127.0.0.1`, `[::1]` (any port) or a name in
  `broker.allowed_hosts`; an `Origin` header, when sent, must name one of
  those hosts. Otherwise `403` (DNS-rebinding defence).
- Missing or wrong token: `401` with `WWW-Authenticate: Bearer`.
- `GET /healthz` (outside the base path) needs no token and returns `ok`.

Clients must ignore unknown response fields; new fields may be added
without a version change.

## Errors

Every non-2xx response has this body:

```json
{ "error": "invalid server configuration",
  "details": ["servers.example.env.API_TOKEN: literal value looks like a secret ..."] }
```

`details` is omitted when empty. Status codes: `400` invalid request or
config, `401` authentication, `403` host/origin/peer, `404` unknown server,
`409` conflict (name exists, or no config file for reload), `500` file I/O.
Error messages never contain environment values.

## Endpoints

### `GET /status`

Runtime state of every registered backend.

```json
{
  "version": "0.0.1",
  "sessions": 2,
  "servers": [
    {
      "name": "example",
      "prefix": "example",
      "transport": "stdio",
      "state": "running",
      "pids": [4242, 4250],
      "rss_bytes": 81264640,
      "private_bytes": 75497472,
      "in_flight": 0,
      "idle_remaining_secs": 540,
      "calls": 17,
      "restarts": 1,
      "failures": 0,
      "last_error": null
    }
  ]
}
```

- `state`: `stopped`, `starting`, `running`, `draining`, `stopping`, `failed`.
- `pids`, `rss_bytes`, `private_bytes`: summed over the backend's whole process
  tree while running (empty / zero otherwise and for `http` backends).
  Private bytes are the commit charge on Windows, `Private_Clean +
  Private_Dirty` on Linux, and equal to RSS elsewhere.
- `idle_remaining_secs`: time to the idle unload; `null` unless running with
  no call in flight.
- `restarts`: processes (or upstream sessions) started after the first one,
  after an idle unload, a stop or a crash.

### `GET /servers`

Every configured or registered server.

```json
{
  "servers": [
    {
      "name": "example",
      "prefix": "example",
      "registered": true,
      "config": { "transport": "stdio", "command": "npx",
                  "args": ["-y", "some-mcp-server"],
                  "env": { "API_KEY": { "from_env": "EXAMPLE_API_KEY" } } },
      "error": null
    }
  ]
}
```

`config` is `null` for servers registered directly in code. `error` explains
why a configured server is not registered (for example `disabled`, or a
missing environment variable).

### `POST /servers`

Registers a server. `server` has exactly the shape of a `[servers.<name>]`
table in the config file (durations as strings such as `"5m"`).

```json
{
  "name": "example",
  "server": {
    "transport": "stdio",
    "command": "npx",
    "args": ["-y", "some-mcp-server"],
    "env": { "API_KEY": { "from_env": "EXAMPLE_API_KEY" } },
    "idle_ttl": "5m"
  }
}
```

- Names use `[A-Za-z0-9_.-]`, at most 64 characters.
- The whole resulting config is validated (unique prefixes, transport
  fields, secret-looking literals need `allow_literal_env = true`), and
  `from_env` references must resolve, before anything changes.
- With a config file the new table is appended to it, keeping comments and
  formatting, by writing a temporary file and renaming it over the original.
  Without one the server lasts until the broker exits.
- The backend itself is not started. Unless the server has `probe = false`,
  and no valid catalog exists for it (see [CATALOG.md](CATALOG.md)), a
  throwaway instance is probed once in the background (spawn, list tools,
  prompts and resources, stop) so later lists need no process.

Response `201`:

```json
{ "server": { "name": "example", "prefix": "example", "registered": true,
              "config": { "...": "..." }, "error": null },
  "persisted": true }
```

`409` if the name exists, `400` if invalid.

### `DELETE /servers/{name}`

Removes a server from the config (file or memory) and the router, then
drains its backend (in-flight calls may finish for up to 30 s, then the
process tree is killed). Response: `{ "name": "example", "persisted": true }`.

### `POST /servers/{name}/stop[?force=true]`

Unloads the backend but keeps the server registered; the next call starts it
again. By default it drains (waits up to 30 s for in-flight calls);
`force=true` kills the tree at once and fails calls in flight. Response:
`{ "name": "example", "state": "stopped" }`.

### `POST /reload`

Re-reads the config file and applies the difference: new servers are
registered, removed ones unregistered and unloaded, changed ones (command,
arguments, resolved environment, TTL, prefix, ...) unloaded and registered
again. Unchanged servers keep running. On a parse or validation error nothing
changes (`400`). `409` if the broker runs without a config file.

```json
{ "added": ["new"], "removed": ["old"], "changed": ["example"],
  "errors": ["other: environment variable EXAMPLE_TOKEN (needed for TOKEN) is not set"],
  "restart_required": ["broker.bind"] }
```

`restart_required` lists `[broker]` settings that changed but only apply
after a restart (`bind`, `token_file`, `token_env`, `allow_remote`,
`allowed_hosts`, `log_dir`, `catalog_dir`). `default_idle_ttl`,
`start_timeout` and `catalog_max_age` apply on reload. A changed server whose
launch fingerprint changed (command, arguments, working directory, URL,
environment or header names) is probed again.

`mcpdoze serve` also watches the config file and reloads it automatically
(debounced) unless started with `--no-watch`.

### `POST /servers/{name}/refresh`

Re-lists one server's tools, prompts, resources and resource templates now:
on its running process if it has one, else by probing a throwaway instance
(started, listed, stopped; the server's own instance stays as it was). A
non-empty difference is stored in the catalog and sent as
`notifications/*/list_changed` to agent sessions with an open stream.

```json
{ "name": "example", "changed": true, "tools": 12, "prompts": 0,
  "resources": 3, "resource_templates": 1, "error": null }
```

A listing failure is still `200`, with `error` set and the catalog unchanged.
`404` if the server is unknown.

### `POST /refresh`

Refreshes every registered server concurrently, as above:
`{ "servers": [ { "name": "example", "changed": false, ... } ] }`, ordered by
name.

### `GET /servers/{name}/logs[?lines=N]`

The last `N` lines (default 100, at most 500) of the backend's stderr, kept in
memory across restarts. Response:
`{ "name": "example", "lines": ["...", "..."] }`. Backend output is shown as
is; it is only reachable with the token from a loopback peer.
