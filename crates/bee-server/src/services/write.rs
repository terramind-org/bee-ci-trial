//! `WriteService` (SPEC §7): verified uploads, CreateChangeset with the SPEC §7.2 checks and
//! secret-scanning push protection, rebase, merge, revert, and Land through the coalescer.

use std::collections::BTreeMap;
use std::sync::Arc;

use bee_core::{ChangeId, Limits, Mode, ObjectId, ObjectKind, RefName, RefNamespace};
use bee_history::{History, MergeOptions, MergeOutcome, RebaseOptions, RebaseOutcome};
use bee_object::{DeltaEntry, DeltaOp, Meta, Timestamp, validate_delta};
use bee_policy::{Action, Policy, PushedFile};
use bee_proto::v1::{self, ErrorReason, write_service_server::WriteService};
use bee_secrets::{Bypass, BypassReason, evaluate_push_protection};
use bee_store::{PutOutcome, ReadFrom, UploadRecord};
use futures_util::StreamExt;
use serde_json::json;

use crate::auth::{Caller, Usage};
use crate::coalescer::{self, LandJob};
use crate::convert::{self, change_id_from, change_id_to, id_from, id_to};
use crate::errors::{ApiError, ApiResult};
use crate::services::Rpc;
use crate::services::object::finish;
use crate::services::query::{resolve_rev, view};
use crate::state::{AppState, RepoCtx, RequestMeta};

/// Header naming a push-protection bypass reason (`false_positive`, `used_in_tests`,
/// `will_fix_later`).
pub const BYPASS_HEADER: &str = "bee-push-protection-bypass";
/// Request header naming an approved forge push-protection bypass request (`SecurityService`).
pub const FORGE_BYPASS_HEADER: &str = "bee-push-protection-bypass-id";

/// Upload batches are written once they hold this many bytes.
const UPLOAD_BATCH_BYTES: usize = 8 << 20;

/// A land that changed at most this many files has their blame derived right after it.
const WARM_BLAME_FILES: usize = 32;
/// Files larger than this are left for the first blame request to derive.
const WARM_BLAME_BYTES: u64 = 4 << 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LandRoute {
    Direct,
    Ordinary,
    OwnerCoalescer,
}

/// Chooses the server-side route for a land after its forwarded-owner proof is checked.
/// A caller can supply the marker, but cannot make it a verified forward without the owner
/// identity and lease fence held by this process.
fn land_route(protected: bool, forwarded: bool, verified_owner: bool) -> LandRoute {
    if !protected {
        LandRoute::Direct
    } else if forwarded && verified_owner {
        LandRoute::OwnerCoalescer
    } else {
        LandRoute::Ordinary
    }
}

/// Derives, in the background, what the first reads after a land would otherwise derive:
/// the blame of each file the `landed` changesets put (`land.warm_blame`), the git ids of
/// the new `tip` (`land.warm_git`) and the pack a stock `git clone` of the repo asks for
/// (`land.warm_pack`, kept in the gateway's pack cache). All are pure functions of history
/// (SPEC §8, §17.4), so whether or when this runs never changes an answer; a failure here is
/// left for the request that needs the value.
///
/// One task per repo does the warming, so lands in quick succession do not derive the same
/// rows side by side: lands arriving while it runs are queued and taken together, the git
/// ids for the newest tip only.
fn warm_after_land(state: &Arc<AppState>, repo: &RepoCtx, tip: ObjectId, landed: Vec<ObjectId>) {
    let land = &state.cfg.land;
    if landed.is_empty() || !(land.warm_blame || land.warm_git || land.warm_pack) {
        return;
    }
    let id = repo.repo.id;
    {
        let mut queue = state.warm_queue.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(waiting) = queue.get_mut(&id) {
            // A task is running for this repo: it takes this land next.
            waiting.push((repo.clone(), tip, landed));
            return;
        }
        queue.insert(id, vec![(repo.clone(), tip, landed)]);
    }
    let state = state.clone();
    tokio::spawn(async move {
        loop {
            let batch = {
                let mut queue = state.warm_queue.lock().unwrap_or_else(|p| p.into_inner());
                let waiting = queue.entry(id).or_default();
                if waiting.is_empty() {
                    queue.remove(&id);
                    return;
                }
                std::mem::take(waiting)
            };
            let Some((repo, tip, _)) = batch.last().cloned() else {
                continue;
            };
            let history = state.history(&repo);
            if state.cfg.land.warm_git {
                warm_git(&history, tip).await;
            }
            if state.cfg.land.warm_pack {
                crate::git_mount::warm_clone_pack(&state, &repo).await;
            }
            if state.cfg.land.warm_blame {
                for (_, _, landed) in batch {
                    for cs in landed {
                        warm_blame(&history, cs).await;
                    }
                }
            }
        }
    });
}

/// Derives and stores the git ids of `tip` and everything under it.
async fn warm_git(history: &History, tip: ObjectId) {
    let Ok(format) = bee_git_http::gitmap::detect_format(history, &[tip]).await else {
        return;
    };
    let mut view = bee_git_http::gitmap::GitView::new(history, format);
    if view.commit_oid(&tip).await.is_ok() {
        let _ = view.flush().await;
    }
}

/// Derives and stores the blame of each file changeset `cs` puts, when it puts few enough.
async fn warm_blame(history: &History, cs: ObjectId) {
    let Ok(c) = history.changeset(&cs).await else {
        return;
    };
    let files: Vec<_> = c
        .delta()
        .iter()
        .filter(|d| {
            matches!(d.op, DeltaOp::Put { mode, size, .. }
                if matches!(mode, Mode::File | Mode::Executable) && size <= WARM_BLAME_BYTES)
        })
        .map(|d| d.path.clone())
        .collect();
    if files.len() > WARM_BLAME_FILES {
        return;
    }
    for path in files {
        let _ = history
            .blame(&cs, &path, &bee_history::BlameOptions::default())
            .await;
    }
}

/// The write service.
pub struct Writes {
    state: Arc<AppState>,
}

