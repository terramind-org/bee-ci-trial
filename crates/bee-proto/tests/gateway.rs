//! The Connect gateway against tiny fake services: unary JSON and binary, server and client
//! streaming envelopes, metadata, and error mapping, in process and over a raw TCP socket.

use std::pin::Pin;

use axum::Router;
use axum::body::Body;
use base64::Engine as _;
use bee_proto::connect::{BASE64, ConnectError, ConnectGateway, EndStream};
use bee_proto::envelope::{self, FLAG_END_STREAM};
use bee_proto::errors;
use bee_proto::v1::object_service_server::{ObjectService, ObjectServiceServer};
use bee_proto::v1::write_service_server::{WriteService, WriteServiceServer};
use bee_proto::v1::*;
use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use prost::Message;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::Stream;
use tonic::{Code, Status};

type ChunkStream = Pin<Box<dyn Stream<Item = Result<ObjectChunk, Status>> + Send>>;

/// Fake object tier: echoes metadata, streams one chunk per id, fails on demand.
struct FakeObjects;

#[tonic::async_trait]
impl ObjectService for FakeObjects {
    async fn get_server_info(
        &self,
        request: tonic::Request<GetServerInfoRequest>,
    ) -> Result<tonic::Response<ServerInfo>, Status> {
        let caller = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("anonymous")
            .to_owned();
        let mut response = tonic::Response::new(ServerInfo {
            version: "test".into(),
            tier: caller,
            max_blob_bytes: 1 << 40,
            redaction_epoch: 3,
            ..ServerInfo::default()
        });
        response
            .metadata_mut()
            .insert("x-served-by", "fake".parse().expect("header"));
        Ok(response)
    }

    type GetObjectsStream = ChunkStream;

    async fn get_objects(
        &self,
        request: tonic::Request<GetObjectsRequest>,
    ) -> Result<tonic::Response<ChunkStream>, Status> {
        let ids = request.into_inner().ids;
        let items: Vec<Result<ObjectChunk, Status>> = ids
            .into_iter()
            .map(|id| {
                if id.raw.as_slice() == b"bad" {
                    Err(errors::status(ErrorReason::Integrity, "hash mismatch"))
                } else {
                    let mut data = id.raw.clone();
                    data.reverse();
                    Ok(ObjectChunk {
                        total_size: data.len() as u64,
                        id: Some(id),
                        data,
                        last: true,
                        status: ItemStatus::Ok as i32,
                        ..ObjectChunk::default()
                    })
                }
            })
            .collect();
        Ok(tonic::Response::new(Box::pin(tokio_stream::iter(items))))
    }

    async fn has_objects(
        &self,
        request: tonic::Request<HasObjectsRequest>,
    ) -> Result<tonic::Response<HasObjectsResponse>, Status> {
        let present = request
            .into_inner()
            .ids
            .iter()
            .map(|id| !id.raw.is_empty())
            .collect();
        Ok(tonic::Response::new(HasObjectsResponse { present }))
    }

    async fn resolve_ref(
        &self,
        request: tonic::Request<ResolveRefRequest>,
    ) -> Result<tonic::Response<ResolveRefResponse>, Status> {
        let request = request.into_inner();
        if request.name == "main" {
            return Ok(tonic::Response::new(ResolveRefResponse {
                r#ref: Some(Ref {
                    name: "main".into(),
                    target: Some(ObjectId { raw: vec![1, 2, 3] }),
                    version: 7,
                }),
                ..ResolveRefResponse::default()
            }));
        }
        let mut detail = errors::error_detail(ErrorReason::VersionMismatch, "stale version");
        detail.payload = Some(error_detail::Payload::VersionMismatch(VersionMismatch {
            r#ref: request.name,
            expected_version: request.min_version,
            current_version: 9,
            current_target: None,
        }));
        Err(errors::status_from_detail(&detail))
    }

    async fn get_tier_stats(
        &self,
        _request: tonic::Request<GetTierStatsRequest>,
    ) -> Result<tonic::Response<TierStats>, Status> {
        // A bare status with no ErrorDetail: the gateway must synthesise one.
        Err(Status::unavailable("primary down"))
    }

    async fn get_daemon_snapshot(
        &self,
        _request: tonic::Request<GetDaemonSnapshotRequest>,
    ) -> Result<tonic::Response<DaemonSnapshot>, Status> {
        Ok(tonic::Response::new(DaemonSnapshot::default()))
    }
}

