//! The HTTP/JSON client every agent-facing surface shares.
//!
//! [`RpcClient::call_rpc_json`] is the single code path from "service, method, JSON request"
//! to "JSON response or [`ErrorDetail`]". The MCP tools use it, and `bee-cli` uses it for
//! its RPC-backed commands, so the API, the CLI and MCP send identical requests and surface
//! identical errors (PRODUCT §3.2).
//!
//! It speaks the Connect-shaped HTTP/JSON surface of SPEC §12.1 and works against any server
//! that serves it: `POST {endpoint}/bee.v1.<Service>/<Method>` with a proto3-JSON body
//! (`application/json`) for unary RPCs and enveloped frames (`application/connect+json`) for
//! streaming ones. Requests are checked against the request message locally first, so a
//! malformed request fails with `INVALID_ARGUMENT` (subcode `bad_json`) without a round trip.
//!
//! Every failure is an [`ErrorDetail`] with a SPEC §12.2 reason: the server's own detail when it
//! sent one, else the default reason for the Connect code or HTTP status, and `UNAVAILABLE` or
//! `DEADLINE_EXCEEDED` (both retryable) for transport failures and timeouts.

use std::time::Duration;

use bee_proto::connect::{ConnectError, EndStream};
use bee_proto::envelope::{self, FLAG_END_STREAM};
use bee_proto::errors::{self, code_from_connect_name, default_reason};
use bee_proto::methods::{METHODS, MethodDescriptor, MethodKind, find_method};
use bee_proto::v1::{ErrorDetail, ErrorReason};
use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, USER_AGENT};
use http::{Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

/// Default limit on one response message or unary response body: 16 MiB.
pub const MAX_MESSAGE_BYTES: usize = 16 << 20;

/// Where and how to reach a bee server.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Base URL of the HTTP/JSON surface, e.g. `https://bee.example.com` or
    /// `http://127.0.0.1:7420`. A path prefix is kept (`https://host/api` serves
    /// `https://host/api/bee.v1.<Service>/<Method>`).
    pub endpoint: String,
    /// Bearer token sent as `Authorization: Bearer <token>`, if any.
    pub token: Option<String>,
    /// Deadline of one unary call; also sent as `Connect-Timeout-Ms`.
    pub request_timeout: Duration,
}

impl ClientConfig {
    /// Default endpoint when `BEE_ENDPOINT` is unset: a local `bee-server --dev`.
    pub const DEFAULT_ENDPOINT: &'static str = "http://127.0.0.1:7420";

    /// A config for `endpoint` with no token and a 60 s deadline.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: None,
            request_timeout: Duration::from_secs(60),
        }
    }

    /// Reads `BEE_ENDPOINT` (default [`Self::DEFAULT_ENDPOINT`]) and `BEE_TOKEN`.
    pub fn from_env() -> Self {
        let mut config = Self::new(
            std::env::var("BEE_ENDPOINT")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| Self::DEFAULT_ENDPOINT.to_owned()),
        );
        config.token = std::env::var("BEE_TOKEN")
            .ok()
            .filter(|v| !v.trim().is_empty());
        config
    }
}

/// Limits on collecting a server stream into one result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamLimits {
    /// Stop after this many messages.
    pub max_items: usize,
    /// Stop after this long, counted from sending the request.
    pub timeout: Duration,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            max_items: 100,
            timeout: Duration::from_secs(10),
        }
    }
}

/// Why a collected stream stopped before the server ended it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truncation {
    /// [`StreamLimits::max_items`] messages arrived.
    MaxItems,
    /// [`StreamLimits::timeout`] passed.
    Timeout,
}

impl Truncation {
    /// Wire name: `max_items` or `timeout`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxItems => "max_items",
            Self::Timeout => "timeout",
        }
    }
}

