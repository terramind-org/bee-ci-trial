//! The HTTP edge: routing, authentication, rate limits, API versions, ETags, pagination and
//! the response envelope shared by every REST endpoint and GraphQL.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body as AxBody;
use axum::extract::Request;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bee_forge::auth::Caller;
use bee_proto::v1::{ErrorReason, PageRequest, PageResponse, Principal};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;

use crate::error::{ApiError, ApiResult};
use crate::hub::{API_VERSION, Compat, RateState};
use crate::ports::Actor;

/// A boxed handler future.
pub(crate) type BoxFut = Pin<Box<dyn Future<Output = ApiResult<Reply>> + Send>>;
/// A REST handler.
pub(crate) type Handler = fn(Arc<Ctx>, Params) -> BoxFut;

/// Scopes reported in `X-OAuth-Scopes`; what a token may really do is its bee scope.
const SCOPES: &str = "admin:org, admin:repo_hook, delete_repo, gist, notifications, read:org, repo, user, workflow, write:packages";

/// Largest number of items gathered from forge pages for one GitHub list.
pub(crate) const LIST_CAP: usize = 10_000;

/// A response body.
#[derive(Clone, Debug)]
pub(crate) enum Body {
    /// JSON.
    Json(Value),
    /// Raw bytes with a content type.
    Raw(Bytes, String),
    /// Nothing.
    Empty,
}

/// A handler's answer.
#[derive(Clone, Debug)]
pub(crate) struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Reply {
    /// `200` with JSON.
    pub fn json(v: Value) -> Reply {
        Reply {
            status: 200,
            headers: Vec::new(),
            body: Body::Json(v),
        }
    }

    /// `201` with JSON.
    pub fn created(v: Value) -> Reply {
        Reply {
            status: 201,
            ..Reply::json(v)
        }
    }

    /// `202` with JSON.
    pub fn accepted(v: Value) -> Reply {
        Reply {
            status: 202,
            ..Reply::json(v)
        }
    }

    /// `204`.
    pub fn no_content() -> Reply {
        Reply {
            status: 204,
            headers: Vec::new(),
            body: Body::Empty,
        }
    }

    /// `302` to `url`.
    pub fn redirect(url: &str) -> Reply {
        Reply {
            status: 302,
            headers: vec![("location".into(), url.to_string())],
            body: Body::Empty,
        }
    }

    /// Raw bytes.
    pub fn raw(bytes: impl Into<Bytes>, content_type: &str) -> Reply {
        Reply {
            status: 200,
            headers: Vec::new(),
            body: Body::Raw(bytes.into(), content_type.to_string()),
        }
    }

    /// Adds a header.
    pub fn header(mut self, k: &str, v: impl Into<String>) -> Reply {
        self.headers.push((k.to_string(), v.into()));
        self
    }

    /// Replaces the status.
    pub fn status(mut self, s: u16) -> Reply {
        self.status = s;
        self
    }
}