/// Fake write path: only client-streaming UploadObjects is served.
struct FakeWrites;

#[tonic::async_trait]
impl WriteService for FakeWrites {
    async fn upload_objects(
        &self,
        request: tonic::Request<tonic::Streaming<UploadItem>>,
    ) -> Result<tonic::Response<UploadObjectsResponse>, Status> {
        let mut stream = request.into_inner();
        let mut stored = Vec::new();
        while let Some(item) = stream.message().await? {
            if item.last
                && let Some(id) = item.id
            {
                stored.push(id);
            }
        }
        Ok(tonic::Response::new(UploadObjectsResponse {
            stored,
            already_present: Vec::new(),
        }))
    }

    async fn create_changeset(
        &self,
        _r: tonic::Request<CreateChangesetRequest>,
    ) -> Result<tonic::Response<CreateChangesetResponse>, Status> {
        Err(errors::status(ErrorReason::Unimplemented, "fake"))
    }

    async fn rebase(
        &self,
        _r: tonic::Request<RebaseRequest>,
    ) -> Result<tonic::Response<RebaseResponse>, Status> {
        Err(errors::status(ErrorReason::Unimplemented, "fake"))
    }

    async fn merge(
        &self,
        _r: tonic::Request<MergeRequest>,
    ) -> Result<tonic::Response<MergeResponse>, Status> {
        Err(errors::status(ErrorReason::Unimplemented, "fake"))
    }

    async fn land(
        &self,
        _r: tonic::Request<LandRequest>,
    ) -> Result<tonic::Response<LandResponse>, Status> {
        Err(errors::status(ErrorReason::Unimplemented, "fake"))
    }

    async fn revert(
        &self,
        _r: tonic::Request<RevertRequest>,
    ) -> Result<tonic::Response<RevertResponse>, Status> {
        Err(errors::status(ErrorReason::Unimplemented, "fake"))
    }
}

fn router() -> Router {
    ConnectGateway::new()
        .max_message_bytes(64 * 1024)
        .add_service(ObjectServiceServer::new(FakeObjects))
        .add_service(WriteServiceServer::new(FakeWrites))
        .into_router()
}

struct Reply {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Bytes,
}

async fn post(path: &str, content_type: &str, body: impl Into<Body>) -> Reply {
    post_with(path, content_type, body, &[]).await
}

async fn post_with(
    path: &str,
    content_type: &str,
    body: impl Into<Body>,
    headers: &[(&str, &str)],
) -> Reply {
    use tower::ServiceExt as _;
    let mut builder = Request::post(path).header("content-type", content_type);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = router()
        .oneshot(builder.body(body.into()).expect("request"))
        .await
        .expect("infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    Reply {
        status,
        headers,
        body,
    }
}

