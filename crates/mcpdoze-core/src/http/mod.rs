// SPDX-License-Identifier: GPL-3.0-or-later

//! Agent-facing Streamable HTTP endpoint (JSON response mode) plus the
//! admin API.
//!
//! * `POST /mcp`: one JSON-RPC message per body. Requests get
//!   `application/json`, notifications and responses get `202 Accepted`.
//!   `initialize` creates a session and returns `Mcp-Session-Id`.
//!   Requests without a session id are served on an ephemeral session after
//!   checking `MCP-Protocol-Version` (stateless clients).
//! * `GET /mcp`: `405` until the SSE stream exists.
//! * `DELETE /mcp`: ends the session and cancels its in-flight calls.
//! * `/admin/v1/...`: see [`crate::admin`].
//! * `GET /healthz`: liveness (`ok`), no token required.
//!
//! # Security layers
//!
//! One middleware runs on every request, in this order:
//!
//! 1. `Host` must be exactly one header naming `localhost`, `127.0.0.1`,
//!    `[::1]` or a configured extra host (DNS-rebinding defence): else `403`.
//! 2. `Origin`, when present, must name one of the same hosts: else `403`.
//! 3. `/healthz` is answered without further checks.
//! 4. `/admin/*` is refused (`403`) unless the TCP peer is loopback,
//!    whatever the bind address.
//! 5. Everything else needs `Authorization: Bearer <token>` (constant-time
//!    compare): else `401` with `WWW-Authenticate: Bearer`.
//!
//! Header values, and in particular the token, are never logged.
//!
//! If the agent disconnects mid-request the handler future is dropped, which
//! cancels the backend call (see the router).

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::Value;

use crate::admin::{self, Control};
use crate::auth::{self, Token};
use crate::backend::jsonrpc::{self, Kind};
use crate::config::Config;
use crate::router::{is_supported_version, Router};

/// Session header.
pub const SESSION_HEADER: &str = "mcp-session-id";
/// Protocol version header.
pub const VERSION_HEADER: &str = "mcp-protocol-version";
/// Largest accepted request body.
pub const MAX_BODY: usize = 16 * 1024 * 1024;

/// Request admission settings: the bearer token and extra allowed hosts.
#[derive(Debug, Clone)]
pub struct Security {
    token: Token,
    allowed_hosts: Vec<String>,
}

impl Security {
    /// `allowed_hosts` are host names accepted in `Host` / `Origin` besides
    /// the loopback names (no port).
    pub fn new(token: Token, allowed_hosts: Vec<String>) -> Self {
        Self {
            token,
            allowed_hosts,
        }
    }
}

/// Builds the axum application: `/mcp`, the admin API and `/healthz`
/// behind the security middleware.
pub fn app(control: Arc<Control>, security: Arc<Security>) -> axum::Router {
    let mcp = axum::Router::new()
        .route("/mcp", post(post_mcp).get(get_mcp).delete(delete_mcp))
        .with_state(control.router().clone());
    axum::Router::new()
        .merge(mcp)
        .nest(admin::api::BASE, admin::routes(control))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(axum::middleware::from_fn_with_state(security, guard))
}

fn reject(status: StatusCode, msg: &str) -> Response {
    let body = admin::api::ErrorResponse {
        error: msg.to_owned(),
        details: Vec::new(),
    };
    let mut resp = (status, axum::Json(body)).into_response();
    if status == StatusCode::UNAUTHORIZED {
        resp.headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    resp
}

/// The single header `name`, if present exactly once and valid text.
/// `Err` when repeated or not visible ASCII.
fn single(headers: &HeaderMap, name: header::HeaderName) -> Result<Option<&str>, ()> {
    let mut it = headers.get_all(name).iter();
    match (it.next(), it.next()) {
        (None, _) => Ok(None),
        (Some(v), None) => v.to_str().map(Some).map_err(|_| ()),
        _ => Err(()),
    }
}

async fn guard(State(sec): State<Arc<Security>>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let allowed: Vec<&str> = sec.allowed_hosts.iter().map(String::as_str).collect();
    let host_ok = match single(headers, header::HOST) {
        Ok(h) => auth::check_host(h, &allowed),
        Err(()) => false,
    };
    if !host_ok {
        tracing::debug!("request refused: Host not allowed");
        return reject(StatusCode::FORBIDDEN, "host not allowed");
    }
    let origin_ok = match single(headers, header::ORIGIN) {
        Ok(o) => auth::check_origin(o, &allowed),
        Err(()) => false,
    };
    if !origin_ok {
        tracing::debug!("request refused: Origin not allowed");
        return reject(StatusCode::FORBIDDEN, "origin not allowed");
    }
    let path = req.uri().path();
    if path == "/healthz" {
        return next.run(req).await;
    }
    if path == "/admin" || path.starts_with("/admin/") {
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        if !peer.is_some_and(|p| admin::peer_allowed(&p)) {
            tracing::debug!("admin request refused: peer is not loopback");
            return reject(StatusCode::FORBIDDEN, "admin API is loopback-only");
        }
    }
    let authorized = match single(headers, header::AUTHORIZATION) {
        Ok(Some(v)) => auth::verify_bearer(v, &sec.token),
        _ => false,
    };
    if !authorized {
        tracing::debug!("request refused: missing or invalid bearer token");
        return reject(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
    }
    next.run(req).await
}

/// Binds a listener (callers pass a loopback address such as
/// `127.0.0.1:7878`).
pub async fn bind(addr: SocketAddr) -> io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr).await
}

/// Serves the broker on `listener` until `shutdown` resolves, then stops
/// every backend.
pub async fn serve(
    listener: tokio::net::TcpListener,
    control: Arc<Control>,
    security: Security,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let app = app(control.clone(), Arc::new(security));
    let res = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await;
    control.shutdown().await;
    res
}

/// [`serve`] for a router whose servers are registered directly (no config;
/// the admin API manages them in memory).
pub async fn serve_router(
    listener: tokio::net::TcpListener,
    router: Arc<Router>,
    security: Security,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let control = Control::new(router, Config::default(), None);
    serve(listener, control, security, shutdown).await
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
