// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal JSON-RPC 2.0 helpers over `serde_json::Value`.
//!
//! The broker is a proxy, so messages stay untyped: unknown fields and
//! methods pass through unchanged (see `docs/decisions/0001`).

use serde_json::{json, Value};

/// Invalid JSON was received.
pub const PARSE_ERROR: i64 = -32700;
/// The JSON sent is not a valid request object.
pub const INVALID_REQUEST: i64 = -32600;
/// The method does not exist or is not available.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// Invalid method parameters.
pub const INVALID_PARAMS: i64 = -32602;
/// Internal JSON-RPC error.
pub const INTERNAL_ERROR: i64 = -32603;
/// The backend is unavailable (crashed, backing off, failed to start).
pub const BACKEND_UNAVAILABLE: i64 = -32000;
/// A scoped server reached its `max_instances` cap.
pub const INSTANCE_LIMIT: i64 = -32001;
/// The request was cancelled (MCP convention).
pub const REQUEST_CANCELLED: i64 = -32800;

/// The kind of a JSON-RPC message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Has `method` and `id`.
    Request,
    /// Has `method`, no `id`.
    Notification,
    /// Has `id` and `result` or `error`, no `method`.
    Response,
    /// Anything else.
    Invalid,
}

/// Classifies a message.
pub fn kind(msg: &Value) -> Kind {
    let Some(obj) = msg.as_object() else {
        return Kind::Invalid;
    };
    let has_id = obj.get("id").is_some_and(|i| !i.is_null());
    match (obj.get("method").and_then(Value::as_str), has_id) {
        (Some(_), true) => Kind::Request,
        (Some(_), false) => Kind::Notification,
        (None, _) if obj.contains_key("result") || obj.contains_key("error") => Kind::Response,
        _ => Kind::Invalid,
    }
}

/// Builds a request.
pub fn request(id: impl Into<Value>, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id.into(), "method": method, "params": params})
}

/// Builds a notification.
pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Builds a success response.
pub fn result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Builds an error response.
pub fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

/// Stable map key for a request id (`1` and `"1"` differ).
pub fn id_key(id: &Value) -> String {
    id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages() {
        assert_eq!(kind(&request(1, "a", json!({}))), Kind::Request);
        assert_eq!(kind(&notification("a", json!({}))), Kind::Notification);
        assert_eq!(kind(&result(json!(1), json!({}))), Kind::Response);
        assert_eq!(kind(&error(json!(1), 1, "x")), Kind::Response);
        assert_eq!(kind(&json!([1])), Kind::Invalid);
        assert_eq!(kind(&json!({"id": 1})), Kind::Invalid);
        assert_eq!(
            kind(&json!({"id": null, "method": "x"})),
            Kind::Notification
        );
    }

    #[test]
    fn id_keys_distinguish_types() {
        assert_ne!(id_key(&json!(1)), id_key(&json!("1")));
    }
}
