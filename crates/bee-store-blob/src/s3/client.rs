//! A minimal signed HTTP client for the S3 REST API: addressing, SigV4, retries with jitter
//! and deadlines. The store above it speaks in object keys and decoded responses.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bee_core::{BeeError, Reason};
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use super::sigv4::{self, SigningKey, Stamp};
use super::xml;
use super::{RetryPolicy, S3Credentials};
use crate::common::{internal, unavailable};

type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Where requests go: scheme and `host[:port]` of the endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub scheme: String,
    pub authority: String,
}

impl Target {
    /// Parses `http(s)://host[:port]`; no path, query or userinfo.
    pub(crate) fn parse(endpoint: &str) -> Result<Target, BeeError> {
        let bad = |why: &str| {
            BeeError::invalid(
                bee_core::Subcode::InvalidField,
                format!("S3 endpoint `{endpoint}`: {why}"),
            )
        };
        let (scheme, rest) = endpoint
            .split_once("://")
            .ok_or_else(|| bad("expected http:// or https://"))?;
        if scheme != "http" && scheme != "https" {
            return Err(bad("scheme must be http or https"));
        }
        let authority = rest.trim_end_matches('/');
        if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
            return Err(bad("expected only host[:port]"));
        }
        Ok(Target {
            scheme: scheme.to_string(),
            authority: authority.to_string(),
        })
    }
}

