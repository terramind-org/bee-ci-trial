//! `GitService` (SPEC §17.4, `docs/spec/operations.md` §9): the git gateway as RPCs, for
//! frontends that speak the git protocols themselves (an SSH frontend, a separate HTTP edge)
//! and for tools that need exact git ids.
//!
//! Every call opens the same session the smart HTTP routes open
//! (`git_mount::open_session`): the caller's token, the repo gate, policy, push protection,
//! budgets, audit and the Land coalescer apply unchanged. Reads need the caller to read every
//! path of the repo (git carries whole trees), as for `git clone`.
//!
//! - `GetGitObjects` streams objects by git id in 1 MiB chunks (or sizes only).
//! - `GitListRefs` is `ls-refs`; `GitPlanFetch` negotiates and plans a pack, stores it as an
//!   object of the org (an upload by the caller) and returns its signed URL.
//! - `GitResolve` maps ids both ways; `imported` marks commits rebuilt byte-for-byte from an
//!   import.
//! - `CreateGitPackUpload` reserves a signed PUT URL (the LFS transfer route, keyed by the
//!   pack's SHA-256); `GitIngestCommits` runs receive-pack over that pack (or inline bytes).
//! - Settings: `git_enabled` and `push_auto_change` are the forge's repo settings; LFS, SSH,
//!   trailer and client-version settings are server rows. A repo with git disabled answers
//!   `GIT_DISABLED` on every gateway RPC; LFS off answers `INVALID_STATE` (`lfs_disabled`).
//! - `CreateGitCredential` mints an attenuated token for git; `ResolveGitSshKey` resolves a
//!   repo deploy key presented to an SSH frontend (called with a service or operator token).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use bee_core::{ObjectId, ObjectKind};
use bee_git::{GitHasher, GitOid, ObjectFormat, ObjectType};
use bee_git_http::gitmap::GitView;
use bee_git_http::walk::{Deepen, FetchArgs, Filter, plan};
use bee_git_http::{Access, GitError, RepoSession, advertise, lfs, receive};
use bee_policy::Action;
use bee_proto::v1::git_service_server::GitService;
use bee_proto::v1::{self, ErrorReason};
use bee_store::UploadRecord;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::Caller;
use crate::convert::{id_from, id_to};
use crate::errors::{ApiError, ApiResult};
use crate::services::support::{get_json, put_json, require_operator};
use crate::services::{Rpc, RpcStream, stream_of};
use crate::state::{AppState, RepoCtx, RequestMeta};
use crate::util::timestamp_ms;

/// Server-side git settings (`<repo id>`).
pub const NS_GIT_CONFIG: &str = "git_config";

/// Chunk size of `GetGitObjects`.
const CHUNK: usize = 1 << 20;

/// Server-side git settings of a repo.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GitSettings {
    /// LFS served.
    pub lfs_enabled: bool,
    /// SSH served.
    pub ssh_enabled: bool,
    /// Refuse commits without a change-id trailer.
    pub require_change_id_trailer: bool,
    /// Oldest git version accepted.
    pub min_client_version: String,
    /// Offer packfile-uris / bundle-uri.
    pub seed_packs_enabled: bool,
}

impl Default for GitSettings {
    fn default() -> Self {
        GitSettings {
            lfs_enabled: true,
            ssh_enabled: true,
            require_change_id_trailer: false,
            min_client_version: String::new(),
            seed_packs_enabled: false,
        }
    }
}

/// The git service.
pub struct GitApi {
    state: Arc<AppState>,
}

impl GitApi {
    /// The service over `state`.
    pub fn new(state: Arc<AppState>) -> GitApi {
        GitApi { state }
    }
}

fn git_err(e: GitError) -> ApiError {
    let reason = bee_proto::errors::parse_reason(&e.reason).unwrap_or(ErrorReason::Internal);
    ApiError::new(reason, e.message)
}

fn format_to(f: ObjectFormat) -> i32 {
    match f {
        ObjectFormat::Sha1 => v1::GitObjectFormat::Sha1 as i32,
        ObjectFormat::Sha256 => v1::GitObjectFormat::Sha256 as i32,
    }
}

fn type_to(t: ObjectType) -> i32 {
    let t = match t {
        ObjectType::Commit => v1::GitObjectType::Commit,
        ObjectType::Tree => v1::GitObjectType::Tree,
        ObjectType::Blob => v1::GitObjectType::Blob,
        ObjectType::Tag => v1::GitObjectType::Tag,
    };
    t as i32
}

fn parse_oid(format: ObjectFormat, s: &str) -> ApiResult<GitOid> {
    format
        .parse_hex(s.trim().as_bytes())
        .map_err(|_| ApiError::invalid("invalid_oid", format!("{s:?} is not a git object id")))
}

/// A gateway session with the caller's token.
struct Session {
    session: Arc<dyn RepoSession>,
    repo: RepoCtx,
}

/// The server-side git settings of a repo (defaults when never set).
pub(crate) async fn settings_of(state: &AppState, repo: &RepoCtx) -> ApiResult<GitSettings> {
    Ok(get_json(state.kv.as_ref(), NS_GIT_CONFIG, &repo.id_text())
        .await?
        .unwrap_or_default())
}

/// The forge's `git_enabled` repo setting (on without a forge).
pub(crate) async fn git_enabled(state: &AppState, repo: &RepoCtx) -> bool {
    match state.hub.get() {
        Some(w) => w
            .forge
            .orgs
            .repo_settings(&bee_forge::Cx::system(), &repo.id_text())
            .await
            .map(|s| s.git_enabled)
            .unwrap_or(true),
        None => true,
    }
}

/// The oldest git a repo accepts when it sets none.
pub(crate) const DEFAULT_MIN_CLIENT_VERSION: &str = "2.30.0";

impl GitApi {
    async fn settings(&self, repo: &RepoCtx) -> ApiResult<GitSettings> {
        settings_of(&self.state, repo).await
    }

    async fn git_enabled(&self, repo: &RepoCtx) -> bool {
        git_enabled(&self.state, repo).await
    }

