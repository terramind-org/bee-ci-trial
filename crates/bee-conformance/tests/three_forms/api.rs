//! The API form: the HTTP/JSON (Connect) surface over a raw HTTP client, and the gRPC surface
//! over a raw-bytes tonic client, both independent of the CLI's and MCP's own clients.

use std::time::Duration;

use base64::Engine as _;
use bee_conformance::parity::normalize::{ErrorFacts, Outcome};
use bee_proto::methods::{MethodDescriptor, MethodKind};
use bytes::{Buf, BufMut, Bytes};
use http_body_util::{BodyExt, Full};
use serde_json::Value;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// What a Connect call returned, with the contract problems seen on the way.
pub struct ApiResult {
    /// The outcome.
    pub outcome: Outcome,
    /// SPEC §12.1/§12.2 violations (wrong status for the reason, missing detail, ...).
    pub problems: Vec<String>,
}

pub type Client = hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    Full<Bytes>,
>;

/// One shared HTTP client.
pub fn client() -> Client {
    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<Full<Bytes>>()
}

/// How long event streams wait for the next message before the call counts as caught up
/// (the CLI's catch-up idle time).
pub const STREAM_IDLE: Duration = Duration::from_secs(2);

/// True for server streams that stay open for new events.
pub fn is_event_stream(m: &MethodDescriptor) -> bool {
    m.kind == MethodKind::ServerStreaming
        && ["Subscribe", "Stream", "Watch", "Tail", "Acquire"]
            .iter()
            .any(|p| m.method.starts_with(p))
}

fn frame(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Calls `m` with `request` (an array of messages for client streams) on the Connect
/// surface of `url`.
pub async fn call(
    http: &Client,
    url: &str,
    token: &str,
    m: &MethodDescriptor,
    request: &Value,
    max_items: Option<u64>,
) -> ApiResult {
    let target = format!("{url}{}", m.path);
    let unary = m.kind == MethodKind::Unary;
    let body = if unary {
        serde_json::to_vec(request).unwrap_or_default()
    } else {
        let messages = match request {
            Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        let mut out = Vec::new();
        for msg in &messages {
            out.extend(frame(0, &serde_json::to_vec(msg).unwrap_or_default()));
        }
        out
    };
    let req = http::Request::builder()
        .method("POST")
        .uri(&target)
        .header(
            "content-type",
            if unary {
                "application/json"
            } else {
                "application/connect+json"
            },
        )
        .header("connect-protocol-version", "1")
        .header("authorization", format!("Bearer {token}"))
        .body(Full::new(Bytes::from(body)))
        .expect("request");
    let resp = match tokio::time::timeout(Duration::from_secs(120), http.request(req)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return transport_failure(format!("HTTP request failed: {e}")),
        Err(_) => return transport_failure("HTTP request timed out".into()),
    };
    let status = resp.status().as_u16();
    if unary {
        let bytes = match resp.into_body().collect().await {
            Ok(b) => b.to_bytes(),
            Err(e) => return transport_failure(format!("reading the body: {e}")),
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                return transport_failure(format!(
                    "HTTP {status}: body is not JSON ({e}): {}",
                    String::from_utf8_lossy(&bytes)
                ));
            }
        };
        if status == 200 {
            return ApiResult {
                outcome: Outcome::Ok(vec![value]),
                problems: Vec::new(),
            };
        }
        let mut problems = Vec::new();
        let facts = connect_error(&value, Some(status), &mut problems);
        return ApiResult {
            outcome: Outcome::Err(facts),
            problems,
        };
    }
    // Streams: frames until the end-stream frame, `max_items`, or (event streams) idle.
    let mut problems = Vec::new();
    if status != 200 {
        // Only a request the gateway cannot route (or decode) is answered before the stream,
        // as a plain Connect error; everything else ends the stream with the error.
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let facts = connect_error(&value, Some(status), &mut problems);
        if facts.reason != "UNIMPLEMENTED" {
            problems.push(format!(
                "stream answered HTTP {status} for {}, want 200",
                facts.reason
            ));
        }
        return ApiResult {
            outcome: Outcome::Err(facts),
            problems,
        };
    }
    let idle = if is_event_stream(m) {
        STREAM_IDLE
    } else {
        Duration::from_secs(120)
    };
    let mut body = resp.into_body();
    let mut buf: Vec<u8> = Vec::new();
    let mut items = Vec::new();
    let mut end: Option<Value> = None;
    'read: loop {
        while buf.len() >= 5 {
            let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
            if buf.len() < 5 + len {
                break;
            }
            let flags = buf[0];
            let payload = buf[5..5 + len].to_vec();
            buf.drain(..5 + len);
            let value: Value = serde_json::from_slice(&payload).unwrap_or_else(|e| {
                problems.push(format!("stream frame is not JSON: {e}"));
                Value::Null
            });
            if flags & 0x02 != 0 {
                end = Some(value);
                break 'read;
            }
            items.push(value);
            if max_items.is_some_and(|n| items.len() as u64 >= n) {
                break 'read;
            }
        }
        match tokio::time::timeout(idle, body.frame()).await {
            Ok(Some(Ok(f))) => {
                if let Ok(data) = f.into_data() {
                    buf.extend_from_slice(&data);
                }
            }
            Ok(Some(Err(e))) => {
                problems.push(format!("stream broke: {e}"));
                break;
            }
            Ok(None) => {
                if !is_event_stream(m) {
                    problems.push("stream ended without an end-stream frame".into());
                }
                break;
            }
            Err(_) => break,
        }
    }
    let outcome = match end.as_ref().and_then(|e| e.get("error")) {
        Some(err) => Outcome::Err(connect_error(err, None, &mut problems)),
        None => Outcome::Ok(items),
    };
    ApiResult { outcome, problems }
}