/// One S3 REST request, in terms of the object key (or none for bucket operations).
#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub method: Method,
    pub key: Option<String>,
    pub query: Vec<(String, String)>,
    /// Extra headers, lowercase names; all of them are signed.
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Request {
    pub(crate) fn new(method: Method, key: Option<&str>) -> Request {
        Request {
            method,
            key: key.map(str::to_string),
            query: Vec::new(),
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }

    pub(crate) fn query(mut self, name: &str, value: impl Into<String>) -> Request {
        self.query.push((name.to_string(), value.into()));
        self
    }

    pub(crate) fn header(mut self, name: &str, value: impl Into<String>) -> Request {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub(crate) fn body(mut self, body: impl Into<Bytes>) -> Request {
        self.body = body.into();
        self
    }
}

/// A response with its body read.
#[derive(Debug)]
pub(crate) struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Response {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The `<Code>` of an S3 error body.
    pub(crate) fn error_code(&self) -> Option<String> {
        xml::first(&self.text(), "Code").map(xml::unescape)
    }

    /// True for a 2xx whose body is nonetheless an `<Error>` (CopyObject and
    /// CompleteMultipartUpload can answer 200 and fail later).
    pub(crate) fn is_error_in_body(&self) -> bool {
        self.status.is_success() && self.text().trim_start().starts_with("<Error>")
    }

    /// The error to report when `op` got an answer it cannot use.
    pub(crate) fn error_for(&self, op: &str) -> BeeError {
        let text = self.text();
        let code = xml::first(&text, "Code").map(xml::unescape);
        let message = xml::first(&text, "Message").map(xml::unescape);
        let reason =
            if self.status.is_server_error() || self.status == StatusCode::TOO_MANY_REQUESTS {
                Reason::Unavailable
            } else {
                Reason::Internal
            };
        let mut e = BeeError::new(
            reason,
            format!(
                "S3 {op} failed: HTTP {} {} {}",
                self.status.as_u16(),
                code.as_deref().unwrap_or("-"),
                message.as_deref().unwrap_or("")
            )
            .trim_end()
            .to_string(),
        )
        .with_payload("s3_status", i64::from(self.status.as_u16()));
        if let Some(code) = code {
            e = e.with_payload("s3_code", code.as_str());
        }
        e
    }
}

/// Connection and signing settings of an [`S3Client`].
#[derive(Clone, Debug)]
pub(crate) struct ClientConfig {
    pub target: Target,
    pub region: String,
    pub bucket: String,
    pub path_style: bool,
    pub credentials: S3Credentials,
    pub retry: RetryPolicy,
}

/// The signed client; cheap to clone.
#[derive(Clone)]
pub(crate) struct S3Client {
    http: HttpClient,
    cfg: Arc<ClientConfig>,
}

impl std::fmt::Debug for S3Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Client").field("cfg", &self.cfg).finish()
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A uniformly random number in `0..=max`.
fn random_up_to(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    let mut raw = [0u8; 8];
    if getrandom::fill(&mut raw).is_err() {
        // Without entropy, fall back to the middle of the range: still spreads retries
        // across attempts, just not across clients.
        return max / 2;
    }
    u64::from_le_bytes(raw) % (max + 1)
}

impl S3Client {
    pub(crate) fn new(cfg: ClientConfig) -> S3Client {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        S3Client {
            http: Client::builder(TokioExecutor::new()).build(connector),
            cfg: Arc::new(cfg),
        }
    }

    pub(crate) fn config(&self) -> &ClientConfig {
        &self.cfg
    }

    /// `(host, percent-encoded path)` for a key or the bucket.
    pub(crate) fn location(&self, key: Option<&str>) -> (String, String) {
        let encoded_key = key.map(|k| sigv4::uri_encode(k, true));
        if self.cfg.path_style {
            let path = match encoded_key {
                Some(k) => format!("/{}/{k}", self.cfg.bucket),
                None => format!("/{}", self.cfg.bucket),
            };
            (self.cfg.target.authority.clone(), path)
        } else {
            let host = format!("{}.{}", self.cfg.bucket, self.cfg.target.authority);
            let path = match encoded_key {
                Some(k) => format!("/{k}"),
                None => "/".to_string(),
            };
            (host, path)
        }
    }

    fn signing_key(&self) -> SigningKey<'_> {
        SigningKey {
            access_key: &self.cfg.credentials.access_key_id,
            secret_key: &self.cfg.credentials.secret_access_key,
            session_token: self.cfg.credentials.session_token.as_deref(),
            region: &self.cfg.region,
        }
    }

    /// A presigned GET URL for `key`, valid `expires_secs` from `now_secs`.
    pub(crate) fn presign_get(&self, key: &str, expires_secs: u64, now_secs: u64) -> String {
        let (host, path) = self.location(Some(key));
        let query = sigv4::presign_query(
            &self.signing_key(),
            "GET",
            &host,
            &path,
            expires_secs,
            &Stamp::at(now_secs),
        );
        format!("{}://{host}{path}?{query}", self.cfg.target.scheme)
    }

    /// Sends one attempt, signed now.
    async fn once(&self, req: &Request) -> Result<Response, String> {
        let when = Stamp::at(unix_secs());
        let (host, path) = self.location(req.key.as_deref());
        let payload = sigv4::sha256_hex(&req.body);
        let mut headers = vec![
            ("host".to_string(), host.clone()),
            ("x-amz-date".to_string(), when.stamp.clone()),
            ("x-amz-content-sha256".to_string(), payload.clone()),
        ];
        if let Some(token) = &self.cfg.credentials.session_token {
            headers.push(("x-amz-security-token".to_string(), token.clone()));
        }
        headers.extend(req.headers.iter().cloned());
        let authorization = sigv4::authorization(
            &self.signing_key(),
            req.method.as_str(),
            &path,
            &req.query,
            &headers,
            &payload,
            &when,
        );
        let mut uri = format!("{}://{host}{path}", self.cfg.target.scheme);
        if !req.query.is_empty() {
            uri.push('?');
            uri.push_str(&sigv4::encode_query(&req.query));
        }
        let mut builder = http::Request::builder().method(req.method.clone()).uri(uri);
        for (name, value) in headers.iter().filter(|(n, _)| n != "host") {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let request = builder
            .header("authorization", authorization)
            .body(Full::new(req.body.clone()))
            .map_err(|e| format!("building the request: {e}"))?;
        let response = self
            .http
            .request(request)
            .await
            .map_err(|e| format!("sending the request: {e}"))?;
        let (parts, body) = response.into_parts();
        let body = body
            .collect()
            .await
            .map_err(|e| format!("reading the response: {e}"))?
            .to_bytes();
        Ok(Response {
            status: parts.status,
            headers: parts.headers,
            body,
        })
    }

    /// Sends `req`, retrying connection failures, timeouts, throttling and 5xx answers with
    /// exponential backoff and jitter, within the policy's attempt and deadline limits. Any
    /// other answer, 4xx included, is returned for the caller to interpret.
    pub(crate) async fn send(&self, req: &Request) -> bee_core::Result<Response> {
        let policy = &self.cfg.retry;
        let deadline = Instant::now() + policy.deadline;
        let mut attempt: u32 = 0;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(BeeError::new(
                    Reason::DeadlineExceeded,
                    format!(
                        "S3 {} deadline of {:?} exceeded",
                        req.method, policy.deadline
                    ),
                ));
            }
            let budget = remaining.min(policy.request_timeout);
            let failure = match tokio::time::timeout(budget, self.once(req)).await {
                Ok(Ok(response)) if !retryable(response.status) => return Ok(response),
                Ok(Ok(response)) => format!("HTTP {}", response.status.as_u16()),
                Ok(Err(e)) => e,
                Err(_) => format!("no answer within {budget:?}"),
            };
            if attempt >= policy.max_retries {
                return Err(unavailable(format!(
                    "S3 {} failed after {} attempts: {failure}",
                    req.method,
                    attempt + 1
                )));
            }
            let ceiling = policy
                .base_backoff
                .saturating_mul(1u32 << attempt.min(16))
                .min(policy.max_backoff);
            // Equal jitter: half the ceiling is guaranteed, the rest is random.
            let ceiling_ms = ceiling.as_millis() as u64;
            let delay = Duration::from_millis(ceiling_ms / 2 + random_up_to(ceiling_ms / 2));
            if delay >= deadline.saturating_duration_since(Instant::now()) {
                return Err(BeeError::new(
                    Reason::DeadlineExceeded,
                    format!(
                        "S3 {} deadline exceeded while retrying: {failure}",
                        req.method
                    ),
                ));
            }
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    /// Like [`S3Client::send`], but any status outside `ok` is an error.
    pub(crate) async fn expect(
        &self,
        op: &str,
        req: &Request,
        ok: &[u16],
    ) -> bee_core::Result<Response> {
        let response = self.send(req).await?;
        if ok.contains(&response.status.as_u16()) && !response.is_error_in_body() {
            Ok(response)
        } else if response.is_error_in_body() {
            Err(internal(format!("S3 {op} failed: {}", response.text())))
        } else {
            Err(response.error_for(op))
        }
    }
}

fn retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || (status.is_server_error() && status != StatusCode::NOT_IMPLEMENTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoints() {
        assert_eq!(
            Target::parse("http://127.0.0.1:9000/").unwrap(),
            Target {
                scheme: "http".into(),
                authority: "127.0.0.1:9000".into()
            }
        );
        assert!(Target::parse("127.0.0.1:9000").is_err());
        assert!(Target::parse("ftp://x").is_err());
        assert!(Target::parse("https://x/bucket").is_err());
        assert!(Target::parse("https://u:p@x").is_err());
    }

    fn client(path_style: bool) -> S3Client {
        S3Client::new(ClientConfig {
            target: Target::parse("https://s3.example.com").unwrap(),
            region: "r".into(),
            bucket: "bkt".into(),
            path_style,
            credentials: S3Credentials::new("k", "s"),
            retry: RetryPolicy::default(),
        })
    }

    #[tokio::test]
    async fn addressing_styles() {
        assert_eq!(
            client(true).location(Some("o/a b")),
            ("s3.example.com".to_string(), "/bkt/o/a%20b".to_string())
        );
        assert_eq!(
            client(false).location(Some("o/x")),
            ("bkt.s3.example.com".to_string(), "/o/x".to_string())
        );
        assert_eq!(client(true).location(None).1, "/bkt");
        assert_eq!(client(false).location(None).1, "/");
    }

    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..100 {
            assert!(random_up_to(10) <= 10);
        }
        assert_eq!(random_up_to(0), 0);
    }
}
