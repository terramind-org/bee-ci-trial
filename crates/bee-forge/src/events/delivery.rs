//! Webhook HTTP delivery: payload formats, HMAC SHA-256 signatures, the HTTP client and the
//! retry schedule.
//!
//! # Request shape
//!
//! Every delivery is an HTTP `POST` with a JSON body and these headers:
//!
//! | header | value |
//! |---|---|
//! | `Content-Type` | `application/json` |
//! | `User-Agent` | `Beehive-Hookshot/1` |
//! | `X-Bee-Event` | the event kind, e.g. `lease.acquired` |
//! | `X-Bee-Delivery` | a unique delivery GUID |
//! | `X-Bee-Hook-ID` | the webhook id |
//! | `X-Bee-Signature-256` | `sha256=<hex HMAC-SHA256(secret, body)>` (when a secret is set) |
//!
//! With [`WebhookFormat::Github`] the body follows GitHub's webhook payload shape (`action`,
//! `repository`, `organization`, `sender`, and the subject under GitHub's key) and the
//! headers are GitHub's: `X-GitHub-Event`, `X-GitHub-Delivery`, `X-GitHub-Hook-ID`,
//! `X-GitHub-Hook-Installation-Target-Type`, `X-GitHub-Hook-Installation-Target-ID` and
//! `X-Hub-Signature-256`. The full bee event is always included as `bee_event`.
//!
//! # Retries
//!
//! A delivery succeeds on any 2xx answer. Otherwise it is retried with exponential backoff
//! ([`RetryPolicy`]) until `max_attempts`; every attempt is a row in the delivery log.

use std::collections::BTreeMap;
use std::time::Duration;

use bee_proto::v1::{CheckStatus, Event, WebhookFormat, event};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::time::Instant;

use crate::ids::hex;
use crate::module::Registry;

/// Maximum response body kept in the delivery log.
pub const MAX_RESPONSE_BODY: usize = 4096;

/// Retry schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts in total, including the first.
    pub max_attempts: u32,
    /// Delay before the second attempt.
    pub base_delay: Duration,
    /// Upper bound of any delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 8,
            base_delay: Duration::from_secs(10),
            max_delay: Duration::from_secs(3600),
        }
    }
}

impl RetryPolicy {
    /// Delay after failed attempt `attempt` (1-based): `base * 2^(attempt-1)`, capped.
    pub fn delay(&self, attempt: u32) -> Duration {
        let factor = 1u32
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.base_delay
            .checked_mul(factor)
            .unwrap_or(self.max_delay)
            .min(self.max_delay)
    }
}

/// `sha256=<hex>` HMAC of `body` under `secret`.
pub fn signature(secret: &str, body: &[u8]) -> String {
    // HMAC accepts keys of any length, so construction cannot fail.
    let Ok(mut mac) = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()) else {
        return String::new();
    };
    mac.update(body);
    format!("sha256={}", hex(&mac.finalize().into_bytes()))
}

/// Constant-time check of a `sha256=<hex>` signature (for receivers and tests).
pub fn verify_signature(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(sig) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Some(raw) = crate::ids::unhex(sig) else {
        return false;
    };
    let mut mac = match <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    mac.verify_slice(&raw).is_ok()
}

/// One prepared request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    /// Body bytes (JSON).
    pub body: String,
    /// Headers, sorted.
    pub headers: BTreeMap<String, String>,
}

/// Target of a webhook for GitHub's installation-target headers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookTarget {
    /// Webhook id.
    pub hook_id: String,
    /// Repo id for repo hooks.
    pub repo_id: String,
    /// Org id.
    pub org_id: String,
}

fn event_json(e: &Event) -> Value {
    serde_json::to_value(e).unwrap_or(Value::Null)
}

/// The GitHub event name and action for `e`.
pub fn github_name(registry: &Registry, e: &Event) -> (String, String) {
    if e.kind == "ping" {
        return ("ping".into(), String::new());
    }
    if e.kind == "check.updated"
        && let Some(event::Body::Check(c)) = &e.body
    {
        let done = !matches!(
            CheckStatus::try_from(c.status).unwrap_or(CheckStatus::Unspecified),
            CheckStatus::Queued | CheckStatus::Running | CheckStatus::Unspecified
        );
        let action = if done { "completed" } else { "created" };
        return ("check_run".into(), action.into());
    }
    match registry.event_kind(&e.kind).and_then(|k| k.github) {
        Some((name, action)) => (name.into(), action.into()),
        None => (format!("bee_{}", e.kind.replace('.', "_")), String::new()),
    }
}

