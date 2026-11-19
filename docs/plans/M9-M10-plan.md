# M9 / M10 plan: failure diagnostics and token optimisation

Status: planned. Source article: MindStudio, 10 MCP Optimization Techniques (2026-04-29).
All paths are relative to the repository root.

## 1. Summary

**Part A (M10, token optimisation).** Of the ten techniques, a broker can do six honestly, all as per-server opt-in: 1, 3, 4, 5, 6 and 7 (7 as truncate-plus-retrieve, not summarise). Batching (8) is possible as an opt-in meta-tool, but it weakens per-tool approval. Techniques 2, 9 and 10 belong to the server author or the agent; the broker can only offer a slice-a-stored-result meta-tool and a duplicate-call metric. Measurement is cheap and comes first, since `bytes_in`/`bytes_out` are already logged per call.

**Part B (M9, failure diagnostics).** A new `diag` module writes sanitised-by-construction failure records (JSONL): shapes, sizes, timings and lifecycle state, never values. It is fed by taps at existing choke points and is off by default. Off costs one atomic load per frame and per call, not literally zero.

**Order:** Part B core first (it also fixes the bugs below), then A phase 0 and 1, then the result store and the rest.

**Owner decision needed:** the default debug mode. Recommendation: ship `off`; `doctor` and `status` hint at enabling it when `failures > 0`. Revisit a default of `failures` once the redaction tests have run in the field.

## 2. Bugs and gaps found while tracing the code

These shape both parts. Items 1-6 are fixed alongside M9 regardless of the debug mode.

1. **Stdio calls have no timeout.** `Router::forward_traced` (`crates/mcpdoze-core/src/router/forward.rs:318`) selects only on the response and agent cancel. HTTP backends have `request_timeout` (120 s, `backend/http.rs:338`).
2. **An oversize backend line hangs its caller.** `read_loop` (`backend/stdio.rs:494`) drops a line over 16 MiB with a warning; the pending call is never failed, and with item 1 nothing ends it.
3. **String ids are silently dropped.** `dispatch` (`backend/stdio.rs:516`) requires `id.as_u64()`. A server that echoes ids as strings gets "response with foreign id dropped" at debug level and the call hangs.
4. **The exit status is thrown away.** `ProcessTree` holds `ExitStatus` (`proctree/mod.rs:75`), but `exited()` returns `()`. `Instance::on_closed` (`backend/lifecycle.rs:703`) records only "backend exited unexpectedly".
5. **The negotiated protocol version is never checked.** `StdioConn::initialize` (`backend/stdio.rs:212`) sends `2025-11-25` and accepts any result.
6. **Stdout pollution is invisible.** Non-JSON lines are dropped at `debug` level with no count (`backend/stdio.rs:505`).
7. **HTTP failures keep only the status code** (`XErr::Status(u16)`, `backend/http.rs:95-114`): no content-type or body class.

**Not verified** (the implementer must check):
- whether `Control::apply` (`admin/mod.rs:377`) treats any `ServerConfig` change as "unload and re-register" (the docs say so);
- whether the prefix `mcpdoze` is already reserved in config validation;
- whether `proptest` is already a dependency;
- the name of the secret-literal detector in `config/mod.rs`.

## 3. Part A: token optimisation

### 3.1 Article techniques and verdicts

| # | Technique (MindStudio) | Verdict | Size | Hook | Depends on |
|---|---|---|---|---|---|
| 0 | Measure tokens per tool call | implement, on by default | S | `router/usage.rs`, `stats.rs` | none |
| 1 | Filter tool outputs at the server level | opt-in per tool | M | `Router::call_tool`, after the forward | 0, A2 |
| 2 | Run computation server-side | out of scope; offer result-slice meta-tool | S on top of 7 | meta-tools | 7 |
| 3 | Compress tool descriptions, tool selection layer | opt-in: allow/deny (S), trim (S), deferred mode (L) | S/S/L | `Router::list_tools` (`router/lists.rs:501`) | 0 |
| 4 | Intelligent pagination | opt-in `default_args` (S); result paging via 7 | S | `call_tool`, before the forward | 7 for paging |
| 5 | Session result cache | opt-in per tool, strict | M | `call_tool`, both sides | result store |
| 6 | TOON / compact encoding | opt-in: `minify_json` (S), `toon` (M) | S/M | result transform | A2 |
| 7 | Summarise large responses, `get_full_content` | opt-in truncate plus retrieve; no summarising | M | result transform and meta-tool | result store |
| 8 | Batch tool calls | opt-in meta-tool, read-only tools by default | M | meta-tools | meta-tool framework |
| 9 | Selective context injection | out of scope (agent side) | none | none | none |
| 10 | Dedupe / normalise call patterns | out of scope for behaviour; implement a `dup` metric | S | `usage.rs` | 0 |

