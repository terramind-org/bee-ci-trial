//! The GitHub HTTP client: REST and GraphQL over hyper + rustls, rate-limit aware.
//!
//! - **Primary limits.** Every response's `x-ratelimit-*` headers are tracked; when the
//!   budget is spent the client waits for the reset before sending.
//! - **Secondary limits.** A 403/429 with `Retry-After` waits that long and retries; one
//!   without it but naming the secondary limit backs off exponentially from a minute.
//! - **Conditional requests.** [`Client::get_conditional`] sends `If-None-Match` with the
//!   stored ETag; a `304` returns the stored body and costs no quota. ETags are exported
//!   with [`Client::etags`] so a checkpoint carries them across runs.
//! - **Transient failures** (5xx, connection errors) retry with exponential backoff.
//! - Requests are sent one at a time, which keeps an import under the concurrency part of
//!   the secondary limit by construction.
//!
//! Waits longer than [`ClientConfig::max_wait_secs`] fail with `RESOURCE_EXHAUSTED` (and
//! `retry_after_secs`) instead of blocking; the import is resumable, so the caller can come
//! back later.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::Credential;
use crate::clock::{Clock, parse_rfc3339};
use crate::error::{
    Result, bad_json, denied, invalid, not_found, rate_limited, unauthenticated, unavailable,
};

type Http = HyperClient<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Client settings.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// REST base, `https://api.github.com` (or `https://<host>/api/v3`).
    pub api_url: String,
    /// GraphQL endpoint, `https://api.github.com/graphql`.
    pub graphql_url: String,
    /// The web host, `https://github.com`, for links and git URLs.
    pub web_url: String,
    /// Longest single wait for a rate limit before failing with `RESOURCE_EXHAUSTED`.
    pub max_wait_secs: u64,
    /// Retries of one request (transient errors and secondary limits).
    pub max_retries: u32,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Items per page (GitHub allows up to 100).
    pub per_page: u32,
}

impl Default for ClientConfig {
    fn default() -> ClientConfig {
        ClientConfig {
            api_url: "https://api.github.com".into(),
            graphql_url: "https://api.github.com/graphql".into(),
            web_url: "https://github.com".into(),
            max_wait_secs: 3600,
            max_retries: 8,
            timeout: Duration::from_secs(60),
            per_page: 100,
        }
    }
}

impl ClientConfig {
    /// Settings for a GitHub-compatible server at `base` (tests, GitHub Enterprise Server):
    /// REST at `base`, GraphQL at `base/graphql`, web at `base`.
    pub fn at(base: &str) -> ClientConfig {
        let b = base.trim_end_matches('/').to_string();
        ClientConfig {
            api_url: b.clone(),
            graphql_url: format!("{b}/graphql"),
            web_url: b,
            ..ClientConfig::default()
        }
    }
}

/// What the client has done, for progress output and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Requests sent.
    pub requests: u64,
    /// Conditional requests answered `304 Not Modified`.
    pub not_modified: u64,
    /// Retries after transient failures.
    pub retries: u64,
    /// Waits for the primary rate limit.
    pub rate_limit_waits: u64,
    /// Waits for a secondary rate limit.
    pub secondary_limit_waits: u64,
    /// Seconds spent waiting.
    pub waited_secs: u64,
}

/// The primary rate limit as last reported.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    /// Requests per window.
    pub limit: Option<u64>,
    /// Requests left.
    pub remaining: Option<u64>,
    /// Window reset (epoch seconds).
    pub reset: Option<u64>,
}

/// A response.
#[derive(Clone, Debug)]
pub struct Response {
    /// Status code.
    pub status: u16,
    /// Headers (lower-case names).
    pub headers: BTreeMap<String, String>,
    /// Body bytes.
    pub body: Bytes,
}

impl Response {
    /// A header value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The body as JSON.
    pub fn json(&self) -> Result<Value> {
        serde_json::from_slice(&self.body).map_err(|e| bad_json("response", e))
    }
}