impl Writes {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> Writes {
        Writes { state }
    }
}

/// A fresh random ChangeId.
pub fn fresh_change_id() -> ApiResult<ChangeId> {
    let b = crate::util::random_bytes(16)?;
    ChangeId::from_bytes(&b).map_err(ApiError::from)
}

fn now_ts(state: &AppState) -> Timestamp {
    Timestamp::new((state.now_ms() / 1000) as i64, 0)
}

impl Writes {
    async fn upload_impl(
        &self,
        caller_md: &tonic::metadata::MetadataMap,
        mut stream: tonic::Streaming<v1::UploadItem>,
        meta: &RequestMeta,
    ) -> ApiResult<v1::UploadObjectsResponse> {
        let s = &self.state;
        let caller = s.caller(caller_md).await?;
        let mut repo: Option<RepoCtx> = None;
        let mut partial: BTreeMap<ObjectId, Vec<u8>> = BTreeMap::new();
        let mut ready: Vec<(ObjectId, Vec<u8>)> = Vec::new();
        let mut ready_bytes = 0usize;
        let mut resp = v1::UploadObjectsResponse::default();
        let max_blob = s.cfg.limits.max_blob_bytes;
        while let Some(item) = stream.next().await {
            let item =
                item.map_err(|st| ApiError::invalid("bad_stream", st.message().to_string()))?;
            if repo.is_none() {
                let r = s.repo(item.repo.as_ref()).await?;
                s.gate(&caller, &r, Action::Commit, None).await?;
                repo = Some(r);
            }
            let id = id_from(item.id.as_ref(), "id")?;
            if !matches!(
                id.kind(),
                ObjectKind::Blob | ObjectKind::Tree | ObjectKind::Meta
            ) {
                return Err(ApiError::invalid(
                    "wrong_kind",
                    format!("{id}: only blobs, trees and metas are uploaded"),
                ));
            }
            let buf = partial.entry(id).or_default();
            if item.offset != buf.len() as u64 {
                return Err(ApiError::invalid(
                    "bad_offset",
                    format!(
                        "{id}: offset {} but {} bytes received",
                        item.offset,
                        buf.len()
                    ),
                ));
            }
            buf.extend_from_slice(&item.data);
            if buf.len() as u64 > max_blob {
                let mut e = ApiError::new(
                    ErrorReason::TooLarge,
                    format!("{id} exceeds the blob size limit"),
                );
                e.detail.payload = Some(v1::error_detail::Payload::LimitExceeded(
                    v1::LimitExceeded {
                        limit: "max_blob_bytes".into(),
                        maximum: max_blob,
                        actual: buf.len() as u64,
                    },
                ));
                return Err(e);
            }
            if item.last {
                let bytes = partial.remove(&id).unwrap_or_default();
                ready_bytes += bytes.len();
                ready.push((id, bytes));
                if ready_bytes >= UPLOAD_BATCH_BYTES
                    && let Some(r) = &repo
                {
                    self.flush(&caller, r, &mut ready, &mut resp).await?;
                    ready_bytes = 0;
                }
            }
            if meta.remaining().is_zero() {
                return Err(ApiError::new(
                    ErrorReason::DeadlineExceeded,
                    "deadline exceeded during upload",
                ));
            }
        }
        if let Some((id, _)) = partial.iter().next() {
            return Err(ApiError::invalid(
                "incomplete_object",
                format!("{id} ended without last = true"),
            ));
        }
        if let Some(r) = &repo {
            self.flush(&caller, r, &mut ready, &mut resp).await?;
            let count = resp.stored.len() + resp.already_present.len();
            s.audit(
                &caller,
                meta,
                Some(r),
                "objects",
                "commit",
                json!({ "objects": count }),
            )
            .await?;
        }
        Ok(resp)
    }

    async fn flush(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        ready: &mut Vec<(ObjectId, Vec<u8>)>,
        resp: &mut v1::UploadObjectsResponse,
    ) -> ApiResult<()> {
        if ready.is_empty() {
            return Ok(());
        }
        let s = &self.state;
        let batch = std::mem::take(ready);
        for (id, _) in &batch {
            if crate::redaction::is_redacted(s, repo.org.id, id).await? {
                return Err(ApiError::new(
                    ErrorReason::Redacted,
                    format!("{id} is redacted and cannot be uploaded again"),
                ));
            }
        }
        let bytes: u64 = batch.iter().map(|(_, b)| b.len() as u64).sum();
        // Billing: storage budgets and spending limits refuse before anything is stored.
        s.forge.spend(caller, repo, bytes, false).await?;
        s.charge(
            caller,
            Usage {
                upload_bytes: bytes,
                ..Usage::default()
            },
        )
        .await?;
        let ids: Vec<ObjectId> = batch.iter().map(|(id, _)| *id).collect();
        let sizes: Vec<u64> = batch.iter().map(|(_, b)| b.len() as u64).collect();
        let outcomes = s
            .writer
            .clone()
            .with_limits(
                Limits::with_blob_size(s.cfg.limits.max_blob_bytes).unwrap_or(Limits::DEFAULT),
            )
            .upload(
                repo.org.id,
                batch,
                Some(UploadRecord {
                    repo: Some(repo.repo.id),
                    principal: caller.id().to_string(),
                }),
                s.now_ms(),
            )
            .await?;
        let mut stored_bytes = 0u64;
        for ((id, o), size) in ids.iter().zip(outcomes).zip(sizes) {
            match o {
                PutOutcome::Inserted => {
                    stored_bytes += size;
                    resp.stored.push(id_to(id));
                }
                PutOutcome::AlreadyPresent => resp.already_present.push(id_to(id)),
            }
        }
        // Metering never fails a write that already happened.
        let _ = s.forge.spend(caller, repo, stored_bytes, true).await;
        Ok(())
    }

