//! A Connect-protocol client (SPEC §12.1) over TCP/TLS or a unix socket.
//!
//! The daemon uses it to reach the tier above (binary protobuf: `application/proto` and
//! `application/connect+proto`), working copies and the CLI use it to reach the daemon or the
//! server (proto3-JSON, the shape of `bee_wc::Transport`), and the daemon fetches signed
//! CDN URLs (SPEC §10.5) with it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use bee_proto::connect::{ConnectError, EndStream};
use bee_proto::envelope::{self, FLAG_END_STREAM};
use bee_proto::errors;
use bee_proto::v1::{ErrorDetail, ErrorReason};
use bytes::{Bytes, BytesMut};
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use serde_json::Value;

use crate::error::{Result, err, unavailable};

/// Largest message accepted from a peer.
pub const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Where a client connects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// `http://host:port` or `https://host:port` (no trailing slash).
    Http(String),
    /// A unix socket path (the node daemon).
    Unix(PathBuf),
}

impl Endpoint {
    /// Parses `unix:<path>`, a bare absolute path (a socket) or an `http(s)://` URL.
    pub fn parse(text: &str) -> Result<Self> {
        if let Some(path) = text.strip_prefix("unix:") {
            return Ok(Endpoint::Unix(PathBuf::from(path)));
        }
        if text.starts_with('/') {
            return Ok(Endpoint::Unix(PathBuf::from(text)));
        }
        if text.starts_with("http://") || text.starts_with("https://") {
            return Ok(Endpoint::Http(text.trim_end_matches('/').to_owned()));
        }
        Err(crate::error::invalid(
            "bad_endpoint",
            format!("endpoint {text:?} is not unix:<path>, http:// or https://"),
        ))
    }
}

type Pooled = Client<HttpsConnector<HttpConnector>, Body>;

/// Response trailers that matter to caches, from either transport form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trailers {
    /// Name to values.
    pub values: BTreeMap<String, Vec<String>>,
}

impl Trailers {
    /// First value of `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|v| v.first())
            .map(String::as_str)
    }

    /// Call metadata from response headers: `trailer-<name>` (unary trailers) and `bee-*`
    /// response metadata.
    fn from_headers(headers: &HeaderMap) -> Self {
        let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in headers {
            let name = k.as_str();
            let name = name.strip_prefix("trailer-").unwrap_or(name);
            if let (true, Ok(v)) = (name.starts_with("bee-"), v.to_str()) {
                values
                    .entry(name.to_owned())
                    .or_default()
                    .push(v.to_owned());
            }
        }
        Self { values }
    }
}

/// A Connect client. Cheap to clone; TCP clones share a connection pool.
#[derive(Clone)]
pub struct ConnectClient {
    endpoint: Endpoint,
    pooled: Pooled,
    timeout: Duration,
}

impl std::fmt::Debug for ConnectClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectClient")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

/// The `bee.v1.ErrorDetail` of a failed Connect exchange.
pub fn error_from_body(status: StatusCode, body: &[u8]) -> Box<ErrorDetail> {
    if let Ok(ce) = serde_json::from_slice::<ConnectError>(body) {
        if let Some(d) = ce.error_detail() {
            return Box::new(d);
        }
        let reason = errors::default_reason(ce.grpc_code());
        return err(reason, ce.message);
    }
    let reason = match status.as_u16() {
        400 => ErrorReason::InvalidArgument,
        401 => ErrorReason::Unauthenticated,
        403 => ErrorReason::PermissionDenied,
        404 => ErrorReason::NotFound,
        409 => ErrorReason::AlreadyExists,
        429 => ErrorReason::ResourceExhausted,
        501 => ErrorReason::Unimplemented,
        502..=504 => ErrorReason::Unavailable,
        _ => ErrorReason::Internal,
    };
    err(
        reason,
        format!(
            "HTTP {status}: {}",
            String::from_utf8_lossy(&body[..body.len().min(300)])
        ),
    )
}

impl ConnectClient {
    /// A client for `endpoint` with a per-request timeout.
    pub fn new(endpoint: Endpoint, timeout: Duration) -> Self {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        let pooled = Client::builder(TokioExecutor::new()).build(connector);
        Self {
            endpoint,
            pooled,
            timeout,
        }
    }

