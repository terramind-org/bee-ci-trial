//! The HTTP surface: git smart HTTP and the LFS batch API, as an axum router the server merges
//! into its single listener.
//!
//! | Route | Use |
//! |---|---|
//! | `GET /<owner>/<repo>.git/info/refs?service=git-upload-pack` | fetch advertisement (v2 when `Git-Protocol: version=2`, else v0/v1) |
//! | `POST /<owner>/<repo>.git/git-upload-pack` | `ls-refs` / `fetch` (v2), negotiation and pack (v0/v1) |
//! | `GET /<owner>/<repo>.git/info/refs?service=git-receive-pack` | push advertisement |
//! | `POST /<owner>/<repo>.git/git-receive-pack` | push |
//! | `POST /<owner>/<repo>.git/info/lfs/objects/batch` | LFS batch API |
//! | `PUT`/`GET /<owner>/<repo>.git/info/lfs/objects/<oid>?…&sig=` | signed LFS transfers |
//!
//! The `.git` suffix is optional. Credentials are HTTP Basic (the token as password, any user
//! name; or the token as user name with an empty password) or `Authorization: Bearer <token>`.
//! A request without a usable credential gets `401` with a `WWW-Authenticate: Basic` challenge,
//! which makes git ask its credential helper.

use std::io::Read;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use http::{HeaderMap, HeaderValue, StatusCode, header};

use crate::advertise;
use crate::backend::{Access, GitBackend, OpenRequest, RepoSession, Via};
use crate::cache::{CacheUse, PackCache};
use crate::error::{GitError, GitResult};
use crate::gitmap::GitView;
use crate::lfs::{self, BatchRequest, LFS_MEDIA_TYPE, Signed};
use crate::locks;
use crate::metrics::GitMetrics;
use crate::pktline;
use crate::receive;
use crate::upload;

/// Gateway settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GatewayConfig {
    /// Total bytes of packs the pack cache keeps (0 disables it).
    pub pack_cache_bytes: usize,
    /// Largest request body accepted (a push's pack, after gzip decoding).
    pub max_request_bytes: usize,
}

impl Default for GatewayConfig {
    fn default() -> GatewayConfig {
        GatewayConfig {
            pack_cache_bytes: 256 << 20,
            max_request_bytes: 1 << 30,
        }
    }
}

/// The git gateway: a backend, a pack cache, metrics and the routes.
pub struct GitGateway {
    pub(crate) backend: Arc<dyn GitBackend>,
    pub(crate) cache: PackCache,
    pub(crate) cfg: GatewayConfig,
    pub(crate) metrics: Arc<GitMetrics>,
}

impl GitGateway {
    /// A gateway over `backend`.
    pub fn new(backend: Arc<dyn GitBackend>, cfg: GatewayConfig) -> Arc<GitGateway> {
        GitGateway::with_metrics(backend, cfg, Arc::new(GitMetrics::new()))
    }

    /// A gateway over `backend` counting into `metrics` (the server's registry).
    pub fn with_metrics(
        backend: Arc<dyn GitBackend>,
        cfg: GatewayConfig,
        metrics: Arc<GitMetrics>,
    ) -> Arc<GitGateway> {
        Arc::new(GitGateway {
            backend,
            cache: PackCache::new(cfg.pack_cache_bytes),
            cfg,
            metrics,
        })
    }

    /// The gateway's metrics.
    pub fn metrics(&self) -> &Arc<GitMetrics> {
        &self.metrics
    }

    /// The pack cache (tests read its statistics).
    pub fn cache(&self) -> &PackCache {
        &self.cache
    }

    /// Builds and caches the pack a stock `git clone` of the repo would ask for, as seen by
    /// `session` ([`upload::prewarm_clone`]); the server calls it after a land so the first
    /// clone of the new tip is a pack-cache hit. True when a pack was built and kept.
    pub async fn prewarm_clone(&self, session: &dyn RepoSession) -> GitResult<bool> {
        if self.cache.capacity() == 0 {
            return Ok(false);
        }
        let format = advertise::repo_format(session).await?;
        let mut view = GitView::new(session.history(), format);
        let refs = advertise::list(session, &mut view).await?;
        upload::prewarm_clone(session, &mut view, &refs, &self.cache).await
    }

