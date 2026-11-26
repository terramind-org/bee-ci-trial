//! HTTP/JSON gateway in Connect-protocol shape (SPEC §12.1).
//!
//! The gateway serves `POST /bee.v1.<Service>/<Method>` for every tonic service added to it,
//! so `@connectrpc/connect-web` clients, curl and simple agents work without a gRPC stack.
//! It is generic: each request is transcoded to a gRPC request and handed to the same tonic
//! service object that serves gRPC, then the gRPC response is transcoded back.
//!
//! - **Unary:** `Content-Type: application/json` with a proto3-JSON body (or
//!   `application/proto` with a binary body). Success is `200` with the response message.
//! - **Streaming** (server, client and bidi, half-duplex): `Content-Type:
//!   application/connect+json` (or `application/connect+proto`). Each message is a frame of one
//!   flags byte, a 4-byte big-endian length and the payload; the response ends with a frame
//!   flagged `0x02` holding `{"error"?: {...}, "metadata"?: {...}}`.
//! - **Errors:** unary errors use the Connect HTTP status for the code and the JSON body
//!   `{"code", "message", "details": [{"type", "value", "debug"}]}`; the detail of type
//!   `bee.v1.ErrorDetail` is always present (synthesised from the code when a service sent
//!   none), `value` is its base64 protobuf encoding and `debug` its proto3-JSON form.
//! - **Metadata:** request headers are forwarded as gRPC metadata; response headers are
//!   returned as headers; gRPC trailers become `trailer-<name>` headers (unary) or end-stream
//!   `metadata` (streaming). `Connect-Timeout-Ms` becomes `grpc-timeout`.
//! - Only identity encoding is served; a compressed request fails with `unimplemented`.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::Body as AxumBody;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::post;
use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use bytes::{Bytes, BytesMut};
use http::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};
use http_body_util::{BodyExt, Full};
use prost::Message;
use serde::{Deserialize, Serialize};
use tonic::Code;
use tonic::server::NamedService;
use tower::ServiceExt as _;
use tower::util::BoxCloneSyncService;

use crate::envelope::{self, FLAG_COMPRESSED, FLAG_END_STREAM};
use crate::errors::{self, ERROR_DETAIL_TYPE, RpcStatus, TYPE_URL_PREFIX};
use crate::methods::{self, MethodDescriptor};
use crate::v1::{ErrorDetail, ErrorReason};

/// Base64 as Connect writes it: standard alphabet, no padding; padding accepted on input.
pub const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Default limit on a request body and on any single message: 16 MiB.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 << 20;

/// A tonic service as the gateway stores it.
pub type GrpcService =
    BoxCloneSyncService<http::Request<Full<Bytes>>, http::Response<tonic::body::Body>, Infallible>;

/// One detail of a Connect error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnectErrorDetail {
    /// Fully qualified message type, e.g. `bee.v1.ErrorDetail`.
    #[serde(rename = "type")]
    pub type_name: String,
    /// Base64 of the protobuf-encoded message.
    pub value: String,
    /// Optional JSON rendering for humans and JSON-only clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug: Option<serde_json::Value>,
}

/// A Connect error body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnectError {
    /// Connect code name, e.g. `failed_precondition`.
    pub code: String,
    /// Developer-facing message.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Typed details.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<ConnectErrorDetail>,
}

impl ConnectError {
    /// The Connect form of a tonic status. A `bee.v1.ErrorDetail` is always included: the
    /// status's own, or one built from [`errors::default_reason`] when it carries none.
    pub fn from_status(status: &tonic::Status) -> Self {
        let mut details: Vec<ConnectErrorDetail> = errors::rpc_status(status)
            .map(|rpc| rpc.details)
            .unwrap_or_default()
            .into_iter()
            .map(|any| {
                let type_name = errors::any_type_name(&any.type_url).to_owned();
                let debug = (type_name == ERROR_DETAIL_TYPE)
                    .then(|| ErrorDetail::decode(any.value.as_ref()).ok())
                    .flatten()
                    .and_then(|detail| serde_json::to_value(detail).ok());
                ConnectErrorDetail {
                    type_name,
                    value: BASE64.encode(&any.value),
                    debug,
                }
            })
            .collect();
        if !details.iter().any(|d| d.type_name == ERROR_DETAIL_TYPE) {
            let detail =
                errors::error_detail(errors::default_reason(status.code()), status.message());
            details.push(ConnectErrorDetail {
                type_name: ERROR_DETAIL_TYPE.to_owned(),
                value: BASE64.encode(detail.encode_to_vec()),
                debug: serde_json::to_value(&detail).ok(),
            });
        }
        Self {
            code: errors::connect_code_name(status.code()).to_owned(),
            message: status.message().to_owned(),
            details,
        }
    }

