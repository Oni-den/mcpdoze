# Configuration

MCPDoze reads one TOML file. Unknown keys are rejected, so typos fail loudly.

- Location: `--config PATH`, else the `MCPDOZE_CONFIG` environment variable,
  else `config.toml` in the platform config directory (`mcpdoze doctor` prints
  the resolved path).
- The file is optional: without it the broker starts with defaults and no
  servers. `mcpdoze add` creates and edits it, keeping comments.
- `mcpdoze serve` watches the file and applies changes (debounced); disable
  with `--no-watch`. `mcpdoze reload` applies it on demand. On an error
  nothing changes.
- Durations are strings: `"500ms"`, `"30s"`, `"5m"`, `"1h"`, `"1d"`, or sums
  such as `"1h30m"`.

```toml
[broker]
bind = "127.0.0.1:7878"
default_idle_ttl = "10m"

[servers.example]
transport = "stdio"
command = "npx"
args = ["-y", "some-mcp-server"]
env = { API_KEY = { from_env = "EXAMPLE_API_KEY" } }
idle_ttl = "5m"
```

## `[broker]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `bind` | string | `"127.0.0.1:7878"` | Listen address, `host:port` with an IP or `localhost`. Overridden by `serve --bind`. A non-loopback address requires `allow_remote = true` and a token source. Needs a restart. |
| `token_file` | path | `<data dir>/token` | File holding the bearer token. Created owner-only on first start; a relative path is resolved against the data directory. Mutually exclusive with `token_env`. Needs a restart. |
| `token_env` | string | unset | Name of an environment variable that holds the token instead of a file. |
| `allow_remote` | bool | `false` | Explicit opt-in for a non-loopback bind. The admin API stays loopback-only regardless. |
| `allowed_hosts` | list of strings | `[]` | Extra `Host`/`Origin` host names accepted besides `localhost`, `127.0.0.1`, `[::1]`. Host names or IPs only: no port, wildcard, slash or `@`. |
| `default_idle_ttl` | duration | `"10m"` | Idle time before a backend is unloaded. Must be above 0. |
| `start_timeout` | duration | `"60s"` | How long a backend may take to start and initialise before the call fails. Raise it for slow first starts (see TROUBLESHOOTING). |
| `log_dir` | path | unset | Reserved for log files; backend stderr is currently read with `mcpdoze logs`. |
| `catalog_dir` | path | `<data dir>/catalog` | Where per-server tool catalogs are stored (`<server>.json`). Needs a restart. |
| `catalog_max_age` | duration | unset (off) | Re-list a server's catalog in the background once it is older than this. A server's own value wins. |
| `client_request_timeout` | duration | `"120s"` | How long the broker waits for an agent to answer a server-to-client request such as sampling or elicitation; then the backend gets `-32603`. See [SCOPES.md](SCOPES.md). |
| `session_idle_timeout` | duration | `"30m"` | An agent session with no request and no open event stream for this long is ended; its per-session instances are dropped and the agent must initialise again. |

Example for a remote-reachable broker (do this only behind your own network
controls):

```toml
[broker]
bind = "0.0.0.0:7878"
allow_remote = true
token_env = "MCPDOZE_BROKER_TOKEN"
allowed_hosts = ["broker.example.com"]
```

## `[servers.<name>]`

