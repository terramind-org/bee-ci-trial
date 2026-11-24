//! The GitHub compatibility gateway's ports on the server (`docs/spec/github-compat.md`,
//! "Server wiring"):
//!
//! | Port | Implementation |
//! |---|---|
//! | `GitPort` | [`GatedGit`] over `bee_compat::memory::HistoryGit`: writes (blobs, trees, commits, file edits, ref moves) pass the server's repo gate for `commit` first (repo access, `ARCHIVED`, token scope, policy); ref moves go through `RefService` ([`ServerRefMover`]): policy, ref events, CI, webhooks |
//! | `AssetStore` | [`ServerAssets`]: release asset bytes at the forge's signed upload URL, in the blob store |
//! | `CompatState` | [`KvState`]: the server KV (namespace `compat_state`), so it survives restarts |
//! | `CodeSearch` | [`ServerCodeSearch`]: the server's code index (`SearchService`'s trigram index) |
//! | config | `public_url`/`web_url` from `server.public_url`; the sealed-box key derived from the server's URL key |
//!
//! [`asset_router`] serves the signed release asset URLs (`/releases/...?expires=&signature=`)
//! for upload (`PUT`) and download (`GET`).

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use bee_compat::ports::{
    Actor, AssetStore, CodeHit, CodeSearch, Comparison, CompatState, FileChange, FileEdit,
    GitCommitInfo, GitPort, GitRefInfo, NewGitCommit, NewTreeItem, PathEntry, RefMover,
    TreeListing,
};
use bee_compat::{ApiError as CompatError, ApiResult as CompatResult, CompatConfig};
use bee_core::RefName;
use bee_forge::community::ports::HmacSigner;
use bee_forge::packages::ports::BlobPort;
use bee_policy::{Action, Actor as PolicyActor, PrincipalKind};
use bee_proto::v1::{self, ErrorReason, ObjectId, Repo, ref_service_server::RefService};
use bee_search::{CodeSearchRequest, IndexStore, Indexer, RepoTarget, Searcher};
use bytes::Bytes;
use http::{StatusCode, header};

use super::ForgeWiring;
use super::packages::ServerPackageBlobs;
use crate::services::refs::Refs;
use crate::state::AppState;

/// Server KV namespace of the gateway's documents.
pub const NS_COMPAT_STATE: &str = "compat_state";

/// The signer of the forge's signed URLs (the same key the community ports use).
pub fn url_signer(state: &AppState) -> crate::errors::ApiResult<HmacSigner> {
    let key = blake3::derive_key(
        "bee forge signed urls v1",
        &crate::server::url_key(&state.cfg)?,
    );
    Ok(HmacSigner::new(&state.public_url(), &key))
}

/// The gateway's settings from the server's: public and web origin from `server.public_url`
/// (derived from each request's `Host` when unset), and the Actions secrets sealed-box key
/// derived from the server's URL key (a random per-server key, never the gateway's fixed
/// development key).
pub fn config(state: &AppState) -> crate::errors::ApiResult<CompatConfig> {
    let public = state
        .cfg
        .server
        .public_url
        .trim_end_matches('/')
        .to_string();
    let public = (!public.is_empty()).then_some(public);
    Ok(CompatConfig {
        web_url: public.clone(),
        public_url: public,
        secret_box_key: blake3::derive_key(
            "bee compat sealed box v1",
            &crate::server::url_key(&state.cfg)?,
        ),
        ..CompatConfig::default()
    })
}

/// The gateway over `w`, with every port wired to the server.
pub fn build(
    state: &Arc<AppState>,
    w: &ForgeWiring,
) -> crate::errors::ApiResult<Arc<bee_compat::Compat>> {
    let git = Arc::new(GatedGit {
        inner: bee_compat::memory::HistoryGit::new(state.writer.clone(), state.clock.clone())
            .with_ref_mover(Arc::new(ServerRefMover {
                state: Arc::downgrade(state),
            })),
        state: Arc::downgrade(state),
    });
    Ok(bee_compat::Compat::builder(w.forge.clone(), git)
        .changes(w.changes.clone())
        .ci(w.ci.clone())
        .enterprise(w.enterprise.clone())
        .clock(state.clock.clone())
        .state(Arc::new(KvState {
            state: Arc::downgrade(state),
        }))
        .assets(Arc::new(ServerAssets::new(state)?))
        .code_search(Arc::new(ServerCodeSearch::new(state)))
        .config(config(state)?)
        .build())
}

fn gone() -> CompatError {
    CompatError::new(503, ErrorReason::Unavailable, "server stopping")
}

// ----- gated writes ----------------------------------------------------------------------------

/// The gateway's VCS port: reads go straight to the history (every route authorizes the
/// caller's read through the forge first), while every write passes the server's repo gate
/// for `commit` with the caller's token before anything is stored, so an archived repo, a
/// read-only role or a token scoped elsewhere stores nothing.
pub struct GatedGit {
    inner: bee_compat::memory::HistoryGit,
    state: Weak<AppState>,
}