    /// A gateway-originated error for `reason`.
    pub fn from_reason(reason: ErrorReason, message: impl Into<String>) -> Self {
        Self::from_status(&errors::status(reason, message))
    }

    /// The tonic status this error describes (client side of the round trip). Details whose
    /// value is not valid base64 are dropped; an unknown code becomes `UNKNOWN`.
    pub fn into_status(self) -> tonic::Status {
        let code = errors::code_from_connect_name(&self.code).unwrap_or(Code::Unknown);
        let details = self
            .details
            .iter()
            .filter_map(|d| {
                let value = BASE64.decode(d.value.as_bytes()).ok()?;
                Some(pbjson_types::Any {
                    type_url: format!("{TYPE_URL_PREFIX}{}", d.type_name),
                    value: value.into(),
                })
            })
            .collect();
        let rpc = RpcStatus {
            code: code as i32,
            message: self.message.clone(),
            details,
        };
        tonic::Status::with_details(code, self.message, Bytes::from(rpc.encode_to_vec()))
    }

    /// The `bee.v1.ErrorDetail` in this error, if present and decodable.
    pub fn error_detail(&self) -> Option<ErrorDetail> {
        let detail = self
            .details
            .iter()
            .find(|d| d.type_name == ERROR_DETAIL_TYPE)?;
        let bytes = BASE64.decode(detail.value.as_bytes()).ok()?;
        ErrorDetail::decode(bytes.as_slice()).ok()
    }

    /// gRPC code of this error.
    pub fn grpc_code(&self) -> Code {
        errors::code_from_connect_name(&self.code).unwrap_or(Code::Unknown)
    }
}

/// The JSON object in a Connect end-of-stream frame.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EndStream {
    /// Set when the stream failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ConnectError>,
    /// Trailing metadata.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Vec<String>>,
}

/// Message codec of a request, from its content type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    Json,
    Proto,
}

/// Request protocol, from its content type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Protocol {
    streaming: bool,
    codec: Codec,
}

impl Protocol {
    fn from_content_type(value: Option<&HeaderValue>) -> Option<Self> {
        let raw = value?.to_str().ok()?;
        let essence = raw.split(';').next()?.trim().to_ascii_lowercase();
        let (streaming, codec) = match essence.as_str() {
            "application/json" => (false, Codec::Json),
            "application/proto" => (false, Codec::Proto),
            "application/connect+json" => (true, Codec::Json),
            "application/connect+proto" => (true, Codec::Proto),
            _ => return None,
        };
        Some(Self { streaming, codec })
    }

    fn response_content_type(self) -> &'static str {
        match (self.streaming, self.codec) {
            (false, Codec::Json) => "application/json",
            (false, Codec::Proto) => "application/proto",
            (true, Codec::Json) => "application/connect+json",
            (true, Codec::Proto) => "application/connect+proto",
        }
    }
}

/// Request headers that describe the HTTP exchange rather than the call; never forwarded.
fn is_transport_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "content-type"
            | "content-length"
            | "content-encoding"
            | "accept"
            | "accept-encoding"
            | "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-connection"
            | "connect-protocol-version"
            | "connect-timeout-ms"
            | "connect-content-encoding"
            | "connect-accept-encoding"
    ) || name.starts_with("grpc-")
}

/// The Connect gateway: a set of tonic services served over HTTP/JSON.
#[derive(Clone, Default)]
pub struct ConnectGateway {
    services: HashMap<String, GrpcService>,
    max_message_bytes: usize,
}