    async fn open(
        &self,
        md: &tonic::metadata::MetadataMap,
        r: Option<&v1::RepoRef>,
        access: Access,
    ) -> ApiResult<Session> {
        let s = &self.state;
        let token = AppState::bearer(md).ok_or_else(|| {
            ApiError::unauthenticated("this call needs `Authorization: Bearer <token>`")
        })?;
        let repo = s.repo(r).await?;
        // The repo gate first, so a repo the caller cannot see is `NOT_FOUND` either way.
        let session = crate::git_mount::open_session(s, &token, &repo.full_name, access)
            .await
            .map_err(git_err)?;
        if !self.git_enabled(&repo).await {
            return Err(ApiError::new(
                ErrorReason::GitDisabled,
                format!("git is disabled for {}", repo.full_name),
            ));
        }
        Ok(Session { session, repo })
    }

    async fn objects(
        &self,
        md: &tonic::metadata::MetadataMap,
        r: v1::GetGitObjectsRequest,
    ) -> ApiResult<Vec<v1::GitObjectChunk>> {
        let ses = self.open(md, r.repo.as_ref(), Access::Read).await?;
        let history = ses.session.history();
        let format = advertise::repo_format(ses.session.as_ref())
            .await
            .map_err(git_err)?;
        let mut view = GitView::new(history, format);
        if r.oids.len() > 10_000 {
            return Err(ApiError::invalid("too_many", "at most 10000 oids per call"));
        }
        let mut out = Vec::new();
        for (i, text) in r.oids.iter().enumerate() {
            let fail = |oid: &str, reason: &str| v1::GitObjectChunk {
                oid: oid.to_string(),
                last: true,
                error_reason: reason.to_string(),
                ..Default::default()
            };
            let Ok(oid) = parse_oid(format, text) else {
                out.push(fail(text, "INVALID_ARGUMENT"));
                continue;
            };
            let raw = match view.bee_of(&oid).await.map_err(git_err)? {
                Some(id) => match id.kind() {
                    ObjectKind::Changeset => match view.commit_object(&id).await {
                        Ok((_, raw)) => Some(raw),
                        Err(e) if e.reason == "REDACTED" => {
                            out.push(fail(text, "REDACTED"));
                            continue;
                        }
                        Err(e) => return Err(git_err(e)),
                    },
                    ObjectKind::Tree => Some(view.tree_object(&id).await.map_err(git_err)?.raw),
                    ObjectKind::Blob => match view.blob_bytes(&id).await {
                        Ok(b) => Some(bee_git::RawObject::new(ObjectType::Blob, b)),
                        Err(e) if e.reason == "REDACTED" => {
                            out.push(fail(text, "REDACTED"));
                            continue;
                        }
                        Err(e) => return Err(git_err(e)),
                    },
                    _ => None,
                },
                None => bee_import::lookup_git_tag(history.meta(), history.repo(), &oid)
                    .await?
                    .map(|b| bee_git::RawObject::new(ObjectType::Tag, b)),
            };
            let Some(raw) = raw else {
                out.push(fail(text, "NOT_FOUND"));
                continue;
            };
            let size = raw.data.len() as u64;
            if r.sizes_only {
                out.push(v1::GitObjectChunk {
                    oid: oid.to_hex(),
                    r#type: type_to(raw.kind),
                    size,
                    last: true,
                    ..Default::default()
                });
                continue;
            }
            let skip = if i == 0 { r.resume_offset as usize } else { 0 };
            let data = raw.data.get(skip.min(raw.data.len())..).unwrap_or(&[]);
            if data.is_empty() {
                out.push(v1::GitObjectChunk {
                    oid: oid.to_hex(),
                    r#type: type_to(raw.kind),
                    size,
                    offset: skip as u64,
                    last: true,
                    ..Default::default()
                });
                continue;
            }
            let n = data.len().div_ceil(CHUNK);
            for (k, c) in data.chunks(CHUNK).enumerate() {
                out.push(v1::GitObjectChunk {
                    oid: oid.to_hex(),
                    r#type: type_to(raw.kind),
                    size,
                    offset: (skip + k * CHUNK) as u64,
                    data: c.to_vec(),
                    last: k + 1 == n,
                    ..Default::default()
                });
            }
        }
        view.flush().await.map_err(git_err)?;
        Ok(out)
    }

    async fn list_refs(
        &self,
        md: &tonic::metadata::MetadataMap,
        r: v1::GitListRefsRequest,
    ) -> ApiResult<v1::GitListRefsResponse> {
        let access = if r.for_push {
            Access::Write
        } else {
            Access::Read
        };
        let ses = self.open(md, r.repo.as_ref(), access).await?;
        let history = ses.session.history();
        let format = advertise::repo_format(ses.session.as_ref())
            .await
            .map_err(git_err)?;
        let mut view = GitView::new(history, format);
        let list = advertise::list(ses.session.as_ref(), &mut view)
            .await
            .map_err(git_err)?;
        let wanted = |name: &str| {
            let kind_ok = (r.include_pull_refs || !name.starts_with("refs/pull/"))
                && (r.include_virtual_refs || !name.starts_with("refs/bee/"))
                && (r.include_replace_refs || !name.starts_with("refs/replace/"));
            kind_ok
                && (r.ref_prefixes.is_empty() || r.ref_prefixes.iter().any(|p| name.starts_with(p)))
        };
        let mut refs = Vec::new();
        if wanted("HEAD") || r.ref_prefixes.iter().any(|p| p == "HEAD") {
            match list.head_ref() {
                Some(h) => refs.push(v1::GitRefAdvertisement {
                    name: "HEAD".into(),
                    oid: h.oid.to_hex(),
                    symref_target: if r.symrefs {
                        list.head.clone()
                    } else {
                        String::new()
                    },
                    version: h.version,
                    ..Default::default()
                }),
                None if r.unborn => refs.push(v1::GitRefAdvertisement {
                    name: "HEAD".into(),
                    symref_target: list.head.clone(),
                    unborn: true,
                    ..Default::default()
                }),
                None => {}
            }
        }
        for a in &list.refs {
            if !wanted(&a.name) {
                continue;
            }
            refs.push(v1::GitRefAdvertisement {
                name: a.name.clone(),
                oid: a.oid.to_hex(),
                peeled_oid: if r.peel {
                    a.peeled.map(|p| p.to_hex()).unwrap_or_default()
                } else {
                    String::new()
                },
                version: a.version,
                ..Default::default()
            });
        }
        let policy = self.state.policy(&ses.repo, "main").await?;
        let (epoch, _) = self.state.epochs(&ses.repo, &policy).await?;
        let mut caps = vec![
            "ls-refs=unborn".to_string(),
            "fetch=shallow wait-for-done filter".into(),
            format!(
                "object-format={}",
                if format == ObjectFormat::Sha256 {
                    "sha256"
                } else {
                    "sha1"
                }
            ),
            "object-info".into(),
        ];
        if r.for_push {
            caps = bee_git_http::upload::receive_caps(format)
                .split(' ')
                .map(str::to_string)
                .collect();
        }
        Ok(v1::GitListRefsResponse {
            refs,
            object_format: format_to(format),
            capabilities: caps,
            policy_epoch: epoch.to_string(),
        })
    }