    /// Builds and validates a meta (the server sets `creator` and the provenance chain);
    /// returns its id and canonical bytes without storing it.
    #[allow(clippy::too_many_arguments)]
    fn build_meta(
        &self,
        caller: &Caller,
        author: &str,
        author_time: Option<Timestamp>,
        message: &str,
        provenance: Option<&v1::Provenance>,
        committer: Option<(String, Option<Timestamp>)>,
        git: Option<Vec<u8>>,
    ) -> ApiResult<(ObjectId, Vec<u8>)> {
        let s = &self.state;
        let author = if author.is_empty() {
            caller
                .record
                .as_ref()
                .map(|r| r.display.clone())
                .unwrap_or_else(|| caller.id().to_string())
        } else {
            author.to_string()
        };
        let mut meta = Meta::new(
            author,
            author_time.unwrap_or_else(|| now_ts(s)),
            caller.id(),
            message,
        );
        let chain = caller.chain_ids();
        let prov = match provenance {
            Some(p) => {
                let mut prov = convert::provenance_from(p)?;
                if !prov.principal_chain.is_empty() && prov.principal_chain != chain {
                    return Err(ApiError::invalid(
                        "principal_chain_mismatch",
                        "provenance.principal_chain is set by the server and disagrees with the token",
                    ));
                }
                prov.principal_chain = chain;
                if prov.session.is_none() {
                    prov.session = caller.token.session().map(str::to_string);
                }
                Some(prov)
            }
            None if chain.len() > 1 || caller.token.session().is_some() => {
                Some(bee_object::Provenance {
                    principal_chain: chain,
                    session: caller.token.session().map(str::to_string),
                    ..Default::default()
                })
            }
            None => None,
        };
        meta.provenance = prov;
        if let Some((c, t)) = committer {
            meta.committer = Some(c);
            meta.committer_time = t;
        }
        meta.git = git;
        meta.validate(&Limits::DEFAULT).map_err(ApiError::from)?;
        Ok((meta.id(), meta.encode()))
    }

    /// Builds and uploads a meta; returns its id.
    #[allow(clippy::too_many_arguments)]
    async fn make_meta(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        author: &str,
        author_time: Option<Timestamp>,
        message: &str,
        provenance: Option<&v1::Provenance>,
        committer: Option<(String, Option<Timestamp>)>,
        git: Option<Vec<u8>>,
    ) -> ApiResult<ObjectId> {
        let s = &self.state;
        let (id, bytes) = self.build_meta(
            caller,
            author,
            author_time,
            message,
            provenance,
            committer,
            git,
        )?;
        s.writer
            .upload(
                repo.org.id,
                vec![(id, bytes)],
                Some(UploadRecord {
                    repo: Some(repo.repo.id),
                    principal: caller.id().to_string(),
                }),
                s.now_ms(),
            )
            .await?;
        Ok(id)
    }

