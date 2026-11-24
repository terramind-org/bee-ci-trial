//! `S3BlobStore` against a scripted in-process HTTP server: retries, jitter, deadlines,
//! pagination, conditional-put fallback, multipart cleanup and S3's quirks (errors inside a
//! 200). These run everywhere; the behaviours that need a real service are in `s3.rs`.

use crate::common;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bee_core::{ObjectKind, Reason};
use bee_store::{BlobStore, StorageClass, blob_key};
use bee_store_blob::s3::RetryPolicy;
use bee_store_blob::{S3BlobStore, S3Config, S3Credentials};
use bytes::Bytes;
use common::{blob, fresh_org, noise};
use http::{HeaderMap, Method, Request, Response};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

fn pct_decode(text: &str) -> String {
    let raw = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && i + 2 < raw.len()
            && let Ok(b) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Clone, Debug)]
struct Recorded {
    method: Method,
    path: String,
    query: BTreeMap<String, String>,
    headers: HeaderMap,
    body_len: usize,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    delay: Duration,
}

impl Reply {
    fn status(status: u16) -> Reply {
        Reply {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            delay: Duration::ZERO,
        }
    }
    fn body(mut self, body: impl Into<Vec<u8>>) -> Reply {
        self.body = body.into();
        self
    }
    fn header(mut self, name: &'static str, value: impl Into<String>) -> Reply {
        self.headers.push((name, value.into()));
        self
    }
    fn delayed(mut self, delay: Duration) -> Reply {
        self.delay = delay;
        self
    }
}

type Handler = dyn Fn(usize, &Recorded) -> Reply + Send + Sync;

