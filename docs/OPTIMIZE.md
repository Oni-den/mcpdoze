# Token optimisation

**Status: implemented in this release: measurement, tool allow/deny, description trimming, `default_args`, `minify_json`, truncation with `result_get`, field filters and `encode = "toon"`. Still planned (being finished): the result cache and the batch meta-tool.** The cache and batch sections describe the intended design. Details are in `docs/plans/M9-M10-plan.md`.

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
- `mcpdoze__batch({calls: [{name, arguments}]})`: up to 4 concurrent inner calls through the normal call path. With `batch = "read-only"` only tools with `readOnlyHint` are allowed. Nested batches are refused. Batching bypasses the agent's per-tool approval, so enable it deliberately.
- Deferred mode (`mcpdoze__search_tools`, `mcpdoze__describe_tool`, `mcpdoze__call_tool`) is not committed. It hides per-tool approval from the client and loses `outputSchema`; ADR 0003 decides.

## Caching rules

A tool is cached only if listed in `cache.tools` and annotated `readOnlyHint = true`. The key is server, scope, session, tool and canonical arguments. Entries are invalidated by TTL, any non-read-only call to the same server, a backend respawn, `list_changed`, and `notifications/resources/updated`. Stateless (2026-07-28) requests have no session and are not cached. Memory only, byte-capped, never on disk. `on_hit = "reference"` returns a stub plus a ref and is allowed only for tools without `outputSchema`.

## TOON encoding

With `encode = "toon"` a text block that parses as JSON is replaced by a TOON-style text: objects as indented `key: value` lines, uniform arrays of flat objects as a table (`items[3]{id,name,status}:` then one comma-separated row per element), scalar arrays inline, other arrays as `- ` lists, and JSON-style quoting only where a bare string would be ambiguous. The broker applies it only when the text decodes back to exactly the same JSON (numbers exact, key order kept) and is at least 20% smaller; otherwise the original is sent. `structuredContent` is never changed, so `outputSchema` stays valid. Results with `isError`, and non-text blocks, are never touched. The tag is `toon`.

The agent sees TOON text, not JSON. A format description line is deliberately not prepended: the notation is regular enough for models to read, and a hint on every result would cost tokens that the encoding just saved. The tradeoff is that an agent may occasionally misread or mis-copy a TOON value, so leave `encode` off for tools whose output is fed verbatim into other tools or parsers. Large uniform lists (search hits, issue lists, rows) gain the most; small or deeply nested results usually fall back to JSON.

## Pitfalls

- Over-filtering leaves the agent without data it needs; filters are explicit and never inferred.
- Cached data can be stale; use short TTLs and only for reference-style reads.
- Re-audit after adding tools: transforms are derived from the live catalog, but your `keep` and `default_args` lists are not.
