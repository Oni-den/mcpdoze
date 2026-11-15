// SPDX-License-Identifier: GPL-3.0-or-later

//! The 2026-07-28 (stateless) lane of `POST /mcp`; see
//! `docs/decisions/0002-stateless-protocol-support.md`.
//!
//! A request is modern when `params._meta` carries
//! `io.modelcontextprotocol/protocolVersion`. Then, in this order:
//!
//! 1. `MCP-Protocol-Version` must be present and equal the `_meta` value,
//!    `Mcp-Method` must equal `method`, and for `tools/call`,
//!    `resources/read` and `prompts/get` `Mcp-Name` must equal
//!    `params.name` / `params.uri` (after decoding the `=?base64?...?=`
//!    sentinel): else `400` with `-32020` (HeaderMismatch).
//! 2. The version must be 2026-07-28: else `400` with `-32022` and
//!    `data.supported` / `data.requested`.
//! 3. The method must be served on this lane: else `404` with `-32601`.
//!
//! `Mcp-Session-Id` and `Last-Event-ID` are ignored and never sent. A
//! request carrying `_meta.progressToken` or a `logLevel` gets an SSE
//! response scoped to it (progress and log messages, then the result);
//! otherwise plain JSON. Dropping the response (client disconnect) cancels
//! the backend call. `subscriptions/listen` answers with a long-lived SSE
//! stream: `notifications/subscriptions/acknowledged` first, every message
//! tagged with the listen request's id, and a final `resultType: complete`
//! response when the broker ends it.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_core::Stream;
use serde_json::{json, Value};
use tokio::time::{Instant, Interval, MissedTickBehavior};

use super::{declared_cwd, json_response, VERSION_HEADER};
use crate::backend::jsonrpc::{self, Kind};
use crate::router::modern::{
    supported_versions, LiveGuard, META_LOG_LEVEL, META_PROTOCOL_VERSION, META_SUBSCRIPTION_ID,
    MODERN_METHODS, MODERN_PROTOCOL_VERSION, NAMED_METHODS,
};
use crate::router::{Router, Session, StreamPoll};

/// `Mcp-Method` header.
pub const METHOD_HEADER: &str = "mcp-method";
/// `Mcp-Name` header.
pub const NAME_HEADER: &str = "mcp-name";
/// HeaderMismatch.
pub const HEADER_MISMATCH: i64 = -32020;
/// UnsupportedProtocolVersion.
pub const UNSUPPORTED_VERSION: i64 = -32022;

/// True if `msg` is a 2026-07-28 message (modern `_meta` version key).
pub(super) fn is_modern(msg: &Value) -> bool {
    msg["params"]["_meta"].get(META_PROTOCOL_VERSION).is_some()
}

