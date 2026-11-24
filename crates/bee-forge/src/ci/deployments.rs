//! Deployments requested through the API, their statuses, custom protection rules and the
//! `green/*` status that gates them (PRODUCT §4.14, `DeploymentService`).
//!
//! Workflow jobs that name an environment get their deployment from the environment gate
//! ([`super::environments`]); this file adds what `DeploymentService` needs on top:
//!
//! - **Create**: the ref must resolve (a pinned changeset must be reachable from its tip), the
//!   required contexts must have passed (unset = every check reported on the changeset), the
//!   environment's ref policy must allow the ref and, with `require_green`, the changeset
//!   must be green (or, with `deploy_green_tip`, the newest green changeset of the ref is
//!   deployed instead). Environments are created on first use. A deployment to an environment
//!   with required reviewers or custom protection rules is created `PENDING` and the call
//!   answers `APPROVAL_REQUIRED`.
//! - **Statuses** append to the history; `SUCCESS` records the environment URL and, with
//!   `auto_inactive`, marks earlier successful non-transient, non-production deployments of
//!   the environment `INACTIVE`. A deployment waiting on protection may only fail.
//! - **Reviews**: required reviewers approve or reject (prevent-self-review and admin bypass as
//!   for jobs); custom rules are answered by their app (`ReportDeploymentProtectionRuleResult`).
//!   The last approval queues the deployment, or the job it gates.
//! - **Green**: a changeset is green when it has post-land check runs and every latest one
//!   passed; `green/<ref>` is the newest green changeset on the ref's first-parent chain. The
//!   ref itself is written by [`super::green`] as checks pass; the status here is computed from
//!   the check runs, so it is right even where the VCS port cannot write refs.

use bee_proto::v1::{
    Deployment, DeploymentGreenStatus, DeploymentPendingReview, DeploymentProtectionRule,
    DeploymentRefPolicyMode, DeploymentReview, DeploymentReviewDecision, DeploymentState,
    DeploymentStatus, ErrorReason, ObjectId, Repo, SubjectKind, WorkflowConclusion, WorkflowStatus,
};

use super::convert::{bee_ref, git_ref};
use super::engine::{Ci, Effects, job_of, job_row, repo_ref_of, status_of};
use super::environments::{
    deployment_event, enabled_rule_apps, env_key, may_review, ref_allowed, valid_env_name,
};
use super::model::{DeploymentRecord, EnvironmentRecord, JobIndex, JobRecord, RunRecord};
use super::{BISECTS, DEPLOYMENTS, ENVIRONMENTS, JOB_INDEX, JOBS, RUNS, SURFACE};
use crate::audit::AuditDraft;
use crate::checks::{RunFilter, policy_status};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::store::{Key, Row, Scan, Tx};

/// How far back `green/<ref>` is looked for.
pub const GREEN_LOOKBACK: usize = 200;

/// A deployment requested through the API.
#[derive(Clone, Debug, Default)]
pub struct NewDeployment {
    /// Ref to deploy (`main`, `refs/heads/main`, `tags/v1`).
    pub git_ref: String,
    /// Optional pin; must be the ref's tip or reachable from it.
    pub changeset: Option<ObjectId>,
    /// Task (`deploy` when empty).
    pub task: String,
    /// Environment (`production` when empty).
    pub environment: String,
    /// Free text.
    pub description: String,
    /// Free-form JSON.
    pub payload_json: String,
    /// The environment goes away when no longer needed.
    pub transient: bool,
    /// A production environment (defaults to the name being `production`).
    pub production: bool,
    /// Require the ref to contain the default ref (it is never merged in for the caller).
    pub auto_merge: bool,
    /// Contexts that must have passed; `None` = every check reported on the changeset.
    pub required_contexts: Option<Vec<String>>,
    /// With `require_green` and a tip that is not green, deploy the newest green changeset of
    /// the ref instead of refusing (ignored with a pinned `changeset`).
    pub deploy_green_tip: bool,
}

/// A new deployment status.
#[derive(Clone, Debug, Default)]
pub struct NewDeploymentStatus {
    /// The state.
    pub state: DeploymentState,
    /// Free text.
    pub description: String,
    /// Moves the deployment to this environment when set.
    pub environment: String,
    /// Logs.
    pub log_url: String,
    /// Where the deployed thing is reachable.
    pub environment_url: String,
    /// On `SUCCESS`, mark earlier deployments of the environment inactive.
    pub auto_inactive: bool,
}

/// `ListDeployments` filters (empty = any).
#[derive(Clone, Debug, Default)]
pub struct DeploymentFilter {
    /// Environment.
    pub environment: String,
    /// Ref as requested.
    pub git_ref: String,
    /// Deployed changeset (hex).
    pub changeset_hex: String,
    /// Task.
    pub task: String,
    /// Latest state; `Unspecified` = any.
    pub state: DeploymentState,
    /// Creator principal id.
    pub creator_id: String,
}

