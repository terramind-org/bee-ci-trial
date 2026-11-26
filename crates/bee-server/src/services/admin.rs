//! `AdminService` (SPEC §6.4, §13, PRODUCT §4.3, §4.10): redaction lifecycle with epochs,
//! garbage collection (dry run and real), fsck, and signing-key rotation.

use std::sync::Arc;

use bee_core::{ObjectId, RepoPath};
use bee_history::LogOptions;
use bee_policy::{Action, Policy};
use bee_proto::v1::{self, ErrorReason, admin_service_server::AdminService};
use bee_store::{FsckOptions, GcOptions, OrgRow, SweepOutcome};
use serde_json::json;

use crate::auth::Caller;
use crate::convert::{id_from, id_to};
use crate::errors::{ApiError, ApiResult};
use crate::redaction::{self, State};
use crate::services::Rpc;
use crate::services::object::finish;
use crate::services::query::resolve_rev;
use crate::state::{AppState, RepoCtx, RequestMeta};
use crate::util::timestamp_ms;

/// The admin service.
pub struct Admin {
    state: Arc<AppState>,
}

impl Admin {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> Admin {
        Admin { state }
    }
}

impl Admin {
    /// Requires `action` from every principal of the chain under the org default policy.
    async fn org_admin(
        &self,
        caller: &Caller,
        org_name: &str,
        action: Action,
    ) -> ApiResult<OrgRow> {
        let s = &self.state;
        let org = s
            .meta
            .org_by_name(org_name)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("org {org_name} not found")))?;
        let text = s.org_text(&org).await?;
        let policy = Policy::from_sources(Some(&text), None).map_err(|e| {
            ApiError::new(ErrorReason::Internal, format!("invalid org policy: {e}"))
        })?;
        for a in &caller.actors {
            if !policy.actions_of(a).contains(&action) {
                return Err(ApiError::policy(
                    ErrorReason::PolicyDenied,
                    "members",
                    vec![format!("role:{}", action.as_str())],
                    format!("{} lacks `{}` in org {org_name}", a.id, action.as_str()),
                ));
            }
        }
        if !caller.token.allows_action(action) {
            return Err(ApiError::policy(
                ErrorReason::PolicyDenied,
                "token",
                vec![format!("token:{}", action.as_str())],
                "the token does not allow this action",
            ));
        }
        Ok(org)
    }

    fn propagation_bound_ms(&self) -> u64 {
        let s = &self.state;
        (s.cfg.blobs.signed_url_ttl_secs * 1000).max(s.cfg.auth.revocation_refresh_ms)
    }

    async fn targets(&self, repo: &RepoCtx, req: &v1::RedactRequest) -> ApiResult<Vec<ObjectId>> {
        let s = &self.state;
        let mut ids = Vec::new();
        for raw in &req.ids {
            ids.push(id_from(Some(raw), "ids")?);
        }
        let history = s.history(repo);
        if req.meta {
            // The metas of the changesets named by `ids`; with none named, of the changeset
            // `at` resolves to.
            let mut named = false;
            for id in ids.iter_mut() {
                if id.kind() == bee_core::ObjectKind::Changeset {
                    *id = *history.changeset(id).await?.meta();
                    named = true;
                }
            }
            if !named {
                let at = resolve_rev(&history, req.at.as_ref()).await?;
                ids.push(*history.changeset(&at).await?.meta());
            }
        }
        if !req.path.is_empty() {
            let path = RepoPath::new(req.path.clone()).map_err(ApiError::from)?;
            let at = resolve_rev(&history, req.at.as_ref()).await?;
            if req.all_versions {
                let mut token = None;
                loop {
                    let page = history
                        .path_log(
                            &at,
                            Some(&path),
                            &LogOptions {
                                limit: 200,
                                page_token: token.clone(),
                                ..LogOptions::default()
                            },
                        )
                        .await?;
                    for e in &page.entries {
                        let root = *history.changeset(&e.changeset).await?.root();
                        if let Some(entry) = history.entry_at(&root, &path).await?
                            && entry.mode.is_blob()
                        {
                            ids.push(entry.id);
                        }
                    }
                    match page.next_page_token {
                        Some(t) => token = Some(t),
                        None => break,
                    }
                }
            } else {
                let root = *history.changeset(&at).await?.root();
                let entry = history
                    .entry_at(&root, &path)
                    .await?
                    .ok_or_else(|| ApiError::not_found(format!("{path} not found")))?;
                ids.push(entry.id);
            }
        }
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            return Err(ApiError::invalid(
                "missing_field",
                "name ids, a path, or meta = true",
            ));
        }
        Ok(ids)
    }

    async fn redact_impl(
        &self,
        caller: &Caller,
        req: v1::RedactRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::RedactResponse> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        s.gate(caller, &repo, Action::Redact, None).await?;
        if req.purge_now {
            s.gate(caller, &repo, Action::Purge, None).await?;
        }
        if req.reason.trim().is_empty() {
            return Err(ApiError::invalid(
                "missing_field",
                "a reason is required for audit",
            ));
        }
        let ids = self.targets(&repo, &req).await?;
        let purge_after_ms = if req.purge_after_seconds > 0 {
            u64::from(req.purge_after_seconds) * 1000
        } else {
            s.cfg.redaction.quarantine_days * bee_store::DAY_MS
        };
        let mut epoch = 0;
        for id in &ids {
            epoch = if req.purge_now {
                redaction::purge(s, &repo, id, caller.id(), &req.reason).await?
            } else {
                redaction::quarantine(s, &repo, id, caller.id(), &req.reason, purge_after_ms)
                    .await?
            };
        }
        let kind = if req.purge_now {
            "redaction.purged"
        } else {
            "redaction.quarantined"
        };
        s.events
            .append(repo.repo.id, kind, &json!({ "objects": ids.iter().map(|i| i.to_text()).collect::<Vec<_>>(), "epoch": epoch }), s.now_ms())
            .await?;
        s.audit(
            caller,
            meta,
            Some(&repo),
            "redaction",
            kind,
            json!({ "objects": ids.len(), "reason": req.reason, "epoch": epoch }),
        )
        .await?;
        Ok(v1::RedactResponse {
            redaction_epoch: epoch,
            effective_by: Some(timestamp_ms(s.now_ms() + self.propagation_bound_ms())),
            redacted: ids.iter().map(id_to).collect(),
        })
    }

    async fn unredact_impl(
        &self,
        caller: &Caller,
        req: v1::UnredactRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::UnredactResponse> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        s.gate(caller, &repo, Action::Redact, None).await?;
        let mut epoch = s.redaction_epoch(&repo).await?;
        let mut done = Vec::new();
        for raw in &req.ids {
            let id = id_from(Some(raw), "ids")?;
            epoch = redaction::unredact(s, &repo, &id).await?;
            done.push(id.to_text());
        }
        s.events
            .append(
                repo.repo.id,
                "redaction.unredacted",
                &json!({ "objects": done, "epoch": epoch }),
                s.now_ms(),
            )
            .await?;
        s.audit(
            caller,
            meta,
            Some(&repo),
            "redaction",
            "unredact",
            json!({ "objects": done.len(), "epoch": epoch }),
        )
        .await?;
        Ok(v1::UnredactResponse {
            redaction_epoch: epoch,
        })
    }

    async fn purge_impl(
        &self,
        caller: &Caller,
        req: v1::PurgeNowRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::PurgeNowResponse> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        s.gate(caller, &repo, Action::Purge, None).await?;
        let mut epoch = s.redaction_epoch(&repo).await?;
        let mut purged = Vec::new();
        for raw in &req.ids {
            let id = id_from(Some(raw), "ids")?;
            let rec = redaction::record(s.kv.as_ref(), repo.org.id, &id).await?;
            if !rec.as_ref().is_some_and(|r| r.state == State::Quarantined) {
                return Err(ApiError::new(
                    ErrorReason::NotQuarantined,
                    format!("{id} is not quarantined; redact it first"),
                ));
            }
            let reason = if req.reason.is_empty() {
                rec.map(|r| r.reason).unwrap_or_default()
            } else {
                req.reason.clone()
            };
            epoch = redaction::purge(s, &repo, &id, caller.id(), &reason).await?;
            purged.push(id);
        }
        s.events
            .append(repo.repo.id, "redaction.purged", &json!({ "objects": purged.iter().map(|i| i.to_text()).collect::<Vec<_>>(), "epoch": epoch }), s.now_ms())
            .await?;
        s.audit(
            caller,
            meta,
            Some(&repo),
            "redaction",
            "purge",
            json!({ "objects": purged.len(), "epoch": epoch }),
        )
        .await?;
        Ok(v1::PurgeNowResponse {
            redaction_epoch: epoch,
            purged: purged.iter().map(id_to).collect(),
        })
    }

    async fn list_impl(
        &self,
        caller: &Caller,
        req: v1::ListRedactionsRequest,
    ) -> ApiResult<v1::ListRedactionsResponse> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        s.gate(caller, &repo, Action::Read, None).await?;
        let current = s.redaction_epoch(&repo).await?;
        let mut out = Vec::new();
        let records = redaction::list(s.kv.as_ref(), repo.org.id).await?;
        let mut seen = std::collections::BTreeSet::new();
        for r in records {
            if r.repo != repo.id_text() || r.epoch < req.since_epoch {
                continue;
            }
            let state = match r.state {
                State::Quarantined => v1::RedactionState::Quarantined,
                State::Purged => v1::RedactionState::Purged,
                State::Unredacted => v1::RedactionState::Unredacted,
            };
            if req.state != 0 && req.state != state as i32 {
                continue;
            }
            let Ok(id) = ObjectId::parse(&r.object) else {
                continue;
            };
            seen.insert(id);
            out.push(v1::Redaction {
                id: Some(id_to(&id)),
                reason: r.reason,
                principal: r.principal,
                time: Some(timestamp_ms(r.at_ms)),
                epoch: r.epoch,
                state: state as i32,
                purge_at: (r.state == State::Quarantined).then(|| timestamp_ms(r.purge_at_ms)),
            });
        }
        // Purges made below the server (for example by an earlier tool) are listed too.
        if req.state == 0 || req.state == v1::RedactionState::Purged as i32 {
            for r in s.meta.list_redactions(repo.org.id).await? {
                if seen.contains(&r.object) {
                    continue;
                }
                if s.meta
                    .object_paths(repo.repo.id, &r.object, bee_store::ReadFrom::Primary)
                    .await?
                    .is_empty()
                {
                    continue;
                }
                out.push(v1::Redaction {
                    id: Some(id_to(&r.object)),
                    reason: r.reason,
                    principal: r.principal,
                    time: Some(timestamp_ms(r.at_ms)),
                    epoch: 0,
                    state: v1::RedactionState::Purged as i32,
                    purge_at: None,
                });
            }
        }
        Ok(v1::ListRedactionsResponse {
            redactions: out,
            current_epoch: current,
            page: Some(v1::PageResponse::default()),
        })
    }

    async fn gc_impl(
        &self,
        caller: &Caller,
        req: v1::RunGcRequest,
        _meta: &RequestMeta,
    ) -> ApiResult<v1::RunGcResponse> {
        let s = &self.state;
        let org_name = if req.org.is_empty() {
            match &req.repo {
                Some(r) => s.repo(Some(r)).await?.org.name,
                None => return Err(ApiError::invalid("missing_field", "org is required")),
            }
        } else {
            req.org.clone()
        };
        let org = self.org_admin(caller, &org_name, Action::Purge).await?;
        let started = s.now_ms();
        let mut opts = GcOptions::at(started);
        opts.dry_run = req.dry_run;
        let window = u64::from(req.safety_window_seconds) * 1000;
        opts.upload_window_ms = opts.upload_window_ms.max(window);
        let report = bee_store::gc::run(s.meta.as_ref(), s.blobs.as_ref(), org.id, &opts).await?;
        let (changesets, objects) = match &report.outcome {
            Some(SweepOutcome::Swept {
                changesets,
                objects,
            }) => (changesets.len() as u64, objects.len() as u64),
            Some(SweepOutcome::Deferred { .. }) => (0, 0),
            None => (
                report.plan.sweep.changesets.len() as u64,
                report.plan.sweep.objects.len() as u64,
            ),
        };
        let mut bytes = 0u64;
        for id in &report.deleted_bytes {
            if let Ok(Some(row)) = s
                .meta
                .get_object(org.id, id, bee_store::ReadFrom::Primary)
                .await
            {
                bytes += row.size;
            }
        }
        s.forge
            .admin_audit(
                caller,
                &org.id.0.to_text(),
                &org.name,
                "admin.gc",
                json!({
                    "dry_run": req.dry_run,
                    "changesets": changesets,
                    "objects": objects,
                    "bytes": bytes,
                    "deferred": report.deferred(),
                }),
            )
            .await;
        Ok(v1::RunGcResponse {
            objects_collected: objects,
            bytes_collected: bytes,
            changesets_collected: changesets,
            dry_run: req.dry_run,
            started_at: Some(timestamp_ms(started)),
            finished_at: Some(timestamp_ms(s.now_ms())),
        })
    }

    /// `GetRepoHealth` (F416-032): counts, bytes, large files and warnings at a ref.
    async fn health_impl(
        &self,
        caller: &Caller,
        req: v1::GetRepoHealthRequest,
    ) -> ApiResult<v1::RepoHealth> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        let ref_name = if req.r#ref.is_empty() {
            "main".to_string()
        } else {
            req.r#ref.trim_start_matches("refs/heads/").to_string()
        };
        let name = bee_core::RefName::new(ref_name.clone()).map_err(ApiError::from)?;
        let policy = s
            .gate(caller, &repo, Action::Read, Some(ref_name.as_str()))
            .await?;
        let threshold = if req.large_file_bytes == 0 {
            DEFAULT_LARGE_FILE_BYTES
        } else {
            req.large_file_bytes
        };
        let refs = s
            .meta
            .list_refs(repo.repo.id, "")
            .await?
            .iter()
            .filter(|r| r.target.is_some())
            .count() as u64;
        let row = s
            .meta
            .get_ref(repo.repo.id, &name)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("ref {ref_name} not found")))?;
        let tip = row
            .target
            .ok_or_else(|| ApiError::not_found(format!("ref {ref_name} is deleted")))?;
        let history = s.history(&repo);
        let cs = history.changeset(&tip).await?;
        let depth = history
            .graph_node(&tip)
            .await
            .map(|g| g.depth + 1)
            .unwrap_or(0);
        let leaves = history.list_leaves(cs.root(), None).await?;
        let mut dirs = std::collections::BTreeSet::new();
        let mut out = v1::RepoHealth {
            repo: Some(v1::RepoRef {
                repo: Some(v1::repo_ref::Repo::FullName(repo.full_name.clone())),
            }),
            r#ref: ref_name.clone(),
            tip: Some(id_to(&tip)),
            ref_version: row.version,
            refs,
            history_depth: depth,
            large_file_bytes: threshold,
            ..Default::default()
        };
        let push_limit = policy.doc().push.max_file_size;
        let mut large: Vec<(String, u64, bee_core::ObjectId)> = Vec::new();
        for (path, entry) in &leaves {
            let p = path.to_string();
            let mut d = p.as_str();
            while let Some((parent, _)) = d.rsplit_once('/') {
                dirs.insert(parent.to_string());
                d = parent;
            }
            out.files += 1;
            out.tip_bytes += entry.size;
            out.largest_file_bytes = out.largest_file_bytes.max(entry.size);
            let readable = caller
                .actors
                .iter()
                .all(|a| policy.check_access(a, Action::Read, &p).allowed);
            if !readable {
                out.hidden_paths += 1;
                continue;
            }
            if entry.size >= threshold {
                large.push((p.clone(), entry.size, entry.id));
            }
            if let Some(limit) = push_limit
                && entry.size.saturating_mul(10) >= limit.saturating_mul(8)
            {
                out.warnings.push(v1::RepoHealthWarning {
                    code: "near_push_limit".into(),
                    message: format!(
                        "/{p} is {} bytes, at least 80% of push.max_file_size ({limit})",
                        entry.size
                    ),
                    path: p.clone().into_bytes(),
                });
            }
        }
        out.directories = dirs.len() as u64;
        large.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (p, size, id) in large.iter().take(MAX_LARGE_FILES) {
            out.warnings.push(v1::RepoHealthWarning {
                code: "large_file".into(),
                message: format!(
                    "/{p} is {size} bytes (threshold {threshold}); consider Git LFS or removing it"
                ),
                path: p.clone().into_bytes(),
            });
            out.large_files.push(v1::RepoHealthFile {
                path: p.clone().into_bytes(),
                size_bytes: *size,
                blob: Some(id_to(id)),
            });
        }
        if out.tip_bytes >= LARGE_TREE_BYTES {
            out.warnings.push(v1::RepoHealthWarning {
                code: "large_tree".into(),
                message: format!("the tree at {ref_name} holds {} bytes", out.tip_bytes),
                path: Vec::new(),
            });
        }
        if out.files >= MANY_FILES {
            out.warnings.push(v1::RepoHealthWarning {
                code: "many_files".into(),
                message: format!("the tree at {ref_name} holds {} files", out.files),
                path: Vec::new(),
            });
        }
        if refs >= MANY_REFS {
            out.warnings.push(v1::RepoHealthWarning {
                code: "many_refs".into(),
                message: format!("the repo has {refs} live refs"),
                path: Vec::new(),
            });
        }
        Ok(out)
    }

    async fn fsck_impl(
        &self,
        caller: &Caller,
        req: v1::RunFsckRequest,
    ) -> ApiResult<v1::RunFsckResponse> {
        let s = &self.state;
        let (org_name, repo_filter) = match &req.repo {
            Some(r) if r.repo.is_some() => {
                let ctx = s.repo(Some(r)).await?;
                (ctx.org.name.clone(), Some(ctx.repo.id))
            }
            _ => (req.org.clone(), None),
        };
        let org = self
            .org_admin(caller, &org_name, Action::ManageSettings)
            .await?;
        let opts = FsckOptions {
            eager: s.registry.eager_types(),
            ..FsckOptions::default()
        };
        let report = bee_store::fsck(s.meta.as_ref(), s.blobs.as_ref(), org.id, &opts).await?;
        let violations: Vec<v1::FsckViolation> = report
            .violations
            .iter()
            .filter(|v| repo_filter.is_none() || v.repo.is_none() || v.repo == repo_filter)
            .map(|v| v1::FsckViolation {
                kind: v.kind.as_str().to_string(),
                repo_id: v.repo.map(|r| r.0.to_text()).unwrap_or_default(),
                object: v.object.as_ref().map(id_to),
                detail: v.detail.clone(),
            })
            .collect();
        for v in &violations {
            if crate::integrity::is_integrity_violation(&v.kind) {
                let object = v
                    .object
                    .as_ref()
                    .and_then(|o| ObjectId::from_bytes(&o.raw).ok())
                    .map(|o| o.to_text())
                    .unwrap_or_default();
                s.metrics
                    .integrity_failure("fsck", &object, &v.detail, s.now_ms());
            }
        }
        s.forge
            .admin_audit(
                caller,
                &org.id.0.to_text(),
                &org.name,
                "admin.fsck",
                json!({
                    "clean": violations.is_empty(),
                    "violations": violations.len(),
                    "objects_checked": report.stats.objects,
                    "changesets_checked": report.stats.changesets,
                    "refs_checked": report.stats.refs,
                }),
            )
            .await;
        Ok(v1::RunFsckResponse {
            clean: violations.is_empty(),
            violations,
            objects_checked: report.stats.objects,
            changesets_checked: report.stats.changesets,
            refs_checked: report.stats.refs,
        })
    }
}