    /// The endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn uri_for(&self, path_and_query: &str) -> String {
        match &self.endpoint {
            Endpoint::Http(base) => format!("{base}{path_and_query}"),
            Endpoint::Unix(_) => path_and_query.to_owned(),
        }
    }

    /// Sends a request whose URI is a path (and query) on this endpoint; streams the response.
    pub async fn send(&self, mut req: Request<Body>) -> Result<Response<Incoming>> {
        let pq = req
            .uri()
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_owned();
        let uri = self
            .uri_for(&pq)
            .parse::<http::Uri>()
            .map_err(|e| crate::error::invalid("bad_uri", e.to_string()))?;
        *req.uri_mut() = uri;
        let fut = async {
            match &self.endpoint {
                Endpoint::Http(_) => self
                    .pooled
                    .request(req)
                    .await
                    .map_err(|e| unavailable(format!("upstream request failed: {e}"))),
                Endpoint::Unix(path) => {
                    let stream = tokio::net::UnixStream::connect(path).await.map_err(|e| {
                        unavailable(format!("cannot connect to {}: {e}", path.display()))
                    })?;
                    let (mut sender, conn) =
                        hyper::client::conn::http1::handshake(TokioIo::new(stream))
                            .await
                            .map_err(|e| unavailable(format!("daemon handshake failed: {e}")))?;
                    tokio::spawn(async move {
                        let _ = conn.await;
                    });
                    if !req.headers().contains_key(HOST) {
                        req.headers_mut()
                            .insert(HOST, http::HeaderValue::from_static("localhost"));
                    }
                    sender
                        .send_request(req)
                        .await
                        .map_err(|e| unavailable(format!("daemon request failed: {e}")))
                }
            }
        };
        tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| err(ErrorReason::DeadlineExceeded, "request timed out"))?
    }

    async fn exchange(
        &self,
        path: &str,
        content_type: &'static str,
        auth: Option<&str>,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap, Bytes)> {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(CONTENT_TYPE, content_type)
            .header("connect-protocol-version", "1");
        if let Some(a) = auth {
            req = req.header(AUTHORIZATION, a);
        }
        let req = req
            .body(Body::from(body))
            .map_err(|e| crate::error::invalid("bad_request", e.to_string()))?;
        let resp = self.send(req).await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = tokio::time::timeout(self.timeout, resp.into_body().collect())
            .await
            .map_err(|_| err(ErrorReason::DeadlineExceeded, "response timed out"))?
            .map_err(|e| unavailable(format!("reading the response failed: {e}")))?
            .to_bytes();
        if bytes.len() > MAX_MESSAGE_BYTES * 4 {
            return Err(err(ErrorReason::TooLarge, "response too large"));
        }
        Ok((status, headers, bytes))
    }

    /// Unary call with a raw body in `content_type` (`application/json` or `application/proto`).
    pub async fn unary_raw(
        &self,
        path: &str,
        content_type: &'static str,
        auth: Option<&str>,
        body: Bytes,
    ) -> Result<(Bytes, Trailers)> {
        let (status, headers, bytes) = self.exchange(path, content_type, auth, body).await?;
        if status != StatusCode::OK {
            return Err(error_from_body(status, &bytes));
        }
        Ok((bytes, Trailers::from_headers(&headers)))
    }

    /// Streaming call: sends `messages` (already encoded in `content_type`'s codec) as frames,
    /// returns every response message and the end-stream metadata.
    pub async fn stream_raw(
        &self,
        path: &str,
        content_type: &'static str,
        auth: Option<&str>,
        messages: &[Bytes],
    ) -> Result<(Vec<Bytes>, Trailers)> {
        let mut body = BytesMut::new();
        for m in messages {
            body.extend_from_slice(&envelope::encode(0, m));
        }
        let (status, headers, bytes) = self
            .exchange(path, content_type, auth, body.freeze())
            .await?;
        if status != StatusCode::OK {
            return Err(error_from_body(status, &bytes));
        }
        let frames = envelope::decode_all(&bytes, MAX_MESSAGE_BYTES)
            .map_err(|e| err(ErrorReason::Internal, format!("bad stream framing: {e}")))?;
        let mut out = Vec::new();
        let mut trailers = Trailers::from_headers(&headers);
        for frame in frames {
            if frame.flags & FLAG_END_STREAM != 0 {
                let end: EndStream = if frame.data.iter().all(u8::is_ascii_whitespace) {
                    EndStream::default()
                } else {
                    serde_json::from_slice(&frame.data).map_err(|e| {
                        err(
                            ErrorReason::Internal,
                            format!("bad end-of-stream frame: {e}"),
                        )
                    })?
                };
                trailers.values.extend(end.metadata);
                if let Some(e) = end.error {
                    return Err(Box::new(e.error_detail().unwrap_or_else(|| {
                        errors::error_detail(errors::default_reason(e.grpc_code()), e.message)
                    })));
                }
            } else {
                out.push(frame.data);
            }
        }
        Ok((out, trailers))
    }

    /// Unary call in binary protobuf.
    pub async fn unary<Req: Message, Resp: Message + Default>(
        &self,
        path: &str,
        auth: Option<&str>,
        req: &Req,
    ) -> Result<(Resp, Trailers)> {
        let (bytes, trailers) = self
            .unary_raw(
                path,
                "application/proto",
                auth,
                Bytes::from(req.encode_to_vec()),
            )
            .await?;
        let resp = Resp::decode(bytes).map_err(|e| {
            err(
                ErrorReason::Internal,
                format!("bad response from {path}: {e}"),
            )
        })?;
        Ok((resp, trailers))
    }

    /// Server-streaming call in binary protobuf.
    pub async fn server_stream<Req: Message, Resp: Message + Default>(
        &self,
        path: &str,
        auth: Option<&str>,
        req: &Req,
    ) -> Result<(Vec<Resp>, Trailers)> {
        let (frames, trailers) = self
            .stream_raw(
                path,
                "application/connect+proto",
                auth,
                &[Bytes::from(req.encode_to_vec())],
            )
            .await?;
        let mut out = Vec::with_capacity(frames.len());
        for f in frames {
            out.push(Resp::decode(f).map_err(|e| {
                err(
                    ErrorReason::Internal,
                    format!("bad message from {path}: {e}"),
                )
            })?);
        }
        Ok((out, trailers))
    }

    /// Unary call in proto3-JSON.
    pub async fn unary_json(&self, path: &str, auth: Option<&str>, req: &Value) -> Result<Value> {
        let body =
            serde_json::to_vec(req).map_err(|e| err(ErrorReason::Internal, e.to_string()))?;
        let (bytes, _) = self
            .unary_raw(path, "application/json", auth, Bytes::from(body))
            .await?;
        parse_json(&bytes)
    }

    /// Streaming call in proto3-JSON: every request message in, every response message out.
    pub async fn stream_json(
        &self,
        path: &str,
        auth: Option<&str>,
        reqs: &[Value],
    ) -> Result<Vec<Value>> {
        let mut msgs = Vec::with_capacity(reqs.len());
        for r in reqs {
            msgs.push(Bytes::from(
                serde_json::to_vec(r).map_err(|e| err(ErrorReason::Internal, e.to_string()))?,
            ));
        }
        let (frames, _) = self
            .stream_raw(path, "application/connect+json", auth, &msgs)
            .await?;
        frames.iter().map(|f| parse_json(f)).collect()
    }

    /// `GET` of an absolute URL (a signed CDN URL); always over TCP/TLS.
    pub async fn get_url(&self, url: &str) -> std::result::Result<Bytes, String> {
        let req = Request::builder()
            .method(Method::GET)
            .uri(url)
            .body(Body::empty())
            .map_err(|e| e.to_string())?;
        let fut = async {
            let resp = self.pooled.request(req).await.map_err(|e| e.to_string())?;
            if resp.status() != StatusCode::OK {
                return Err(format!("HTTP {}", resp.status()));
            }
            resp.into_body()
                .collect()
                .await
                .map(|b| b.to_bytes())
                .map_err(|e| e.to_string())
        };
        tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| "timed out".to_owned())?
    }
}

