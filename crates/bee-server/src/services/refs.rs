//! `RefService` (SPEC §5, §7.5): listing, plain CAS updates with fast-forward rules and force
//! permission for non-protected refs, deletes, renames and the ref log. Protected refs move
//! only through Land.

use std::sync::Arc;

use bee_core::{ObjectId, ObjectKind, RefName};
use bee_policy::{Action, Policy, RefAction};
use bee_proto::v1::{self, ErrorReason, ref_service_server::RefService};
use bee_store::{Expected, ReadFrom, RefStep, RefUpdate};
use serde_json::json;

use crate::auth::Caller;
use crate::convert::{id_from, id_to, ref_to};
use crate::errors::{ApiError, ApiResult};
use crate::events::new_event;
use crate::services::Rpc;
use crate::services::object::finish;
use crate::services::query::{page_size, page_token};
use crate::state::{AppState, RepoCtx, RequestMeta};

/// The ref service.
pub struct Refs {
    state: Arc<AppState>,
}

/// Most ref-log entries RenameRef carries to the new name.
const MAX_RENAME_STEPS: usize = 100_000;

/// Server-table namespace of ref redirects left by RenameRef (`<repo id>/<old>` → new name).
pub const NS_REF_REDIRECT: &str = "ref_redirect";

fn redirect_key(repo: &RepoCtx, name: &str) -> String {
    format!("{}/{name}", repo.id_text())
}

/// The name a renamed ref now goes by, when `name` has no live ref and was renamed.
pub async fn renamed_to(state: &AppState, repo: &RepoCtx, name: &str) -> ApiResult<Option<String>> {
    Ok(state
        .kv
        .get(NS_REF_REDIRECT, redirect_key(repo, name).as_bytes())
        .await?
        .map(|v| String::from_utf8_lossy(&v).into_owned()))
}

impl Refs {
    async fn refuse_if_queue_or_policy_protected(
        &self,
        repo: &RepoCtx,
        name: &RefName,
    ) -> ApiResult<()> {
        if self
            .state
            .hub
            .queue_or_policy_protected(&self.state, repo, name.as_str())
            .await?
        {
            return Err(ApiError::policy(
                ErrorReason::PolicyDenied,
                "protected",
                vec![format!("ref:update:{}", name.as_str())],
                format!("{} is protected and moves only through Land", name.as_str()),
            ));
        }
        Ok(())
    }

    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> Refs {
        Refs { state }
    }
}

impl Refs {
    /// Token action and policy check for a ref operation.
    async fn authorize(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        name: &RefName,
        action: RefAction,
    ) -> ApiResult<Arc<Policy>> {
        let s = &self.state;
        let own = name.workspace_owner().is_some_and(|o| o == caller.id());
        let token_action = if own {
            Action::Commit
        } else {
            Action::ManageRefs
        };
        let policy = s
            .gate_token_only(caller, repo, token_action, Some(name.as_str()))
            .await?;
        let actors = s.policy_actors(caller, repo).await?;
        let d = policy.check_ref_chain(&actors, action, name.as_str());
        if !d.allowed {
            return Err(ApiError::policy(
                ErrorReason::PolicyDenied,
                &d.rule,
                vec![format!("ref:{}:{}", ref_action_name(action), name.as_str())],
                d.detail,
            ));
        }
        Ok(policy)
    }

