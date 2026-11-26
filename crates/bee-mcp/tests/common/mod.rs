//! Shared test fixtures: a fake bee server speaking the Connect HTTP/JSON shape, MCP
//! clients over in-process stdio pipes and over HTTP, and a small JSON Schema checker.

#![allow(dead_code)]

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::post;
use bee_mcp::{ClientConfig, McpConfig, McpServer};
use bee_proto::connect::{ConnectError, EndStream};
use bee_proto::envelope::{self, FLAG_END_STREAM};
use bee_proto::errors;
use bee_proto::v1::{
    Changeset, ConflictSet, DiffItem, ErrorReason, Event, LogResponse, PageResponse,
    WhoAmIResponse, error_detail,
};
use bytes::Bytes;
use futures_util::StreamExt as _;
use http::StatusCode;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::net::TcpListener;

/// The token the fake server accepts.
pub const TOKEN: &str = "agent-token-1";

/// One request the fake server saw.
#[derive(Clone, Debug)]
pub struct Seen {
    pub path: String,
    pub content_type: String,
    pub authorization: Option<String>,
    pub protocol_version: Option<String>,
    pub body: Bytes,
}

/// A fake bee server on a loopback port.
pub struct FakeServer {
    pub endpoint: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeServer {
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn last(&self, path: &str) -> Seen {
        self.seen()
            .into_iter()
            .rev()
            .find(|s| s.path == path)
            .unwrap_or_else(|| panic!("no request to {path}"))
    }

    pub fn mcp_config(&self) -> McpConfig {
        let mut client = ClientConfig::new(&self.endpoint);
        client.token = Some(TOKEN.to_owned());
        client.request_timeout = Duration::from_secs(5);
        McpConfig::new(client)
    }

    pub fn mcp_server(&self) -> McpServer {
        McpServer::new(self.mcp_config()).expect("config")
    }
}

fn json_ok(value: Value) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&value).unwrap()))
        .unwrap()
}

fn connect_error(error: &ConnectError) -> Response {
    let status = errors::http_status(error.grpc_code());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(error).unwrap()))
        .unwrap()
}

fn frame(value: &impl serde::Serialize) -> Bytes {
    envelope::encode(0, &serde_json::to_vec(value).unwrap())
}

fn end_frame(end: &EndStream) -> Bytes {
    envelope::encode(FLAG_END_STREAM, &serde_json::to_vec(end).unwrap())
}

fn stream_response(body: Body) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "application/connect+json")
        .body(body)
        .unwrap()
}

async fn record(seen: &Arc<Mutex<Vec<Seen>>>, request: Request) -> Seen {
    let (parts, body) = request.into_parts();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let entry = Seen {
        path: parts.uri.path().to_owned(),
        content_type: header("content-type").unwrap_or_default(),
        authorization: header("authorization"),
        protocol_version: header("connect-protocol-version"),
        body: body.collect().await.unwrap().to_bytes(),
    };
    seen.lock().unwrap().push(entry.clone());
    entry
}

/// The merge conflict `WriteService/Land` reports.
pub fn merge_conflict_detail() -> bee_proto::v1::ErrorDetail {
    let mut detail = errors::error_detail(ErrorReason::MergeConflict, "main moved: 1 conflict");
    detail.request_id = "req-42".into();
    detail.payload = Some(error_detail::Payload::Conflicts(ConflictSet {
        conflicts: vec![bee_proto::v1::Conflict {
            path: b"src/lib.rs".to_vec(),
            kind: bee_proto::v1::ConflictKind::Content as i32,
            ..Default::default()
        }],
        ..Default::default()
    }));
    detail
}