impl GatedGit {
    /// Verifies a signed commit against the forge's signing keys (F49-004).
    async fn verify(&self, c: &mut GitCommitInfo) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Some(v) = c.verification.as_mut() else {
            return;
        };
        if let Ok(Some(verdict)) = state
            .forge
            .verify_signature(v.payload.as_bytes(), v.signature.as_bytes())
            .await
        {
            use bee_proto::v1::SecuritySignatureReason as R;
            v.verified = verdict.verified();
            v.reason = match verdict.reason {
                R::Valid => "valid",
                R::UnknownKey => "unknown_key",
                R::BadSignature | R::SignerMismatch => "invalid",
                R::Malformed => "malformed_signature",
                R::ExpiredKey => "expired_key",
                R::RevokedKey => "not_signing_key",
                R::UnverifiedEmail => "unverified_email",
                R::Unsigned | R::Unspecified => "unsigned",
            }
            .into();
        }
    }

    async fn writable(&self, repo: &Repo, actor: &Actor) -> CompatResult<()> {
        let state = self.state.upgrade().ok_or_else(gone)?;
        let to_compat = |e: crate::errors::ApiError| {
            let reason =
                bee_proto::errors::parse_reason(&e.detail.reason).unwrap_or(ErrorReason::Internal);
            CompatError::from_reason(reason, &e.detail.message, &e.detail.subcode)
        };
        let mut md = tonic::metadata::MetadataMap::new();
        let v = format!("Bearer {}", actor.token)
            .parse()
            .map_err(|_| CompatError::new(401, ErrorReason::Unauthenticated, "Bad credentials"))?;
        md.insert("authorization", v);
        let caller = state.caller(&md).await.map_err(to_compat)?;
        let ctx = state
            .repo(Some(&v1::RepoRef {
                repo: Some(v1::repo_ref::Repo::Id(repo.id.clone())),
            }))
            .await
            .map_err(to_compat)?;
        state
            .gate(&caller, &ctx, Action::Commit, None)
            .await
            .map(|_| ())
            .map_err(to_compat)
    }
}

#[async_trait]
impl GitPort for GatedGit {
    async fn refs(&self, repo: &Repo, prefix: &str) -> CompatResult<Vec<GitRefInfo>> {
        self.inner.refs(repo, prefix).await
    }

    async fn resolve(&self, repo: &Repo, rev: &str) -> CompatResult<Option<String>> {
        self.inner.resolve(repo, rev).await
    }

    async fn commit(&self, repo: &Repo, sha: &str) -> CompatResult<Option<GitCommitInfo>> {
        let mut c = self.inner.commit(repo, sha).await?;
        if let Some(c) = c.as_mut() {
            self.verify(c).await;
        }
        Ok(c)
    }

    async fn log(
        &self,
        repo: &Repo,
        head: &str,
        path: Option<&str>,
        skip: usize,
        limit: usize,
    ) -> CompatResult<Vec<GitCommitInfo>> {
        let mut out = self.inner.log(repo, head, path, skip, limit).await?;
        for c in &mut out {
            self.verify(c).await;
        }
        Ok(out)
    }

    async fn tree(
        &self,
        repo: &Repo,
        sha: &str,
        recursive: bool,
    ) -> CompatResult<Option<TreeListing>> {
        self.inner.tree(repo, sha, recursive).await
    }

    async fn blob(&self, repo: &Repo, sha: &str) -> CompatResult<Option<Vec<u8>>> {
        self.inner.blob(repo, sha).await
    }

    async fn path(&self, repo: &Repo, commit: &str, path: &str) -> CompatResult<Option<PathEntry>> {
        self.inner.path(repo, commit, path).await
    }

    async fn compare(&self, repo: &Repo, base: &str, head: &str) -> CompatResult<Comparison> {
        self.inner.compare(repo, base, head).await
    }

    async fn diff(&self, repo: &Repo, from: &str, to: &str) -> CompatResult<Vec<FileChange>> {
        self.inner.diff(repo, from, to).await
    }

    async fn create_blob(
        &self,
        repo: &Repo,
        actor: &Actor,
        bytes: Vec<u8>,
    ) -> CompatResult<String> {
        self.writable(repo, actor).await?;
        self.inner.create_blob(repo, actor, bytes).await
    }

    async fn create_tree(
        &self,
        repo: &Repo,
        actor: &Actor,
        base_tree: Option<&str>,
        items: Vec<NewTreeItem>,
    ) -> CompatResult<String> {
        self.writable(repo, actor).await?;
        self.inner.create_tree(repo, actor, base_tree, items).await
    }

    async fn create_commit(
        &self,
        repo: &Repo,
        actor: &Actor,
        c: NewGitCommit,
    ) -> CompatResult<String> {
        self.writable(repo, actor).await?;
        self.inner.create_commit(repo, actor, c).await
    }

    async fn update_ref(
        &self,
        repo: &Repo,
        actor: &Actor,
        name: &str,
        sha: Option<&str>,
        expected: Option<&str>,
        force: bool,
    ) -> CompatResult<()> {
        self.writable(repo, actor).await?;
        self.inner
            .update_ref(repo, actor, name, sha, expected, force)
            .await
    }

    async fn commit_files(
        &self,
        repo: &Repo,
        actor: &Actor,
        branch: &str,
        parent: Option<&str>,
        edits: Vec<FileEdit>,
        message: &str,
    ) -> CompatResult<String> {
        self.writable(repo, actor).await?;
        self.inner
            .commit_files(repo, actor, branch, parent, edits, message)
            .await
    }

    async fn changeset(&self, repo: &Repo, sha: &str) -> CompatResult<Option<ObjectId>> {
        self.inner.changeset(repo, sha).await
    }

    async fn git_id(&self, repo: &Repo, changeset: &ObjectId) -> CompatResult<Option<String>> {
        self.inner.git_id(repo, changeset).await
    }
}