impl ConnectGateway {
    /// An empty gateway with the default size limit.
    pub fn new() -> Self {
        Self {
            services: HashMap::new(),
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }

    /// Sets the limit on a request body and on any single message.
    pub fn max_message_bytes(mut self, limit: usize) -> Self {
        self.max_message_bytes = limit;
        self
    }

    /// Adds a tonic-generated server (e.g. `RefServiceServer::new(impl)`). The service's
    /// `NamedService::NAME` decides which paths it answers.
    pub fn add_service<S>(mut self, service: S) -> Self
    where
        S: tower::Service<
                http::Request<Full<Bytes>>,
                Response = http::Response<tonic::body::Body>,
                Error = Infallible,
            > + NamedService
            + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
    {
        self.services
            .insert(S::NAME.to_owned(), BoxCloneSyncService::new(service));
        self
    }

    /// True if a service with this fully qualified name was added.
    pub fn has_service(&self, name: &str) -> bool {
        self.services.contains_key(name)
    }

    /// An axum router serving `POST /{service}/{method}` for every added service.
    pub fn into_router(self) -> Router {
        Router::new()
            .route("/{service}/{method}", post(handle))
            .with_state(Arc::new(self))
    }

    async fn call(&self, request: Request) -> Response {
        let path = request.uri().path().to_owned();
        let protocol = Protocol::from_content_type(request.headers().get(CONTENT_TYPE));
        let Some(method) = methods::find_method(&path) else {
            return unary_error(
                StatusCode::NOT_FOUND,
                &ConnectError::from_reason(
                    ErrorReason::Unimplemented,
                    format!("unknown procedure {path}"),
                ),
                HeaderMap::new(),
            );
        };
        let Some(service) = self.services.get(method.service) else {
            return unary_error(
                StatusCode::NOT_FOUND,
                &ConnectError::from_reason(
                    ErrorReason::Unimplemented,
                    format!("{} is not served here", method.service),
                ),
                HeaderMap::new(),
            );
        };
        let Some(protocol) = protocol.filter(|p| p.streaming != method.kind.is_unary()) else {
            let expected = if method.kind.is_unary() {
                "application/json, application/proto"
            } else {
                "application/connect+json, application/connect+proto"
            };
            let mut response = unary_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                &ConnectError::from_reason(
                    ErrorReason::InvalidArgument,
                    format!("{path} accepts Content-Type {expected}"),
                ),
                HeaderMap::new(),
            );
            response
                .headers_mut()
                .insert("accept-post", HeaderValue::from_static(expected));
            return response;
        };
        match self.prepare(method, protocol, request).await {
            Ok(grpc_request) => {
                let response = match service.clone().oneshot(grpc_request).await {
                    Ok(response) => response,
                    Err(never) => match never {},
                };
                if protocol.streaming {
                    self.stream_response(method, protocol, response)
                } else {
                    self.unary_response(method, protocol, response).await
                }
            }
            Err(error) if protocol.streaming => {
                stream_with_end(protocol, HeaderMap::new(), Vec::new(), Some(error), None)
            }
            Err(error) => {
                let status = http_status_of(&error);
                unary_error(status, &error, HeaderMap::new())
            }
        }
    }