### 3.2 Shared design

- **One choke point.** `tools/call` for both protocol eras goes through `Router::call_tool` (`router/forward.rs:126`); `handle_modern` delegates to `request` (`router/modern.rs:160`). Lists go through `Router::list_tools`.
- **View-time transforms.** New module `crates/mcpdoze-core/src/router/optimize/`: `mod.rs` (compiled per-server policy), `view.rs` (list transforms), `result.rs` (result transforms), `store.rs` (result store), `meta.rs` (meta-tools), `toon.rs`. The disk catalog keeps the backend's raw lists, so a catalog refresh re-derives everything (avoids "compress once, forget forever").
- **Meta-tools** live under the reserved prefix `mcpdoze` (`mcpdoze__result_get`, `mcpdoze__batch`, ...). They are intercepted in `call_tool` before `named()` and listed only when a feature that needs them is enabled.
- **Config** (both `ServerConfig` and the new structs use `deny_unknown_fields`):

```toml
[servers.example.optimize]
tools_allow = ["search*", "get_*"]   # 3a; globs on backend tool names
tools_deny = []
description_max_chars = 240          # 3b
strip_schema_descriptions = false    # 3b
minify_json = false                  # 6a
encode = "none"                      # 6b: none | toon
max_result_bytes = 0                 # 7; 0 = off
cache = { ttl = "60s", tools = ["get_user"], on_hit = "full" }  # 5; full | reference
batch = "off"                        # 8: off | read-only | all

[servers.example.tools.list_issues]
default_args = { per_page = 20 }               # 4
keep = ["/items/*/id", "/items/*/title"]       # 1; JSON-pointer globs (or drop = [...])
```

- **Reload.** `optimize` and `tools` must not enter `catalog::fingerprint` (`router/catalog.rs:380`) and must not unload the backend. A change that affects lists must send `notifications/tools/list_changed`.
- **Global protocol rules** for every result transform:
  - never touch a result with `isError: true` or a JSON-RPC error;
  - never touch non-text content blocks;
  - if a transform changes or removes `structuredContent`, drop that tool's `outputSchema` from the listed view;
  - if the text block is the JSON serialisation of `structuredContent`, keep the two consistent;
  - if the transformed result is not smaller, send the original.
- **Savings must be measurable.** `Router::forward` records the call before any transform, so `bytes_out` is the raw size. Split the recording so `call_tool` finishes it after transforms and adds `bytes_sent`, `opt` (applied transform tags) and `cached`.

### 3.3 Per-item implementation

**0. Measurement (implement).**
- No tokenizer dependency: report `est_tokens = bytes / 4`, labelled as an estimate.
- `SummaryBuilder` (`stats.rs:858`) gains a per-tool table: calls, average and p95 `bytes_out`, total bytes, share of server total, `dup_calls`, `bytes_saved`.
- `mcpdoze stats --tools [--top N]`. `GET /admin/v1/stats` gains `tools` and, per server, `list_bytes` (catalog tool-list size after view transforms, the per-request schema overhead of technique 3).
- No new private data: tool names are already written.

**1. Filter outputs (opt-in per tool).**
- Projection with `keep` or `drop` pointer-globs, applied to `structuredContent` and to text blocks that parse as JSON.
- Risk: over-filtering. Mitigations: explicit config only, never inferred; append one marker line (`[mcpdoze: fields filtered]`).
- `outputSchema` is dropped from the listing for filtered tools.

**2. Server-side computation (out of scope).**
- The broker does not know tool semantics and must not run agent-supplied code.
- Instead: `mcpdoze__result_get(ref, pointer?, offset?, limit?)` over the result store lets an agent slice or page a large stored result. Aggregation tools remain the server author's job.

**3. Descriptions and tool selection (opt-in, three steps).**
- **3a allow/deny (S).** Filter in `list_tools`. A denied tool still called returns `-32602 unknown tool`.
- **3b trim (S).** `description_max_chars` cuts at a sentence boundary. `strip_schema_descriptions` removes `description`, `title` and `examples` inside `inputSchema` only. Never rename parameters (needs call translation, breaks agent allowlists).
- **3c deferred mode (L, last).** `[broker] tool_mode = "search"` or per-server `expose = "deferred"`: list only `mcpdoze__search_tools`, `mcpdoze__describe_tool`, `mcpdoze__call_tool`. Risks: per-tool allowlists and approval prompts see only `call_tool`; `outputSchema` is lost; agents with native tool search gain nothing. Needs ADR 0003 before any code.