async fn handle(State(seen): State<Arc<Mutex<Vec<Seen>>>>, request: Request) -> Response {
    let entry = record(&seen, request).await;
    if entry.authorization.as_deref() != Some(&format!("Bearer {TOKEN}"))
        && entry.authorization.as_deref() != Some("Bearer other-token")
    {
        return connect_error(&ConnectError::from_reason(
            ErrorReason::Unauthenticated,
            "missing or unknown token",
        ));
    }
    match entry.path.as_str() {
        "/bee.v1.QueryService/Log" => {
            let request: Value = serde_json::from_slice(&entry.body).unwrap_or(json!({}));
            let response = LogResponse {
                changesets: vec![Changeset {
                    generation: 7,
                    ..Default::default()
                }],
                page: Some(PageResponse {
                    next_page_token: format!(
                        "after:{}",
                        request["repo"]["fullName"].as_str().unwrap_or("?")
                    ),
                }),
            };
            json_ok(serde_json::to_value(response).unwrap())
        }
        "/bee.v1.AuthService/WhoAmI" => {
            let response = WhoAmIResponse {
                token_id: entry.authorization.clone().unwrap_or_default(),
                ..Default::default()
            };
            json_ok(serde_json::to_value(response).unwrap())
        }
        "/bee.v1.WriteService/Land" => {
            let status = errors::status_from_detail(&merge_conflict_detail());
            connect_error(&ConnectError::from_status(&status))
        }
        "/bee.v1.RefService/ListRefs" => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("upstream down"))
            .unwrap(),
        "/bee.v1.QueryService/Diff" => {
            let mut body = Vec::new();
            for (i, path) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
                body.extend_from_slice(&frame(&DiffItem {
                    path: path.as_bytes().to_vec(),
                    additions: i as u32 + 1,
                    ..Default::default()
                }));
            }
            body.extend_from_slice(&end_frame(&EndStream::default()));
            stream_response(Body::from(body))
        }
        "/bee.v1.QueryService/ReadPath" => {
            let mut body = Vec::new();
            body.extend_from_slice(&frame(&json!({})));
            let failed = ConnectError::from_reason(ErrorReason::Unavailable, "store hiccup");
            body.extend_from_slice(&end_frame(&EndStream {
                error: Some(failed),
                ..Default::default()
            }));
            stream_response(Body::from(body))
        }
        "/bee.v1.EventService/Subscribe" => {
            // One event, then the stream stays open: collection must stop at its timeout.
            let first = frame(&Event {
                seq: 1,
                kind: "ref.updated".into(),
                ..Default::default()
            });
            let stream = futures_util::stream::once(async move { Ok::<_, Infallible>(first) })
                .chain(futures_util::stream::pending());
            stream_response(Body::from_stream(stream))
        }
        "/bee.v1.WriteService/UploadObjects" => {
            let frames = envelope::decode_all(&entry.body, 1 << 20).unwrap();
            let mut body = Vec::new();
            body.extend_from_slice(&frame(&json!({"stored": frames.len() as u32})));
            body.extend_from_slice(&end_frame(&EndStream::default()));
            stream_response(Body::from(body))
        }
        _ => connect_error(&ConnectError::from_reason(
            ErrorReason::Unimplemented,
            format!("{} is not served by the fake", entry.path),
        )),
    }
}

/// Starts the fake bee server.
pub async fn fake_server() -> FakeServer {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/{service}/{method}", post(handle))
        .with_state(Arc::clone(&seen));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    FakeServer {
        endpoint: format!("http://{addr}"),
        seen,
    }
}

/// An MCP client over in-process pipes connected to `serve_stdio`.
pub struct StdioClient {
    writer: WriteHalf<DuplexStream>,
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    /// Notifications received while waiting for responses.
    pub notifications: Vec<Value>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl StdioClient {
    pub fn start(server: McpServer) -> Self {
        let (client_side, server_side) = tokio::io::duplex(1 << 20);
        let (server_read, server_write) = tokio::io::split(server_side);
        let task = tokio::spawn(async move {
            bee_mcp::stdio::serve_stdio(&server, server_read, server_write).await
        });
        let (client_read, client_write) = tokio::io::split(client_side);
        Self {
            writer: client_write,
            lines: BufReader::new(client_read).lines(),
            notifications: Vec::new(),
            task,
        }
    }

    pub async fn send_raw(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    pub async fn send(&mut self, message: &Value) {
        self.send_raw(&serde_json::to_string(message).unwrap())
            .await;
    }

    pub async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(20), self.lines.next_line())
            .await
            .expect("timed out waiting for the server")
            .unwrap()
            .expect("server closed stdout");
        assert!(!line.contains('\n'));
        serde_json::from_str(&line).expect("server wrote invalid JSON")
    }

    /// Sends a request and returns its response, keeping notifications seen meanwhile.
    pub async fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let message = self.recv().await;
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
            assert!(message.get("method").is_some(), "unexpected {message}");
            self.notifications.push(message);
        }
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    pub async fn initialize(&mut self, version: &str) -> Value {
        let response = self
            .request(
                0,
                "initialize",
                json!({
                    "protocolVersion": version,
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "1"}
                }),
            )
            .await;
        self.notify("notifications/initialized", json!({})).await;
        response
    }

    pub async fn call(&mut self, id: i64, tool: &str, arguments: Value) -> Value {
        let response = self
            .request(
                id,
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
            )
            .await;
        response["result"].clone()
    }

    /// Closes stdin and waits for the server to exit.
    pub async fn close(self) {
        let mut writer = self.writer;
        writer.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(20), self.task)
            .await
            .expect("server did not exit")
            .unwrap()
            .unwrap();
    }
}