    /// Validates the Connect request and builds the equivalent gRPC request.
    async fn prepare(
        &self,
        method: &MethodDescriptor,
        protocol: Protocol,
        request: Request,
    ) -> Result<http::Request<Full<Bytes>>, ConnectError> {
        let (parts, body) = request.into_parts();
        if let Some(version) = parts.headers.get("connect-protocol-version")
            && version.as_bytes() != b"1"
        {
            return Err(invalid(
                "bad_protocol_version",
                "Connect-Protocol-Version must be 1",
            ));
        }
        let encoding_header = if protocol.streaming {
            "connect-content-encoding"
        } else {
            "content-encoding"
        };
        if let Some(encoding) = parts.headers.get(encoding_header)
            && encoding.as_bytes() != b"identity"
        {
            return Err(ConnectError::from_reason(
                ErrorReason::Unimplemented,
                "only identity content encoding is supported",
            ));
        }
        let body = axum::body::to_bytes(body, self.max_message_bytes)
            .await
            .map_err(|_| too_large(self.max_message_bytes))?;

        let mut grpc_body = BytesMut::new();
        if protocol.streaming {
            let frames = envelope::decode_all(&body, self.max_message_bytes)
                .map_err(|e| invalid("bad_envelope", e.to_string()))?;
            for frame in frames {
                if frame.flags & FLAG_COMPRESSED != 0 {
                    return Err(ConnectError::from_reason(
                        ErrorReason::Unimplemented,
                        "compressed messages are not supported",
                    ));
                }
                if frame.flags & FLAG_END_STREAM != 0 {
                    return Err(invalid(
                        "bad_envelope",
                        "end-stream frames are not allowed in requests",
                    ));
                }
                let payload = encode_request(method, protocol.codec, &frame.data)?;
                grpc_body.extend_from_slice(&envelope::encode(0, &payload));
            }
        } else {
            let payload = encode_request(method, protocol.codec, &body)?;
            grpc_body.extend_from_slice(&envelope::encode(0, &payload));
        }

        let mut builder = http::Request::builder()
            .method(Method::POST)
            .uri(method.path)
            .header(CONTENT_TYPE, "application/grpc")
            .header("te", "trailers");
        if let Some(timeout) = parts
            .headers
            .get("connect-timeout-ms")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        {
            builder = builder.header("grpc-timeout", grpc_timeout(timeout));
        }
        let mut grpc_request = builder
            .body(Full::new(grpc_body.freeze()))
            .map_err(|e| invalid("bad_request", e.to_string()))?;
        for (name, value) in &parts.headers {
            if !is_transport_header(name.as_str()) {
                grpc_request
                    .headers_mut()
                    .append(name.clone(), value.clone());
            }
        }
        Ok(grpc_request)
    }

    async fn unary_response(
        &self,
        method: &MethodDescriptor,
        protocol: Protocol,
        response: http::Response<tonic::body::Body>,
    ) -> Response {
        let (parts, body) = response.into_parts();
        let mut headers = forwarded_response_headers(&parts.headers);
        if let Some(status) = tonic::Status::from_header_map(&parts.headers) {
            // Trailers-only response: the call failed before any message.
            if status.code() != Code::Ok {
                let error = ConnectError::from_status(&status);
                return unary_error(http_status_of(&error), &error, headers);
            }
        }
        let collected = match body.collect().await {
            Ok(collected) => collected,
            Err(status) => {
                let error = ConnectError::from_status(&status);
                return unary_error(http_status_of(&error), &error, headers);
            }
        };
        let trailers = collected.trailers().cloned().unwrap_or_default();
        let data = collected.to_bytes();
        let status = tonic::Status::from_header_map(&trailers)
            .unwrap_or_else(|| tonic::Status::internal("response ended without grpc-status"));
        for (name, value) in &trailers {
            if !name.as_str().starts_with("grpc-")
                && let Ok(prefixed) = HeaderName::try_from(format!("trailer-{}", name.as_str()))
            {
                headers.append(prefixed, value.clone());
            }
        }
        if status.code() != Code::Ok {
            let error = ConnectError::from_status(&status);
            return unary_error(http_status_of(&error), &error, headers);
        }
        let frames = match envelope::decode_all(&data, usize::MAX) {
            Ok(frames) if frames.len() == 1 => frames,
            _ => {
                let error = ConnectError::from_reason(
                    ErrorReason::Internal,
                    "service returned a malformed unary response",
                );
                return unary_error(http_status_of(&error), &error, headers);
            }
        };
        let payload = match encode_response(method, protocol.codec, &frames[0].data) {
            Ok(payload) => payload,
            Err(error) => return unary_error(http_status_of(&error), &error, headers),
        };
        let mut response = Response::new(AxumBody::from(payload));
        *response.headers_mut() = headers;
        response.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static(protocol.response_content_type()),
        );
        response
    }

    fn stream_response(
        &self,
        method: &MethodDescriptor,
        protocol: Protocol,
        response: http::Response<tonic::body::Body>,
    ) -> Response {
        let (parts, body) = response.into_parts();
        let headers = forwarded_response_headers(&parts.headers);
        if let Some(status) = tonic::Status::from_header_map(&parts.headers) {
            let error = (status.code() != Code::Ok).then(|| ConnectError::from_status(&status));
            return stream_with_end(protocol, headers, Vec::new(), error, Some(&parts.headers));
        }
        let state = StreamState {
            body,
            decoder: envelope::Decoder::new(self.max_message_bytes),
            method: *method,
            codec: protocol.codec,
            done: false,
        };
        let stream = futures_util::stream::unfold(state, |mut state| async move {
            if state.done {
                return None;
            }
            let chunk = state.next_chunk().await;
            Some((Ok::<Bytes, Infallible>(chunk), state))
        });
        let mut response = Response::new(AxumBody::from_stream(stream));
        *response.headers_mut() = headers;
        response.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static(protocol.response_content_type()),
        );
        response
    }
}