**4. Pagination (opt-in).**
- The broker cannot invent pagination a tool lacks. `default_args` are merged into `arguments` only for keys the agent omitted, and validated against the catalog `inputSchema` at config load (unknown key is an error).
- Paging `tools/list` itself saves nothing: skip. Result paging comes from technique 7.

**5. Session cache (opt-in, strict).**
- Eligible only if the tool is in `cache.tools` and has `annotations.readOnlyHint == true` in the catalog.
- Key: (server, instance scope key, session id, tool, canonical-args hash). Stateless 2026-07-28 requests have no session, so no cache for them in v1.
- Invalidation: TTL; any non-read-only call to the same server; backend respawn (generation change); `list_changed`; `notifications/resources/updated`.
- `on_hit = "full"` (default) saves backend work, not tokens. `"reference"` returns a stub plus a `ref` for `mcpdoze__result_get`; allowed only for tools without `outputSchema`, and the stub says the content equals an earlier result.
- Memory only, byte-capped, never on disk.

**6. Compact encoding (opt-in).**
- **6a `minify_json` (S).** Re-serialise pretty-printed JSON text blocks compactly. Lossless.
- **6b `encode = "toon"` (M).** Real TOON uses tabular headers for uniform arrays, not the article's pipe example. Small in-tree encoder (`toon.rs`) to avoid an unvetted dependency under `cargo-deny`. Encode only when the round-trip is lossless and the saving is at least 20 %. Text block only; `structuredContent` stays unless the user also sets `drop_structured`, which strips `outputSchema` from the listing.

**7. Large responses (opt-in truncate plus retrieve).**
- The broker has no model, and summarising through client sampling would spend the tokens it is trying to save.
- `max_result_bytes`: keep the head (and a short tail) of text blocks, append `[mcpdoze: truncated, N of M bytes; call mcpdoze__result_get with ref=...]`.
- **Result store** (`store.rs`): in memory, 128-bit random refs bound to the session id, TTL 15 min, capped by count and bytes with LRU eviction, purged on session close. A lost ref returns a clear error telling the agent to re-run the tool. Shared by techniques 2, 4 and 5.

**8. Batch (opt-in meta-tool).**
- `mcpdoze__batch({calls: [{name, arguments}]})`: concurrency 4, each inner call through the normal `call_tool` path so stats, cancellation and server-to-client routing still work. One text block per call with index and error flag.
- Main risk: it bypasses per-tool approval. Default when enabled is `read-only` (tools with `readOnlyHint`). Nested batch rejected; no `outputSchema`. Low priority: agents already issue parallel calls.

**9. Selective context injection (out of scope).**
- The broker never sees the agent's context. Technique 7 makes pruning cheap because pruned content stays retrievable; the per-tool report from item 0 shows what to prune.

**10. Dedupe (metric only).**
- Per-session ring of the last 32 (tool, args-hash) pairs, in memory only. The call event gets `dup: true`; the hash is never written (avoids the guess-confirm weakness `docs/STATS.md` documents for unsalted hashes). Mitigation is technique 5.

### 3.4 Phases and acceptance tests

New mock knobs in `crates/mcpdoze-testkit/src/bin/mock_mcp_stdio.rs` and `src/lib.rs` (`MockServerSpec`): `--big-items N` (tool returning an N-element uniform, pretty-printed JSON array); `--structured` (tool with `outputSchema` and `structuredContent`); `--read-only TOOLS` (sets `readOnlyHint`); a `counter` tool returning an incrementing number; `--verbose-descriptions`. Tests in new `crates/mcpdoze-testkit/tests/optimize.rs`.