/// Builds the request for `event` in `format`.
pub fn prepare(
    registry: &Registry,
    format: WebhookFormat,
    target: &HookTarget,
    event: &Event,
    guid: &str,
    secret: &str,
) -> Prepared {
    let mut headers = BTreeMap::new();
    headers.insert("content-type".into(), "application/json".into());
    headers.insert("user-agent".into(), "Beehive-Hookshot/1".into());
    let body = match format {
        WebhookFormat::Github => {
            let (name, action) = github_name(registry, event);
            let mut obj = serde_json::Map::new();
            if !action.is_empty() {
                obj.insert("action".into(), json!(action));
            }
            if !event.repo_id.is_empty() {
                obj.insert(
                    "repository".into(),
                    json!({ "id": event.repo_id, "node_id": event.repo_id }),
                );
            }
            if !event.org_id.is_empty() {
                obj.insert("organization".into(), json!({ "id": event.org_id }));
            }
            obj.insert(
                "sender".into(),
                json!({ "login": event.principal, "id": event.principal }),
            );
            match &event.body {
                Some(event::Body::RefUpdated(r)) => {
                    obj.insert("ref".into(), json!(r.name));
                    obj.insert(
                        "after".into(),
                        json!(r.target.as_ref().map(|t| hex(&t.raw)).unwrap_or_default()),
                    );
                }
                Some(event::Body::Change(c)) => {
                    obj.insert(
                        "pull_request".into(),
                        serde_json::to_value(c).unwrap_or(Value::Null),
                    );
                }
                Some(event::Body::Review(r)) => {
                    obj.insert(
                        "review".into(),
                        serde_json::to_value(r).unwrap_or(Value::Null),
                    );
                }
                Some(event::Body::Check(c)) => {
                    obj.insert(
                        "check_run".into(),
                        serde_json::to_value(c).unwrap_or(Value::Null),
                    );
                }
                Some(event::Body::Lease(l)) => {
                    obj.insert(
                        "lease".into(),
                        serde_json::to_value(l).unwrap_or(Value::Null),
                    );
                }
                _ => {}
            }
            if !event.payload_json.is_empty()
                && let Ok(v) = serde_json::from_str::<Value>(&event.payload_json)
            {
                obj.insert("payload".into(), v);
            }
            if event.kind == "ping" {
                obj.insert("zen".into(), json!("Agents subscribe; they do not poll."));
                obj.insert("hook_id".into(), json!(target.hook_id));
            }
            obj.insert("bee_event".into(), event_json(event));
            headers.insert("x-github-event".into(), name);
            headers.insert("x-github-delivery".into(), guid.into());
            headers.insert("x-github-hook-id".into(), target.hook_id.clone());
            let (tt, tid) = if target.repo_id.is_empty() {
                ("organization", target.org_id.clone())
            } else {
                ("repository", target.repo_id.clone())
            };
            headers.insert("x-github-hook-installation-target-type".into(), tt.into());
            headers.insert("x-github-hook-installation-target-id".into(), tid);
            Value::Object(obj).to_string()
        }
        WebhookFormat::Bee | WebhookFormat::Unspecified => {
            headers.insert("x-bee-event".into(), event.kind.clone());
            headers.insert("x-bee-delivery".into(), guid.into());
            headers.insert("x-bee-hook-id".into(), target.hook_id.clone());
            event_json(event).to_string()
        }
    };
    if !secret.is_empty() {
        let sig = signature(secret, body.as_bytes());
        let name = if format == WebhookFormat::Github {
            "x-hub-signature-256"
        } else {
            "x-bee-signature-256"
        };
        headers.insert(name.into(), sig);
    }
    Prepared { body, headers }
}

/// Re-signs a stored request for redelivery under a fresh GUID.
pub fn reprepare(
    stored_headers: &BTreeMap<String, String>,
    body: &str,
    guid: &str,
    secret: &str,
) -> Prepared {
    let mut headers = stored_headers.clone();
    for k in ["x-github-delivery", "x-bee-delivery"] {
        if headers.contains_key(k) {
            headers.insert(k.into(), guid.into());
        }
    }
    if !secret.is_empty() {
        let sig = signature(secret, body.as_bytes());
        for k in ["x-hub-signature-256", "x-bee-signature-256"] {
            if headers.contains_key(k) {
                headers.insert(k.into(), sig.clone());
            }
        }
    }
    Prepared {
        body: body.to_string(),
        headers,
    }
}