/// State of a streaming response being transcoded frame by frame.
struct StreamState {
    body: tonic::body::Body,
    decoder: envelope::Decoder,
    method: MethodDescriptor,
    codec: Codec,
    done: bool,
}

impl StreamState {
    /// The next bytes to send: one message frame, or the end-stream frame (then `done`).
    async fn next_chunk(&mut self) -> Bytes {
        loop {
            match self.decoder.next_frame() {
                Ok(Some(frame)) => {
                    return match encode_response(&self.method, self.codec, &frame.data) {
                        Ok(payload) => envelope::encode(0, &payload),
                        Err(error) => self.end(Some(error), None),
                    };
                }
                Ok(None) => {}
                Err(e) => {
                    let error = ConnectError::from_reason(ErrorReason::TooLarge, e.to_string());
                    return self.end(Some(error), None);
                }
            }
            match self.body.frame().await {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => self.decoder.push(&data),
                    Err(frame) => {
                        let trailers = frame.into_trailers().unwrap_or_default();
                        let error = match tonic::Status::from_header_map(&trailers) {
                            Some(status) if status.code() == Code::Ok => None,
                            Some(status) => Some(ConnectError::from_status(&status)),
                            None => Some(ConnectError::from_reason(
                                ErrorReason::Internal,
                                "stream ended without grpc-status",
                            )),
                        };
                        return self.end(error, Some(&trailers));
                    }
                },
                Some(Err(status)) => {
                    return self.end(Some(ConnectError::from_status(&status)), None);
                }
                None => {
                    let error = ConnectError::from_reason(
                        ErrorReason::Internal,
                        "stream ended without trailers",
                    );
                    return self.end(Some(error), None);
                }
            }
        }
    }

    fn end(&mut self, error: Option<ConnectError>, trailers: Option<&HeaderMap>) -> Bytes {
        self.done = true;
        end_stream_frame(error, trailers)
    }
}

async fn handle(State(gateway): State<Arc<ConnectGateway>>, request: Request) -> Response {
    gateway.call(request).await
}

fn encode_request(
    method: &MethodDescriptor,
    codec: Codec,
    payload: &[u8],
) -> Result<Vec<u8>, ConnectError> {
    match codec {
        Codec::Json => (method.request_json_to_proto)(payload)
            .map_err(|e| invalid("bad_json", format!("{}: {e}", method.input_type))),
        Codec::Proto => {
            // Validate before forwarding so malformed bytes fail with a clear reason.
            (method.request_proto_to_json)(payload)
                .map_err(|e| invalid("bad_proto", format!("{}: {e}", method.input_type)))?;
            Ok(payload.to_vec())
        }
    }
}

fn encode_response(
    method: &MethodDescriptor,
    codec: Codec,
    payload: &[u8],
) -> Result<Vec<u8>, ConnectError> {
    match codec {
        Codec::Json => (method.response_proto_to_json)(payload).map_err(|e| {
            ConnectError::from_reason(
                ErrorReason::Internal,
                format!("{}: {e}", method.output_type),
            )
        }),
        Codec::Proto => Ok(payload.to_vec()),
    }
}