| Phase | Contents | Acceptance |
|---|---|---|
| A0 | Item 0, `dup` metric | `stats --tools --json` ranks the `big` tool first; a repeated identical call sets `dup`; no args hash in the stats file |
| A1 | 3a, 3b, 4 `default_args`, 6a | Denied tool absent from `tools/list` with zero backends running and a call gives -32602; `list_bytes` drops; a config change emits `list_changed` without a respawn (same pid); a `default_args` value reaches the mock only when the agent omitted it |
| A2 | Result store, 7, `mcpdoze__result_get`, 1 | Truncated result plus `result_get` pages reassemble byte-identical; a ref from session A is rejected in session B; a filtered tool lists no `outputSchema`; `isError` results untouched |
| A3 | 5, 6b | `counter` returns the same value within the TTL and a new one after a write call or respawn; a non-read-only tool is never cached; TOON round-trips on `big` and falls back when not smaller |
| A4 | 8, 3c (after the ADR) | Batch of three is three `call` events and one backend pid; a write tool is refused in `read-only`; cancelling the batch cancels the inner calls |

## 4. Part B: failure diagnostics

### 4.1 Module and modes

- New `crates/mcpdoze-core/src/diag/`: `mod.rs` (`Diag` recorder: bounded queue plus one writer thread, the `stats.rs` pattern), `record.rs` (schema), `shape.rs`, `redact.rs`, `classify.rs`, `ring.rs` (per-instance frame and transition rings), `export.rs`.
- Modes: `off` (default), `failures`, `trace` (also slow and odd-but-successful calls and noise), plus a separate `raw` flag.
- Each `Instance` holds an `Arc<DiagTap>` with an `AtomicU8` level.

### 4.2 Capture points

| Point | Code | Captured | Kinds |
|---|---|---|---|
| Spawn | `Instance::run_start` (`lifecycle.rs:531-539`) | `io::ErrorKind`, launcher basename, `.cmd`-shim flag | `spawn_failed` |
| Start timeout | `run_start` timeout arm (`:545`) | phase reached (spawned, init sent, any stdout bytes, init replied), stdout noise count | `init_timeout`, `init_no_output` |
| Handshake | `StdioConn::initialize` (`stdio.rs:212`), `HttpConn` | requested and returned `protocolVersion`, `serverInfo` name/version, capability keys, JSON-RPC error | `init_error`, `version_mismatch` |
| Exit | `exit_watch` (`stdio.rs:384`), `on_closed` (`lifecycle.rs:684`) | exit code, Unix signal, Windows NTSTATUS name, uptime, calls served | `exit_unexpected` |
| Crash mid-call | `fail_all` (`stdio.rs:370`) into `forward_traced` (`forward.rs:331`) | one record per in-flight call, linked by `incident`; last request sent flagged `suspect` | `crash_in_flight` |
| Stderr | `StderrRing` (`stdio.rs:107`) | sequence number and timestamp per line; tail since spawn or last N lines | attached to records |
| Framing | `read_loop` (`stdio.rs:489-507`) | oversize line (bytes consumed, pending count), non-JSON line (length, signature class), invalid JSON-RPC | `frame_oversize`, `stdout_pollution`, `frame_invalid` |
| Id mapping | `dispatch` (`stdio.rs:516-522`) | id JSON type, late response versus unknown id | `id_type_mismatch`, `late_response` |
| JSON-RPC errors | `forward_traced` after outcome | code, scrubbed message, shape of `data`; `isError` results only with `tool_errors = true` | `rpc_error` |
| Time | `forward_traced`; diag sweeper over in-flight calls | `slow_call` threshold (state snapshot, last-frame age), agent cancel, agent disconnect (`CancelOnDrop`) | `stalled`, `agent_cancelled`, `agent_disconnected` |
| Lifecycle races | `acquire`, `begin` when `pending.closed` (`stdio.rs:303`), leftover connection (`lifecycle.rs:574`) | last 16 transitions (state, reason, in-flight, age) | `race_unload`, `backoff_rejected` |
| HTTP | exchange in `backend/http.rs:338-357`, `:482`, `:618` | status, content-type, body length and class (json, sse, html, empty), `www-authenticate` scheme, `retry-after`, session present (bool), transport error class, whether the helper was re-run | `http_status`, `http_auth`, `http_non_mcp_body`, `http_session_lost`, `http_timeout`, `sse_truncated`, `connect_failed` |
| Server-to-client | `router/events.rs`, `session.rs:277` | method, routed, unsupported or timed out | `client_request_unserved` |
| Listing | `list_all` (`router/lists.rs:40-75`) | `LIST_TIMEOUT`, page cap, cursor repeat | `list_failed`, `list_cursor_loop` |

Stdout pollution signatures: `Content-Length:` header framing, UTF-8 BOM, NUL bytes (UTF-16, PowerShell), ANSI escapes, npm or pip banner, a lone `{` (pretty-printed multi-line JSON), plain text log line.