/// Parses a JSON message body; an empty body is `{}`.
pub fn parse_json(bytes: &[u8]) -> Result<Value> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_slice(bytes)
        .map_err(|e| err(ErrorReason::Internal, format!("response is not JSON: {e}")))
}

/// Server-streaming calls [`BlockingClient::server_streams`] keeps in flight at once.
pub const CONCURRENT_STREAMS: usize = 8;

/// A blocking client for code outside an async runtime (the CLI, working copies, tests). It
/// owns a current-thread runtime, so it must not be used from inside another runtime's task.
pub struct BlockingClient {
    client: ConnectClient,
    runtime: tokio::runtime::Runtime,
    authorization: Option<String>,
}

impl BlockingClient {
    /// A client for `endpoint`, sending `token` as a bearer credential.
    pub fn new(endpoint: Endpoint, token: Option<&str>) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                err(
                    ErrorReason::Internal,
                    format!("cannot start a runtime: {e}"),
                )
            })?;
        let client = {
            let _guard = runtime.enter();
            ConnectClient::new(endpoint, Duration::from_secs(300))
        };
        Ok(Self {
            client,
            runtime,
            authorization: token.map(|t| format!("Bearer {t}")),
        })
    }

    /// The endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        self.client.endpoint()
    }

    /// A unary proto3-JSON call (`/bee.v1.<Service>/<Method>`), the shape of
    /// `bee_wc::Transport::unary`.
    pub fn unary(&self, path: &str, request: &Value) -> Result<Value> {
        self.runtime.block_on(
            self.client
                .unary_json(path, self.authorization.as_deref(), request),
        )
    }

    /// A server-streaming proto3-JSON call, the shape of `bee_wc::Transport::server_stream`.
    pub fn server_stream(&self, path: &str, request: &Value) -> Result<Vec<Value>> {
        self.runtime.block_on(self.client.stream_json(
            path,
            self.authorization.as_deref(),
            std::slice::from_ref(request),
        ))
    }

    /// One server-streaming call per request, up to [`CONCURRENT_STREAMS`] at once; each
    /// call's messages in request order (the shape of `bee_wc::Transport::server_streams`).
    pub fn server_streams(&self, path: &str, requests: &[Value]) -> Vec<Result<Vec<Value>>> {
        use futures_util::StreamExt as _;
        let auth = self.authorization.as_deref();
        self.runtime.block_on(
            futures_util::stream::iter(
                requests
                    .iter()
                    .map(|r| self.client.stream_json(path, auth, std::slice::from_ref(r))),
            )
            .buffered(CONCURRENT_STREAMS)
            .collect(),
        )
    }

    /// A binary-protobuf unary call.
    pub fn unary_proto<Req: Message, Resp: Message + Default>(
        &self,
        path: &str,
        request: &Req,
    ) -> Result<Resp> {
        self.runtime
            .block_on(
                self.client
                    .unary::<Req, Resp>(path, self.authorization.as_deref(), request),
            )
            .map(|(resp, _)| resp)
    }

    /// One binary-protobuf server-streaming call per request, up to [`CONCURRENT_STREAMS`]
    /// at once; each call's messages in request order.
    pub fn server_streams_proto<Req: Message, Resp: Message + Default>(
        &self,
        path: &str,
        requests: &[Req],
    ) -> Vec<Result<Vec<Resp>>> {
        use futures_util::StreamExt as _;
        let auth = self.authorization.as_deref();
        self.runtime.block_on(
            futures_util::stream::iter(requests.iter().map(|r| async move {
                self.client
                    .server_stream::<Req, Resp>(path, auth, r)
                    .await
                    .map(|(msgs, _)| msgs)
            }))
            .buffered(CONCURRENT_STREAMS)
            .collect(),
        )
    }

    /// A client-streaming proto3-JSON call, the shape of `bee_wc::Transport::client_stream`.
    pub fn client_stream(&self, path: &str, requests: &[Value]) -> Result<Value> {
        let mut out = self.runtime.block_on(self.client.stream_json(
            path,
            self.authorization.as_deref(),
            requests,
        ))?;
        Ok(out.pop().unwrap_or(Value::Object(serde_json::Map::new())))
    }

    /// Calls a daemon-local operation (`/daemon/v1/<op>`, JSON in and out).
    pub fn local(&self, op: &str, request: &Value) -> Result<Value> {
        let path = format!("/daemon/v1/{op}");
        self.runtime.block_on(async {
            let body = serde_json::to_vec(request)
                .map_err(|e| err(ErrorReason::Internal, e.to_string()))?;
            let mut req = Request::builder()
                .method(Method::POST)
                .uri(path.as_str())
                .header(CONTENT_TYPE, "application/json");
            if let Some(a) = &self.authorization {
                req = req.header(AUTHORIZATION, a.as_str());
            }
            let req = req
                .body(Body::from(body))
                .map_err(|e| crate::error::invalid("bad_request", e.to_string()))?;
            let resp = self.client.send(req).await?;
            let status = resp.status();
            let bytes = resp
                .into_body()
                .collect()
                .await
                .map_err(|e| unavailable(format!("reading the response failed: {e}")))?
                .to_bytes();
            if status != StatusCode::OK {
                return Err(local_error(status, &bytes));
            }
            parse_json(&bytes)
        })
    }
}

