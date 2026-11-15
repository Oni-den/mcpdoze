// SPDX-License-Identifier: GPL-3.0-or-later

//! Agent-facing Streamable HTTP endpoint (JSON response mode).
//!
//! * `POST /mcp`: one JSON-RPC message per body. Requests get
//!   `application/json`, notifications and responses get `202 Accepted`.
//!   `initialize` creates a session and returns `Mcp-Session-Id`.
//!   Requests without a session id are served on an ephemeral session after
//!   checking `MCP-Protocol-Version` (stateless clients).
//! * `GET /mcp`: `405` until the SSE stream exists.
//! * `DELETE /mcp`: ends the session and cancels its in-flight calls.
//! * `GET /healthz`: liveness.
//!
//! If the agent disconnects mid-request the handler future is dropped, which
//! cancels the backend call (see the router).

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::Value;

use crate::backend::jsonrpc::{self, Kind};
use crate::router::{is_supported_version, Router};

/// Session header.
pub const SESSION_HEADER: &str = "mcp-session-id";
/// Protocol version header.
pub const VERSION_HEADER: &str = "mcp-protocol-version";
/// Largest accepted request body.
pub const MAX_BODY: usize = 16 * 1024 * 1024;

/// Builds the axum application for `router`.
pub fn app(router: Arc<Router>) -> axum::Router {
    let app = axum::Router::new()
        .route("/mcp", post(post_mcp).get(get_mcp).delete(delete_mcp))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(router);
    security_layers(app)
}

/// ===== M5 INSERTION POINT =====
///
/// Bearer-token auth and `Host` / `Origin` validation (DNS rebinding) are
/// added here as tower layers wrapping every route, e.g.
/// `app.layer(axum::middleware::from_fn_with_state(auth, check))`.
/// Until then the endpoint must only be bound to loopback.
fn security_layers(app: axum::Router) -> axum::Router {
    app
}

/// Binds a listener (callers pass a loopback address such as
/// `127.0.0.1:7878`).
pub async fn bind(addr: SocketAddr) -> io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr).await
}

/// Serves `router` on `listener` until `shutdown` resolves, then stops
/// every backend.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Arc<Router>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let res = axum::serve(listener, app(router.clone()))
        .with_graceful_shutdown(shutdown)
        .await;
    router.shutdown().await;
    res
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn rpc_error(status: StatusCode, code: i64, message: &str) -> Response {
    json_response(status, &jsonrpc::error(Value::Null, code, message))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

async fn post_mcp(State(router): State<Arc<Router>>, headers: HeaderMap, body: Bytes) -> Response {
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return rpc_error(StatusCode::BAD_REQUEST, jsonrpc::PARSE_ERROR, "parse error"),
    };
    if msg.is_array() {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            jsonrpc::INVALID_REQUEST,
            "JSON-RPC batches are not supported",
        );
    }
    let kind = jsonrpc::kind(&msg);
    if kind == Kind::Invalid {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            jsonrpc::INVALID_REQUEST,
            "invalid JSON-RPC message",
        );
    }
    let version = header_str(&headers, VERSION_HEADER);
    if version.is_some_and(|v| !is_supported_version(v)) {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            jsonrpc::INVALID_REQUEST,
            "unsupported MCP-Protocol-Version",
        );
    }
    let session_id = header_str(&headers, SESSION_HEADER);

    if kind == Kind::Request && msg["method"] == "initialize" {
        if session_id.is_some() {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                jsonrpc::INVALID_REQUEST,
                "initialize must not carry a session id",
            );
        }
        let (negotiated, result) =
            router.initialize_result(msg["params"]["protocolVersion"].as_str());
        let session = router.create_session(&negotiated);
        let mut resp = json_response(StatusCode::OK, &jsonrpc::result(msg["id"].clone(), result));
        if let Ok(v) = HeaderValue::from_str(session.id()) {
            resp.headers_mut().insert(SESSION_HEADER, v);
        }
        return resp;
    }

    let session = match session_id {
        Some(id) => match router.session(id) {
            Some(s) => s,
            None => {
                return rpc_error(
                    StatusCode::NOT_FOUND,
                    jsonrpc::INVALID_REQUEST,
                    "unknown session",
                )
            }
        },
        None => match version {
            // Stateless client: one ephemeral session for this request.
            Some(v) => router.ephemeral_session(v),
            None => {
                return rpc_error(
                    StatusCode::BAD_REQUEST,
                    jsonrpc::INVALID_REQUEST,
                    "missing Mcp-Session-Id (or MCP-Protocol-Version for stateless use)",
                )
            }
        },
    };
    match router.handle(&session, msg).await {
        Some(resp) => json_response(StatusCode::OK, &resp),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn get_mcp() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST, DELETE")],
    )
        .into_response()
}

async fn delete_mcp(State(router): State<Arc<Router>>, headers: HeaderMap) -> Response {
    match header_str(&headers, SESSION_HEADER) {
        Some(id) if router.end_session(id) => StatusCode::OK.into_response(),
        Some(_) => StatusCode::NOT_FOUND.into_response(),
        None => StatusCode::BAD_REQUEST.into_response(),
    }
}