/// Decodes an `Mcp-Name` value: the `=?base64?<b64>?=` sentinel form, else
/// the value itself. `None` for a malformed sentinel.
pub fn decode_header_value(v: &str) -> Option<String> {
    match v
        .strip_prefix("=?base64?")
        .and_then(|r| r.strip_suffix("?="))
    {
        Some(b64) => base64_decode(b64).and_then(|b| String::from_utf8(b).ok()),
        None => Some(v.to_owned()),
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let s = s.trim_end_matches('=').as_bytes();
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

fn single<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ()> {
    let mut it = headers.get_all(name).iter();
    match (it.next(), it.next()) {
        (None, _) => Ok(None),
        (Some(v), None) => v.to_str().map(Some).map_err(|_| ()),
        _ => Err(()),
    }
}

fn error(
    status: StatusCode,
    id: &Value,
    code: i64,
    message: String,
    data: Option<Value>,
) -> Response {
    let mut body = jsonrpc::error(id.clone(), code, message);
    if let Some(d) = data {
        body["error"]["data"] = d;
    }
    json_response(status, &body)
}

fn mismatch(id: &Value, message: impl Into<String>) -> Response {
    error(
        StatusCode::BAD_REQUEST,
        id,
        HEADER_MISMATCH,
        format!("Header mismatch: {}", message.into()),
        None,
    )
}

/// Validates the metadata headers of a modern request against its body.
/// The error is the finished response (once per rejected request; its size
/// does not matter on this cold path).
#[allow(clippy::result_large_err)]
fn validate(headers: &HeaderMap, msg: &Value) -> Result<(), Response> {
    let id = &msg["id"];
    let method = msg["method"].as_str().unwrap_or_default();
    let Some(body_version) = msg["params"]["_meta"][META_PROTOCOL_VERSION].as_str() else {
        return Err(mismatch(id, "_meta protocolVersion must be a string"));
    };
    match single(headers, VERSION_HEADER) {
        Ok(Some(v)) if v == body_version => {}
        Ok(Some(_)) => {
            return Err(mismatch(
                id,
                "MCP-Protocol-Version header does not match _meta protocolVersion",
            ))
        }
        Ok(None) => return Err(mismatch(id, "MCP-Protocol-Version header is missing")),
        Err(()) => return Err(mismatch(id, "MCP-Protocol-Version header is malformed")),
    }
    match single(headers, METHOD_HEADER) {
        Ok(Some(m)) if m == method => {}
        Ok(Some(_)) => return Err(mismatch(id, "Mcp-Method header does not match method")),
        Ok(None) => return Err(mismatch(id, "Mcp-Method header is missing")),
        Err(()) => return Err(mismatch(id, "Mcp-Method header is malformed")),
    }
    if NAMED_METHODS.contains(&method) {
        let field = if method == "resources/read" {
            "uri"
        } else {
            "name"
        };
        let body_name = msg["params"][field].as_str();
        match single(headers, NAME_HEADER) {
            Ok(Some(raw)) => match decode_header_value(raw) {
                Some(n) if Some(n.as_str()) == body_name => {}
                Some(_) => {
                    return Err(mismatch(
                        id,
                        format!("Mcp-Name header does not match params.{field}"),
                    ))
                }
                None => return Err(mismatch(id, "Mcp-Name header has a malformed base64 value")),
            },
            Ok(None) => return Err(mismatch(id, "Mcp-Name header is missing")),
            Err(()) => return Err(mismatch(id, "Mcp-Name header is malformed")),
        }
    }
    if body_version != MODERN_PROTOCOL_VERSION {
        return Err(error(
            StatusCode::BAD_REQUEST,
            id,
            UNSUPPORTED_VERSION,
            format!("unsupported protocol version {body_version:?}"),
            Some(json!({"supported": supported_versions(), "requested": body_version})),
        ));
    }
    if !MODERN_METHODS.contains(&method) {
        return Err(error(
            StatusCode::NOT_FOUND,
            id,
            jsonrpc::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
            None,
        ));
    }
    Ok(())
}

/// Cancels the session's in-flight calls unless disarmed (the response
/// future was dropped: the client went away).
struct CancelAll {
    session: Arc<Session>,
    armed: bool,
}

impl Drop for CancelAll {
    fn drop(&mut self) {
        if self.armed {
            self.session.cancel_all();
        }
    }
}

/// `POST /mcp` for a modern message.
pub(super) async fn post(
    router: Arc<Router>,
    uri: &Uri,
    headers: &HeaderMap,
    msg: Value,
) -> Response {
    match jsonrpc::kind(&msg) {
        Kind::Request => {}
        Kind::Notification => return StatusCode::ACCEPTED.into_response(),
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                &Value::Null,
                jsonrpc::INVALID_REQUEST,
                "clients must not send JSON-RPC responses on MCP 2026-07-28".into(),
                None,
            )
        }
    }
    if let Err(resp) = validate(headers, &msg) {
        tracing::debug!(
            method = msg["method"].as_str().unwrap_or_default(),
            "modern request rejected"
        );
        return resp;
    }
    let meta = msg["params"]["_meta"].clone();
    let (session, live) = router.modern_session(&meta);
    if let Some((cwd, query)) = declared_cwd(headers, uri) {
        router.note_client_cwd(&session, &cwd, query);
    }
    if msg["method"] == "subscriptions/listen" {
        return listen(router, session, live, msg).await;
    }
    let streamed = !meta["progressToken"].is_null() || meta.get(META_LOG_LEVEL).is_some();
    if streamed {
        if router.is_closing() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let Some(stream_id) = session.open_stream() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let fut = {
            let (router, session) = (router.clone(), session.clone());
            Box::pin(async move { router.handle_modern(&session, msg).await })
        };
        let body = RequestStream {
            keepalive: keepalive(router.sse_keepalive()),
            session,
            stream_id,
            fut: Some(fut),
            done: false,
            _live: live,
        };
        return sse_response(axum::body::Body::from_stream(body));
    }
    let mut guard = CancelAll {
        session: session.clone(),
        armed: true,
    };
    let resp = router.handle_modern(&session, msg).await;
    guard.armed = false;
    drop(live);
    json_response(StatusCode::OK, &resp)
}

fn sse_response(body: axum::body::Body) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        body,
    )
        .into_response()
}

fn keepalive(every: Duration) -> Interval {
    let mut k = tokio::time::interval_at(Instant::now() + every, every);
    k.set_missed_tick_behavior(MissedTickBehavior::Delay);
    k
}

fn event(m: &Value) -> Bytes {
    Bytes::from(format!("event: message\ndata: {m}\n\n"))
}

type Answer = Pin<Box<dyn Future<Output = Value> + Send>>;

/// SSE response of one modern request: its progress and log messages, then
/// the result. Dropped early (client gone): the call is cancelled.
struct RequestStream {
    session: Arc<Session>,
    stream_id: u64,
    fut: Option<Answer>,
    done: bool,
    keepalive: Interval,
    _live: LiveGuard,
}