fn transport_failure(message: String) -> ApiResult {
    ApiResult {
        outcome: Outcome::Err(ErrorFacts {
            reason: "TRANSPORT".into(),
            retryable: false,
            subcode: String::new(),
            message: message.clone(),
        }),
        problems: vec![message],
    }
}

/// Reads a Connect error body (`{"code", "message", "details": [...]}`) and checks it against
/// the SPEC §12.2 table.
fn connect_error(
    value: &Value,
    http_status: Option<u16>,
    problems: &mut Vec<String>,
) -> ErrorFacts {
    let code = value["code"].as_str().unwrap_or_default().to_owned();
    let detail = value["details"].as_array().and_then(|ds| {
        ds.iter()
            .find(|d| d["type"] == "bee.v1.ErrorDetail")
            .cloned()
    });
    let Some(detail) = detail else {
        problems.push(format!("error without a bee.v1.ErrorDetail: {value}"));
        return ErrorFacts {
            reason: format!("<no detail: {code}>"),
            retryable: false,
            subcode: String::new(),
            message: value["message"].as_str().unwrap_or_default().to_owned(),
        };
    };
    let debug = &detail["debug"];
    // The binary `value` must decode to the same detail as `debug`.
    if let Some(b64) = detail["value"].as_str() {
        let raw = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(b64.trim_end_matches('='))
            .unwrap_or_default();
        match <bee_proto::v1::ErrorDetail as prost::Message>::decode(raw.as_slice()) {
            Ok(d) if d.reason == debug["reason"].as_str().unwrap_or_default() => {}
            Ok(d) => problems.push(format!(
                "detail value says {} but debug says {}",
                d.reason, debug["reason"]
            )),
            Err(e) => problems.push(format!("detail value does not decode: {e}")),
        }
    } else {
        problems.push("ErrorDetail without a binary value".into());
    }
    let reason = debug["reason"].as_str().unwrap_or_default().to_owned();
    match bee_proto::errors::REASONS
        .iter()
        .find(|r| r.name() == reason)
    {
        Some(info) => {
            if info.connect_code() != code {
                problems.push(format!(
                    "{reason} answered Connect code {code}, SPEC says {}",
                    info.connect_code()
                ));
            }
            // SPEC §12.2: UNIMPLEMENTED is 501, except that paths the server does not route
            // (a service that is not registered) answer 404.
            let not_routed = reason == "UNIMPLEMENTED" && http_status == Some(404);
            if let Some(s) = http_status
                && s != info.http_status()
                && !not_routed
            {
                problems.push(format!(
                    "{reason} answered HTTP {s}, SPEC says {}",
                    info.http_status()
                ));
            }
        }
        None => problems.push(format!("unknown reason {reason:?}")),
    }
    ErrorFacts {
        reason,
        retryable: debug["retryable"].as_bool().unwrap_or(false),
        subcode: debug["subcode"].as_str().unwrap_or_default().to_owned(),
        message: debug["message"].as_str().unwrap_or_default().to_owned(),
    }
}