/// A raw HTTP response.
pub struct HttpReply {
    pub status: StatusCode,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

impl HttpReply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&self.body)))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// The JSON payloads of an SSE body.
    pub fn sse_messages(&self) -> Vec<Value> {
        String::from_utf8_lossy(&self.body)
            .split("\n\n")
            .filter_map(|event| {
                let data: Vec<&str> = event
                    .lines()
                    .filter_map(|l| l.strip_prefix("data: "))
                    .collect();
                (!data.is_empty()).then(|| serde_json::from_str(&data.join("\n")).unwrap())
            })
            .collect()
    }
}

/// An MCP server mounted at `/mcp` on a loopback port, and a raw HTTP client for it.
pub struct HttpMcp {
    pub url: String,
    client: Client<HttpConnector, Full<Bytes>>,
}

impl HttpMcp {
    pub async fn start(config: McpConfig) -> Self {
        let app = Router::new().nest("/mcp", bee_mcp::mcp_router(config).expect("config"));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url: format!("http://{addr}/mcp"),
            client: Client::builder(TokioExecutor::new()).build_http(),
        }
    }

    pub async fn send(
        &self,
        method: http::Method,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> HttpReply {
        let mut builder = http::Request::builder().method(method).uri(&self.url);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let bytes = body
            .map(|b| Bytes::from(serde_json::to_vec(b).unwrap()))
            .unwrap_or_default();
        let response = tokio::time::timeout(
            Duration::from_secs(20),
            self.client.request(builder.body(Full::new(bytes)).unwrap()),
        )
        .await
        .expect("timed out")
        .unwrap();
        let (parts, body) = response.into_parts();
        HttpReply {
            status: parts.status,
            headers: parts.headers,
            body: body.collect().await.unwrap().to_bytes(),
        }
    }

    /// POSTs one JSON-RPC message with the standard MCP headers.
    pub async fn post(
        &self,
        session: Option<&str>,
        extra: &[(&str, &str)],
        body: &Value,
    ) -> HttpReply {
        let mut headers = vec![
            ("content-type", "application/json"),
            ("accept", "application/json, text/event-stream"),
        ];
        if let Some(id) = session {
            headers.push(("mcp-session-id", id));
            headers.push(("mcp-protocol-version", "2025-11-25"));
        }
        headers.extend_from_slice(extra);
        self.send(http::Method::POST, &headers, Some(body)).await
    }

    /// Initializes a session and returns its id.
    pub async fn initialize(&self, extra: &[(&str, &str)]) -> String {
        let reply = self
            .post(
                None,
                extra,
                &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": {"name": "test", "version": "1"}
                }}),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json()["result"]["protocolVersion"], "2025-11-25");
        let id = reply
            .header("mcp-session-id")
            .expect("session id")
            .to_owned();
        let ack = self
            .post(
                Some(&id),
                extra,
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            )
            .await;
        assert_eq!(ack.status, StatusCode::ACCEPTED);
        assert!(ack.body.is_empty());
        id
    }
}