    async fn plan_fetch(
        &self,
        caller: &Caller,
        md: &tonic::metadata::MetadataMap,
        r: v1::GitPlanFetchRequest,
    ) -> ApiResult<v1::GitPlanFetchResponse> {
        let s = &self.state;
        let ses = self.open(md, r.repo.as_ref(), Access::Read).await?;
        let history = ses.session.history();
        let format = advertise::repo_format(ses.session.as_ref())
            .await
            .map_err(git_err)?;
        let mut view = GitView::new(history, format);
        let list = advertise::list(ses.session.as_ref(), &mut view)
            .await
            .map_err(git_err)?;
        let mut wants = Vec::new();
        for w in &r.want_oids {
            wants.push(parse_oid(format, w)?);
        }
        let mut wanted_refs = Vec::new();
        for name in &r.want_refs {
            let a = list
                .get(name)
                .ok_or_else(|| ApiError::not_found(format!("want-ref {name} is not a ref")))?;
            wanted_refs.push(v1::GitWantedRef {
                name: name.clone(),
                oid: a.oid.to_hex(),
            });
            wants.push(a.oid);
        }
        if wants.is_empty() {
            return Err(ApiError::invalid(
                "missing_field",
                "want_oids or want_refs is required",
            ));
        }
        let mut haves = Vec::new();
        for h in &r.have_oids {
            haves.push(parse_oid(format, h)?);
        }
        let mut shallow = Vec::new();
        for h in &r.shallow_oids {
            shallow.push(parse_oid(format, h)?);
        }
        let deepen = if !r.deepen_not.is_empty() {
            let mut ids = Vec::new();
            for n in &r.deepen_not {
                let a = list
                    .get(n)
                    .or_else(|| list.get(&format!("refs/heads/{n}")))
                    .ok_or_else(|| ApiError::not_found(format!("deepen-not {n} is not a ref")))?;
                ids.push(a.target);
            }
            Some(Deepen::Not(ids))
        } else if let Some(t) = &r.deepen_since {
            Some(Deepen::Since(t.seconds))
        } else if r.deepen > 0 {
            Some(if r.deepen_relative {
                Deepen::Relative(r.deepen)
            } else {
                Deepen::Depth(r.deepen)
            })
        } else {
            None
        };
        let filter = match r.filter.as_ref().and_then(|f| f.filter.as_ref()) {
            Some(v1::git_fetch_filter::Filter::BlobNone(true)) => Some(Filter::BlobNone),
            Some(v1::git_fetch_filter::Filter::BlobLimit(n)) => Some(Filter::BlobLimit(*n)),
            Some(_) => match r
                .filter
                .as_ref()
                .map(|f| f.spec.as_str())
                .filter(|s| !s.is_empty())
            {
                Some(spec) => Some(Filter::parse(spec).map_err(git_err)?),
                None => None,
            },
            None => None,
        };
        let args = FetchArgs {
            wants,
            haves: haves.clone(),
            shallow,
            deepen,
            filter,
            ofs_delta: r.ofs_delta,
            include_tag: r.include_tag,
            thin_pack: r.thin_pack && r.filter.is_none(),
        };
        let p = plan(&mut view, &args, &list.tags).await.map_err(git_err)?;
        view.flush().await.map_err(git_err)?;
        let ready = r.done || haves.is_empty() || !p.common.is_empty();
        let acks = v1::GitFetchAcks {
            common_oids: p.common.iter().map(GitOid::to_hex).collect(),
            nak: p.common.is_empty() && !haves.is_empty(),
            ready,
        };
        let policy = s.policy(&ses.repo, "main").await?;
        let (epoch, _) = s.epochs(&ses.repo, &policy).await?;
        let mut resp = v1::GitPlanFetchResponse {
            acks: Some(acks),
            wanted_refs,
            shallow_oids: p.shallow.iter().map(GitOid::to_hex).collect(),
            unshallow_oids: p.unshallow.iter().map(GitOid::to_hex).collect(),
            object_count: p.objects as u64,
            estimated_bytes: p.pack.len() as u64,
            policy_epoch: epoch.to_string(),
            ..Default::default()
        };
        if ready {
            let key = bee_git_http::cache::key(&ses.repo.full_name, &args, &p.common, &[]);
            resp.pack_cache_key = bee_core::hex::encode(&key);
            let id = bee_object::blob_id(&p.pack);
            s.writer
                .upload(
                    ses.repo.org.id,
                    vec![(id, p.pack)],
                    Some(UploadRecord {
                        repo: Some(ses.repo.repo.id),
                        principal: caller.id().to_string(),
                    }),
                    s.now_ms(),
                )
                .await?;
            resp.pack_cached = true;
            if s.url_issuer.is_some() {
                let u = s
                    .issue_url(
                        ses.repo.org.id,
                        &id,
                        Duration::from_secs(s.cfg.blobs.signed_url_ttl_secs),
                        Some(caller),
                        &[&ses.repo],
                    )
                    .await?;
                resp.pack_url = u.url;
                resp.pack_url_expires_at = Some(timestamp_ms(u.expires_at_ms));
            }
        }
        Ok(resp)
    }