/// One page of a list endpoint.
#[derive(Clone, Debug, Default)]
pub struct Page {
    /// Items on the page.
    pub items: Vec<Value>,
    /// The next page's URL (from the `Link` header).
    pub next: Option<String>,
    /// True when the page came from the ETag cache (`304`).
    pub not_modified: bool,
}

#[derive(Default)]
struct Inner {
    stats: Stats,
    rate: RateLimit,
    etags: BTreeMap<String, (String, String)>,
    app_token: Option<(String, u64)>,
}

/// The GitHub client.
#[derive(Clone)]
pub struct Client {
    http: Http,
    cfg: ClientConfig,
    credential: Credential,
    clock: Arc<dyn Clock>,
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("api_url", &self.cfg.api_url)
            .field("credential", &self.credential)
            .finish()
    }
}

/// A request body.
enum Body<'a> {
    Empty,
    Json(&'a Value),
}

impl Client {
    /// A client for `cfg` acting with `credential`, waiting on `clock`.
    pub fn new(cfg: ClientConfig, credential: Credential, clock: Arc<dyn Clock>) -> Client {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        let http = HyperClient::builder(TokioExecutor::new()).build(connector);
        Client {
            http,
            cfg,
            credential,
            clock,
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// The settings.
    pub fn config(&self) -> &ClientConfig {
        &self.cfg
    }

    /// The credential.
    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// The clock.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Counters so far.
    pub fn stats(&self) -> Stats {
        self.lock().stats.clone()
    }

    /// The rate limit as last reported by GitHub.
    pub fn rate(&self) -> RateLimit {
        self.lock().rate.clone()
    }

    /// Stored ETags and bodies, by URL (for checkpoints).
    pub fn etags(&self) -> BTreeMap<String, (String, String)> {
        self.lock().etags.clone()
    }

    /// Restores ETags saved by [`Client::etags`].
    pub fn load_etags(&self, etags: BTreeMap<String, (String, String)>) {
        self.lock().etags = etags;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock only means another task panicked mid-update of counters.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// An absolute URL for an API path (`/repos/..`) or an absolute URL as is.
    pub fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}{}", self.cfg.api_url, path)
        }
    }

    /// A list URL with `per_page` added.
    pub fn list_url(&self, path: &str) -> String {
        let sep = if path.contains('?') { '&' } else { '?' };
        self.url(&format!("{path}{sep}per_page={}", self.cfg.per_page))
    }

    /// The token git should use (`x-access-token:<token>` basic auth), when there is one.
    pub async fn git_token(&self) -> Result<Option<String>> {
        match &self.credential {
            Credential::Anonymous => Ok(None),
            Credential::Token(t) => Ok(Some(t.clone())),
            Credential::App(_) => Ok(Some(self.installation_token().await?)),
        }
    }

    /// The `Authorization` header git should send (`Basic` over `x-access-token`).
    pub async fn git_auth_header(&self) -> Result<Option<String>> {
        Ok(self.git_token().await?.map(|t| {
            let b = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{t}"));
            format!("Authorization: Basic {b}")
        }))
    }

    async fn authorization(&self) -> Result<Option<String>> {
        match &self.credential {
            Credential::Anonymous => Ok(None),
            Credential::Token(t) => Ok(Some(format!("Bearer {t}"))),
            Credential::App(_) => Ok(Some(format!("token {}", self.installation_token().await?))),
        }
    }

    /// The App installation token, exchanged from a JWT and cached until a minute before it
    /// expires.
    pub async fn installation_token(&self) -> Result<String> {
        let Credential::App(app) = &self.credential else {
            return Err(unauthenticated("not authenticated as a GitHub App"));
        };
        let now = self.clock.now_secs();
        if let Some((t, exp)) = &self.lock().app_token
            && *exp > now + 60
        {
            return Ok(t.clone());
        }
        let jwt = app.jwt(now)?;
        let url = self.url(&format!(
            "/app/installations/{}/access_tokens",
            app.installation_id
        ));
        let resp = self
            .send_raw("POST", &url, Some(format!("Bearer {jwt}")), None, &[])
            .await?;
        if resp.status == 401 || resp.status == 403 || resp.status == 404 {
            return Err(unauthenticated(format!(
                "GitHub refused the App JWT for installation {} (HTTP {})",
                app.installation_id, resp.status
            )));
        }
        if resp.status != 201 && resp.status != 200 {
            return Err(unavailable(format!(
                "installation token exchange failed with HTTP {}",
                resp.status
            )));
        }
        let v = resp.json()?;
        let token = v["token"]
            .as_str()
            .ok_or_else(|| bad_json("installation token", "no `token`"))?
            .to_string();
        let exp = v["expires_at"]
            .as_str()
            .and_then(parse_rfc3339)
            .unwrap_or(now + 3000);
        self.lock().app_token = Some((token.clone(), exp));
        Ok(token)
    }

