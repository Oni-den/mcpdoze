# Instance scopes and server-to-client requests

Every backend instance is keyed by `(server, scope_key)`. The scope is set
per server:

```toml
[servers.example]
transport = "stdio"
command = "example-mcp-server"
args = ["--root", "${cwd}"]
env = { EXAMPLE_HOME = "${cwd}/.example", API_KEY = { from_env = "EXAMPLE_API_KEY" } }
scope = "per-cwd"        # shared (default) | per-cwd | per-session
max_instances = 8        # cap on scoped instances of this server (1-256)
idle_ttl = "5m"
```

| Scope | `scope_key` | One instance per | Unloaded |
|---|---|---|---|
| `shared` | `""` | server | idle TTL |
| `per-cwd` | agent working directory | directory | idle TTL only |
| `per-session` | agent session id | `Mcp-Session-Id` | session end, idle TTL |

Each instance has its own lifecycle (lazy start, idle TTL, in-flight count,
crash backoff). `tools/list` and the other lists still come from the single
catalog entry of the server, so listing never starts a scoped instance.

## The agent's working directory (`per-cwd`)

The broker takes the directory from, in this order:

1. **The agent's roots.** If the agent advertised the `roots` client
   capability in `initialize` and has its event stream (`GET /mcp`) open,
   the broker sends it one `roots/list` request over that stream before the
   first scoped call (10 s timeout) and uses the first `file://` root that is
   an existing directory. `notifications/roots/list_changed` from the agent
   makes the broker ask again on the next call.
2. **`X-Mcpdoze-Cwd` header** with an absolute path (or a `file://` URI),
   on `initialize` or any later POST.
3. **`cwd` query parameter** on the endpoint URL, percent-encoded, for
   agents that cannot send custom headers:
   `http://127.0.0.1:7878/mcp?cwd=%2Fhome%2Fexample%2Fproject`.

Relative paths and paths that are not existing directories are ignored. If
no directory is known the call is served by the server's **shared fallback
instance** (logged once per session at `info`). Stateless clients (no
session) can use the header or query on each request.

Directories are compared after normalisation (trailing separators removed;
on Windows `/` and `\` are equal and case is ignored), so two sessions in the
same directory share one process.

## `${cwd}` expansion

For `per-cwd` instances, and `per-session` instances whose session has a
known directory:

- `${cwd}` is replaced in `args`, in literal `env` values and in `cwd`.
- It is **never** expanded inside values resolved from `{ from_env = ... }`
  references.
- The process is started in the agent's directory unless `cwd` is
  configured (then the configured, expanded `cwd` is used).

The shared fallback of a scoped server expands `${cwd}` to the broker's own
working directory. `shared` servers are launched exactly as configured.

## Per-session instances

A `per-session` instance belongs to one agent session and is unloaded when:

- the session ends (`DELETE /mcp`, eviction of the least recently used
  session beyond 1024, broker shutdown),
- the session times out: no request and no open event stream for
  `broker.session_idle_timeout` (default `30m`); the session is then ended
  and the agent gets `404` and must initialise again, as the MCP transport
  specification requires,
- or its idle TTL expires.

Stateless clients have no session; a `per-session` server serves them from
its shared fallback instance.

## Instance cap

At most `max_instances` scoped instances (default 8) exist per server; the
shared fallback is not counted. Stopped instances are dropped from the count
first. Beyond the cap a call gets:

```json
{"jsonrpc":"2.0","id":7,"error":{"code":-32001,"message":"server \"example\" already runs its maximum of 8 per-cwd instances (max_instances); try again when one is unloaded"}}
```

## Status and admin operations

`GET /admin/v1/status` lists every instance under `instances` with its
`scope_key` (see [ADMIN_API.md](ADMIN_API.md)). Session ids are shown only
as `session:<first 8 characters>`. `stop`, `remove` and `reload` act on all
instances of a server; `logs` merges them, prefixed with the scope key.

## Server-to-client requests

Backends can send requests to their client (`sampling/createMessage`,
`elicitation/create`, `roots/list`, `ping`). The broker is that client.

| Request | Handling |
|---|---|
| `ping` | answered by the broker |
| `roots/list` | answered by the broker: the instance's directory as one `file://` root for `per-cwd` (and `per-session` with a known directory), else an empty list |
| anything else | routed to the owning agent session, or refused |

**Owning session.** For a `per-session` instance it is the instance's
session. Otherwise it is the only agent session with a call in flight on that
backend connection at the moment the request arrives. When several sessions
have calls in flight, or none do, the request is ambiguous and the backend
immediately gets `-32601` ("not supported here: the mcpdoze broker cannot
tell which agent session it is for").

**Delivery.** The request is sent on the session's event stream
(`GET /mcp`) with a broker id (`mcpdoze-s2c-<n>`). The agent POSTs its
JSON-RPC response with that id to `/mcp` (`202 Accepted`); the broker sends
it to the backend under the backend's original id, `result` or `error`
unchanged. If the session has no open event stream the backend gets `-32601`
at once. If the agent does not answer within
`broker.client_request_timeout` (default `120s`), or the session ends, the
backend gets `-32603`. At most 32 such requests wait per session.

**Advertised capabilities.** The broker's `initialize` request to a backend
offers only what it can serve for that instance:

| Scope | `roots` | `sampling` | `elicitation` |
|---|---|---|---|
| `shared` | yes (empty list) | no | no |
| `per-cwd` | yes (the directory) | no | no |
| `per-session` | yes | yes | yes |

Sampling and elicitation stay off for shared backends even while only one
agent session is connected: a second session can attach at any moment, and
the attribution rule above (exactly one session in flight) cannot be known
when the backend is initialised. A shared backend that sends such a request
anyway is routed by the rule above. Use `scope = "per-session"` for servers
that need sampling or elicitation.

Backend `notifications/message` (logging) goes to the sessions with calls in
flight on the connection it arrived on; progress notifications follow their
progress token.

## Crash detection

The broker watches the backend's root process. When it exits (crash, or
killed from outside) calls in flight fail with `-32000` within about 200 ms,
even when a grandchild still holds the output pipe; the rest of the process
tree is killed, the instance enters `failed` with exponential backoff and the
next call after the backoff starts a new process.
