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
      "last_error": null,
      "scope": "shared",
      "instances": [
        { "scope_key": "", "state": "running", "pids": [4242, 4250],
          "rss_bytes": 81264640, "private_bytes": 75497472, "in_flight": 0,
          "idle_remaining_secs": 540, "calls": 17, "restarts": 1,
          "failures": 0, "last_error": null, "debug": "off" }
      ]
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
- `scope`: `shared`, `per-cwd` or `per-session` (see [SCOPES.md](SCOPES.md)).
- `instances`: one entry per backend instance `(server, scope_key)`, with the
  same per-instance fields as above. `scope_key` is `""` for the shared
  instance (for scoped servers: the fallback used when the agent directory
  is unknown, listed once it has been used), the normalised directory for
  `per-cwd`, and `session:<first 8 characters of the session id>` for
  `per-session`. Stopped scoped instances are dropped from the list.
- `debug` (per instance): the failure-log mode in effect, `off`, `failures`
  or `trace` ([DEBUG.md](DEBUG.md)). `last_failure` (`{ "id": "f-000123",
  "kind": "crash_in_flight", "ts": 1767225600123 }`) is the newest classified
  failure of that instance in the failure log; it is omitted while the log has
  none (and always when debug has never been on). Ids match the failure
  records, so `mcpdoze debug show` finds it.
- For `shared` servers the top-level fields describe the one instance, as
  before. For scoped servers they aggregate every instance: `state` is the
  most active state, `pids` and memory are summed over all trees,
  `in_flight`, `calls` and `restarts` are sums, `failures` is the maximum.
- A backend whose root process exits (crash or external kill) is noticed
  at once, even when a grandchild keeps its output pipe open: calls in
  flight fail with a JSON-RPC error, the instance shows `failed` with empty
  `pids` and is restarted by the next call after the backoff.

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
drains every instance of it (in-flight calls may finish for up to 30 s, then
the process trees are killed). Response: `{ "name": "example", "persisted": true }`.

### `POST /servers/{name}/stop[?force=true]`

Unloads the backend, every scoped instance included, but keeps the server
registered; the next call starts the instance it needs again. By default it
drains (waits up to 30 s for in-flight calls); `force=true` kills the trees
at once and fails calls in flight. Response:
`{ "name": "example", "state": "stopped" }` (the most active state left).

### `POST /reload`

Re-reads the config file and applies the difference: new servers are
registered, removed ones unregistered and unloaded, changed ones (command,
arguments, resolved environment, TTL, prefix, scope, `max_instances`, ...)
unloaded (all instances) and registered again. Unchanged servers keep running. On a parse or validation error nothing
changes (`400`). `409` if the broker runs without a config file.

```json
{ "added": ["new"], "removed": ["old"], "changed": ["example"],
  "errors": ["other: environment variable EXAMPLE_TOKEN (needed for TOKEN) is not set"],
  "restart_required": ["broker.bind"] }
```

`restart_required` lists `[broker]` settings that changed but only apply
after a restart (`bind`, `token_file`, `token_env`, `allow_remote`,
`allowed_hosts`, `log_dir`, `catalog_dir`) and `stats` when the `[stats]`
table changed. `default_idle_ttl`,
`start_timeout`, `catalog_max_age`, `client_request_timeout` and
`session_idle_timeout` apply on reload. A changed server whose
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

### `GET /debug`, `POST /debug`

The failure log ([DEBUG.md](DEBUG.md)). `GET` returns the effective state:

```json
{ "mode": "off", "servers": { "example": "failures" },
  "runtime": { "servers": { "example": "failures" } },
  "raw": false, "expires_in_secs": 1740, "available": true,
  "written": 12, "dropped": 0 }
```

- `mode`: effective mode for servers without a setting of their own;
  `servers`: effective mode of every registered server (`off`, `failures`,
  `trace`).
- `runtime`: the runtime layer set by `POST` (`all` and per-server modes). It
  has the highest precedence, is not written to the config, and is lost on
  restart. `expires_in_secs` is present while a `for` timer runs.
- `available`: false when no debug directory can be determined; everything
  then stays off.
- `raw`: raw frame capture; always `false` in this version.
- `written`, `dropped`: records written / dropped (queue full) by this broker
  process.

`POST` replaces the runtime layer and applies at once, without unloading any
backend. Body (unknown fields are rejected):

```json
{ "mode": "failures", "servers": ["example"], "raw": false, "for": "30m" }
```

`mode` is `off`, `failures` or `trace`; `servers` limits it to those servers
(empty: every server); `for` reverts the runtime layer after that duration.
`raw: true` is refused with `400` ("raw capture is not available"). The
response is the new `GET /debug` state. `400` for an invalid mode, server name
or duration, and when no debug directory can be determined.
`mcpdoze debug on|off|status` use these endpoints.

### `GET /debug/failures[?server=&kind=&since=&limit=]`

Failure records, oldest first, as `{ "records": [ ... ] }` (the schema of
[DEBUG.md](DEBUG.md)). `since` is a duration ago (`1h`) or unix milliseconds;
`limit` defaults to 50 and is capped at 500. Pending records are flushed
before reading, so a `limit=1` request is also how `mcpdoze debug export` asks
the broker to flush. The records are sanitised by construction; still,
treat them as local diagnostics (loopback and token only).

### `GET /servers/{name}/logs[?lines=N]`

The last `N` lines (default 100, at most 500) of the backend's stderr, kept in
memory across restarts. For scoped servers the lines of each instance follow
the shared instance's, prefixed with `[<scope_key>] `. Response:
`{ "name": "example", "lines": ["...", "..."] }`. Backend output is shown as
is; it is only reachable with the token from a loopback peer.

### `GET /stats[?since=7d]`

Summary of the statistics log ([STATS.md](STATS.md)) over the last `since`
(a duration, default `7d`); buffered events are flushed first. `mcpdoze stats`
prints the same summary. Each server has `failures`: classified call
failures by kind in the period (from the `fail` field of call events;
empty when none). `409` when `[stats] enabled = false`, `400` for an
invalid `since`.

```json
{ "since_ms": 1767225600000, "until_ms": 1767830400000, "events": 5120,
  "sessions_opened": 14, "peak_sessions": 3, "dropped": 0,
  "servers": [ { "server": "example", "calls": 412, "errors": 3,
    "error_rate": 0.007, "p50_ms": 38, "p95_ms": 910, "cold_starts": 21,
    "cold_start_rate": 0.051, "spawns": 21, "unloads": { "idle": 20, "crash": 1 },
    "failures": { "rpc_error": 2, "stalled": 1 } } ],
  "memory": { "samples": 9800, "avg_actual_mb": 182.4, "peak_actual_mb": 610.0,
    "avg_est_direct_mb": 395.1, "peak_est_direct_mb": 1480.2,
    "avg_saved_mb": 212.7, "mb_hours_saved": 34742.0 } }
```