    async fn send_raw(
        &self,
        method: &str,
        url: &str,
        authorization: Option<String>,
        body: Option<Vec<u8>>,
        extra: &[(&str, String)],
    ) -> Result<Response> {
        let mut b = http::Request::builder()
            .method(method)
            .uri(url)
            .header("user-agent", "bee-github")
            .header("x-github-api-version", "2022-11-28");
        let mut has_accept = false;
        for (k, v) in extra {
            has_accept |= k.eq_ignore_ascii_case("accept");
            b = b.header(*k, v);
        }
        if !has_accept {
            b = b.header("accept", "application/vnd.github+json");
        }
        if let Some(a) = authorization {
            b = b.header("authorization", a);
        }
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let req = b
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|e| invalid("invalid_url", format!("bad request for {url}: {e}")))?;
        self.lock().stats.requests += 1;
        let fut = self.http.request(req);
        let resp = tokio::time::timeout(self.cfg.timeout, fut)
            .await
            .map_err(|_| unavailable(format!("{method} {url}: timed out")))?
            .map_err(|e| unavailable(format!("{method} {url}: {e}")))?;
        let (parts, body) = resp.into_parts();
        let body = body
            .collect()
            .await
            .map_err(|e| unavailable(format!("{method} {url}: reading body: {e}")))?
            .to_bytes();
        let mut headers = BTreeMap::new();
        for (k, v) in &parts.headers {
            if let Ok(s) = v.to_str() {
                headers
                    .entry(k.as_str().to_ascii_lowercase())
                    .and_modify(|cur: &mut String| {
                        cur.push_str(", ");
                        cur.push_str(s);
                    })
                    .or_insert_with(|| s.to_string());
            }
        }
        Ok(Response {
            status: parts.status.as_u16(),
            headers,
            body,
        })
    }

    async fn wait(&self, secs: u64, secondary: bool) -> Result<()> {
        if secs > self.cfg.max_wait_secs {
            return Err(rate_limited(
                format!(
                    "GitHub rate limit: next request allowed in {secs}s, above the {}s wait limit; resume later",
                    self.cfg.max_wait_secs
                ),
                secs,
            ));
        }
        {
            let mut i = self.lock();
            if secondary {
                i.stats.secondary_limit_waits += 1;
            } else {
                i.stats.rate_limit_waits += 1;
            }
            i.stats.waited_secs += secs;
        }
        self.clock.sleep(Duration::from_secs(secs)).await;
        Ok(())
    }

    fn note_rate(&self, r: &Response) {
        let num = |k: &str| r.header(k).and_then(|v| v.trim().parse::<u64>().ok());
        let mut i = self.lock();
        if let Some(v) = num("x-ratelimit-limit") {
            i.rate.limit = Some(v);
        }
        if let Some(v) = num("x-ratelimit-remaining") {
            i.rate.remaining = Some(v);
        }
        if let Some(v) = num("x-ratelimit-reset") {
            i.rate.reset = Some(v);
        }
    }

    /// Sends a request with rate-limit handling and retries; `extra` adds headers.
    async fn send(
        &self,
        method: &str,
        url: &str,
        body: Body<'_>,
        extra: &[(&str, String)],
    ) -> Result<Response> {
        let bytes = match body {
            Body::Empty => None,
            Body::Json(v) => Some(serde_json::to_vec(v).unwrap_or_default()),
        };
        let mut secondary_backoff = 60u64;
        let mut refreshed_app_token = false;
        let mut attempt = 0u32;
        loop {
            // Primary limit: wait for the reset when the budget is spent.
            let (remaining, reset) = {
                let i = self.lock();
                (i.rate.remaining, i.rate.reset)
            };
            if remaining == Some(0)
                && let Some(reset) = reset
            {
                let now = self.clock.now_secs();
                if reset >= now {
                    self.wait(reset - now + 1, false).await?;
                }
                self.lock().rate.remaining = None;
            }
            let auth = self.authorization().await?;
            let resp = match self.send_raw(method, url, auth, bytes.clone(), extra).await {
                Ok(r) => r,
                Err(e) if attempt < self.cfg.max_retries => {
                    attempt += 1;
                    self.lock().stats.retries += 1;
                    let _ = e;
                    self.clock
                        .sleep(Duration::from_secs(1u64 << attempt.min(6)))
                        .await;
                    continue;
                }
                Err(e) => return Err(e),
            };
            self.note_rate(&resp);
            match resp.status {
                200..=299 | 304 | 301 | 302 | 303 | 307 | 308 => return Ok(resp),
                401 if matches!(self.credential, Credential::App(_)) && !refreshed_app_token => {
                    // The installation token expired early or was revoked: exchange again.
                    refreshed_app_token = true;
                    self.lock().app_token = None;
                    continue;
                }
                401 => {
                    return Err(unauthenticated(format!(
                        "GitHub rejected the credential for {method} {url}"
                    )));
                }
                403 | 429 => {
                    if attempt >= self.cfg.max_retries {
                        return Err(rate_limited(
                            format!("{method} {url}: still rate limited after {attempt} retries"),
                            secondary_backoff,
                        ));
                    }
                    attempt += 1;
                    if let Some(ra) = resp
                        .header("retry-after")
                        .and_then(|v| v.trim().parse::<u64>().ok())
                    {
                        self.wait(ra, true).await?;
                        continue;
                    }
                    if resp.header("x-ratelimit-remaining") == Some("0") {
                        let reset = resp
                            .header("x-ratelimit-reset")
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(0);
                        let now = self.clock.now_secs();
                        self.wait(reset.saturating_sub(now) + 1, false).await?;
                        self.lock().rate.remaining = None;
                        continue;
                    }
                    let text = String::from_utf8_lossy(&resp.body).to_ascii_lowercase();
                    if text.contains("secondary rate limit") || text.contains("abuse") {
                        self.wait(secondary_backoff, true).await?;
                        secondary_backoff = (secondary_backoff * 2).min(900);
                        continue;
                    }
                    return Err(denied(format!(
                        "GitHub denied {method} {url}: {}",
                        message_of(&resp)
                    )));
                }
                404 | 410 => {
                    return Err(not_found(format!(
                        "GitHub has no {url} (or the credential cannot see it)"
                    )));
                }
                500..=599 if attempt < self.cfg.max_retries => {
                    attempt += 1;
                    self.lock().stats.retries += 1;
                    self.clock
                        .sleep(Duration::from_secs(1u64 << attempt.min(6)))
                        .await;
                }
                s => {
                    return Err(unavailable(format!(
                        "{method} {url}: HTTP {s}: {}",
                        message_of(&resp)
                    )));
                }
            }
        }
    }

    /// `GET` a JSON document.
    pub async fn get_json(&self, path: &str) -> Result<Value> {
        self.send("GET", &self.url(path), Body::Empty, &[])
            .await?
            .json()
    }

    /// `GET` and decode.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let v = self.get_json(path).await?;
        serde_json::from_value(v).map_err(|e| bad_json(path, e))
    }

    /// `POST` JSON.
    pub async fn post_json(&self, path: &str, body: &Value) -> Result<Value> {
        let r = self
            .send("POST", &self.url(path), Body::Json(body), &[])
            .await?;
        if r.body.is_empty() {
            return Ok(Value::Null);
        }
        r.json()
    }

    /// `PATCH` JSON.
    pub async fn patch_json(&self, path: &str, body: &Value) -> Result<Value> {
        let r = self
            .send("PATCH", &self.url(path), Body::Json(body), &[])
            .await?;
        if r.body.is_empty() {
            return Ok(Value::Null);
        }
        r.json()
    }

    /// One page of a list endpoint (`url` absolute or a path).
    pub async fn get_page(&self, url: &str) -> Result<Page> {
        let resp = self.send("GET", &self.url(url), Body::Empty, &[]).await?;
        page_of(&resp, false)
    }

    /// One page with `If-None-Match`; a `304` serves the stored body and costs no quota.
    pub async fn get_conditional(&self, url: &str) -> Result<Page> {
        let url = self.url(url);
        let stored = self.lock().etags.get(&url).cloned();
        let extra: Vec<(&str, String)> = stored
            .as_ref()
            .map(|(etag, _)| vec![("if-none-match", etag.clone())])
            .unwrap_or_default();
        let resp = self.send("GET", &url, Body::Empty, &extra).await?;
        if resp.status == 304
            && let Some((etag, body)) = stored
        {
            self.lock().stats.not_modified += 1;
            let cached = Response {
                status: 200,
                headers: resp.headers.clone(),
                body: Bytes::from(body),
            };
            let mut page = page_of(&cached, true)?;
            page.next = link_next(resp.header("link"));
            let _ = etag;
            return Ok(page);
        }
        if let Some(etag) = resp.header("etag") {
            let body = String::from_utf8_lossy(&resp.body).into_owned();
            self.lock()
                .etags
                .insert(url.clone(), (etag.to_string(), body));
        }
        page_of(&resp, false)
    }

    /// Every item of a list endpoint, following `Link: rel="next"`.
    pub async fn get_all(&self, path: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut next = Some(self.list_url(path));
        while let Some(url) = next {
            let page = self.get_page(&url).await?;
            out.extend(page.items);
            next = page.next;
        }
        Ok(out)
    }

    /// A GraphQL query. GraphQL `errors` fail the call, except that a `RATE_LIMITED` error
    /// waits for the reset and retries.
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let url = self.cfg.graphql_url.clone();
        for _ in 0..=self.cfg.max_retries {
            let v = self
                .send("POST", &url, Body::Json(&body), &[])
                .await?
                .json()?;
            if let Some(errs) = v.get("errors").and_then(Value::as_array)
                && !errs.is_empty()
            {
                if errs
                    .iter()
                    .any(|e| e["type"].as_str() == Some("RATE_LIMITED"))
                {
                    let reset = self.rate().reset.unwrap_or(0);
                    let now = self.clock.now_secs();
                    self.wait(reset.saturating_sub(now).max(1), false).await?;
                    continue;
                }
                let msgs: Vec<&str> = errs.iter().filter_map(|e| e["message"].as_str()).collect();
                if v.get("data").is_some_and(|d| !d.is_null()) {
                    // Partial data (for example a deleted commit in a timeline): the caller
                    // reads what is there.
                    return Ok(v["data"].clone());
                }
                return Err(unavailable(format!("GraphQL error: {}", msgs.join("; "))));
            }
            return Ok(v["data"].clone());
        }
        Err(rate_limited("GraphQL rate limit did not reset", 60))
    }

    /// Downloads bytes, following redirects (to a CDN or object store, where the GitHub
    /// credential is not sent). `accept` overrides the `Accept` header.
    pub async fn download(&self, url: &str, accept: Option<&str>) -> Result<Vec<u8>> {
        let mut url = self.url(url);
        let origin = origin_of(&url);
        for _ in 0..5 {
            let extra: Vec<(&str, String)> = accept
                .map(|a| vec![("accept", a.to_string())])
                .unwrap_or_default();
            let resp = if origin_of(&url) == origin {
                self.send("GET", &url, Body::Empty, &extra).await?
            } else {
                let r = self.send_raw("GET", &url, None, None, &extra).await?;
                if !(200..400).contains(&r.status) {
                    return Err(unavailable(format!("GET {url}: HTTP {}", r.status)));
                }
                r
            };
            if matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
                let loc = resp
                    .header("location")
                    .ok_or_else(|| unavailable(format!("redirect from {url} without Location")))?;
                url = if loc.starts_with('/') {
                    format!("{}{loc}", origin_of(&url))
                } else {
                    loc.to_string()
                };
                continue;
            }
            return Ok(resp.body.to_vec());
        }
        Err(unavailable(format!("too many redirects from {url}")))
    }

    /// `POST` raw JSON to an absolute URL with extra headers, returning the response (the
    /// LFS batch API, which is not under the REST base).
    pub async fn post_raw(
        &self,
        url: &str,
        body: &Value,
        extra: &[(&str, String)],
        basic_auth: bool,
    ) -> Result<Response> {
        let auth = if basic_auth {
            self.git_token().await?.map(|t| {
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{t}"))
                )
            })
        } else {
            self.authorization().await?
        };
        let r = self
            .send_raw(
                "POST",
                url,
                auth,
                Some(serde_json::to_vec(body).unwrap_or_default()),
                extra,
            )
            .await?;
        self.note_rate(&r);
        Ok(r)
    }

    /// `GET` an absolute URL with extra headers and no GitHub credential unless `href`
    /// is on the API origin (LFS downloads carry their own headers).
    pub async fn get_raw(&self, url: &str, extra: &[(&str, String)]) -> Result<Response> {
        self.send_raw("GET", url, None, None, extra).await
    }

    /// The rate limit from `GET /rate_limit` (free of quota).
    pub async fn fetch_rate_limit(&self) -> Result<RateLimit> {
        let v = self.get_json("/rate_limit").await?;
        let core = &v["resources"]["core"];
        let rl = RateLimit {
            limit: core["limit"].as_u64(),
            remaining: core["remaining"].as_u64(),
            reset: core["reset"].as_u64(),
        };
        Ok(rl)
    }
}