/// The error of a daemon-local call: `{"error": <ErrorDetail as proto3-JSON>}`.
pub fn local_error(status: StatusCode, body: &[u8]) -> Box<ErrorDetail> {
    if let Ok(v) = serde_json::from_slice::<Value>(body)
        && let Some(e) = v.get("error")
        && let Ok(d) = serde_json::from_value::<ErrorDetail>(e.clone())
    {
        return Box::new(d);
    }
    error_from_body(status, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_parse() {
        assert_eq!(
            Endpoint::parse("unix:/tmp/d.sock").expect("unix"),
            Endpoint::Unix("/tmp/d.sock".into())
        );
        assert_eq!(
            Endpoint::parse("/run/bee.sock").expect("path"),
            Endpoint::Unix("/run/bee.sock".into())
        );
        assert_eq!(
            Endpoint::parse("http://127.0.0.1:9/").expect("http"),
            Endpoint::Http("http://127.0.0.1:9".into())
        );
        assert!(Endpoint::parse("ftp://x").is_err());
    }

    #[test]
    fn connect_errors_keep_their_detail() {
        let ce = ConnectError::from_reason(ErrorReason::Redacted, "gone");
        let body = serde_json::to_vec(&ce).expect("json");
        let d = error_from_body(StatusCode::NOT_FOUND, &body);
        assert_eq!(d.reason, "REDACTED");
        let d = error_from_body(StatusCode::BAD_GATEWAY, b"<html>");
        assert_eq!(d.reason, "UNAVAILABLE");
        assert!(d.retryable);
    }
}
