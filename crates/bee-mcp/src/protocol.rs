//! JSON-RPC 2.0 framing and the MCP constants this server speaks.
//!
//! MCP messages are JSON-RPC 2.0 requests (with an `id`), notifications (without one) and
//! responses. This module classifies incoming values and builds outgoing ones; it holds no
//! state.

use serde_json::{Value, json};

/// Protocol revisions this server implements, newest first. All are the handshake-based
/// ("legacy era") revisions that begin with `initialize`; a request in the stateless
/// 2026-07-28 shape gets "method not found" for `server/discover`, which dual-era clients
/// read as "fall back to `initialize`".
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The revision offered when the client asks for one this server does not implement.
pub const LATEST_PROTOCOL_VERSION: &str = SUPPORTED_PROTOCOL_VERSIONS[0];

/// The revision Streamable HTTP assumes when a request carries no `MCP-Protocol-Version`
/// header and nothing else identifies the version.
pub const DEFAULT_HTTP_PROTOCOL_VERSION: &str = "2025-03-26";

/// JSON-RPC: the text is not valid JSON.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC: the value is not a valid request object.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC: the method does not exist here.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC: invalid method parameters (including an unknown tool name).
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC: internal error.
pub const INTERNAL_ERROR: i64 = -32603;

/// True if `version` is one of [`SUPPORTED_PROTOCOL_VERSIONS`].
pub fn is_supported_version(version: &str) -> bool {
    SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
}

/// True if the revision has tool `title`, `outputSchema` and `structuredContent`
/// (introduced in 2025-06-18).
pub fn has_structured_output(version: &str) -> bool {
    version >= "2025-06-18"
}

/// One incoming JSON-RPC message, classified.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    /// A request: it has an `id` and expects exactly one response.
    Request {
        /// The request id (a string or an integer).
        id: Value,
        /// Method name.
        method: String,
        /// Parameters, `{}` when absent.
        params: Value,
    },
    /// A notification: no `id`, no response.
    Notification {
        /// Method name.
        method: String,
        /// Parameters, `{}` when absent.
        params: Value,
    },
    /// A response to a request the server sent. This server sends none, so these are ignored.
    Response,
    /// Not a valid JSON-RPC message; the value is the error response to send.
    Invalid(Value),
}

impl Incoming {
    /// Classifies one JSON value (one element of a batch, or a whole non-batch message).
    pub fn classify(value: Value) -> Self {
        let Value::Object(mut object) = value else {
            return Self::Invalid(error_response(
                Value::Null,
                INVALID_REQUEST,
                "a JSON-RPC message must be an object",
                None,
            ));
        };
        let id = object.remove("id");
        let error_id = id.clone().filter(valid_id).unwrap_or(Value::Null);
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Self::Invalid(error_response(
                error_id,
                INVALID_REQUEST,
                "\"jsonrpc\" must be \"2.0\"",
                None,
            ));
        }
        let Some(method) = object.get("method") else {
            if object.contains_key("result") || object.contains_key("error") {
                return Self::Response;
            }
            return Self::Invalid(error_response(
                error_id,
                INVALID_REQUEST,
                "a request needs a \"method\"",
                None,
            ));
        };
        let Some(method) = method.as_str().map(str::to_owned) else {
            return Self::Invalid(error_response(
                error_id,
                INVALID_REQUEST,
                "\"method\" must be a string",
                None,
            ));
        };
        let params = match object.remove("params") {
            None | Some(Value::Null) => json!({}),
            Some(params @ Value::Object(_)) => params,
            Some(_) => {
                return Self::Invalid(error_response(
                    error_id,
                    INVALID_REQUEST,
                    "\"params\" must be an object",
                    None,
                ));
            }
        };
        match id {
            None => Self::Notification { method, params },
            Some(id) if valid_id(&id) => Self::Request { id, method, params },
            Some(_) => Self::Invalid(error_response(
                Value::Null,
                INVALID_REQUEST,
                "\"id\" must be a string or an integer",
                None,
            )),
        }
    }
}

/// MCP request ids are strings or integers, never null.
fn valid_id(id: &Value) -> bool {
    id.is_string() || id.is_i64() || id.is_u64()
}

/// A success response.
pub fn result_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// An error response; `data` is attached when given.
pub fn error_response(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let (Some(data), Some(object)) = (data, error.as_object_mut()) {
        object.insert("data".into(), data);
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

/// A notification sent by the server.
pub fn notification(method: &str, params: Option<Value>) -> Value {
    match params {
        Some(params) => json!({"jsonrpc": "2.0", "method": method, "params": params}),
        None => json!({"jsonrpc": "2.0", "method": method}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_requests_notifications_and_responses() {
        assert_eq!(
            Incoming::classify(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})),
            Incoming::Request {
                id: json!(1),
                method: "ping".into(),
                params: json!({})
            }
        );
        assert!(matches!(
            Incoming::classify(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
            Incoming::Notification { .. }
        ));
        assert_eq!(
            Incoming::classify(json!({"jsonrpc": "2.0", "id": "a", "result": {}})),
            Incoming::Response
        );
    }

    #[test]
    fn rejects_malformed_messages() {
        for bad in [
            json!(3),
            json!({"id": 1, "method": "ping"}),
            json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 1.5, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": 7}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "x", "params": [1]}),
            json!({"jsonrpc": "2.0", "id": 1}),
        ] {
            let Incoming::Invalid(response) = Incoming::classify(bad.clone()) else {
                panic!("accepted {bad}");
            };
            assert_eq!(response["error"]["code"], INVALID_REQUEST, "{bad}");
        }
    }

    #[test]
    fn version_features() {
        assert!(has_structured_output("2025-06-18"));
        assert!(has_structured_output("2025-11-25"));
        assert!(!has_structured_output("2025-03-26"));
        assert!(is_supported_version(LATEST_PROTOCOL_VERSION));
        assert!(!is_supported_version("2026-07-28"));
    }
}