// ----- refs ------------------------------------------------------------------------------------

/// Moves refs through the server's `RefService` with the caller's token.
pub struct ServerRefMover {
    state: Weak<AppState>,
}

fn authed<T>(token: &str, msg: T) -> CompatResult<tonic::Request<T>> {
    let mut r = tonic::Request::new(msg);
    let v = format!("Bearer {token}")
        .parse()
        .map_err(|_| CompatError::new(401, ErrorReason::Unauthenticated, "Bad credentials"))?;
    r.metadata_mut().insert("authorization", v);
    Ok(r)
}

#[async_trait]
impl RefMover for ServerRefMover {
    async fn move_ref(
        &self,
        repo: &Repo,
        actor: &Actor,
        name: &str,
        target: Option<&ObjectId>,
        expected: Option<u64>,
        force: bool,
    ) -> CompatResult<()> {
        let state = self.state.upgrade().ok_or_else(gone)?;
        let refs = Refs::new(state);
        let rr = Some(v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::Id(repo.id.clone())),
        });
        let reason = "github compat".to_string();
        match (target, expected) {
            (Some(t), None) => {
                refs.create_ref(authed(
                    &actor.token,
                    v1::CreateRefRequest {
                        repo: rr,
                        name: name.to_string(),
                        target: Some(t.clone()),
                        reason,
                    },
                )?)
                .await?;
            }
            (Some(t), Some(v)) => {
                refs.update_ref(authed(
                    &actor.token,
                    v1::UpdateRefRequest {
                        repo: rr,
                        name: name.to_string(),
                        target: Some(t.clone()),
                        expected_version: v,
                        allow_non_fast_forward: force,
                        reason,
                    },
                )?)
                .await?;
            }
            (None, v) => {
                refs.delete_ref(authed(
                    &actor.token,
                    v1::DeleteRefRequest {
                        repo: rr,
                        name: name.to_string(),
                        expected_version: v.unwrap_or(0),
                        reason,
                    },
                )?)
                .await?;
            }
        }
        Ok(())
    }
}

// ----- release assets --------------------------------------------------------------------------

/// Release asset bytes at the forge's signed URLs, stored in the blob store.
pub struct ServerAssets {
    blobs: ServerPackageBlobs,
    signer: HmacSigner,
    state: Weak<AppState>,
}

impl ServerAssets {
    /// Assets over `state`.
    pub fn new(state: &Arc<AppState>) -> crate::errors::ApiResult<ServerAssets> {
        Ok(ServerAssets {
            blobs: ServerPackageBlobs::new(state),
            signer: url_signer(state)?,
            state: Arc::downgrade(state),
        })
    }

    /// The blob key of a signed URL (path, `expires`, `signature`) if the signature is valid
    /// for `method` now.
    fn verify(&self, method: &str, path: &str, query: &str) -> Option<String> {
        self.verify_under("releases/", method, path, query)
    }

    fn verify_under(&self, prefix: &str, method: &str, path: &str, query: &str) -> Option<String> {
        let key = path.trim_start_matches('/');
        if !key.starts_with(prefix) || key.contains("..") {
            return None;
        }
        let mut expires = None;
        let mut sig = None;
        for kv in query.split('&') {
            match kv.split_once('=') {
                Some(("expires", v)) => expires = v.parse::<u64>().ok(),
                Some(("signature", v)) => sig = Some(v.to_string()),
                _ => {}
            }
        }
        let now = self.state.upgrade()?.now_ms();
        self.signer
            .verify(method, key, expires?, &sig?, now)
            .then(|| key.to_string())
    }

    /// Stores bytes at a signed upload URL (`path?query`).
    pub async fn put_signed(&self, path: &str, query: &str, bytes: Bytes) -> CompatResult<()> {
        let key = self.verify("PUT", path, query).ok_or_else(|| {
            CompatError::new(
                403,
                ErrorReason::PermissionDenied,
                "the signed upload URL is invalid or expired",
            )
        })?;
        self.blobs
            .put(&key, bytes)
            .await
            .map_err(|e| CompatError::new(503, e.reason, e.message))
    }

    /// Bytes at a signed download URL.
    pub async fn get_signed(&self, path: &str, query: &str) -> Option<Bytes> {
        let key = self.verify("GET", path, query)?;
        self.blobs.get(&key).await.ok().flatten()
    }
}

#[async_trait]
impl AssetStore for ServerAssets {
    async fn put(
        &self,
        upload_url: &str,
        _headers: &[(String, String)],
        bytes: Bytes,
    ) -> CompatResult<()> {
        let rest = upload_url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(upload_url);
        let path_q = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
        let (path, query) = path_q.split_once('?').unwrap_or((path_q, ""));
        self.put_signed(path, query, bytes).await
    }
}

/// Signed release asset URLs: `PUT` stores the bytes, `GET` (and `HEAD`) serves them. The
/// signature in the query is the authorization. Social preview images (F416-011) are `PUT`
/// to their signed URL and then served publicly at the same path. Post media
/// (`posts/<repo>/<media>`) is `PUT` to its upload URL and served at download URLs that name
/// the principal chain they were issued to; repo access is decided again for it on every
/// request (SPEC §9.6).
pub fn asset_router(assets: Arc<ServerAssets>) -> Router {
    Router::new()
        .route("/releases/{*rest}", any(serve_asset))
        .route("/archives/{*rest}", any(serve_archive))
        .route("/social-previews/{*rest}", any(serve_social_preview))
        .route("/posts/{*rest}", any(serve_post_media))
        .route("/_evidence/{*rest}", any(serve_evidence))
        .with_state(assets)
}