    /// The routes, ready to merge into the server's HTTP router.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/{owner}/{repo}/info/refs", get(info_refs))
            .route("/{owner}/{repo}/git-upload-pack", post(upload_pack))
            .route("/{owner}/{repo}/git-receive-pack", post(receive_pack))
            .route("/{owner}/{repo}/info/lfs/objects/batch", post(lfs_batch))
            .route(
                "/{owner}/{repo}/info/lfs/objects/{oid}",
                get(lfs_get).put(lfs_put),
            )
            .route(
                "/{owner}/{repo}/info/lfs/locks",
                get(locks_list).post(locks_create),
            )
            .route("/{owner}/{repo}/info/lfs/locks/verify", post(locks_verify))
            .route(
                "/{owner}/{repo}/info/lfs/locks/{id}/unlock",
                post(locks_unlock),
            )
            .layer(axum::middleware::from_fn_with_state(self.clone(), observe))
            .with_state(self.clone())
    }
}

/// Marks a response whose status is 200 but which carries a protocol error (`ERR` packet).
#[derive(Clone, Copy, Debug)]
struct Failed;

/// The metrics `service` label of a gateway path.
fn service_of(path: &str) -> &'static str {
    if path.ends_with("/info/refs") {
        "info-refs"
    } else if path.ends_with("/git-upload-pack") {
        "upload-pack"
    } else if path.ends_with("/git-receive-pack") {
        "receive-pack"
    } else if path.ends_with("/info/lfs/objects/batch") {
        "lfs-batch"
    } else if path.contains("/info/lfs/objects/") {
        "lfs-transfer"
    } else if path.contains("/info/lfs/locks") {
        "lfs-locks"
    } else {
        "other"
    }
}

/// Counts every request and its duration (`bee_git_requests_total`,
/// `bee_git_request_duration_seconds`).
async fn observe(
    State(gw): Gw,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let service = service_of(req.uri().path());
    let started = std::time::Instant::now();
    let resp = next.run(req).await;
    let ok = resp.status().as_u16() < 400 && resp.extensions().get::<Failed>().is_none();
    gw.metrics
        .request("http", service, ok, started.elapsed().as_secs_f64());
    resp
}

type Gw = State<Arc<GitGateway>>;

fn repo_name(repo: &str) -> &str {
    repo.strip_suffix(".git").unwrap_or(repo)
}

/// The credential of a request: HTTP Basic (password, or the user name when the password is
/// empty) or Bearer.
pub fn credential(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, rest) = v.split_once(' ')?;
    let rest = rest.trim();
    match scheme.to_ascii_lowercase().as_str() {
        "basic" => {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(rest)
                .ok()?;
            let text = String::from_utf8(raw).ok()?;
            let (user, pass) = text.split_once(':').unwrap_or((&text, ""));
            let t = if pass.is_empty() { user } else { pass };
            (!t.is_empty()).then(|| t.to_string())
        }
        "bearer" | "token" => (!rest.is_empty()).then(|| rest.to_string()),
        _ => None,
    }
}

fn query_param(q: &Option<String>, name: &str) -> Option<String> {
    q.as_deref()?
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| lfs::decode_component(v))
}

fn error_response(e: &GitError) -> Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut resp = (status, format!("{}\n", e.line())).into_response();
    if e.is_unauthenticated() {
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Beehive\""),
        );
    }
    resp
}

fn git_response(content_type: &str, body: Vec<u8>) -> Response {
    let mut resp = Response::new(Body::from(body));
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

/// A protocol-level error a git client prints as `remote error: …`.
fn err_pkt(content_type: &str, e: &GitError) -> Response {
    let mut out = Vec::new();
    pktline::text(&mut out, &format!("ERR {}\n", e.line()));
    let mut resp = git_response(content_type, out);
    resp.extensions_mut().insert(Failed);
    resp
}

fn protocol_v2(headers: &HeaderMap) -> bool {
    headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(':').any(|p| p.trim() == "version=2"))
}

fn protocol_v1(headers: &HeaderMap) -> bool {
    headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(':').any(|p| p.trim() == "version=1"))
}

impl GitGateway {
    async fn open(
        &self,
        owner: &str,
        repo: &str,
        headers: &HeaderMap,
        access: Access,
    ) -> GitResult<Arc<dyn RepoSession>> {
        self.open_via(owner, repo, headers, access, false).await
    }

    /// Opens the repo for a Git LFS API request.
    async fn open_lfs(
        &self,
        owner: &str,
        repo: &str,
        headers: &HeaderMap,
        access: Access,
    ) -> GitResult<Arc<dyn RepoSession>> {
        self.open_via(owner, repo, headers, access, true).await
    }