fn deployment_row(repo_id: &str, d: &DeploymentRecord, n1: i64, version: u64) -> Row {
    let dep = d.deployment.clone().unwrap_or_default();
    let row = Row::new(repo_id, dep.id.clone())
        .k1(env_key(&dep.environment))
        .k2(dep.workflow_run_id.clone())
        .n1(n1)
        .msg(d);
    if version > 0 {
        row.version(version)
    } else {
        row
    }
}

fn active(dep: &Deployment) -> bool {
    dep.state == DeploymentState::Success as i32
}

fn hex_of(id: &Option<ObjectId>) -> String {
    id.as_ref().map(|c| hex(&c.raw)).unwrap_or_default()
}

impl Ci {
    async fn deployment_record(
        tx: &mut Tx,
        repo_id: &str,
        id: &str,
    ) -> Result<(DeploymentRecord, Row)> {
        if id.is_empty() {
            return Err(ForgeError::missing("deployment_id"));
        }
        tx.get_msg::<DeploymentRecord>(&DEPLOYMENTS, repo_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("deployment {id}")))
    }

    /// Latest check runs of a changeset by name.
    async fn latest_checks(
        &self,
        cx: &Cx,
        repo_id: &str,
        cs: &ObjectId,
    ) -> Result<Vec<bee_proto::v1::CheckRun>> {
        self.checks
            .list(
                cx,
                repo_id,
                &RunFilter {
                    changeset_hex: Some(hex(&cs.raw)),
                    latest_only: true,
                    ..RunFilter::default()
                },
            )
            .await
    }

    /// Whether a changeset passed post-land testing: `(green, failing, pending)`.
    pub async fn green_of(
        &self,
        cx: &Cx,
        repo_id: &str,
        cs: &ObjectId,
    ) -> Result<(bool, Vec<String>, Vec<String>)> {
        let mut failing = Vec::new();
        let mut pending = Vec::new();
        let mut any = false;
        for r in self.latest_checks(cx, repo_id, cs).await? {
            if !r.post_land {
                continue;
            }
            any = true;
            match policy_status(bee_proto::v1::CheckStatus::try_from(r.status).unwrap_or_default())
            {
                bee_policy::CheckStatus::Failure => failing.push(r.name.clone()),
                bee_policy::CheckStatus::Pending => pending.push(r.name.clone()),
                _ => {}
            }
        }
        failing.sort();
        pending.sort();
        Ok((
            any && failing.is_empty() && pending.is_empty(),
            failing,
            pending,
        ))
    }

    /// `GetDeploymentGreenStatus`: whether `changeset` (default: the ref's tip) is green, the
    /// newest green changeset on the ref, and whether a bisect of the ref is running.
    pub async fn green_status(
        &self,
        cx: &Cx,
        repo: &Repo,
        git: &str,
        changeset: Option<ObjectId>,
    ) -> Result<DeploymentGreenStatus> {
        if git.is_empty() {
            return Err(ForgeError::missing("ref"));
        }
        let name = bee_ref(git);
        let tip = match changeset.filter(|c| !c.raw.is_empty()) {
            Some(c) => c,
            None => self
                .vcs
                .ref_tip(repo, &name)
                .await?
                .ok_or_else(|| ForgeError::not_found(format!("ref {name}")))?,
        };
        let (green, failing, pending) = self.green_of(cx, &repo.id, &tip).await?;
        let mut green_tip = green.then(|| tip.clone());
        let mut cur = Some(tip.clone());
        let mut seen = 0;
        while green_tip.is_none() && seen < GREEN_LOOKBACK {
            let Some(c) = cur.take() else { break };
            seen += 1;
            let parent = match self.vcs.first_parent(repo, &c).await {
                Ok(p) => p,
                Err(e) if e.reason == ErrorReason::Unimplemented => None,
                Err(e) => return Err(e),
            };
            if let Some(p) = &parent
                && self.green_of(cx, &repo.id, p).await?.0
            {
                green_tip = Some(p.clone());
            }
            cur = parent;
        }
        let bisect_in_progress = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan(
                &BISECTS,
                &Scan::scope(repo.id.clone(), 100).key(Key::K1("running".into())),
            )
            .await?
            .iter()
            .any(|r| r.k2.as_deref() == Some(name.as_str()))
        };
        Ok(DeploymentGreenStatus {
            r#ref: name.clone(),
            tip: Some(tip),
            green,
            green_ref: format!("green/{name}"),
            green_tip,
            failing_contexts: failing,
            pending_contexts: pending,
            bisect_in_progress,
            evaluated_at: Some(ts(self.now())),
        })
    }

    /// What `deploy_green_tip` deploys: `tip` itself unless `env_name` requires green and
    /// `tip` is not green; then the newest green changeset of `ref_name` (the target
    /// `green/<ref>` tracks), or POLICY_DENIED (`require_green`) when there is none within
    /// [`GREEN_LOOKBACK`] changesets.
    async fn green_tip_for(
        &self,
        cx: &Cx,
        repo: &Repo,
        env_name: &str,
        ref_name: &str,
        tip: ObjectId,
    ) -> Result<ObjectId> {
        let require = match self.get_environment(cx, repo, env_name).await {
            Ok(e) => e.require_green,
            Err(e) if e.reason == ErrorReason::NotFound => false,
            Err(e) => return Err(e),
        };
        if !require {
            return Ok(tip);
        }
        let st = self.green_status(cx, repo, ref_name, Some(tip)).await?;
        st.green_tip.ok_or_else(|| {
            ForgeError::policy_denied(
                "require_green",
                vec![format!("green/{ref_name}")],
                format!(
                    "{env_name} requires a green changeset and {ref_name} has none in its last {GREEN_LOOKBACK} changesets"
                ),
            )
        })
    }

    /// Creates a deployment (`CreateDeployment`). Errors: NOT_FOUND (ref), INVALID_ARGUMENT
    /// (changeset not reachable, bad payload), POLICY_DENIED (required contexts, ref policy,
    /// not green), MERGE_CONFLICT (`auto_merge` and the ref is behind the default ref),
    /// APPROVAL_REQUIRED (created, waiting on reviewers or protection rules).
    pub async fn create_deployment(
        &self,
        cx: &Cx,
        repo: &Repo,
        d: NewDeployment,
    ) -> Result<Deployment> {
        if d.git_ref.is_empty() {
            return Err(ForgeError::missing("ref"));
        }
        let env_name = if d.environment.is_empty() {
            "production".to_string()
        } else {
            d.environment.clone()
        };
        valid_env_name(&env_name)?;
        if !d.payload_json.is_empty()
            && serde_json::from_str::<serde_json::Value>(&d.payload_json).is_err()
        {
            return Err(ForgeError::invalid(
                "bad_payload",
                "payload_json is not JSON",
            ));
        }
        let name = bee_ref(&d.git_ref);
        let tip = self
            .vcs
            .ref_tip(repo, &name)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("ref {name}")))?;
        let cs = match d.changeset.clone().filter(|c| !c.raw.is_empty()) {
            Some(c) if c != tip => {
                if !self.vcs.is_ancestor(repo, &c, &tip).await? {
                    return Err(ForgeError::invalid(
                        "unreachable_changeset",
                        format!("{} is not reachable from {name}", hex(&c.raw)),
                    ));
                }
                c
            }
            _ => tip.clone(),
        };
        let default = bee_ref(if repo.default_ref.is_empty() {
            "main"
        } else {
            &repo.default_ref
        });
        if d.auto_merge
            && name != default
            && let Some(dt) = self.vcs.ref_tip(repo, &default).await?
            && !self.vcs.is_ancestor(repo, &dt, &cs).await?
        {
            return Err(ForgeError::new(
                ErrorReason::MergeConflict,
                format!(
                    "{name} is behind {default}; CI does not write to refs, so merge {default} into it and deploy again"
                ),
            )
            .with_subcode("auto_merge_behind"));
        }
        let pinned = d.changeset.as_ref().is_some_and(|c| !c.raw.is_empty());
        let cs = if d.deploy_green_tip && !pinned {
            Box::pin(self.green_tip_for(cx, repo, &env_name, &name, cs)).await?
        } else {
            cs
        };
        let checks = self.latest_checks(cx, &repo.id, &cs).await?;
        let wanted: Vec<String> = match &d.required_contexts {
            Some(list) => list.clone(),
            None => checks.iter().map(|c| c.name.clone()).collect(),
        };
        let mut missing = Vec::new();
        for ctx in &wanted {
            let ok = checks.iter().any(|c| {
                c.name == *ctx
                    && matches!(
                        policy_status(
                            bee_proto::v1::CheckStatus::try_from(c.status).unwrap_or_default()
                        ),
                        bee_policy::CheckStatus::Success
                            | bee_policy::CheckStatus::Neutral
                            | bee_policy::CheckStatus::Skipped
                    )
            });
            if !ok {
                missing.push(ctx.clone());
            }
        }
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            return Err(ForgeError::policy_denied(
                "required_contexts",
                missing.clone(),
                format!("required contexts have not passed: {}", missing.join(", ")),
            ));
        }
        let mut fresh_env = None;
        let env = match self.get_environment(cx, repo, &env_name).await {
            Ok(e) => e,
            Err(e) if e.reason == ErrorReason::NotFound => {
                let e = bee_proto::v1::DeploymentEnvironment {
                    repo: repo_ref_of(&repo.id),
                    name: env_name.clone(),
                    ref_policy_mode: DeploymentRefPolicyMode::All as i32,
                    ..Default::default()
                };
                fresh_env = Some(e.clone());
                e
            }
            Err(e) => return Err(e),
        };
        if !ref_allowed(&env, &git_ref(&name), &default) {
            return Err(ForgeError::policy_denied(
                "environment_ref_policy",
                vec![name.clone()],
                format!(
                    "{name} is not allowed to deploy to {} due to environment protection rules",
                    env.name
                ),
            ));
        }
        if env.require_green {
            let (green, failing, pending) = self.green_of(cx, &repo.id, &cs).await?;
            if !green {
                let mut what: Vec<String> = failing.into_iter().chain(pending).collect();
                if what.is_empty() {
                    what.push("post_land".into());
                }
                return Err(ForgeError::policy_denied(
                    "require_green",
                    what,
                    format!(
                        "{} requires a green changeset; {} has not passed post-land testing",
                        env.name,
                        hex(&cs.raw)
                    ),
                ));
            }
        }
        let pending = !env.reviewers.is_empty();
        let rules = enabled_rule_apps(&env);
        let waiting = pending || !rules.is_empty();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        if let Some(mut e) = fresh_env {
            e.id = tx.new_id()?;
            e.created_at = Some(ts(now));
            e.updated_at = Some(ts(now));
            tx.put(
                &ENVIRONMENTS,
                &Row::new(repo.id.clone(), env_key(&env_name)).msg(&EnvironmentRecord {
                    environment: Some(e),
                }),
            )
            .await?;
        }
        let state = if waiting {
            DeploymentState::Pending
        } else {
            DeploymentState::Queued
        };
        let dep = Deployment {
            id: tx.new_id()?,
            repo: repo_ref_of(&repo.id),
            r#ref: name.clone(),
            changeset: Some(cs.clone()),
            task: if d.task.is_empty() {
                "deploy".into()
            } else {
                d.task.clone()
            },
            environment: env.name.clone(),
            original_environment: env.name.clone(),
            description: d.description.clone(),
            payload_json: d.payload_json.clone(),
            transient_environment: d.transient,
            production_environment: d.production || env.name.eq_ignore_ascii_case("production"),
            creator: Some(cx.principal()),
            state: state as i32,
            workflow_run_id: String::new(),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
        };
        let status = DeploymentStatus {
            id: tx.new_id()?,
            deployment_id: dep.id.clone(),
            state: state as i32,
            environment: env.name.clone(),
            creator: Some(cx.principal()),
            created_at: Some(ts(now)),
            ..Default::default()
        };
        let rec = DeploymentRecord {
            deployment: Some(dep.clone()),
            statuses: vec![status],
            reviews: Vec::new(),
            job_id: String::new(),
            pending,
            wait_started_ms: 0,
            rule_pending: rules,
        };
        tx.insert(&DEPLOYMENTS, &deployment_row(&repo.id, &rec, now as i64, 0))
            .await?;
        for kind in ["deployment.created", "deployment_status.created"] {
            tx.emit_event(deployment_event(kind, &repo.id, &repo.org_id, &dep))
                .await?;
        }
        if waiting {
            tx.emit_event(deployment_event(
                "deployment_review.requested",
                &repo.id,
                &repo.org_id,
                &dep,
            ))
            .await?;
        }
        tx.audit(
            AuditDraft::new("deployment.create", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Deployment, &dep.id)
                .after(
                    &serde_json::json!({
                        "environment": env.name, "ref": name, "changeset": hex(&cs.raw),
                        "task": dep.task,
                    })
                    .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        if waiting {
            return Err(ForgeError::new(
                ErrorReason::ApprovalRequired,
                format!(
                    "deployment {} was created and waits on the protection rules of {}",
                    dep.id, env.name
                ),
            )
            .with_subcode("pending_review"));
        }
        Ok(dep)
    }

    /// One deployment.
    pub async fn get_deployment(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Deployment> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(Self::deployment_record(&mut tx, &repo.id, id)
            .await?
            .0
            .deployment
            .unwrap_or_default())
    }

    /// Deployments matching `f`, newest first.
    pub async fn find_deployments(
        &self,
        cx: &Cx,
        repo: &Repo,
        f: &DeploymentFilter,
    ) -> Result<Vec<Deployment>> {
        let all = self.list_deployments(cx, repo, &f.environment).await?;
        let want_ref = (!f.git_ref.is_empty()).then(|| bee_ref(&f.git_ref));
        Ok(all
            .into_iter()
            .filter(|d| want_ref.as_ref().is_none_or(|r| d.r#ref == *r))
            .filter(|d| f.changeset_hex.is_empty() || hex_of(&d.changeset) == f.changeset_hex)
            .filter(|d| f.task.is_empty() || d.task == f.task)
            .filter(|d| f.state == DeploymentState::Unspecified || d.state == f.state as i32)
            .filter(|d| {
                f.creator_id.is_empty() || d.creator.as_ref().is_some_and(|c| c.id == f.creator_id)
            })
            .collect())
    }

    /// Deletes a deployment. Errors: INVALID_STATE (the active deployment of a non-transient
    /// environment).
    pub async fn delete_deployment(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::deployment_record(&mut tx, &repo.id, id).await?;
        let dep = rec.deployment.unwrap_or_default();
        if active(&dep) && !dep.transient_environment {
            return Err(ForgeError::invalid_state(format!(
                "deployment {id} is the active deployment of {}; mark it inactive first",
                dep.environment
            )));
        }
        tx.delete(&DEPLOYMENTS, &repo.id, id).await?;
        tx.audit(
            AuditDraft::new("deployment.delete", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Deployment, id),
        )
        .await?;
        tx.commit().await
    }

    /// Appends a status (`CreateDeploymentStatus`). Errors: INVALID_STATE (the deployment
    /// still waits on protection rules and the state is not a failure).
    pub async fn create_deployment_status(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        s: NewDeploymentStatus,
    ) -> Result<DeploymentStatus> {
        if s.state == DeploymentState::Unspecified {
            return Err(ForgeError::missing("state"));
        }
        if !s.environment.is_empty() {
            valid_env_name(&s.environment)?;
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::deployment_record(&mut tx, &repo.id, id).await?;
        let mut dep = rec.deployment.clone().unwrap_or_default();
        let gated = rec.pending || !rec.rule_pending.is_empty();
        if gated
            && !matches!(
                s.state,
                DeploymentState::Failure | DeploymentState::Error | DeploymentState::Inactive
            )
        {
            return Err(ForgeError::invalid_state(format!(
                "deployment {id} waits on the protection rules of {}",
                dep.environment
            )));
        }
        let now = tx.now_ms();
        if !s.environment.is_empty() && !s.environment.eq_ignore_ascii_case(&dep.environment) {
            dep.environment = s.environment.clone();
        }
        let status = DeploymentStatus {
            id: tx.new_id()?,
            deployment_id: id.to_string(),
            state: s.state as i32,
            description: s.description.clone(),
            environment: dep.environment.clone(),
            log_url: s.log_url.clone(),
            environment_url: s.environment_url.clone(),
            creator: Some(cx.principal()),
            created_at: Some(ts(now)),
        };
        dep.state = s.state as i32;
        dep.updated_at = Some(ts(now));
        rec.deployment = Some(dep.clone());
        rec.statuses.push(status.clone());
        tx.delete(&DEPLOYMENTS, &repo.id, id).await?;
        tx.insert(
            &DEPLOYMENTS,
            &deployment_row(&repo.id, &rec, row.n1.unwrap_or(now as i64), 0),
        )
        .await?;
        let mut inactivated = Vec::new();
        if s.state == DeploymentState::Success {
            if !s.environment_url.is_empty()
                && let Some((mut er, erow)) = tx
                    .get_msg::<EnvironmentRecord>(
                        &ENVIRONMENTS,
                        &repo.id,
                        &env_key(&dep.environment),
                    )
                    .await?
            {
                let mut e = er.environment.clone().unwrap_or_default();
                e.url = s.environment_url.clone();
                e.updated_at = Some(ts(now));
                er.environment = Some(e);
                tx.update(
                    &ENVIRONMENTS,
                    &Row::new(repo.id.clone(), env_key(&dep.environment))
                        .version(erow.version)
                        .msg(&er),
                )
                .await?;
            }
            if s.auto_inactive {
                let others = tx
                    .scan_msgs::<DeploymentRecord>(
                        &DEPLOYMENTS,
                        &Scan::scope(repo.id.clone(), 1000).key(Key::K1(env_key(&dep.environment))),
                    )
                    .await?;
                for (mut o, orow) in others {
                    let mut od = o.deployment.clone().unwrap_or_default();
                    if od.id >= dep.id
                        || !active(&od)
                        || od.transient_environment
                        || od.production_environment
                    {
                        continue;
                    }
                    od.state = DeploymentState::Inactive as i32;
                    od.updated_at = Some(ts(now));
                    o.statuses.push(DeploymentStatus {
                        id: tx.new_id()?,
                        deployment_id: od.id.clone(),
                        state: DeploymentState::Inactive as i32,
                        description: format!("superseded by deployment {}", dep.id),
                        environment: od.environment.clone(),
                        creator: Some(cx.principal()),
                        created_at: Some(ts(now)),
                        ..Default::default()
                    });
                    o.deployment = Some(od.clone());
                    tx.update(
                        &DEPLOYMENTS,
                        &deployment_row(&repo.id, &o, orow.n1.unwrap_or(0), orow.version),
                    )
                    .await?;
                    inactivated.push(od);
                }
            }
        }
        tx.emit_event(deployment_event(
            "deployment_status.created",
            &repo.id,
            &repo.org_id,
            &dep,
        ))
        .await?;
        for od in &inactivated {
            tx.emit_event(deployment_event(
                "deployment_status.created",
                &repo.id,
                &repo.org_id,
                od,
            ))
            .await?;
        }
        tx.audit(
            AuditDraft::new("deployment.status", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Deployment, id)
                .after(
                    &serde_json::json!({
                        "state": s.state.as_str_name(),
                        "inactivated": inactivated.iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
                    })
                    .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        Ok(status)
    }

    /// One status of a deployment.
    pub async fn get_deployment_status(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        status_id: &str,
    ) -> Result<DeploymentStatus> {
        self.deployment_statuses(cx, repo, id)
            .await?
            .into_iter()
            .find(|s| s.id == status_id)
            .ok_or_else(|| ForgeError::not_found(format!("deployment status {status_id}")))
    }

    /// Review decisions recorded on a deployment, oldest first.
    pub async fn deployment_reviews(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
    ) -> Result<Vec<DeploymentReview>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(Self::deployment_record(&mut tx, &repo.id, id)
            .await?
            .0
            .reviews)
    }

    /// Deployments waiting on review in a repo (one environment when `env` is set), with
    /// whether `actor` may approve each.
    pub async fn pending_reviews(
        &self,
        cx: &Cx,
        repo: &Repo,
        env: &str,
        actor: &str,
        actor_is_admin: bool,
    ) -> Result<Vec<DeploymentPendingReview>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut scan = Scan::scope(repo.id.clone(), 1000);
        if !env.is_empty() {
            scan = scan.key(Key::K1(env_key(env)));
        }
        let mut out = Vec::new();
        for (d, _) in tx
            .scan_msgs::<DeploymentRecord>(&DEPLOYMENTS, &scan)
            .await?
        {
            if !d.pending && d.rule_pending.is_empty() {
                continue;
            }
            if !d.job_id.is_empty() {
                let waiting = tx
                    .get_msg::<JobRecord>(&JOBS, &repo.id, &d.job_id)
                    .await?
                    .is_some_and(|(j, _)| status_of(job_of(&j)) == WorkflowStatus::Waiting);
                if !waiting {
                    continue;
                }
            }
            let dep = d.deployment.clone().unwrap_or_default();
            let e = tx
                .get_msg::<EnvironmentRecord>(&ENVIRONMENTS, &repo.id, &env_key(&dep.environment))
                .await?
                .and_then(|(r, _)| r.environment)
                .unwrap_or_default();
            out.push(DeploymentPendingReview {
                deployment_id: dep.id.clone(),
                environment: dep.environment.clone(),
                workflow_run_id: dep.workflow_run_id.clone(),
                wait_timer_seconds: e.wait_timer_seconds,
                wait_timer_started_at: (d.wait_started_ms > 0).then(|| ts(d.wait_started_ms)),
                reviewers: e.reviewers.clone(),
                caller_can_approve: d.pending
                    && may_review(&e, &dep, actor, actor_is_admin).is_ok(),
                pending_rule_app_slugs: d.rule_pending.clone(),
            });
        }
        out.sort_by(|a, b| b.deployment_id.cmp(&a.deployment_id));
        Ok(out)
    }

    /// Approves or rejects one pending deployment as a required reviewer
    /// (`ReviewPendingDeployment`). Errors: PERMISSION_DENIED, INVALID_STATE.
    pub async fn review_deployment(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        decision: DeploymentReviewDecision,
        comment: &str,
        actor_is_admin: bool,
    ) -> Result<Deployment> {
        if decision == DeploymentReviewDecision::Unspecified {
            return Err(ForgeError::missing("decision"));
        }
        let (rec, _) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::deployment_record(&mut tx, &repo.id, id).await?
        };
        let dep = rec.deployment.clone().unwrap_or_default();
        if !rec.job_id.is_empty() {
            let out = self
                .review_deployments(
                    cx,
                    repo,
                    &dep.workflow_run_id,
                    std::slice::from_ref(&dep.environment),
                    decision,
                    comment,
                    actor_is_admin,
                )
                .await?;
            return out.into_iter().find(|d| d.id == id).ok_or_else(|| {
                ForgeError::invalid_state("the deployment is not waiting on review")
            });
        }
        if !rec.pending {
            return Err(ForgeError::invalid_state(format!(
                "deployment {id} is not waiting on review"
            )));
        }
        let env = self.get_environment(cx, repo, &dep.environment).await?;
        let bypass = may_review(&env, &dep, &cx.actor_id, actor_is_admin)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::deployment_record(&mut tx, &repo.id, id).await?;
        if !rec.pending {
            return Err(ForgeError::invalid_state(format!(
                "deployment {id} is not waiting on review"
            )));
        }
        let now = tx.now_ms();
        rec.pending = false;
        rec.reviews.push(DeploymentReview {
            deployment_id: id.to_string(),
            environment: dep.environment.clone(),
            decision: decision as i32,
            reviewer: Some(cx.principal()),
            comment: comment.to_string(),
            bypassed: bypass,
            created_at: Some(ts(now)),
        });
        let next = if decision == DeploymentReviewDecision::Rejected {
            Some(DeploymentState::Failure)
        } else if rec.rule_pending.is_empty() {
            Some(DeploymentState::Queued)
        } else {
            None
        };
        let dep = self
            .settle_manual(&mut tx, repo, &mut rec, next, comment, now)
            .await?;
        tx.update(
            &DEPLOYMENTS,
            &deployment_row(&repo.id, &rec, row.n1.unwrap_or(0), row.version),
        )
        .await?;
        tx.emit_event(deployment_event(
            "deployment_review.decided",
            &repo.id,
            &repo.org_id,
            &dep,
        ))
        .await?;
        tx.audit(
            AuditDraft::new("deployment.review", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Deployment, id)
                .after(
                    &serde_json::json!({ "decision": decision.as_str_name(), "bypassed": bypass })
                        .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        Ok(dep)
    }

    /// Moves a manual deployment to `next` (if any) after a decision.
    async fn settle_manual(
        &self,
        tx: &mut Tx,
        repo: &Repo,
        rec: &mut DeploymentRecord,
        next: Option<DeploymentState>,
        comment: &str,
        now: u64,
    ) -> Result<Deployment> {
        let mut dep = rec.deployment.clone().unwrap_or_default();
        if let Some(state) = next {
            dep.state = state as i32;
            dep.updated_at = Some(ts(now));
            rec.statuses.push(DeploymentStatus {
                id: tx.new_id()?,
                deployment_id: dep.id.clone(),
                state: state as i32,
                description: comment.to_string(),
                environment: dep.environment.clone(),
                creator: Some(bee_proto::v1::Principal {
                    id: "system".into(),
                    ..Default::default()
                }),
                created_at: Some(ts(now)),
                ..Default::default()
            });
            rec.deployment = Some(dep.clone());
            tx.emit_event(deployment_event(
                "deployment_status.created",
                &repo.id,
                &repo.org_id,
                &dep,
            ))
            .await?;
        }
        Ok(dep)
    }

    /// Adds an app-backed custom protection rule to an environment.
    pub async fn add_protection_rule(
        &self,
        cx: &Cx,
        repo: &Repo,
        env: &str,
        app_id: &str,
    ) -> Result<DeploymentProtectionRule> {
        if app_id.is_empty() {
            return Err(ForgeError::missing("app_id"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut er, row)) = tx
            .get_msg::<EnvironmentRecord>(&ENVIRONMENTS, &repo.id, &env_key(env))
            .await?
        else {
            return Err(ForgeError::not_found(format!("environment {env}")));
        };
        let mut e = er.environment.clone().unwrap_or_default();
        if e.protection_rules.iter().any(|r| r.app_id == app_id) {
            return Err(ForgeError::already_exists(format!(
                "protection rule for app {app_id}"
            )));
        }
        if e.protection_rules.len() >= 6 {
            return Err(ForgeError::too_large(
                "protection_rules",
                6,
                e.protection_rules.len() as u64 + 1,
            ));
        }
        let rule = DeploymentProtectionRule {
            id: tx.new_id()?,
            app_id: app_id.to_string(),
            app_slug: app_id.to_string(),
            enabled: true,
        };
        e.protection_rules.push(rule.clone());
        e.updated_at = Some(ts(tx.now_ms()));
        er.environment = Some(e.clone());
        tx.update(
            &ENVIRONMENTS,
            &Row::new(repo.id.clone(), env_key(env))
                .version(row.version)
                .msg(&er),
        )
        .await?;
        tx.audit(
            AuditDraft::new("environment.protection_rule_add", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Deployment, &e.id)
                .after(&serde_json::json!({ "app_id": app_id, "rule": rule.id }).to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok(rule)
    }

    /// Removes a custom protection rule.
    pub async fn remove_protection_rule(
        &self,
        cx: &Cx,
        repo: &Repo,
        env: &str,
        rule_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut er, row)) = tx
            .get_msg::<EnvironmentRecord>(&ENVIRONMENTS, &repo.id, &env_key(env))
            .await?
        else {
            return Err(ForgeError::not_found(format!("environment {env}")));
        };
        let mut e = er.environment.clone().unwrap_or_default();
        let before = e.protection_rules.len();
        e.protection_rules.retain(|r| r.id != rule_id);
        if e.protection_rules.len() == before {
            return Err(ForgeError::not_found(format!("protection rule {rule_id}")));
        }
        e.updated_at = Some(ts(tx.now_ms()));
        er.environment = Some(e.clone());
        tx.update(
            &ENVIRONMENTS,
            &Row::new(repo.id.clone(), env_key(env))
                .version(row.version)
                .msg(&er),
        )
        .await?;
        tx.audit(
            AuditDraft::new("environment.protection_rule_remove", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Deployment, &e.id)
                .after(&serde_json::json!({ "rule": rule_id }).to_string()),
        )
        .await?;
        tx.commit().await
    }

    /// Records a custom protection rule's answer (`ReportDeploymentProtectionRuleResult`). The
    /// caller must be the rule's app (principal id = app id). The last approval releases the
    /// deployment (or queues the job it gates); a rejection fails it. Errors:
    /// PERMISSION_DENIED (not a rule of the environment), INVALID_STATE (not waiting on it).
    pub async fn report_protection_rule(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        environment: &str,
        decision: DeploymentReviewDecision,
        comment: &str,
    ) -> Result<()> {
        if decision == DeploymentReviewDecision::Unspecified {
            return Err(ForgeError::missing("decision"));
        }
        let app = cx.actor_id.strip_prefix("app:").unwrap_or(&cx.actor_id);
        let mut fx = Effects::default();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::deployment_record(&mut tx, &repo.id, id).await?;
        let dep = rec.deployment.clone().unwrap_or_default();
        if !environment.is_empty() && !environment.eq_ignore_ascii_case(&dep.environment) {
            return Err(ForgeError::invalid(
                "environment_mismatch",
                format!("deployment {id} targets {}", dep.environment),
            ));
        }
        let env = tx
            .get_msg::<EnvironmentRecord>(&ENVIRONMENTS, &repo.id, &env_key(&dep.environment))
            .await?
            .and_then(|(r, _)| r.environment)
            .unwrap_or_default();
        if !env.protection_rules.iter().any(|r| r.app_id == app) {
            return Err(ForgeError::permission_denied(format!(
                "{} is not a protection rule app of {}",
                cx.actor_id, dep.environment
            )));
        }
        if !rec.rule_pending.iter().any(|a| a == app) {
            return Err(ForgeError::invalid_state(format!(
                "deployment {id} is not waiting on {app}"
            )));
        }
        let now = tx.now_ms();
        rec.rule_pending.retain(|a| a != app);
        rec.reviews.push(DeploymentReview {
            deployment_id: id.to_string(),
            environment: dep.environment.clone(),
            decision: decision as i32,
            reviewer: Some(cx.principal()),
            comment: comment.to_string(),
            bypassed: false,
            created_at: Some(ts(now)),
        });
        let rejected = decision == DeploymentReviewDecision::Rejected;
        if rejected {
            rec.rule_pending.clear();
            rec.pending = false;
        }
        let released = !rejected && !rec.pending && rec.rule_pending.is_empty();
        if rec.job_id.is_empty() {
            let next = if rejected {
                Some(DeploymentState::Failure)
            } else if released {
                Some(DeploymentState::Queued)
            } else {
                None
            };
            self.settle_manual(&mut tx, repo, &mut rec, next, comment, now)
                .await?;
            tx.update(
                &DEPLOYMENTS,
                &deployment_row(&repo.id, &rec, row.n1.unwrap_or(0), row.version),
            )
            .await?;
        } else {
            tx.update(
                &DEPLOYMENTS,
                &deployment_row(&repo.id, &rec, row.n1.unwrap_or(0), row.version),
            )
            .await?;
            if rejected || released {
                self.settle_job(&mut tx, repo, &rec, &env, rejected, comment, &mut fx)
                    .await?;
            }
        }
        tx.emit_event(deployment_event(
            "deployment_review.decided",
            &repo.id,
            &repo.org_id,
            &dep,
        ))
        .await?;
        tx.audit(
            AuditDraft::new("deployment.protection_rule_result", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Deployment, id)
                .after(
                    &serde_json::json!({ "app": app, "decision": decision.as_str_name() })
                        .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        self.apply(cx, fx).await;
        Ok(())
    }

    /// Queues (or fails) the job a deployment gates once its protection settled.
    #[allow(clippy::too_many_arguments)]
    async fn settle_job(
        &self,
        tx: &mut Tx,
        repo: &Repo,
        rec: &DeploymentRecord,
        env: &bee_proto::v1::DeploymentEnvironment,
        rejected: bool,
        comment: &str,
        fx: &mut Effects,
    ) -> Result<()> {
        let dep = rec.deployment.clone().unwrap_or_default();
        let run_id = dep.workflow_run_id.clone();
        let Some((run, _)) = tx.get_msg::<RunRecord>(&RUNS, &repo.id, &run_id).await? else {
            return Ok(());
        };
        let Some((mut j, jrow)) = tx
            .get_msg::<JobRecord>(&JOBS, &repo.id, &rec.job_id)
            .await?
        else {
            return Ok(());
        };
        if status_of(job_of(&j)) != WorkflowStatus::Waiting {
            return Ok(());
        }
        let now = tx.now_ms();
        if rejected {
            j.env_state = "rejected".into();
            j.error = format!(
                "The deployment to {} was rejected by a protection rule{}",
                dep.environment,
                if comment.is_empty() {
                    String::new()
                } else {
                    format!(": {comment}")
                }
            );
            self.finish_job(tx, &repo.id, &mut j, WorkflowConclusion::Failure, "", fx)
                .await?;
        } else if env.wait_timer_seconds > 0 {
            j.wait_until_ms = now + u64::from(env.wait_timer_seconds) * 1000;
            tx.put(
                &JOB_INDEX,
                &Row::new("", job_of(&j).id.clone())
                    .n1(j.wait_until_ms as i64)
                    .msg(&JobIndex {
                        repo_id: repo.id.clone(),
                        org_id: repo.org_id.clone(),
                        run_id: run_id.clone(),
                    }),
            )
            .await?;
        } else {
            j.env_state = "approved".into();
            self.deployment_progress(tx, &repo.id, &dep.id, DeploymentState::Queued, &repo.org_id)
                .await?;
            self.enqueue_job(tx, &repo.id, &run, &mut j, fx).await?;
        }
        tx.update(&JOBS, &job_row(&repo.id, &j).version(jrow.version))
            .await?;
        fx.advance.insert((repo.id.clone(), run_id));
        Ok(())
    }
}