struct Stub {
    endpoint: String,
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl Stub {
    fn requests(&self) -> Vec<Recorded> {
        self.log.lock().unwrap().clone()
    }
}

async fn spawn_stub(handler: impl Fn(usize, &Recorded) -> Reply + Send + Sync + 'static) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let handler: Arc<Handler> = Arc::new(handler);
    let log2 = log.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (handler, log) = (handler.clone(), log2.clone());
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let (handler, log) = (handler.clone(), log.clone());
                    async move {
                        let (parts, body) = req.into_parts();
                        let body_len = body
                            .collect()
                            .await
                            .map(|b| b.to_bytes().len())
                            .unwrap_or(0);
                        let query = parts
                            .uri
                            .query()
                            .unwrap_or("")
                            .split('&')
                            .filter(|p| !p.is_empty())
                            .map(|p| {
                                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                                (pct_decode(k), pct_decode(v))
                            })
                            .collect();
                        let recorded = Recorded {
                            method: parts.method,
                            path: parts.uri.path().to_string(),
                            query,
                            headers: parts.headers,
                            body_len,
                        };
                        let index = {
                            let mut log = log.lock().unwrap();
                            log.push(recorded.clone());
                            log.len() - 1
                        };
                        let reply = handler(index, &recorded);
                        tokio::time::sleep(reply.delay).await;
                        let mut resp = Response::builder().status(reply.status);
                        for (k, v) in reply.headers {
                            resp = resp.header(k, v);
                        }
                        Ok::<_, std::convert::Infallible>(
                            resp.body(Full::new(Bytes::from(reply.body))).unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Stub { endpoint, log }
}

fn store(stub: &Stub, tweak: impl FnOnce(&mut S3Config)) -> S3BlobStore {
    let mut config = S3Config::new("bkt", "us-east-1", S3Credentials::new("AKIDSTUB", "secret"))
        .with_endpoint(&stub.endpoint);
    config.retry = RetryPolicy {
        max_retries: 3,
        base_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(5),
        request_timeout: Duration::from_secs(5),
        deadline: Duration::from_secs(30),
    };
    config.multipart_threshold = 5 << 20;
    config.part_size = 5 << 20;
    tweak(&mut config);
    S3BlobStore::new(config).unwrap()
}

fn xml_list(keys: &[(String, u64)], truncated_with: Option<&str>) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
    );
    out.push_str(&format!(
        "<IsTruncated>{}</IsTruncated>",
        truncated_with.is_some()
    ));
    if let Some(t) = truncated_with {
        out.push_str(&format!(
            "<NextContinuationToken>{t}</NextContinuationToken>"
        ));
    }
    for (key, size) in keys {
        out.push_str(&format!(
            "<Contents><Key>{key}</Key><LastModified>2024-05-01T12:00:00.500Z</LastModified>\
             <Size>{size}</Size><StorageClass>STANDARD_IA</StorageClass></Contents>"
        ));
    }
    out.push_str("</ListBucketResult>");
    out
}

fn head_ok(len: usize) -> Reply {
    // For HEAD, the server drops the body and keeps the length it would have sent.
    Reply::status(200)
        .body(vec![0u8; len])
        .header("last-modified", "Fri, 24 May 2013 00:00:00 GMT")
}

#[tokio::test(flavor = "multi_thread")]
async fn throttling_and_server_errors_are_retried_with_signed_requests() {
    let stub = spawn_stub(|n, _| match n {
        0 => Reply::status(503).body("<Error><Code>SlowDown</Code></Error>"),
        1 => Reply::status(500),
        _ => head_ok(3),
    })
    .await;
    let s = store(&stub, |_| {});
    let org = fresh_org();
    let (id, _) = blob(b"abc");
    assert_eq!(s.has(org, &[id]).await.unwrap(), vec![true]);

    let seen = stub.requests();
    assert_eq!(seen.len(), 3);
    for r in &seen {
        assert_eq!(r.method, Method::HEAD);
        assert_eq!(r.path, format!("/bkt/{}", blob_key(org, &id, false)));
        let auth = r.header("authorization").unwrap();
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDSTUB/"),
            "{auth}"
        );
        assert!(auth.contains("/us-east-1/s3/aws4_request"), "{auth}");
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        assert_eq!(r.header("x-amz-content-sha256").unwrap().len(), 64);
        assert_eq!(r.header("x-amz-date").unwrap().len(), 16);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn absent_objects_are_answered_without_retrying() {
    let stub = spawn_stub(|_, _| Reply::status(404)).await;
    let s = store(&stub, |_| {});
    let org = fresh_org();
    let (id, _) = blob(b"abc");
    assert_eq!(s.has(org, &[id]).await.unwrap(), vec![false]);
    assert_eq!(s.get(org, &id).await.unwrap(), None);
    assert_eq!(s.stat(org, &id).await.unwrap(), None);
    assert!(!s.delete(org, &id, None).await.unwrap());
    // has: 1 request; get: 1; stat: HEAD o/ and HEAD q/ = 2; delete: 2.
    assert_eq!(stub.requests().len(), 1 + 1 + 2 + 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn gives_up_after_the_retry_budget_with_a_retryable_error() {
    let stub = spawn_stub(|_, _| Reply::status(500)).await;
    let s = store(&stub, |c| c.retry.max_retries = 2);
    let (id, _) = blob(b"abc");
    let err = s.has(fresh_org(), &[id]).await.unwrap_err();
    assert_eq!(err.reason, Reason::Unavailable);
    assert!(err.retryable);
    assert_eq!(stub.requests().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn backoff_is_exponential_with_jitter() {
    let stub = spawn_stub(|_, _| Reply::status(503)).await;
    let s = store(&stub, |c| {
        c.retry.max_retries = 3;
        c.retry.base_backoff = Duration::from_millis(40);
        c.retry.max_backoff = Duration::from_secs(5);
    });
    let (id, _) = blob(b"abc");
    let started = Instant::now();
    assert!(s.has(fresh_org(), &[id]).await.is_err());
    let elapsed = started.elapsed();
    // Equal jitter: each sleep is between half and all of 40, 80, 160 ms.
    assert!(elapsed >= Duration::from_millis(140), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert_eq!(stub.requests().len(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_services_hit_the_deadline() {
    let stub = spawn_stub(|_, _| head_ok(3).delayed(Duration::from_secs(10))).await;
    let s = store(&stub, |c| {
        c.retry.max_retries = 50;
        c.retry.request_timeout = Duration::from_millis(100);
        c.retry.deadline = Duration::from_millis(450);
    });
    let (id, _) = blob(b"abc");
    let started = Instant::now();
    let err = s.has(fresh_org(), &[id]).await.unwrap_err();
    assert_eq!(err.reason, Reason::DeadlineExceeded);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(stub.requests().len() >= 2, "timeouts are retried");
}

#[tokio::test(flavor = "multi_thread")]
async fn listing_follows_continuation_tokens_and_reads_entry_fields() {
    let org = fresh_org();
    let ids: Vec<_> = (0..5)
        .map(|i| blob(format!("listed {i}").as_bytes()).0)
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    let keys: Vec<(String, u64)> = sorted
        .iter()
        .enumerate()
        .map(|(i, id)| (blob_key(org, id, false), 10 + i as u64))
        .collect();
    let stub = spawn_stub({
        let keys = keys.clone();
        move |_, r| {
            let prefix = r.query.get("prefix").cloned().unwrap_or_default();
            assert_eq!(r.query.get("list-type").map(String::as_str), Some("2"));
            if !prefix.contains("/b1/") || prefix.starts_with("q/") {
                return Reply::status(200).body(xml_list(&[], None));
            }
            match r.query.get("continuation-token") {
                None => Reply::status(200).body(xml_list(&keys[..2], Some("page2"))),
                Some(t) => {
                    assert_eq!(t, "page2");
                    assert!(!r.query.contains_key("start-after"));
                    Reply::status(200).body(xml_list(&keys[2..], None))
                }
            }
        }
    })
    .await;
    let s = store(&stub, |_| {});
    let listed = s.list(org, None, 4).await.unwrap();
    assert_eq!(
        listed.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        sorted[..4]
    );
    let (_, info) = &listed[1];
    assert_eq!(info.len, 11);
    assert_eq!(info.class, StorageClass::Cold);
    assert!(!info.quarantined);
    assert_eq!(info.written_at_ms, 1_714_564_800_500);
    // Limit 4 left room for a second page only because the first held two keys.
    let first_pages: Vec<_> = stub
        .requests()
        .into_iter()
        .filter(|r| {
            r.query
                .get("prefix")
                .is_some_and(|p| p.contains("/b1/") && p.starts_with("o/"))
        })
        .collect();
    assert_eq!(first_pages.len(), 2);

    // `after` becomes start-after of its own kind only; earlier kinds are skipped.
    let after = sorted[1];
    let _ = s.list(org, Some(after), 10).await.unwrap();
    let seen = stub.requests();
    let with_start: Vec<_> = seen
        .iter()
        .filter_map(|r| r.query.get("start-after"))
        .collect();
    assert_eq!(with_start.len(), 2, "o/ and q/ streams of the blob kind");
    assert!(with_start.iter().any(|s| s.contains(&hex_of(&after))));
    assert!(seen.iter().all(|r| {
        r.query.get("prefix").is_none_or(|p| {
            !p.contains(&format!("/{}/", ObjectKind::Blob.text_tag()))
                || r.query.contains_key("max-keys")
        })
    }));
}

fn hex_of(id: &bee_core::ObjectId) -> String {
    bee_core::hex::encode(id.digest())
}

#[tokio::test(flavor = "multi_thread")]
async fn range_reads_handle_unsatisfiable_and_ignored_ranges() {
    let data = noise(1000, 2);
    let (id, _) = blob(&data);
    let body = data.clone();
    let stub = spawn_stub(move |_, r| {
        if r.method == Method::HEAD {
            return head_ok(1000);
        }
        match r.header("range") {
            Some("bytes=5000-5009") => Reply::status(416),
            Some("bytes=10-19") => Reply::status(200).body(body.clone()),
            Some(other) => panic!("unexpected range {other}"),
            None => panic!("a range read must send a Range header"),
        }
    })
    .await;
    let s = store(&stub, |_| {});
    let org = fresh_org();
    assert_eq!(s.get_range(org, &id, 5000, 10).await.unwrap(), Some(vec![]));
    // A service that ignores Range and answers 200 is sliced locally.
    assert_eq!(
        s.get_range(org, &id, 10, 10).await.unwrap(),
        Some(data[10..20].to_vec())
    );
    // Zero length needs no body transfer.
    assert_eq!(s.get_range(org, &id, 10, 0).await.unwrap(), Some(vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn services_without_conditional_put_are_detected_once() {
    let stub = spawn_stub(|_, r| match (&r.method, r.header("if-none-match")) {
        (&Method::PUT, Some("*")) => Reply::status(501)
            .body("<Error><Code>NotImplemented</Code><Message>no</Message></Error>"),
        (&Method::PUT, None) => Reply::status(200),
        (&Method::DELETE, _) => Reply::status(204),
        other => panic!("unexpected {other:?}"),
    })
    .await;
    let s = store(&stub, |_| {});
    let org = fresh_org();
    let (id, bytes) = blob(b"fallback");
    s.put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    s.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
    let puts: Vec<_> = stub
        .requests()
        .into_iter()
        .filter(|r| r.method == Method::PUT)
        .collect();
    let conditional: Vec<bool> = puts
        .iter()
        .map(|r| r.header("if-none-match").is_some())
        .collect();
    // Conditional (rejected), then plain; the second put never tries the header again.
    assert_eq!(conditional, vec![true, false, false]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_existing_object_is_touched_not_reuploaded() {
    let stub = spawn_stub(|_, r| match &r.method {
        &Method::PUT if r.header("x-amz-copy-source").is_some() => {
            Reply::status(200).body("<CopyObjectResult><ETag>x</ETag></CopyObjectResult>")
        }
        &Method::PUT => Reply::status(412).body("<Error><Code>PreconditionFailed</Code></Error>"),
        &Method::DELETE => Reply::status(204),
        other => panic!("unexpected {other:?}"),
    })
    .await;
    let s = store(&stub, |c| c.cold_class = "STANDARD_IA".into());
    let org = fresh_org();
    let (id, bytes) = blob(b"already there");
    s.put(org, &id, bytes, StorageClass::Cold).await.unwrap();
    let seen = stub.requests();
    let copy = seen
        .iter()
        .find(|r| r.header("x-amz-copy-source").is_some())
        .expect("a touch copy");
    let key = blob_key(org, &id, false);
    assert_eq!(copy.path, format!("/bkt/{key}"));
    assert_eq!(
        copy.header("x-amz-copy-source").unwrap(),
        format!("/bkt/{key}")
    );
    assert_eq!(copy.header("x-amz-metadata-directive"), Some("REPLACE"));
    assert_eq!(copy.header("x-amz-storage-class"), Some("STANDARD_IA"));
    assert!(copy.header("x-amz-meta-bee-touch").is_some());
    // The first attempt carried the bytes; the touch did not.
    let upload = seen
        .iter()
        .find(|r| r.header("if-none-match").is_some())
        .unwrap();
    assert_eq!(upload.body_len, b"already there".len());
    assert_eq!(copy.body_len, 0);
    // And the quarantined copy, if any, is removed.
    assert!(
        seen.iter()
            .any(|r| r.method == Method::DELETE && r.path.contains("/q/"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_inside_a_200_are_errors() {
    // HEAD finds the object; the server-side copy then fails with a 200 and an <Error> body,
    // as S3 does when a copy fails after the response started.
    let stub = spawn_stub(|_, r| match &r.method {
        &Method::HEAD => head_ok(5),
        &Method::PUT => Reply::status(200)
            .body("<Error><Code>InternalError</Code><Message>try again</Message></Error>"),
        other => panic!("unexpected {other:?}"),
    })
    .await;
    let s = store(&stub, |_| {});
    let org = fresh_org();
    let (id, _) = blob(b"12345");
    assert!(s.quarantine(org, &id).await.is_err());
    // The original was not deleted: no DELETE was sent.
    assert!(stub.requests().iter().all(|r| r.method != Method::DELETE));
    assert!(s.set_class(org, &id, StorageClass::Cold).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_multipart_uploads_are_aborted() {
    let stub = spawn_stub(|_, r| match (&r.method, r.query.get("partNumber").map(String::as_str)) {
        (&Method::POST, _) if r.query.contains_key("uploads") => Reply::status(200).body(
            "<InitiateMultipartUploadResult><UploadId>up-1</UploadId></InitiateMultipartUploadResult>",
        ),
        (&Method::PUT, Some("2")) => Reply::status(400).body("<Error><Code>EntityTooSmall</Code></Error>"),
        (&Method::PUT, Some(_)) => Reply::status(200).header("etag", "\"abc\""),
        (&Method::DELETE, _) => Reply::status(204),
        other => panic!("unexpected {other:?}"),
    })
    .await;
    let s = store(&stub, |_| {});
    let org = fresh_org();
    let data = noise(11 * 1024 * 1024, 5);
    let (id, bytes) = blob(&data);
    let err = s.put(org, &id, bytes, StorageClass::Hot).await.unwrap_err();
    assert!(err.message.contains("EntityTooSmall"), "{}", err.message);
    let seen = stub.requests();
    let abort = seen
        .iter()
        .find(|r| r.method == Method::DELETE && r.query.contains_key("uploadId"))
        .expect("the upload is aborted");
    assert_eq!(
        abort.query.get("uploadId").map(String::as_str),
        Some("up-1")
    );
    // No complete was sent, and no quarantine cleanup either.
    assert!(
        seen.iter()
            .all(|r| !(r.method == Method::POST && r.query.contains_key("uploadId")))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn multipart_sends_parts_in_order_and_completes_with_their_etags() {
    let stub = spawn_stub(|_, r| match (&r.method, r.query.get("partNumber").map(String::as_str)) {
        (&Method::POST, _) if r.query.contains_key("uploads") => Reply::status(200)
            .body("<InitiateMultipartUploadResult><UploadId>u</UploadId></InitiateMultipartUploadResult>"),
        (&Method::PUT, Some(n)) => Reply::status(200).header("etag", format!("\"etag-{n}\"")),
        (&Method::POST, _) => {
            Reply::status(200).body("<CompleteMultipartUploadResult><ETag>x</ETag></CompleteMultipartUploadResult>")
        }
        (&Method::DELETE, _) => Reply::status(204),
        other => panic!("unexpected {other:?}"),
    })
    .await;
    let s = store(&stub, |_| {});
    let org = fresh_org();
    let data = noise(12 * 1024 * 1024 + 1, 6);
    let (id, bytes) = blob(&data);
    s.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
    let seen = stub.requests();
    let mut parts: Vec<(usize, usize)> = seen
        .iter()
        .filter_map(|r| {
            r.query
                .get("partNumber")
                .map(|n| (n.parse().unwrap(), r.body_len))
        })
        .collect();
    parts.sort();
    // 12 MiB + 1 in 5 MiB parts: 5, 5, 2 MiB + 1.
    assert_eq!(parts, vec![(1, 5 << 20), (2, 5 << 20), (3, (2 << 20) + 1)]);
    let complete = seen
        .iter()
        .find(|r| r.method == Method::POST && r.query.contains_key("uploadId"))
        .unwrap();
    assert_eq!(
        complete.query.get("uploadId").map(String::as_str),
        Some("u")
    );
    assert!(complete.body_len > 100);
}

#[tokio::test(flavor = "multi_thread")]
async fn other_failures_are_errors_with_the_s3_code() {
    let stub = spawn_stub(|_, _| {
        Reply::status(403).body("<Error><Code>AccessDenied</Code><Message>no</Message></Error>")
    })
    .await;
    let s = store(&stub, |_| {});
    let (id, _) = blob(b"x");
    let err = s.get(fresh_org(), &id).await.unwrap_err();
    assert_eq!(err.reason, Reason::Internal);
    assert!(err.message.contains("AccessDenied"), "{}", err.message);
    assert_eq!(stub.requests().len(), 1, "403 is not retried");
}