    async fn resolve(
        &self,
        md: &tonic::metadata::MetadataMap,
        r: v1::GitResolveRequest,
    ) -> ApiResult<v1::GitResolveResponse> {
        if r.queries.len() > 1000 {
            return Err(ApiError::invalid("too_many", "at most 1000 queries"));
        }
        let ses = self.open(md, r.repo.as_ref(), Access::Read).await?;
        let history = ses.session.history();
        let format = advertise::repo_format(ses.session.as_ref())
            .await
            .map_err(git_err)?;
        let mut view = GitView::new(history, format);
        let mut results = Vec::new();
        for q in &r.queries {
            let not_found = v1::GitResolveResult {
                status: v1::ItemStatus::NotFound as i32,
                ..Default::default()
            };
            let (id, oid) = match &q.id {
                Some(v1::git_resolve_query::Id::GitOid(g)) => {
                    let Ok(oid) = parse_oid(format, g) else {
                        results.push(not_found);
                        continue;
                    };
                    match view.bee_of(&oid).await.map_err(git_err)? {
                        Some(id) => (id, oid),
                        None => {
                            results.push(not_found);
                            continue;
                        }
                    }
                }
                Some(v1::git_resolve_query::Id::BeeId(sid)) => {
                    let Ok(id) = id_from(Some(sid), "bee_id") else {
                        results.push(not_found);
                        continue;
                    };
                    let oid = match id.kind() {
                        ObjectKind::Changeset if history.has_changeset(&id).await? => {
                            view.commit_oid(&id).await
                        }
                        ObjectKind::Tree | ObjectKind::Blob => view.git_of(&id).await,
                        _ => {
                            results.push(not_found);
                            continue;
                        }
                    };
                    match oid {
                        Ok(o) => (id, o),
                        Err(e) if e.reason == "REDACTED" => {
                            results.push(v1::GitResolveResult {
                                status: v1::ItemStatus::Redacted as i32,
                                bee_id: Some(id_to(&id)),
                                ..Default::default()
                            });
                            continue;
                        }
                        Err(_) => {
                            results.push(not_found);
                            continue;
                        }
                    }
                }
                None => {
                    results.push(not_found);
                    continue;
                }
            };
            let (git_type, kind, change_id, imported) = match id.kind() {
                ObjectKind::Changeset => {
                    let cs = history.changeset(&id).await?;
                    let imported = match history.meta_object(cs.meta()).await? {
                        Some(m) => match &m.git {
                            Some(ext) => {
                                let root = view.tree_oid(cs.root()).await.map_err(git_err)?;
                                let c = bee_git::export::imported_commit(root, ext, &m.message);
                                format
                                    .hash_object(ObjectType::Commit, &c.serialize())
                                    .map(|h| h == oid)
                                    .unwrap_or(false)
                            }
                            None => false,
                        },
                        None => false,
                    };
                    (
                        v1::GitObjectType::Commit,
                        v1::ObjectKind::Changeset,
                        Some(v1::ChangeId {
                            raw: cs.change_id().as_bytes().to_vec(),
                        }),
                        imported,
                    )
                }
                ObjectKind::Tree => (v1::GitObjectType::Tree, v1::ObjectKind::Tree, None, false),
                _ => (v1::GitObjectType::Blob, v1::ObjectKind::Blob, None, false),
            };
            results.push(v1::GitResolveResult {
                status: v1::ItemStatus::Ok as i32,
                git_oid: oid.to_hex(),
                git_type: git_type as i32,
                bee_id: Some(id_to(&id)),
                bee_kind: kind as i32,
                change_id,
                imported,
            });
        }
        view.flush().await.map_err(git_err)?;
        Ok(v1::GitResolveResponse {
            results,
            object_format: format_to(format),
        })
    }

    async fn pack_upload(
        &self,
        caller: &Caller,
        md: &tonic::metadata::MetadataMap,
        r: v1::CreateGitPackUploadRequest,
    ) -> ApiResult<v1::CreateGitPackUploadResponse> {
        let s = &self.state;
        let ses = self.open(md, r.repo.as_ref(), Access::Write).await?;
        let sha = r.sha256.to_ascii_lowercase();
        lfs::parse_oid(&sha).map_err(git_err)?;
        let max = s.cfg.limits.max_blob_bytes;
        if r.size_bytes == 0 || r.size_bytes > max {
            let mut e = ApiError::new(
                ErrorReason::TooLarge,
                format!("packs are at most {max} bytes"),
            );
            e.detail.payload = Some(v1::error_detail::Payload::LimitExceeded(
                v1::LimitExceeded {
                    limit: "pack_bytes".into(),
                    maximum: max,
                    actual: r.size_bytes,
                },
            ));
            return Err(e);
        }
        let now_s = s.now_ms() / 1000;
        let signed = lfs::Signed {
            method: "PUT".into(),
            repo: ses.repo.full_name.clone(),
            oid: sha.clone(),
            size: r.size_bytes,
            exp: now_s + lfs::URL_TTL_SECS,
            principal: caller.id().to_string(),
        };
        let key = crate::git_mount::lfs_url_key(s)?;
        Ok(v1::CreateGitPackUploadResponse {
            upload_id: sha,
            upload_url: signed.url(&s.public_url(), &key).map_err(git_err)?,
            expires_at: Some(timestamp_ms(signed.exp * 1000)),
            max_bytes: max,
        })
    }

