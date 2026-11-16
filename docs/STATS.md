# Statistics log

The broker keeps an append-only usage log so you can see, over weeks, what
sharing servers through MCPDoze costs and saves compared with running every
MCP server directly inside each agent session.

```text
mcpdoze stats                 # last 7 days
mcpdoze stats --since 30d     # any duration: 12h, 90m, 30d
mcpdoze stats --json          # machine-readable summary
```

`mcpdoze stats` asks the running broker (`GET /admin/v1/stats`, see
[ADMIN_API.md](ADMIN_API.md)); when the broker is not reachable it reads the
files directly.

## Configuration

```toml
[stats]
enabled = true              # default
retention_days = 90         # default
sample_interval_secs = 60   # default
debug = false               # default
# dir = "stats"             # default <data dir>/stats; relative to the data dir
```

Changes take effect after a broker restart (`mcpdoze reload` reports
`stats` under `restart_required`). See [CONFIGURATION.md](CONFIGURATION.md).

## Files

- `<dir>/stats-YYYY-MM.jsonl`: one JSON object per line, in the UTC month of
  the event. New lines are only appended.
- Retention: a monthly file is deleted once its whole month ended more than
  `retention_days` ago (checked at start and every few hours), so up to one
  extra month is kept.
- Writing never blocks a request: events go into a bounded in-memory queue
  (4096 events) read by one writer thread, which buffers and flushes about
  once a second. When the queue is full the event is dropped and counted;
  the count appears as `dropped` in every `sample` and in the summary. A
  broker that is killed (rather than stopped) can lose the last second.

## Events

Every line has `ev` (the event type) and `ts` (Unix time in milliseconds).

| `ev` | Fields |
|---|---|
| `call` | `server`, `kind` (`tool`, `prompt`, `resource`), `name`, `duration_ms`, `ok`, `error_code` (JSON-RPC error responses), `cold_start`, `spawn_wait_ms`, `bytes_in`, `bytes_out`, `era` (`legacy` 2025-11-25 or `modern` 2026-07-28), `scope`, `instance` (scoped instances) |
| `spawn` | `server`, `instance`, `startup_ms` (start decision to initialised), `generation` |
| `unload` | `server`, `instance`, `reason` (`idle`, `crash`, `stop`, `reload`), `uptime_s`, `rss_mb` (working set at the last sample), `generation` |
| `session` | `action` (`open`, `close`), `count` (stored sessions after the change) |
| `sample` | `interval_s`, `broker_rss_mb`, `broker_private_mb`, `sessions`, `running_instances`, `servers` (per server: `instances`, `pids`, `private_mb`, `working_mb`, `sessions`, `est_direct_mb`), `est_direct_mb`, `actual_mb`, `saved_mb`, `dropped` |

- `ok` is false for a JSON-RPC error and for a tool result with
  `isError: true` (then `error_code` is absent).
- `cold_start` means the backend was not running when the call arrived;
  `spawn_wait_ms` is the time spent waiting for it.
- `bytes_in` / `bytes_out` are the sizes of the request params and of the
  response as compact JSON.
- With `debug = true`, `call` events also carry `req_id` (per broker run),
  `acquire_ms` (obtaining a running backend) and `backend_ms` (send to
  response).
- Session events count stored (2025-11-25) sessions; stateless 2026-07-28
  requests have no session.

Example lines:

```json
{"ev":"call","ts":1767225600123,"server":"example","kind":"tool","name":"search","duration_ms":41,"ok":true,"cold_start":false,"spawn_wait_ms":0,"bytes_in":58,"bytes_out":2211,"era":"legacy","scope":"shared"}
{"ev":"sample","ts":1767225660000,"interval_s":60,"broker_rss_mb":14.2,"broker_private_mb":9.8,"sessions":3,"running_instances":2,"servers":[{"server":"example","instances":1,"pids":3,"private_mb":182.0,"working_mb":201.5,"sessions":3,"est_direct_mb":546.0}],"est_direct_mb":546.0,"actual_mb":191.8,"saved_mb":354.2,"dropped":0}
```

## Privacy

The log is for capacity questions, not for auditing what agents did:

- No tool arguments, tool results, prompt arguments or resource contents.
- No resource URIs: `name` of a `resource` call is a 12-hex-digit hash of the
  URI.
- No directories and no session ids: `instance` is a 12-hex-digit hash of the
  instance key (the normalised directory for `per-cwd`, the session id for
  `per-session`); session ids are kept only in memory, to count concurrent
  sessions.
- No tokens, headers or environment values.

The hashes are unsalted SHA-256 prefixes: they are stable across restarts (so
an instance can be followed over time) and do not reveal the input, but
someone who guesses a directory can confirm the guess by hashing it. Tool and
prompt names and server names are written as they are.

## Comparing with direct servers

The point of the log is the `sample` comparison:

- `actual_mb`: what the broker side really uses, the broker's own private
  memory plus the private memory of every running backend process tree.
- `est_direct_mb`: an estimate of the same work without MCPDoze. For each
  server, the last measured private memory of one instance times the number
  of sessions that called that server during the sample window (every
  sessionless 2026-07-28 caller together counts as one session). A server
  that is unloaded keeps its last measured size.
- `saved_mb = est_direct_mb - actual_mb`; negative when the broker costs more
  (for example one session with everything warm).
- The summary averages and peaks these over the period and integrates
  `saved_mb` over time as MB-hours (`saved_mb * interval_s / 3600` per
  sample).

For a measured, not estimated, comparison run the same workload both ways and
compare totals; [`scripts/case-study.ps1`](../scripts/case-study.ps1) does this
for two sessions, see [CASE_STUDY.md](CASE_STUDY.md).

### Caveats: the direct figure is an estimate

- It assumes a direct per-session server would use what one broker instance
  uses. Shared instances often hold more state (caches for every session), so
  this can overstate a single direct copy; a fresh direct copy also skips the
  warm-up the broker paid once, which can go either way.
- It counts only sessions that *called* a server in the window. Agents
  usually start every configured server at session start, called or not, so
  for idle servers the real direct cost is higher than estimated: the
  estimate is conservative there.
- Idle unloads are the broker's other saving: a direct setup keeps its
  processes for the whole session. In a window without calls the estimate
  for that server is 0, so this saving is not counted either.
- Private memory is commit charge on Windows and `Private_Clean +
  Private_Dirty` on Linux; on macOS it equals the resident size. Remote
  (`http`) servers use no local memory and are not counted.
- Samples are only taken while the broker runs.