/// The messages of a server stream, collected up to [`StreamLimits`].
#[derive(Clone, Debug, PartialEq)]
pub struct StreamOutput {
    /// Messages in arrival order, proto3-JSON.
    pub items: Vec<Value>,
    /// Set when collection stopped before the server ended the stream.
    pub truncated: Option<Truncation>,
    /// Set when the server ended the stream with an error after `items`.
    pub error: Option<ErrorDetail>,
}

impl StreamOutput {
    /// JSON form: `{"items": [...], "count": n, "truncated": bool, "truncatedReason"?: ...}`.
    pub fn to_json(&self) -> Value {
        let mut out = json!({
            "items": self.items,
            "count": self.items.len(),
            "truncated": self.truncated.is_some(),
        });
        if let (Some(t), Some(object)) = (self.truncated, out.as_object_mut()) {
            object.insert("truncatedReason".into(), json!(t.as_str()));
        }
        out
    }
}

/// Result of one call: a single response message or a collected stream.
#[derive(Clone, Debug, PartialEq)]
pub enum RpcOutput {
    /// Unary and client-streaming RPCs: the response message.
    Message(Value),
    /// Server-streaming and bidi RPCs: the collected messages.
    Stream(StreamOutput),
}

impl RpcOutput {
    /// The response as one JSON document ([`StreamOutput::to_json`] for streams).
    pub fn to_json(&self) -> Value {
        match self {
            Self::Message(value) => value.clone(),
            Self::Stream(stream) => stream.to_json(),
        }
    }
}

/// The CLI/MCP error document of SPEC §12.2: `{reason, message, retryable, payload?}`.
///
/// `payload` holds every other field of the detail (`subcode`, `retryAfterMs`, `requestId` and
/// the typed payload such as `conflicts`) in proto3-JSON, and is omitted when empty. This is
/// the `error` object of `schemas/v1/error.schema.json` and the structured content of a
/// failed MCP tool call.
pub fn error_document(detail: &ErrorDetail) -> Value {
    let mut payload = match serde_json::to_value(detail) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    for key in ["reason", "message", "retryable"] {
        payload.remove(key);
    }
    let mut doc = json!({
        "reason": detail.reason,
        "message": detail.message,
        "retryable": detail.retryable,
    });
    if !payload.is_empty()
        && let Some(object) = doc.as_object_mut()
    {
        object.insert("payload".into(), Value::Object(payload));
    }
    doc
}

/// An [`ErrorDetail`] for `reason` with a subcode.
pub fn error_with_subcode(
    reason: ErrorReason,
    subcode: &str,
    message: impl Into<String>,
) -> ErrorDetail {
    let mut detail = errors::error_detail(reason, message);
    detail.subcode = subcode.to_owned();
    detail
}

/// Finds an RPC from loose names: the service as `bee.v1.QueryService`, `QueryService`,
/// `Query` or `query`, and the method as `Log` or `log` (snake_case also works, e.g.
/// `who_am_i`).
pub fn resolve_method(service: &str, method: &str) -> Option<&'static MethodDescriptor> {
    let exact = if service.starts_with("bee.") {
        format!("/{service}/{method}")
    } else {
        format!("/bee.v1.{service}/{method}")
    };
    if let Some(found) = find_method(&exact) {
        return Some(found);
    }
    let want_service = normalize(service.rsplit('.').next().unwrap_or(service));
    let want_service = want_service
        .strip_suffix("service")
        .unwrap_or(&want_service)
        .to_owned();
    let want_method = normalize(method);
    METHODS.iter().find(|m| {
        let short = m.service.rsplit('.').next().unwrap_or(m.service);
        let short = short.strip_suffix("Service").unwrap_or(short);
        normalize(short) == want_service && normalize(m.method) == want_method
    })
}

fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// A client of the HTTP/JSON surface. Cheap to clone; clones share connections.
#[derive(Clone)]
pub struct RpcClient {
    http: HttpClient,
    endpoint: String,
    authorization: Option<String>,
    request_timeout: Duration,
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient")
            .field("endpoint", &self.endpoint)
            .field("authorized", &self.authorization.is_some())
            .finish()
    }
}