The name uses `[A-Za-z0-9_.-]`, at most 64 characters. Tools are exposed as
`<prefix>__<tool>`; names over 64 characters are shortened with a stable hash
suffix.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `transport` | `"stdio"` or `"http"` | required | Local process, or remote Streamable HTTP endpoint. |
| `command` | string | required for stdio | Program to run. Not allowed for http. |
| `args` | list of strings | `[]` | Arguments for `command`. |
| `env` | table | `{}` | Backend environment variables, each a literal string or `{ from_env = "NAME" }`. |
| `cwd` | path | broker's | Working directory of the backend. |
| `scope` | `"shared"`, `"per-cwd"`, `"per-session"` | `"shared"` | Instance scope; see [SCOPES.md](SCOPES.md). |
| `max_instances` | integer | `8` | Cap on scoped instances of this server (range 1-256; the shared fallback is not counted). Beyond it a call fails with error `-32001`. See [SCOPES.md](SCOPES.md). |
| `idle_ttl` | duration | `broker.default_idle_ttl` | Idle time before unload. |
| `prefix` | string | sanitised name | Tool prefix: `[a-z0-9_]`, at most 32 characters, no `__`, not starting or ending with `_`, unique across servers. |
| `enabled` | bool | `true` | `false` keeps the entry but does not register it. |
| `url` | string | required for http | `http://` or `https://` endpoint. Not allowed for stdio. |
| `headers` | table | `{}` | Headers sent to an http server, same value forms as `env`. Not allowed for stdio. |
| `headers_helper` | string | unset | Shell command that prints a JSON object of headers for an http server; see [Headers helper](#headers-helper). Not allowed for stdio. |
| `allow_literal_env` | bool | `false` | Permit secret-looking literal values in `env` or `headers`. |
| `probe` | bool | `true` | At registration, start a throwaway instance once to list tools when no valid catalog exists. With `false`, the first `tools/list` starts the server. |
| `catalog_max_age` | duration | `broker.catalog_max_age` | Background catalog refresh age for this server. |
| `subscriptions` | `"keep-alive"` or `"allow-unload"` | `"keep-alive"` | What an instance holding resource subscriptions does at its idle TTL: stay loaded (and respawn eagerly after a crash), or unload and resubscribe on its next start. See [SUBSCRIPTIONS.md](SUBSCRIPTIONS.md). |

```toml
[servers.remote]
transport = "http"
url = "https://example.com/mcp"
headers = { Authorization = { from_env = "REMOTE_AUTH" } }
idle_ttl = "30m"
prefix = "rem"
```

## Headers helper

For credentials that expire (OAuth access tokens, short-lived API keys) an
http server can name a command that produces its headers:

```toml
[servers.remote]
transport = "http"
url = "https://example.com/mcp"
headers_helper = "example-token-tool --json"
```

- The command runs through the platform shell (`cmd /d /s /c` on Windows,
  `sh -c` elsewhere) in the broker's environment (without `MCPDOZE_TOKEN`),
  with stdin and stderr closed.
- It must exit 0 within 10 seconds and print at most 64 KiB: one JSON object
  of string values, e.g. `{"Authorization": "Bearer <token>"}`. Names and
  values must be valid HTTP headers (at most 64).
- It runs before every upstream `initialize` (first connect, re-initialise
  after a lost session or an idle unload) and once more when a call is
  answered with 401 or 403; the call is then retried once.
- Its headers override `headers` entries of the same name (case-insensitive).
- The output is never logged, stored or shown in errors. A failure (timeout,
  non-zero exit, invalid output) fails the connect with a short reason.
- The command (not its output) is part of the catalog fingerprint, so
  changing it re-lists the server.
- `mcpdoze import` copies `headersHelper` from Claude Code configs without
  running it. Helpers from project-scoped files (`./.mcp.json`, project
  entries) are only imported with `--allow-helpers`.

The opposite direction, an agent authenticating to the broker, can use
`mcpdoze token --headers-json` as the agent's own headers helper: it prints
`{"Authorization":"Bearer <token>"}` (also when stdout is a pipe) from the
token file or `broker.token_env`.

## Secrets: environment variable names, not values

Never write a secret into the config. Use `{ from_env = "NAME" }` (CLI:
`--env KEY=ENVVAR`, `--header NAME=ENVVAR`). The variable is read from the
**broker's** environment when the backend starts, so it must be set where the
broker runs (including when started at login). A missing variable makes the
server unregistered with a clear error; values are never logged, stored in the
catalog or hashed into its fingerprint.

A literal that looks like a secret (name containing KEY, TOKEN, SECRET or
PASSWORD; a credential header; a known token prefix; a JWT or private key) is
rejected unless `allow_literal_env = true` (CLI: `--allow-literal-env`
together with `--env-literal NAME=VALUE`).

## Related

- Instance scopes: [SCOPES.md](SCOPES.md)
- Catalog behaviour: [CATALOG.md](CATALOG.md)
- Runtime control: [ADMIN_API.md](ADMIN_API.md)
- Environment variables read by the CLI: `MCPDOZE_CONFIG`, `MCPDOZE_URL`,
  `MCPDOZE_TOKEN_FILE`. Agents read the broker token from `MCPDOZE_TOKEN`.