    async fn ingest(
        &self,
        caller: &Caller,
        md: &tonic::metadata::MetadataMap,
        r: v1::GitIngestCommitsRequest,
    ) -> ApiResult<v1::GitIngestCommitsResponse> {
        let s = &self.state;
        let ses = self.open(md, r.repo.as_ref(), Access::Write).await?;
        let history = ses.session.history();
        let format = advertise::repo_format(ses.session.as_ref())
            .await
            .map_err(git_err)?;
        let pack = match &r.pack {
            Some(v1::git_ingest_commits_request::Pack::PackBytes(b)) => {
                if b.len() > 1 << 20 {
                    return Err(ApiError::invalid(
                        "pack_too_large_inline",
                        "inline packs are at most 1 MiB; use CreateGitPackUpload",
                    ));
                }
                b.clone()
            }
            Some(v1::git_ingest_commits_request::Pack::PackUploadId(id)) => {
                let signed = lfs::Signed {
                    method: "GET".into(),
                    repo: ses.repo.full_name.clone(),
                    oid: id.to_ascii_lowercase(),
                    size: 0,
                    exp: 0,
                    principal: caller.id().to_string(),
                };
                lfs::get(history, &signed).await.map_err(git_err)?
            }
            None => Vec::new(),
        };
        let mut commands = Vec::new();
        for c in &r.commands {
            commands.push(receive::Command {
                old: parse_oid(format, &c.old_oid)?,
                new: parse_oid(format, &c.new_oid)?,
                name: c.ref_name.clone(),
            });
        }
        if commands.is_empty() {
            return Err(ApiError::invalid("missing_field", "commands is required"));
        }
        let mut caps = vec!["report-status".to_string()];
        if r.atomic {
            caps.push("atomic".into());
        }
        if !r.push_options.is_empty() {
            caps.push("push-options".into());
        }
        let req = receive::PushRequest {
            commands: commands.clone(),
            caps,
            options: r.push_options.clone(),
            shallow: Vec::new(),
            pack,
        };
        let mut view = GitView::new(history, format);
        let list = advertise::list(ses.session.as_ref(), &mut view)
            .await
            .map_err(git_err)?;
        let (unpack, outcomes) =
            receive::execute(ses.session.as_ref(), &mut view, &list, &req, s.now_ms())
                .await
                .map_err(git_err)?;
        let mut results = Vec::new();
        let mut change_ids = Vec::new();
        let mut atomic_rejected = r.atomic
            && outcomes
                .iter()
                .any(|o| matches!(o, receive::Outcome::Ng(_)));
        for (c, o) in commands.iter().zip(&outcomes) {
            let bee = bee_git_http::refs::bee_name(&c.name, caller.id()).ok();
            let protected = match &bee {
                Some(n) => ses.session.is_protected(n).await.unwrap_or(false),
                None => false,
            };
            match o {
                receive::Outcome::Ok { new } => {
                    let final_oid = new.unwrap_or(c.new);
                    let changeset = if final_oid.is_null() {
                        None
                    } else {
                        view.bee_of(&final_oid).await.map_err(git_err)?
                    };
                    let version = match &bee {
                        Some(n) => s
                            .meta
                            .get_ref(ses.repo.repo.id, n)
                            .await?
                            .map(|r| r.version)
                            .unwrap_or(0),
                        None => 0,
                    };
                    let applied = if c.new.is_null() {
                        v1::GitPushApplyKind::RefDelete
                    } else if c.name.starts_with("refs/heads/ws/") {
                        v1::GitPushApplyKind::WorkspaceUpdate
                    } else if protected {
                        v1::GitPushApplyKind::Land
                    } else {
                        v1::GitPushApplyKind::RefUpdate
                    };
                    results.push(v1::GitRefUpdateResult {
                        ref_name: c.name.clone(),
                        ok: true,
                        applied: applied as i32,
                        result_oid: if final_oid.is_null() {
                            String::new()
                        } else {
                            final_oid.to_hex()
                        },
                        changeset: changeset.as_ref().map(id_to),
                        ref_version: version,
                        ..Default::default()
                    });
                }
                receive::Outcome::Ng(e) => {
                    let api = git_err(e.clone());
                    results.push(v1::GitRefUpdateResult {
                        ref_name: c.name.clone(),
                        ok: false,
                        error: Some(*api.detail),
                        ..Default::default()
                    });
                }
            }
            if !c.new.is_null()
                && let Some(id) = view.bee_of(&c.new).await.map_err(git_err)?
                && id.kind() == ObjectKind::Changeset
            {
                let cs = history.changeset(&id).await?;
                change_ids.push(v1::GitChangeIdMapping {
                    git_oid: c.new.to_hex(),
                    changeset: Some(id_to(&id)),
                    change_id: Some(v1::ChangeId {
                        raw: cs.change_id().as_bytes().to_vec(),
                    }),
                    change_id_trailer: bee_core::hex::encode(cs.change_id().as_bytes()),
                    trailer_added: false,
                });
            }
        }
        view.flush().await.map_err(git_err)?;
        if unpack.is_err() {
            atomic_rejected = r.atomic;
        }
        Ok(v1::GitIngestCommitsResponse {
            unpack_ok: unpack.is_ok(),
            unpack_error: unpack.err().map(|e| e.message).unwrap_or_default(),
            results,
            change_ids,
            messages: Vec::new(),
            atomic_rejected,
        })
    }

    async fn config_of(&self, caller: &Caller, repo: &RepoCtx) -> ApiResult<v1::GitRepoConfig> {
        let s = &self.state;
        s.gate(caller, repo, Action::Read, None).await?;
        let settings = self.settings(repo).await?;
        let (git_enabled, auto) = match s.hub.get() {
            Some(w) => match w
                .forge
                .orgs
                .repo_settings(&bee_forge::Cx::system(), &repo.id_text())
                .await
            {
                Ok(r) => (r.git_enabled, r.push_auto_change),
                Err(_) => (true, false),
            },
            None => (true, false),
        };
        let tips: Vec<ObjectId> = s
            .meta
            .list_refs(repo.repo.id, "")
            .await?
            .iter()
            .filter_map(|r| r.target)
            .collect();
        let history = s.history(repo);
        let format = bee_git_http::gitmap::detect_format(&history, &tips)
            .await
            .map_err(git_err)?;
        Ok(v1::GitRepoConfig {
            repo: Some(v1::RepoRef {
                repo: Some(v1::repo_ref::Repo::FullName(repo.full_name.clone())),
            }),
            git_enabled,
            object_format: format_to(format),
            lfs_enabled: settings.lfs_enabled,
            ssh_enabled: settings.ssh_enabled,
            push_auto_change: auto,
            require_change_id_trailer: settings.require_change_id_trailer,
            min_client_version: settings.min_client_version,
            seed_packs_enabled: settings.seed_packs_enabled,
        })
    }