fn json_body(reply: &Reply) -> Value {
    serde_json::from_slice(&reply.body).expect("JSON body")
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[tokio::test]
async fn unary_json_round_trip_with_metadata() {
    let reply = post_with(
        "/bee.v1.ObjectService/GetServerInfo",
        "application/json",
        "{}",
        &[
            ("authorization", "Bearer abc"),
            ("connect-protocol-version", "1"),
        ],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["content-type"], "application/json");
    assert_eq!(reply.headers["x-served-by"], "fake");
    let body = json_body(&reply);
    assert_eq!(body["version"], "test");
    assert_eq!(
        body["tier"], "Bearer abc",
        "authorization forwarded as metadata"
    );
    // proto3-JSON: 64-bit integers are strings, names are lowerCamelCase.
    assert_eq!(body["maxBlobBytes"], "1099511627776");
    assert_eq!(body["redactionEpoch"], "3");
}

#[tokio::test]
async fn unary_json_fields_and_empty_body() {
    let request = json!({"repo": {"fullName": "acme/api"}, "ids": [{"raw": b64(b"x")}, {}]});
    let reply = post(
        "/bee.v1.ObjectService/HasObjects",
        "application/json; charset=utf-8",
        request.to_string(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(json_body(&reply), json!({"present": [true, false]}));

    // An empty body is `{}`; unknown fields are ignored.
    let reply = post(
        "/bee.v1.ObjectService/GetDaemonSnapshot",
        "application/json",
        "",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = post(
        "/bee.v1.ObjectService/HasObjects",
        "application/json",
        r#"{"futureField": 1, "ids": []}"#,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(json_body(&reply), json!({}));
}

#[tokio::test]
async fn unary_binary_proto() {
    let request = HasObjectsRequest {
        repo: None,
        ids: vec![ObjectId { raw: vec![9] }],
    };
    let reply = post(
        "/bee.v1.ObjectService/HasObjects",
        "application/proto",
        request.encode_to_vec(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["content-type"], "application/proto");
    let response = HasObjectsResponse::decode(reply.body).expect("proto");
    assert_eq!(response.present, vec![true]);
}

#[tokio::test]
async fn unary_error_maps_to_connect_error_and_back() {
    let reply = post(
        "/bee.v1.ObjectService/ResolveRef",
        "application/json",
        r#"{"name": "release/1", "minVersion": "4"}"#,
    )
    .await;
    // VERSION_MISMATCH → ABORTED → Connect "aborted" → HTTP 409.
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.headers["content-type"], "application/json");
    let body = json_body(&reply);
    assert_eq!(body["code"], "aborted");
    assert_eq!(body["message"], "stale version");
    assert_eq!(body["details"][0]["type"], "bee.v1.ErrorDetail");
    assert_eq!(body["details"][0]["debug"]["reason"], "VERSION_MISMATCH");
    assert_eq!(
        body["details"][0]["debug"]["versionMismatch"]["currentVersion"],
        "9"
    );

    // The base64 value decodes to the exact ErrorDetail the service sent.
    let error: ConnectError = serde_json::from_slice(&reply.body).expect("connect error");
    let raw = BASE64
        .decode(error.details[0].value.as_bytes())
        .expect("base64");
    let detail = ErrorDetail::decode(raw.as_slice()).expect("detail");
    assert_eq!(detail.reason, "VERSION_MISMATCH");
    assert!(!detail.retryable);
    let Some(error_detail::Payload::VersionMismatch(mismatch)) = &detail.payload else {
        panic!("payload {:?}", detail.payload);
    };
    assert_eq!(
        (mismatch.expected_version, mismatch.current_version),
        (4, 9)
    );

    // Connect error → tonic status keeps code, message and detail.
    let status = error.into_status();
    assert_eq!(status.code(), Code::Aborted);
    assert_eq!(status.message(), "stale version");
    assert_eq!(errors::detail_from_status(&status), Some(detail.clone()));
    // And back again gives the same Connect error.
    assert_eq!(
        serde_json::to_value(ConnectError::from_status(&status)).expect("json"),
        body
    );
}

#[tokio::test]
async fn bare_status_gets_synthesised_detail() {
    let reply = post(
        "/bee.v1.ObjectService/GetTierStats",
        "application/json",
        "{}",
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    let error: ConnectError = serde_json::from_slice(&reply.body).expect("connect error");
    assert_eq!(error.code, "unavailable");
    let detail = error.error_detail().expect("detail");
    assert_eq!(detail.reason, "UNAVAILABLE");
    assert_eq!(detail.message, "primary down");
    assert!(detail.retryable);
}

#[tokio::test]
async fn server_streaming_envelopes() {
    let request = json!({"ids": [{"raw": b64(b"ab")}, {"raw": b64(b"xyz")}]});
    let reply = post(
        "/bee.v1.ObjectService/GetObjects",
        "application/connect+json",
        envelope::encode(0, request.to_string().as_bytes()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["content-type"], "application/connect+json");
    let frames = envelope::decode_all(&reply.body, usize::MAX).expect("frames");
    assert_eq!(frames.len(), 3, "two messages and the end-stream frame");
    let first: Value = serde_json::from_slice(&frames[0].data).expect("json");
    assert_eq!(first["data"], b64(b"ba"));
    assert_eq!(first["status"], "ITEM_STATUS_OK");
    assert_eq!(first["totalSize"], "2");
    let second: Value = serde_json::from_slice(&frames[1].data).expect("json");
    assert_eq!(second["data"], b64(b"zyx"));
    assert_eq!(frames[0].flags, 0);
    assert_eq!(frames[2].flags, FLAG_END_STREAM);
    let end: EndStream = serde_json::from_slice(&frames[2].data).expect("end");
    assert!(end.error.is_none(), "{end:?}");
}

#[tokio::test]
async fn server_streaming_error_in_end_stream() {
    let request = json!({"ids": [{"raw": b64(b"ok")}, {"raw": b64(b"bad")}]});
    let reply = post(
        "/bee.v1.ObjectService/GetObjects",
        "application/connect+json",
        envelope::encode(0, request.to_string().as_bytes()),
    )
    .await;
    // Streaming errors travel in the end-stream frame; the HTTP status stays 200.
    assert_eq!(reply.status, StatusCode::OK);
    let frames = envelope::decode_all(&reply.body, usize::MAX).expect("frames");
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].flags, FLAG_END_STREAM);
    let end: EndStream = serde_json::from_slice(&frames[1].data).expect("end");
    let error = end.error.expect("error");
    assert_eq!(error.code, "data_loss");
    let detail = error.error_detail().expect("detail");
    assert_eq!(detail.reason, "INTEGRITY");
    assert!(detail.retryable);
}

#[tokio::test]
async fn client_streaming_envelopes() {
    let mut body = Vec::new();
    for (id, last) in [(b"one".as_slice(), false), (b"one", true), (b"two", true)] {
        let item = json!({"id": {"raw": b64(id)}, "data": b64(b"payload"), "last": last});
        body.extend_from_slice(&envelope::encode(0, item.to_string().as_bytes()));
    }
    let reply = post(
        "/bee.v1.WriteService/UploadObjects",
        "application/connect+json",
        body,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let frames = envelope::decode_all(&reply.body, usize::MAX).expect("frames");
    assert_eq!(frames.len(), 2);
    let response: Value = serde_json::from_slice(&frames[0].data).expect("json");
    assert_eq!(
        response,
        json!({"stored": [{"raw": b64(b"one")}, {"raw": b64(b"two")}]})
    );
    assert_eq!(frames[1].flags, FLAG_END_STREAM);
}

#[tokio::test]
async fn binary_streaming() {
    let request = GetObjectsRequest {
        ids: vec![ObjectId { raw: vec![1, 2] }],
        ..GetObjectsRequest::default()
    };
    let reply = post(
        "/bee.v1.ObjectService/GetObjects",
        "application/connect+proto",
        envelope::encode(0, &request.encode_to_vec()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let frames = envelope::decode_all(&reply.body, usize::MAX).expect("frames");
    let chunk = ObjectChunk::decode(frames[0].data.clone()).expect("chunk");
    assert_eq!(chunk.data, vec![2, 1]);
}

#[tokio::test]
async fn protocol_errors() {
    // Unknown procedure.
    let reply = post("/bee.v1.NopeService/Nope", "application/json", "{}").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&reply)["code"], "unimplemented");

    // Known procedure whose service was not added.
    let reply = post("/bee.v1.RefService/ListRefs", "application/json", "{}").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let error: ConnectError = serde_json::from_slice(&reply.body).expect("error");
    assert_eq!(
        error.error_detail().expect("detail").reason,
        "UNIMPLEMENTED"
    );

    // Unsupported content type, and a unary content type on a streaming method.
    let reply = post("/bee.v1.ObjectService/GetServerInfo", "text/plain", "{}").await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(reply.headers.contains_key("accept-post"));
    let reply = post("/bee.v1.ObjectService/GetObjects", "application/json", "{}").await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // Malformed JSON: invalid_argument with subcode bad_json.
    let reply = post(
        "/bee.v1.ObjectService/HasObjects",
        "application/json",
        "{\"ids\": 3}",
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let error: ConnectError = serde_json::from_slice(&reply.body).expect("error");
    assert_eq!(error.code, "invalid_argument");
    let detail = error.error_detail().expect("detail");
    assert_eq!(
        (detail.reason.as_str(), detail.subcode.as_str()),
        ("INVALID_ARGUMENT", "bad_json")
    );

    // Wrong protocol version.
    let reply = post_with(
        "/bee.v1.ObjectService/GetServerInfo",
        "application/json",
        "{}",
        &[("connect-protocol-version", "2")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    // Compressed requests are refused.
    let reply = post_with(
        "/bee.v1.ObjectService/GetServerInfo",
        "application/json",
        "{}",
        &[("content-encoding", "gzip")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED);

    // Oversized body: TOO_LARGE with the limit in the payload.
    let reply = post(
        "/bee.v1.ObjectService/HasObjects",
        "application/json",
        vec![b' '; 70 * 1024],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let error: ConnectError = serde_json::from_slice(&reply.body).expect("error");
    let detail = error.error_detail().expect("detail");
    assert_eq!(detail.reason, "TOO_LARGE");
    assert!(matches!(
        detail.payload,
        Some(error_detail::Payload::LimitExceeded(LimitExceeded {
            maximum: 65536,
            ..
        }))
    ));

    // A malformed streaming request fails inside the stream with HTTP 200.
    let reply = post(
        "/bee.v1.ObjectService/GetObjects",
        "application/connect+json",
        vec![0u8, 0, 0, 0, 9, b'{'],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let frames = envelope::decode_all(&reply.body, usize::MAX).expect("frames");
    let end: EndStream = serde_json::from_slice(&frames[0].data).expect("end");
    assert_eq!(end.error.expect("error").code, "invalid_argument");

    // GET is not part of this surface.
    use tower::ServiceExt as _;
    let response = router()
        .oneshot(
            Request::get("/bee.v1.ObjectService/GetServerInfo")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

/// Sends one HTTP/1.1 request over a plain TCP socket and returns (status line, body).
async fn raw_http(addr: std::net::SocketAddr, path: &str, body: &str) -> (String, String) {
    let mut socket = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Connect-Protocol-Version: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8(raw).expect("utf-8");
    let (head, rest) = text.split_once("\r\n\r\n").expect("head");
    let status_line = head.lines().next().unwrap_or_default().to_owned();
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(rest)
    } else {
        rest.to_owned()
    };
    (status_line, body)
}

fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).expect("chunk size");
        if size == 0 {
            break;
        }
        out.push_str(&tail[..size]);
        rest = &tail[size + 2..];
    }
    out
}

#[tokio::test]
async fn raw_http_client_over_tcp() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, router()).await.expect("serve");
    });

    let (status, body) = raw_http(addr, "/bee.v1.ObjectService/GetServerInfo", "{}").await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let body: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(body["version"], "test");

    let (status, body) = raw_http(
        addr,
        "/bee.v1.ObjectService/ResolveRef",
        r#"{"name":"main"}"#,
    )
    .await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let body: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(body["ref"]["version"], "7");
    assert_eq!(body["ref"]["target"]["raw"], b64(&[1, 2, 3]));

    let (status, body) = raw_http(
        addr,
        "/bee.v1.ObjectService/ResolveRef",
        r#"{"name":"other"}"#,
    )
    .await;
    assert_eq!(status, "HTTP/1.1 409 Conflict");
    let error: ConnectError = serde_json::from_str(&body).expect("connect error");
    assert_eq!(error.code, "aborted");
    assert_eq!(
        error.error_detail().expect("detail").reason,
        "VERSION_MISMATCH"
    );
    server.abort();
}