/// Default large-file threshold of `GetRepoHealth` (GitHub warns at 50 MiB).
const DEFAULT_LARGE_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// Most large files `GetRepoHealth` lists.
const MAX_LARGE_FILES: usize = 100;
/// A tree this large earns a `large_tree` warning (5 GiB).
const LARGE_TREE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// A tree with this many files earns a `many_files` warning.
const MANY_FILES: u64 = 500_000;
/// This many live refs earn a `many_refs` warning.
const MANY_REFS: u64 = 10_000;

macro_rules! admin_rpc {
    ($self:ident, $req:ident, |$caller:ident, $inner:ident, $meta:ident| $body:expr) => {{
        let $meta = RequestMeta::of(&$req);
        let r = async {
            let $caller = $self.state.caller($req.metadata()).await?;
            let $inner = $req.into_inner();
            $body.await.map(tonic::Response::new)
        }
        .await;
        finish(&$meta, r)
    }};
}

#[tonic::async_trait]
impl AdminService for Admin {
    async fn redact(&self, req: tonic::Request<v1::RedactRequest>) -> Rpc<v1::RedactResponse> {
        admin_rpc!(self, req, |caller, inner, meta| self
            .redact_impl(&caller, inner, &meta))
    }

    async fn unredact(
        &self,
        req: tonic::Request<v1::UnredactRequest>,
    ) -> Rpc<v1::UnredactResponse> {
        admin_rpc!(self, req, |caller, inner, meta| self
            .unredact_impl(&caller, inner, &meta))
    }

