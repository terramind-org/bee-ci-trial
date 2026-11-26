//! The tonic `WorkflowService` over [`Ci`], following the forge service shape
//! (`docs/spec/forge-core.md` §7). `SecretService` and `DeploymentService` are in
//! [`super::vault_service`] and [`super::deploy_service`].
//!
//! | RPCs | authorization |
//! |---|---|
//! | reads (workflows, runs, jobs, logs, artifacts, caches, usage, approvals, attestations) | `read` |
//! | dispatch, repository dispatch, enable/disable, cancel, re-run, approve, delete runs/logs/artifacts/caches | `commit` (write role) |
//! | review pending deployments | `read`, plus being a required reviewer (or an admin when the environment allows bypass) |
//! | artifact upload/finalize, cache lookup/reserve/commit, OIDC tokens, attestation creation | the job's own token (principal `job:<id>`) while the job runs |
//! | repo actions policy and OIDC template writes, attestation deletion | `manage_settings`, admin |
//! | org policies, enabled repos, required workflows, org OIDC template | org owner/admin (reads: org members) |

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bee_policy::Action;
use bee_proto::v1::workflow_service_server::{WorkflowService, WorkflowServiceServer};
use bee_proto::v1::*;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::engine::{Ci, Rerun, job_of};
use super::policy::PolicyScope;
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::{ts, ts_ms};
use crate::service::events::reauthorize_every;
use crate::service::{ForgeServices, ResponseStream, next, page, reply};

/// `WorkflowService` and `RunnerService` over a forge and its CI module.
#[derive(Clone)]
pub struct CiServices {
    pub(crate) base: ForgeServices,
    pub(crate) ci: Arc<Ci>,
}

/// One page of `items` for a page request (offset tokens).
pub(crate) fn paged<T>(items: Vec<T>, p: &Option<PageRequest>) -> (Vec<T>, Option<PageResponse>) {
    let (tok, size) = page(p);
    let start: usize = tok.parse().unwrap_or(0);
    let len = items.len();
    let end = (start + size).min(len);
    let out: Vec<T> = items
        .into_iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    let token = if end < len {
        end.to_string()
    } else {
        String::new()
    };
    (out, next(token))
}

impl CiServices {
    /// The services over `forge` and `ci`.
    pub fn new(forge: Arc<Forge>, ci: Arc<Ci>) -> CiServices {
        CiServices {
            base: ForgeServices::new(forge),
            ci,
        }
    }

    /// The CI module.
    pub fn ci(&self) -> &Arc<Ci> {
        &self.ci
    }

    /// The `WorkflowService` server.
    pub fn workflow_service(&self) -> WorkflowServiceServer<CiServices> {
        WorkflowServiceServer::new(self.clone())
    }

    /// The `RunnerService` server.
    pub fn runner_service(
        &self,
    ) -> bee_proto::v1::runner_service_server::RunnerServiceServer<CiServices> {
        bee_proto::v1::runner_service_server::RunnerServiceServer::new(self.clone())
    }

    /// The `SecretService` server ([`super::vault_service`]).
    pub fn secret_service(
        &self,
    ) -> bee_proto::v1::secret_service_server::SecretServiceServer<CiServices> {
        bee_proto::v1::secret_service_server::SecretServiceServer::new(self.clone())
    }

    /// The `DeploymentService` server ([`super::deploy_service`]).
    pub fn deployment_service(
        &self,
    ) -> bee_proto::v1::deployment_service_server::DeploymentServiceServer<CiServices> {
        bee_proto::v1::deployment_service_server::DeploymentServiceServer::new(self.clone())
    }

    /// Adds `WorkflowService`, `RunnerService`, `SecretService` and `DeploymentService` to a
    /// tonic router builder.
    pub fn register(&self, routes: &mut tonic::service::RoutesBuilder) {
        routes.add_service(self.workflow_service());
        routes.add_service(self.runner_service());
        routes.add_service(self.secret_service());
        routes.add_service(self.deployment_service());
    }

    pub(crate) fn forge(&self) -> &Arc<Forge> {
        self.base.forge()
    }

    pub(crate) async fn read(&self, caller: &Caller, r: &Option<RepoRef>) -> Result<(Repo, Cx)> {
        let repo = self.base.repo(caller, r).await?;
        let cx = self
            .forge()
            .authz
            .repo(caller, &repo, Action::Read, &[], None, None)
            .await?;
        Ok((repo, cx))
    }

    pub(crate) async fn write(
        &self,
        caller: &Caller,
        r: &Option<RepoRef>,
        audit: &str,
    ) -> Result<(Repo, Cx)> {
        let repo = self.base.repo(caller, r).await?;
        let auth = self
            .forge()
            .authz
            .repo(caller, &repo, Action::Commit, &[], None, None)
            .await;
        let cx = self
            .base
            .guarded(caller, &repo.org_id, &repo.id, audit, auth)
            .await?;
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        Ok((repo, cx))
    }

    pub(crate) async fn admin(&self, caller: &Caller, repo: &Repo, audit: &str) -> Result<Cx> {
        let auth = self
            .forge()
            .authz
            .repo(
                caller,
                repo,
                Action::ManageSettings,
                &[],
                None,
                Some(RepoRole::Admin),
            )
            .await;
        self.base
            .guarded(caller, &repo.org_id, &repo.id, audit, auth)
            .await
    }

    pub(crate) async fn org(
        &self,
        caller: &Caller,
        org: &str,
        perm: OrgPerm,
        audit: &str,
    ) -> Result<(String, Cx)> {
        if org.is_empty() {
            return Err(ForgeError::missing("org"));
        }
        let org_id = self.base.org_id(caller, org).await?;
        let auth = self.forge().authz.org(caller, &org_id, perm).await;
        let cx = if perm == OrgPerm::ViewMembers {
            auth?
        } else {
            self.base.guarded(caller, &org_id, "", audit, auth).await?
        };
        Ok((org_id, cx))
    }

    /// The calling job: the caller must be `job:<job_id>` and the job must run in the repo.
    /// The job id of a job-token caller. Requests that name no job (finalizing an artifact,
    /// committing a cache) take it from the token, so any other caller is refused as
    /// PERMISSION_DENIED rather than told that a field it cannot send is missing.
    fn caller_job(caller: &Caller) -> Result<String> {
        caller
            .cx
            .actor_id
            .strip_prefix("job:")
            .filter(|j| !j.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| ForgeError::permission_denied("only the job's own token may call this"))
    }