Plain bug fixes alongside (independent of the mode): expose the exit status and put it in `last_error` ("backend exited, code 1"); on an oversize frame fail the connection's pending calls; accept string ids that parse to a pending id; add optional `[servers.x] call_timeout`.

### 4.3 Record schema (`v: 1`, one JSON object per line)

```json
{"v":1,"ts":1767225600123,"id":"f-000123","incident":"i-4-7c1e","kind":"crash_in_flight","repeat":1,
 "server":"example","transport":"stdio","launcher":"npx","scope":"shared","instance":null,"generation":4,
 "backend":{"state":"failed","prev_state":"running","uptime_ms":8123,"in_flight":2,"pending":2,
   "calls_served":17,"restarts":3,"failures":1,"last_transition":{"to":"running","age_ms":8123,"reason":"spawn"},
   "protocol":"2025-11-25","server_info":{"name":"example-server","version":"1.2.3"},"caps":["tools","resources.subscribe"]},
 "request":{"req":881,"era":"legacy","method":"tools/call","tool":"search","id_type":"number","progress":true,
   "bytes":2311,"elapsed_ms":412,"acquire_ms":0,"cold_start":false,"suspect":true,
   "shape":{"query":{"t":"string","len":1832,"cls":"multiline"},"limit":{"t":"int","digits":2},"cursor":{"t":"null"}},
   "schema_diff":{"missing_required":[],"extra_keys":0,"type_mismatch":["/cursor: null, schema says string"]}},
 "exit":{"code":1,"signal":null,"status_name":null},
 "error":{"origin":"broker","code":-32000,"message":"backend connection closed"},
 "frames":[{"dt_ms":-412,"dir":"out","type":"request","method":"tools/call","bid":18,"bytes":2311},
           {"dt_ms":-3,"dir":"in","type":"noise","bytes":57,"sig":"text"}],
 "stderr":{"total":12,"tail":["TypeError: Cannot read properties of null (reading 'length')","    at <path>:41:17"]},
 "cause":{"guess":"server crashed on a null optional argument","confidence":"medium","hints":["/cursor is null; schema type is string"]},
 "broker":{"version":"0.2.1","os":"windows","arch":"x86_64"}}
```

- `req` is the same id as STATS `req_id`: move the counter from `Stats::next_request_id` to `Router` so both logs correlate.
- `schema_diff` compares the arguments with the catalog `inputSchema` (null versus absent, extra keys, wrong types). It is the most diagnostic field.
- `classify.rs` is a pure function from (kind, exit, signature, schema_diff, phase) to `cause`. Examples: `stdout_pollution` with a `Content-Length` signature -> "server uses LSP-style framing"; `init_timeout` with `launcher = npx` and no stdout -> "package download; raise `start_timeout`"; signal 9 -> "killed, possibly out of memory"; `http_non_mcp_body` with html -> "URL is not the MCP endpoint, or a proxy or login page answered".

### 4.4 Redaction rules

Enforced by types: `record.rs` has no field that can hold a raw `Value`.

| Data | Rule |
|---|---|
| Argument and result values | Never stored. Shape only: type, length, class (path, url, json, base64, multiline, ascii), integer digit count. Booleans and schema-declared `enum` members kept. |
| Object keys | Kept only if declared in the tool's public `inputSchema`; otherwise `k#<hash8>`. |
| Env and header values | Never recorded; names only. Resolved values in `BackendSpec.env` and headers, and the broker token, are exact-match scrubbed from all free text (`<env:NAME>`). |
| Free text (stderr, error messages) | Scrubber: home prefixes become `<home>`; user name, host name and per-cwd directory become placeholders; any string argument of the triggering call found in the text becomes `<arg:/pointer>` (bounded in-memory set of at most 32 values and 4 KiB, never written); `Bearer ...`, JWTs, known token prefixes, emails, IPs, URL query strings and hex/base64 runs of 24+ become `<redacted:len>`. Stderr lines that do not look like errors (`Error`, `at ...`, `Traceback`, `panic`, `E[A-Z]+`, `npm ERR!`) become `<line len=N>`. Truncate to 300 characters. |
| URLs | Scheme, host hash, path depth. Never the query. |
| Resource URIs, session ids, directories | Salted hash: HMAC with a per-install salt in the data dir, never exported. Export re-keys the hashes. |
| `serverInfo`, tool names, method names | Kept (public). |
| Raw capture | Only with `raw = true`: full frames to `<debug dir>/raw/<server>.jsonl`, owner-only, auto-off after `raw_for` (default 30 min), deleted after 24 h, never read by `export`; `doctor` warns while on. Still holds no env or header values. |

