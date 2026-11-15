// SPDX-License-Identifier: GPL-3.0-or-later

//! The per-session notification stream behind `GET /mcp`.
//!
//! The stream drains the session's bounded queue (see
//! [`crate::router::Session`]): one `event: message` per notification, a
//! `: keep-alive` comment at a fixed interval while idle, and end of stream
//! when the session ends, the broker shuts down, or a newer stream of the
//! same session replaces it. Nothing is buffered here, so a slow reader
//! only ever holds up the session queue, which drops its oldest entries.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Bytes;
use futures_core::Stream;
use tokio::time::{Instant, Interval, MissedTickBehavior};

use crate::router::{Session, StreamPoll};

/// Server-sent-event body of one session.
pub(crate) struct SseStream {
    session: Arc<Session>,
    id: u64,
    greeted: bool,
    keepalive: Interval,
}

impl SseStream {
    pub(crate) fn new(session: Arc<Session>, id: u64, keepalive: Duration) -> Self {
        let mut keepalive = tokio::time::interval_at(Instant::now() + keepalive, keepalive);
        keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            session,
            id,
            greeted: false,
            keepalive,
        }
    }
}

impl Stream for SseStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if !this.greeted {
            // Sent at once so the client sees the stream is open.
            this.greeted = true;
            return Poll::Ready(Some(Ok(Bytes::from_static(b": mcpdoze stream open\n\n"))));
        }
        match this.session.poll_stream(this.id, cx.waker()) {
            StreamPoll::Message(m) => {
                this.keepalive.reset();
                return Poll::Ready(Some(Ok(Bytes::from(format!(
                    "event: message\ndata: {m}\n\n"
                )))));
            }
            StreamPoll::Closed => return Poll::Ready(None),
            StreamPoll::Empty => {}
        }
        if this.keepalive.poll_tick(cx).is_ready() {
            return Poll::Ready(Some(Ok(Bytes::from_static(b": keep-alive\n\n"))));
        }
        Poll::Pending
    }
}

impl Drop for SseStream {
    fn drop(&mut self) {
        self.session.close_stream(self.id);
    }
}
