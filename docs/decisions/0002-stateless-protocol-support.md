# 0002: Accepting MCP 2026-07-28 (stateless) clients

Status: proposed. Recommendation: defer to v0.2; v0.1 stays on 2025-11-25 and fails modern probes in a way that makes dual-era clients fall back.

## Verified facts

Legend: P = primary source, S = secondary; V = verified (read directly), U = unverified.

1. Status: 2026-07-28 is the **current** revision. GitHub shows a final release (28 Jul 2026, "Latest") after a release candidate (29 May 2026). [P, V] https://modelcontextprotocol.io/specification/versioning , https://github.com/modelcontextprotocol/modelcontextprotocol/releases
2. `initialize` / `notifications/initialized` are removed; protocol-level sessions and `Mcp-Session-Id` are removed (SEP-2567, SEP-2575). [P, V] https://modelcontextprotocol.io/specification/2026-07-28/changelog
3. Every request carries `params._meta` keys `io.modelcontextprotocol/protocolVersion` (required), `.../clientCapabilities` (required), `.../clientInfo` (SHOULD). Results carry `_meta["io.modelcontextprotocol/serverInfo"]` (SHOULD). [P, V] changelog above; https://modelcontextprotocol.io/specification/2026-07-28/server/discover.md
4. Every POST MUST carry `MCP-Protocol-Version` equal to the `_meta` value, plus `Mcp-Method` (all requests) and `Mcp-Name` (tools/call, resources/read, prompts/get; Base64 `=?base64?..?=` sentinel for non-ASCII). Mismatch/missing: 400 + error `-32020 HeaderMismatch`. Unsupported version: 400 + `-32022 UnsupportedProtocolVersion` with `data.supported`/`requested`. Unknown method: 404 + `-32601`. [P, V] https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http.md
5. `server/discover` is mandatory; result has `supportedVersions`, `capabilities`, `instructions`, `ttlMs`, `cacheScope`. [P, V] discover.md above.
6. GET stream endpoint removed; servers SHOULD answer GET/DELETE with 405, ignore `Mcp-Session-Id` and `Last-Event-ID`. Change notifications come only via `subscriptions/listen` (a POST whose SSE response stays open; first event `notifications/subscriptions/acknowledged`; opt-in by `toolsListChanged`, `promptsListChanged`, `resourcesListChanged`, `resourceSubscriptions`; events tagged `io.modelcontextprotocol/subscriptionId`). `resources/subscribe` removed. [P, V] streamable-http.md, changelog.
7. Server-to-client requests (sampling, elicitation, roots) must not be sent on any stream. They become Multi Round-Trip Requests: the result is `{resultType:"input_required", inputRequests, requestState}` for `tools/call`, `prompts/get`, `resources/read` only; the client retries the original call with a new id plus `inputResponses` and the echoed `requestState`. All results carry `resultType` ("complete" default). [P, V] https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr.md
8. Cancellation on HTTP = closing the request's response stream; `notifications/cancelled` is stdio-only. Progress and `notifications/message` flow only on the originating request's SSE response stream. [P, V] streamable-http.md
9. Removed: `ping`, `logging/setLevel`, `notifications/roots/list_changed`; logging is opt-in per request via `_meta` `io.modelcontextprotocol/logLevel`. Roots, Sampling, Logging are **deprecated** (still functional, 12-month window). Tasks moved to an extension. List results require `ttlMs` and `cacheScope`. [P, V] changelog.
10. Dual-era servers are allowed: modern `_meta` => stateless handling; an `initialize` request => legacy semantics scoped to the session. A modern-only server answers legacy clients with 400; dual-era clients fall back to `initialize` only if the 400 body is NOT a recognized modern error. [P, V] https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning.md
11. Press coverage agreed on the headline changes. [S] https://flaviocopes.com/mcp-2026-07-28-stateless/ . One secondary source's claim that `ping` is removed matched the primary changelog; others' details were not relied upon.
12. Not read: the stdio and cancellation pages, SEP texts. [U] Error-code numbers appear renumbered in the changelog ("-32001 to -32020"); use the numbers in 4 and 6 above.