impl RpcClient {
    /// A client for `config`. `https://` endpoints use rustls with the Mozilla root set;
    /// `http://` is for loopback and trusted networks (SPEC §12.1).
    pub fn new(config: &ClientConfig) -> Self {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        let http = Client::builder(TokioExecutor::new()).build(connector);
        Self {
            http,
            endpoint: config.endpoint.trim_end_matches('/').to_owned(),
            authorization: config.token.as_ref().map(|t| format!("Bearer {t}")),
            request_timeout: config.request_timeout,
        }
    }

    /// The same client sending `authorization` as the full `Authorization` header value
    /// (e.g. `Bearer <token>`) instead of the configured token. `bee-server` uses this to
    /// forward each MCP caller's own credential.
    pub fn with_authorization(&self, authorization: Option<String>) -> Self {
        let mut client = self.clone();
        if authorization.is_some() {
            client.authorization = authorization;
        }
        client
    }

    /// Base URL this client calls.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Calls one RPC with a proto3-JSON request and returns the proto3-JSON response.
    ///
    /// Server-streaming RPCs return [`StreamOutput::to_json`] collected with the default
    /// [`StreamLimits`]; client-streaming RPCs take `{"messages": [<request>, ...]}`.
    pub async fn call_rpc_json(
        &self,
        service: &str,
        method: &str,
        request: &Value,
    ) -> Result<Value, ErrorDetail> {
        let Some(descriptor) = resolve_method(service, method) else {
            return Err(errors::error_detail(
                ErrorReason::Unimplemented,
                format!("no RPC {service}/{method} in bee.v1"),
            ));
        };
        match self
            .call_method(descriptor, request, StreamLimits::default())
            .await?
        {
            RpcOutput::Stream(StreamOutput {
                error: Some(error), ..
            }) => Err(error),
            output => Ok(output.to_json()),
        }
    }

    /// Calls one RPC. A stream that fails after some messages is `Ok` with
    /// [`StreamOutput::error`] set, so callers can show what arrived.
    pub async fn call_method(
        &self,
        method: &MethodDescriptor,
        request: &Value,
        limits: StreamLimits,
    ) -> Result<RpcOutput, ErrorDetail> {
        let messages: Vec<&Value> = match method.kind {
            MethodKind::Unary | MethodKind::ServerStreaming => vec![request],
            MethodKind::ClientStreaming | MethodKind::BidiStreaming => {
                match request.get("messages") {
                    Some(Value::Array(items)) => items.iter().collect(),
                    _ => {
                        return Err(error_with_subcode(
                            ErrorReason::InvalidArgument,
                            "bad_json",
                            format!(
                                "{} streams requests: pass {{\"messages\": [<{}>, ...]}}",
                                method.path, method.input_type
                            ),
                        ));
                    }
                }
            }
        };
        let mut encoded = Vec::with_capacity(messages.len());
        for message in messages {
            if !message.is_object() {
                return Err(error_with_subcode(
                    ErrorReason::InvalidArgument,
                    "bad_json",
                    format!("a {} must be a JSON object", method.input_type),
                ));
            }
            let bytes = serde_json::to_vec(message).unwrap_or_default();
            if let Err(e) = (method.request_json_to_proto)(&bytes) {
                return Err(error_with_subcode(
                    ErrorReason::InvalidArgument,
                    "bad_json",
                    format!("not a valid {}: {e}", method.input_type),
                ));
            }
            encoded.push(bytes);
        }
        let unary_request = method.kind.is_unary();
        let body: Bytes = if unary_request {
            encoded.pop().unwrap_or_default().into()
        } else {
            let mut body = Vec::new();
            for message in &encoded {
                body.extend_from_slice(&envelope::encode(0, message));
            }
            body.into()
        };
        let timeout = if matches!(
            method.kind,
            MethodKind::ServerStreaming | MethodKind::BidiStreaming
        ) {
            limits.timeout
        } else {
            self.request_timeout
        };
        let deadline = Instant::now() + timeout;
        let response = self
            .send(method, body, unary_request, timeout, deadline)
            .await?;
        let (parts, body) = response.into_parts();
        if unary_request {
            let bytes = read_all(body, deadline).await?;
            if parts.status != StatusCode::OK {
                return Err(error_from_body(parts.status, &bytes));
            }
            return parse_message(method, &bytes).map(RpcOutput::Message);
        }
        if parts.status != StatusCode::OK {
            let bytes = read_all(body, deadline).await?;
            return Err(error_from_body(parts.status, &bytes));
        }
        let max_items = match method.kind {
            MethodKind::ClientStreaming => 1,
            _ => limits.max_items.max(1),
        };
        let stream = collect_stream(method, body, max_items, deadline).await?;
        match method.kind {
            MethodKind::ClientStreaming => match stream {
                StreamOutput {
                    error: Some(error), ..
                } => Err(error),
                StreamOutput { mut items, .. } if !items.is_empty() => {
                    Ok(RpcOutput::Message(items.swap_remove(0)))
                }
                _ => Err(errors::error_detail(
                    ErrorReason::Internal,
                    format!("{} returned no response message", method.path),
                )),
            },
            _ => Ok(RpcOutput::Stream(stream)),
        }
    }