### 4.5 Enabling

Precedence: runtime, then CLI, then env, then per-server, then global.

```toml
[debug]
mode = "off"          # off | failures | trace
dir = "debug"         # relative to the data dir
max_file_bytes = 5242880
keep = 5
frames = 32
stderr_lines = 40
slow_call = "30s"
tool_errors = false

[servers.example]
debug = "trace"       # overrides the global mode
```

- Env: `MCPDOZE_DEBUG=failures|trace`, or a comma list of server names (meaning `failures` for those).
- CLI: `serve --debug[=MODE] [--debug-server NAME]...` in `ServeArgs` (`crates/mcpdoze/src/run.rs`, shared with `mcpdozed`).
- Runtime (not persisted, reverts on restart): `GET /admin/v1/debug`; `POST /admin/v1/debug` with `{"mode":"failures","servers":["example"],"raw":false,"for":"30m"}`; `GET /admin/v1/debug/failures?server=&kind=&since=&limit=`.
- `[debug]` and `servers.*.debug` apply on reload without unloading backends.

### 4.6 Storage

- `<data dir>/debug/failures.jsonl`, rotated by size (5 MB, 5 kept). Move `RotatingLog` from `crates/mcpdoze/src/logfile.rs` into `mcpdoze-core` and re-export it.
- Owner-only files: generalise `create_private` (`auth/mod.rs:115`) into a shared helper. Directory 0700 on Unix.
- Flood control: 20 records per minute per server, then one `suppressed` record with a count. An identical signature (kind, tool, shape hash, exit) bumps `repeat` on a compact line instead of a full record.

### 4.7 CLI

- `mcpdoze debug on [SERVER...] [--trace] [--raw] [--for 30m]`, `debug off`, `debug status`, `debug show [--server S] [--kind K] [--last N]`.
- `mcpdoze debug export [--server S] [--since 24h] [--out FILE|--stdout] [--strict] [--identify]`:
  - reads the files directly after asking the broker to flush, so it works with the broker down (as `stats` does);
  - emits one text file (JSON in a markdown fence, pasteable into an issue; no zip dependency);
  - contents: manifest (versions, OS, redaction counts); server launch shape (transport, launcher basename, flag names, argument count, env and header names); catalog summary; failure records; per-kind counts and p50/p95 from stats; a `replay` section;
  - `--identify` adds the package argument; `--strict` drops all free text;
  - a second scrub pass at export, then a self-check with the rule set of `scripts/pii-lint.sh` (home paths, user name, emails, token patterns). Any hit fails closed with "re-run with --strict".
- `doctor` (`crates/mcpdoze/src/commands/doctor.rs`) gains a debug-mode line, a `warn` if raw capture is on, and per server with failures in the last 24 h: counts by kind, the top `cause.guess` as a `hint:`, and the export command.

### 4.8 Ties to STATS and the admin API

- `call` event: add `fail` (the kind) when classified. `unload` event: add `exit_code` and `signal` for crashes.
- `Summary` and `GET /stats`: per-server `failures: {kind: n}`.
- `GET /status` instances: add `debug` (mode) and `last_failure {id, kind, ts}`. `last_error` gains the exit code.
- `mcpdoze logs` is unchanged (raw stderr, loopback and token only). Document that it is not safe to paste, unlike `debug export`.

### 4.9 Replay

- New scripted mode for the mock: `mock_mcp_stdio --script FILE`. Steps are `{on: {method, tool?, nth?}, do: [...]}`, with actions `respond`, `stdout_raw`, `stderr`, `delay_ms`, `exit {code}`, `oversize {bytes}`, `string_id`, `content_length_framing`, `pretty_json`.
- `mcpdoze_testkit::replay::from_bundle(path)` builds a `MockServerSpec` and synthetic requests with the same types, lengths and classes as the recorded shapes, runs them through an in-process broker (`tests/support/harness.rs`) and asserts the same `kind`.
- A sanitised bundle becomes a regression fixture under `crates/mcpdoze-testkit/tests/fixtures/failures/`.
- Limit: replay reproduces the broker-visible symptom (framing, timing, exit), not the server's internal bug.

### 4.10 Overhead

- **Off:** one relaxed atomic load per frame and per call; no allocation, no directory created.
- **Failures:** per frame, a fixed-size `FrameMeta` pushed into a per-instance ring. Per call, a shape walk capped at 200 nodes and depth 6, plus the small argument-string set. Disk writes only on failure, off the request path, dropped (and counted) when the queue is full.
- **Trace:** adds records for slow calls and noise. **Raw:** one serialisation per frame.

