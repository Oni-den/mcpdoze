# Failure log and debug mode

**Status: Planned (M9), not implemented.** This document describes the intended design. Details are in `docs/plans/M9-M10-plan.md`.

When a brokered MCP server fails, MCPDoze can record why: which request, with what shape, against which backend state, and how the failure unfolded. The records are used to fix MCPDoze's support for specific servers, and are safe to attach to a public issue.

## Modes

| Mode | Records |
|---|---|
| `off` (default) | nothing; one atomic load per frame and per call, no allocation, no directory created |
| `failures` | classified failure records |
| `trace` | failures plus slow calls and odd-but-successful calls |
| `raw` (separate flag) | full frames to a local-only file; time-limited |

The default mode is an open owner decision; the proposal is `off`, with `doctor` and `status` hinting to enable debugging when failures occurred.

## Enabling

Precedence: runtime, CLI, environment, per-server, global.

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
debug = "trace"
```

- Environment: `MCPDOZE_DEBUG=failures|trace`, or a comma list of server names.
- CLI: `serve --debug[=MODE] [--debug-server NAME]...`
- Runtime (reverts on restart): `GET` and `POST /admin/v1/debug`, `GET /admin/v1/debug/failures`.
- `mcpdoze debug on [SERVER...] [--trace] [--raw] [--for 30m]`, `debug off`, `debug status`, `debug show`.
- Config changes apply on reload without unloading backends.

## What is captured

Spawn failure, start timeout (and how far startup got), handshake and protocol-version problems, unexpected exit (exit code, signal), crash during an in-flight call, a scrubbed stderr tail, framing problems (oversize line, non-JSON on stdout, invalid JSON-RPC), JSON-RPC id type mismatches, JSON-RPC errors, stalled or cancelled calls, lifecycle races, HTTP failures (status, content type, body class, auth scheme, session loss, timeout), unserved server-to-client requests, and listing failures.

Each failure has a `kind` and a `cause` guess from a pure classifier, for example "server uses LSP-style framing" or "package download; raise `start_timeout`".

## Record format

One JSON object per line in `<data dir>/debug/failures.jsonl`, schema `v: 1`. Main fields: `kind`, `server`, `transport`, `backend` (state, uptime, in-flight count, protocol, server info, last transitions), `request` (method, tool, id type, size, timing, argument **shape**, and `schema_diff` against the tool's `inputSchema`), `exit`, `error`, `frames` (direction, type, size; never content), `stderr` tail, `cause`, `broker` (version, OS, arch). `req` matches the `req_id` in the stats log, so the two correlate.

## Redaction

The record types cannot hold a raw JSON value, so values cannot leak by construction.

- Argument and result values are never stored; only type, length, class and integer digit count.
- Object keys are kept only if declared in the tool's public `inputSchema`; others become `k#<hash8>`.
- Environment and header values are never recorded, only names; resolved values and the broker token are scrubbed from free text.
- Free text is scrubbed of home paths, user and host names, arguments of the triggering call, bearer tokens, JWTs, emails, IPs, URL queries and long hex/base64 runs; non-error stderr lines are reduced to a length; text is truncated to 300 characters.
- URLs keep scheme, host hash and path depth. Resource URIs, session ids and directories are salted hashes; the salt is per install and never exported.
- Raw capture writes full frames to an owner-only file, turns itself off after 30 minutes by default, is deleted after 24 hours, and is never read by `export`. It still contains no env or header values. `doctor` warns while it is on.

Files are owner-only, rotated at 5 MB with 5 kept, and flood-limited to 20 records per minute per server; identical failures bump a `repeat` counter.

## Export

```
mcpdoze debug export [--server S] [--since 24h] [--out FILE|--stdout] [--strict] [--identify]
```

Works with the broker down. Produces one text file (JSON in a markdown fence) with a manifest, the server's launch shape (transport, launcher basename, flag names, env and header names), a catalog summary, the failure records, per-kind counts and latency, and a replay section. `--identify` adds the package argument; `--strict` drops all free text. A second scrub runs at export followed by a self-check; any hit fails closed with a request to re-run with `--strict`.

## What is safe to paste

- `mcpdoze debug export` output: yes.
- `failures.jsonl`: designed to be safe, but prefer the export.
- `mcpdoze logs` (raw backend stderr): **no**. It is unscrubbed.
- The raw capture file: **never**.

## Replay

A sanitised bundle can be turned into a scripted mock server in the testkit (`mock_mcp_stdio --script`), with synthetic requests of the same types, lengths and classes, run through an in-process broker, and checked to produce the same `kind`. This reproduces the broker-visible symptom (framing, timing, exit), not the server's internal bug. Bundles become regression fixtures.

## Related changes

M9 also fixes hangs from oversize frames and string ids, surfaces the backend exit status in `status`, adds `fail`, `exit_code` and per-server failure counts to stats, and adds an optional per-server `call_timeout`.