    /// Push rules and blob availability (SPEC §7.2): a put of a blob the caller neither
    /// uploaded nor can read fails exactly like an absent blob.
    pub(crate) async fn check_puts(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        policy: &Policy,
        delta: &[DeltaEntry],
    ) -> ApiResult<()> {
        let s = &self.state;
        let files: Vec<PushedFile> = delta
            .iter()
            .filter_map(|e| match e.op {
                DeltaOp::Put { size, mode, .. } if mode != Mode::Gitlink => Some(PushedFile {
                    path: e.path.to_string(),
                    size,
                }),
                _ => None,
            })
            .collect();
        let violations = policy.check_push(&files);
        if !violations.is_empty() {
            return Err(ApiError::policy(
                ErrorReason::PolicyDenied,
                "push",
                violations.iter().map(|v| v.id.clone()).collect(),
                violations
                    .iter()
                    .map(|v| v.detail.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        let mut missing = Vec::new();
        for e in delta {
            if let DeltaOp::Put { id, mode, .. } = e.op {
                if mode == Mode::Gitlink {
                    continue;
                }
                let row = s
                    .meta
                    .get_object(repo.org.id, &id, ReadFrom::Primary)
                    .await?;
                let ok = row.is_some_and(|r| r.state == bee_store::ObjectState::Live)
                    && s.object_readable(caller, repo, policy, &id).await?;
                if !ok {
                    missing.push(id);
                }
            }
        }
        if !missing.is_empty() {
            let mut e = ApiError::new(
                ErrorReason::MissingObject,
                format!(
                    "{} referenced object(s) are not available; upload them first",
                    missing.len()
                ),
            );
            e.detail.payload = Some(v1::error_detail::Payload::MissingObjects(
                v1::MissingObjects {
                    ids: missing.iter().map(id_to).collect(),
                },
            ));
            return Err(e);
        }
        Ok(())
    }

    /// Secret scanning push protection over the blobs a delta adds.
    pub(crate) async fn scan_secrets(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        policy: &Policy,
        delta: &[DeltaEntry],
        md: &tonic::metadata::MetadataMap,
        meta: &RequestMeta,
    ) -> ApiResult<()> {
        let s = &self.state;
        let forge_on = s.forge.protects(repo).await;
        // "admins" means holding `manage_policy` in the repo's policy, forge repo roles
        // included (`@repo-role/admin`, SPEC §9.4).
        let actors = s.policy_actors(caller, repo).await?;
        if !s.cfg.push_protection.enabled && !forge_on {
            return Ok(());
        }
        let history = s.history(repo);
        let mut findings = Vec::new();
        let mut blobs: BTreeMap<String, ObjectId> = BTreeMap::new();
        let mut files: Vec<(String, Vec<u8>, Option<ObjectId>)> = Vec::new();
        for e in delta {
            if let DeltaOp::Put { id, mode, size } = e.op {
                if !matches!(mode, Mode::File | Mode::Executable)
                    || size > bee_secrets::DEFAULT_MAX_FILE_SIZE as u64
                {
                    continue;
                }
                let content = history.blob(&id).await?;
                let path = e.path.to_string();
                if s.cfg.push_protection.enabled {
                    findings.extend(s.scanner.scan(&path, content.bytes_or_empty()));
                }
                if forge_on {
                    files.push((path.clone(), content.bytes_or_empty().to_vec(), Some(id)));
                }
                blobs.insert(path, id);
            }
        }
        // A bypass with a reason that this server allows (and audits below) also covers the
        // forge's push protection, so one write is not blocked twice for the same secret.
        let server_bypass = md
            .get(BYPASS_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|r| BypassReason::parse(r.trim()))
            .is_some()
            && match s.cfg.push_protection.bypass.as_str() {
                "writers" => true,
                "admins" => actors
                    .iter()
                    .all(|a| policy.actions_of(a).contains(&Action::ManagePolicy)),
                _ => false,
            };
        if forge_on && !server_bypass {
            // The forge's push protection: custom patterns and delegated bypass requests.
            let bypass_id = md
                .get(FORGE_BYPASS_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty());
            s.forge.protect(caller, repo, &files, bypass_id).await?;
        }
        if findings.is_empty() {
            return Ok(());
        }
        let requested = md
            .get(BYPASS_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let bypass = match requested {
            None => None,
            Some(r) => {
                let reason = BypassReason::parse(r).ok_or_else(|| {
                    ApiError::invalid(
                        "invalid_bypass_reason",
                        "bypass reason must be false_positive, used_in_tests or will_fix_later",
                    )
                })?;
                let allowed = match s.cfg.push_protection.bypass.as_str() {
                    "writers" => true,
                    "admins" => actors
                        .iter()
                        .all(|a| policy.actions_of(a).contains(&Action::ManagePolicy)),
                    _ => false,
                };
                if !allowed {
                    return Err(ApiError::policy(
                        ErrorReason::PolicyDenied,
                        "push_protection.bypass",
                        vec!["bypass:push_protection".into()],
                        "this principal may not bypass push protection",
                    ));
                }
                Some(Bypass {
                    principal: caller.id().to_string(),
                    reason,
                })
            }
        };
        let decision = evaluate_push_protection(findings, bypass);
        let blocking: Vec<_> = decision
            .findings
            .iter()
            .filter(|f| f.confidence.blocks())
            .collect();
        if decision.blocked {
            s.metrics.secret_findings("blocked", blocking.len() as u64);
            let mut e = ApiError::new(
                ErrorReason::SecretDetected,
                decision
                    .to_error()
                    .map(|e| e.message)
                    .unwrap_or_else(|| "secret detected".into()),
            );
            e.detail.payload = Some(v1::error_detail::Payload::PushProtection(
                v1::PushProtectionBlock {
                    findings: blocking
                        .iter()
                        .map(|f| v1::SecretFinding {
                            path: f.path.as_bytes().to_vec(),
                            blob: blobs.get(&f.path).map(id_to),
                            line: f.line as u32,
                            secret_type: f.provider.clone(),
                            pattern_id: f.rule_id.clone(),
                        })
                        .collect(),
                    bypass_allowed: s.cfg.push_protection.bypass != "none",
                    bypass_request_url: String::new(),
                },
            ));
            s.forge.secret_refusal(caller, repo, "vcs.write", &e).await;
            return Err(e);
        }
        if let Some(b) = &decision.bypass {
            s.metrics.secret_findings("bypassed", blocking.len() as u64);
            s.audit(
                caller,
                meta,
                Some(repo),
                "push_protection",
                "bypass",
                json!({
                    "reason": b.reason.as_str(),
                    "findings": blocking.iter().map(|f| json!({
                        "path_digest": crate::logging::path_digest(f.path.as_bytes()),
                        "rule": f.rule_id, "line": f.line, "preview": f.redacted_preview,
                    })).collect::<Vec<_>>(),
                }),
            )
            .await?;
        }
        Ok(())
    }

    async fn create_impl(
        &self,
        md: &tonic::metadata::MetadataMap,
        req: v1::CreateChangesetRequest,
        meta: &RequestMeta,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::CreateChangesetResponse)> {
        let s = &self.state;
        let caller = s.caller(md).await?;
        // 1. Repo and repo-level permission.
        let repo = s.repo(req.repo.as_ref()).await?;
        let policy = s.gate(&caller, &repo, Action::Commit, None).await?;
        if req.import_mode {
            if caller.token.principal.kind != bee_policy::PrincipalKind::Service {
                return Err(ApiError::denied(
                    "import_mode is reserved to service principals",
                ));
            }
            // SPEC §17.4: importer mode writes history verbatim (no push protection, its own
            // meta), so it needs ref administration and write on the whole repo.
            s.gate(&caller, &repo, Action::ForceUpdate, None).await?;
            s.require_paths(
                &caller,
                &repo,
                &policy,
                Action::Commit,
                &[String::new()],
                None,
            )?;
        }
        // 2. Shape: ChangeId, parents, delta (SPEC §4.4, §4.6).
        let change_id = change_id_from(req.change_id.as_ref(), "change_id")?;
        let mut parents = Vec::with_capacity(req.parents.len());
        for p in &req.parents {
            let id = id_from(Some(p), "parents")?;
            if id.kind() != ObjectKind::Changeset {
                return Err(ApiError::invalid(
                    "wrong_kind",
                    format!("parent {id} is not a changeset"),
                ));
            }
            if parents.contains(&id) {
                return Err(ApiError::invalid(
                    "invalid_parents",
                    format!("parent {id} is listed twice"),
                ));
            }
            parents.push(id);
        }
        if parents.is_empty() && !req.import_mode {
            return Err(ApiError::invalid(
                "invalid_parents",
                "a changeset needs a parent (the repo's empty root at least)",
            ));
        }
        if parents.len() > 16 {
            return Err(ApiError::new(ErrorReason::TooLarge, "at most 16 parents"));
        }
        let delta = convert::delta_from(&req.delta)?;
        validate_delta(&delta, !parents.is_empty(), &Limits::DEFAULT).map_err(ApiError::from)?;
        if delta.is_empty() && !req.allow_empty {
            return Err(ApiError::new(
                ErrorReason::EmptyChange,
                "the delta is empty; set allow_empty to create it anyway",
            ));
        }
        // 3. Path permissions (token scope and policy) for every changed path.
        let paths: Vec<String> = delta.iter().map(|e| e.path.to_string()).collect();
        s.require_paths(&caller, &repo, &policy, Action::Commit, &paths, None)?;
        // 4. Push rules and referenced objects.
        self.check_puts(&caller, &repo, &policy, &delta).await?;
        // 5. Push protection.
        if !req.import_mode {
            self.scan_secrets(&caller, &repo, &policy, &delta, md, meta)
                .await?;
        }
        // 6. Meta and trees.
        let committer = (!req.committer.is_empty() || req.committer_time.is_some()).then(|| {
            (
                req.committer.clone(),
                convert::timestamp_from(req.committer_time.as_ref()),
            )
        });
        let git = req
            .extensions
            .iter()
            .find(|e| u64::from(e.key) == bee_object::GIT_EXTENSION_KEY)
            .map(|e| e.value.clone());
        if git.is_some() && !req.import_mode {
            return Err(ApiError::invalid(
                "invalid_field",
                "meta extensions need import_mode",
            ));
        }
        let (meta_id, meta_bytes) = self.build_meta(
            &caller,
            &req.author,
            convert::timestamp_from(req.author_time.as_ref()),
            &req.message,
            req.provenance.as_ref(),
            committer,
            git,
        )?;
        let history = s.history(&repo);
        let built = s
            .writer
            .build(
                repo.org.id,
                repo.repo.id,
                parents,
                meta_id,
                change_id,
                delta,
            )
            .await?;
        // 7. expected_root_tree.
        if let Some(expected) =
            convert::opt_id_from(req.expected_root_tree.as_ref(), "expected_root_tree")?
            && expected != *built.changeset.root()
        {
            return Err(ApiError::new(
                ErrorReason::RootTreeMismatch,
                format!(
                    "computed root {} differs from expected_root_tree {expected}",
                    built.changeset.root()
                ),
            ));
        }
        // 8. Idempotent: an identical request returns the stored changeset without charging.
        let id = built.id();
        let exists = s
            .meta
            .get_changeset(repo.repo.id, &id, ReadFrom::Primary)
            .await?
            .is_some();
        if !exists {
            s.charge(
                &caller,
                Usage {
                    changesets: 1,
                    ..Usage::default()
                },
            )
            .await?;
            // SPEC §6.2: the meta, the new trees and the changeset row with its edges,
            // object_paths and eager rows in one transaction (one durable commit).
            let rows = history.eager().rows(&history, id, &built.changeset).await?;
            s.writer
                .store_new(
                    repo.org.id,
                    repo.repo.id,
                    (meta_id, meta_bytes),
                    &built,
                    Some(UploadRecord {
                        repo: Some(repo.repo.id),
                        principal: caller.id().to_string(),
                    }),
                    rows,
                    s.now_ms(),
                )
                .await?;
            s.audit(
                &caller,
                meta,
                Some(&repo),
                &id.to_text(),
                "commit",
                json!({ "paths": paths.len() }),
            )
            .await?;
            // The forge verifies the issued run token. Other credentials have no evidence
            // to record, so keep their writes off this additional forge lookup path.
            if caller.is_agent()
                && caller
                    .token
                    .session()
                    .is_some_and(|s| s.starts_with("run-"))
            {
                s.forge.note_safety_changeset(&caller, &repo, &id).await;
            }
        }
        let cs = view(s, &caller, &policy, &history, &id, true).await?;
        Ok((
            repo,
            policy,
            v1::CreateChangesetResponse {
                changeset: Some(cs),
            },
        ))
    }

    async fn rebase_impl(
        &self,
        md: &tonic::metadata::MetadataMap,
        req: v1::RebaseRequest,
        meta: &RequestMeta,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::RebaseResponse)> {
        let s = &self.state;
        let caller = s.caller(md).await?;
        let repo = s.repo(req.repo.as_ref()).await?;
        // A rebase stores a new changeset: the repo must take writes from this caller
        // (archived repos and read-only roles refuse), while the policy asks for read only.
        s.authorize_repo(&caller, &repo, Action::Commit).await?;
        let policy = s.gate(&caller, &repo, Action::Read, None).await?;
        let history = s.history(&repo);
        let c = id_from(req.changeset.as_ref(), "changeset")?;
        let cs = history.changeset(&c).await?;
        for e in cs.delta() {
            if !s.path_allowed(
                &caller,
                repo.repo.id,
                &policy,
                Action::Read,
                &e.path.to_string(),
            ) {
                return Err(ApiError::policy(
                    ErrorReason::PolicyDenied,
                    "access",
                    vec![format!("access:read:{}", e.path)],
                    "rebase needs read access to every path the changeset changes",
                ));
            }
        }
        let onto = resolve_rev(&history, req.onto.as_ref()).await?;
        let opts = RebaseOptions {
            allow_empty: req.allow_empty,
            ..RebaseOptions::default()
        };
        let id = match history.rebase(&c, &onto, &opts, s.now_ms()).await? {
            RebaseOutcome::Unchanged(id) => id,
            RebaseOutcome::Rebased(id) => {
                s.audit(
                    &caller,
                    meta,
                    Some(&repo),
                    &id.to_text(),
                    "rebase",
                    json!({ "source": c.to_text(), "onto": onto.to_text() }),
                )
                .await?;
                id
            }
            RebaseOutcome::Conflicts(conflicts) => {
                return Err(convert::rebase_conflict_error(
                    &conflicts,
                    &onto,
                    &c,
                    cs.change_id(),
                ));
            }
        };
        let cs = view(s, &caller, &policy, &history, &id, true).await?;
        Ok((
            repo,
            policy,
            v1::RebaseResponse {
                changeset: Some(cs),
            },
        ))
    }

    async fn merge_impl(
        &self,
        md: &tonic::metadata::MetadataMap,
        req: v1::MergeRequest,
        meta: &RequestMeta,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::MergeResponse)> {
        let s = &self.state;
        let caller = s.caller(md).await?;
        let repo = s.repo(req.repo.as_ref()).await?;
        let policy = s.gate(&caller, &repo, Action::Commit, None).await?;
        let history = s.history(&repo);
        let ours = resolve_rev(&history, req.ours.as_ref()).await?;
        let theirs = resolve_rev(&history, req.theirs.as_ref()).await?;
        let change_id = match req.change_id.as_ref().filter(|c| !c.raw.is_empty()) {
            Some(c) => change_id_from(Some(c), "change_id")?,
            None => fresh_change_id()?,
        };
        let message = if req.message.is_empty() {
            format!("Merge {} into {}", theirs.to_text(), ours.to_text())
        } else {
            req.message.clone()
        };
        let meta_id = self
            .make_meta(
                &caller,
                &repo,
                &req.author,
                convert::timestamp_from(req.author_time.as_ref()),
                &message,
                req.provenance.as_ref(),
                None,
                None,
            )
            .await?;
        s.charge(
            &caller,
            Usage {
                changesets: 1,
                ..Usage::default()
            },
        )
        .await?;
        let opts = MergeOptions {
            allow_empty: req.allow_empty,
            ..MergeOptions::default()
        };
        let id = match history
            .merge(&ours, &theirs, meta_id, change_id, &opts, s.now_ms())
            .await?
        {
            MergeOutcome::Merged { id, .. } => id,
            MergeOutcome::Conflicts(c) => return Err(convert::conflict_error(&c, &ours, &theirs)),
        };
        // Writing a merge needs write access to every path it changes from its first parent.
        let cs = history.changeset(&id).await?;
        let paths: Vec<String> = cs.delta().iter().map(|e| e.path.to_string()).collect();
        s.require_paths(&caller, &repo, &policy, Action::Commit, &paths, None)?;
        s.audit(
            &caller,
            meta,
            Some(&repo),
            &id.to_text(),
            "merge",
            json!({ "ours": ours.to_text(), "theirs": theirs.to_text() }),
        )
        .await?;
        let cs = view(s, &caller, &policy, &history, &id, true).await?;
        Ok((
            repo,
            policy,
            v1::MergeResponse {
                changeset: Some(cs),
            },
        ))
    }

    async fn squash(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        history: &History,
        head: &ObjectId,
        tip: &ObjectId,
        req: &v1::LandRequest,
    ) -> ApiResult<ObjectId> {
        let s = &self.state;
        let stack = history.land_stack(head, tip).await?;
        let Some(first) = stack.first() else {
            return Ok(*head);
        };
        let base = history
            .changeset(first)
            .await?
            .first_parent()
            .copied()
            .ok_or_else(|| {
                ApiError::invalid("invalid_parents", "cannot squash a parentless changeset")
            })?;
        let base_root = *history.changeset(&base).await?.root();
        let head_cs = history.changeset(head).await?;
        let delta = history
            .diff_trees(Some(&base_root), Some(head_cs.root()), None)
            .await?;
        let message = if req.message.is_empty() {
            let mut msgs = Vec::new();
            for id in &stack {
                let cs = history.changeset(id).await?;
                if let Some(m) = history.meta_object(cs.meta()).await? {
                    msgs.push(m.message);
                }
            }
            msgs.join("\n\n")
        } else {
            req.message.clone()
        };
        let change_id = match req.change_id.as_ref().filter(|c| !c.raw.is_empty()) {
            Some(c) => change_id_from(Some(c), "change_id")?,
            None => *head_cs.change_id(),
        };
        let meta_id = self
            .make_meta(caller, repo, "", None, &message, None, None, None)
            .await?;
        let built = s
            .writer
            .build(
                repo.org.id,
                repo.repo.id,
                vec![base],
                meta_id,
                change_id,
                delta,
            )
            .await?;
        history.store_built(&built, s.now_ms()).await?;
        Ok(built.id())
    }

    async fn land_impl(
        &self,
        md: &tonic::metadata::MetadataMap,
        req: v1::LandRequest,
        meta: &RequestMeta,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::LandResponse)> {
        let s = &self.state;
        let caller = s.caller(md).await?;
        let repo = s.repo(req.repo.as_ref()).await?;
        let mut verified_owner = false;
        let effect = if caller.is_agent()
            && caller
                .token
                .session()
                .is_some_and(|s| s.starts_with("run-"))
            && let Ok(name) = RefName::new(req.r#ref.clone())
            && let Ok(head) = id_from(req.head.as_ref(), "head")
        {
            Some(format!("ref:{}:{}", name.as_str(), head.to_text()))
        } else {
            None
        };
        let result = self
            .land_as(md, req, meta, &caller, &repo, &mut verified_owner)
            .await;
        // Only a verified owner forward continues an attempt recorded by the first server.
        if !verified_owner && let Some(effect) = effect {
            s.forge
                .note_safety_land(
                    &caller,
                    &repo,
                    &effect,
                    result.as_ref().err().map(ApiError::reason),
                )
                .await;
        }
        result.map(|(policy, response)| (repo, policy, response))
    }

    async fn land_as(
        &self,
        md: &tonic::metadata::MetadataMap,
        req: v1::LandRequest,
        meta: &RequestMeta,
        caller: &Caller,
        repo: &RepoCtx,
        verified_owner: &mut bool,
    ) -> ApiResult<(Arc<Policy>, v1::LandResponse)> {
        let s = &self.state;
        let name = RefName::new(req.r#ref.clone()).map_err(ApiError::from)?;
        let policy = s
            .gate(caller, repo, Action::Land, Some(name.as_str()))
            .await?;
        // A landed budget that is already spent is refused before any work.
        for c in caller.token.budget_charges() {
            if let Some(max) = c.limit.max_landed
                && s.ledger.usage(s.kv.as_ref(), &c.key).await?.landed >= max
            {
                return Err(ApiError::new(
                    ErrorReason::BudgetExhausted,
                    "the landed-changes budget of this token is used up",
                ));
            }
        }
        let history = s.history(repo);
        let mut head = id_from(req.head.as_ref(), "head")?;
        match v1::LandMethod::try_from(req.method).unwrap_or(v1::LandMethod::Unspecified) {
            v1::LandMethod::Unspecified | v1::LandMethod::Rebase => {}
            v1::LandMethod::Squash => {
                let tip = s
                    .meta
                    .get_ref(repo.repo.id, &name)
                    .await?
                    .and_then(|r| r.target)
                    .ok_or_else(|| {
                        ApiError::not_found(format!("ref {} not found", name.as_str()))
                    })?;
                head = self
                    .squash(caller, repo, &history, &head, &tip, &req)
                    .await?;
            }
            v1::LandMethod::MergeCommit => {
                return Err(ApiError::new(
                    ErrorReason::MergeInStack,
                    "merge-commit lands are not accepted: protected refs land linear stacks (rebase or squash)",
                ));
            }
        }
        // Authorize the stack's paths here (token Datalog and policy), in parallel with other
        // requests; the coalescer re-checks scope and policy at the tip it publishes on.
        if let Some(tip) = s
            .meta
            .get_ref(repo.repo.id, &name)
            .await?
            .and_then(|r| r.target)
        {
            let stack = history.land_stack(&head, &tip).await?;
            let mut paths = Vec::new();
            for id in &stack {
                for e in history.changeset(id).await?.delta() {
                    paths.push(e.path.to_string());
                }
            }
            paths.sort();
            paths.dedup();
            s.require_paths(
                caller,
                repo,
                &policy,
                Action::Land,
                &paths,
                Some(name.as_str()),
            )?;
            if s.forge.protects(repo).await {
                // Forge push protection over what the stack adds, at Land too.
                let mut files = Vec::new();
                for id in &stack {
                    for e in history.changeset(id).await?.delta() {
                        if let DeltaOp::Put { id: b, mode, size } = e.op
                            && matches!(mode, Mode::File | Mode::Executable)
                            && size <= bee_secrets::DEFAULT_MAX_FILE_SIZE as u64
                        {
                            let content = history.blob(&b).await?;
                            files.push((
                                e.path.to_string(),
                                content.bytes_or_empty().to_vec(),
                                Some(b),
                            ));
                        }
                    }
                }
                let bypass_id = md
                    .get(FORGE_BYPASS_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::trim)
                    .filter(|v| !v.is_empty());
                s.forge.protect(caller, repo, &files, bypass_id).await?;
            }
        }
        let protected =
            policy.is_protected(name.as_str()) || name.namespace() == RefNamespace::Protected;
        let forwarded = md.get("x-bee-forwarded").is_some();
        // A forward naming its owner: only that owner, still at that fence, takes it and
        // queues it to its own coalescer. A marker without an owner is client-controlled and
        // therefore follows the ordinary protected-ref route.
        if forwarded && let Some(owner) = md.get(coalescer::OWNER_HEADER) {
            let owner = owner.to_str().unwrap_or_default();
            let fence = md
                .get(coalescer::FENCE_HEADER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            coalescer::verify_forwarded_owner(s, repo.repo.id, name.as_str(), owner, fence).await?;
            *verified_owner = true;
        }
        let job = LandJob {
            repo: repo.clone(),
            ref_name: name.clone(),
            head,
            expected_version: req.expected_version,
            allow_empty: req.allow_empty,
            change_request_id: req.change_request_id.clone(),
            caller: caller.clone(),
            meta: meta.clone(),
        };
        let routed = match land_route(protected, forwarded, *verified_owner) {
            LandRoute::Direct => coalescer::land(s, job, false, false).await,
            LandRoute::Ordinary | LandRoute::OwnerCoalescer => {
                coalescer::land(s, job, true, false).await
            }
        };
        let out = match routed {
            Ok(o) => o,
            Err(e) => {
                if let Some(v1::error_detail::Payload::Conflicts(set)) = &e.detail.payload {
                    s.forge
                        .land_conflict(repo, name.as_str(), &head, &req.change_request_id, set)
                        .await;
                }
                let label = match e.reason() {
                    ErrorReason::MergeConflict => "conflict",
                    ErrorReason::PolicyDenied | ErrorReason::Leased => "denied",
                    ErrorReason::LandContention => "contention",
                    _ => "error",
                };
                s.metrics.land(label);
                return Err(e);
            }
        };
        let newly = out.members.iter().filter(|m| !m.skipped).count();
        s.metrics
            .land(if newly == 0 { "skipped" } else { "landed" });
        if newly > 0 {
            s.audit(
                caller,
                meta,
                Some(repo),
                name.as_str(),
                "land",
                json!({
                    "version": out.version,
                    "target": out.target.to_text(),
                    "landed": out.members.iter().map(|m| json!({
                        "change_id": m.change_id.to_text(), "source": m.source.to_text(),
                        "result": m.result.to_text(), "skipped": m.skipped,
                    })).collect::<Vec<_>>(),
                }),
            )
            .await?;
        }
        let policy = s
            .policy_at(repo, name.as_str(), Some(&out.target))
            .await
            .unwrap_or(policy);
        let resp = v1::LandResponse {
            r#ref: Some(convert::ref_to(
                name.as_str(),
                Some(&out.target),
                out.version,
            )),
            landed: out
                .members
                .iter()
                .map(|m| v1::LandedChange {
                    change_id: Some(change_id_to(&m.change_id)),
                    source: Some(id_to(&m.source)),
                    result: Some(id_to(&m.result)),
                    skipped: m.skipped,
                })
                .collect(),
            attempts: out.attempts,
            event_seq: out.event_seq,
        };
        warm_after_land(
            &self.state,
            repo,
            out.target,
            out.members
                .iter()
                .filter(|m| !m.skipped)
                .map(|m| m.result)
                .collect(),
        );
        if newly > 0 {
            // Change requests whose changes this land published are recorded as landed.
            s.forge
                .landed(caller, repo, name.as_str(), &resp, &req.change_request_id)
                .await;
        }
        Ok((policy, resp))
    }

    async fn revert_impl(
        &self,
        md: &tonic::metadata::MetadataMap,
        req: v1::RevertRequest,
        meta: &RequestMeta,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::RevertResponse)> {
        let s = &self.state;
        let caller = s.caller(md).await?;
        let repo = s.repo(req.repo.as_ref()).await?;
        let policy = s.gate(&caller, &repo, Action::Commit, None).await?;
        let history = s.history(&repo);
        let c = id_from(req.changeset.as_ref(), "changeset")?;
        let cs = history.changeset(&c).await?;
        let parent = cs.first_parent().copied().ok_or_else(|| {
            ApiError::invalid("invalid_parents", "the empty root cannot be reverted")
        })?;
        let parent_root = *history.changeset(&parent).await?.root();
        let mut delta = Vec::new();
        for e in cs.delta() {
            match history.entry_at(&parent_root, &e.path).await? {
                Some(old) if old.mode != Mode::Tree => {
                    delta.push(DeltaEntry::put(e.path.clone(), old.mode, old.id, old.size))
                }
                _ => match e.op {
                    DeltaOp::Put { .. } => delta.push(DeltaEntry::delete(e.path.clone())),
                    DeltaOp::Delete => {}
                },
            }
        }
        let paths: Vec<String> = delta.iter().map(|e| e.path.to_string()).collect();
        s.require_paths(&caller, &repo, &policy, Action::Commit, &paths, None)?;
        let subject = history
            .meta_object(cs.meta())
            .await?
            .map(|m| m.message.lines().next().unwrap_or("").to_string())
            .unwrap_or_default();
        let message = if req.message.is_empty() {
            format!("Revert \"{subject}\"\n\nThis reverts {}.", c.to_text())
        } else {
            req.message.clone()
        };
        let change_id = match req.change_id.as_ref().filter(|c| !c.raw.is_empty()) {
            Some(x) => change_id_from(Some(x), "change_id")?,
            None => fresh_change_id()?,
        };
        let meta_id = self
            .make_meta(
                &caller,
                &repo,
                "",
                None,
                &message,
                req.provenance.as_ref(),
                None,
                None,
            )
            .await?;
        s.charge(
            &caller,
            Usage {
                changesets: 1,
                ..Usage::default()
            },
        )
        .await?;
        let built = s
            .writer
            .build(
                repo.org.id,
                repo.repo.id,
                vec![c],
                meta_id,
                change_id,
                delta,
            )
            .await?;
        let revert = history.store_built(&built, s.now_ms()).await?;
        let onto = match req.onto.as_ref().filter(|r| !r.expr.is_empty()) {
            Some(r) => resolve_rev(&history, Some(r)).await?,
            None => c,
        };
        let id = match history
            .rebase(&revert, &onto, &RebaseOptions::default(), s.now_ms())
            .await?
        {
            RebaseOutcome::Unchanged(id) | RebaseOutcome::Rebased(id) => id,
            RebaseOutcome::Conflicts(conflicts) => {
                return Err(convert::conflict_error(&conflicts, &onto, &revert));
            }
        };
        s.audit(
            &caller,
            meta,
            Some(&repo),
            &id.to_text(),
            "revert",
            json!({ "reverts": c.to_text() }),
        )
        .await?;
        let cs = view(s, &caller, &policy, &history, &id, true).await?;
        Ok((
            repo,
            policy,
            v1::RevertResponse {
                changeset: Some(cs),
            },
        ))
    }
}

macro_rules! write_rpc {
    ($self:ident, $req:ident, $impl:ident) => {{
        let meta = RequestMeta::of(&$req);
        let (md, _, inner) = $req.into_parts();
        let r = async {
            let (repo, policy, resp) = $self.$impl(&md, inner, &meta).await?;
            $self
                .state
                .with_epochs(&repo, &policy, tonic::Response::new(resp))
                .await
        }
        .await;
        finish(&meta, r)
    }};
}

#[tonic::async_trait]
impl WriteService for Writes {
    async fn upload_objects(
        &self,
        req: tonic::Request<tonic::Streaming<v1::UploadItem>>,
    ) -> Rpc<v1::UploadObjectsResponse> {
        let meta = RequestMeta::of(&req);
        let (md, _, stream) = req.into_parts();
        let r = self
            .upload_impl(&md, stream, &meta)
            .await
            .map(tonic::Response::new);
        finish(&meta, r)
    }

    async fn create_changeset(
        &self,
        req: tonic::Request<v1::CreateChangesetRequest>,
    ) -> Rpc<v1::CreateChangesetResponse> {
        write_rpc!(self, req, create_impl)
    }

    async fn rebase(&self, req: tonic::Request<v1::RebaseRequest>) -> Rpc<v1::RebaseResponse> {
        write_rpc!(self, req, rebase_impl)
    }

    async fn merge(&self, req: tonic::Request<v1::MergeRequest>) -> Rpc<v1::MergeResponse> {
        write_rpc!(self, req, merge_impl)
    }

    async fn land(&self, req: tonic::Request<v1::LandRequest>) -> Rpc<v1::LandResponse> {
        write_rpc!(self, req, land_impl)
    }

    async fn revert(&self, req: tonic::Request<v1::RevertRequest>) -> Rpc<v1::RevertResponse> {
        write_rpc!(self, req, revert_impl)
    }
}

#[cfg(test)]
mod tests {
    use super::{LandRoute, land_route};

    #[test]
    fn land_route_requires_a_verified_owner_for_forwarding() {
        assert_eq!(land_route(true, false, false), LandRoute::Ordinary);
        assert_eq!(land_route(true, true, false), LandRoute::Ordinary);
        assert_eq!(land_route(true, true, true), LandRoute::OwnerCoalescer);
        assert_eq!(land_route(false, true, true), LandRoute::Direct);
    }
}