    async fn list_redactions(
        &self,
        req: tonic::Request<v1::ListRedactionsRequest>,
    ) -> Rpc<v1::ListRedactionsResponse> {
        admin_rpc!(self, req, |caller, inner, meta| self
            .list_impl(&caller, inner))
    }

    async fn run_gc(&self, req: tonic::Request<v1::RunGcRequest>) -> Rpc<v1::RunGcResponse> {
        admin_rpc!(self, req, |caller, inner, meta| self
            .gc_impl(&caller, inner, &meta))
    }

    async fn purge_now(
        &self,
        req: tonic::Request<v1::PurgeNowRequest>,
    ) -> Rpc<v1::PurgeNowResponse> {
        admin_rpc!(self, req, |caller, inner, meta| self
            .purge_impl(&caller, inner, &meta))
    }

    async fn get_repo_health(
        &self,
        req: tonic::Request<v1::GetRepoHealthRequest>,
    ) -> Rpc<v1::RepoHealth> {
        admin_rpc!(self, req, |caller, inner, meta| self
            .health_impl(&caller, inner))
    }

    async fn run_fsck(&self, req: tonic::Request<v1::RunFsckRequest>) -> Rpc<v1::RunFsckResponse> {
        admin_rpc!(self, req, |caller, inner, _meta| self
            .fsck_impl(&caller, inner))
    }

