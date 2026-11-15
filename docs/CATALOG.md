# Catalog, notifications, prompts and resources

## Disk catalog

MCPDoze answers `tools/list`, `prompts/list`, `resources/list` and
`resources/templates/list` from a per-server catalog, so listing needs no
backend process.

- File: `<catalog_dir>/<server>.json` (`broker.catalog_dir`, default
  `catalog` in the platform data directory; a relative path is resolved
  against that directory). Written atomically (temporary file, then rename).
- Content: `version` (currently `1`), `fingerprint`, `updated_at` (Unix
  seconds), `server_info`, `capabilities`, `tools`, `prompts`, `resources`,
  `resource_templates`, exactly as the backend listed them (backend names,
  not prefixed).
- Fingerprint: a hash of transport, command, arguments, working directory,
  URL, environment variable NAMES and header NAMES. Values of environment
  variables and headers are never hashed or stored.
- A missing, unreadable, corrupt, oversized (over 32 MiB) or other-version
  file counts as no catalog; it is rewritten by the next probe or list.

### Probe

On registration (start-up, `POST /admin/v1/servers`, reload) a server with
a valid catalog of the same fingerprint is used as is: nothing is spawned.
Otherwise, with `probe = true` (the default), a throwaway instance is
started in the background, listed and stopped. With `probe = false` the
server stays uncatalogued, and its own instance is started by the first
list request.

```toml
[servers.example]
transport = "stdio"
command = "npx"
args = ["-y", "some-mcp-server"]
probe = true              # default
catalog_max_age = "7d"    # optional; also settable under [broker]
```

### Refresh

The catalog is re-listed in the background, one refresh at a time per
server:

- after every spawn of a catalogued backend (on the new process);
- on backend `notifications/tools/list_changed`, `prompts/list_changed` or
  `resources/list_changed`;
- after a `tools/call` that the backend rejects as unknown (`-32602` or
  `-32601`) or for a tool the catalog does not list (the call itself is
  forwarded as usual, so the agent gets the backend's normal JSON-RPC
  answer);
- when the entry is older than `catalog_max_age` (off by default; served
  meanwhile);
- on `POST /admin/v1/servers/{name}/refresh` or `POST /admin/v1/refresh`;
- on a fingerprint change (re-probe at registration).

A non-empty difference is stored and `notifications/<list>/list_changed` is
sent to every agent session with an open notification stream.

## Notification stream (`GET /mcp`)

`GET /mcp` with a valid `Mcp-Session-Id` and `Accept: text/event-stream`
opens the session's server-sent-event stream. `POST /mcp` keeps answering
in JSON response mode.

- Each notification is one `event: message` with the JSON-RPC message as
  `data`. A comment (`: keep-alive`) is sent every 25 s while idle.
- Delivered: `notifications/tools|prompts|resources/list_changed`, progress
  notifications (`notifications/progress`, routed by the rewritten progress
  token back to the agent's own token), and backend `notifications/message`
  (log) to sessions with a call in flight on that backend.
- Each session has one bounded queue of 256 messages; when full, the oldest
  is dropped (and counted). Progress and log messages for a session are
  queued even before its stream opens; list changes go only to sessions
  with an open stream. Sessions without a stream (stateless clients) see
  changes at their next list.
- One stream per session; opening another replaces the first. The stream
  closes on `DELETE /mcp`, session eviction and broker shutdown. There is
  no resumption (`Last-Event-ID` is ignored).
- Errors: no session id `405`, unknown session `404`, `Accept` without
  `text/event-stream` `406`.

## Prompts

Prompts are exposed as `<prefix>__<name>`, like tools. `prompts/get` strips
the prefix and forwards to the backend.

## Resources and the URI mapping

A backend resource URI `<uri>` on the server with prefix `<prefix>` is
exposed as

```text
mcpdoze://<prefix>/<enc>
```

where `<enc>` is `<uri>` with every byte outside the RFC 3986 unreserved set
(`A-Z a-z 0-9 - . _ ~`) percent-encoded (`%XX`, upper-case hex). Example:
`file:///docs/a b.txt` on prefix `ex` becomes
`mcpdoze://ex/file%3A%2F%2F%2Fdocs%2Fa%20b.txt`. `resources/read` decodes the
segment once, forwards the original URI, and maps the `uri` of each returned
content item back.

Resource templates are mapped the same way, except that `{...}` expressions
are kept verbatim so clients can expand them: `mock://items/{id}` becomes
`mcpdoze://ex/mock%3A%2F%2Fitems%2F{id}`. Because the reverse mapping
decodes once, an expanded value reaches the backend decoded once (a `{path}`
value `a/b` arrives as `a/b`, not `a%2Fb`).

Resource subscriptions (`resources/subscribe`, `resources/unsubscribe`) are
not supported yet and return `-32601`; the broker advertises
`resources.subscribe = false`. Planned for v0.2.