    pub(crate) async fn job_caller(
        &self,
        caller: &Caller,
        r: &Option<RepoRef>,
        job_id: &str,
    ) -> Result<(Repo, Cx)> {
        if job_id.is_empty() {
            return Err(ForgeError::missing("job_id"));
        }
        if caller.cx.actor_id != format!("job:{job_id}") {
            return Err(ForgeError::permission_denied(
                "only the job's own token may call this",
            ));
        }
        let (repo, cx) = self.read(caller, r).await?;
        let j = self.ci.job_record(&cx, &repo.id, job_id).await?;
        if job_of(&j).status != WorkflowStatus::InProgress as i32 {
            return Err(ForgeError::invalid_state("the job is not running"));
        }
        Ok((repo, cx))
    }

    pub(crate) async fn policy_scope(
        &self,
        caller: &Caller,
        s: &Option<ActionsPolicyScope>,
        write: bool,
        audit: &str,
    ) -> Result<(PolicyScope, Cx)> {
        let s = s.clone().ok_or_else(|| ForgeError::missing("scope"))?;
        match ActionsPolicyScopeKind::try_from(s.kind)
            .unwrap_or(ActionsPolicyScopeKind::Unspecified)
        {
            ActionsPolicyScopeKind::Repo => {
                let repo = self.base.repo(caller, &s.repo).await?;
                let cx = if write {
                    self.admin(caller, &repo, audit).await?
                } else {
                    self.forge()
                        .authz
                        .repo(caller, &repo, Action::Read, &[], None, None)
                        .await?
                };
                Ok((PolicyScope::Repo(Box::new(repo)), cx))
            }
            ActionsPolicyScopeKind::Org => {
                let perm = if write {
                    OrgPerm::Manage
                } else {
                    OrgPerm::ViewMembers
                };
                let (org_id, cx) = self.org(caller, &s.org, perm, audit).await?;
                Ok((PolicyScope::Org(org_id), cx))
            }
            ActionsPolicyScopeKind::Unspecified => Err(ForgeError::missing("scope.kind")),
        }
    }

    pub(crate) async fn is_admin(&self, caller: &Caller, repo: &Repo) -> bool {
        self.forge()
            .authz
            .repo(
                caller,
                repo,
                Action::ManageSettings,
                &[],
                None,
                Some(RepoRole::Admin),
            )
            .await
            .is_ok()
    }

    pub(crate) async fn repo_ids(&self, caller: &Caller, refs: &[RepoRef]) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for r in refs {
            out.push(self.base.repo(caller, &Some(r.clone())).await?.id);
        }
        Ok(out)
    }

    async fn workflow_id(&self, cx: &Cx, repo: &Repo, name: &str) -> Result<String> {
        match self.ci.find_workflow(cx, repo, name).await {
            Ok(w) => Ok(w.workflow.unwrap_or_default().id),
            Err(_) => {
                self.ci.list_workflows(cx, repo, "").await?;
                Ok(self
                    .ci
                    .find_workflow(cx, repo, name)
                    .await?
                    .workflow
                    .unwrap_or_default()
                    .id)
            }
        }
    }

    async fn run_with_jobs(
        &self,
        cx: &Cx,
        repo: &Repo,
        mut run: WorkflowRun,
        include: bool,
    ) -> Result<WorkflowRun> {
        if include {
            run.jobs = self.ci.list_jobs(cx, &repo.id, &run.id, 0, false).await?;
        }
        Ok(run)
    }

    async fn job_repo_from_id(&self, cx: &Cx, repo: &Repo, job_id: &str) -> Result<WorkflowJob> {
        Ok(job_of(&self.ci.job_record(cx, &repo.id, job_id).await?).clone())
    }
}

fn diag(
    d: &bee_workflow::Diagnostic,
    rule: &str,
    sev: WorkflowDiagnosticSeverity,
) -> WorkflowDiagnostic {
    WorkflowDiagnostic {
        severity: sev as i32,
        line: d.pos.line,
        column: d.pos.col,
        message: d.message.clone(),
        rule: rule.to_string(),
    }
}

#[tonic::async_trait]
impl WorkflowService for CiServices {
    async fn list_workflows(
        &self,
        req: Request<ListWorkflowsRequest>,
    ) -> std::result::Result<Response<ListWorkflowsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let all = self.ci.list_workflows(&cx, &repo, &r.r#ref).await?;
            let (workflows, page) = paged(all, &r.page);
            Ok(ListWorkflowsResponse { workflows, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow(
        &self,
        req: Request<GetWorkflowRequest>,
    ) -> std::result::Result<Response<Workflow>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let id = self.workflow_id(&cx, &repo, &r.workflow).await?;
            let mut w = self
                .ci
                .find_workflow(&cx, &repo, &id)
                .await?
                .workflow
                .unwrap_or_default();
            if r.include_source {
                let name = super::convert::bee_ref(if r.r#ref.is_empty() {
                    if repo.default_ref.is_empty() {
                        "main"
                    } else {
                        &repo.default_ref
                    }
                } else {
                    &r.r#ref
                });
                if let Some(tip) = self.ci.vcs.ref_tip(&repo, &name).await?
                    && let Some(b) = self.ci.vcs.read_file(&repo, &tip, &w.path).await?
                {
                    w.source = String::from_utf8_lossy(&b).into_owned();
                }
            }
            Ok(w)
        }
        .await;
        reply(&caller, out)
    }

