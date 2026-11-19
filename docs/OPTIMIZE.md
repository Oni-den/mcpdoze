# Token optimisation

**Status: implemented in this release: measurement, tool allow/deny, description trimming, `default_args`, `minify_json`, truncation with `result_get`, field filters, `encode = "toon"`, the session result cache and the batch meta-tool.** Details and rationale are in `docs/plans/M9-M10-plan.md`.

A broker sits between the agent and the MCP server, so it can reduce what reaches the agent's context without touching either side. Everything here is **opt-in per server**. A server with no `optimize` table behaves exactly as today.

Transforms are applied at view time. The disk catalog keeps the backend's own lists, so a catalog refresh re-derives every transform.

## What is and is not in scope

| Technique | MCPDoze |
|---|---|
| Measure bytes per tool | yes, on by default (`stats --tools`) |
| Filter result fields | yes, opt-in per tool |
| Server-side computation | no (server author's job); `mcpdoze__result_get` slices stored results |
| Trim descriptions, hide tools, deferred tool mode | yes, opt-in (deferred mode needs a decision record first) |
| Pagination | `default_args` only; the broker cannot invent paging a tool lacks |
| Session result cache | yes, read-only tools only |
| Compact encoding | JSON minify, and a TOON-style text encoding |
| Large results | truncate plus retrieve; no model-based summarising |
| Batch calls | opt-in meta-tool, read-only tools by default |
| Pruning the agent's context | no (the broker never sees it); duplicate calls are reported as a metric |

## Configuration

```toml
[servers.example.optimize]
tools_allow = ["search*", "get_*"]   # globs on backend tool names
tools_deny = []
description_max_chars = 240          # cut at a sentence boundary
strip_schema_descriptions = false    # removes description/title/examples inside inputSchema only
minify_json = false                  # re-serialise pretty-printed JSON text blocks
encode = "none"                      # none | toon
max_result_bytes = 0                 # 0 = off
cache = { ttl = "60s", tools = ["get_user"], on_hit = "full" }   # full | reference
batch = "off"                        # off | read-only | all

[servers.example.tools.list_issues]
default_args = { per_page = 20 }               # merged only for keys the agent omitted
keep = ["/items/*/id", "/items/*/title"]       # JSON-pointer globs (or drop = [...])
```

Both tables reject unknown keys. `default_args` keys are validated against the tool's catalog `inputSchema` at config load. Changing `optimize` or `tools` does not unload the backend; it sends `notifications/tools/list_changed` when the change affects lists.

## Protocol rules

Every result transform follows these rules:

- A result with `isError: true`, or a JSON-RPC error, is never changed.
- Non-text content blocks are never changed.
- If a transform changes or removes `structuredContent`, the tool's `outputSchema` is dropped from the listed view so a validating client does not reject the result.
- If the text block is the JSON form of `structuredContent`, the two stay consistent.
- If the transformed result is not smaller, the original is sent.
- Parameter names are never renamed.

## Measuring

`mcpdoze stats --tools [--top N]` reports per tool: calls, average and p95 bytes out, total bytes, share of the server total, duplicate calls and bytes saved. `est_tokens` is `bytes / 4` and is an estimate. Per server, `list_bytes` is the size of the listed tool catalog after transforms. Duplicate detection keeps a small in-memory ring per session; hashes are never written.

## Meta-tools

Meta-tools use the reserved prefix `mcpdoze` and appear in `tools/list` only when a feature needs them.

- `mcpdoze__result_get(ref, pointer?, offset?, limit?)`: retrieve or slice a stored result. Refs are random, bound to the session, expire after 15 minutes, and are lost when the session closes (the error tells the agent to re-run the tool).
- `mcpdoze__batch({calls: [{name, arguments}]})`: see "Batch" below.
- Deferred mode (`mcpdoze__search_tools`, `mcpdoze__describe_tool`, `mcpdoze__call_tool`) is not committed. It hides per-tool approval from the client and loses `outputSchema`; ADR 0003 decides.

## Batch

`mcpdoze__batch({calls: [{name, arguments}, ...]})` runs several tool calls in one agent request. It is listed in `tools/list` only while at least one server has `batch` other than `off`. It has no `outputSchema`.

- `batch = "off"` (default): the server's tools cannot be batched. `"read-only"`: only tools whose catalog entry has `annotations.readOnlyHint = true` (a tool the catalog does not know yet is refused). `"all"`: any tool; this bypasses the agent's per-tool approval, so set it deliberately.
- The mode is checked per inner call against the server that owns the tool. A refused call reports an error in its own block; the other calls still run.
- Limits: 1 to 16 calls per batch (otherwise the whole batch is rejected with an error result), four in flight at once. Names are the full listed names (`server__tool`). Calls to `mcpdoze__*` tools (so nested batches) are refused. A tool hidden by `tools_deny`/`tools_allow` is refused as `unknown tool`.
- Every inner call goes through the normal call path, so tool policy, `default_args`, result transforms, the cache, cancellation, server-to-client requests and diagnostics apply to it, and each is recorded in the statistics log as usual.
- The result has one text block per call, in order: `[<index>] <tool name> isError=<bool>`, a newline, then the call's text (non-text content is replaced by a note). The batch call itself is recorded once, with server `mcpdoze`, tool `batch` and opt tag `batch`; its bytes are the batch result only, so inner bytes are not counted twice.
- Cancelling the batch request, or ending the session, cancels the inner calls.

## Result cache

```toml
cache = { ttl = "60s", tools = ["get_user"], on_hit = "full" }   # full | reference
```

- `ttl` (required, greater than zero), `tools` (required, non-empty globs on backend tool names), `on_hit` (`full` default, or `reference`). Unknown keys are rejected.
- Eligible: a tool that matches `cache.tools` AND is annotated `readOnlyHint = true` in the catalog. Others are never cached. A call of a tool that is not known to be read-only (annotation missing or false) invalidates the server's entries before and after it runs.
- Key: server, instance scope, session, tool and the canonical arguments (a hash is kept, never the arguments). Sessions never share entries. Stateless (2026-07-28) requests have no session and are not cached.
- Only plain results are stored: JSON-RPC errors and `isError` results never are.
- Invalidation: TTL expiry, any non-read-only call to the same server, a backend respawn (process unload), a tool list change, `notifications/resources/updated`, a changed server policy, and the end of the session.
- Memory only, never on disk; capped at 16 MiB of serialised results with least-recently-used eviction. Keys and values are not logged.
- `on_hit = "full"` returns the stored result. `on_hit = "reference"` returns a short stub naming a `mcpdoze__result_get` ref (the stored result is read from the result store, refs are session bound and expire like other refs); it is used only for tools without `outputSchema`, which get the full result. A hit is recorded with `cached = true` and the tag `cache`.
- Stale data is possible within the TTL; keep it short and cache only reference-style reads.
## TOON encoding

With `encode = "toon"` a text block that parses as JSON is replaced by a TOON-style text: objects as indented `key: value` lines, uniform arrays of flat objects as a table (`items[3]{id,name,status}:` then one comma-separated row per element), scalar arrays inline, other arrays as `- ` lists, and JSON-style quoting only where a bare string would be ambiguous. The broker applies it only when the text decodes back to exactly the same JSON (numbers exact, key order kept) and is at least 20% smaller; otherwise the original is sent. `structuredContent` is never changed, so `outputSchema` stays valid. Results with `isError`, and non-text blocks, are never touched. The tag is `toon`.

The agent sees TOON text, not JSON. A format description line is deliberately not prepended: the notation is regular enough for models to read, and a hint on every result would cost tokens that the encoding just saved. The tradeoff is that an agent may occasionally misread or mis-copy a TOON value, so leave `encode` off for tools whose output is fed verbatim into other tools or parsers. Large uniform lists (search hits, issue lists, rows) gain the most; small or deeply nested results usually fall back to JSON.

## Pitfalls

- Over-filtering leaves the agent without data it needs; filters are explicit and never inferred.
- Cached data can be stale; use short TTLs and only for reference-style reads.
- Re-audit after adding tools: transforms are derived from the live catalog, but your `keep` and `default_args` lists are not.
