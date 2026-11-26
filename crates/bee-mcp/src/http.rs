//! The Streamable HTTP transport (`/mcp` on `bee-server`).
//!
//! One endpoint, served at the router's root:
//!
//! - `POST` carries one JSON-RPC message or a batch. An `initialize` request creates a session
//!   and the response carries its id in `Mcp-Session-Id`; every later request must send that
//!   header (missing: 400, unknown or expired: 404). Requests get `application/json`, or
//!   `text/event-stream` when notifications (such as `notifications/tools/list_changed` after
//!   `enable_toolset`) precede the response. Notifications and responses alone get 202.
//! - `DELETE` with `Mcp-Session-Id` ends the session (204).
//! - `GET` answers 405: the server never sends messages outside a request.
//!
//! The `Authorization` header of each request is forwarded to the bee server, so every MCP
//! client acts with its own token (falling back to the configured token when absent). A
//! session is bound to the credential that created it; a request presenting a different one is
//! answered as if the session did not exist.
//!
//! `Origin` is validated against DNS rebinding: absent, loopback, same-host and configured
//! origins pass ([`McpConfig::allowed_origins`]), anything else gets 403.
//! `MCP-Protocol-Version`, when sent, must be a supported version (else 400).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::any;
use http::header::{ACCEPT, ALLOW, AUTHORIZATION, CONTENT_TYPE, HOST, HeaderMap, ORIGIN};
use http::{HeaderValue, Method, StatusCode};
use serde_json::Value;

use crate::protocol::{
    INTERNAL_ERROR, INVALID_REQUEST, Incoming, PARSE_ERROR, error_response, is_supported_version,
};
use crate::server::{CallContext, ConfigError, McpConfig, McpServer, Session};

/// Header carrying the session id.
pub const SESSION_HEADER: &str = "mcp-session-id";

/// Header carrying the negotiated protocol version.
pub const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// An axum router serving MCP over Streamable HTTP at its root.
///
/// `bee-server` mounts it with `Router::new().nest("/mcp", mcp_router(config)?)`, which
/// answers at `/mcp`. `config.client.endpoint` should point at the server's own HTTP/JSON
/// surface (usually its loopback address); callers' `Authorization` headers are forwarded.
pub fn mcp_router(config: McpConfig) -> Result<Router, ConfigError> {
    Ok(router_for(McpServer::new(config)?))
}

/// The Streamable HTTP router for an existing server.
pub fn router_for(server: McpServer) -> Router {
    let state = Arc::new(HttpState {
        server,
        sessions: Mutex::new(HashMap::new()),
    });
    Router::new().route("/", any(handle)).with_state(state)
}

struct HttpSession {
    session: Session,
    authorization: Option<String>,
    last_used: Mutex<Instant>,
}

struct HttpState {
    server: McpServer,
    sessions: Mutex<HashMap<String, Arc<HttpSession>>>,
}

impl HttpState {
    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<HttpSession>>> {
        self.sessions.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The session for `id` if it exists, has not expired and belongs to `authorization`.
    fn lookup(&self, id: &str, authorization: &Option<String>) -> Option<Arc<HttpSession>> {
        let idle = self.server.config().session_idle_timeout;
        let mut sessions = self.sessions();
        let found = sessions.get(id).cloned()?;
        let mut last = found.last_used.lock().unwrap_or_else(|p| p.into_inner());
        if last.elapsed() > idle {
            drop(last);
            sessions.remove(id);
            return None;
        }
        if &found.authorization != authorization {
            return None;
        }
        *last = Instant::now();
        drop(last);
        Some(found)
    }

    fn insert(&self, id: String, session: Arc<HttpSession>) {
        let config = self.server.config();
        let mut sessions = self.sessions();
        let idle = config.session_idle_timeout;
        sessions.retain(|_, s| {
            s.last_used
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .elapsed()
                <= idle
        });
        while sessions.len() >= config.max_sessions.max(1) {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, s)| *s.last_used.lock().unwrap_or_else(|p| p.into_inner()))
                .map(|(k, _)| k.clone());
            match oldest {
                Some(key) => sessions.remove(&key),
                None => break,
            };
        }
        sessions.insert(id, session);
    }
}