    async fn validate_workflow(
        &self,
        req: Request<ValidateWorkflowRequest>,
    ) -> std::result::Result<Response<ValidateWorkflowResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let path = if r.path.is_empty() {
                ".github/workflows/workflow.yml".to_string()
            } else {
                r.path.clone()
            };
            let mut diagnostics = Vec::new();
            if !super::engine::is_ci_workflow_path(&path) {
                diagnostics.push(WorkflowDiagnostic {
                    severity: WorkflowDiagnosticSeverity::Warning as i32,
                    message: format!("{path} is not in a workflow directory; it would never run"),
                    rule: "not_a_workflow_path".into(),
                    ..Default::default()
                });
            }
            match bee_workflow::parse_workflow(&path, &r.source) {
                Err(e) => {
                    for d in &e.diagnostics {
                        diagnostics.push(diag(
                            d,
                            "invalid_workflow",
                            WorkflowDiagnosticSeverity::Error,
                        ));
                    }
                    Ok(ValidateWorkflowResponse {
                        valid: false,
                        diagnostics,
                        triggers: Vec::new(),
                    })
                }
                Ok(wf) => {
                    let policy = self.ci.effective_policy(&cx, &repo).await?;
                    let mut valid = true;
                    for j in &wf.jobs {
                        if let bee_workflow::model::JobKind::Steps(s) = &j.kind {
                            for st in &s.steps {
                                if let bee_workflow::model::StepAction::Uses { action, .. } =
                                    &st.action
                                    && let Err(m) =
                                        self.ci.check_uses(&policy, &repo, &[action.render()])
                                {
                                    valid = false;
                                    diagnostics.push(WorkflowDiagnostic {
                                        severity: WorkflowDiagnosticSeverity::Error as i32,
                                        line: st.pos.line,
                                        column: st.pos.col,
                                        message: m,
                                        rule: "disallowed_action".into(),
                                    });
                                }
                            }
                        }
                    }
                    Ok(ValidateWorkflowResponse {
                        valid,
                        diagnostics,
                        triggers: wf.on.iter().map(|t| t.name.clone()).collect(),
                    })
                }
            }
        }
        .await;
        reply(&caller, out)
    }

    async fn enable_workflow(
        &self,
        req: Request<EnableWorkflowRequest>,
    ) -> std::result::Result<Response<Workflow>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "workflow.enable").await?;
            let id = self.workflow_id(&cx, &repo, &r.workflow).await?;
            self.ci.set_workflow_state(&cx, &repo, &id, true).await
        }
        .await;
        reply(&caller, out)
    }

    async fn disable_workflow(
        &self,
        req: Request<DisableWorkflowRequest>,
    ) -> std::result::Result<Response<Workflow>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "workflow.disable").await?;
            let id = self.workflow_id(&cx, &repo, &r.workflow).await?;
            self.ci.set_workflow_state(&cx, &repo, &id, false).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_usage(
        &self,
        req: Request<GetWorkflowUsageRequest>,
    ) -> std::result::Result<Response<WorkflowUsage>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let id = self.workflow_id(&cx, &repo, &r.workflow).await?;
            self.ci.workflow_usage(&cx, &repo.id, &id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn dispatch_workflow(
        &self,
        req: Request<DispatchWorkflowRequest>,
    ) -> std::result::Result<Response<DispatchWorkflowResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "workflow.dispatch").await?;
            if r.workflow.is_empty() {
                return Err(ForgeError::missing("workflow"));
            }
            let inputs: BTreeMap<String, String> =
                r.inputs.into_iter().map(|kv| (kv.key, kv.value)).collect();
            let (run_id, run_number) = self
                .ci
                .dispatch(&cx, &repo, &r.workflow, &r.r#ref, inputs)
                .await?;
            Ok(DispatchWorkflowResponse { run_id, run_number })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_repository_dispatch_event(
        &self,
        req: Request<CreateRepositoryDispatchEventRequest>,
    ) -> std::result::Result<Response<CreateRepositoryDispatchEventResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .write(&caller, &r.repo, "workflow.repository_dispatch")
                .await?;
            let run_ids = self
                .ci
                .repository_dispatch(&cx, &repo, &r.event_type, &r.client_payload_json)
                .await?;
            Ok(CreateRepositoryDispatchEventResponse { run_ids })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_workflow_runs(
        &self,
        req: Request<ListWorkflowRunsRequest>,
    ) -> std::result::Result<Response<ListWorkflowRunsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let wid = if r.workflow.is_empty() {
                String::new()
            } else {
                self.workflow_id(&cx, &repo, &r.workflow).await?
            };
            let after = r.created_after.as_ref().map(ts_ms);
            let before = r.created_before.as_ref().map(ts_ms);
            let head = r.head_changeset.as_ref().map(|h| crate::ids::hex(&h.raw));
            let mut runs: Vec<WorkflowRun> = self
                .ci
                .list_runs(&cx, &repo.id, &wid)
                .await?
                .into_iter()
                .filter(|x| r.r#ref.is_empty() || x.head_ref == super::convert::bee_ref(&r.r#ref))
                .filter(|x| r.event.is_empty() || x.event == r.event)
                .filter(|x| r.status == 0 || x.status == r.status)
                .filter(|x| r.conclusion == 0 || x.conclusion == r.conclusion)
                .filter(|x| {
                    r.actor_id.is_empty() || x.actor.as_ref().is_some_and(|a| a.id == r.actor_id)
                })
                .filter(|x| {
                    head.as_ref().is_none_or(|h| {
                        x.head_changeset
                            .as_ref()
                            .is_some_and(|c| crate::ids::hex(&c.raw) == *h)
                    })
                })
                .filter(|x| {
                    r.change_request_number == 0
                        || x.change_request_numbers.contains(&r.change_request_number)
                })
                .filter(|x| !r.exclude_change_requests || x.change_request_numbers.is_empty())
                .filter(|x| {
                    after.is_none_or(|a| x.created_at.as_ref().map(ts_ms).unwrap_or(0) >= a)
                })
                .filter(|x| {
                    before.is_none_or(|b| x.created_at.as_ref().map(ts_ms).unwrap_or(0) < b)
                })
                .collect();
            runs.sort_by(|a, b| b.id.cmp(&a.id));
            let total = runs.len() as u64;
            let (page_runs, page) = paged(runs, &r.page);
            let mut out_runs = Vec::new();
            for x in page_runs {
                out_runs.push(self.run_with_jobs(&cx, &repo, x, r.include_jobs).await?);
            }
            Ok(ListWorkflowRunsResponse {
                runs: out_runs,
                page,
                total_count: total,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_run(
        &self,
        req: Request<GetWorkflowRunRequest>,
    ) -> std::result::Result<Response<WorkflowRun>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            self.ci.get_run(&cx, &repo.id, &r.run_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_run_attempt(
        &self,
        req: Request<GetWorkflowRunAttemptRequest>,
    ) -> std::result::Result<Response<WorkflowRun>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let mut run = self
                .ci
                .run_attempt(&cx, &repo.id, &r.run_id, r.attempt)
                .await?;
            run.jobs = self
                .ci
                .list_jobs(&cx, &repo.id, &r.run_id, run.run_attempt, false)
                .await?;
            Ok(run)
        }
        .await;
        reply(&caller, out)
    }

    type WatchWorkflowRunStream = ResponseStream<WorkflowRunUpdate>;

    async fn watch_workflow_run(
        &self,
        req: Request<WatchWorkflowRunRequest>,
    ) -> std::result::Result<Response<Self::WatchWorkflowRunStream>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            self.ci.get_run(&cx, &repo.id, &r.run_id).await?;
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<WorkflowRunUpdate>>(16);
            let ci = self.ci.clone();
            let base = self.clone();
            let auth_caller = caller.clone();
            let repo_ref = r.repo.clone();
            let run_id = r.run_id.clone();
            tokio::spawn(async move {
                let mut last_jobs: BTreeMap<String, WorkflowJob> = BTreeMap::new();
                let mut last_run: Option<WorkflowRun> = None;
                let mut checked = tokio::time::Instant::now();
                loop {
                    if let Err(e) = reauthorize_every(&mut checked, || async {
                        let fresh = base.base.fresh(&auth_caller)?;
                        base.read(&fresh, &repo_ref).await.map(|_| ())
                    })
                    .await
                    {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                    let cx = cx.clone().with_timeout(Duration::from_secs(10));
                    let wake = ci.wake_signal().clone();
                    let notified = wake.notified();
                    let run = match ci.get_run(&cx, &repo.id, &run_id).await {
                        Ok(x) => x,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    };
                    let jobs = ci
                        .list_jobs(&cx, &repo.id, &run_id, 0, false)
                        .await
                        .unwrap_or_default();
                    let changed: Vec<WorkflowJob> = jobs
                        .iter()
                        .filter(|j| last_jobs.get(&j.id) != Some(j))
                        .cloned()
                        .collect();
                    let done = run.status == WorkflowStatus::Completed as i32;
                    if last_run.as_ref() != Some(&run) || !changed.is_empty() {
                        for j in &changed {
                            last_jobs.insert(j.id.clone(), j.clone());
                        }
                        last_run = Some(run.clone());
                        if tx
                            .send(Ok(WorkflowRunUpdate {
                                run: Some(run),
                                changed_jobs: changed,
                                r#final: done,
                            }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    if done {
                        return;
                    }
                    tokio::select! {
                        _ = notified => {}
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    }
                }
            });
            let rid = caller.cx.request_id.clone();
            let stream =
                ReceiverStream::new(rx).map(move |item| item.map_err(|e| e.to_status(&rid)));
            Ok(Box::pin(stream) as ResponseStream<WorkflowRunUpdate>)
        }
        .await;
        reply(&caller, out)
    }

    async fn cancel_workflow_run(
        &self,
        req: Request<CancelWorkflowRunRequest>,
    ) -> std::result::Result<Response<CancelWorkflowRunResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "workflow.run_cancel").await?;
            self.ci.cancel_run(&cx, &repo.id, &r.run_id, false).await?;
            Ok(CancelWorkflowRunResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn force_cancel_workflow_run(
        &self,
        req: Request<ForceCancelWorkflowRunRequest>,
    ) -> std::result::Result<Response<ForceCancelWorkflowRunResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .write(&caller, &r.repo, "workflow.run_force_cancel")
                .await?;
            self.ci.cancel_run(&cx, &repo.id, &r.run_id, true).await?;
            Ok(ForceCancelWorkflowRunResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn rerun_workflow_run(
        &self,
        req: Request<RerunWorkflowRunRequest>,
    ) -> std::result::Result<Response<WorkflowRun>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "workflow.run_rerun").await?;
            self.ci
                .rerun(&cx, &repo.id, &r.run_id, Rerun::All, r.enable_debug_logging)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn rerun_failed_workflow_jobs(
        &self,
        req: Request<RerunFailedWorkflowJobsRequest>,
    ) -> std::result::Result<Response<WorkflowRun>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "workflow.run_rerun").await?;
            self.ci
                .rerun(
                    &cx,
                    &repo.id,
                    &r.run_id,
                    Rerun::Failed,
                    r.enable_debug_logging,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn rerun_workflow_job(
        &self,
        req: Request<RerunWorkflowJobRequest>,
    ) -> std::result::Result<Response<WorkflowRun>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "workflow.run_rerun").await?;
            let job = self.job_repo_from_id(&cx, &repo, &r.job_id).await?;
            self.ci
                .rerun(
                    &cx,
                    &repo.id,
                    &job.run_id,
                    Rerun::Job(r.job_id.clone()),
                    r.enable_debug_logging,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_workflow_run(
        &self,
        req: Request<DeleteWorkflowRunRequest>,
    ) -> std::result::Result<Response<DeleteWorkflowRunResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "workflow.run_delete").await?;
            self.ci.delete_run(&cx, &repo, &r.run_id).await?;
            Ok(DeleteWorkflowRunResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn approve_workflow_run(
        &self,
        req: Request<ApproveWorkflowRunRequest>,
    ) -> std::result::Result<Response<WorkflowRun>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "workflow.run_approve").await?;
            self.ci.approve_run(&cx, &repo.id, &r.run_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_workflow_run_approvals(
        &self,
        req: Request<ListWorkflowRunApprovalsRequest>,
    ) -> std::result::Result<Response<ListWorkflowRunApprovalsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            self.ci.get_run(&cx, &repo.id, &r.run_id).await?;
            let (approvals, page) = paged(self.ci.run_approvals(&cx, &r.run_id).await?, &r.page);
            Ok(ListWorkflowRunApprovalsResponse { approvals, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_workflow_run_pending_deployments(
        &self,
        req: Request<ListWorkflowRunPendingDeploymentsRequest>,
    ) -> std::result::Result<Response<ListWorkflowRunPendingDeploymentsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let admin = self.is_admin(&caller, &repo).await;
            let pending = self
                .ci
                .pending_deployments(&cx, &repo, &r.run_id, &caller.cx.actor_id, admin)
                .await?;
            Ok(ListWorkflowRunPendingDeploymentsResponse { pending })
        }
        .await;
        reply(&caller, out)
    }

    async fn review_workflow_run_pending_deployments(
        &self,
        req: Request<ReviewWorkflowRunPendingDeploymentsRequest>,
    ) -> std::result::Result<Response<ReviewWorkflowRunPendingDeploymentsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let admin = self.is_admin(&caller, &repo).await;
            let decision = DeploymentReviewDecision::try_from(r.decision)
                .unwrap_or(DeploymentReviewDecision::Unspecified);
            let res = self
                .ci
                .review_deployments(
                    &cx,
                    &repo,
                    &r.run_id,
                    &r.environments,
                    decision,
                    &r.comment,
                    admin,
                )
                .await;
            if let Err(e) = &res {
                self.forge()
                    .authz
                    .audit_denial(&caller, &repo.org_id, &repo.id, "deployment.review", e)
                    .await;
            }
            Ok(ReviewWorkflowRunPendingDeploymentsResponse { deployments: res? })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_run_usage(
        &self,
        req: Request<GetWorkflowRunUsageRequest>,
    ) -> std::result::Result<Response<WorkflowRunUsage>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            self.ci.run_usage(&cx, &repo.id, &r.run_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_workflow_jobs(
        &self,
        req: Request<ListWorkflowJobsRequest>,
    ) -> std::result::Result<Response<ListWorkflowJobsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let all = r.attempt == 0 && r.filter == WorkflowJobAttemptFilter::All as i32;
            let jobs = self
                .ci
                .list_jobs(&cx, &repo.id, &r.run_id, r.attempt, all)
                .await?;
            let (jobs, page) = paged(jobs, &r.page);
            Ok(ListWorkflowJobsResponse { jobs, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_job(
        &self,
        req: Request<GetWorkflowJobRequest>,
    ) -> std::result::Result<Response<WorkflowJob>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            self.job_repo_from_id(&cx, &repo, &r.job_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_job_summary(
        &self,
        req: Request<GetWorkflowJobSummaryRequest>,
    ) -> std::result::Result<Response<WorkflowJobSummary>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let j = self.ci.job_record(&cx, &repo.id, &r.job_id).await?;
            Ok(WorkflowJobSummary {
                job_id: r.job_id.clone(),
                truncated: j.summary.len() >= 1024 * 1024,
                markdown: j.summary,
            })
        }
        .await;
        reply(&caller, out)
    }

    type TailWorkflowJobLogsStream = ResponseStream<WorkflowLogChunk>;

    async fn tail_workflow_job_logs(
        &self,
        req: Request<TailWorkflowJobLogsRequest>,
    ) -> std::result::Result<Response<Self::TailWorkflowJobLogsStream>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let first = self
                .ci
                .read_log(&cx, &repo.id, &r.job_id, r.offset, r.step_number)
                .await?;
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<WorkflowLogChunk>>(64);
            let ci = self.ci.clone();
            let base = self.clone();
            let auth_caller = caller.clone();
            let repo_ref = r.repo.clone();
            let job_id = r.job_id.clone();
            let follow = r.follow;
            let step = r.step_number;
            let mut offset = r.offset;
            tokio::spawn(async move {
                let mut batch = first;
                let mut checked = tokio::time::Instant::now();
                loop {
                    if let Err(e) = reauthorize_every(&mut checked, || async {
                        let fresh = base.base.fresh(&auth_caller)?;
                        base.read(&fresh, &repo_ref).await.map(|_| ())
                    })
                    .await
                    {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                    let mut eof = false;
                    for c in batch {
                        offset = offset.max(c.offset + c.data.len() as u64);
                        eof |= c.eof;
                        if tx.send(Ok(c)).await.is_err() {
                            return;
                        }
                    }
                    if eof || !follow {
                        return;
                    }
                    let wake = ci.wake_signal().clone();
                    tokio::select! {
                        _ = wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    }
                    let cx = cx.clone().with_timeout(Duration::from_secs(10));
                    batch = match ci.read_log(&cx, &repo.id, &job_id, offset, step).await {
                        Ok(b) => b
                            .into_iter()
                            .filter(|c| !c.data.is_empty() || c.eof)
                            .collect(),
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    };
                }
            });
            let rid = caller.cx.request_id.clone();
            let stream =
                ReceiverStream::new(rx).map(move |item| item.map_err(|e| e.to_status(&rid)));
            Ok(Box::pin(stream) as ResponseStream<WorkflowLogChunk>)
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_job_logs_url(
        &self,
        req: Request<GetWorkflowJobLogsUrlRequest>,
    ) -> std::result::Result<Response<WorkflowLogsUrl>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let j = self.ci.job_record(&cx, &repo.id, &r.job_id).await?;
            if j.logs_deleted {
                return Err(ForgeError::new(
                    ErrorReason::Redacted,
                    "the job's logs were deleted",
                ));
            }
            let (url, exp) = self.ci.job_logs_url(&repo.id, &r.job_id);
            Ok(WorkflowLogsUrl {
                url,
                expires_at: Some(ts(exp)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_run_logs_url(
        &self,
        req: Request<GetWorkflowRunLogsUrlRequest>,
    ) -> std::result::Result<Response<WorkflowLogsUrl>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let run = self.ci.get_run(&cx, &repo.id, &r.run_id).await?;
            let attempt = if r.attempt == 0 {
                run.run_attempt
            } else {
                r.attempt
            };
            let (url, exp) = self.ci.run_logs_url(&repo.id, &r.run_id, attempt);
            Ok(WorkflowLogsUrl {
                url,
                expires_at: Some(ts(exp)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_workflow_run_logs(
        &self,
        req: Request<DeleteWorkflowRunLogsRequest>,
    ) -> std::result::Result<Response<DeleteWorkflowRunLogsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "workflow.logs_delete").await?;
            self.ci.delete_run_logs(&cx, &repo, &r.run_id).await?;
            Ok(DeleteWorkflowRunLogsResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_workflow_artifacts(
        &self,
        req: Request<ListWorkflowArtifactsRequest>,
    ) -> std::result::Result<Response<ListWorkflowArtifactsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let (artifacts, page) = paged(
                self.ci
                    .list_artifacts(&cx, &repo, &r.run_id, &r.name)
                    .await?,
                &r.page,
            );
            Ok(ListWorkflowArtifactsResponse { artifacts, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_artifact(
        &self,
        req: Request<GetWorkflowArtifactRequest>,
    ) -> std::result::Result<Response<WorkflowArtifact>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            self.ci.get_artifact(&cx, &repo, &r.artifact_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_workflow_artifact(
        &self,
        req: Request<DeleteWorkflowArtifactRequest>,
    ) -> std::result::Result<Response<DeleteWorkflowArtifactResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "artifact.delete").await?;
            self.ci.delete_artifact(&cx, &repo, &r.artifact_id).await?;
            Ok(DeleteWorkflowArtifactResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_artifact_download_url(
        &self,
        req: Request<GetWorkflowArtifactDownloadUrlRequest>,
    ) -> std::result::Result<Response<GetWorkflowArtifactDownloadUrlResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let (url, exp) = self
                .ci
                .artifact_download_url(&cx, &repo, &r.artifact_id)
                .await?;
            Ok(GetWorkflowArtifactDownloadUrlResponse {
                url,
                expires_at: Some(ts(exp)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_workflow_artifact_upload(
        &self,
        req: Request<CreateWorkflowArtifactUploadRequest>,
    ) -> std::result::Result<Response<CreateWorkflowArtifactUploadResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.job_caller(&caller, &r.repo, &r.job_id).await?;
            let (a, url, exp) = self
                .ci
                .create_artifact(
                    &cx,
                    &repo,
                    &r.job_id,
                    &r.name,
                    r.archive,
                    &r.content_type,
                    r.retention_days,
                )
                .await?;
            Ok(CreateWorkflowArtifactUploadResponse {
                artifact_id: a.id,
                upload_url: url,
                upload_headers: vec![KeyValue {
                    key: "Content-Type".into(),
                    value: a.content_type,
                }],
                expires_at: Some(ts(exp)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn finalize_workflow_artifact(
        &self,
        req: Request<FinalizeWorkflowArtifactRequest>,
    ) -> std::result::Result<Response<WorkflowArtifact>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let job_id = Self::caller_job(&caller)?;
            let (repo, cx) = self.job_caller(&caller, &r.repo, &job_id).await?;
            let a = self.ci.get_artifact(&cx, &repo, &r.artifact_id).await?;
            if a.job_id != job_id {
                return Err(ForgeError::permission_denied(
                    "the artifact belongs to another job",
                ));
            }
            self.ci
                .finalize_artifact(&cx, &repo, &r.artifact_id, r.size_bytes, &r.digest)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_workflow_caches(
        &self,
        req: Request<ListWorkflowCachesRequest>,
    ) -> std::result::Result<Response<ListWorkflowCachesResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let sort =
                WorkflowCacheSort::try_from(r.sort).unwrap_or(WorkflowCacheSort::Unspecified);
            let (caches, page) = paged(
                self.ci
                    .list_caches(&cx, &repo, &r.key_prefix, &r.r#ref, sort)
                    .await?,
                &r.page,
            );
            Ok(ListWorkflowCachesResponse { caches, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_workflow_caches(
        &self,
        req: Request<DeleteWorkflowCachesRequest>,
    ) -> std::result::Result<Response<DeleteWorkflowCachesResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "cache.delete").await?;
            let deleted_count = self
                .ci
                .delete_caches(&cx, &repo, &r.cache_id, &r.key, &r.r#ref)
                .await?;
            Ok(DeleteWorkflowCachesResponse { deleted_count })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_cache_usage(
        &self,
        req: Request<GetWorkflowCacheUsageRequest>,
    ) -> std::result::Result<Response<WorkflowCacheUsage>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            self.ci.cache_usage(&cx, &repo).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_workflow_cache_usage_by_repo(
        &self,
        req: Request<ListWorkflowCacheUsageByRepoRequest>,
    ) -> std::result::Result<Response<ListWorkflowCacheUsageByRepoResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self.org(&caller, &r.org, OrgPerm::ViewMembers, "").await?;
            let mut usages = Vec::new();
            let mut after = String::new();
            loop {
                let (repos, next_tok) = self
                    .forge()
                    .orgs
                    .list_repos(&cx, &org_id, &after, 500)
                    .await?;
                for repo in &repos {
                    let u = self.ci.cache_usage(&cx, repo).await?;
                    if u.active_count > 0 {
                        usages.push(u);
                    }
                }
                if next_tok.is_empty() || repos.is_empty() {
                    break;
                }
                after = next_tok;
            }
            let total = usages.iter().map(|u| u.active_size_bytes).sum();
            let (usages, page) = paged(usages, &r.page);
            Ok(ListWorkflowCacheUsageByRepoResponse {
                usages,
                page,
                total_size_bytes: total,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn lookup_workflow_cache(
        &self,
        req: Request<LookupWorkflowCacheRequest>,
    ) -> std::result::Result<Response<LookupWorkflowCacheResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.job_caller(&caller, &r.repo, &r.job_id).await?;
            let (cache, exact_match, download_url, exp) = self
                .ci
                .lookup_cache(&cx, &repo, &r.job_id, &r.key, &r.version, &r.restore_keys)
                .await?;
            Ok(LookupWorkflowCacheResponse {
                cache: Some(cache),
                exact_match,
                download_url,
                expires_at: Some(ts(exp)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn reserve_workflow_cache(
        &self,
        req: Request<ReserveWorkflowCacheRequest>,
    ) -> std::result::Result<Response<ReserveWorkflowCacheResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.job_caller(&caller, &r.repo, &r.job_id).await?;
            let (c, url, exp) = self
                .ci
                .reserve_cache(&cx, &repo, &r.job_id, &r.key, &r.version, r.size_bytes)
                .await?;
            Ok(ReserveWorkflowCacheResponse {
                cache_id: c.id,
                upload_url: url,
                upload_headers: Vec::new(),
                expires_at: Some(ts(exp)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn commit_workflow_cache(
        &self,
        req: Request<CommitWorkflowCacheRequest>,
    ) -> std::result::Result<Response<WorkflowCache>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let job_id = Self::caller_job(&caller)?;
            let (repo, cx) = self.job_caller(&caller, &r.repo, &job_id).await?;
            self.ci
                .commit_cache(&cx, &repo, &r.cache_id, r.size_bytes, &r.digest)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_actions_policy(
        &self,
        req: Request<GetActionsPolicyRequest>,
    ) -> std::result::Result<Response<ActionsPolicy>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.policy_scope(&caller, &r.scope, false, "").await?;
            self.ci.get_policy(&cx, &scope).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_actions_policy(
        &self,
        req: Request<SetActionsPolicyRequest>,
    ) -> std::result::Result<Response<ActionsPolicy>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .policy_scope(&caller, &r.scope, true, "actions_policy.update")
                .await?;
            self.ci.set_policy(&cx, &scope, &r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_actions_policy_enabled_repos(
        &self,
        req: Request<ListActionsPolicyEnabledReposRequest>,
    ) -> std::result::Result<Response<ListActionsPolicyEnabledReposResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(
                    &caller,
                    &r.org,
                    OrgPerm::Manage,
                    "actions_policy.enabled_repos",
                )
                .await?;
            let ids = self.ci.enabled_repos(&cx, &org_id).await?;
            let refs: Vec<RepoRef> = ids
                .iter()
                .filter_map(|i| super::engine::repo_ref_of(i))
                .collect();
            let (repos, page) = paged(refs, &r.page);
            Ok(ListActionsPolicyEnabledReposResponse { repos, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_actions_policy_enabled_repos(
        &self,
        req: Request<SetActionsPolicyEnabledReposRequest>,
    ) -> std::result::Result<Response<SetActionsPolicyEnabledReposResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(
                    &caller,
                    &r.org,
                    OrgPerm::Manage,
                    "actions_policy.enabled_repos",
                )
                .await?;
            let ids = self.repo_ids(&caller, &r.repos).await?;
            self.ci.set_enabled_repos(&cx, &org_id, ids).await?;
            Ok(SetActionsPolicyEnabledReposResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_required_workflows(
        &self,
        req: Request<ListRequiredWorkflowsRequest>,
    ) -> std::result::Result<Response<ListRequiredWorkflowsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self.org(&caller, &r.org, OrgPerm::ViewMembers, "").await?;
            let (workflows, page) = paged(self.ci.required_workflows(&cx, &org_id).await?, &r.page);
            Ok(ListRequiredWorkflowsResponse { workflows, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_required_workflow(
        &self,
        req: Request<CreateRequiredWorkflowRequest>,
    ) -> std::result::Result<Response<WorkflowRequirement>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(&caller, &r.org, OrgPerm::Manage, "workflow.required_create")
                .await?;
            let source = self.base.repo(&caller, &r.source_repo).await?;
            if source.org_id != org_id {
                return Err(ForgeError::invalid(
                    "invalid_field",
                    "the source repo must belong to the org",
                ));
            }
            let mut selected = Vec::new();
            for s in &r.selected_repos {
                selected.push(self.base.repo(&caller, &Some(s.clone())).await?);
            }
            let scope = WorkflowRequirementScope::try_from(r.scope)
                .unwrap_or(WorkflowRequirementScope::All);
            self.ci
                .create_required(
                    &cx,
                    &org_id,
                    &r.name,
                    &source,
                    &r.source_path,
                    &r.source_ref,
                    scope,
                    selected,
                    r.enforced,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_required_workflow(
        &self,
        req: Request<UpdateRequiredWorkflowRequest>,
    ) -> std::result::Result<Response<WorkflowRequirement>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(&caller, &r.org, OrgPerm::Manage, "workflow.required_update")
                .await?;
            let selected = match &r.selected_repos {
                Some(s) => {
                    let mut v = Vec::new();
                    for x in &s.repos {
                        v.push(self.base.repo(&caller, &Some(x.clone())).await?);
                    }
                    Some(v)
                }
                None => None,
            };
            let scope = r
                .scope
                .and_then(|s| WorkflowRequirementScope::try_from(s).ok());
            self.ci
                .update_required(
                    &cx,
                    &org_id,
                    &r.required_workflow_id,
                    r.name.clone(),
                    r.source_path.clone(),
                    r.source_ref.clone(),
                    scope,
                    selected,
                    r.enforced,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_required_workflow(
        &self,
        req: Request<DeleteRequiredWorkflowRequest>,
    ) -> std::result::Result<Response<DeleteRequiredWorkflowResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(&caller, &r.org, OrgPerm::Manage, "workflow.required_delete")
                .await?;
            self.ci
                .delete_required(&cx, &org_id, &r.required_workflow_id)
                .await?;
            Ok(DeleteRequiredWorkflowResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn create_workflow_oidc_token(
        &self,
        req: Request<CreateWorkflowOidcTokenRequest>,
    ) -> std::result::Result<Response<CreateWorkflowOidcTokenResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.job_caller(&caller, &r.repo, &r.job_id).await?;
            let (token, exp) = self
                .ci
                .oidc_token(&cx, &repo, &r.job_id, &r.audience)
                .await?;
            Ok(CreateWorkflowOidcTokenResponse {
                token,
                expires_at: Some(ts(exp)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_oidc_claim_template(
        &self,
        req: Request<GetWorkflowOidcClaimTemplateRequest>,
    ) -> std::result::Result<Response<WorkflowOidcClaimTemplate>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.policy_scope(&caller, &r.scope, false, "").await?;
            self.ci.oidc_template(&cx, &scope).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_workflow_oidc_claim_template(
        &self,
        req: Request<SetWorkflowOidcClaimTemplateRequest>,
    ) -> std::result::Result<Response<WorkflowOidcClaimTemplate>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .policy_scope(&caller, &r.scope, true, "actions_policy.oidc_template")
                .await?;
            self.ci
                .set_oidc_template(&cx, &scope, r.use_default, r.include_claim_keys)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_attestation(
        &self,
        req: Request<CreateAttestationRequest>,
    ) -> std::result::Result<Response<Attestation>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.job_caller(&caller, &r.repo, &r.job_id).await?;
            self.ci
                .create_attestation(
                    &cx,
                    &repo,
                    &r.job_id,
                    r.subjects,
                    &r.predicate_type,
                    &r.predicate_json,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_attestations(
        &self,
        req: Request<ListAttestationsRequest>,
    ) -> std::result::Result<Response<ListAttestationsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let list = if r.repo.as_ref().is_some_and(|x| x.repo.is_some()) {
                let (repo, cx) = self.read(&caller, &r.repo).await?;
                self.ci
                    .list_attestations(&cx, &repo.id, "", &r.subject_digest, &r.predicate_type)
                    .await?
            } else {
                let (org_id, cx) = self.org(&caller, &r.org, OrgPerm::ViewMembers, "").await?;
                self.ci
                    .list_attestations(&cx, "", &org_id, &r.subject_digest, &r.predicate_type)
                    .await?
            };
            let (attestations, page) = paged(list, &r.page);
            Ok(ListAttestationsResponse { attestations, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn verify_attestation(
        &self,
        req: Request<VerifyAttestationRequest>,
    ) -> std::result::Result<Response<VerifyAttestationResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            if r.subject_digest.is_empty() {
                return Err(ForgeError::missing("subject_digest"));
            }
            let (repo_id, org_id, cx) = if r.repo.as_ref().is_some_and(|x| x.repo.is_some()) {
                let (repo, cx) = self.read(&caller, &r.repo).await?;
                (repo.id, String::new(), cx)
            } else {
                let (org_id, cx) = self.org(&caller, &r.org, OrgPerm::ViewMembers, "").await?;
                (String::new(), org_id, cx)
            };
            let (ok, failures) = self
                .ci
                .verify_attestations(
                    &cx,
                    &repo_id,
                    &org_id,
                    &r.subject_digest,
                    &r.predicate_type,
                    &r.expected_signer_workflow,
                    &r.expected_source_ref,
                )
                .await?;
            Ok(VerifyAttestationResponse {
                verified: !ok.is_empty(),
                attestations: ok,
                failures,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_attestation(
        &self,
        req: Request<DeleteAttestationRequest>,
    ) -> std::result::Result<Response<DeleteAttestationResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.admin(&caller, &repo, "attestation.delete").await?;
            self.ci
                .delete_attestation(&cx, &repo, &r.attestation_id)
                .await?;
            Ok(DeleteAttestationResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn search_workflow_run_logs(
        &self,
        req: Request<SearchWorkflowRunLogsRequest>,
    ) -> std::result::Result<Response<SearchWorkflowRunLogsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            if r.query.trim().is_empty() {
                return Err(ForgeError::missing("query"));
            }
            if r.run_id.is_empty() {
                return Err(ForgeError::missing("run_id"));
            }
            self.ci.get_run(&cx, &repo.id, &r.run_id).await?;
            let limit = if r.limit == 0 { 100 } else { r.limit.min(1000) } as usize;
            let matches = self
                .ci
                .search_logs(&cx, &repo, &r.run_id, &r.query, limit)
                .await?
                .into_iter()
                .map(|m| WorkflowLogMatch {
                    job_id: m.job_id,
                    job_name: m.job_name,
                    line: m.line,
                    text: m.text,
                })
                .collect();
            Ok(SearchWorkflowRunLogsResponse { matches })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workflow_badge(
        &self,
        req: Request<GetWorkflowBadgeRequest>,
    ) -> std::result::Result<Response<WorkflowBadge>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            if r.workflow.is_empty() {
                return Err(ForgeError::missing("workflow"));
            }
            let id = self.workflow_id(&cx, &repo, &r.workflow).await?;
            let b = self.ci.badge(&cx, &repo, &id, &r.branch).await?;
            Ok(WorkflowBadge {
                svg: b.svg(),
                label: b.label,
                message: b.message,
                color: b.color,
            })
        }
        .await;
        reply(&caller, out)
    }
}

impl Ci {
    /// Runs of a repo (latest attempts), optionally of one workflow.
    pub async fn list_runs(
        &self,
        cx: &Cx,
        repo_id: &str,
        workflow_id: &str,
    ) -> Result<Vec<WorkflowRun>> {
        let mut tx = self.db.begin(super::SURFACE, cx).await?;
        let mut scan = crate::store::Scan::scope(repo_id, crate::store::MAX_SCAN);
        if !workflow_id.is_empty() {
            scan = scan.key(crate::store::Key::K1(workflow_id.to_string()));
        }
        Ok(tx
            .scan_msgs::<super::model::RunRecord>(&super::RUNS, &scan)
            .await?
            .into_iter()
            .map(|(r, _)| super::engine::served_run(&r))
            .collect())
    }

    /// One attempt of a run.
    pub async fn run_attempt(
        &self,
        cx: &Cx,
        repo_id: &str,
        run_id: &str,
        attempt: u32,
    ) -> Result<WorkflowRun> {
        let run = self.get_run(cx, repo_id, run_id).await?;
        if attempt == 0 || attempt == run.run_attempt {
            return Ok(run);
        }
        let mut tx = self.db.begin(super::SURFACE, cx).await?;
        tx.get_msg::<WorkflowRun>(&super::ATTEMPTS, run_id, &format!("{attempt:010}"))
            .await?
            .map(|(r, _)| r)
            .ok_or_else(|| ForgeError::not_found(format!("attempt {attempt} of run {run_id}")))
    }

    /// Deletes a completed run with its attempts, jobs, logs and artifacts.
    pub async fn delete_run(&self, cx: &Cx, repo: &Repo, run_id: &str) -> Result<()> {
        let run = self.get_run(cx, &repo.id, run_id).await?;
        if run.status != WorkflowStatus::Completed as i32 {
            return Err(ForgeError::invalid_state("the run is still running"));
        }
        self.delete_run_logs(cx, repo, run_id).await?;
        for a in self.list_artifacts(cx, repo, run_id, "").await? {
            self.delete_artifact(cx, repo, &a.id).await?;
        }
        let mut tx = self.db.begin(super::SURFACE, cx).await?;
        for (_, row) in super::engine::Ci::run_jobs(&mut tx, &repo.id, run_id, None).await? {
            tx.delete(&super::JOBS, &repo.id, &row.id).await?;
        }
        for row in tx
            .scan(&super::ATTEMPTS, &crate::store::Scan::scope(run_id, 1000))
            .await?
        {
            tx.delete(&super::ATTEMPTS, run_id, &row.id).await?;
        }
        tx.delete(&super::RUNS, &repo.id, run_id).await?;
        tx.audit(
            crate::audit::AuditDraft::new("workflow.run_delete", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::WorkflowRun, run_id),
        )
        .await?;
        tx.commit().await
    }
}