/// Path parameters of a matched route.
#[derive(Clone, Debug, Default)]
pub(crate) struct Params(Vec<(&'static str, String)>);

impl Params {
    /// A parameter (empty when absent).
    pub fn get(&self, k: &str) -> &str {
        self.0
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    /// A numeric parameter (`404` when not a number, as GitHub answers).
    pub fn num(&self, k: &str) -> ApiResult<u64> {
        self.get(k).parse().map_err(|_| ApiError::not_found())
    }
}

/// One route segment.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Seg {
    Lit(&'static str),
    Param(&'static str),
    Rest(&'static str),
}

/// A route.
pub(crate) struct Route {
    pub method: Method,
    pub segs: Vec<Seg>,
    pub handler: Handler,
}

impl Route {
    /// A route from `"/repos/:owner/:repo/contents/*path"`.
    pub fn new(method: Method, pattern: &'static str, handler: Handler) -> Route {
        let segs = pattern
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| {
                if let Some(p) = s.strip_prefix(':') {
                    Seg::Param(p)
                } else if let Some(p) = s.strip_prefix('*') {
                    Seg::Rest(p)
                } else {
                    Seg::Lit(s)
                }
            })
            .collect();
        Route {
            method,
            segs,
            handler,
        }
    }

    fn matches(&self, method: &Method, parts: &[&str]) -> Option<Params> {
        let same = self.method == *method || (self.method == Method::GET && method == Method::HEAD);
        if !same {
            return None;
        }
        let mut out = Vec::new();
        for (i, seg) in self.segs.iter().enumerate() {
            match seg {
                Seg::Lit(l) => {
                    if parts.get(i) != Some(l) {
                        return None;
                    }
                }
                Seg::Param(p) => {
                    let v = parts.get(i).filter(|v| !v.is_empty())?;
                    out.push((*p, decode(v)));
                }
                Seg::Rest(p) => {
                    if i >= parts.len() {
                        return None;
                    }
                    let v: Vec<String> = parts[i..].iter().map(|s| decode(s)).collect();
                    out.push((*p, v.join("/")));
                    return Some(Params(out));
                }
            }
        }
        (parts.len() == self.segs.len()).then_some(Params(out))
    }
}

/// Percent-decodes a path segment or query component (`+` is a space in queries only).
pub(crate) fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes a query component.
pub(crate) fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Parses a query string.
pub(crate) fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (decode(&k.replace('+', " ")), decode(&v.replace('+', " ")))
        })
        .collect()
}

/// Everything a handler knows about its request.
pub(crate) struct Ctx {
    pub hub: Arc<Compat>,
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub token: Option<String>,
    pub caller: Option<Caller>,
    pub origin: String,
    pub prefix: String,
    pub web: String,
    pub request_id: String,
    pub client: String,
}

impl Ctx {
    /// An API URL for `path` (which starts with `/`).
    pub fn api(&self, path: &str) -> String {
        format!("{}{}{}", self.origin, self.prefix, path)
    }

    /// A web URL for `path`.
    pub fn html(&self, path: &str) -> String {
        format!("{}{}", self.web, path)
    }

    /// An uploads URL for `path`.
    pub fn uploads(&self, path: &str) -> String {
        format!("{}/api/uploads{}", self.origin, path)
    }

    /// A query parameter.
    pub fn q(&self, k: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }

    /// A header.
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.get(k).and_then(|v| v.to_str().ok())
    }

    /// Whether the `Accept` header asks for `media` (e.g. `diff`, `raw`).
    pub fn accepts(&self, media: &str) -> bool {
        self.header("accept").is_some_and(|a| {
            a.split(',').any(|t| {
                let t = t.trim();
                t.contains(&format!(".{media}")) || t == format!("application/{media}")
            })
        })
    }

    /// `page` and `per_page` (GitHub's defaults: 1 and 30, at most 100).
    pub fn page(&self) -> (usize, usize) {
        let page = self
            .q("page")
            .and_then(|p| p.parse().ok())
            .filter(|p: &usize| *p >= 1)
            .unwrap_or(1);
        let per = self
            .q("per_page")
            .and_then(|p| p.parse().ok())
            .filter(|p: &usize| *p >= 1)
            .unwrap_or(30)
            .min(100);
        (page, per)
    }

    /// The JSON body (`{}` when empty).
    pub fn body_json(&self) -> ApiResult<Value> {
        if self.body.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(Value::Object(Default::default()));
        }
        serde_json::from_slice(&self.body)
            .map_err(|_| ApiError::bad_request("Problems parsing JSON"))
    }

    /// The verified caller (`401` without one).
    pub fn caller(&self) -> ApiResult<&Caller> {
        self.caller.as_ref().ok_or_else(ApiError::unauthenticated)
    }

    /// The caller's principal id (empty when anonymous).
    pub fn me(&self) -> &str {
        self.caller
            .as_ref()
            .map(|c| c.cx.actor_id.as_str())
            .unwrap_or("")
    }

    /// The caller as a VCS writer.
    pub async fn actor(&self) -> ApiResult<Actor> {
        let c = self.caller()?;
        let token = self.token.clone().unwrap_or_default();
        let p = self.principal(&c.cx.actor_id).await;
        let login = self.login(&p).await;
        let (_, emails) = self
            .hub
            .forge
            .hooks
            .directory
            .identities(&c.cx.actor_id)
            .await;
        Ok(Actor {
            principal_id: c.cx.actor_id.clone(),
            chain: c.cx.chain.clone(),
            token,
            name: if p.display.is_empty() {
                login.clone()
            } else {
                p.display.clone()
            },
            email: emails
                .first()
                .cloned()
                .unwrap_or_else(|| format!("{login}@users.noreply.beehive")),
        })
    }

    /// A tonic request carrying the caller's bee token.
    pub fn req<T>(&self, msg: T) -> tonic::Request<T> {
        let mut r = tonic::Request::new(msg);
        let md = r.metadata_mut();
        if let Some(t) = &self.token
            && let Ok(v) = format!("Bearer {t}").parse()
        {
            md.insert("authorization", v);
        }
        if let Ok(v) = self.request_id.parse() {
            md.insert("x-request-id", v);
        }
        if let Some(ua) = self.header("user-agent")
            && let Ok(v) = ua.parse()
        {
            md.insert("user-agent", v);
        }
        if !self.client.is_empty()
            && let Ok(v) = self.client.parse()
        {
            md.insert("x-forwarded-for", v);
        }
        r
    }

    /// A principal by id (a bare record when the directory does not know it).
    pub async fn principal(&self, id: &str) -> Principal {
        self.hub
            .forge
            .hooks
            .directory
            .get(id)
            .await
            .unwrap_or_else(|| Principal {
                id: id.to_string(),
                ..Default::default()
            })
    }

    /// The GitHub login of a principal: its first login, else its id.
    pub async fn login(&self, p: &Principal) -> String {
        if p.id.is_empty() {
            return "ghost".into();
        }
        let (logins, _) = self.hub.forge.hooks.directory.identities(&p.id).await;
        logins.into_iter().next().unwrap_or_else(|| p.id.clone())
    }

    /// The principal behind a login (or a principal id), `404` when unknown.
    pub async fn by_login(&self, login: &str) -> ApiResult<Principal> {
        let dir = &self.hub.forge.hooks.directory;
        if let Some(p) = dir.by_login(login).await {
            return Ok(p);
        }
        dir.get(login).await.ok_or_else(ApiError::not_found)
    }

    /// The principal id behind a login, `422` naming `field` when unknown.
    pub async fn id_of_login(&self, login: &str, field: &str) -> ApiResult<String> {
        match self.by_login(login).await {
            Ok(p) => Ok(p.id),
            Err(_) => Err(ApiError::validation(
                "User",
                field,
                "invalid",
                &format!("Could not resolve to a User with the login of '{login}'."),
            )),
        }
    }

    /// One GitHub page of `items` with its `Link` header.
    pub fn paginate(&self, items: Vec<Value>) -> Reply {
        let total = items.len();
        let (page, per) = self.page();
        let out: Vec<Value> = items.into_iter().skip((page - 1) * per).take(per).collect();
        let mut r = Reply::json(Value::Array(out));
        if let Some(link) = self.link(page, per, total) {
            r = r.header("link", link);
        }
        r
    }

    /// A page already cut from a list of `total` items, with its `Link` header.
    pub fn paged(&self, page_items: Vec<Value>, total: usize) -> Reply {
        let (page, per) = self.page();
        let mut r = Reply::json(Value::Array(page_items));
        if let Some(link) = self.link(page, per, total) {
            r = r.header("link", link);
        }
        r
    }

    /// One page of `items` wrapped as `{"total_count": n, key: [...]}` (Actions, search).
    pub fn paginate_wrapped(&self, key: &str, items: Vec<Value>, extra: Value) -> Reply {
        let total = items.len();
        let (page, per) = self.page();
        let out: Vec<Value> = items.into_iter().skip((page - 1) * per).take(per).collect();
        let mut body = serde_json::json!({"total_count": total});
        if let (Value::Object(m), Value::Object(e)) = (&mut body, extra) {
            m.extend(e);
        }
        body[key] = Value::Array(out);
        let mut r = Reply::json(body);
        if let Some(link) = self.link(page, per, total) {
            r = r.header("link", link);
        }
        r
    }

    fn link(&self, page: usize, per: usize, total: usize) -> Option<String> {
        let last = total.div_ceil(per).max(1);
        if last <= 1 && page <= 1 {
            return None;
        }
        let url = |p: usize| {
            let mut q: Vec<String> = self
                .query
                .iter()
                .filter(|(k, _)| k != "page")
                .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
                .collect();
            q.push(format!("page={p}"));
            format!("<{}?{}>", self.api(&self.path), q.join("&"))
        };
        let mut parts = Vec::new();
        if page > 1 {
            parts.push(format!("{}; rel=\"prev\"", url((page - 1).min(last))));
        }
        if page < last {
            parts.push(format!("{}; rel=\"next\"", url(page + 1)));
            parts.push(format!("{}; rel=\"last\"", url(last)));
        }
        if page > 1 {
            parts.push(format!("{}; rel=\"first\"", url(1)));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// Gathers every page of a forge list (up to [`LIST_CAP`] items).
pub(crate) async fn all_pages<T, F, Fut>(mut f: F) -> ApiResult<Vec<T>>
where
    F: FnMut(PageRequest) -> Fut,
    Fut: Future<Output = ApiResult<(Vec<T>, Option<PageResponse>)>>,
{
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let (items, next) = f(PageRequest {
            page_size: 500,
            page_token: token.clone(),
        })
        .await?;
        let got = items.len();
        out.extend(items);
        token = next.map(|p| p.next_page_token).unwrap_or_default();
        if token.is_empty() || got == 0 || out.len() >= LIST_CAP {
            return Ok(out);
        }
    }
}

fn parse_auth(v: &str) -> Option<String> {
    let v = v.trim();
    let (scheme, rest) = v.split_once(' ')?;
    let rest = rest.trim();
    match scheme.to_ascii_lowercase().as_str() {
        "token" | "bearer" => Some(rest.to_string()),
        "basic" => {
            let raw = STANDARD.decode(rest).ok()?;
            let text = String::from_utf8(raw).ok()?;
            let (user, pass) = text.split_once(':').unwrap_or((&text, ""));
            Some(if pass.is_empty() { user } else { pass }.to_string())
        }
        _ => None,
    }
}

fn origin_of(hub: &Compat, headers: &HeaderMap, uri: &http::Uri) -> (String, String) {
    let origin = match &hub.config.public_url {
        Some(u) => u.trim_end_matches('/').to_string(),
        None => {
            let host = headers
                .get("x-forwarded-host")
                .or_else(|| headers.get("host"))
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .or_else(|| uri.authority().map(|a| a.to_string()))
                .unwrap_or_else(|| "localhost".into());
            let scheme = headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .or_else(|| uri.scheme_str().map(str::to_string))
                .unwrap_or_else(|| "http".into());
            format!("{scheme}://{host}")
        }
    };
    let web = match &hub.config.web_url {
        Some(u) => u.trim_end_matches('/').to_string(),
        None => match origin.split_once("://") {
            Some((s, h)) => format!("{s}://{}", h.strip_prefix("api.").unwrap_or(h)),
            None => origin.clone(),
        },
    };
    (origin, web)
}

/// The response for an error, before common headers.
pub(crate) fn error_reply(e: &ApiError) -> Reply {
    let mut r = Reply {
        status: e.status,
        headers: Vec::new(),
        body: Body::Json(e.body()),
    };
    if let Some(s) = e.retry_after {
        r = r.header("retry-after", s.to_string());
    }
    r
}

/// Builds the request context: body, URLs, credentials. Errors are ready responses.
pub(crate) async fn context(
    hub: Arc<Compat>,
    prefix: &str,
    req: Request,
) -> Result<Arc<Ctx>, Reply> {
    let (parts, body) = req.into_parts();
    let request_id = hub.forge.db.ids().ulid().unwrap_or_else(|_| "0".repeat(26));
    let body = match http_body_util::Limited::new(body, hub.config.max_body_bytes)
        .collect()
        .await
    {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            return Err(error_reply(&ApiError::new(
                413,
                ErrorReason::TooLarge,
                "Request body too large",
            )));
        }
    };
    if let Some(v) = parts
        .headers
        .get("x-github-api-version")
        .and_then(|v| v.to_str().ok())
        && v != API_VERSION
    {
        return Err(error_reply(&ApiError::bad_request(format!(
            "API version {v} is not supported."
        ))));
    }
    let path = parts.uri.path();
    let path = path.strip_prefix(prefix).unwrap_or(path);
    let path = if path.is_empty() { "/" } else { path };
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };
    let query = parse_query(parts.uri.query().unwrap_or(""));
    let (origin, web) = origin_of(&hub, &parts.headers, &parts.uri);
    let client = parts
        .headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    let presented = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| parse_auth(v).ok_or(()));
    let token = match presented {
        Some(Ok(t)) => match hub.ports.tokens.bee_token(&t).await {
            Some(s) => Some(s),
            None => return Err(error_reply(&ApiError::bad_credentials())),
        },
        Some(Err(())) => return Err(error_reply(&ApiError::bad_credentials())),
        None => hub.ports.tokens.anonymous().await,
    };
    let caller = match &token {
        Some(t) => {
            let mut r = tonic::Request::new(());
            if let Ok(v) = format!("Bearer {t}").parse() {
                r.metadata_mut().insert("authorization", v);
            }
            if let Ok(v) = request_id.parse() {
                r.metadata_mut().insert("x-request-id", v);
            }
            match hub.forge.authn.authenticate(&r) {
                Ok(c) => Some(c),
                Err(_) => return Err(error_reply(&ApiError::bad_credentials())),
            }
        }
        None => None,
    };
    Ok(Arc::new(Ctx {
        hub,
        method: parts.method,
        path: path.to_string(),
        query,
        headers: parts.headers,
        body,
        token,
        caller,
        origin,
        prefix: prefix.to_string(),
        web,
        request_id,
        client,
    }))
}