    async fn target_exists(&self, repo: &RepoCtx, id: &ObjectId) -> ApiResult<()> {
        if id.kind() != ObjectKind::Changeset {
            return Err(ApiError::invalid(
                "wrong_kind",
                format!("{id} is not a changeset"),
            ));
        }
        if self
            .state
            .meta
            .get_changeset(repo.repo.id, id, ReadFrom::Primary)
            .await?
            .is_none()
        {
            return Err(ApiError::not_found(format!("changeset {id} not found")));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn cas(
        &self,
        caller: &Caller,
        meta: &RequestMeta,
        repo: &RepoCtx,
        name: &RefName,
        expected: Expected,
        old: Option<ObjectId>,
        target: Option<ObjectId>,
        reason: &str,
    ) -> ApiResult<u64> {
        let s = &self.state;
        let version_after = match expected {
            Expected::Version(v) => v + 1,
            Expected::Absent => 0,
        };
        let out = s
            .writer
            .update_ref(RefUpdate {
                repo: repo.repo.id,
                name: name.clone(),
                expected,
                steps: vec![RefStep {
                    target,
                    landed: Vec::new(),
                }],
                principal: caller.id().to_string(),
                at_ms: s.now_ms(),
                events: vec![new_event(
                    "ref.updated",
                    &json!({
                        "ref": name.as_str(),
                        "old": old.map(|o| o.to_text()),
                        "new": target.map(|t| t.to_text()),
                        "version": version_after,
                        "principal_chain": caller.chain_ids(),
                        "reason": reason,
                        "request_id": meta.request_id,
                    }),
                    s.now_ms(),
                )],
            })
            .await
            .map_err(|e| {
                let mut a: ApiError = e.into();
                if let Some(v1::error_detail::Payload::VersionMismatch(v)) = &mut a.detail.payload {
                    v.r#ref = name.as_str().to_string();
                    if let Expected::Version(x) = expected {
                        v.expected_version = x;
                    }
                }
                a
            })?;
        s.metrics
            .events
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        s.audit(
            caller,
            meta,
            Some(repo),
            name.as_str(),
            "ref",
            json!({
                "old": old.map(|o| o.to_text()),
                "new": target.map(|t| t.to_text()),
                "version": out.new_version,
                "reason": reason,
            }),
        )
        .await?;
        // Change requests follow their refs (git pushes report with their push options).
        if !meta.method.starts_with("git-receive-pack") {
            s.forge
                .ref_moved(repo, name.as_str(), target.as_ref(), caller, &[], false)
                .await;
        }
        Ok(out.new_version)
    }

    async fn list_impl(
        &self,
        caller: &Caller,
        req: v1::ListRefsRequest,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::ListRefsResponse)> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        let policy = s.gate(caller, &repo, Action::Read, None).await?;
        let size = page_size(req.page.as_ref());
        let after = page_token(req.page.as_ref());
        let rows = s.meta.list_refs(repo.repo.id, &req.prefix).await?;
        let mut refs = Vec::new();
        let mut next = String::new();
        for r in rows {
            let Some(t) = r.target else { continue };
            if after.as_deref().is_some_and(|a| r.name.as_str() <= a) {
                continue;
            }
            if refs.len() == size {
                next = refs
                    .last()
                    .map(|x: &v1::Ref| x.name.clone())
                    .unwrap_or_default();
                break;
            }
            refs.push(ref_to(r.name.as_str(), Some(&t), r.version));
        }
        Ok((
            repo,
            policy,
            v1::ListRefsResponse {
                refs,
                page: Some(v1::PageResponse {
                    next_page_token: next,
                }),
            },
        ))
    }

    async fn create_impl(
        &self,
        caller: &Caller,
        req: v1::CreateRefRequest,
        meta: &RequestMeta,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::Ref)> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        let name = RefName::new(req.name.clone()).map_err(ApiError::from)?;
        let policy = self
            .authorize(caller, &repo, &name, RefAction::Create)
            .await?;
        let target = id_from(req.target.as_ref(), "target")?;
        self.target_exists(&repo, &target).await?;
        let version = match s.meta.get_ref(repo.repo.id, &name).await? {
            Some(r) if r.target.is_some() => {
                self.refuse_if_queue_or_policy_protected(&repo, &name)
                    .await?;
                return Err(ApiError::new(
                    ErrorReason::AlreadyExists,
                    format!("ref {} exists", name.as_str()),
                ));
            }
            Some(r) => {
                // A deleted ref keeps its numbering.
                self.cas(
                    caller,
                    meta,
                    &repo,
                    &name,
                    Expected::Version(r.version),
                    None,
                    Some(target),
                    &req.reason,
                )
                .await?
            }
            None => {
                self.cas(
                    caller,
                    meta,
                    &repo,
                    &name,
                    Expected::Absent,
                    None,
                    Some(target),
                    &req.reason,
                )
                .await?
            }
        };
        Ok((repo, policy, ref_to(name.as_str(), Some(&target), version)))
    }

