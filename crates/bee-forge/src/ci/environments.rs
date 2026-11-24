//! Environments, protection rules, deployments and reviews.
//!
//! A job that names an `environment` passes its gate before it is queued: the ref must be
//! allowed (all refs, protected refs — the default branch and `release/*` — or custom
//! patterns), then required reviewers must approve (prevent-self-review and admin bypass
//! honoured), then the wait timer must elapse. With `require_green` the run's changeset must
//! be green first ([`Ci::green_gate`]): a failing post-land check refuses the job, pending ones
//! or a running bisect of the ref hold it (re-checked every `green_poll_ms`). Every gated job gets a deployment record whose
//! statuses follow the job (`queued`, `in_progress`, `success`/`failure`). Environments that
//! do not exist are created on first use with no rules, as on GitHub.

use bee_proto::v1::{
    CheckStatus, Deployment, DeploymentEnvironment, DeploymentEnvironmentReviewer,
    DeploymentPendingReview, DeploymentRefPolicy, DeploymentRefPolicyMode, DeploymentRefPolicyType,
    DeploymentReview, DeploymentReviewDecision, DeploymentReviewerKind, DeploymentState,
    DeploymentStatus, Event, ObjectId, Repo, SubjectKind, SubjectRef, WorkflowConclusion,
    WorkflowStatus,
};
use bee_workflow::glob::Pattern;

use super::engine::{Ci, Effects, job_of, job_row, repo_ref_of, run_of, status_of};
use super::model::{ApprovalRecord, DeploymentRecord, EnvironmentRecord, JobRecord, RunRecord};
use super::{APPROVALS, BISECTS, DEPLOYMENTS, ENVIRONMENTS, JOB_INDEX, JOBS, RUNS, SURFACE};
use crate::audit::AuditDraft;
use crate::checks::{RunFilter, policy_status};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::store::{Key, Row, Scan, Tx};

/// Longest wait timer (30 days).
pub const MAX_WAIT_TIMER_SECONDS: u32 = 43_200 * 60;

/// The outcome of an environment gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Queue the job.
    Proceed,
    /// Wait for reviewers or the wait timer.
    Wait,
    /// The environment refuses the job.
    Reject(String),
}

/// `env_state` of a job held until its changeset is green.
pub(crate) const GREEN_WAIT: &str = "green_wait";

/// What `require_green` says about a workflow job's deployment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GreenGate {
    /// The changeset is green.
    Green,
    /// Wait and look again (post-land checks pending, or a bisect of the ref running).
    Hold(String),
    /// The changeset cannot become green as it is (a post-land check failed, or none ran).
    Refuse(String),
}

/// A new environment or an update.
#[derive(Clone, Debug, Default)]
pub struct EnvironmentSpec {
    /// Required reviewers (`None` = unchanged on update).
    pub reviewers: Option<Vec<DeploymentEnvironmentReviewer>>,
    /// The deployment's creator may not approve it.
    pub prevent_self_review: Option<bool>,
    /// Wait timer.
    pub wait_timer_seconds: Option<u32>,
    /// Ref policy mode.
    pub ref_policy_mode: Option<DeploymentRefPolicyMode>,
    /// Custom ref patterns (`None` = unchanged on update).
    pub ref_policies: Option<Vec<DeploymentRefPolicy>>,
    /// Admins may bypass the rules.
    pub can_admins_bypass: Option<bool>,
    /// Only green tips may deploy.
    pub require_green: Option<bool>,
}

pub(crate) fn env_key(name: &str) -> String {
    name.to_lowercase()
}

pub(crate) fn valid_env_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 255 || name.contains(['\n', '\r']) {
        return Err(ForgeError::invalid(
            "invalid_name",
            format!("bad environment name {name:?}"),
        ));
    }
    Ok(())
}