/// A tonic codec that passes protobuf bytes through untouched.
#[derive(Clone, Copy, Default)]
struct RawCodec;

struct RawEncoder;
struct RawDecoder;

impl Codec for RawCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = RawEncoder;
    type Decoder = RawDecoder;
    fn encoder(&mut self) -> RawEncoder {
        RawEncoder
    }
    fn decoder(&mut self) -> RawDecoder {
        RawDecoder
    }
}

impl Encoder for RawEncoder {
    type Item = Vec<u8>;
    type Error = tonic::Status;
    fn encode(&mut self, item: Vec<u8>, dst: &mut EncodeBuf<'_>) -> Result<(), tonic::Status> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for RawDecoder {
    type Item = Vec<u8>;
    type Error = tonic::Status;
    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Vec<u8>>, tonic::Status> {
        let mut out = vec![0u8; src.remaining()];
        src.copy_to_slice(&mut out);
        Ok(Some(out))
    }
}

/// Calls a unary or server-streaming `m` over gRPC; `None` for other shapes.
pub async fn call_grpc(
    url: &str,
    token: &str,
    m: &MethodDescriptor,
    request: &Value,
    max_items: Option<u64>,
) -> Option<Result<Outcome, String>> {
    if !matches!(m.kind, MethodKind::Unary | MethodKind::ServerStreaming) {
        return None;
    }
    Some(grpc_inner(url, token, m, request, max_items).await)
}

async fn grpc_inner(
    url: &str,
    token: &str,
    m: &MethodDescriptor,
    request: &Value,
    max_items: Option<u64>,
) -> Result<Outcome, String> {
    let json = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    let proto = (m.request_json_to_proto)(&json).map_err(|e| format!("encode: {e}"))?;
    let channel = tonic::transport::Endpoint::from_shared(url.to_owned())
        .map_err(|e| e.to_string())?
        .connect()
        .await
        .map_err(|e| format!("gRPC connect: {e}"))?;
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.map_err(|e| format!("gRPC ready: {e}"))?;
    let mut req = tonic::Request::new(proto);
    req.metadata_mut().insert(
        "authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| "token header")?,
    );
    let path: http::uri::PathAndQuery = m.path.parse().map_err(|_| "path")?;
    let to_json = |bytes: &[u8]| -> Result<Value, String> {
        let j = (m.response_proto_to_json)(bytes).map_err(|e| format!("decode: {e}"))?;
        serde_json::from_slice(&j).map_err(|e| e.to_string())
    };
    let status_outcome = |s: tonic::Status| -> Result<Outcome, String> {
        let d = bee_proto::errors::detail_from_status(&s)
            .ok_or_else(|| format!("gRPC status without ErrorDetail: {s}"))?;
        Ok(Outcome::Err(ErrorFacts {
            reason: d.reason,
            retryable: d.retryable,
            subcode: d.subcode,
            message: d.message,
        }))
    };
    if m.kind == MethodKind::Unary {
        return match grpc.unary(req, path, RawCodec).await {
            Ok(resp) => Ok(Outcome::Ok(vec![to_json(&resp.into_inner())?])),
            Err(s) => status_outcome(s),
        };
    }
    let mut stream = match grpc.server_streaming(req, path, RawCodec).await {
        Ok(resp) => resp.into_inner(),
        Err(s) => return status_outcome(s),
    };
    let idle = if is_event_stream(m) {
        STREAM_IDLE
    } else {
        Duration::from_secs(120)
    };
    let mut items = Vec::new();
    loop {
        if max_items.is_some_and(|n| items.len() as u64 >= n) {
            break;
        }
        match tokio::time::timeout(idle, stream.message()).await {
            Ok(Ok(Some(bytes))) => items.push(to_json(&bytes)?),
            Ok(Ok(None)) | Err(_) => break,
            Ok(Err(s)) => return status_outcome(s),
        }
    }
    Ok(Outcome::Ok(items))
}

/// A JSON summary of a request for failure messages.
pub fn describe(request: &Value) -> String {
    let text = request.to_string();
    let limit = if std::env::var_os("BEE_CONFORMANCE_SHOW").is_some() {
        3000
    } else {
        400
    };
    if text.len() > limit {
        format!(
            "{}…",
            &text[..text.char_indices().nth(limit).map_or(text.len(), |c| c.0)]
        )
    } else {
        text
    }
}