    async fn update_impl(
        &self,
        caller: &Caller,
        req: v1::UpdateRefRequest,
        meta: &RequestMeta,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::Ref)> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        let name = RefName::new(req.name.clone()).map_err(ApiError::from)?;
        let target = id_from(req.target.as_ref(), "target")?;
        self.target_exists(&repo, &target).await?;
        let current = s
            .meta
            .get_ref(repo.repo.id, &name)
            .await?
            .filter(|r| r.target.is_some())
            .ok_or_else(|| {
                ApiError::not_found(format!("ref {} not found; use CreateRef", name.as_str()))
            })?;
        let old = current.target;
        let ff = match old {
            Some(o) => s.history(&repo).is_ancestor(&o, &target).await?,
            None => true,
        };
        let action = if ff {
            RefAction::Update
        } else {
            RefAction::ForceUpdate
        };
        if !ff && !req.allow_non_fast_forward {
            return Err(ApiError::new(
                ErrorReason::NotFastForward,
                format!(
                    "{} does not descend from the tip of {}; set allow_non_fast_forward",
                    target,
                    name.as_str()
                ),
            ));
        }
        let policy = self.authorize(caller, &repo, &name, action).await?;
        self.refuse_if_queue_or_policy_protected(&repo, &name)
            .await?;
        if !ff {
            // Force needs the update permission too.
            self.authorize(caller, &repo, &name, RefAction::Update)
                .await?;
        }
        let expected = if req.expected_version == 0 {
            current.version
        } else {
            req.expected_version
        };
        let version = self
            .cas(
                caller,
                meta,
                &repo,
                &name,
                Expected::Version(expected),
                old,
                Some(target),
                &req.reason,
            )
            .await?;
        Ok((repo, policy, ref_to(name.as_str(), Some(&target), version)))
    }

    async fn delete_impl(
        &self,
        caller: &Caller,
        req: v1::DeleteRefRequest,
        meta: &RequestMeta,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::DeleteRefResponse)> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        let name = RefName::new(req.name.clone()).map_err(ApiError::from)?;
        let policy = self
            .authorize(caller, &repo, &name, RefAction::Delete)
            .await?;
        let current = s
            .meta
            .get_ref(repo.repo.id, &name)
            .await?
            .filter(|r| r.target.is_some())
            .ok_or_else(|| ApiError::not_found(format!("ref {} not found", name.as_str())))?;
        self.refuse_if_queue_or_policy_protected(&repo, &name)
            .await?;
        let expected = if req.expected_version == 0 {
            current.version
        } else {
            req.expected_version
        };
        self.cas(
            caller,
            meta,
            &repo,
            &name,
            Expected::Version(expected),
            current.target,
            None,
            &req.reason,
        )
        .await?;
        Ok((repo, policy, v1::DeleteRefResponse {}))
    }

    async fn rename_impl(
        &self,
        caller: &Caller,
        req: v1::RenameRefRequest,
        meta: &RequestMeta,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::Ref)> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        let name = RefName::new(req.name.clone()).map_err(ApiError::from)?;
        let new_name = RefName::new(req.new_name.clone()).map_err(ApiError::from)?;
        self.authorize(caller, &repo, &name, RefAction::Delete)
            .await?;
        let policy = self
            .authorize(caller, &repo, &new_name, RefAction::Create)
            .await?;
        let current = s
            .meta
            .get_ref(repo.repo.id, &name)
            .await?
            .filter(|r| r.target.is_some())
            .ok_or_else(|| ApiError::not_found(format!("ref {} not found", name.as_str())))?;
        if req.expected_version != 0 && req.expected_version != current.version {
            let mut e = ApiError::new(ErrorReason::VersionMismatch, "stale expected_version");
            e.detail.payload = Some(v1::error_detail::Payload::VersionMismatch(
                v1::VersionMismatch {
                    r#ref: name.as_str().to_string(),
                    expected_version: req.expected_version,
                    current_version: current.version,
                    current_target: current.target.as_ref().map(id_to),
                },
            ));
            return Err(e);
        }
        let target = current.target;
        // The version line and ref log move with the ref: the old ref's log is replayed onto
        // the new name as one multi-step update (same targets and landed entries, in order).
        let mut steps = Vec::new();
        let mut from = 1;
        loop {
            let page = s.meta.ref_log(repo.repo.id, &name, from, 1000).await?;
            let Some(last) = page.last() else { break };
            from = last.version + 1;
            for e in &page {
                steps.push(RefStep {
                    target: e.new,
                    landed: e.landed.clone(),
                });
            }
            if steps.len() > MAX_RENAME_STEPS {
                return Err(ApiError::invalid(
                    "rename_too_long",
                    format!(
                        "ref {} has more than {MAX_RENAME_STEPS} versions to carry; create a new \
                         ref at its target instead",
                        name.as_str()
                    ),
                ));
            }
            if page.len() < 1000 {
                break;
            }
        }
        if steps.last().map(|st| st.target) != Some(target) {
            steps.push(RefStep {
                target,
                landed: Vec::new(),
            });
        }
        let new_existing = s.meta.get_ref(repo.repo.id, &new_name).await?;
        let expected_new = match new_existing {
            Some(r) if r.target.is_some() => {
                return Err(ApiError::new(
                    ErrorReason::AlreadyExists,
                    format!("ref {} exists", new_name.as_str()),
                ));
            }
            Some(r) => Expected::Version(r.version),
            None => Expected::Absent,
        };
        let reason = if req.reason.is_empty() {
            format!("rename from {}", name.as_str())
        } else {
            req.reason.clone()
        };
        let version = s
            .writer
            .update_ref(RefUpdate {
                repo: repo.repo.id,
                name: new_name.clone(),
                expected: expected_new,
                steps,
                principal: caller.id().to_string(),
                at_ms: s.now_ms(),
                events: vec![new_event(
                    "ref.updated",
                    &json!({
                        "ref": new_name.as_str(),
                        "old": null,
                        "new": target.map(|t| t.to_text()),
                        "renamed_from": name.as_str(),
                        "principal_chain": caller.chain_ids(),
                        "reason": reason,
                        "request_id": meta.request_id,
                    }),
                    s.now_ms(),
                )],
            })
            .await
            .map_err(ApiError::from)?
            .new_version;
        s.audit(
            caller,
            meta,
            Some(&repo),
            new_name.as_str(),
            "ref.rename",
            json!({ "from": name.as_str(), "new": target.map(|t| t.to_text()), "version": version }),
        )
        .await?;
        // Two CAS steps: if the old ref moved meanwhile, undo the new one and report it.
        if let Err(e) = self
            .cas(
                caller,
                meta,
                &repo,
                &name,
                Expected::Version(current.version),
                target,
                None,
                &format!("renamed to {}", new_name.as_str()),
            )
            .await
        {
            let _ = self
                .cas(
                    caller,
                    meta,
                    &repo,
                    &new_name,
                    Expected::Version(version),
                    target,
                    None,
                    "rename rolled back",
                )
                .await;
            return Err(e);
        }
        // The old name now redirects to the new one (reads of the old name resolve the new
        // ref while no ref of that name exists); the new name is live, so it redirects no more.
        s.kv.apply(vec![
            crate::kv::KvOp::Put {
                ns: NS_REF_REDIRECT.into(),
                key: redirect_key(&repo, name.as_str()).into_bytes(),
                value: new_name.as_str().as_bytes().to_vec(),
            },
            crate::kv::KvOp::Delete {
                ns: NS_REF_REDIRECT.into(),
                key: redirect_key(&repo, new_name.as_str()).into_bytes(),
            },
        ])
        .await?;
        if !meta.method.starts_with("git-receive-pack") {
            s.forge
                .ref_moved(
                    &repo,
                    new_name.as_str(),
                    target.as_ref(),
                    caller,
                    &[],
                    false,
                )
                .await;
        }
        Ok((
            repo,
            policy,
            ref_to(new_name.as_str(), target.as_ref(), version),
        ))
    }

    async fn log_impl(
        &self,
        caller: &Caller,
        req: v1::GetRefLogRequest,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::GetRefLogResponse)> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        let name = RefName::new(req.name.clone()).map_err(ApiError::from)?;
        let policy = s
            .gate(caller, &repo, Action::Read, Some(name.as_str()))
            .await?;
        let size = page_size(req.page.as_ref());
        let from = match page_token(req.page.as_ref()) {
            Some(t) => t
                .parse::<u64>()
                .map_err(|_| ApiError::invalid("bad_page_token", "bad page token"))?,
            None => req.from_version.max(1),
        };
        let rows = s.meta.ref_log(repo.repo.id, &name, from, size + 1).await?;
        let next = rows
            .get(size)
            .map(|e| e.version.to_string())
            .unwrap_or_default();
        let entries = rows
            .into_iter()
            .take(size)
            .map(|e| v1::RefLogEntry {
                version: e.version,
                old: e.old.as_ref().map(id_to),
                new: e.new.as_ref().map(id_to),
                principal: e.principal,
                time: Some(crate::util::timestamp_ms(e.at_ms)),
                landed: e
                    .landed
                    .iter()
                    .map(|l| v1::LandedChange {
                        change_id: Some(crate::convert::change_id_to(&l.change_id)),
                        source: Some(id_to(&l.source)),
                        result: Some(id_to(&l.result)),
                        skipped: false,
                    })
                    .collect(),
            })
            .collect();
        Ok((
            repo,
            policy,
            v1::GetRefLogResponse {
                entries,
                page: Some(v1::PageResponse {
                    next_page_token: next,
                }),
            },
        ))
    }
}