    async fn update_config(
        &self,
        caller: &Caller,
        r: v1::UpdateGitConfigRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::GitRepoConfig> {
        let s = &self.state;
        let repo = s.repo(r.repo.as_ref()).await?;
        s.gate(caller, &repo, Action::ManageSettings, None).await?;
        if let Some(f) = r.object_format {
            let current = self.config_of(caller, &repo).await?.object_format;
            if f != current {
                // Only the root changeset exists: no history yet.
                let has_history = s.meta.list_refs(repo.repo.id, "").await?.iter().any(|x| {
                    x.target
                        .is_some_and(|t| t != bee_object::Changeset::empty_root().id())
                });
                if has_history {
                    return Err(ApiError::new(
                        ErrorReason::InvalidState,
                        "the object format can only change while the repo has no history",
                    ));
                }
                if f != v1::GitObjectFormat::Sha1 as i32 {
                    return Err(ApiError::invalid(
                        "unsupported_format",
                        "new history is written as SHA-1; SHA-256 repos come from importing SHA-256 history",
                    ));
                }
            }
        }
        let mut st = self.settings(&repo).await?;
        if let Some(v) = r.lfs_enabled {
            st.lfs_enabled = v;
        }
        if let Some(v) = r.ssh_enabled {
            st.ssh_enabled = v;
        }
        if let Some(v) = r.require_change_id_trailer {
            st.require_change_id_trailer = v;
        }
        if let Some(v) = &r.min_client_version {
            if !v.is_empty() && parse_version(v).is_none() {
                return Err(ApiError::invalid(
                    "bad_version",
                    "min_client_version is like 2.30.0",
                ));
            }
            st.min_client_version = v.clone();
        }
        if let Some(v) = r.seed_packs_enabled {
            // Refused rather than stored unenforced: the gateway serves no packfile-uris or
            // bundle-uri (`docs/spec/git-gateway.md` §9).
            if v {
                return Err(ApiError::invalid(
                    "unsupported_setting",
                    "seed packs (packfile-uris, bundle-uri) are not served by this gateway",
                ));
            }
            st.seed_packs_enabled = v;
        }
        put_json(s.kv.as_ref(), NS_GIT_CONFIG, &repo.id_text(), &st).await?;
        if r.git_enabled.is_some() || r.push_auto_change.is_some() {
            let w = s.hub.get().ok_or_else(|| {
                ApiError::new(ErrorReason::Unavailable, "repo settings need the forge")
            })?;
            let cx = crate::forge::landing::caller_cx(caller);
            let (ge, pa) = (r.git_enabled, r.push_auto_change);
            w.forge
                .orgs
                .update_repo_settings(&cx, &repo.id_text(), 0, move |x| {
                    if let Some(v) = ge {
                        x.git_enabled = v;
                    }
                    if let Some(v) = pa {
                        x.push_auto_change = v;
                    }
                    Ok(())
                })
                .await
                .map_err(crate::forge::forge_to_api)?;
        }
        s.audit(
            caller,
            meta,
            Some(&repo),
            &repo.id_text(),
            "git_config.update",
            json!({}),
        )
        .await?;
        self.config_of(caller, &repo).await
    }
}

pub(crate) fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let v = v
        .trim()
        .trim_start_matches("git/")
        .trim_start_matches("git version ");
    let mut it = v.split(['.', '-', ' ']);
    let a = it.next()?.parse().ok()?;
    let b = it.next().unwrap_or("0").parse().ok()?;
    let c = it.next().unwrap_or("0").parse().unwrap_or(0);
    Some((a, b, c))
}

/// The SHA-256 fingerprint of an SSH public key blob (`SHA256:<base64, no padding>`).
fn fingerprint(blob: &[u8]) -> String {
    let mut h = GitHasher::new(ObjectFormat::Sha256);
    h.update(blob);
    let d = h
        .finish()
        .map(|o| o.as_bytes().to_vec())
        .unwrap_or_default();
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(d)
    )
}

macro_rules! git_rpc {
    ($self:ident, $req:ident, |$caller:ident, $inner:ident, $meta:ident| $body:expr) => {
        $crate::unary!($self.state, $req, |$caller, $inner, $meta| $body)
    };
}

#[tonic::async_trait]
impl GitService for GitApi {
    type GetGitObjectsStream = RpcStream<v1::GitObjectChunk>;

    async fn get_git_objects(
        &self,
        req: tonic::Request<v1::GetGitObjectsRequest>,
    ) -> Rpc<Self::GetGitObjectsStream> {
        let meta = RequestMeta::of(&req);
        let md = req.metadata().clone();
        let r = async {
            self.state.caller(&md).await?;
            let chunks = self.objects(&md, req.into_inner()).await?;
            Ok::<_, ApiError>(tonic::Response::new(stream_of(
                chunks.into_iter().map(Ok).collect(),
            )))
        }
        .await;
        crate::services::object::finish(&meta, r)
    }

    async fn git_list_refs(
        &self,
        req: tonic::Request<v1::GitListRefsRequest>,
    ) -> Rpc<v1::GitListRefsResponse> {
        let md = req.metadata().clone();
        git_rpc!(self, req, |caller, inner, meta| {
            let _ = &caller;
            self.list_refs(&md, inner)
        })
    }

    async fn git_plan_fetch(
        &self,
        req: tonic::Request<v1::GitPlanFetchRequest>,
    ) -> Rpc<v1::GitPlanFetchResponse> {
        let md = req.metadata().clone();
        git_rpc!(self, req, |caller, inner, meta| self
            .plan_fetch(&caller, &md, inner))
    }

    async fn git_resolve(
        &self,
        req: tonic::Request<v1::GitResolveRequest>,
    ) -> Rpc<v1::GitResolveResponse> {
        let md = req.metadata().clone();
        git_rpc!(self, req, |caller, inner, meta| {
            let _ = &caller;
            self.resolve(&md, inner)
        })
    }

    async fn create_git_pack_upload(
        &self,
        req: tonic::Request<v1::CreateGitPackUploadRequest>,
    ) -> Rpc<v1::CreateGitPackUploadResponse> {
        let md = req.metadata().clone();
        git_rpc!(self, req, |caller, inner, meta| self
            .pack_upload(&caller, &md, inner))
    }

    async fn git_ingest_commits(
        &self,
        req: tonic::Request<v1::GitIngestCommitsRequest>,
    ) -> Rpc<v1::GitIngestCommitsResponse> {
        let md = req.metadata().clone();
        git_rpc!(self, req, |caller, inner, meta| self
            .ingest(&caller, &md, inner))
    }

    async fn get_git_config(
        &self,
        req: tonic::Request<v1::GetGitConfigRequest>,
    ) -> Rpc<v1::GitRepoConfig> {
        git_rpc!(self, req, |caller, inner, meta| async {
            let repo = self.state.repo(inner.repo.as_ref()).await?;
            self.config_of(&caller, &repo).await
        })
    }

    async fn update_git_config(
        &self,
        req: tonic::Request<v1::UpdateGitConfigRequest>,
    ) -> Rpc<v1::GitRepoConfig> {
        git_rpc!(self, req, |caller, inner, meta| self
            .update_config(&caller, inner, &meta))
    }