    async fn create_backup(&self, req: tonic::Request<v1::CreateBackupRequest>) -> Rpc<v1::Backup> {
        admin_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            crate::services::support::require_operator(s, &caller).await?;
            let rec = crate::backup::create(s, &inner.cell, &inner.note).await?;
            s.audit(
                &caller,
                &meta,
                None,
                &rec.id,
                "backup.create",
                json!({ "location": rec.location }),
            )
            .await?;
            Ok::<_, ApiError>(crate::backup::backup_proto(s, &rec))
        })
    }

    async fn list_backups(
        &self,
        req: tonic::Request<v1::ListBackupsRequest>,
    ) -> Rpc<v1::ListBackupsResponse> {
        admin_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            crate::services::support::require_operator(s, &caller).await?;
            let all: Vec<_> = crate::backup::list(s)
                .await?
                .into_iter()
                .filter(|b| inner.cell.is_empty() || b.cell == inner.cell)
                .collect();
            let (token, size) = crate::services::support::page_of(&inner.page);
            let (page, next) = crate::services::support::offset_page(&all, &token, size)?;
            Ok::<_, ApiError>(v1::ListBackupsResponse {
                backups: page
                    .iter()
                    .map(|b| crate::backup::backup_proto(s, b))
                    .collect(),
                page: crate::services::support::next_page(next),
            })
        })
    }

    async fn start_restore(
        &self,
        req: tonic::Request<v1::StartRestoreRequest>,
    ) -> Rpc<v1::Restore> {
        admin_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            crate::services::support::require_operator(s, &caller).await?;
            let rec = crate::backup::start_restore(s, inner).await?;
            s.audit(
                &caller,
                &meta,
                None,
                &rec.id,
                "restore.start",
                json!({ "backup": rec.backup_id, "target_cell": rec.target_cell, "drill": rec.drill }),
            )
            .await?;
            Ok::<_, ApiError>(crate::backup::restore_proto(&rec))
        })
    }

    async fn get_restore(&self, req: tonic::Request<v1::GetRestoreRequest>) -> Rpc<v1::Restore> {
        admin_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            crate::services::support::require_operator(s, &caller).await?;
            if inner.id.is_empty() {
                return Err(ApiError::invalid("missing_field", "id is required"));
            }
            let rec: crate::backup::RestoreRec = crate::services::support::get_json(
                s.kv.as_ref(),
                crate::backup::NS_RESTORE,
                &inner.id,
            )
            .await?
            .ok_or_else(|| ApiError::not_found(format!("restore {}", inner.id)))?;
            Ok::<_, ApiError>(crate::backup::restore_proto(&rec))
        })
    }

    async fn rotate_signing_key(
        &self,
        req: tonic::Request<v1::RotateSigningKeyRequest>,
    ) -> Rpc<v1::RotateSigningKeyResponse> {
        admin_rpc!(self, req, |caller, inner, meta| async {
            // Signing keys sign every tenant's tokens: server operators only.
            crate::services::support::require_operator(&self.state, &caller).await?;
            if !inner.algorithm.is_empty() && inner.algorithm != "ed25519" {
                return Err(ApiError::invalid(
                    "unsupported_algorithm",
                    "only ed25519 keys are issued",
                ));
            }
            let k = crate::keys::rotate(&self.state).await?;
            self.state
                .audit(
                    &caller,
                    &meta,
                    None,
                    &k.id.to_string(),
                    "rotate_signing_key",
                    json!({}),
                )
                .await?;
            Ok(v1::RotateSigningKeyResponse {
                key: Some(v1::SigningKey {
                    key_id: k.id.to_string(),
                    algorithm: "ed25519".into(),
                    public_key: k.public_key,
                    signing: true,
                    created_at: Some(timestamp_ms(k.created_ms)),
                    retired_at: None,
                }),
            })
        })
    }

    async fn retire_signing_key(
        &self,
        req: tonic::Request<v1::RetireSigningKeyRequest>,
    ) -> Rpc<v1::RetireSigningKeyResponse> {
        admin_rpc!(self, req, |caller, inner, meta| async {
            // Signing keys sign every tenant's tokens: server operators only.
            crate::services::support::require_operator(&self.state, &caller).await?;
            let id: u32 = inner
                .key_id
                .parse()
                .map_err(|_| ApiError::invalid("invalid_id", "key_id must be a number"))?;
            crate::keys::retire(&self.state, id).await?;
            self.state
                .audit(
                    &caller,
                    &meta,
                    None,
                    &inner.key_id,
                    "retire_signing_key",
                    json!({}),
                )
                .await?;
            Ok::<_, ApiError>(v1::RetireSigningKeyResponse {})
        })
    }
}