## Broker design (minimal "modern lane")

Today `http/mod.rs::post_mcp` already has an ephemeral-session path (version header, no session id). The modern lane reuses it.

**http/**
- Classify a POST as modern when `params._meta` has `io.modelcontextprotocol/protocolVersion`. Require header equality, `Mcp-Method` equal to `method`, `Mcp-Name` equal to `params.name|uri` (decode Base64 sentinel); else 400 `-32020`.
- Version not in `{2026-07-28, 2025-11-25}` on a modern request: 400 `-32022` with `supported`. `is_supported_version` must now key on the era, since the header `2026-07-28` currently takes the legacy ephemeral path.
- Modern requests never read or emit `Mcp-Session-Id`; GET and DELETE return 405 on the modern lane (legacy sessions keep both).
- Dropping the response future (client disconnect) cancels via a drop guard calling `session.cancel_all()`, which already emits `notifications/cancelled` toward the backend.

**router/**
- `ephemeral_session` gains `capabilities` and `clientInfo` taken from `_meta` (per request, not stored beyond the request).
- Answer `server/discover` locally from the aggregated catalog: `supportedVersions`, merged capabilities, serverInfo, `ttlMs`/`cacheScope: "private"`.
- On the way out add `resultType:"complete"` to results, and `ttlMs`/`cacheScope` to the five list/read results. On the way in to backends, strip the modern `_meta` keys (keep `progressToken` and traceparent keys) and re-wrap as a legacy call. Backends stay on 2025-11-25 (`initialize` once per instance, unchanged). Do not forward `server/discover` or `ping`.
- `subscriptions/listen`: open the existing session stream as the POST's SSE response, send `acknowledged`, filter `list_changed` by opt-in, add `subscriptionId`, keep-alive via `sse.rs`. The ephemeral session lives as long as the stream.
- Backend-initiated requests (elicitation, sampling, roots) during a modern call: v0.2 first cut answers the backend with a JSON-RPC error (decline). A true mapping needs parking the in-flight call under a broker-minted, integrity-protected `requestState` and resuming it on retry; that is server state and belongs in a separate decision.
- Progress: when `_meta.progressToken` is present, respond as SSE scoped to the request instead of JSON so progress notifications can be delivered.

## Risks

- Era ambiguity: a client sending a modern header to a legacy-only path; mitigated by the classification rule above.
- Deprecated features still work: legacy clients' roots/sampling must keep working; do not remove them.
- MRTR parking adds memory and expiry state; header-vs-body validation must be strict (spec security rationale).
- Spec text may still receive backward-compatible edits ("current").

## Recommendation

Defer to v0.2. Reasons: v0.1 scope is fixed around 2025-11-25 and the M7 server-to-client routing; the modern lane needs discover, header validation, listen, SSE progress and a decision on MRTR; 2025-11-25 clients remain the installed base at release. For v0.1, answer any modern-looking request (a `_meta` version key, or `MCP-Protocol-Version: 2026-07-28`) with a plain 400 whose body is NOT a modern JSON-RPC error, so dual-era clients fall back to `initialize` per fact 10; log it. Ship the modern lane as M9 in v0.2.

## Test plan

1. Modern `tools/list`/`tools/call` with matching headers: 200, `resultType`, no session header.
2. Header/`_meta` mismatch, missing `Mcp-Method`/`Mcp-Name`, Base64 name: 400 `-32020`.
3. Version `1900-01-01`: 400 `-32022`, `supported` lists both versions.
4. `server/discover` equals aggregated catalog.
5. GET/DELETE on modern lane: 405; stray `Mcp-Session-Id` and `Last-Event-ID` ignored.
6. `subscriptions/listen`: ack, filtered `list_changed` after a backend refresh, closes cleanly.
7. Disconnect mid-call: backend receives `notifications/cancelled`; no leaked ephemeral session.
8. Progress token round trip against a 2025-11-25 stdio fixture.
9. Backend elicitation during a modern call: declined, no hang.
10. Legacy regression: full 2025-11-25 suite (sessions, GET stream, DELETE) unchanged; mixed legacy and modern clients on one backend share one instance.
