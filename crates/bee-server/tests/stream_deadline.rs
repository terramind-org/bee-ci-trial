//! A server-streaming RPC ends only at a deadline its client sent (SPEC §15.3): without one,
//! `limits.default_deadline_ms` does not cut an event subscription, and a stream that does end
//! on the client's deadline says how to resume. Over the Connect surface and over gRPC.

mod common;

use std::time::Duration;

use bee_proto::v1;
use bytes::{Buf, BytesMut};
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};

/// A Connect server stream read message by message.
struct ConnectStream {
    body: http_body_util::combinators::BoxBody<bytes::Bytes, String>,
    buf: BytesMut,
}

/// One envelope: its flags and payload.
struct Envelope {
    flags: u8,
    data: Vec<u8>,
}

impl ConnectStream {
    /// Opens `Subscribe` for the playground repo (all `issue.*` events from the log's start).
    async fn subscribe(t: &TestServer, timeout_ms: Option<u64>) -> ConnectStream {
        let body = serde_json::to_vec(&json!({ "repo": repo(), "kinds": ["issue.*"] })).unwrap();
        let mut payload = vec![0u8];
        payload.extend_from_slice(&(body.len() as u32).to_be_bytes());
        payload.extend_from_slice(&body);
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build_http::<http_body_util::Full<bytes::Bytes>>();
        let mut req = http::Request::builder()
            .method("POST")
            .uri(format!("{}/bee.v1.EventService/Subscribe", t.url()))
            .header("content-type", "application/connect+json")
            .header("connect-protocol-version", "1")
            .header("authorization", format!("Bearer {}", t.token));
        if let Some(ms) = timeout_ms {
            req = req.header("connect-timeout-ms", ms.to_string());
        }
        let resp = client
            .request(
                req.body(http_body_util::Full::new(bytes::Bytes::from(payload)))
                    .unwrap(),
            )
            .await
            .expect("subscribe request");
        assert_eq!(resp.status(), 200);
        ConnectStream {
            body: resp.into_body().map_err(|e| e.to_string()).boxed(),
            buf: BytesMut::new(),
        }
    }

    /// The next envelope, or `None` when the body ended or nothing arrived within `wait`.
    async fn next(&mut self, wait: Duration) -> Option<Envelope> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if self.buf.len() >= 5 {
                let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]])
                    as usize;
                if self.buf.len() >= 5 + len {
                    let flags = self.buf[0];
                    self.buf.advance(5);
                    let data = self.buf.split_to(len).to_vec();
                    return Some(Envelope { flags, data });
                }
            }
            match tokio::time::timeout_at(deadline, self.body.frame()).await {
                Ok(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        self.buf.extend_from_slice(&data);
                    }
                }
                _ => return None,
            }
        }
    }

    /// Reads events until one of kind `kind` titled `title` arrives; panics if the stream ends
    /// or `wait` passes first.
    async fn event_titled(&mut self, title: &str, wait: Duration) {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let env = self
                .next(left)
                .await
                .unwrap_or_else(|| panic!("no event titled {title:?} within {wait:?}"));
            let text = String::from_utf8_lossy(&env.data).to_string();
            assert_eq!(
                env.flags & 2,
                0,
                "the stream ended while waiting for {title:?}: {text}"
            );
            if text.contains(title) {
                return;
            }
        }
    }

    /// Reads to the end-of-stream envelope and returns its JSON.
    async fn end(&mut self, wait: Duration) -> Value {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let env = self.next(left).await.expect("the stream ends in time");
            if env.flags & 2 != 0 {
                return serde_json::from_slice(&env.data).expect("end-of-stream JSON");
            }
        }
    }
}

async fn create_issue(t: &TestServer, title: &str) {
    t.call(
        "IssueService",
        "CreateIssue",
        json!({ "repo": repo(), "title": title, "body": "" }),
    )
    .await
    .expect("create issue");
}

/// Without a timeout header, a subscription outlives `default_deadline_ms` (500 ms here) and
/// the 10 s default every forge call context starts with, and still receives events
/// published long after it opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscription_without_a_client_deadline_stays_open() {
    let t = start_with(|c| c.limits.default_deadline_ms = 500).await;
    let mut stream = ConnectStream::subscribe(&t, None).await;

    tokio::time::sleep(Duration::from_secs(2)).await;
    create_issue(&t, "published after two seconds").await;
    stream
        .event_titled("published after two seconds", Duration::from_secs(5))
        .await;

    // Past the 10 s that the request's own call context carries: access is decided again every
    // second, and none of those decisions may end the stream.
    tokio::time::sleep(Duration::from_secs(10)).await;
    create_issue(&t, "published after twelve seconds").await;
    stream
        .event_titled("published after twelve seconds", Duration::from_secs(5))
        .await;
}

/// With `Connect-Timeout-Ms` the stream ends at that deadline, with `deadline_exceeded` and
/// the field to resume from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscription_ends_at_the_deadline_its_client_sent() {
    let t = start_with(|c| c.limits.default_deadline_ms = 500).await;
    let started = tokio::time::Instant::now();
    let mut stream = ConnectStream::subscribe(&t, Some(300)).await;
    let end = stream.end(Duration::from_secs(5)).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(3),
        "ended after {elapsed:?}"
    );
    assert_eq!(end["error"]["code"], "deadline_exceeded", "{end}");
    let message = end["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("the client timeout of 300 ms elapsed")
            && message.contains("from_seq")
            && message.contains("resume"),
        "{message}"
    );
}

/// The same over gRPC: no `grpc-timeout` keeps the stream open, one ends it with
/// `DEADLINE_EXCEEDED` and the resume hint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grpc_subscriptions_follow_the_same_rule() {
    // Leave enough time for the first HTTP/2 connection to open, then outlive this short
    // default deadline before publishing an event.
    let t = start_with(|c| c.limits.default_deadline_ms = 3_000).await;
    let mut events = v1::event_service_client::EventServiceClient::new(t.grpc_channel().await);
    let request = || v1::SubscribeRequest {
        repo: prepo(),
        from_seq: 0,
        kinds: vec!["issue.*".into()],
        org: String::new(),
    };

    let mut open = events
        .subscribe(t.bearer(&t.token, request()))
        .await
        .expect("subscribe")
        .into_inner();
    tokio::time::sleep(Duration::from_secs(4)).await;
    create_issue(&t, "grpc after four seconds").await;
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let e = open.message().await.expect("stream stays healthy");
            let e = e.expect("stream stays open");
            if e.payload_json.contains("grpc after four seconds") {
                return e;
            }
        }
    })
    .await
    .expect("the event published after two seconds arrives");
    assert_eq!(got.kind, "issue.opened");
    drop(open);

    let mut timed = t.bearer(&t.token, request());
    timed
        .metadata_mut()
        .insert("grpc-timeout", "300m".parse().unwrap());
    let mut stream = events
        .subscribe(timed)
        .await
        .expect("subscribe")
        .into_inner();
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.message().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the stream ended without an error"),
                Err(s) => return s,
            }
        }
    })
    .await
    .expect("the stream ends at its deadline");
    assert_eq!(status.code(), tonic::Code::DeadlineExceeded, "{status:?}");
    assert!(
        status.message().contains("300 ms") && status.message().contains("from_seq"),
        "{status:?}"
    );
}