    async fn open_via(
        &self,
        owner: &str,
        repo: &str,
        headers: &HeaderMap,
        access: Access,
        lfs: bool,
    ) -> GitResult<Arc<dyn RepoSession>> {
        let agent = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .filter(|a| a.starts_with("git/"));
        let token = credential(headers);
        // A read without credentials goes to the backend, which decides whether it serves
        // anonymous reads (public repos) and answers 401 when not; a write always needs one.
        if token.is_none() && access == Access::Write {
            return Err(GitError::unauthenticated(
                "credentials required: use your bee token as the HTTP password",
            ));
        }
        self.backend
            .open(OpenRequest {
                owner,
                repo: repo_name(repo),
                token: token.as_deref(),
                access,
                via: Via {
                    agent,
                    lfs,
                    ssh: false,
                },
            })
            .await
    }

    async fn read_body(&self, headers: &HeaderMap, body: Body) -> GitResult<Vec<u8>> {
        let limit = self.cfg.max_request_bytes;
        let raw = axum::body::to_bytes(body, limit)
            .await
            .map_err(|_| GitError::new(bee_core::Reason::TooLarge, "request body too large"))?;
        let gzip = headers
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("gzip") || v.eq_ignore_ascii_case("x-gzip"));
        if !gzip {
            return Ok(raw.to_vec());
        }
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&raw[..])
            .take(limit as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|e| GitError::invalid(format!("bad gzip request body: {e}")))?;
        if out.len() > limit {
            return Err(GitError::new(
                bee_core::Reason::TooLarge,
                "request body too large",
            ));
        }
        Ok(out)
    }
}

async fn info_refs(
    State(gw): Gw,
    Path((owner, repo)): Path<(String, String)>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    let service = query_param(&q, "service").unwrap_or_default();
    let (access, upload) = match service.as_str() {
        "git-upload-pack" => (Access::Read, true),
        "git-receive-pack" => (Access::Write, false),
        _ => {
            return error_response(&GitError::invalid(
                "only the smart HTTP protocol is served (?service=git-upload-pack or git-receive-pack)",
            ));
        }
    };
    let session = match gw.open(&owner, &repo, &headers, access).await {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };
    let ct = format!("application/x-{service}-advertisement");
    let r = async {
        let format = advertise::repo_format(session.as_ref()).await?;
        if upload && protocol_v2(&headers) {
            return Ok(upload::advertise_v2(format));
        }
        let mut view = GitView::new(session.history(), format);
        let refs = advertise::list(session.as_ref(), &mut view).await?;
        Ok::<_, GitError>(upload::advertise_v0(
            &service,
            &refs,
            protocol_v1(&headers),
            upload,
        ))
    }
    .await;
    match r {
        Ok(body) => git_response(&ct, body),
        Err(e) => error_response(&e),
    }
}

async fn upload_pack(
    State(gw): Gw,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let ct = "application/x-git-upload-pack-result";
    let session = match gw.open(&owner, &repo, &headers, Access::Read).await {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };
    let body = match gw.read_body(&headers, body).await {
        Ok(b) => b,
        Err(e) => return error_response(&e),
    };
    let used = CacheUse::new(&gw.cache);
    let version = if protocol_v2(&headers) {
        2
    } else if protocol_v1(&headers) {
        1
    } else {
        0
    };
    gw.metrics.fetch_protocol("http", version);
    let r = async {
        let format = advertise::repo_format(session.as_ref()).await?;
        let mut view = GitView::new(session.history(), format);
        let refs = advertise::list(session.as_ref(), &mut view).await?;
        if protocol_v2(&headers) {
            upload::serve_v2(session.as_ref(), &mut view, &refs, &used, &body).await
        } else {
            upload::serve_v0(session.as_ref(), &mut view, &refs, &used, &body).await
        }
    }
    .await;
    match r {
        Ok(out) => {
            gw.metrics.upload_bytes("http", out.len() as u64);
            gw.metrics.pack_cache(used.outcome());
            let mut resp = git_response(ct, out);
            resp.headers_mut()
                .insert("x-bee-pack-cache", HeaderValue::from_static(used.outcome()));
            resp
        }
        Err(e) if e.is_unauthenticated() => error_response(&e),
        Err(e) => err_pkt(ct, &e),
    }
}