### 4.11 Tests

- **Unit (`diag/`):** shape goldens; classifier table; seeded property-style scrubber test (no planted secret, home path or argument substring survives); rotation, rate limit; compile-level proof that record types hold no `Value`.
- **Integration (new `crates/mcpdoze-testkit/tests/diag.rs`), one test per kind:**
  - existing knobs: `garbage_on_start`, `crash_after`, `startup_delay_ms` above `start_timeout`, a missing program, HTTP `reject_calls`, `slow_ms`, `drop_session_after`;
  - new stdio knobs: `--exit-code`, `--oversize-response`, `--string-ids`, `--content-length-framing`, `--init-error`, `--init-version`, `--pretty-json`, `--stderr-leak` (prints an env value and the call arguments to stderr);
  - new HTTP knobs: `--html-error`, `--status N`.
- **Canary test** (style of `tests/env_hygiene.rs` and `tests/support/log_capture.rs`): canaries in env, header, argument and cwd are absent from `failures.jsonl` and from the export, and present only in the raw file when raw is on.
- **Off test:** no debug directory, tap counters stay at zero.
- Admin toggle tests in `tests/admin.rs`, CLI parse tests in `crates/mcpdoze/src/cli.rs`, export self-check test.

## 5. Work breakdown

Tiers: `haiku` mechanical work, `sonnet` core Rust, `fable` design reviews only. Every task: SPDX header in new source files, `cargo fmt`, `cargo clippy -D warnings`, `cargo test`; no machine data in fixtures.

