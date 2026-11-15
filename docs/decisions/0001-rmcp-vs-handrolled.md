# 0001: Hand-rolled JSON-RPC data path instead of `rmcp`

Status: accepted (M1 spike, applies to M2/M3)

## Context

MCPDoze is a proxy. Between agents and backends it must:

- forward requests it does not understand (custom or newer methods),
- rewrite request ids (one backend shared by many agent sessions),
- rewrite `_meta.progressToken` and route progress notifications by token,
- translate `notifications/cancelled` between id spaces,
- keep backend responses byte-for-byte meaningful (unknown fields preserved).

The plan named `rmcp` (official Rust MCP SDK, 3.10.0 at the time of the
spike) as the transport and type layer, with a spike gate: if it cannot pass
raw messages cleanly, use it for types only or not at all.

## Findings

- `rmcp`'s server-side Streamable HTTP (`StreamableHttpService`, feature
  `server-side-http`) is built around a typed `Service<RoleServer>` handler.
  Incoming messages are decoded into `ClientRequest` / `ClientNotification`
  enums; unknown methods fall into a `CustomRequest` variant. Request ids and
  peer bookkeeping are owned by the SDK's service loop, so a proxy cannot
  choose or remap ids without fighting that loop.
- The client side (`child_process` transport plus `Peer`) likewise owns the id
  space towards the backend and answers server-to-client requests through a
  typed `ClientHandler`. Routing a backend request to "the session that owns
  the in-flight call" needs the raw id and the call map, which the typed peer
  hides.
- Round-tripping through typed models risks dropping or normalising fields the
  broker does not know, which is wrong for a passthrough.
- The features needed (`server`, `server-side-http`, `client`,
  `transport-child-process`) pull in `schemars`, `rand`, `sse-stream`,
  `process-wrap`, `hyper-util` and macro crates. Builds run on RAM-limited
  machines; the proxy needs none of the typed tool/schema machinery.

## Decision

Do not depend on `rmcp` for the data path.

- Messages are `serde_json::Value`; small helpers build JSON-RPC requests,
  notifications and errors.
- Backend stdio framing is hand-rolled: newline-delimited JSON, one writer task,
  one reader task, a 16 MiB line cap, non-JSON lines dropped.
- The agent-facing Streamable HTTP endpoint is hand-written on `axum`
  (`POST /mcp` JSON response mode, `DELETE /mcp`, `Mcp-Session-Id`).

## Consequences

- Full control over id remapping, progress tokens, cancellation and unknown
  methods; few dependencies (`axum`, `serde_json`, `tokio`, `tracing`).
- Protocol conformance is our responsibility: per-version conformance tests
  against the testkit mock servers are required, and new protocol revisions
  must be tracked by hand.
- SSE response streaming and the standalone `GET /mcp` stream are not yet
  implemented; they can be added on `axum::response::sse` without changing the
  router.
- `rmcp` may still be used later for typed models in tests or admin tooling if
  that pays for itself; the router does not depend on it.
