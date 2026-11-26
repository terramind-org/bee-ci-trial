//! Packages and Pages on the server (`docs/spec/packages-pages.md` §5): the ports they need
//! and their HTTP mounts.
//!
//! - [`ServerPackageBlobs`]: the registries' and Pages' key/value blob port over the blob store
//!   (content-addressed blobs of the internal `-packages` org; the key → blob mapping and size
//!   live in the server tables).
//! - [`TeamPorts`], [`CiArtifacts`], [`EnvironmentGate`]: team grants from enterprise teams,
//!   Pages workflow artifacts from CI, the `github-pages` environment protection from CI
//!   environments (all bound once those modules exist).
//! - [`registry_router`]: `PackageRegistry::handle` under `/v2/` (OCI) and `/npm|maven|nuget|
//!   rubygems|cargo/<owner>/`, and the signed upload URLs of native versions under `/pkg/`.
//! - [`pages_router`]: the host-based Pages listener (`forge.pages.respond`).

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use bee_forge::ci::Ci;
use bee_forge::enterprise::teams::Teams;
use bee_forge::packages::ports::{BlobPort, TeamMembership};
use bee_forge::pages::ports::{Artifact, ArtifactSource, DeployGate};
use bee_forge::{Cx, ForgeError};
use bee_proto::v1::{ErrorReason, Repo};
use bee_store::{OrgId, OrgRow};
use http::StatusCode;
use serde::{Deserialize, Serialize};

use super::fe;
use super::late::Late;
use crate::state::AppState;

type FResult<T> = bee_forge::Result<T>;

/// The internal VCS org holding package and Pages bytes.
pub const PACKAGE_ORG: &str = "-packages";
/// Server-table namespace of package blob keys.
pub const NS_PACKAGE_BLOB: &str = "package_blob";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct KeyRec {
    blob: String,
    size: u64,
}

/// Key/value blobs over the content-addressed blob store.
pub struct ServerPackageBlobs {
    state: Weak<AppState>,
}

impl ServerPackageBlobs {
    /// Blobs over `state`.
    pub fn new(state: &Arc<AppState>) -> ServerPackageBlobs {
        ServerPackageBlobs {
            state: Arc::downgrade(state),
        }
    }

    fn state(&self) -> FResult<Arc<AppState>> {
        self.state
            .upgrade()
            .ok_or_else(|| ForgeError::unavailable("server stopping"))
    }

    async fn org(state: &AppState) -> FResult<OrgId> {
        if let Some(o) = state.meta.org_by_name(PACKAGE_ORG).await.map_err(fe)? {
            return Ok(o.id);
        }
        let id = OrgId(crate::util::new_ulid(state.now_ms()).map_err(fe)?);
        state
            .meta
            .create_org(OrgRow {
                id,
                name: PACKAGE_ORG.into(),
                created_at_ms: state.now_ms(),
            })
            .await
            .map_err(fe)?;
        Ok(state
            .meta
            .org_by_name(PACKAGE_ORG)
            .await
            .map_err(fe)?
            .map(|o| o.id)
            .unwrap_or(id))
    }

    async fn rec(state: &AppState, key: &str) -> FResult<Option<KeyRec>> {
        Ok(state
            .kv
            .get(NS_PACKAGE_BLOB, key.as_bytes())
            .await
            .map_err(fe)?
            .and_then(|b| serde_json::from_slice(&b).ok()))
    }
}

#[async_trait]
impl BlobPort for ServerPackageBlobs {
    async fn put(&self, key: &str, data: Bytes) -> FResult<()> {
        let state = self.state()?;
        let org = Self::org(&state).await?;
        let id = bee_object::blob_id(&data);
        let size = data.len() as u64;
        state
            .writer
            .upload(org, vec![(id, data.to_vec())], None, state.now_ms())
            .await
            .map_err(fe)?;
        state
            .kv
            .put(
                NS_PACKAGE_BLOB,
                key.as_bytes(),
                serde_json::to_vec(&KeyRec {
                    blob: id.to_text(),
                    size,
                })
                .unwrap_or_default(),
            )
            .await
            .map_err(fe)
    }