fn header_str(headers: &HeaderMap, name: impl http::header::AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn plain(status: StatusCode, message: &str) -> Response {
    let body = serde_json::to_vec(&error_response(Value::Null, INVALID_REQUEST, message, None))
        .unwrap_or_default();
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_default()
}

fn json_response(status: StatusCode, value: &Value, session_id: Option<&str>) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json");
    if let Some(id) = session_id {
        builder = builder.header(SESSION_HEADER, id);
    }
    builder
        .body(Body::from(serde_json::to_vec(value).unwrap_or_default()))
        .unwrap_or_default()
}

fn sse_response(messages: &[Value]) -> Response {
    let mut body = String::new();
    for message in messages {
        body.push_str("event: message\ndata: ");
        body.push_str(&serde_json::to_string(message).unwrap_or_default());
        body.push_str("\n\n");
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from(body))
        .unwrap_or_default()
}

/// Host part of an origin or host header value, lowercased, without port.
fn host_of(authority: &str) -> String {
    let authority = authority
        .split_once("://")
        .map_or(authority, |(_, rest)| rest);
    let authority = authority.split('/').next().unwrap_or(authority);
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    host.to_ascii_lowercase()
}

fn origin_allowed(config: &McpConfig, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if config
        .allowed_origins
        .iter()
        .any(|a| a == "*" || a.eq_ignore_ascii_case(origin))
    {
        return true;
    }
    let host = host_of(origin);
    if matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") {
        return true;
    }
    let authority = origin
        .split_once("://")
        .map_or(origin, |(_, rest)| rest)
        .to_ascii_lowercase();
    header_str(headers, HOST).is_some_and(|h| h.eq_ignore_ascii_case(&authority))
}

fn accepts(headers: &HeaderMap) -> bool {
    match header_str(headers, ACCEPT) {
        None => true,
        Some(accept) => {
            let accept = accept.to_ascii_lowercase();
            accept.contains("application/json")
                || accept.contains("text/event-stream")
                || accept.contains("*/*")
                || accept.contains("application/*")
        }
    }
}

fn new_session_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

async fn handle(State(state): State<Arc<HttpState>>, request: Request) -> Response {
    let config = state.server.config();
    if !origin_allowed(config, request.headers()) {
        return plain(StatusCode::FORBIDDEN, "origin not allowed");
    }
    match *request.method() {
        Method::POST => post(&state, request).await,
        Method::DELETE => {
            let authorization = header_str(request.headers(), AUTHORIZATION).map(str::to_owned);
            let Some(id) = header_str(request.headers(), SESSION_HEADER) else {
                return plain(StatusCode::BAD_REQUEST, "missing Mcp-Session-Id");
            };
            if state.lookup(id, &authorization).is_none() {
                return plain(StatusCode::NOT_FOUND, "unknown session");
            }
            state.sessions().remove(id);
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .unwrap_or_default()
        }
        _ => {
            let mut response = plain(
                StatusCode::METHOD_NOT_ALLOWED,
                "this endpoint serves POST and DELETE; it opens no standalone event stream",
            );
            response
                .headers_mut()
                .insert(ALLOW, HeaderValue::from_static("POST, DELETE"));
            response
        }
    }
}