    async fn send(
        &self,
        method: &MethodDescriptor,
        body: Bytes,
        unary: bool,
        timeout: Duration,
        deadline: Instant,
    ) -> Result<http::Response<hyper::body::Incoming>, ErrorDetail> {
        let url = format!("{}{}", self.endpoint, method.path);
        let uri: Uri = url.parse().map_err(|e| {
            error_with_subcode(
                ErrorReason::InvalidArgument,
                "bad_endpoint",
                format!("endpoint {url:?} is not a valid URL: {e}"),
            )
        })?;
        let content_type = if unary {
            "application/json"
        } else {
            "application/connect+json"
        };
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(CONTENT_TYPE, content_type)
            .header("connect-protocol-version", "1")
            .header("connect-timeout-ms", timeout.as_millis().to_string())
            .header(USER_AGENT, concat!("bee-mcp/", env!("CARGO_PKG_VERSION")));
        if let Some(auth) = &self.authorization {
            let value = HeaderValue::from_str(auth).map_err(|_| {
                errors::error_detail(
                    ErrorReason::Unauthenticated,
                    "the token contains characters not allowed in a header",
                )
            })?;
            builder = builder.header(AUTHORIZATION, value);
        }
        let request = builder.body(Full::new(body)).map_err(|e| {
            errors::error_detail(ErrorReason::Internal, format!("building request: {e}"))
        })?;
        match tokio::time::timeout_at(deadline, self.http.request(request)).await {
            Err(_) => Err(deadline_exceeded(method)),
            Ok(Err(e)) => Err(errors::error_detail(
                ErrorReason::Unavailable,
                format!("cannot reach {}: {}", self.endpoint, error_chain(&e)),
            )),
            Ok(Ok(response)) => Ok(response),
        }
    }
}

/// Calls one RPC with a fresh client for `config`; see [`RpcClient::call_rpc_json`].
pub async fn call_rpc_json(
    config: &ClientConfig,
    service: &str,
    method: &str,
    request: &Value,
) -> Result<Value, ErrorDetail> {
    RpcClient::new(config)
        .call_rpc_json(service, method, request)
        .await
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        text.push_str(": ");
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}

fn deadline_exceeded(method: &MethodDescriptor) -> ErrorDetail {
    errors::error_detail(
        ErrorReason::DeadlineExceeded,
        format!("{} did not answer before the deadline", method.path),
    )
}

