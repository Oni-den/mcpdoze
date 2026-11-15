// SPDX-License-Identifier: GPL-3.0-or-later

//! The upstream `GET` event stream of an HTTP backend.
//!
//! A 2025-11-25 Streamable HTTP server delivers server-initiated messages
//! that belong to no request (such as `notifications/resources/updated`)
//! only on the session's `GET` stream. The broker opens that stream while
//! the instance is pinned by resource subscriptions (see
//! `docs/SUBSCRIPTIONS.md`) and closes it when the last subscription goes.
//! Messages are routed exactly like those on a POST response stream.
//!
//! The stream reconnects with a short backoff after transport errors or an
//! upstream end; an upstream `405` (no `GET` support) stops it for the life
//! of the connection.

use std::sync::{Arc, Weak};
use std::time::Duration;

use reqwest::header::ACCEPT;
use serde_json::Value;

use super::{lock, HttpConn, SseParser, MAX_RESPONSE};

/// Delay before reopening after the first failure, doubled up to [`MAX_DELAY`].
const BASE_DELAY: Duration = Duration::from_millis(250);
const MAX_DELAY: Duration = Duration::from_secs(5);

enum End {
    /// The upstream does not offer a `GET` stream.
    Unsupported,
    /// Closed or failed; try again.
    Retry,
}

impl HttpConn {
    /// Opens (`on`) or closes the upstream event stream. Idempotent.
    pub(crate) fn set_listening(self: &Arc<Self>, on: bool) {
        let mut l = lock(&self.listen);
        if !on {
            if let Some(h) = l.take() {
                h.abort();
            }
            return;
        }
        if l.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        *l = Some(rt.spawn(listen_loop(Arc::downgrade(self))).abort_handle());
    }

    /// True while the upstream event stream task runs.
    pub fn is_listening(&self) -> bool {
        lock(&self.listen)
            .as_ref()
            .is_some_and(|h| !h.is_finished())
    }

    async fn listen_once(self: &Arc<Self>) -> End {
        let (sid, _) = self.snapshot();
        let Some(sid) = sid else {
            // Stateless upstream: nothing to listen on yet.
            return End::Retry;
        };
        let proto = lock(&self.session).protocol.clone();
        let mut req = super::client()
            .get(&self.url)
            .header(ACCEPT, "text/event-stream")
            .header("Mcp-Session-Id", sid);
        for (k, v) in self.current_headers() {
            req = req.header(k, v);
        }
        if let Some(p) = proto {
            req = req.header("MCP-Protocol-Version", p);
        }
        let mut resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(server = %self.server, error = %e.without_url(), "upstream event stream failed");
                return End::Retry;
            }
        };
        match resp.status().as_u16() {
            405 => {
                tracing::info!(server = %self.server, "upstream has no event stream; resource updates may not arrive");
                return End::Unsupported;
            }
            s if !(200..300).contains(&s) => return End::Retry,
            _ => {}
        }
        tracing::debug!(server = %self.server, "upstream event stream open");
        let mut parser = SseParser::default();
        let mut since_event = 0usize;
        while let Ok(Some(chunk)) = resp.chunk().await {
            since_event += chunk.len();
            if since_event > MAX_RESPONSE {
                tracing::warn!(server = %self.server, "upstream event over 16 MiB; stream reopened");
                return End::Retry;
            }
            for data in parser.feed(&chunk) {
                since_event = 0;
                if let Ok(msg) = serde_json::from_str::<Value>(&data) {
                    self.dispatch(msg, None);
                }
            }
        }
        End::Retry
    }
}

async fn listen_loop(conn: Weak<HttpConn>) {
    let mut delay = BASE_DELAY;
    loop {
        let Some(c) = conn.upgrade() else { return };
        if *c.closed.borrow() {
            return;
        }
        let started = tokio::time::Instant::now();
        let end = c.listen_once().await;
        drop(c);
        match end {
            End::Unsupported => return,
            End::Retry => {
                if started.elapsed() > MAX_DELAY {
                    delay = BASE_DELAY;
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_DELAY);
            }
        }
    }
}