/// The HTTP status of a forge error answering a media request.
fn media_status(e: &bee_forge::ForgeError) -> StatusCode {
    match e.reason {
        ErrorReason::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ErrorReason::InvalidArgument => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ErrorReason::NotFound => StatusCode::NOT_FOUND,
        ErrorReason::InvalidState => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// `<repo>/<media>` of a post media key.
fn media_parts(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("posts/")?;
    let (repo, media) = rest.split_once('/')?;
    (!repo.is_empty() && !media.is_empty() && !media.contains('/'))
        .then(|| (repo.to_string(), media.to_string()))
}

/// Post media (`bee_forge::media`). `PUT` at a signed upload URL stores the bytes of a
/// pending upload (sniffed, measured and hashed by the forge). `GET` and `HEAD` at a signed
/// download URL decide repo read access again for the URL's principal chain, then serve the
/// finalized bytes with their sniffed type, `nosniff`, a sandbox CSP and `Range` support.
async fn serve_post_media(State(assets): State<Arc<ServerAssets>>, req: Request) -> Response {
    use bee_forge::media::serve::{CACHE_CONTROL, RangeAsk, SAFETY_HEADERS, parse_range};
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let (Some(state), Some(w)) = (
        assets.state.upgrade(),
        assets.state.upgrade().and_then(|s| s.hub.get().cloned()),
    ) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let signer = w.posts.uploads().signer().clone();
    let now = state.now_ms();
    let cx = bee_forge::Cx::system();
    let head = match req.method().as_str() {
        "PUT" => {
            let Some(key) = signer.verify_upload("posts/", &path, &query, now) else {
                return (
                    StatusCode::FORBIDDEN,
                    "the signed upload URL is invalid or expired",
                )
                    .into_response();
            };
            let Some((repo_id, media_id)) = media_parts(&key) else {
                return (StatusCode::NOT_FOUND, "not found").into_response();
            };
            let limit = bee_forge::media::upload::max_upload_bytes() as usize;
            let body = match to_bytes(req.into_body(), limit).await {
                Ok(b) => b,
                Err(_) => {
                    return (StatusCode::PAYLOAD_TOO_LARGE, "the upload is too large")
                        .into_response();
                }
            };
            return match w.posts.receive_media(&cx, &repo_id, &media_id, body).await {
                Ok(()) => StatusCode::OK.into_response(),
                Err(e) => (media_status(&e), e.message).into_response(),
            };
        }
        "GET" => false,
        "HEAD" => true,
        _ => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
    };
    let Some((key, chain)) = signer.verify_download("posts/", &path, &query, now) else {
        return (
            StatusCode::FORBIDDEN,
            "the signed media URL is invalid or expired",
        )
            .into_response();
    };
    let Some((repo_id, media_id)) = media_parts(&key) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    // The URL proves who it was issued to, not that they may still read the repo: decide again.
    let chain: Vec<(String, PrincipalKind)> = chain
        .into_iter()
        .map(|(id, k)| (id, PrincipalKind::parse(&k).unwrap_or(PrincipalKind::Human)))
        .collect();
    let rr = v1::RepoRef {
        repo: Some(v1::repo_ref::Repo::Id(repo_id.clone())),
    };
    let decided = match state.repo(Some(&rr)).await {
        Ok(repo) => state.authorize_chain(&chain, &repo, Action::Read).await,
        Err(e) => Err(e),
    };
    match decided {
        Ok(_) => {}
        Err(e) if e.detail.retryable => {
            return (StatusCode::SERVICE_UNAVAILABLE, e.detail.message).into_response();
        }
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    }
    let rec = match w.posts.media_for_download(&cx, &repo_id, &media_id).await {
        Ok(r) => r,
        Err(e) => return (media_status(&e), e.message).into_response(),
    };
    let bytes = match w.posts.media_bytes(&rec).await {
        Ok(b) => b,
        Err(e) => return (media_status(&e), e.message).into_response(),
    };
    let len = bytes.len() as u64;
    let mut h = http::HeaderMap::new();
    for (k, v) in SAFETY_HEADERS {
        h.insert(*k, http::HeaderValue::from_static(v));
    }
    h.insert(
        header::CACHE_CONTROL,
        http::HeaderValue::from_static(CACHE_CONTROL),
    );
    if let Ok(v) = http::HeaderValue::from_str(&rec.content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (status, body) = match parse_range(range.as_deref(), len) {
        RangeAsk::Full => (StatusCode::OK, bytes),
        RangeAsk::Part(a, b) => {
            if let Ok(v) = http::HeaderValue::from_str(&format!("bytes {a}-{b}/{len}")) {
                h.insert(header::CONTENT_RANGE, v);
            }
            (
                StatusCode::PARTIAL_CONTENT,
                bytes.slice(a as usize..=b as usize),
            )
        }
        RangeAsk::Unsatisfiable => {
            if let Ok(v) = http::HeaderValue::from_str(&format!("bytes */{len}")) {
                h.insert(header::CONTENT_RANGE, v);
            }
            return (StatusCode::RANGE_NOT_SATISFIABLE, h).into_response();
        }
    };
    if let Ok(v) = http::HeaderValue::from_str(&body.len().to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    let body = if head {
        Body::empty()
    } else {
        Body::from(body)
    };
    (status, h, body).into_response()
}

/// Evidence of change requests (`bee_forge::briefs::transfer`). `PUT` at a signed upload URL
/// stores the bytes of one asset of a pending item. `GET` and `HEAD` at a signed download URL
/// decide repo read access again for the URL's principal chain (SPEC §9.6), then serve the
/// asset with its sniffed type, `nosniff`, a sandbox CSP, `attachment` unless it is an image,
/// video or audio, an `ETag` and `Range` support.
async fn serve_evidence(State(assets): State<Arc<ServerAssets>>, req: Request) -> Response {
    use bee_forge::media::serve::{CACHE_CONTROL, RangeAsk, SAFETY_HEADERS, parse_range};
    let target = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let (Some(state), Some(w)) = (
        assets.state.upgrade(),
        assets.state.upgrade().and_then(|s| s.hub.get().cloned()),
    ) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let denied = |e: &bee_forge::ForgeError| match e.reason {
        ErrorReason::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::FORBIDDEN,
    };
    let head = match req.method().as_str() {
        "PUT" => {
            let up = match w.briefs.verify_upload(&target) {
                Ok(u) => u,
                Err(e) => {
                    return (denied(&e), "the signed upload URL is invalid or expired")
                        .into_response();
                }
            };
            let limit = w.briefs.max_upload_bytes() as usize;
            let body = match to_bytes(req.into_body(), limit).await {
                Ok(b) => b,
                Err(_) => {
                    return (StatusCode::PAYLOAD_TOO_LARGE, "the upload is too large")
                        .into_response();
                }
            };
            return match w.briefs.receive_upload(&up, body.to_vec()).await {
                Ok(()) => StatusCode::OK.into_response(),
                Err(e) => (media_status(&e), e.message).into_response(),
            };
        }
        "GET" => false,
        "HEAD" => true,
        _ => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
    };
    let down = match w.briefs.verify_download(&target) {
        Ok(d) => d,
        Err(e) => {
            return (denied(&e), "the signed evidence URL is invalid or expired").into_response();
        }
    };
    // The URL proves who it was issued to, not that they may still read the repo: decide again.
    let chain: Vec<(String, PrincipalKind)> = down
        .chain
        .iter()
        .map(|(id, k)| {
            (
                id.clone(),
                PrincipalKind::parse(k).unwrap_or(PrincipalKind::Human),
            )
        })
        .collect();
    let rr = v1::RepoRef {
        repo: Some(v1::repo_ref::Repo::Id(down.repo_id.clone())),
    };
    let decided = match state.repo(Some(&rr)).await {
        Ok(repo) => state.authorize_chain(&chain, &repo, Action::Read).await,
        Err(e) => Err(e),
    };
    match decided {
        Ok(_) => {}
        Err(e) if e.detail.retryable => {
            return (StatusCode::SERVICE_UNAVAILABLE, e.detail.message).into_response();
        }
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    }
    let file = match w.briefs.download(&down).await {
        Ok(f) => f,
        Err(e) => return (media_status(&e), e.message).into_response(),
    };
    let bytes = Bytes::from(file.bytes);
    let len = bytes.len() as u64;
    let mut h = http::HeaderMap::new();
    for (k, v) in SAFETY_HEADERS {
        h.insert(*k, http::HeaderValue::from_static(v));
    }
    h.insert(
        header::CACHE_CONTROL,
        http::HeaderValue::from_static(CACHE_CONTROL),
    );
    if let Ok(v) = http::HeaderValue::from_str(&file.content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Some(d) = file.disposition {
        h.insert(
            header::CONTENT_DISPOSITION,
            http::HeaderValue::from_static(d),
        );
    }
    if let Ok(v) = http::HeaderValue::from_str(&format!("\"{}\"", file.digest)) {
        h.insert(header::ETAG, v);
    }
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (status, body) = match parse_range(range.as_deref(), len) {
        RangeAsk::Full => (StatusCode::OK, bytes),
        RangeAsk::Part(a, b) => {
            if let Ok(v) = http::HeaderValue::from_str(&format!("bytes {a}-{b}/{len}")) {
                h.insert(header::CONTENT_RANGE, v);
            }
            (
                StatusCode::PARTIAL_CONTENT,
                bytes.slice(a as usize..=b as usize),
            )
        }
        RangeAsk::Unsatisfiable => {
            if let Ok(v) = http::HeaderValue::from_str(&format!("bytes */{len}")) {
                h.insert(header::CONTENT_RANGE, v);
            }
            return (StatusCode::RANGE_NOT_SATISFIABLE, h).into_response();
        }
    };
    if let Ok(v) = http::HeaderValue::from_str(&body.len().to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    let body = if head {
        Body::empty()
    } else {
        Body::from(body)
    };
    (status, h, body).into_response()
}

/// Server KV namespace: the redaction epoch each kept source archive was built at.
pub const NS_ARCHIVE_EPOCH: &str = "archive_epoch";

/// Largest source archive kept in the blob store for reuse; bigger ones are rebuilt.
const ARCHIVE_CACHE_MAX: usize = 256 << 20;

fn archive_headers(key: &crate::source_archive::ArchiveKey, cache: &str) -> http::HeaderMap {
    let mut h = http::HeaderMap::new();
    if let Ok(v) = http::HeaderValue::from_str(key.format.content_type()) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) =
        http::HeaderValue::from_str(&format!("attachment; filename=\"{}\"", key.file_name()))
    {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Ok(v) = http::HeaderValue::from_str(cache) {
        h.insert("x-bee-archive-cache", v);
    }
    // The URL names the tree: its bytes never change.
    h.insert(
        header::CACHE_CONTROL,
        http::HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    h
}

/// Source archives at the signed URLs `GetSourceArchiveUrl` hands out
/// (`archives/<repo>/<tree>/<prefix>.<tar.gz|zip>`): the signature is the authorization (the
/// RPC checked that the caller reads every path). A built archive is served from the blob
/// store; otherwise it is streamed from the tree while it is written, then kept, keyed by
/// tree, and the request is marked built.
async fn serve_archive(State(assets): State<Arc<ServerAssets>>, req: Request) -> Response {
    use crate::source_archive::{ArchiveKey, Kind, Writer, check_limits, new_parents};
    let head = match req.method().as_str() {
        "GET" => false,
        "HEAD" => true,
        _ => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
    };
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let Some(key) = assets.verify_under("archives/", "GET", &path, &query) else {
        return (
            StatusCode::FORBIDDEN,
            "the signed archive URL is invalid or expired",
        )
            .into_response();
    };
    let Some(ak) = ArchiveKey::parse(&key) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let (Some(state), Some(w)) = (
        assets.state.upgrade(),
        assets.state.upgrade().and_then(|s| s.hub.get().cloned()),
    ) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let repo = match state
        .repo(Some(&v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::Id(ak.repo.clone())),
        }))
        .await
    {
        Ok(r) => r,
        Err(_) => return (StatusCode::NOT_FOUND, "repository not found").into_response(),
    };
    // A kept archive is only good while the repo's redaction epoch is the one it was built at:
    // a redaction since then may have removed bytes it holds.
    let epoch = match state.redaction_epoch(&repo).await {
        Ok(e) => e,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.detail.message).into_response(),
    };
    let built_at = state
        .kv
        .get(NS_ARCHIVE_EPOCH, key.as_bytes())
        .await
        .ok()
        .flatten()
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|t| t.parse::<u64>().ok());
    if built_at == Some(epoch)
        && let Some(b) = assets.blobs.get(&key).await.ok().flatten()
    {
        let body = if head { Body::empty() } else { Body::from(b) };
        return (archive_headers(&ak, "hit"), body).into_response();
    }
    let cx = bee_forge::Cx::system();
    let rec = match w
        .forge
        .community
        .releases
        .archive_request(&cx, &ak.repo, &key)
        .await
    {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such archive request").into_response(),
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.message).into_response(),
    };
    let Ok(tree) = bee_core::ObjectId::from_bytes(&ak.tree_raw()) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let history = state.history(&repo);
    let leaves: Vec<_> = match history.list_leaves(&tree, None).await {
        Ok(l) => l
            .into_iter()
            .filter(|(_, e)| e.mode != bee_core::Mode::Gitlink)
            .collect(),
        Err(e) => return (StatusCode::NOT_FOUND, e.message).into_response(),
    };
    // Redacted content is never archived (SPEC §13): refuse before anything is sent.
    let ids: Vec<bee_core::ObjectId> = leaves.iter().map(|(_, e)| e.id).collect();
    match crate::redaction::redacted_among(&state, repo.org.id, &ids).await {
        Ok(r) if r.is_empty() => {}
        Ok(r) => {
            let path = leaves
                .iter()
                .find(|(_, e)| r.contains(&e.id))
                .map(|(p, _)| p.to_string())
                .unwrap_or_default();
            return (
                StatusCode::NOT_FOUND,
                format!("REDACTED: {path} is redacted; this tree cannot be archived"),
            )
                .into_response();
        }
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.detail.message).into_response(),
    }
    let bytes: u64 = leaves.iter().map(|(_, e)| e.size).sum();
    if let Err(e) = check_limits(ak.format, leaves.len() + 1, bytes) {
        return (StatusCode::BAD_REQUEST, format!("TOO_LARGE: {e}")).into_response();
    }
    if head {
        return (archive_headers(&ak, "miss"), Body::empty()).into_response();
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let headers = archive_headers(&ak, "miss");
    let repo_id = ak.repo.clone();
    tokio::spawn(async move {
        let fail = |m: String| Err(std::io::Error::other(m));
        let mut out = Writer::new(ak.format);
        let mut kept: Option<Vec<u8>> = Some(Vec::new());
        let mut seen = std::collections::BTreeSet::from([ak.prefix.clone()]);
        let send = async |chunk: Vec<u8>, kept: &mut Option<Vec<u8>>| -> bool {
            if chunk.is_empty() {
                return true;
            }
            if let Some(k) = kept.as_mut() {
                if k.len() + chunk.len() <= ARCHIVE_CACHE_MAX {
                    k.extend_from_slice(&chunk);
                } else {
                    *kept = None;
                }
            }
            tx.send(Ok(Bytes::from(chunk))).await.is_ok()
        };
        if let Err(e) = out.add(&ak.prefix, Kind::Dir, &[]) {
            let _ = tx.send(fail(e.to_string())).await;
            return;
        }
        for (path, entry) in &leaves {
            let full = format!("{}/{path}", ak.prefix);
            let kind = match entry.mode {
                bee_core::Mode::Executable => Kind::Executable,
                bee_core::Mode::Symlink => Kind::Symlink,
                _ => Kind::File,
            };
            let data = match history.blob(&entry.id).await {
                Ok(bee_history::BlobContent::Bytes(b)) => b,
                Ok(bee_history::BlobContent::Redacted) => {
                    let _ = tx.send(fail(format!("REDACTED: {path}"))).await;
                    return;
                }
                Err(e) => {
                    let _ = tx
                        .send(fail(format!("{}: {}", e.reason.as_str(), e.message)))
                        .await;
                    return;
                }
            };
            let mut r = Ok(());
            for d in new_parents(&full, &mut seen) {
                r = r.and_then(|()| out.add(&d, Kind::Dir, &[]));
            }
            if let Err(e) = r.and_then(|()| out.add(&full, kind, &data)) {
                let _ = tx.send(fail(e.to_string())).await;
                return;
            }
            if !send(out.take(), &mut kept).await {
                return;
            }
        }
        let rest = match out.finish() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(fail(e.to_string())).await;
                return;
            }
        };
        let delivered = send(rest, &mut kept).await;
        drop(tx);
        // Complete: keep it for the next request of the same tree, with the redaction epoch
        // it was built at.
        if delivered
            && let Some(all) = kept
            && assets.blobs.put(&key, Bytes::from(all)).await.is_ok()
            && state
                .kv
                .put(
                    NS_ARCHIVE_EPOCH,
                    key.as_bytes(),
                    epoch.to_string().into_bytes(),
                )
                .await
                .is_ok()
        {
            let _ = w
                .forge
                .community
                .releases
                .mark_archive_built(&bee_forge::Cx::system(), &repo_id, &rec.id)
                .await;
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    (headers, Body::from_stream(stream)).into_response()
}

/// The image type of PNG or JPEG bytes.
fn image_type(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else {
        None
    }
}

async fn serve_social_preview(State(assets): State<Arc<ServerAssets>>, req: Request) -> Response {
    use bee_forge::orgs::repos::{SOCIAL_PREVIEW_MAX_BYTES, SOCIAL_PREVIEW_PREFIX};
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let key = path.trim_start_matches('/').to_string();
    match req.method().as_str() {
        "GET" | "HEAD" => {
            if !key.starts_with(SOCIAL_PREVIEW_PREFIX) || key.contains("..") {
                return (StatusCode::NOT_FOUND, "not found").into_response();
            }
            match assets.blobs.get(&key).await.ok().flatten() {
                Some(b) => (
                    [(
                        header::CONTENT_TYPE,
                        image_type(&b).unwrap_or("application/octet-stream"),
                    )],
                    Body::from(b),
                )
                    .into_response(),
                None => (StatusCode::NOT_FOUND, "not found").into_response(),
            }
        }
        "PUT" => {
            let Some(key) = assets.verify_under(SOCIAL_PREVIEW_PREFIX, "PUT", &path, &query) else {
                return (
                    StatusCode::FORBIDDEN,
                    "the signed upload URL is invalid or expired",
                )
                    .into_response();
            };
            let body = match to_bytes(req.into_body(), SOCIAL_PREVIEW_MAX_BYTES as usize).await {
                Ok(b) => b,
                Err(_) => {
                    return (StatusCode::PAYLOAD_TOO_LARGE, "image too large").into_response();
                }
            };
            if image_type(&body).is_none() {
                return (
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "a social preview is a PNG or JPEG image",
                )
                    .into_response();
            }
            let Some(repo_id) = key
                .strip_prefix(SOCIAL_PREVIEW_PREFIX)
                .and_then(|r| r.split('/').next())
                .map(str::to_string)
            else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            if let Err(e) = assets.blobs.put(&key, body).await {
                return (StatusCode::SERVICE_UNAVAILABLE, e.message).into_response();
            }
            let Some(state) = assets.state.upgrade() else {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            };
            let Some(w) = state.hub.get() else {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            };
            let url = bee_forge::community::ports::UrlSigner::public(&assets.signer, &key);
            match w
                .forge
                .orgs
                .set_social_preview(&bee_forge::Cx::system(), &repo_id, &url)
                .await
            {
                Ok(_) => StatusCode::OK.into_response(),
                Err(e) => (StatusCode::NOT_FOUND, e.message).into_response(),
            }
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

async fn serve_asset(State(assets): State<Arc<ServerAssets>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    match req.method().as_str() {
        "GET" | "HEAD" => match assets.get_signed(&path, &query).await {
            Some(b) => (
                [(header::CONTENT_TYPE, "application/octet-stream")],
                Body::from(b),
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
        "PUT" => {
            let body = match to_bytes(req.into_body(), 2 << 30).await {
                Ok(b) => b,
                Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
            };
            match assets.put_signed(&path, &query, body).await {
                Ok(()) => StatusCode::OK.into_response(),
                Err(e) => (
                    StatusCode::from_u16(e.status).unwrap_or(StatusCode::FORBIDDEN),
                    e.message,
                )
                    .into_response(),
            }
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

// ----- state -----------------------------------------------------------------------------------

/// The gateway's documents in the server KV.
pub struct KvState {
    state: Weak<AppState>,
}

fn kv_err(e: impl std::fmt::Display) -> CompatError {
    CompatError::new(503, ErrorReason::Unavailable, e.to_string())
}

#[async_trait]
impl CompatState for KvState {
    async fn get(&self, key: &str) -> CompatResult<Option<String>> {
        let state = self.state.upgrade().ok_or_else(gone)?;
        Ok(state
            .kv
            .get(NS_COMPAT_STATE, key.as_bytes())
            .await
            .map_err(kv_err)?
            .map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    async fn put(&self, key: &str, value: &str) -> CompatResult<()> {
        let state = self.state.upgrade().ok_or_else(gone)?;
        state
            .kv
            .put(NS_COMPAT_STATE, key.as_bytes(), value.as_bytes().to_vec())
            .await
            .map_err(kv_err)
    }

    async fn delete(&self, key: &str) -> CompatResult<()> {
        let state = self.state.upgrade().ok_or_else(gone)?;
        state
            .kv
            .apply(vec![crate::kv::KvOp::delete(
                NS_COMPAT_STATE,
                key.as_bytes(),
            )])
            .await
            .map(|_| ())
            .map_err(kv_err)
    }
}

// ----- code search -----------------------------------------------------------------------------

/// `GET /search/code` over `bee-search`: the server's code index (`SearchService`'s,
/// kept current by its indexer; one pass runs before each query so fresh pushes are found),
/// or an in-memory index synced per query when the search service is not running. Only
/// paths every reader of the repo may read are searched, so restricted paths never surface
/// through the gateway.
pub struct ServerCodeSearch {
    state: Weak<AppState>,
    fallback: Arc<dyn IndexStore>,
}

impl ServerCodeSearch {
    /// Search over `state`.
    pub fn new(state: &Arc<AppState>) -> ServerCodeSearch {
        ServerCodeSearch {
            state: Arc::downgrade(state),
            fallback: Arc::new(bee_search::MemoryIndexStore::new()),
        }
    }
}

#[async_trait]
impl CodeSearch for ServerCodeSearch {
    async fn search(
        &self,
        repos: &[Repo],
        query: &str,
        limit: usize,
    ) -> CompatResult<Vec<CodeHit>> {
        let state = self.state.upgrade().ok_or_else(gone)?;
        let shared = crate::services::search::index_of(&state);
        let store: Arc<dyn IndexStore> = match &shared {
            Some(index) => {
                index.index_pass(&state).await.map_err(|e| {
                    CompatError::new(503, ErrorReason::Unavailable, e.detail.message)
                })?;
                index.store().map_err(|e| {
                    CompatError::new(503, ErrorReason::Unavailable, e.detail.message)
                })?
            }
            None => self.fallback.clone(),
        };
        let indexer = Indexer::new(store.clone(), state.meta.clone(), state.blobs.clone());
        let mut targets = Vec::new();
        let mut policies = std::collections::BTreeMap::new();
        for r in repos {
            let Ok(ctx) = super::adapters::repo_ctx(&state, &r.id).await else {
                continue;
            };
            let default = if r.default_ref.is_empty() {
                "main"
            } else {
                r.default_ref.as_str()
            };
            let Ok(name) = RefName::new(default) else {
                continue;
            };
            if shared.is_none()
                && indexer
                    .sync_ref(ctx.org.id, ctx.repo.id, &name)
                    .await
                    .is_err()
            {
                continue;
            }
            if let Ok(p) = state.policy(&ctx, default).await {
                policies.insert(ctx.repo.id, p);
            }
            let (org_name, repo_name) = r.full_name.split_once('/').unwrap_or(("", &r.name));
            targets.push(RepoTarget {
                org: ctx.org.id,
                org_name: org_name.to_string(),
                repo: ctx.repo.id,
                repo_name: repo_name.to_string(),
                ref_name: default.to_string(),
            });
        }
        let anyone = PolicyActor::new("anyone", PrincipalKind::Human);
        let auth = |repo: bee_store::RepoId, path: &bee_core::RepoPath| {
            policies.get(&repo).is_some_and(|p| {
                p.check_access(&anyone, Action::Read, &path.to_string())
                    .allowed
            })
        };
        let resp = Searcher::new(store)
            .search_code(
                &CodeSearchRequest {
                    query: query.to_string(),
                    page_size: limit.clamp(1, 100) as u32,
                    max_line_matches_per_file: 5,
                    ..Default::default()
                },
                &targets,
                &auth,
            )
            .map_err(|e| CompatError::new(422, ErrorReason::InvalidArgument, e.message))?;
        let ids: std::collections::BTreeMap<bee_store::RepoId, String> = repos
            .iter()
            .filter_map(|r| {
                bee_core::Ulid::parse(&r.id)
                    .ok()
                    .map(|u| (bee_store::RepoId(u), r.id.clone()))
            })
            .collect();
        let orgs: std::collections::BTreeMap<bee_store::RepoId, bee_store::OrgId> =
            targets.iter().map(|t| (t.repo, t.org)).collect();
        let mut out = Vec::new();
        for h in resp.hits {
            let (Some(id), Some(org)) = (ids.get(&h.repo), orgs.get(&h.repo)) else {
                continue;
            };
            // GitHub clients expect the git blob id.
            let Ok(Some(bytes)) = state.blobs.get(*org, &h.blob).await else {
                continue;
            };
            let Ok(sha) =
                bee_git::ObjectFormat::Sha1.hash_object(bee_git::ObjectType::Blob, &bytes)
            else {
                continue;
            };
            out.push(CodeHit {
                repo_id: id.clone(),
                path: h.path.to_string(),
                sha: sha.to_hex(),
                fragments: h.line_matches.into_iter().map(|m| m.line).collect(),
            });
        }
        Ok(out)
    }
}
