# Resource subscriptions

Agents can subscribe to resource changes through the broker. Many agents
subscribed to the same resource cost the backend one subscription.

## How agents subscribe

- 2025-11-25 sessions: `resources/subscribe` and `resources/unsubscribe`
  with the mapped URI (`mcpdoze://<prefix>/<encoded-uri>`, as listed by
  `resources/list`). Updates arrive on the session's `GET /mcp` event stream
  as `notifications/resources/updated`, URI mapped. Stateless requests
  without a session are refused (`-32600`): they have no stream.
- 2026-07-28 clients: list the mapped URIs in `resourceSubscriptions` of a
  `subscriptions/listen` request (see
  [decisions/0002](decisions/0002-stateless-protocol-support.md)). The
  subscription lasts as long as the listen stream.

The broker advertises `resources.subscribe = true` only while at least one
registered backend advertises it. Subscribing to a resource of a backend
that does not support subscriptions fails with `-32601`.

## Sharing

The broker keeps a registry keyed by `(server, instance scope key, backend
URI)`:

- the first subscriber sends `resources/subscribe` to the backend instance
  that serves that agent (so `per-cwd` and `per-session` instances stay
  isolated), and pins the instance;
- further subscribers are only recorded;
- the last subscriber leaving sends `resources/unsubscribe` and unpins.

A subscriber leaves on `resources/unsubscribe`, when its session ends
(`DELETE`, idle timeout, eviction, broker shutdown) or when its listen stream
closes. Updates from an instance go only to the sessions subscribed to that
URI on that instance.

## Idle unload and crashes

Set per server with `subscriptions` (see
[CONFIGURATION.md](CONFIGURATION.md)):

```toml
[servers.example]
subscriptions = "keep-alive"   # default; or "allow-unload"
```

- `keep-alive`: a pinned instance is not unloaded at its idle TTL (the check
  runs in the same critical section as the unload decision, so a subscribe
  racing the timer either keeps the process or starts a new one). After a
  crash it is restarted at once, with the usual exponential backoff.
- `allow-unload`: the instance unloads as usual; the next call starts it.

Whenever a new process of a subscribed instance comes up (respawn after a
crash, start after an unload), the broker replays every subscription on it
and then sends each subscriber one `notifications/resources/updated` per
URI, because changes may have been missed in between. Explicit
`mcpdoze stop` and reloads stop pinned instances too.

## HTTP backends

A 2025-11-25 Streamable HTTP server sends unsolicited notifications only on
its `GET` event stream. The broker opens that stream while the instance has
subscriptions, and closes it after the last unsubscribe. It reconnects with
a short backoff; an upstream `405` disables it for that connection.

## Limits

- The registry lives in memory; a broker restart drops all subscriptions
  (agents resubscribe on their new session).
- `resources/updated` from a backend for a URI nobody subscribed to through
  the broker is dropped.