async fn read_all(body: hyper::body::Incoming, deadline: Instant) -> Result<Bytes, ErrorDetail> {
    let limited = http_body_util::Limited::new(body, MAX_MESSAGE_BYTES);
    match tokio::time::timeout_at(deadline, limited.collect()).await {
        Err(_) => Err(errors::error_detail(
            ErrorReason::DeadlineExceeded,
            "the response did not complete before the deadline",
        )),
        Ok(Err(e)) => Err(errors::error_detail(
            ErrorReason::Unavailable,
            format!("reading the response: {e}"),
        )),
        Ok(Ok(collected)) => Ok(collected.to_bytes()),
    }
}

fn parse_message(method: &MethodDescriptor, bytes: &[u8]) -> Result<Value, ErrorDetail> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    serde_json::from_slice(bytes).map_err(|e| {
        errors::error_detail(
            ErrorReason::Internal,
            format!("{} answered with invalid JSON: {e}", method.path),
        )
    })
}

/// Connect code name the Connect protocol assigns to an HTTP status whose body is not a
/// Connect error.
fn connect_code_for_http_status(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "internal",
        401 => "unauthenticated",
        403 => "permission_denied",
        404 => "unimplemented",
        429 | 502 | 503 | 504 => "unavailable",
        _ => "unknown",
    }
}

/// The default reason for a Connect code name; `INTERNAL` for names this build does not know.
fn reason_for_connect_code(name: &str) -> ErrorReason {
    code_from_connect_name(name).map_or(ErrorReason::Internal, default_reason)
}

/// The [`ErrorDetail`] of a Connect error, or one built from its code.
fn detail_from_connect(error: &ConnectError) -> ErrorDetail {
    if let Some(detail) = error.error_detail() {
        return detail;
    }
    let from_debug = error
        .details
        .iter()
        .filter(|d| d.type_name == errors::ERROR_DETAIL_TYPE)
        .filter_map(|d| d.debug.clone())
        .find_map(|debug| serde_json::from_value::<ErrorDetail>(debug).ok());
    from_debug.unwrap_or_else(|| {
        errors::error_detail(reason_for_connect_code(&error.code), error.message.clone())
    })
}

/// The error of a non-200 response: its Connect error body, or the HTTP status mapping.
fn error_from_body(status: StatusCode, bytes: &[u8]) -> ErrorDetail {
    if let Ok(error) = serde_json::from_slice::<ConnectError>(bytes) {
        return detail_from_connect(&error);
    }
    let text = String::from_utf8_lossy(bytes);
    let snippet: String = text.trim().chars().take(200).collect();
    let message = if snippet.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {snippet}")
    };
    errors::error_detail(
        reason_for_connect_code(connect_code_for_http_status(status)),
        message,
    )
}