async fn receive_pack(
    State(gw): Gw,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let ct = "application/x-git-receive-pack-result";
    let session = match gw.open(&owner, &repo, &headers, Access::Write).await {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };
    let body = match gw.read_body(&headers, body).await {
        Ok(b) => b,
        Err(e) => return error_response(&e),
    };
    gw.metrics.receive_bytes("http", body.len() as u64);
    let r = async {
        let format = advertise::repo_format(session.as_ref()).await?;
        let req = receive::parse(format, &body)?;
        let mut view = GitView::new(session.history(), format);
        let refs = advertise::list(session.as_ref(), &mut view).await?;
        let (unpack, outcomes) = receive::execute(
            session.as_ref(),
            &mut view,
            &refs,
            &req,
            gw.backend.now_ms(),
        )
        .await?;
        view.flush().await?;
        gw.metrics.ref_updates(
            outcomes
                .iter()
                .filter(|o| matches!(o, receive::Outcome::Ok { .. }))
                .count() as u64,
            outcomes
                .iter()
                .filter(|o| matches!(o, receive::Outcome::Ng(_)))
                .count() as u64,
        );
        Ok::<_, GitError>(receive::report(&req, &unpack, &outcomes))
    }
    .await;
    match r {
        Ok(out) => git_response(ct, out),
        Err(e) => error_response(&e),
    }
}

fn lfs_error(e: &GitError) -> Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = serde_json::json!({ "message": e.line() }).to_string();
    let mut resp = (status, body).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(LFS_MEDIA_TYPE),
    );
    if e.is_unauthenticated() {
        h.insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Beehive\""),
        );
        h.insert(
            "lfs-authenticate",
            HeaderValue::from_static("Basic realm=\"Beehive\""),
        );
    }
    resp
}

async fn lfs_batch(
    State(gw): Gw,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let r = async {
        let body = gw.read_body(&headers, body).await?;
        let req: BatchRequest = serde_json::from_slice(&body)
            .map_err(|e| GitError::invalid(format!("bad LFS batch request: {e}")))?;
        let access = if req.operation == "upload" {
            Access::Write
        } else {
            Access::Read
        };
        let session = gw.open_lfs(&owner, &repo, &headers, access).await?;
        let now = gw.backend.now_ms() / 1000;
        lfs::batch(
            session.as_ref(),
            &req,
            &gw.backend.public_url(),
            &gw.backend.url_key(),
            now,
        )
        .await
    }
    .await;
    match r {
        Ok(v) => {
            let mut resp = Response::new(Body::from(v.to_string()));
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(LFS_MEDIA_TYPE),
            );
            resp
        }
        Err(e) => lfs_error(&e),
    }
}

fn lfs_json(status: u16, v: &serde_json::Value) -> Response {
    let mut resp = Response::new(Body::from(v.to_string()));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(LFS_MEDIA_TYPE),
    );
    resp
}

async fn locks_create(
    State(gw): Gw,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let r = async {
        let body = gw.read_body(&headers, body).await?;
        let session = gw.open_lfs(&owner, &repo, &headers, Access::Write).await?;
        locks::create(
            session.as_ref(),
            gw.backend.locks(),
            &body,
            gw.backend.now_ms(),
        )
        .await
    }
    .await;
    match r {
        Ok((status, v)) => lfs_json(status, &v),
        Err(e) => lfs_error(&e),
    }
}

async fn locks_list(
    State(gw): Gw,
    Path((owner, repo)): Path<(String, String)>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    let r = async {
        let session = gw.open_lfs(&owner, &repo, &headers, Access::Read).await?;
        let query = locks::ListQuery {
            path: query_param(&q, "path").filter(|p| !p.is_empty()),
            id: query_param(&q, "id").filter(|p| !p.is_empty()),
            cursor: query_param(&q, "cursor"),
            limit: query_param(&q, "limit").and_then(|l| l.parse().ok()),
        };
        locks::list(session.as_ref(), gw.backend.locks(), &query).await
    }
    .await;
    match r {
        Ok(v) => lfs_json(200, &v),
        Err(e) => lfs_error(&e),
    }
}

async fn locks_verify(
    State(gw): Gw,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let r = async {
        let body = gw.read_body(&headers, body).await?;
        let session = gw.open_lfs(&owner, &repo, &headers, Access::Write).await?;
        locks::verify(session.as_ref(), gw.backend.locks(), &body).await
    }
    .await;
    match r {
        Ok(v) => lfs_json(200, &v),
        Err(e) => lfs_error(&e),
    }
}