    async fn get(&self, key: &str) -> FResult<Option<Bytes>> {
        let state = self.state()?;
        let Some(r) = Self::rec(&state, key).await? else {
            return Ok(None);
        };
        let id = bee_core::ObjectId::parse(&r.blob).map_err(fe)?;
        let org = Self::org(&state).await?;
        Ok(state
            .blobs
            .get(org, &id)
            .await
            .map_err(fe)?
            .map(Bytes::from))
    }

    async fn head(&self, key: &str) -> FResult<Option<u64>> {
        let state = self.state()?;
        Ok(Self::rec(&state, key).await?.map(|r| r.size))
    }

    async fn delete(&self, key: &str) -> FResult<()> {
        let state = self.state()?;
        state
            .kv
            .apply(vec![crate::kv::KvOp::delete(NS_PACKAGE_BLOB, key)])
            .await
            .map(|_| ())
            .map_err(fe)
    }
}

/// Team grants from enterprise teams (members of child teams count).
pub struct TeamPorts(pub Late<Teams>);

#[async_trait]
impl TeamMembership for TeamPorts {
    async fn is_member(&self, org_id: &str, team_id: &str, principal_id: &str) -> bool {
        let Some(teams) = self.0.get() else {
            return false;
        };
        teams
            .member_ids(&super::landing::quick_cx(), org_id, team_id, true)
            .await
            .is_ok_and(|ids| ids.iter().any(|i| i == principal_id))
    }
}

/// Pages workflow artifacts from CI.
pub struct CiArtifacts(pub Late<Ci>);

#[async_trait]
impl ArtifactSource for CiArtifacts {
    async fn fetch(&self, repo: &Repo, artifact_id: &str) -> FResult<Artifact> {
        let ci = self
            .0
            .get()
            .ok_or_else(|| ForgeError::unavailable("CI is starting"))?;
        let cx = Cx::system();
        let meta = ci.get_artifact(&cx, repo, artifact_id).await?;
        let data = ci.artifact_bytes(&cx, &repo.id, artifact_id).await?;
        Ok(Artifact {
            data: Bytes::from(data),
            run_id: meta.run_id,
            changeset: meta.head_changeset,
        })
    }
}

/// The `github-pages` environment's protection: a configured environment with reviewers or
/// protection rules needs an approved deployment (`APPROVAL_REQUIRED`); none deploys freely.
pub struct EnvironmentGate(pub Late<Ci>);

#[async_trait]
impl DeployGate for EnvironmentGate {
    async fn check(&self, cx: &Cx, repo: &Repo, environment: &str) -> FResult<()> {
        let Some(ci) = self.0.get() else {
            return Ok(());
        };
        match ci.get_environment(cx, repo, environment).await {
            Ok(env) if !env.reviewers.is_empty() || !env.protection_rules.is_empty() => {
                Err(ForgeError::new(
                    ErrorReason::ApprovalRequired,
                    format!("environment {environment} needs an approved deployment"),
                ))
            }
            Ok(_) => Ok(()),
            Err(e) if e.reason == ErrorReason::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

// ----- HTTP --------------------------------------------------------------------------------

async fn to_request(req: Request, limit: usize) -> Result<http::Request<Bytes>, Box<Response>> {
    let (parts, body) = req.into_parts();
    match to_bytes(body, limit).await {
        Ok(b) => Ok(http::Request::from_parts(parts, b)),
        Err(_) => Err(Box::new(
            (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
        )),
    }
}

fn from_response(resp: http::Response<Bytes>) -> Response {
    let (parts, body) = resp.into_parts();
    Response::from_parts(parts, Body::from(body))
}

/// The bearer or Basic credential of a registry request, when it is a server token.
fn credential(headers: &http::HeaderMap) -> Option<String> {
    use base64::Engine as _;
    let v = headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .trim();
    if let Some(t) = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))
    {
        return Some(t.trim().to_string());
    }
    if let Some(b) = v
        .strip_prefix("Basic ")
        .or_else(|| v.strip_prefix("basic "))
    {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b.trim())
            .ok()?;
        let text = String::from_utf8(raw).ok()?;
        let (user, pass) = text.split_once(':').unwrap_or((text.as_str(), ""));
        return Some(if pass.is_empty() { user } else { pass }.to_string());
    }
    Some(v.to_string())
}

async fn registry(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let Some(w) = state.hub.get().cloned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "the forge is not running").into_response();
    };
    // Deactivated, deprovisioned or IP-blocked principals are refused here as on every RPC.
    if let Some(token) = credential(req.headers())
        && !token.contains('.')
        && let Ok((v, _)) = state.verify_token(&token).await
    {
        let ip = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let principals: Vec<String> = v.chain().into_iter().map(|p| p.id).collect();
        let app = w.app_token(&v).await;
        if let Err(e) = w.admit(&principals, &[], &ip, app).await {
            return (
                StatusCode::FORBIDDEN,
                format!("{}: {}", e.detail.reason, e.detail.message),
            )
                .into_response();
        }
    }
    let limit = state.cfg.limits.max_blob_bytes as usize + 1024 * 1024;
    let req = match to_request(req, limit).await {
        Ok(r) => r,
        Err(r) => return *r,
    };
    from_response(w.registry.handle(req).await)
}