/// Reads Connect stream frames until the end-stream frame, `max_items` messages or the
/// deadline.
async fn collect_stream(
    method: &MethodDescriptor,
    mut body: hyper::body::Incoming,
    max_items: usize,
    deadline: Instant,
) -> Result<StreamOutput, ErrorDetail> {
    let mut decoder = envelope::Decoder::new(MAX_MESSAGE_BYTES);
    let mut out = StreamOutput {
        items: Vec::new(),
        truncated: None,
        error: None,
    };
    loop {
        let frame = match tokio::time::timeout_at(deadline, body.frame()).await {
            Err(_) => {
                out.truncated = Some(Truncation::Timeout);
                return Ok(out);
            }
            Ok(None) => {
                out.error = Some(errors::error_detail(
                    ErrorReason::Unavailable,
                    format!(
                        "{} stream ended without an end-of-stream frame",
                        method.path
                    ),
                ));
                return Ok(out);
            }
            Ok(Some(Err(e))) => {
                out.error = Some(errors::error_detail(
                    ErrorReason::Unavailable,
                    format!("reading the {} stream: {e}", method.path),
                ));
                return Ok(out);
            }
            Ok(Some(Ok(frame))) => frame,
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        decoder.push(&data);
        loop {
            let next = decoder.next_frame().map_err(|e| {
                errors::error_detail(
                    ErrorReason::Internal,
                    format!("{} sent a malformed stream: {e}", method.path),
                )
            })?;
            let Some(next) = next else { break };
            if next.flags & FLAG_END_STREAM != 0 {
                let end: EndStream = if next.data.iter().all(u8::is_ascii_whitespace) {
                    EndStream::default()
                } else {
                    serde_json::from_slice(&next.data).map_err(|e| {
                        errors::error_detail(
                            ErrorReason::Internal,
                            format!("{} sent an invalid end-of-stream frame: {e}", method.path),
                        )
                    })?
                };
                out.error = end.error.as_ref().map(detail_from_connect);
                return Ok(out);
            }
            out.items.push(parse_message(method, &next.data)?);
            if out.items.len() >= max_items {
                out.truncated = Some(Truncation::MaxItems);
                return Ok(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_loose_names() {
        for (service, method) in [
            ("bee.v1.QueryService", "Log"),
            ("QueryService", "Log"),
            ("Query", "log"),
            ("query", "LOG"),
        ] {
            let found = resolve_method(service, method).expect("found");
            assert_eq!(found.path, "/bee.v1.QueryService/Log");
        }
        assert_eq!(
            resolve_method("auth", "who_am_i").map(|m| m.path),
            Some("/bee.v1.AuthService/WhoAmI")
        );
        assert!(resolve_method("query", "nope").is_none());
    }

    #[test]
    fn error_document_keeps_four_fields() {
        let mut detail = errors::error_detail(ErrorReason::ResourceExhausted, "slow down");
        detail.retry_after_ms = 250;
        let doc = error_document(&detail);
        assert_eq!(doc["reason"], "RESOURCE_EXHAUSTED");
        assert_eq!(doc["retryable"], true);
        assert_eq!(doc["payload"]["retryAfterMs"], "250");
        let keys: Vec<&String> = doc.as_object().expect("object").keys().collect();
        assert_eq!(keys.len(), 4);

        let plain = error_document(&errors::error_detail(ErrorReason::NotFound, "gone"));
        assert!(plain.get("payload").is_none());
    }

    #[test]
    fn errors_without_connect_bodies_map_by_status() {
        let detail = error_from_body(StatusCode::SERVICE_UNAVAILABLE, b"<html>down</html>");
        assert_eq!(detail.reason, "UNAVAILABLE");
        assert!(detail.retryable);
        assert!(detail.message.contains("down"));
        assert_eq!(
            error_from_body(StatusCode::UNAUTHORIZED, b"").reason,
            "UNAUTHENTICATED"
        );
        assert_eq!(
            error_from_body(StatusCode::IM_A_TEAPOT, b"").reason,
            "INTERNAL"
        );
    }

    #[test]
    fn connect_errors_prefer_the_detail_then_debug_then_code() {
        let full = ConnectError::from_reason(ErrorReason::MergeConflict, "conflicts");
        let body = serde_json::to_vec(&full).expect("json");
        assert_eq!(
            error_from_body(StatusCode::BAD_REQUEST, &body).reason,
            "MERGE_CONFLICT"
        );

        let debug_only = json!({"code": "failed_precondition", "message": "leased", "details": [
            {"type": "bee.v1.ErrorDetail", "value": "!!", "debug": {"reason": "LEASED", "message": "leased"}}
        ]});
        let body = serde_json::to_vec(&debug_only).expect("json");
        assert_eq!(
            error_from_body(StatusCode::BAD_REQUEST, &body).reason,
            "LEASED"
        );

        let bare = json!({"code": "aborted", "message": "stale"});
        let body = serde_json::to_vec(&bare).expect("json");
        let detail = error_from_body(StatusCode::CONFLICT, &body);
        assert_eq!(detail.reason, "VERSION_MISMATCH");
        assert_eq!(detail.message, "stale");
    }

    #[test]
    fn stream_output_json_notes_truncation() {
        let out = StreamOutput {
            items: vec![json!({"a": 1})],
            truncated: Some(Truncation::Timeout),
            error: None,
        };
        let value = out.to_json();
        assert_eq!(value["count"], 1);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["truncatedReason"], "timeout");
    }
}