/// What the receiver answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// HTTP status, 0 when unreachable.
    pub status: u16,
    /// Response headers.
    pub headers: BTreeMap<String, String>,
    /// Response body, truncated to [`MAX_RESPONSE_BODY`].
    pub body: String,
    /// Transport error, if any.
    pub error: String,
    /// Round trip.
    pub duration_ms: u32,
}

impl Outcome {
    /// 2xx.
    pub fn success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The HTTP client used for deliveries (HTTP and HTTPS, certificates always verified).
#[derive(Clone)]
pub struct Sender {
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>,
    timeout: Duration,
}

impl Sender {
    /// A client with a per-request timeout.
    pub fn new(timeout: Duration) -> Sender {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Sender {
            client: Client::builder(TokioExecutor::new()).build(connector),
            timeout,
        }
    }

    /// Sends one request; never fails, the outcome records errors.
    pub async fn send(&self, url: &str, req: &Prepared) -> Outcome {
        let started = Instant::now();
        let mut out = Outcome::default();
        let mut builder = http::Request::builder().method("POST").uri(url);
        for (k, v) in &req.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let request = match builder.body(Full::new(Bytes::from(req.body.clone()))) {
            Ok(r) => r,
            Err(e) => {
                out.error = format!("bad request: {e}");
                return out;
            }
        };
        match tokio::time::timeout(self.timeout, self.client.request(request)).await {
            Err(_) => out.error = "timed out".into(),
            Ok(Err(e)) => out.error = format!("unreachable: {e}"),
            Ok(Ok(resp)) => {
                out.status = resp.status().as_u16();
                for (k, v) in resp.headers() {
                    out.headers
                        .insert(k.as_str().to_string(), v.to_str().unwrap_or("").to_string());
                }
                match tokio::time::timeout(self.timeout, resp.into_body().collect()).await {
                    Ok(Ok(b)) => {
                        let bytes = b.to_bytes();
                        let cut = &bytes[..bytes.len().min(MAX_RESPONSE_BODY)];
                        out.body = String::from_utf8_lossy(cut).into_owned();
                    }
                    Ok(Err(e)) => out.error = format!("response body: {e}"),
                    Err(_) => out.error = "response body timed out".into(),
                }
            }
        }
        out.duration_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_verify() {
        let s = signature("key", b"{}");
        assert!(s.starts_with("sha256="));
        assert!(verify_signature("key", b"{}", &s));
        assert!(!verify_signature("other", b"{}", &s));
        assert!(!verify_signature("key", b"{ }", &s));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_secs(10),
            max_delay: Duration::from_secs(60),
        };
        assert_eq!(p.delay(1), Duration::from_secs(10));
        assert_eq!(p.delay(2), Duration::from_secs(20));
        assert_eq!(p.delay(3), Duration::from_secs(40));
        assert_eq!(p.delay(4), Duration::from_secs(60));
        assert_eq!(p.delay(40), Duration::from_secs(60));
    }

    #[test]
    fn github_format_uses_github_headers() {
        let r = Registry::core().unwrap();
        let e = Event {
            kind: "cr.opened".into(),
            repo_id: "R".into(),
            org_id: "O".into(),
            principal: "P".into(),
            ..Default::default()
        };
        let t = HookTarget {
            hook_id: "H".into(),
            repo_id: "R".into(),
            org_id: "O".into(),
        };
        let p = prepare(&r, WebhookFormat::Github, &t, &e, "G", "s");
        assert_eq!(p.headers["x-github-event"], "pull_request");
        assert_eq!(
            p.headers["x-github-hook-installation-target-type"],
            "repository"
        );
        assert!(verify_signature(
            "s",
            p.body.as_bytes(),
            &p.headers["x-hub-signature-256"]
        ));
        let v: Value = serde_json::from_str(&p.body).unwrap();
        assert_eq!(v["action"], "opened");
        let p = prepare(&r, WebhookFormat::Bee, &t, &e, "G", "");
        assert_eq!(p.headers["x-bee-event"], "cr.opened");
        assert!(!p.headers.contains_key("x-bee-signature-256"));
    }
}