/// The rate-limit key of a request: the delegation root, else the client address.
pub(crate) fn rate_key(cx: &Ctx) -> String {
    match &cx.caller {
        Some(c) => format!("p:{}", c.cx.root()),
        None => format!(
            "ip:{}",
            if cx.client.is_empty() {
                "unknown"
            } else {
                &cx.client
            }
        ),
    }
}

/// Consumes one request of `resource`; the limit when exhausted.
pub(crate) fn take_rate(cx: &Ctx, resource: &'static str) -> Result<RateState, RateState> {
    let hub = &cx.hub;
    let limit = if cx.caller.is_none() {
        hub.config.anonymous_limit
    } else {
        match resource {
            "search" => hub.config.search_limit,
            "graphql" => hub.config.graphql_limit,
            _ => hub.config.core_limit,
        }
    };
    let now = hub.ports.clock.now_ms() / 1000;
    hub.limiter.take(&rate_key(cx), resource, limit, now)
}

/// The `403` GitHub answers when a rate limit is exhausted.
pub(crate) fn rate_error(cx: &Ctx, state: &RateState) -> ApiError {
    let who = match &cx.caller {
        Some(c) => format!("user ID {}", crate::ids::num("user", c.cx.root())),
        None => cx.client.clone(),
    };
    let mut e = ApiError::new(
        403,
        ErrorReason::ResourceExhausted,
        format!("API rate limit exceeded for {who}."),
    );
    let now = cx.hub.ports.clock.now_ms() / 1000;
    e.retry_after = Some(state.reset.saturating_sub(now).max(1));
    e
}