fn apply(env: &mut DeploymentEnvironment, s: &EnvironmentSpec) -> Result<()> {
    if let Some(r) = &s.reviewers {
        if r.len() > 6 {
            return Err(ForgeError::too_large(
                "environment_reviewers",
                6,
                r.len() as u64,
            ));
        }
        env.reviewers = r.clone();
    }
    if let Some(v) = s.prevent_self_review {
        env.prevent_self_review = v;
    }
    if let Some(v) = s.wait_timer_seconds {
        if v > MAX_WAIT_TIMER_SECONDS {
            return Err(ForgeError::invalid(
                "invalid_setting",
                "wait timer over 30 days",
            ));
        }
        env.wait_timer_seconds = v;
    }
    if let Some(v) = s.ref_policy_mode {
        env.ref_policy_mode = v as i32;
    }
    if let Some(v) = &s.ref_policies {
        env.ref_policies = v.clone();
    }
    if let Some(v) = s.can_admins_bypass {
        env.can_admins_bypass = v;
    }
    if let Some(v) = s.require_green {
        env.require_green = v;
    }
    Ok(())
}

/// Whether `git_ref` may deploy to `env` in a repo whose default branch is `default`.
pub fn ref_allowed(env: &DeploymentEnvironment, git_ref: &str, default: &str) -> bool {
    match DeploymentRefPolicyMode::try_from(env.ref_policy_mode)
        .unwrap_or(DeploymentRefPolicyMode::All)
    {
        DeploymentRefPolicyMode::ProtectedRefs => {
            let b = git_ref.strip_prefix("refs/heads/").unwrap_or("");
            !b.is_empty() && (b == default || b.starts_with("release/"))
        }
        DeploymentRefPolicyMode::Custom => env.ref_policies.iter().any(|p| {
            let (prefix, kind) = match DeploymentRefPolicyType::try_from(p.r#type)
                .unwrap_or(DeploymentRefPolicyType::Branch)
            {
                DeploymentRefPolicyType::Tag => ("refs/tags/", "tag"),
                _ => ("refs/heads/", "branch"),
            };
            let _ = kind;
            git_ref
                .strip_prefix(prefix)
                .is_some_and(|name| Pattern::new(&p.pattern).matches(name))
        }),
        _ => true,
    }
}

impl Ci {
    /// Creates an environment.
    pub async fn create_environment(
        &self,
        cx: &Cx,
        repo: &Repo,
        name: &str,
        spec: &EnvironmentSpec,
    ) -> Result<DeploymentEnvironment> {
        valid_env_name(name)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if tx
            .get(&ENVIRONMENTS, &repo.id, &env_key(name))
            .await?
            .is_some()
        {
            return Err(ForgeError::already_exists(format!("environment {name}")));
        }
        let now = tx.now_ms();
        let mut env = DeploymentEnvironment {
            id: tx.new_id()?,
            repo: repo_ref_of(&repo.id),
            name: name.to_string(),
            ref_policy_mode: DeploymentRefPolicyMode::All as i32,
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        };
        apply(&mut env, spec)?;
        tx.insert(
            &ENVIRONMENTS,
            &Row::new(repo.id.clone(), env_key(name)).msg(&EnvironmentRecord {
                environment: Some(env.clone()),
            }),
        )
        .await?;
        tx.audit(
            AuditDraft::new("environment.create", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Deployment, &env.id)
                .after(&serde_json::to_string(&env).unwrap_or_default()),
        )
        .await?;
        tx.commit().await?;
        Ok(env)
    }

    /// One environment.
    pub async fn get_environment(
        &self,
        cx: &Cx,
        repo: &Repo,
        name: &str,
    ) -> Result<DeploymentEnvironment> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<EnvironmentRecord>(&ENVIRONMENTS, &repo.id, &env_key(name))
            .await?
            .and_then(|(r, _)| r.environment)
            .ok_or_else(|| ForgeError::not_found(format!("environment {name}")))
    }

    /// A repo's environments.
    pub async fn list_environments(
        &self,
        cx: &Cx,
        repo: &Repo,
    ) -> Result<Vec<DeploymentEnvironment>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<EnvironmentRecord>(&ENVIRONMENTS, &Scan::scope(repo.id.clone(), 1000))
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.environment)
            .collect())
    }

    /// Updates an environment's rules.
    pub async fn update_environment(
        &self,
        cx: &Cx,
        repo: &Repo,
        name: &str,
        spec: &EnvironmentSpec,
    ) -> Result<DeploymentEnvironment> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<EnvironmentRecord>(&ENVIRONMENTS, &repo.id, &env_key(name))
            .await?
        else {
            return Err(ForgeError::not_found(format!("environment {name}")));
        };
        let mut env = rec.environment.clone().unwrap_or_default();
        apply(&mut env, spec)?;
        env.updated_at = Some(ts(tx.now_ms()));
        rec.environment = Some(env.clone());
        tx.update(
            &ENVIRONMENTS,
            &Row::new(repo.id.clone(), env_key(name))
                .version(row.version)
                .msg(&rec),
        )
        .await?;
        tx.audit(
            AuditDraft::new("environment.update", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Deployment, &env.id),
        )
        .await?;
        tx.commit().await?;
        Ok(env)
    }

    /// Deletes an environment with its secrets and variables.
    pub async fn delete_environment(&self, cx: &Cx, repo: &Repo, name: &str) -> Result<()> {
        let env = self.get_environment(cx, repo, name).await?;
        self.purge_scope(
            cx,
            &super::secrets::VaultScope::Environment(
                repo.id.clone(),
                repo.org_id.clone(),
                env.name.clone(),
            ),
        )
        .await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx.delete(&ENVIRONMENTS, &repo.id, &env_key(name)).await? {
            return Err(ForgeError::not_found(format!("environment {name}")));
        }
        tx.audit(
            AuditDraft::new("environment.delete", &repo.org_id)
                .repo_id(&repo.id)
                .target_id(name),
        )
        .await?;
        tx.commit().await
    }

    /// Deployments of a repo, newest first, optionally of one environment.
    pub async fn list_deployments(
        &self,
        cx: &Cx,
        repo: &Repo,
        env: &str,
    ) -> Result<Vec<Deployment>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut scan = Scan::scope(repo.id.clone(), 1000);
        if !env.is_empty() {
            scan = scan.key(Key::K1(env_key(env)));
        }
        let mut v: Vec<Deployment> = tx
            .scan_msgs::<DeploymentRecord>(&DEPLOYMENTS, &scan)
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.deployment)
            .collect();
        v.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(v)
    }

    /// A deployment's statuses, oldest first.
    pub async fn deployment_statuses(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
    ) -> Result<Vec<DeploymentStatus>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<DeploymentRecord>(&DEPLOYMENTS, &repo.id, id)
            .await?
            .map(|(r, _)| r.statuses)
            .ok_or_else(|| ForgeError::not_found(format!("deployment {id}")))
    }

    /// The environment gate of a job (inside the job's transaction).
    pub(crate) async fn gate(
        &self,
        tx: &mut Tx,
        repo_id: &str,
        rec: &RunRecord,
        j: &mut JobRecord,
        git_ref: &str,
        _fx: &mut Effects,
    ) -> Result<Gate> {
        let now = tx.now_ms();
        let name = j.environment.clone();
        let env = match tx
            .get_msg::<EnvironmentRecord>(&ENVIRONMENTS, repo_id, &env_key(&name))
            .await?
        {
            Some((r, _)) => r.environment.unwrap_or_default(),
            None => {
                let env = DeploymentEnvironment {
                    id: tx.new_id()?,
                    repo: repo_ref_of(repo_id),
                    name: name.clone(),
                    ref_policy_mode: DeploymentRefPolicyMode::All as i32,
                    created_at: Some(ts(now)),
                    updated_at: Some(ts(now)),
                    ..Default::default()
                };
                tx.insert(
                    &ENVIRONMENTS,
                    &Row::new(repo_id, env_key(&name)).msg(&EnvironmentRecord {
                        environment: Some(env.clone()),
                    }),
                )
                .await?;
                env
            }
        };
        let default = rec
            .repo_info
            .as_ref()
            .map(|r| r.default_branch.clone())
            .unwrap_or_default();
        if !ref_allowed(&env, git_ref, &default) {
            return Ok(Gate::Reject(format!(
                "{git_ref} is not allowed to deploy to {} due to environment protection rules.",
                env.name
            )));
        }
        if env.require_green {
            let run = run_of(rec);
            let post_land = rec.event.as_ref().is_some_and(|e| {
                e.name == "push"
                    && !default.is_empty()
                    && e.git_ref == super::convert::git_ref(&default)
            });
            match Box::pin(self.green_gate(
                tx,
                repo_id,
                &super::convert::bee_ref(git_ref),
                run.head_changeset.as_ref(),
                &run.id,
                post_land,
            ))
            .await?
            {
                GreenGate::Green => {}
                GreenGate::Hold(_) => {
                    j.env_state = GREEN_WAIT.into();
                    j.wait_until_ms = now + self.config.green_poll_ms.max(1);
                    return Ok(Gate::Wait);
                }
                GreenGate::Refuse(why) => {
                    return Ok(Gate::Reject(format!(
                        "{git_ref} is not allowed to deploy to {} due to environment protection rules: {why}",
                        env.name
                    )));
                }
            }
        }
        if j.deployment_id.is_empty() {
            let run = run_of(rec);
            let dep = Deployment {
                id: tx.new_id()?,
                repo: repo_ref_of(repo_id),
                r#ref: super::convert::bee_ref(git_ref),
                changeset: run.head_changeset.clone(),
                task: "deploy".into(),
                environment: env.name.clone(),
                original_environment: env.name.clone(),
                creator: run.triggering_actor.clone(),
                state: DeploymentState::Queued as i32,
                workflow_run_id: run.id.clone(),
                production_environment: env.name.eq_ignore_ascii_case("production"),
                created_at: Some(ts(now)),
                updated_at: Some(ts(now)),
                ..Default::default()
            };
            let pending = !env.reviewers.is_empty();
            let rules = enabled_rule_apps(&env);
            let waiting = pending || !rules.is_empty();
            let status = DeploymentStatus {
                id: tx.new_id()?,
                deployment_id: dep.id.clone(),
                state: if waiting {
                    DeploymentState::Pending
                } else {
                    DeploymentState::Queued
                } as i32,
                environment: env.name.clone(),
                creator: run.triggering_actor.clone(),
                created_at: Some(ts(now)),
                ..Default::default()
            };
            let mut d = dep.clone();
            d.state = status.state;
            let drec = DeploymentRecord {
                deployment: Some(d.clone()),
                statuses: vec![status],
                reviews: Vec::new(),
                job_id: job_of(j).id.clone(),
                pending,
                wait_started_ms: if !waiting && env.wait_timer_seconds > 0 {
                    now
                } else {
                    0
                },
                rule_pending: rules,
            };
            tx.insert(
                &DEPLOYMENTS,
                &Row::new(repo_id, dep.id.clone())
                    .k1(env_key(&env.name))
                    .k2(run.id.clone())
                    .n1(now as i64)
                    .msg(&drec),
            )
            .await?;
            for kind in ["deployment.created", "deployment_status.created"] {
                tx.emit_event(deployment_event(kind, repo_id, &j.org_id, &d))
                    .await?;
            }
            if pending {
                tx.emit_event(deployment_event(
                    "deployment_review.requested",
                    repo_id,
                    &j.org_id,
                    &d,
                ))
                .await?;
            }
            tx.audit(
                AuditDraft::new("deployment.create", &j.org_id)
                    .repo_id(repo_id)
                    .target(SubjectKind::Deployment, &d.id)
                    .after(
                        &serde_json::json!({ "environment": env.name, "ref": git_ref }).to_string(),
                    ),
            )
            .await?;
            j.deployment_id = d.id.clone();
            if waiting {
                j.env_state = "waiting".into();
                return Ok(Gate::Wait);
            }
            if env.wait_timer_seconds > 0 {
                j.env_state = "waiting".into();
                j.wait_until_ms = now + u64::from(env.wait_timer_seconds) * 1000;
                return Ok(Gate::Wait);
            }
        }
        j.env_state = "approved".into();
        Ok(Gate::Proceed)
    }

    /// What `require_green` says about a job of run `run_id` deploying `cs` from `ref_name`
    /// (inside the job's transaction). The latest `post_land` runs of `cs` reported by anything
    /// but the deploying run decide: a failing one refuses; a running bisect of the ref or a
    /// pending run holds the job. The deploying run's own jobs are left to `needs` (the job
    /// only starts once what it needs passed); when nothing else reports post-land checks, the
    /// changeset counts as tested only if the deploying run is itself its post-land run
    /// (`run_post_land`: a push of the default ref), else the job is refused.
    pub(crate) async fn green_gate(
        &self,
        tx: &mut Tx,
        repo_id: &str,
        ref_name: &str,
        cs: Option<&ObjectId>,
        run_id: &str,
        run_post_land: bool,
    ) -> Result<GreenGate> {
        let Some(cs) = cs.filter(|c| !c.raw.is_empty()) else {
            return Ok(GreenGate::Refuse(
                "require_green: the run has no changeset".into(),
            ));
        };
        let at = hex(&cs.raw);
        let cx = tx.cx().clone();
        let runs = self
            .checks
            .list(
                &cx,
                repo_id,
                &RunFilter {
                    changeset_hex: Some(at.clone()),
                    latest_only: true,
                    ..RunFilter::default()
                },
            )
            .await?;
        let (mut failing, mut pending, mut any) = (Vec::new(), Vec::new(), false);
        for r in runs.iter().filter(|r| r.post_land) {
            let own = !r.workflow_job_id.is_empty()
                && tx
                    .get_msg::<JobRecord>(&JOBS, repo_id, &r.workflow_job_id)
                    .await?
                    .is_some_and(|(jr, _)| job_of(&jr).run_id == run_id);
            if own {
                continue;
            }
            match policy_status(CheckStatus::try_from(r.status).unwrap_or_default()) {
                bee_policy::CheckStatus::Failure => failing.push(r.name.clone()),
                bee_policy::CheckStatus::Pending => pending.push(r.name.clone()),
                _ => any = true,
            }
        }
        failing.sort();
        failing.dedup();
        pending.sort();
        pending.dedup();
        if !failing.is_empty() {
            return Ok(GreenGate::Refuse(format!(
                "require_green: {at} failed post-land checks: {}",
                failing.join(", ")
            )));
        }
        let bisecting = tx
            .scan(
                &BISECTS,
                &Scan::scope(repo_id.to_string(), 100).key(Key::K1("running".into())),
            )
            .await?
            .iter()
            .any(|r| r.k2.as_deref() == Some(ref_name));
        if bisecting {
            return Ok(GreenGate::Hold(format!(
                "require_green: a post-land failure on {ref_name} is being bisected"
            )));
        }
        if !pending.is_empty() {
            return Ok(GreenGate::Hold(format!(
                "require_green: post-land checks of {at} are still running: {}",
                pending.join(", ")
            )));
        }
        if !any && !run_post_land {
            return Ok(GreenGate::Refuse(format!(
                "require_green: no post-land check passed on {at}; only tested changesets of the default ref are green"
            )));
        }
        Ok(GreenGate::Green)
    }

    /// Whether a waiting job's reviews are done and its wait timer elapsed.
    pub(crate) async fn wait_elapsed(
        &self,
        tx: &mut Tx,
        repo_id: &str,
        j: &mut JobRecord,
    ) -> Result<bool> {
        let Some((d, _)) = tx
            .get_msg::<DeploymentRecord>(&DEPLOYMENTS, repo_id, &j.deployment_id)
            .await?
        else {
            return Ok(true);
        };
        Ok(!d.pending
            && d.rule_pending.is_empty()
            && (j.wait_until_ms == 0 || j.wait_until_ms <= tx.now_ms()))
    }

    /// Appends a deployment status.
    pub(crate) async fn deployment_progress(
        &self,
        tx: &mut Tx,
        repo_id: &str,
        id: &str,
        state: DeploymentState,
        org_id: &str,
    ) -> Result<()> {
        self.deployment_progress_url(tx, repo_id, id, state, org_id, "")
            .await
    }

    /// Appends a deployment status carrying the environment URL (empty = none).
    pub(crate) async fn deployment_progress_url(
        &self,
        tx: &mut Tx,
        repo_id: &str,
        id: &str,
        state: DeploymentState,
        org_id: &str,
        environment_url: &str,
    ) -> Result<()> {
        let Some((mut d, row)) = tx
            .get_msg::<DeploymentRecord>(&DEPLOYMENTS, repo_id, id)
            .await?
        else {
            return Ok(());
        };
        let now = tx.now_ms();
        let mut dep = d.deployment.clone().unwrap_or_default();
        if dep.state == state as i32 {
            return Ok(());
        }
        dep.state = state as i32;
        dep.updated_at = Some(ts(now));
        d.statuses.push(DeploymentStatus {
            id: tx.new_id()?,
            deployment_id: id.to_string(),
            state: state as i32,
            environment: dep.environment.clone(),
            log_url: String::new(),
            environment_url: environment_url.to_string(),
            created_at: Some(ts(now)),
            ..Default::default()
        });
        d.deployment = Some(dep.clone());
        tx.update(
            &DEPLOYMENTS,
            &Row::new(repo_id, id)
                .k1(env_key(&dep.environment))
                .k2(dep.workflow_run_id.clone())
                .n1(row.n1.unwrap_or(0))
                .version(row.version)
                .msg(&d),
        )
        .await?;
        tx.emit_event(deployment_event(
            "deployment_status.created",
            repo_id,
            org_id,
            &dep,
        ))
        .await
    }

    /// Environments a run's jobs wait on, with whether `actor` may approve each.
    pub async fn pending_deployments(
        &self,
        cx: &Cx,
        repo: &Repo,
        run_id: &str,
        actor: &str,
        actor_is_admin: bool,
    ) -> Result<Vec<DeploymentPendingReview>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan_msgs::<DeploymentRecord>(
                &DEPLOYMENTS,
                &Scan::scope(repo.id.clone(), 1000).key(Key::K2(run_id.to_string())),
            )
            .await?;
        let mut out = Vec::new();
        for (d, _) in rows {
            let Some((j, _)) = tx
                .get_msg::<JobRecord>(&super::JOBS, &repo.id, &d.job_id)
                .await?
            else {
                continue;
            };
            if status_of(job_of(&j)) != WorkflowStatus::Waiting {
                continue;
            }
            let dep = d.deployment.clone().unwrap_or_default();
            let env = tx
                .get_msg::<EnvironmentRecord>(&ENVIRONMENTS, &repo.id, &env_key(&dep.environment))
                .await?
                .and_then(|(r, _)| r.environment)
                .unwrap_or_default();
            out.push(DeploymentPendingReview {
                deployment_id: dep.id.clone(),
                environment: dep.environment.clone(),
                workflow_run_id: run_id.to_string(),
                wait_timer_seconds: env.wait_timer_seconds,
                wait_timer_started_at: (d.wait_started_ms > 0).then(|| ts(d.wait_started_ms)),
                reviewers: env.reviewers.clone(),
                caller_can_approve: d.pending
                    && may_review(&env, &dep, actor, actor_is_admin).is_ok(),
                pending_rule_app_slugs: d.rule_pending.clone(),
            });
        }
        Ok(out)
    }

    /// Approves or rejects a run's pending deployments for `environments`.
    #[allow(clippy::too_many_arguments)]
    pub async fn review_deployments(
        &self,
        cx: &Cx,
        repo: &Repo,
        run_id: &str,
        environments: &[String],
        decision: DeploymentReviewDecision,
        comment: &str,
        actor_is_admin: bool,
    ) -> Result<Vec<Deployment>> {
        if decision == DeploymentReviewDecision::Unspecified {
            return Err(ForgeError::missing("decision"));
        }
        let mut fx = Effects::default();
        let mut out = Vec::new();
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((rec, _)) = tx.get_msg::<RunRecord>(&RUNS, &repo.id, run_id).await? else {
                return Err(ForgeError::not_found(format!("workflow run {run_id}")));
            };
            let rows = tx
                .scan_msgs::<DeploymentRecord>(
                    &DEPLOYMENTS,
                    &Scan::scope(repo.id.clone(), 1000).key(Key::K2(run_id.to_string())),
                )
                .await?;
            let now = tx.now_ms();
            let mut denied = None;
            let mut decided_envs = Vec::new();
            for (mut d, row) in rows {
                let dep = d.deployment.clone().unwrap_or_default();
                if !d.pending
                    || !environments
                        .iter()
                        .any(|e| e.eq_ignore_ascii_case(&dep.environment))
                {
                    continue;
                }
                let Some((mut j, jrow)) = tx
                    .get_msg::<JobRecord>(&super::JOBS, &repo.id, &d.job_id)
                    .await?
                else {
                    continue;
                };
                if status_of(job_of(&j)) != WorkflowStatus::Waiting {
                    continue;
                }
                let env = tx
                    .get_msg::<EnvironmentRecord>(
                        &ENVIRONMENTS,
                        &repo.id,
                        &env_key(&dep.environment),
                    )
                    .await?
                    .and_then(|(r, _)| r.environment)
                    .unwrap_or_default();
                let bypass = match may_review(&env, &dep, &cx.actor_id, actor_is_admin) {
                    Ok(b) => b,
                    Err(e) => {
                        denied = Some(e);
                        continue;
                    }
                };
                d.reviews.push(DeploymentReview {
                    deployment_id: dep.id.clone(),
                    environment: dep.environment.clone(),
                    decision: decision as i32,
                    reviewer: Some(cx.principal()),
                    comment: comment.to_string(),
                    bypassed: bypass,
                    created_at: Some(ts(now)),
                });
                d.pending = false;
                decided_envs.push(dep.environment.clone());
                tx.update(
                    &DEPLOYMENTS,
                    &Row::new(repo.id.clone(), dep.id.clone())
                        .k1(env_key(&dep.environment))
                        .k2(run_id)
                        .n1(row.n1.unwrap_or(0))
                        .version(row.version)
                        .msg(&d),
                )
                .await?;
                tx.emit_event(deployment_event(
                    "deployment_review.decided",
                    &repo.id,
                    &repo.org_id,
                    &dep,
                ))
                .await?;
                if decision == DeploymentReviewDecision::Approved {
                    if !d.rule_pending.is_empty() {
                        // Custom protection rules still have to answer; their last
                        // approval queues the job.
                    } else if env.wait_timer_seconds > 0 {
                        j.wait_until_ms = now + u64::from(env.wait_timer_seconds) * 1000;
                        if let Some((mut d2, row2)) = tx
                            .get_msg::<DeploymentRecord>(&DEPLOYMENTS, &repo.id, &dep.id)
                            .await?
                        {
                            d2.wait_started_ms = now;
                            tx.update(
                                &DEPLOYMENTS,
                                &Row::new(repo.id.clone(), dep.id.clone())
                                    .k1(env_key(&dep.environment))
                                    .k2(run_id)
                                    .n1(row2.n1.unwrap_or(0))
                                    .version(row2.version)
                                    .msg(&d2),
                            )
                            .await?;
                        }
                        tx.put(
                            &JOB_INDEX,
                            &Row::new("", job_of(&j).id.clone())
                                .n1(j.wait_until_ms as i64)
                                .msg(&super::model::JobIndex {
                                    repo_id: repo.id.clone(),
                                    org_id: repo.org_id.clone(),
                                    run_id: run_id.to_string(),
                                }),
                        )
                        .await?;
                    } else {
                        j.env_state = "approved".into();
                        self.deployment_progress(
                            &mut tx,
                            &repo.id,
                            &dep.id,
                            DeploymentState::Queued,
                            &repo.org_id,
                        )
                        .await?;
                        self.enqueue_job(&mut tx, &repo.id, &rec, &mut j, &mut fx)
                            .await?;
                    }
                } else {
                    j.env_state = "rejected".into();
                    j.error = format!(
                        "The deployment to {} was rejected by {}{}",
                        dep.environment,
                        cx.actor_id,
                        if comment.is_empty() {
                            String::new()
                        } else {
                            format!(": {comment}")
                        }
                    );
                    self.finish_job(
                        &mut tx,
                        &repo.id,
                        &mut j,
                        WorkflowConclusion::Failure,
                        "",
                        &mut fx,
                    )
                    .await?;
                }
                tx.update(&super::JOBS, &job_row(&repo.id, &j).version(jrow.version))
                    .await?;
                let fresh = tx
                    .get_msg::<DeploymentRecord>(&DEPLOYMENTS, &repo.id, &dep.id)
                    .await?
                    .and_then(|(r, _)| r.deployment)
                    .unwrap_or(dep);
                out.push(fresh);
            }
            if out.is_empty() {
                return Err(denied.unwrap_or_else(|| {
                    ForgeError::invalid_state("nothing is pending for those environments")
                }));
            }
            let id = tx.new_id()?;
            tx.insert(
                &APPROVALS,
                &Row::new(run_id, id).msg(&ApprovalRecord {
                    approval: Some(bee_proto::v1::WorkflowRunApproval {
                        kind: bee_proto::v1::WorkflowApprovalKind::Environment as i32,
                        approver: Some(cx.principal()),
                        decision: decision as i32,
                        environments: decided_envs.clone(),
                        comment: comment.to_string(),
                        created_at: Some(ts(now)),
                    }),
                }),
            )
            .await?;
            tx.audit(
                AuditDraft::new("deployment.review", &repo.org_id)
                    .repo_id(&repo.id)
                    .target(SubjectKind::WorkflowRun, run_id)
                    .after(
                        &serde_json::json!({ "environments": decided_envs, "decision": decision.as_str_name() })
                            .to_string(),
                    ),
            )
            .await?;
            let _ = run_of(&rec);
            tx.commit().await?;
        }
        fx.advance.insert((repo.id.clone(), run_id.to_string()));
        self.apply(cx, fx).await;
        Ok(out)
    }

    /// Approvals and environment reviews recorded on a run.
    pub async fn run_approvals(
        &self,
        cx: &Cx,
        run_id: &str,
    ) -> Result<Vec<bee_proto::v1::WorkflowRunApproval>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<ApprovalRecord>(&APPROVALS, &Scan::scope(run_id, 1000))
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.approval)
            .collect())
    }
}