async fn locks_unlock(
    State(gw): Gw,
    Path((owner, repo, id)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let r = async {
        let body = gw.read_body(&headers, body).await?;
        let session = gw.open_lfs(&owner, &repo, &headers, Access::Write).await?;
        locks::unlock(session.as_ref(), gw.backend.locks(), &id, &body).await
    }
    .await;
    match r {
        Ok((status, v)) => lfs_json(status, &v),
        Err(e) => lfs_error(&e),
    }
}

fn signed(
    method: &str,
    owner: &str,
    repo: &str,
    oid: &str,
    q: &Option<String>,
) -> GitResult<(Signed, String)> {
    let bad = || GitError::unauthenticated("the LFS transfer URL is incomplete");
    let num = |name: &str| -> GitResult<u64> {
        query_param(q, name)
            .and_then(|v| v.parse().ok())
            .ok_or_else(bad)
    };
    let s = Signed {
        method: method.into(),
        repo: format!("{owner}/{}", repo_name(repo)),
        oid: oid.to_string(),
        size: num("size")?,
        exp: num("exp")?,
        principal: query_param(q, "p").ok_or_else(bad)?,
    };
    let sig = query_param(q, "sig").ok_or_else(bad)?;
    Ok((s, sig))
}

async fn lfs_put(
    State(gw): Gw,
    Path((owner, repo, oid)): Path<(String, String, String)>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let r = async {
        let (s, sig) = signed("PUT", &owner, &repo, &oid, &q)?;
        s.verify(&gw.backend.url_key(), &sig, gw.backend.now_ms() / 1000)?;
        let body = gw.read_body(&headers, body).await?;
        let history = gw
            .backend
            .signed_history(&owner, repo_name(&repo), &s.principal, true)
            .await?;
        gw.metrics.lfs_bytes("upload", body.len() as u64);
        lfs::put(
            &history,
            &s,
            body,
            gw.cfg.max_request_bytes as u64,
            gw.backend.now_ms(),
        )
        .await
    }
    .await;
    match r {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => lfs_error(&e),
    }
}

async fn lfs_get(
    State(gw): Gw,
    Path((owner, repo, oid)): Path<(String, String, String)>,
    RawQuery(q): RawQuery,
) -> Response {
    let r = async {
        let (s, sig) = signed("GET", &owner, &repo, &oid, &q)?;
        s.verify(&gw.backend.url_key(), &sig, gw.backend.now_ms() / 1000)?;
        let history = gw
            .backend
            .signed_history(&owner, repo_name(&repo), &s.principal, false)
            .await?;
        lfs::get(&history, &s).await
    }
    .await;
    match r {
        Ok(bytes) => {
            gw.metrics.lfs_bytes("download", bytes.len() as u64);
            let mut resp = Response::new(Body::from(bytes));
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            resp
        }
        Err(e) => lfs_error(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(v: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(header::AUTHORIZATION, HeaderValue::from_str(v).unwrap());
        m
    }

    #[test]
    fn credentials() {
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        assert_eq!(
            credential(&h(&format!("Basic {}", b64("x:tok")))).as_deref(),
            Some("tok")
        );
        assert_eq!(
            credential(&h(&format!("Basic {}", b64("tok:")))).as_deref(),
            Some("tok")
        );
        assert_eq!(
            credential(&h(&format!("Basic {}", b64("tok")))).as_deref(),
            Some("tok")
        );
        assert_eq!(credential(&h("Bearer abc")).as_deref(), Some("abc"));
        assert_eq!(credential(&h("bearer abc")).as_deref(), Some("abc"));
        assert_eq!(credential(&h(&format!("Basic {}", b64(":")))), None);
        assert_eq!(credential(&h("Digest x")), None);
        assert_eq!(credential(&HeaderMap::new()), None);
    }

    #[test]
    fn protocol_headers() {
        let mut m = HeaderMap::new();
        m.insert("git-protocol", HeaderValue::from_static("version=2"));
        assert!(protocol_v2(&m) && !protocol_v1(&m));
        m.insert(
            "git-protocol",
            HeaderValue::from_static("object-format=sha1:version=1"),
        );
        assert!(!protocol_v2(&m) && protocol_v1(&m));
        assert_eq!(repo_name("r.git"), "r");
        assert_eq!(repo_name("r"), "r");
    }
}