fn ref_action_name(a: RefAction) -> &'static str {
    match a {
        RefAction::Create => "create",
        RefAction::Update => "update",
        RefAction::Delete => "delete",
        RefAction::ForceUpdate => "force",
    }
}

macro_rules! ref_rpc {
    ($self:ident, $req:ident, $impl:ident) => {{
        let meta = RequestMeta::of(&$req);
        let r = async {
            let caller = $self.state.caller($req.metadata()).await?;
            let (repo, policy, resp) = $self.$impl(&caller, $req.into_inner(), &meta).await?;
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
impl RefService for Refs {
    async fn list_refs(
        &self,
        req: tonic::Request<v1::ListRefsRequest>,
    ) -> Rpc<v1::ListRefsResponse> {
        crate::services::query::unary!(self, req, list_impl)
    }

    async fn create_ref(&self, req: tonic::Request<v1::CreateRefRequest>) -> Rpc<v1::Ref> {
        ref_rpc!(self, req, create_impl)
    }

    async fn update_ref(&self, req: tonic::Request<v1::UpdateRefRequest>) -> Rpc<v1::Ref> {
        ref_rpc!(self, req, update_impl)
    }

    async fn delete_ref(
        &self,
        req: tonic::Request<v1::DeleteRefRequest>,
    ) -> Rpc<v1::DeleteRefResponse> {
        ref_rpc!(self, req, delete_impl)
    }

    async fn get_ref_log(
        &self,
        req: tonic::Request<v1::GetRefLogRequest>,
    ) -> Rpc<v1::GetRefLogResponse> {
        crate::services::query::unary!(self, req, log_impl)
    }

    async fn rename_ref(&self, req: tonic::Request<v1::RenameRefRequest>) -> Rpc<v1::Ref> {
        ref_rpc!(self, req, rename_impl)
    }
}