async fn post(state: &Arc<HttpState>, request: Request) -> Response {
    let config = state.server.config();
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    if let Some(version) = header_str(&headers, PROTOCOL_VERSION_HEADER)
        && !is_supported_version(version)
    {
        return plain(
            StatusCode::BAD_REQUEST,
            &format!("unsupported MCP-Protocol-Version {version}"),
        );
    }
    let content_type = header_str(&headers, CONTENT_TYPE)
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
    if content_type.as_deref() != Some("application/json") {
        return plain(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json",
        );
    }
    if !accepts(&headers) {
        return plain(
            StatusCode::NOT_ACCEPTABLE,
            "Accept must allow application/json and text/event-stream",
        );
    }
    let Ok(bytes) = axum::body::to_bytes(body, config.max_body_bytes).await else {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(e) => {
            let error =
                error_response(Value::Null, PARSE_ERROR, &format!("parse error: {e}"), None);
            return json_response(StatusCode::BAD_REQUEST, &error, None);
        }
    };
    let authorization = header_str(&headers, AUTHORIZATION).map(str::to_owned);
    let context = CallContext {
        authorization: authorization.clone(),
    };
    let (messages, batch) = match value {
        Value::Array(items) if items.is_empty() => {
            return plain(StatusCode::BAD_REQUEST, "empty batch");
        }
        Value::Array(items) => (items.into_iter().map(Incoming::classify).collect(), true),
        single => (vec![Incoming::classify(single)], false),
    };
    let is_initialize =
        |m: &Incoming| matches!(m, Incoming::Request { method, .. } if method == "initialize");

    if messages.iter().any(is_initialize) {
        if batch {
            return plain(
                StatusCode::BAD_REQUEST,
                "initialize must be sent alone, not in a batch",
            );
        }
        let Some(id) = new_session_id() else {
            let error = error_response(
                Value::Null,
                INTERNAL_ERROR,
                "no randomness for a session id",
                None,
            );
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &error, None);
        };
        let session = Arc::new(HttpSession {
            session: state.server.new_session(),
            authorization,
            last_used: Mutex::new(Instant::now()),
        });
        let mut handled = Vec::new();
        for message in messages {
            handled.push(
                state
                    .server
                    .handle(&session.session, message, &context)
                    .await,
            );
        }
        let response = handled
            .into_iter()
            .find_map(|h| h.response)
            .unwrap_or(Value::Null);
        if response.get("result").is_some() {
            state.insert(id.clone(), session);
            return json_response(StatusCode::OK, &response, Some(&id));
        }
        return json_response(StatusCode::OK, &response, None);
    }

    let Some(session_id) = header_str(&headers, SESSION_HEADER) else {
        return plain(
            StatusCode::BAD_REQUEST,
            "missing Mcp-Session-Id: send initialize first",
        );
    };
    let Some(session) = state.lookup(session_id, &authorization) else {
        return plain(
            StatusCode::NOT_FOUND,
            "unknown or expired session: send initialize again",
        );
    };

    let has_request = messages
        .iter()
        .any(|m| matches!(m, Incoming::Request { .. }));
    let mut notifications = Vec::new();
    let mut responses = Vec::new();
    for message in messages {
        let handled = state
            .server
            .handle(&session.session, message, &context)
            .await;
        notifications.extend(handled.notifications);
        responses.extend(handled.response);
    }
    if !has_request {
        if responses.is_empty() {
            return Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(Body::empty())
                .unwrap_or_default();
        }
        let body = if batch {
            Value::Array(responses)
        } else {
            responses.swap_remove(0)
        };
        return json_response(StatusCode::BAD_REQUEST, &body, None);
    }
    let body = if batch {
        Value::Array(responses)
    } else {
        responses.into_iter().next().unwrap_or(Value::Null)
    };
    if notifications.is_empty() {
        json_response(StatusCode::OK, &body, None)
    } else {
        notifications.push(body);
        sse_response(&notifications)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_extracted() {
        assert_eq!(host_of("http://localhost:3000"), "localhost");
        assert_eq!(host_of("https://[::1]:8443"), "::1");
        assert_eq!(host_of("https://Hub.Example.com"), "hub.example.com");
        assert_eq!(host_of("hub.example.com:443"), "hub.example.com");
    }

    #[test]
    fn origins() {
        let config = McpConfig::new(crate::client::ClientConfig::new("http://127.0.0.1:1"));
        let mut headers = HeaderMap::new();
        assert!(origin_allowed(&config, &headers));
        headers.insert(ORIGIN, HeaderValue::from_static("http://localhost:5173"));
        assert!(origin_allowed(&config, &headers));
        headers.insert(ORIGIN, HeaderValue::from_static("https://evil.example"));
        assert!(!origin_allowed(&config, &headers));
        headers.insert(HOST, HeaderValue::from_static("evil.example"));
        assert!(origin_allowed(&config, &headers));
        let mut allowed = config.clone();
        allowed.allowed_origins = vec!["https://hub.example.com".into()];
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_static("https://hub.example.com"));
        assert!(origin_allowed(&allowed, &headers));
    }

    #[test]
    fn session_ids_are_random_hex() {
        let a = new_session_id().expect("id");
        let b = new_session_id().expect("id");
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }
}
