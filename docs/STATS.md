# Statistics log

The broker keeps an append-only usage log so you can see, over weeks, what
sharing servers through MCPDoze costs and saves compared with running every
MCP server directly inside each agent session.

```text
mcpdoze stats                 # last 7 days, one row per server
mcpdoze stats --since 30d     # any duration: 12h, 90m, 30d
mcpdoze stats --tools         # per tool, ranked by response bytes
mcpdoze stats --tools --top 5 # the 5 largest tools (default 20)
mcpdoze stats --json          # the whole summary as JSON (all fields)
```

`mcpdoze stats` asks the running broker (`GET /admin/v1/stats`, see
[ADMIN_API.md](ADMIN_API.md)); when the broker is not reachable it reads the
files directly and says so. Reading the files gives everything except the
tool-list sizes, which are not logged: the broker measures them from its live
catalog, so `--tools` prints "not available" for them without a running
broker.

`--tools` prints one row per tool: calls, average and 95th-percentile
response size, total bytes, share of the server's tool-response bytes,
`est tokens` (total bytes / 4), duplicate calls (`DUP`) and bytes not sent to
the agent thanks to optimisations (`SAVED_BYTES`). A footer gives the totals
and, from the broker, each server's tool-list size (`LIST_BYTES`, `est
tokens`).

Sizes are JSON byte counts. `est tokens` is bytes / 4, a rough estimate for
comparing tools, not a tokenizer count.

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
| `call` | `server`, `kind` (`tool`, `prompt`, `resource`), `name`, `duration_ms`, `ok`, `error_code` (JSON-RPC error responses), `fail` (classified failure, see below), `cold_start`, `spawn_wait_ms`, `bytes_in`, `bytes_out`, `bytes_sent` (only when an optimisation changed the response), `opt` (optimisation tags, see below), `cached` (served from the result cache), `dup` (repeat call, see below), `era` (`legacy` 2025-11-25 or `modern` 2026-07-28), `scope`, `instance` (scoped instances) |
| `spawn` | `server`, `instance`, `startup_ms` (start decision to initialised), `generation` |
| `unload` | `server`, `instance`, `reason` (`idle`, `crash`, `stop`, `reload`), `uptime_s`, `rss_mb` (working set at the last sample), `exit_code` (backend root process, when it exited normally), `signal` (Unix: the signal that killed the root process), `generation` |
| `session` | `action` (`open`, `close`), `count` (stored sessions after the change) |
| `sample` | `interval_s`, `broker_rss_mb`, `broker_private_mb`, `sessions`, `running_instances`, `servers` (per server: `instances`, `pids`, `private_mb`, `working_mb`, `sessions`, `est_direct_mb`), `est_direct_mb`, `actual_mb`, `saved_mb`, `dropped` |

- `ok` is false for a JSON-RPC error and for a tool result with
  `isError: true` (then `error_code` is absent).
- `cold_start` means the backend was not running when the call arrived;
  `spawn_wait_ms` is the time spent waiting for it.
- `bytes_in` / `bytes_out` are the sizes of the request params and of the
  response as compact JSON. `bytes_out` is the size the backend sent.
- `bytes_sent` is the size the agent actually received, present only when it
  differs from `bytes_out` (an optimisation changed the response). The
  difference is the saving (`bytes_saved` in the summary).
- `opt` lists the optimisations applied to the call. Today: `args` (default
  arguments filled in) and `minify` (result trimmed). Other tags may appear as
  optimisations are added; see [OPTIMIZE.md](OPTIMIZE.md). Absent when none
  applied.
- `cached`: the response came from the broker's result cache (strict opt-in,
  see [OPTIMIZE.md](OPTIMIZE.md)).
- `fail` is set only for a JSON-RPC error response and names its class:
  `stalled` (the call timed out at the backend), `crash_in_flight` (the
  backend connection closed during the call), `backoff_rejected` (the backend
  is in restart backoff), `backend_unavailable` (other unavailable backend),
  `agent_cancelled` (the agent cancelled), or `rpc_error` (any other JSON-RPC
  error). A tool result with `isError: true` has `ok` false but no `fail`.
- `dup` (tool calls only): the same session already made an identical call
  (same server, tool and arguments) among its last 32 tool calls. Sessionless
  (2026-07-28) calls share one session. Only the flag is logged; the hash
  lives in memory, is never written, and the whole table is cleared once more
  than 512 sessions are tracked.
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

## Summary

`mcpdoze stats --json` (and `GET /admin/v1/stats`) returns the summary of the
events in the period:

- `since_ms`, `until_ms`, `events`, `sessions_opened`, `peak_sessions`,
  `dropped` (the highest `dropped` count seen).
- `servers`: per server `calls`, `errors`, `error_rate`, `p50_ms`, `p95_ms`,
  `cold_starts`, `cold_start_rate`, `spawns`, `unloads` (count per reason),
  `failures` (count per `fail` class). `calls` counts tool, prompt and
  resource calls; `errors` counts calls with `ok` false.
- `tools`: per tool, tool calls only, largest `total_bytes` first: `calls`,
  `avg_bytes_out`, `p95_bytes_out` (nearest rank), `total_bytes`, `est_tokens`
  (`total_bytes / 4`), `share` (of the server's total tool-response bytes,
  three decimals), `dup_calls`, `bytes_saved` (the sum of `bytes_out -
  bytes_sent`).
- `lists`: per server the size of its tool list as served to agents, from the
  live catalog: `list_bytes` (the prefixed list as JSON) and
  `est_list_tokens` (`list_bytes / 4`). Filled by the broker only; empty when
  the summary is read from the files.
- `memory`: the sample averages and peaks described under
  [Comparing with direct servers](#comparing-with-direct-servers).

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