fn invalid(subcode: &str, message: impl Into<String>) -> ConnectError {
    let mut detail = errors::error_detail(ErrorReason::InvalidArgument, message);
    detail.subcode = subcode.to_owned();
    ConnectError::from_status(&errors::status_from_detail(&detail))
}

fn too_large(limit: usize) -> ConnectError {
    let mut detail = errors::error_detail(
        ErrorReason::TooLarge,
        format!("request exceeds {limit} bytes"),
    );
    detail.payload = Some(crate::v1::error_detail::Payload::LimitExceeded(
        crate::v1::LimitExceeded {
            limit: "max_message_bytes".to_owned(),
            maximum: limit as u64,
            actual: 0,
        },
    ));
    ConnectError::from_status(&errors::status_from_detail(&detail))
}

/// gRPC `grpc-timeout` value for a Connect timeout in milliseconds (at most 8 digits).
fn grpc_timeout(millis: u64) -> String {
    if millis <= 99_999_999 {
        format!("{millis}m")
    } else {
        format!("{}S", (millis / 1000).min(99_999_999))
    }
}

fn http_status_of(error: &ConnectError) -> StatusCode {
    StatusCode::from_u16(errors::http_status(error.grpc_code()))
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// gRPC response headers that are call metadata, not transport.
fn forwarded_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        if !(n.starts_with("grpc-") || matches!(n, "content-type" | "content-length" | "trailer")) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

fn metadata_map(headers: Option<&HeaderMap>) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers.into_iter().flatten() {
        if name.as_str().starts_with("grpc-") || name.as_str() == "content-type" {
            continue;
        }
        if let Ok(value) = value.to_str() {
            map.entry(name.as_str().to_owned())
                .or_default()
                .push(value.to_owned());
        }
    }
    map
}

fn end_stream_frame(error: Option<ConnectError>, trailers: Option<&HeaderMap>) -> Bytes {
    let end = EndStream {
        error,
        metadata: metadata_map(trailers),
    };
    let json = serde_json::to_vec(&end).unwrap_or_else(|_| b"{}".to_vec());
    envelope::encode(FLAG_END_STREAM, &json)
}

fn stream_with_end(
    protocol: Protocol,
    headers: HeaderMap,
    frames: Vec<Bytes>,
    error: Option<ConnectError>,
    trailers: Option<&HeaderMap>,
) -> Response {
    let mut body = BytesMut::new();
    for frame in frames {
        body.extend_from_slice(&frame);
    }
    body.extend_from_slice(&end_stream_frame(error, trailers));
    let mut response = Response::new(AxumBody::from(body.freeze()));
    *response.headers_mut() = headers;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(protocol.response_content_type()),
    );
    response
}

fn unary_error(status: StatusCode, error: &ConnectError, headers: HeaderMap) -> Response {
    let body = serde_json::to_vec(error).unwrap_or_else(|_| b"{\"code\":\"internal\"}".to_vec());
    let mut response = Response::new(AxumBody::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types() {
        let parse =
            |s: &'static str| Protocol::from_content_type(Some(&HeaderValue::from_static(s)));
        assert_eq!(
            parse("application/json; charset=utf-8"),
            Some(Protocol {
                streaming: false,
                codec: Codec::Json
            })
        );
        assert_eq!(
            parse("Application/Connect+Proto"),
            Some(Protocol {
                streaming: true,
                codec: Codec::Proto
            })
        );
        assert_eq!(parse("text/plain"), None);
        assert_eq!(Protocol::from_content_type(None), None);
    }

    #[test]
    fn timeouts() {
        assert_eq!(grpc_timeout(1500), "1500m");
        assert_eq!(grpc_timeout(99_999_999), "99999999m");
        assert_eq!(grpc_timeout(100_000_000), "100000S");
    }

    #[test]
    fn synthesised_detail_for_bare_status() {
        let error = ConnectError::from_status(&tonic::Status::unimplemented("nope"));
        assert_eq!(error.code, "unimplemented");
        let detail = error.error_detail().expect("detail");
        assert_eq!(detail.reason, "UNIMPLEMENTED");
        assert!(!detail.retryable);
    }
}