/// Whether `actor` may review a deployment; `Ok(true)` when only as an admin bypass.
pub(crate) fn may_review(
    env: &DeploymentEnvironment,
    dep: &Deployment,
    actor: &str,
    admin: bool,
) -> Result<bool> {
    let listed = env.reviewers.iter().any(|r| {
        r.kind == DeploymentReviewerKind::Principal as i32
            && r.principal.as_ref().is_some_and(|p| p.id == actor)
    });
    let creator = dep.creator.as_ref().map(|c| c.id.as_str()).unwrap_or("");
    if listed {
        if env.prevent_self_review && creator == actor {
            return Err(ForgeError::permission_denied(format!(
                "{actor} started this deployment and the environment prevents self-review"
            )));
        }
        return Ok(false);
    }
    if admin && env.can_admins_bypass {
        return Ok(true);
    }
    Err(ForgeError::permission_denied(format!(
        "{actor} is not a required reviewer of {}",
        env.name
    )))
}

/// App ids of an environment's enabled custom protection rules.
pub(crate) fn enabled_rule_apps(env: &DeploymentEnvironment) -> Vec<String> {
    env.protection_rules
        .iter()
        .filter(|r| r.enabled)
        .map(|r| r.app_id.clone())
        .collect()
}

pub(crate) fn deployment_event(kind: &str, repo_id: &str, org_id: &str, d: &Deployment) -> Event {
    Event {
        kind: kind.into(),
        repo_id: repo_id.into(),
        org_id: org_id.into(),
        subject: Some(SubjectRef {
            kind: SubjectKind::Deployment as i32,
            repo: repo_ref_of(repo_id),
            id: d.id.clone(),
        }),
        payload_json: serde_json::to_string(d).unwrap_or_default(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_policies() {
        let mut env = DeploymentEnvironment {
            ref_policy_mode: DeploymentRefPolicyMode::ProtectedRefs as i32,
            ..Default::default()
        };
        assert!(ref_allowed(&env, "refs/heads/main", "main"));
        assert!(ref_allowed(&env, "refs/heads/release/1.0", "main"));
        assert!(!ref_allowed(&env, "refs/heads/feature", "main"));
        env.ref_policy_mode = DeploymentRefPolicyMode::Custom as i32;
        env.ref_policies = vec![DeploymentRefPolicy {
            pattern: "v*".into(),
            r#type: DeploymentRefPolicyType::Tag as i32,
        }];
        assert!(ref_allowed(&env, "refs/tags/v1.2", "main"));
        assert!(!ref_allowed(&env, "refs/heads/v1", "main"));
    }
}