/// `PUT`/`GET /pkg/<key>?expires=<ms>&signature=<hex>`: the signed URLs `BeginPackageUpload`
/// hands out (the forge's `HmacSigner` over the server's public URL), so a client can upload a
/// version's files before `FinalizePackageVersion` and fetch them back.
async fn signed_package_blob(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let key = req.uri().path().trim_start_matches('/').to_string();
    let query = req.uri().query().unwrap_or_default().to_string();
    let param = |name: &str| {
        query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.to_string())
    };
    let expires = param("expires").and_then(|v| v.parse::<u64>().ok());
    let signature = param("signature");
    let secret = match crate::server::url_key(&state.cfg) {
        Ok(k) => blake3::derive_key("bee forge signed urls v1", &k),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.detail.message).into_response();
        }
    };
    let signer = bee_forge::community::ports::HmacSigner::new(&state.public_url(), &secret);
    let method = req.method().as_str().to_string();
    let valid = match (expires, signature) {
        (Some(exp), Some(sig)) => signer.verify(&method, &key, exp, &sig, state.now_ms()),
        _ => false,
    };
    if !valid {
        return (StatusCode::FORBIDDEN, "invalid or expired signature").into_response();
    }
    let blobs = ServerPackageBlobs::new(&state);
    match method.as_str() {
        "PUT" => {
            let limit = state.cfg.limits.max_blob_bytes as usize;
            let data = match to_bytes(req.into_body(), limit).await {
                Ok(b) => b,
                Err(_) => {
                    return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
                        .into_response();
                }
            };
            match blobs.put(&key, data).await {
                Ok(()) => StatusCode::CREATED.into_response(),
                Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.message).into_response(),
            }
        }
        "GET" => match blobs.get(&key).await {
            Ok(Some(data)) => (StatusCode::OK, data).into_response(),
            Ok(None) => StatusCode::NOT_FOUND.into_response(),
            Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.message).into_response(),
        },
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

/// The package registry mounts, and the signed upload URLs of native package versions.
pub fn registry_router(state: Arc<AppState>) -> Router {
    let mut r = Router::new()
        .route("/v2", any(registry))
        .route("/v2/", any(registry))
        .route("/v2/{*rest}", any(registry))
        .route("/pkg/{*rest}", any(signed_package_blob));
    for p in ["npm", "maven", "nuget", "rubygems", "cargo"] {
        r = r.route(&format!("/{p}/{{*rest}}"), any(registry));
    }
    r.with_state(state)
}

async fn pages(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let Some(w) = state.hub.get().cloned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "the forge is not running").into_response();
    };
    let https = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| p.eq_ignore_ascii_case("https"));
    // Private sites: the caller from a bearer token, verified by the forge.
    let caller = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .and_then(|v| {
            let mut r = tonic::Request::new(());
            r.metadata_mut().insert("authorization", v);
            w.forge.authn.authenticate(&r).ok()
        });
    let req = match to_request(req, 1024 * 1024).await {
        Ok(r) => r,
        Err(r) => return *r,
    };
    from_response(w.forge.pages.respond(caller.as_ref(), &req, https).await)
}

/// The Pages listener's router: every request is answered by host.
pub fn pages_router(state: Arc<AppState>) -> Router {
    Router::new().fallback(pages).with_state(state)
}