/// Structural check that `schema` is a valid JSON Schema (draft 2020-12 subset used by the
/// generators): known keywords with the right value types, and every `$ref` resolves.
pub fn check_schema(root: &Value) -> Result<(), String> {
    fn node(root: &Value, value: &Value, at: &str) -> Result<(), String> {
        let Some(object) = value.as_object() else {
            return if value.is_boolean() {
                Ok(())
            } else {
                Err(format!("{at}: a schema must be an object or a boolean"))
            };
        };
        for (key, v) in object {
            let here = format!("{at}/{key}");
            match key.as_str() {
                "$schema" | "$id" | "title" | "description" | "format" | "pattern"
                | "contentEncoding" => {
                    if !v.is_string() {
                        return Err(format!("{here}: must be a string"));
                    }
                }
                "$ref" => {
                    let target = v.as_str().ok_or(format!("{here}: must be a string"))?;
                    resolve(root, target).ok_or(format!("{here}: {target} does not resolve"))?;
                }
                "type" => {
                    let ok = |t: &Value| {
                        matches!(
                            t.as_str(),
                            Some(
                                "object"
                                    | "array"
                                    | "string"
                                    | "integer"
                                    | "number"
                                    | "boolean"
                                    | "null"
                            )
                        )
                    };
                    let valid = match v {
                        Value::Array(items) => !items.is_empty() && items.iter().all(ok),
                        other => ok(other),
                    };
                    if !valid {
                        return Err(format!("{here}: bad type {v}"));
                    }
                }
                "properties" | "$defs" => {
                    let map = v.as_object().ok_or(format!("{here}: must be an object"))?;
                    for (name, child) in map {
                        node(root, child, &format!("{here}/{name}"))?;
                    }
                }
                "items" | "additionalProperties" => node(root, v, &here)?,
                "anyOf" => {
                    let variants = v
                        .as_array()
                        .filter(|variants| !variants.is_empty())
                        .ok_or(format!("{here}: must be a non-empty array"))?;
                    for (index, variant) in variants.iter().enumerate() {
                        node(root, variant, &format!("{here}/{index}"))?;
                    }
                }
                "enum" => {
                    if !v.as_array().is_some_and(|a| !a.is_empty()) {
                        return Err(format!("{here}: must be a non-empty array"));
                    }
                }
                "required" => {
                    if !v.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
                        return Err(format!("{here}: must be an array of strings"));
                    }
                    let props = object.get("properties").and_then(Value::as_object);
                    for name in v.as_array().unwrap() {
                        if !props.is_some_and(|p| p.contains_key(name.as_str().unwrap())) {
                            return Err(format!("{here}: {name} is not a property"));
                        }
                    }
                }
                "minimum" | "maximum" => {
                    if !v.is_number() {
                        return Err(format!("{here}: must be a number"));
                    }
                }
                other => return Err(format!("{here}: unknown keyword {other}")),
            }
        }
        Ok(())
    }
    if root.get("type") != Some(&json!("object")) {
        return Err("root must have type object".into());
    }
    node(root, root, "#")
}

/// Resolves a local JSON pointer reference (`#`, `#/$defs/x`).
pub fn resolve<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    let mut current = root;
    for part in pointer.strip_prefix('/')?.split('/') {
        let part = part.replace("~1", "/").replace("~0", "~");
        current = current.get(part.as_str())?;
    }
    Some(current)
}

/// Validates an instance against a schema (types, properties, items, enum, required, $ref).
pub fn validate(root: &Value, schema: &Value, instance: &Value, at: &str) -> Result<(), String> {
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let target = resolve(root, reference).ok_or(format!("{at}: bad ref {reference}"))?;
        validate(root, target, instance, at)?;
    }
    if let Some(variants) = object.get("anyOf").and_then(Value::as_array)
        && !variants
            .iter()
            .any(|variant| validate(root, variant, instance, at).is_ok())
    {
        return Err(format!("{at}: {instance} does not match any anyOf variant"));
    }
    if let Some(t) = object.get("type") {
        let types: Vec<&str> = match t {
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            other => other.as_str().into_iter().collect(),
        };
        let matches = types.iter().any(|t| match *t {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            "integer" => instance.is_i64() || instance.is_u64(),
            "number" => instance.is_number(),
            "boolean" => instance.is_boolean(),
            "null" => instance.is_null(),
            _ => false,
        });
        if !matches {
            return Err(format!("{at}: {instance} is not {types:?}"));
        }
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array)
        && !values.contains(instance)
    {
        return Err(format!("{at}: {instance} not in enum"));
    }
    if let Some(map) = instance.as_object() {
        let props = object.get("properties").and_then(Value::as_object);
        for (key, value) in map {
            match props.and_then(|p| p.get(key)) {
                Some(child) => validate(root, child, value, &format!("{at}/{key}"))?,
                None => {
                    if let Some(extra) = object.get("additionalProperties") {
                        validate(root, extra, value, &format!("{at}/{key}"))?;
                    } else if props.is_some() {
                        return Err(format!("{at}: unexpected property {key}"));
                    }
                }
            }
        }
        for name in object
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !map.contains_key(name.as_str().unwrap_or_default()) {
                return Err(format!("{at}: missing {name}"));
            }
        }
    }
    if let (Some(items), Some(array)) = (object.get("items"), instance.as_array()) {
        for (i, item) in array.iter().enumerate() {
            validate(root, items, item, &format!("{at}/{i}"))?;
        }
    }
    Ok(())
}