impl Stream for RequestStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(None);
        }
        match this.session.poll_stream(this.stream_id, cx.waker()) {
            StreamPoll::Message(m) => {
                this.keepalive.reset();
                return Poll::Ready(Some(Ok(event(&m))));
            }
            // Broker shutdown: the call future is dropped with the stream.
            StreamPoll::Closed => {
                this.done = true;
                this.session.cancel_all();
                return Poll::Ready(None);
            }
            StreamPoll::Empty => {}
        }
        let Some(fut) = this.fut.as_mut() else {
            return Poll::Ready(None);
        };
        match fut.as_mut().poll(cx) {
            Poll::Ready(resp) => {
                this.fut = None;
                this.done = true;
                // Notifications queued before the result go first.
                let mut out = String::new();
                for m in this.session.take_notifications() {
                    out.push_str(&format!("event: message\ndata: {m}\n\n"));
                }
                out.push_str(&format!("event: message\ndata: {resp}\n\n"));
                Poll::Ready(Some(Ok(Bytes::from(out))))
            }
            Poll::Pending => {
                if this.keepalive.poll_tick(cx).is_ready() {
                    return Poll::Ready(Some(Ok(Bytes::from_static(b": keep-alive\n\n"))));
                }
                Poll::Pending
            }
        }
    }
}

impl Drop for RequestStream {
    fn drop(&mut self) {
        if !self.done {
            // Client disconnected mid-call: cancellation per the spec.
            self.session.cancel_all();
        }
        self.session.close_stream(self.stream_id);
        self.session.end();
    }
}

async fn listen(
    router: Arc<Router>,
    session: Arc<Session>,
    live: LiveGuard,
    msg: Value,
) -> Response {
    if router.is_closing() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let id = msg["id"].clone();
    let Some(stream_id) = session.open_stream() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let honored = router.listen(&session, &msg["params"]).await;
    let ack = jsonrpc::notification(
        "notifications/subscriptions/acknowledged",
        json!({"_meta": {META_SUBSCRIPTION_ID: id.clone()}, "notifications": honored}),
    );
    tracing::debug!("subscriptions/listen stream open");
    let body = ListenStream {
        keepalive: keepalive(router.sse_keepalive()),
        session,
        stream_id,
        sub_id: id,
        ack: Some(ack),
        ended: false,
        _live: live,
    };
    sse_response(axum::body::Body::from_stream(body))
}

/// The long-lived SSE response of `subscriptions/listen`.
struct ListenStream {
    session: Arc<Session>,
    stream_id: u64,
    sub_id: Value,
    ack: Option<Value>,
    ended: bool,
    keepalive: Interval,
    _live: LiveGuard,
}

impl Stream for ListenStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if let Some(ack) = this.ack.take() {
            return Poll::Ready(Some(Ok(event(&ack))));
        }
        if this.ended {
            return Poll::Ready(None);
        }
        match this.session.poll_stream(this.stream_id, cx.waker()) {
            StreamPoll::Message(mut m) => {
                this.keepalive.reset();
                if let Some(p) = m
                    .as_object_mut()
                    .map(|o| o.entry("params").or_insert_with(|| json!({})))
                {
                    if !p.is_object() {
                        *p = json!({});
                    }
                    let meta = p
                        .as_object_mut()
                        .map(|o| o.entry("_meta").or_insert_with(|| json!({})));
                    if let Some(Value::Object(meta)) = meta {
                        meta.insert(META_SUBSCRIPTION_ID.into(), this.sub_id.clone());
                    }
                }
                Poll::Ready(Some(Ok(event(&m))))
            }
            StreamPoll::Closed => {
                // The broker ended the subscription: graceful closure.
                this.ended = true;
                let done = jsonrpc::result(
                    this.sub_id.clone(),
                    json!({"resultType": "complete", "_meta": {META_SUBSCRIPTION_ID: this.sub_id.clone()}}),
                );
                Poll::Ready(Some(Ok(event(&done))))
            }
            StreamPoll::Empty => {
                if this.keepalive.poll_tick(cx).is_ready() {
                    return Poll::Ready(Some(Ok(Bytes::from_static(b": keep-alive\n\n"))));
                }
                Poll::Pending
            }
        }
    }
}

impl Drop for ListenStream {
    fn drop(&mut self) {
        // Ends the ephemeral session: its subscriptions are released.
        self.session.close_stream(self.stream_id);
        self.session.end();
        tracing::debug!("subscriptions/listen stream closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64_sentinel() {
        assert_eq!(decode_header_value("plain").as_deref(), Some("plain"));
        assert_eq!(
            decode_header_value("=?base64?SGVsbG8sIOS4lueVjA==?=").as_deref(),
            Some("Hello, 世界")
        );
        assert_eq!(
            decode_header_value("=?base64?IHBhZGRlZCA=?=").as_deref(),
            Some(" padded ")
        );
        assert_eq!(decode_header_value("=?base64?***?="), None);
        assert_eq!(base64_decode("TWE").unwrap(), b"Ma");
    }
}