fn page_of(resp: &Response, not_modified: bool) -> Result<Page> {
    let v = resp.json()?;
    let items = match v {
        Value::Array(a) => a,
        // Search-style envelopes.
        Value::Object(mut m) => match m.remove("items") {
            Some(Value::Array(a)) => a,
            _ => vec![Value::Object(m)],
        },
        other => vec![other],
    };
    Ok(Page {
        items,
        next: link_next(resp.header("link")),
        not_modified,
    })
}

/// The `rel="next"` URL of a `Link` header.
pub fn link_next(link: Option<&str>) -> Option<String> {
    for part in link?.split(',') {
        let mut it = part.split(';');
        let url = it.next()?.trim();
        if it.any(|p| p.trim() == "rel=\"next\"") {
            return Some(
                url.trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string(),
            );
        }
    }
    None
}

fn origin_of(url: &str) -> String {
    let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
    let rest = &url[scheme_end..];
    let host_end = rest.find('/').unwrap_or(rest.len());
    url[..scheme_end + host_end].to_string()
}

fn message_of(r: &Response) -> String {
    serde_json::from_slice::<Value>(&r.body)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(&r.body).chars().take(200).collect())
}

/// Decodes a list of JSON items into `T`, naming the endpoint on failure.
pub fn decode_items<T: DeserializeOwned>(what: &str, items: Vec<Value>) -> Result<Vec<T>> {
    items
        .into_iter()
        .map(|v| serde_json::from_value(v).map_err(|e| bad_json(what, e)))
        .collect()
}

/// A value from a nested JSON path, decoded.
pub fn decode<T: DeserializeOwned + Default>(what: &str, v: &Value) -> Result<T> {
    if v.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(v.clone()).map_err(|e| bad_json(what, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_header_next() {
        let l = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#;
        assert_eq!(
            link_next(Some(l)).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        assert_eq!(link_next(Some(r#"<https://a/x?page=1>; rel="prev""#)), None);
        assert_eq!(link_next(None), None);
    }

    #[test]
    fn origins() {
        assert_eq!(
            origin_of("https://api.github.com/a/b"),
            "https://api.github.com"
        );
        assert_eq!(origin_of("http://127.0.0.1:9/x"), "http://127.0.0.1:9");
    }
}