/// Turns a reply into an HTTP response with GitHub's common headers, ETag and `304`.
pub(crate) fn finish(
    cx: Option<&Ctx>,
    reply: Reply,
    rate: Option<(RateState, &'static str)>,
) -> Response {
    let mut reply = reply;
    let mut rate = rate;
    let mut etag = None;
    if let (Some(cx), Body::Json(v)) = (cx, &reply.body)
        && reply.status == 200
        && (cx.method == Method::GET || cx.method == Method::HEAD)
    {
        let bytes = serde_json::to_vec(v).unwrap_or_default();
        let tag = format!("W/\"{}\"", &blake3::hash(&bytes).to_hex()[..32]);
        let matched = cx.header("if-none-match").is_some_and(|inm| {
            inm.split(',').any(|t| {
                let t = t.trim();
                t == "*" || t.trim_start_matches("W/") == tag.trim_start_matches("W/")
            })
        });
        if matched {
            if let Some((state, res)) = rate.as_mut() {
                cx.hub.limiter.refund(&rate_key(cx), res);
                state.used = state.used.saturating_sub(1);
            }
            reply = Reply {
                status: 304,
                headers: Vec::new(),
                body: Body::Empty,
            };
        }
        etag = Some(tag);
    }
    let (bytes, ctype) = match reply.body {
        Body::Json(v) => (
            Bytes::from(serde_json::to_vec(&v).unwrap_or_default()),
            Some("application/json; charset=utf-8".to_string()),
        ),
        Body::Raw(b, t) => (b, Some(t)),
        Body::Empty => (Bytes::new(), None),
    };
    let head_only = cx.is_some_and(|c| c.method == Method::HEAD);
    let mut resp = Response::new(if head_only {
        AxBody::empty()
    } else {
        AxBody::from(bytes)
    });
    *resp.status_mut() = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::OK);
    let h = resp.headers_mut();
    let mut put = |k: &str, v: &str| {
        if let (Ok(k), Ok(v)) = (
            http::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            h.append(k, v);
        }
    };
    if let Some(t) = &ctype {
        put("content-type", t);
    }
    put("x-github-media-type", "github.v3; format=json");
    put("x-github-api-version-selected", API_VERSION);
    put("vary", "Accept, Authorization, Cookie, X-GitHub-OTP");
    put("access-control-allow-origin", "*");
    put(
        "access-control-expose-headers",
        "ETag, Link, Location, Retry-After, X-GitHub-OTP, X-RateLimit-Limit, X-RateLimit-Remaining, X-RateLimit-Used, X-RateLimit-Resource, X-RateLimit-Reset, X-OAuth-Scopes, X-Accepted-OAuth-Scopes, X-GitHub-Media-Type, X-GitHub-SSO, X-GitHub-Request-Id",
    );
    if let Some(cx) = cx {
        put("x-github-request-id", &cx.request_id);
        if cx.caller.is_some() {
            put("x-oauth-scopes", SCOPES);
            put("x-accepted-oauth-scopes", "");
        }
    }
    if let Some((s, res)) = rate {
        put("x-ratelimit-limit", &s.limit.to_string());
        put("x-ratelimit-remaining", &s.remaining().to_string());
        put("x-ratelimit-used", &s.used.to_string());
        put("x-ratelimit-reset", &s.reset.to_string());
        put("x-ratelimit-resource", res);
    }
    if let Some(t) = &etag {
        put("etag", t);
    }
    for (k, v) in &reply.headers {
        put(k, v);
    }
    resp
}