    async fn git_lfs_batch(
        &self,
        req: tonic::Request<v1::GitLfsBatchRequest>,
    ) -> Rpc<v1::GitLfsBatchResponse> {
        let md = req.metadata().clone();
        git_rpc!(self, req, |caller, inner, meta| async {
            let _ = &caller;
            let s = &self.state;
            let upload = match v1::GitLfsOperation::try_from(inner.operation) {
                Ok(v1::GitLfsOperation::Upload) => true,
                Ok(v1::GitLfsOperation::Download) => false,
                _ => return Err(ApiError::invalid("missing_field", "operation is required")),
            };
            let ses = self
                .open(
                    &md,
                    inner.repo.as_ref(),
                    if upload { Access::Write } else { Access::Read },
                )
                .await?;
            if !self.settings(&ses.repo).await?.lfs_enabled {
                let mut e =
                    ApiError::new(ErrorReason::InvalidState, "LFS is disabled for this repo");
                e.detail.subcode = "lfs_disabled".into();
                return Err(e);
            }
            if inner.objects.len() > 1000 {
                return Err(ApiError::invalid("too_many", "at most 1000 objects"));
            }
            let breq = lfs::BatchRequest {
                operation: if upload { "upload" } else { "download" }.into(),
                transfers: inner.transfers.clone(),
                objects: inner
                    .objects
                    .iter()
                    .map(|o| lfs::BatchObject {
                        oid: o.oid.clone(),
                        size: o.size,
                    })
                    .collect(),
                hash_algo: (!inner.hash_algo.is_empty()).then(|| inner.hash_algo.clone()),
            };
            let key = crate::git_mount::lfs_url_key(s)?;
            let v = lfs::batch(
                ses.session.as_ref(),
                &breq,
                &s.public_url(),
                &key,
                s.now_ms() / 1000,
            )
            .await
            .map_err(git_err)?;
            let mut objects = Vec::new();
            for o in v["objects"].as_array().cloned().unwrap_or_default() {
                let mut actions = Vec::new();
                if let Some(map) = o["actions"].as_object() {
                    for (kind, a) in map {
                        actions.push(v1::GitLfsAction {
                            kind: kind.clone(),
                            href: a["href"].as_str().unwrap_or_default().to_string(),
                            headers: Vec::new(),
                            expires_at: Some(timestamp_ms(
                                s.now_ms() + a["expires_in"].as_u64().unwrap_or(0) * 1000,
                            )),
                        });
                    }
                }
                objects.push(v1::GitLfsObjectResult {
                    oid: o["oid"].as_str().unwrap_or_default().to_string(),
                    size: o["size"].as_u64().unwrap_or(0),
                    authenticated: o["authenticated"].as_bool().unwrap_or(false),
                    actions,
                    error_code: o["error"]["code"].as_u64().unwrap_or(0) as u32,
                    error_message: o["error"]["message"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            Ok::<_, ApiError>(v1::GitLfsBatchResponse {
                transfer: "basic".into(),
                objects,
                hash_algo: "sha256".into(),
            })
        })
    }

    async fn git_preflight(
        &self,
        req: tonic::Request<v1::GitPreflightRequest>,
    ) -> Rpc<v1::GitPreflightResponse> {
        let md = req.metadata().clone();
        git_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            let repo = s.repo(inner.repo.as_ref()).await?;
            let cfg = self.config_of(&caller, &repo).await?;
            let mut problems = Vec::new();
            let mut add = |sev: v1::GitPreflightSeverity, code: &str, msg: String, fix: &str| {
                problems.push(v1::GitPreflightProblem {
                    severity: sev as i32,
                    code: code.into(),
                    message: msg,
                    fix: fix.into(),
                })
            };
            let min = if cfg.min_client_version.is_empty() {
                DEFAULT_MIN_CLIENT_VERSION.to_string()
            } else {
                cfg.min_client_version.clone()
            };
            match (
                parse_version(&inner.client_git_version),
                parse_version(&min),
            ) {
                (Some(have), Some(need)) if have < need => add(
                    v1::GitPreflightSeverity::Error,
                    "git_too_old",
                    format!("git {} is older than {min}", inner.client_git_version),
                    "upgrade git",
                ),
                (None, _) if !inner.client_git_version.is_empty() => add(
                    v1::GitPreflightSeverity::Warning,
                    "unknown_git_version",
                    format!("cannot read git version {:?}", inner.client_git_version),
                    "",
                ),
                _ => {}
            }
            if !cfg.git_enabled {
                add(
                    v1::GitPreflightSeverity::Error,
                    "git_disabled",
                    "git is disabled for this repo".into(),
                    "",
                );
            }
            if inner.transport == "ssh" && !cfg.ssh_enabled {
                add(
                    v1::GitPreflightSeverity::Error,
                    "ssh_disabled",
                    "SSH is disabled for this repo".into(),
                    "use https",
                );
            }
            if inner.protocol_version != 0 && inner.protocol_version < 2 {
                add(
                    v1::GitPreflightSeverity::Warning,
                    "protocol_v0",
                    "protocol v0/v1 is slower and lacks filters".into(),
                    "git config --global protocol.version 2",
                );
            }
            if inner.transport != "ssh" && inner.credential_helper.is_empty() {
                add(
                    v1::GitPreflightSeverity::Warning,
                    "no_credential_helper",
                    "no credential helper is configured".into(),
                    "git config --global credential.helper bee",
                );
            }
            if cfg.lfs_enabled && !inner.lfs_installed {
                add(
                    v1::GitPreflightSeverity::Info,
                    "lfs_missing",
                    "git-lfs is not installed; LFS files will be pointers".into(),
                    "install git-lfs and run git lfs install",
                );
            }
            // Access: read, and push when asked.
            let token = AppState::bearer(&md).unwrap_or_default();
            if crate::git_mount::open_session(s, &token, &repo.full_name, Access::Read)
                .await
                .is_err()
            {
                add(
                    v1::GitPreflightSeverity::Error,
                    "no_read_access",
                    "the credential cannot clone this repo".into(),
                    "",
                );
            }
            if inner.push
                && crate::git_mount::open_session(s, &token, &repo.full_name, Access::Write)
                    .await
                    .is_err()
            {
                add(
                    v1::GitPreflightSeverity::Error,
                    "no_push_access",
                    "the credential cannot push to this repo".into(),
                    "",
                );
            }
            let ok = !problems
                .iter()
                .any(|p| p.severity == v1::GitPreflightSeverity::Error as i32);
            let base = s.public_url();
            Ok::<_, ApiError>(v1::GitPreflightResponse {
                ok,
                problems,
                min_git_version: min,
                remote_url: format!("{}/{}.git", base.trim_end_matches('/'), repo.full_name),
                recommended_config: vec![v1::KeyValue {
                    key: "protocol.version".into(),
                    value: "2".into(),
                }],
                object_format: cfg.object_format,
            })
        })
    }

    async fn create_git_credential(
        &self,
        req: tonic::Request<v1::CreateGitCredentialRequest>,
    ) -> Rpc<v1::CreateGitCredentialResponse> {
        let md = req.metadata().clone();
        git_rpc!(self, req, |caller, inner, meta| async {
            use bee_proto::v1::auth_service_server::AuthService;
            let s = &self.state;
            let mut repos = Vec::new();
            for r in &inner.repos {
                let ctx = s.repo(Some(r)).await?;
                s.gate(&caller, &ctx, Action::Read, None).await?;
                repos.push(ctx.full_name);
            }
            let ttl = if inner.ttl_seconds == 0 {
                3600
            } else {
                u64::from(inner.ttl_seconds)
            };
            let mut exp = s.now_ms() / 1000 + ttl;
            if let Some(e) = caller.token.scope.expires_at {
                exp = exp.min(e);
            }
            let mut actions = vec!["read".to_string()];
            if inner.write {
                actions.extend(["commit".into(), "land".into(), "lease".into()]);
            }
            let scope = v1::TokenScope {
                repos,
                actions,
                expires_at: Some(crate::util::timestamp_secs(exp)),
                session: format!("git-credential:{}", inner.host),
                ..Default::default()
            };
            let api = crate::services::auth::AuthApi::new(s.clone());
            let mut sub = tonic::Request::new(v1::MintTokenRequest {
                principal_id: String::new(),
                scope: Some(scope),
            });
            *sub.metadata_mut() = md.clone();
            sub.extensions_mut().insert(meta.clone());
            let minted = api
                .mint_token(sub)
                .await
                .map_err(|st| match bee_proto::errors::detail_from_status(&st) {
                    Some(d) => ApiError {
                        detail: Box::new(d),
                    },
                    None => ApiError::new(ErrorReason::Internal, st.message().to_string()),
                })?
                .into_inner();
            Ok::<_, ApiError>(v1::CreateGitCredentialResponse {
                username: "x-bee-token".into(),
                password: minted.token,
                credential_id: minted.token_id,
                expires_at: Some(crate::util::timestamp_secs(exp)),
            })
        })
    }

    async fn resolve_git_ssh_key(
        &self,
        req: tonic::Request<v1::ResolveGitSshKeyRequest>,
    ) -> Rpc<v1::ResolveGitSshKeyResponse> {
        git_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            let wanted = if !inner.fingerprint.is_empty() {
                inner.fingerprint.clone()
            } else if !inner.public_key.is_empty() {
                fingerprint(&inner.public_key)
            } else {
                return Err(ApiError::invalid(
                    "missing_field",
                    "public_key or fingerprint is required",
                ));
            };
            let unknown = || ApiError::unauthenticated("unknown SSH key");
            let repo = match inner.repo.as_ref().filter(|r| r.repo.is_some()) {
                Some(r) => s.repo(Some(r)).await.map_err(|_| unknown())?,
                None => return Err(unknown()),
            };
            // A deploy key's public half is no secret, so resolving one mints a credential:
            // only the server's operators (SSH frontends run as a service principal listed in
            // `ops.operators`) or callers holding `manage_settings` on the repo through the
            // repo gate may. The session token itself passes the gate on every use, as the
            // key's principal (whose forge role is the key's: read, or write).
            if crate::services::support::is_operator(s, &caller) {
                require_operator(s, &caller).await?;
            } else {
                s.gate(&caller, &repo, bee_policy::Action::ManageSettings, None)
                    .await?;
            }
            let w = s.hub.get().ok_or_else(unknown)?;
            let keys = w
                .forge
                .orgs
                .deploy_keys(&bee_forge::Cx::system(), &repo.id_text())
                .await
                .map_err(crate::forge::forge_to_api)?;
            let key = keys
                .into_iter()
                .find(|k| {
                    let blob = k.public_key.split_whitespace().nth(1).unwrap_or("");
                    base64::engine::general_purpose::STANDARD
                        .decode(blob)
                        .map(|b| fingerprint(&b) == wanted)
                        .unwrap_or(false)
                })
                .ok_or_else(unknown)?;
            if inner.command == "git-receive-pack" && key.read_only {
                return Err(ApiError::denied("this deploy key is read-only"));
            }
            // A deploy key acts as its own service principal (`deploy-key:<id>`), limited to
            // the repo; policy decides what services may do there.
            let principal = crate::auth::PrincipalRecord {
                id: key.principal_id.clone(),
                kind: "service".into(),
                display: format!("deploy key {}", key.title),
                org: repo.org.id.0.to_text(),
                owner: String::new(),
                runtime: String::new(),
                purpose: String::new(),
                external_ids: Vec::new(),
                groups: Vec::new(),
                deactivated: false,
            };
            let now = s.now_ms() / 1000;
            let exp = now + 3600;
            let mut actions = vec![bee_policy::Action::Read];
            if !key.read_only {
                actions.extend([bee_policy::Action::Commit, bee_policy::Action::Land]);
            }
            let scope = bee_auth::Scope::new()
                .repos(std::slice::from_ref(&repo.full_name))
                .map_err(|e| ApiError::invalid("invalid_scope", e.message))?
                .actions(actions)
                .expires_at(exp)
                .session(format!("ssh:{}", key.id));
            let token = s
                .ring
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .mint(
                    &bee_auth::MintRequest {
                        principal: principal.to_auth(),
                        on_behalf_of: Vec::new(),
                        scope,
                    },
                    now,
                )
                .map_err(ApiError::from)?;
            crate::services::auth::record_token(
                s.kv.as_ref(),
                &crate::auth::TokenRecord {
                    token_id: token.id.0.clone(),
                    principal: principal.id.clone(),
                    parent: String::new(),
                    on_behalf_of: Vec::new(),
                    created_ms: s.now_ms(),
                    expires_at: exp,
                    description: format!("ssh session for deploy key {}", key.id),
                    scope: None,
                },
            )
            .await?;
            s.audit(
                &caller,
                &meta,
                Some(&repo),
                &key.id,
                "ssh_key.resolve",
                json!({ "deploy_key": true }),
            )
            .await?;
            Ok::<_, ApiError>(v1::ResolveGitSshKeyResponse {
                principal: Some(crate::services::auth::principal_to(&principal)),
                key_id: key.id.clone(),
                deploy_key: true,
                read_only: key.read_only,
                session_token: token.text,
                expires_at: Some(crate::util::timestamp_secs(exp)),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_fingerprints() {
        assert_eq!(parse_version("2.45.1"), Some((2, 45, 1)));
        assert_eq!(parse_version("git version 2.30.0"), Some((2, 30, 0)));
        assert!(parse_version("2.29") < parse_version("2.30.0"));
        // ssh-keygen -lf of an all-zero 32-byte blob is stable: just check the shape.
        let f = fingerprint(&[0u8; 51]);
        assert!(f.starts_with("SHA256:") && !f.ends_with('='));
    }
}