| ID | Tier | Task | Files | Acceptance | Deps |
|---|---|---|---|---|---|
| R1 | fable | Review diag record schema and redaction rules; confirm default mode and stderr policy | plan and `docs/DEBUG.md` | Signed-off schema v1 with a field-by-field leak argument | none |
| B1 | sonnet | Exit status plumbing: `ProcessTree::exit_status()`, through `exit_watch`, `on_closed`, `last_error`, `UnloadEvent` | `proctree/mod.rs`, `backend/stdio.rs`, `backend/lifecycle.rs`, `stats.rs`, `router/usage.rs` | `status` shows "exited, code N"; unload event carries `exit_code`; three-OS tree tests pass | none |
| B2 | sonnet | Framing fixes: fail pending on oversize frame; accept string ids; count noise; check init version | `backend/stdio.rs` | Mock `--oversize-response` and `--string-ids` return promptly | B9 |
| B3 | sonnet | `diag` core: recorder, writer, rotation, rate limit, `DiagTap`, rings | `diag/*`, `lib.rs`, move `crates/mcpdoze/src/logfile.rs` | Unit tests; off mode allocates nothing | R1 |
| B4 | sonnet | `shape.rs`, `redact.rs`, `classify.rs` | `diag/*` | Golden and property tests pass | R1 |
| B5 | sonnet | Wire stdio and lifecycle capture points | `backend/stdio.rs`, `backend/lifecycle.rs`, `router/forward.rs`, `router/events.rs`, `router/lists.rs` | `tests/diag.rs` stdio kinds pass | B1, B3, B4 |
| B6 | sonnet | Wire HTTP capture points; `XErr` carries response metadata | `backend/http.rs` | HTTP kinds pass | B3, B4 |
| B7 | haiku | Config `[debug]`, `servers.*.debug`, env, `serve --debug`, reload without unload | `config/mod.rs`, `config/tests.rs`, `crates/mcpdoze/src/run.rs`, `daemon.rs`, `admin/mod.rs` | Parse tests; reload keeps the pid | B3 |
| B8 | sonnet | Admin endpoints and types; `status`/`stats` additions; shared `req` counter | `admin/mod.rs`, `admin/api.rs`, `stats.rs`, `router/usage.rs` | `tests/admin.rs` toggle test; `last_failure` in status | B5 |
| B9 | haiku | Mock knobs and `--script` mode; `MockServerSpec`/`MockHttpSpec` builders | `mcpdoze-testkit/src/bin/*.rs`, `src/lib.rs`, `tests/mock.rs` | Each knob has a `mock.rs` test | none |
| B10 | haiku | Integration test boilerplate, one per kind, plus canary and off tests | `mcpdoze-testkit/tests/diag.rs` | All green on CI targets | B5, B6, B9 |
| B11 | sonnet | `mcpdoze debug on/off/status/show/export`; export scrub and self-check; doctor summary | `crates/mcpdoze/src/cli.rs`, `commands/mod.rs`, new `commands/debug.rs`, `commands/doctor.rs`, `client.rs`, `diag/export.rs` | Export of the canary run passes the self-check; `--strict` has no free text | B8 |
| B12 | sonnet | Replay helper and fixture loader | `mcpdoze-testkit/src/lib.rs` (new `replay`), `tests/diag.rs`, `tests/fixtures/failures/` | A bundle from B10 replays to the same kind | B9, B11 |
| B13 | sonnet | Optional `call_timeout` per server | `config/mod.rs`, `backend/mod.rs`, `router/forward.rs` | Timed-out call returns -32000 and a `stalled` record | B5 |
| R2 | fable | Review leak tests and a real export before release | none | Written sign-off | B10, B11 |
| A0 | sonnet | Per-tool summary, `est_tokens`, `list_bytes`, `dup`, `stats --tools` | `stats.rs`, `stats/tests.rs`, `router/usage.rs`, `admin/api.rs`, `commands/mod.rs`, `cli.rs` | Phase A0 acceptance | none |
| R3 | fable | Review transform pipeline: protocol compliance, reload semantics, reserved prefix | none | Rules in 3.2 confirmed or amended | none |
| A1 | haiku | Config structs `optimize` and `tools.<name>`; validation (`default_args` against schema, globs) | `config/mod.rs`, `config/tests.rs` | Parse and validation tests | R3 |
| A2 | sonnet | `optimize` skeleton, view transforms (3a, 3b), `default_args`, `minify_json`, record after transform, `list_changed` on reload | `router/optimize/*`, `router/lists.rs`, `router/forward.rs`, `router/usage.rs`, `admin/mod.rs` | Phase A1 acceptance | A0, A1 |
| A3 | haiku | Mock knobs for Part A and `tests/optimize.rs` scaffolding | `mcpdoze-testkit/src/bin/mock_mcp_stdio.rs`, `src/lib.rs`, `tests/optimize.rs` | Knob tests | none |
| R4 | fable | Review result store security (ref binding, eviction, stateless era) and meta-tool surface | none | Sign-off | A2 |
| A4 | sonnet | Result store, truncation, `mcpdoze__result_get`, field filter | `router/optimize/store.rs`, `meta.rs`, `result.rs` | Phase A2 acceptance | R4, A3 |
| A5 | sonnet | Cache (5) | `router/optimize/cache.rs`, `router/forward.rs`, `router/events.rs` | Phase A3 cache acceptance | A4 |
| A6 | sonnet | TOON encoder (6b) | `router/optimize/toon.rs` | Round-trip and fallback tests | A2 |
| A7 | sonnet | Batch meta-tool (8) | `router/optimize/meta.rs` | Phase A4 batch acceptance | A4 |
| R5 | fable | ADR 0003: deferred tool mode (3c), go or no-go | `docs/decisions/0003-deferred-tools.md` | Decision recorded | A0 data |
| A8 | sonnet | Deferred mode, only if R5 says go | `router/optimize/meta.rs`, `router/lists.rs` | Three meta-tools listed; calls route; allowlist caveat documented | R5, A4 |
| D1 | haiku | Docs | `docs/*`, `README.md` | `scripts/pii-lint.sh` clean; examples use placeholders | per feature |

## 6. Docs to update

- `docs/DEBUG.md` (new) and `docs/OPTIMIZE.md` (new): design drafts exist; finalise per feature.
- `docs/CONFIGURATION.md`: `[debug]`, `servers.*.debug`, `call_timeout`, `[servers.*.optimize]`, `[servers.*.tools.*]`, `MCPDOZE_DEBUG`.
- `docs/ADMIN_API.md`: `/debug` endpoints, new `status` and `stats` fields.
- `docs/STATS.md`: `fail`, `exit_code`, `bytes_sent`, `opt`, `cached`, `dup`, per-tool summary.
- `docs/TROUBLESHOOTING.md`: "A server fails behind MCPDoze" (enable, reproduce, `doctor`, export); `logs` versus `debug export`.
- `docs/ARCHITECTURE.md`: `diag` and `router/optimize` in the crate list; a security bullet on the failure log.
- `SECURITY.md`: raw capture warning.
- `docs/DEVELOPMENT_PLAN.md`: M9 and M10 milestone rows and a v0.3 section (separate task).