/// Serves one REST request: authenticate, rate-limit, route, render.
pub(crate) async fn dispatch(hub: Arc<Compat>, prefix: &'static str, req: Request) -> Response {
    let cx = match context(hub, prefix, req).await {
        Ok(cx) => cx,
        Err(reply) => return finish(None, reply, None),
    };
    let parts: Vec<&str> = cx.path.split('/').filter(|s| !s.is_empty()).collect();
    let found = crate::rest::routes()
        .iter()
        .find_map(|r| r.matches(&cx.method, &parts).map(|p| (r.handler, p)));
    let resource: &'static str = if cx.path.starts_with("/search/") {
        "search"
    } else {
        "core"
    };
    let free = cx.path == "/rate_limit";
    let rate = if free {
        let now = cx.hub.ports.clock.now_ms() / 1000;
        let limit = if cx.caller.is_some() {
            cx.hub.config.core_limit
        } else {
            cx.hub.config.anonymous_limit
        };
        Ok(cx.hub.limiter.peek(&rate_key(&cx), "core", limit, now))
    } else {
        take_rate(&cx, resource)
    };
    let state = match rate {
        Ok(s) => s,
        Err(s) => {
            let e = rate_error(&cx, &s);
            return finish(Some(&cx), error_reply(&e), Some((s, resource)));
        }
    };
    let reply = match found {
        Some((h, params)) => match h(cx.clone(), params).await {
            Ok(r) => r,
            Err(e) => error_reply(&e),
        },
        None => error_reply(&ApiError::not_found()),
    };
    finish(Some(&cx), reply, Some((state, resource)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(_: Arc<Ctx>, _: Params) -> BoxFut {
        Box::pin(async { Ok(Reply::no_content()) })
    }

    #[test]
    fn routes_match_params_and_rest() {
        let r = Route::new(Method::GET, "/repos/:owner/:repo/contents/*path", h);
        let p = r
            .matches(&Method::GET, &["repos", "o", "r", "contents", "a", "b%20c"])
            .unwrap();
        assert_eq!(p.get("owner"), "o");
        assert_eq!(p.get("path"), "a/b c");
        assert!(
            r.matches(&Method::GET, &["repos", "o", "r", "contents"])
                .is_none()
        );
        assert!(
            r.matches(&Method::POST, &["repos", "o", "r", "contents", "x"])
                .is_none()
        );
        let r = Route::new(Method::GET, "/user", h);
        assert!(r.matches(&Method::HEAD, &["user"]).is_some());
        assert!(r.matches(&Method::GET, &["user", "x"]).is_none());
    }

    #[test]
    fn query_and_auth_parse() {
        let q = parse_query("q=is%3Aopen+label%3Abug&per_page=5");
        assert_eq!(q[0], ("q".into(), "is:open label:bug".into()));
        assert_eq!(parse_auth("token abc"), Some("abc".into()));
        assert_eq!(parse_auth("Bearer abc"), Some("abc".into()));
        assert_eq!(
            parse_auth(&format!("Basic {}", STANDARD.encode("x-access-token:tok"))),
            Some("tok".into())
        );
        assert_eq!(parse_auth("Digest x"), None);
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
    }
}
