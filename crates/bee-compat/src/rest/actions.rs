//! Actions: workflows, runs, jobs, logs, artifacts, dispatch, re-run, cancel; secrets (sealed
//! boxes) and variables at repo, org and environment scope; environments and deployments.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bee_forge::Cx;
use bee_forge::auth::OrgPerm;
use bee_forge::ci::environments::EnvironmentSpec;
use bee_forge::ci::secrets::VaultScope;
use bee_policy::Action;
use bee_proto::v1::deployment_service_server::DeploymentService as _;
use bee_proto::v1::org_service_server::OrgService as _;
use bee_proto::v1::workflow_service_server::WorkflowService as _;
use bee_proto::v1::{
    ApproveWorkflowRunRequest, CancelWorkflowRunRequest, CreateRepositoryDispatchEventRequest,
    DeleteWorkflowArtifactRequest, DeleteWorkflowRunLogsRequest, DeleteWorkflowRunRequest,
    Deployment, DeploymentEnvironment, DeploymentEnvironmentReviewer, DeploymentRefPolicyMode,
    DeploymentReviewerKind, DeploymentState, DeploymentStatus, DisableWorkflowRequest,
    DispatchWorkflowRequest, EnableWorkflowRequest, ForceCancelWorkflowRunRequest, GetOrgRequest,
    GetWorkflowArtifactDownloadUrlRequest, GetWorkflowArtifactRequest,
    GetWorkflowJobLogsUrlRequest, GetWorkflowJobRequest, GetWorkflowRequest,
    GetWorkflowRunAttemptRequest, GetWorkflowRunLogsUrlRequest, GetWorkflowRunRequest, KeyValue,
    ListWorkflowArtifactsRequest, ListWorkflowJobsRequest, ListWorkflowRunsRequest,
    ListWorkflowsRequest, Principal, Repo, RerunFailedWorkflowJobsRequest, RerunWorkflowJobRequest,
    RerunWorkflowRunRequest, Secret, SecretVisibility, Variable, Workflow, WorkflowArtifact,
    WorkflowConclusion, WorkflowJob, WorkflowJobAttemptFilter, WorkflowRun, WorkflowState,
    WorkflowStatus,
};
use bee_proto::v1::{
    CreateDeploymentRequest, CreateDeploymentStatusRequest, DeleteDeploymentRequest,
    DeploymentRequiredContexts, GetDeploymentRequest, ListDeploymentStatusesRequest,
    ListDeploymentsRequest,
};
use serde_json::{Value, json};

use super::{b, ci, need, repo_of, rid, s, strs, u};
use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Reply, all_pages, encode};
use crate::shapes;
use crate::time::{iso, iso_or, parse_iso, ts_secs};

fn status_word(s: WorkflowStatus) -> &'static str {
    match s {
        WorkflowStatus::Requested => "requested",
        WorkflowStatus::Pending => "pending",
        WorkflowStatus::Waiting => "waiting",
        WorkflowStatus::ActionRequired => "action_required",
        WorkflowStatus::InProgress => "in_progress",
        WorkflowStatus::Completed => "completed",
        _ => "queued",
    }
}

fn conclusion_word(c: WorkflowConclusion) -> Value {
    json!(match c {
        WorkflowConclusion::Success => "success",
        WorkflowConclusion::Failure => "failure",
        WorkflowConclusion::Neutral => "neutral",
        WorkflowConclusion::Cancelled => "cancelled",
        WorkflowConclusion::Skipped => "skipped",
        WorkflowConclusion::TimedOut => "timed_out",
        WorkflowConclusion::ActionRequired => "action_required",
        WorkflowConclusion::Stale => "stale",
        WorkflowConclusion::StartupFailure => "startup_failure",
        WorkflowConclusion::Unspecified => return Value::Null,
    })
}

fn state_word(s: WorkflowState) -> &'static str {
    match s {
        WorkflowState::DisabledManually => "disabled_manually",
        WorkflowState::DisabledInactivity => "disabled_inactivity",
        WorkflowState::DisabledFork => "disabled_fork",
        WorkflowState::Deleted => "deleted",
        _ => "active",
    }
}

fn workflow_json(cx: &Ctx, repo: &Repo, w: &Workflow) -> Value {
    let id = cx.hub.ids.remember("workflow", &w.id, &repo.id);
    let file = w.path.rsplit('/').next().unwrap_or(&w.path);
    json!({
        "id": id,
        "node_id": crate::ids::node("Workflow", &w.id),
        "name": if w.name.is_empty() { w.path.clone() } else { w.name.clone() },
        "path": w.path,
        "state": state_word(WorkflowState::try_from(w.state).unwrap_or_default()),
        "created_at": iso(&w.created_at),
        "updated_at": iso_or(&w.updated_at, &w.created_at),
        "url": cx.api(&format!("/repos/{}/actions/workflows/{id}", repo.full_name)),
        "html_url": cx.html(&format!("/{}/blob/{}/{}", repo.full_name, if w.r#ref.is_empty() { repo.default_ref.as_str() } else { w.r#ref.as_str() }, w.path)),
        "badge_url": if w.badge_url.is_empty() { cx.html(&format!("/{}/workflows/{file}/badge.svg", repo.full_name)) } else { w.badge_url.clone() },
    })
}

async fn workflows_of(cx: &Ctx, repo: &Repo) -> ApiResult<Vec<Workflow>> {
    let svc = ci(cx)?.clone();
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListWorkflowsRequest {
            repo: rid(&repo.id),
            r#ref: String::new(),
            page: Some(page),
        });
        async move {
            let r = svc.list_workflows(req).await?.into_inner();
            Ok((r.workflows, r.page))
        }
    })
    .await
}

/// The forge handle of a REST `workflow_id`: a numeric id or a file name.
async fn workflow_handle(cx: &Ctx, repo: &Repo, raw: &str) -> ApiResult<String> {
    match raw.parse::<u64>() {
        Ok(n) => {
            if let Some(ix) = cx.hub.ids.get("workflow", n) {
                return Ok(ix.id);
            }
            workflows_of(cx, repo)
                .await?
                .into_iter()
                .find(|w| crate::ids::num("workflow", &w.id) == n)
                .map(|w| w.id)
                .ok_or_else(ApiError::not_found)
        }
        Err(_) => Ok(raw.to_string()),
    }
}

/// `GET /repos/{owner}/{repo}/actions/workflows`.
pub(crate) async fn workflows(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let items = workflows_of(&cx, &repo)
        .await?
        .iter()
        .map(|w| workflow_json(&cx, &repo, w))
        .collect();
    Ok(cx.paginate_wrapped("workflows", items, json!({})))
}

/// `GET /repos/{owner}/{repo}/actions/workflows/{id}`.
pub(crate) async fn workflow(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let h = workflow_handle(&cx, &repo, p.get("id")).await?;
    let w = ci(&cx)?
        .get_workflow(cx.req(GetWorkflowRequest {
            repo: rid(&repo.id),
            workflow: h,
            r#ref: String::new(),
            include_source: false,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(workflow_json(&cx, &repo, &w)))
}

/// `PUT /repos/{owner}/{repo}/actions/workflows/{id}/enable`.
pub(crate) async fn enable_workflow(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let h = workflow_handle(&cx, &repo, p.get("id")).await?;
    ci(&cx)?
        .enable_workflow(cx.req(EnableWorkflowRequest {
            repo: rid(&repo.id),
            workflow: h,
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `PUT /repos/{owner}/{repo}/actions/workflows/{id}/disable`.
pub(crate) async fn disable_workflow(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let h = workflow_handle(&cx, &repo, p.get("id")).await?;
    ci(&cx)?
        .disable_workflow(cx.req(DisableWorkflowRequest {
            repo: rid(&repo.id),
            workflow: h,
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `POST /repos/{owner}/{repo}/actions/workflows/{id}/dispatches`.
pub(crate) async fn dispatch(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let h = workflow_handle(&cx, &repo, p.get("id")).await?;
    let body = cx.body_json()?;
    let r#ref = need(&body, "ref", "Workflow")?;
    let r#ref = r#ref
        .strip_prefix("refs/heads/")
        .unwrap_or(r#ref)
        .to_string();
    let inputs = body
        .get("inputs")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| KeyValue {
                    key: k.clone(),
                    value: match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    },
                })
                .collect()
        })
        .unwrap_or_default();
    let r = ci(&cx)?
        .dispatch_workflow(cx.req(DispatchWorkflowRequest {
            repo: rid(&repo.id),
            workflow: h,
            r#ref,
            inputs,
        }))
        .await?
        .into_inner();
    if b(&body, "return_run_details") == Some(true) {
        let id = cx.hub.ids.remember("run", &r.run_id, &repo.id);
        return Ok(Reply::json(json!({
            "workflow_run_id": id,
            "run_url": cx.api(&format!("/repos/{}/actions/runs/{id}", repo.full_name)),
            "html_url": cx.html(&format!("/{}/actions/runs/{id}", repo.full_name)),
        })));
    }
    Ok(Reply::no_content())
}

/// `POST /repos/{owner}/{repo}/dispatches`.
pub(crate) async fn repository_dispatch(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    ci(&cx)?
        .create_repository_dispatch_event(
            cx.req(CreateRepositoryDispatchEventRequest {
                repo: rid(&repo.id),
                event_type: need(&body, "event_type", "RepositoryDispatch")?.to_string(),
                client_payload_json: body
                    .get("client_payload")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".into()),
            }),
        )
        .await?;
    Ok(Reply::no_content())
}

async fn run_json(cx: &Ctx, repo: &Repo, r: &WorkflowRun) -> Value {
    let id = cx.hub.ids.remember("run", &r.id, &repo.id);
    let base = cx.api(&format!("/repos/{}/actions/runs/{id}", repo.full_name));
    let git = &cx.hub.ports.git;
    let sha = match &r.head_changeset {
        Some(c) => git.git_id(repo, c).await.ok().flatten().unwrap_or_default(),
        None => String::new(),
    };
    let head_commit = match git.commit(repo, &sha).await.ok().flatten() {
        Some(c) => json!({
            "id": c.sha, "tree_id": c.tree, "message": c.message,
            "timestamp": crate::time::iso_secs(c.committer.time_secs),
            "author": {"name": c.author.name, "email": c.author.email},
            "committer": {"name": c.committer.name, "email": c.committer.email},
        }),
        None => Value::Null,
    };
    let workflow_id = crate::ids::num("workflow", &r.workflow_id);
    let repo_json = shapes::repo(cx, repo, None, None);
    let mut prs = Vec::new();
    for n in &r.change_request_numbers {
        prs.push(json!({
            "number": n,
            "url": cx.api(&format!("/repos/{}/pulls/{n}", repo.full_name)),
            "id": crate::ids::num("pull_number", &format!("{}:{n}", repo.id)),
        }));
    }
    json!({
        "id": id,
        "name": r.name,
        "node_id": crate::ids::node("WorkflowRun", &r.id),
        "head_branch": r.head_ref,
        "head_sha": sha,
        "path": r.workflow_path,
        "display_title": if r.display_title.is_empty() { r.name.clone() } else { r.display_title.clone() },
        "run_number": r.run_number,
        "event": r.event,
        "status": status_word(WorkflowStatus::try_from(r.status).unwrap_or_default()),
        "conclusion": conclusion_word(WorkflowConclusion::try_from(r.conclusion).unwrap_or_default()),
        "workflow_id": workflow_id,
        "check_suite_id": if r.check_suite_id.is_empty() { Value::Null } else { json!(crate::ids::num("check_suite", &r.check_suite_id)) },
        "check_suite_node_id": if r.check_suite_id.is_empty() { Value::Null } else { json!(crate::ids::node("CheckSuite", &r.check_suite_id)) },
        "url": base,
        "html_url": cx.html(&format!("/{}/actions/runs/{id}", repo.full_name)),
        "pull_requests": prs,
        "created_at": iso(&r.created_at),
        "updated_at": iso_or(&r.updated_at, &r.created_at),
        "actor": shapes::user(cx, r.actor.as_ref().unwrap_or(&Principal::default())).await,
        "triggering_actor": shapes::user(cx, r.triggering_actor.as_ref().or(r.actor.as_ref()).unwrap_or(&Principal::default())).await,
        "run_attempt": r.run_attempt.max(1),
        "referenced_workflows": [],
        "run_started_at": iso_or(&r.run_started_at, &r.created_at),
        "jobs_url": format!("{base}/jobs"),
        "logs_url": format!("{base}/logs"),
        "check_suite_url": cx.api(&format!("/repos/{}/check-suites/{}", repo.full_name, crate::ids::num("check_suite", &r.check_suite_id))),
        "artifacts_url": format!("{base}/artifacts"),
        "cancel_url": format!("{base}/cancel"),
        "rerun_url": format!("{base}/rerun"),
        "previous_attempt_url": if r.run_attempt > 1 { json!(format!("{base}/attempts/{}", r.run_attempt - 1)) } else { Value::Null },
        "workflow_url": cx.api(&format!("/repos/{}/actions/workflows/{workflow_id}", repo.full_name)),
        "head_commit": head_commit,
        "repository": repo_json.clone(),
        "head_repository": repo_json,
    })
}

fn status_filter(word: &str) -> (WorkflowStatus, WorkflowConclusion) {
    match word {
        "completed" => (WorkflowStatus::Completed, WorkflowConclusion::Unspecified),
        "in_progress" => (WorkflowStatus::InProgress, WorkflowConclusion::Unspecified),
        "queued" => (WorkflowStatus::Queued, WorkflowConclusion::Unspecified),
        "requested" => (WorkflowStatus::Requested, WorkflowConclusion::Unspecified),
        "waiting" => (WorkflowStatus::Waiting, WorkflowConclusion::Unspecified),
        "pending" => (WorkflowStatus::Pending, WorkflowConclusion::Unspecified),
        "action_required" => (
            WorkflowStatus::Unspecified,
            WorkflowConclusion::ActionRequired,
        ),
        "success" => (WorkflowStatus::Unspecified, WorkflowConclusion::Success),
        "failure" => (WorkflowStatus::Unspecified, WorkflowConclusion::Failure),
        "neutral" => (WorkflowStatus::Unspecified, WorkflowConclusion::Neutral),
        "cancelled" => (WorkflowStatus::Unspecified, WorkflowConclusion::Cancelled),
        "skipped" => (WorkflowStatus::Unspecified, WorkflowConclusion::Skipped),
        "timed_out" => (WorkflowStatus::Unspecified, WorkflowConclusion::TimedOut),
        "stale" => (WorkflowStatus::Unspecified, WorkflowConclusion::Stale),
        "startup_failure" => (
            WorkflowStatus::Unspecified,
            WorkflowConclusion::StartupFailure,
        ),
        _ => (WorkflowStatus::Unspecified, WorkflowConclusion::Unspecified),
    }
}

async fn runs_of(cx: &Ctx, repo: &Repo, workflow: &str) -> ApiResult<Vec<WorkflowRun>> {
    let svc = ci(cx)?.clone();
    let (status, conclusion) = status_filter(cx.q("status").unwrap_or(""));
    let actor = match cx.q("actor") {
        Some(l) => cx
            .by_login(l)
            .await
            .map(|p| p.id)
            .unwrap_or_else(|_| l.to_string()),
        None => String::new(),
    };
    let head = match cx.q("head_sha") {
        Some(sha) => cx.hub.ports.git.changeset(repo, sha).await?,
        None => None,
    };
    let (after, before) = match cx.q("created") {
        Some(c) => {
            let c = c.trim();
            if let Some((a, b)) = c.split_once("..") {
                (
                    parse_iso(a).map(ts_secs),
                    parse_iso(b).map(|x| ts_secs(x + 86_399)),
                )
            } else if let Some(x) = c.strip_prefix(">=") {
                (parse_iso(x).map(ts_secs), None)
            } else if let Some(x) = c.strip_prefix('>') {
                (parse_iso(x).map(|v| ts_secs(v + 1)), None)
            } else if let Some(x) = c.strip_prefix("<=") {
                (None, parse_iso(x).map(ts_secs))
            } else if let Some(x) = c.strip_prefix('<') {
                (None, parse_iso(x).map(|v| ts_secs(v - 1)))
            } else {
                (
                    parse_iso(c).map(ts_secs),
                    parse_iso(c).map(|x| ts_secs(x + 86_399)),
                )
            }
        }
        None => (None, None),
    };
    let branch = cx.q("branch").unwrap_or("").to_string();
    let event = cx.q("event").unwrap_or("").to_string();
    let exclude = cx.q("exclude_pull_requests") == Some("true");
    let mut runs = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListWorkflowRunsRequest {
            repo: rid(&repo.id),
            workflow: workflow.to_string(),
            r#ref: branch.clone(),
            event: event.clone(),
            status: status as i32,
            conclusion: conclusion as i32,
            actor_id: actor.clone(),
            head_changeset: head.clone(),
            change_request_number: 0,
            created_after: after,
            created_before: before,
            exclude_change_requests: exclude,
            page: Some(page),
            include_jobs: false,
        });
        async move {
            let r = svc.list_workflow_runs(req).await?.into_inner();
            Ok((r.runs, r.page))
        }
    })
    .await?;
    runs.sort_by(|a, b| {
        let t = |r: &WorkflowRun| {
            r.created_at
                .as_ref()
                .map(|t| (t.seconds, t.nanos))
                .unwrap_or((0, 0))
        };
        t(b).cmp(&t(a)).then(b.id.cmp(&a.id))
    });
    if let Some(cs) = cx.q("check_suite_id").and_then(|v| v.parse::<u64>().ok()) {
        runs.retain(|r| crate::ids::num("check_suite", &r.check_suite_id) == cs);
    }
    Ok(runs)
}

async fn runs_reply(cx: &Ctx, repo: &Repo, runs: Vec<WorkflowRun>) -> ApiResult<Reply> {
    let total = runs.len();
    let (page, per) = cx.page();
    let mut items = Vec::new();
    for r in runs.iter().skip((page - 1) * per).take(per) {
        items.push(run_json(cx, repo, r).await);
    }
    let mut padded = Vec::with_capacity(total);
    padded.extend(std::iter::repeat_n(Value::Null, (page - 1) * per));
    padded.extend(items);
    while padded.len() < total {
        padded.push(Value::Null);
    }
    Ok(cx.paginate_wrapped("workflow_runs", padded, json!({})))
}

/// `GET /repos/{owner}/{repo}/actions/runs`.
pub(crate) async fn runs(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let list = runs_of(&cx, &repo, "").await?;
    runs_reply(&cx, &repo, list).await
}

/// `GET /repos/{owner}/{repo}/actions/workflows/{id}/runs`.
pub(crate) async fn workflow_runs(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let h = workflow_handle(&cx, &repo, p.get("id")).await?;
    let list = runs_of(&cx, &repo, &h).await?;
    runs_reply(&cx, &repo, list).await
}

async fn run_id(cx: &Ctx, repo: &Repo, n: u64) -> ApiResult<String> {
    if let Some(ix) = cx.hub.ids.get("run", n) {
        return Ok(ix.id);
    }
    let svc = ci(cx)?.clone();
    let all = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListWorkflowRunsRequest {
            repo: rid(&repo.id),
            page: Some(page),
            ..Default::default()
        });
        async move {
            let r = svc.list_workflow_runs(req).await?.into_inner();
            Ok((r.runs, r.page))
        }
    })
    .await?;
    all.into_iter()
        .find(|r| crate::ids::num("run", &r.id) == n)
        .map(|r| r.id)
        .ok_or_else(ApiError::not_found)
}

/// `GET /repos/{owner}/{repo}/actions/runs/{id}`.
pub(crate) async fn run(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    let r = ci(&cx)?
        .get_workflow_run(cx.req(GetWorkflowRunRequest {
            repo: rid(&repo.id),
            run_id: id,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(run_json(&cx, &repo, &r).await))
}

/// `GET /repos/{owner}/{repo}/actions/runs/{id}/attempts/{attempt}`.
pub(crate) async fn run_attempt(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    let r = ci(&cx)?
        .get_workflow_run_attempt(cx.req(GetWorkflowRunAttemptRequest {
            repo: rid(&repo.id),
            run_id: id,
            attempt: p.num("attempt")? as u32,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(run_json(&cx, &repo, &r).await))
}

/// `DELETE /repos/{owner}/{repo}/actions/runs/{id}`.
pub(crate) async fn delete_run(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    ci(&cx)?
        .delete_workflow_run(cx.req(DeleteWorkflowRunRequest {
            repo: rid(&repo.id),
            run_id: id,
        }))
        .await?;
    Ok(Reply::no_content())
}

async fn job_json(cx: &Ctx, repo: &Repo, j: &WorkflowJob) -> Value {
    let id = cx.hub.ids.remember("job", &j.id, &repo.id);
    let run = crate::ids::num("run", &j.run_id);
    let sha = match &j.head_changeset {
        Some(c) => cx
            .hub
            .ports
            .git
            .git_id(repo, c)
            .await
            .ok()
            .flatten()
            .unwrap_or_default(),
        None => String::new(),
    };
    let steps: Vec<Value> = j
        .steps
        .iter()
        .map(|st| {
            json!({
                "name": st.name,
                "status": status_word(WorkflowStatus::try_from(st.status).unwrap_or_default()),
                "conclusion": conclusion_word(WorkflowConclusion::try_from(st.conclusion).unwrap_or_default()),
                "number": st.number,
                "started_at": iso(&st.started_at),
                "completed_at": iso(&st.completed_at),
            })
        })
        .collect();
    json!({
        "id": id,
        "run_id": run,
        "workflow_name": j.workflow_name,
        "head_branch": j.head_ref,
        "run_url": cx.api(&format!("/repos/{}/actions/runs/{run}", repo.full_name)),
        "run_attempt": j.run_attempt.max(1),
        "node_id": crate::ids::node("WorkflowJob", &j.id),
        "head_sha": sha,
        "url": cx.api(&format!("/repos/{}/actions/jobs/{id}", repo.full_name)),
        "html_url": cx.html(&format!("/{}/actions/runs/{run}/job/{id}", repo.full_name)),
        "status": status_word(WorkflowStatus::try_from(j.status).unwrap_or_default()),
        "conclusion": conclusion_word(WorkflowConclusion::try_from(j.conclusion).unwrap_or_default()),
        "created_at": iso(&j.created_at),
        "started_at": iso_or(&j.started_at, &j.created_at),
        "completed_at": iso(&j.completed_at),
        "name": j.name,
        "steps": steps,
        "check_run_url": if j.check_run_id.is_empty() { Value::Null } else { json!(cx.api(&format!("/repos/{}/check-runs/{}", repo.full_name, crate::ids::num("check_run", &j.check_run_id)))) },
        "labels": j.labels,
        "runner_id": if j.runner_id.is_empty() { Value::Null } else { json!(crate::ids::num("runner", &j.runner_id)) },
        "runner_name": if j.runner_name.is_empty() { Value::Null } else { json!(j.runner_name) },
        "runner_group_id": if j.runner_group_id.is_empty() { Value::Null } else { json!(crate::ids::num("runner_group", &j.runner_group_id)) },
        "runner_group_name": Value::Null,
    })
}

async fn jobs_of(cx: &Ctx, repo: &Repo, run: &str, attempt: u32) -> ApiResult<Vec<WorkflowJob>> {
    let svc = ci(cx)?.clone();
    let filter = if cx.q("filter") == Some("all") {
        WorkflowJobAttemptFilter::All
    } else {
        WorkflowJobAttemptFilter::Latest
    };
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListWorkflowJobsRequest {
            repo: rid(&repo.id),
            run_id: run.to_string(),
            attempt,
            filter: filter as i32,
            page: Some(page),
        });
        async move {
            let r = svc.list_workflow_jobs(req).await?.into_inner();
            Ok((r.jobs, r.page))
        }
    })
    .await
}

async fn jobs_reply(cx: &Ctx, repo: &Repo, jobs: Vec<WorkflowJob>) -> ApiResult<Reply> {
    let mut items = Vec::new();
    for j in &jobs {
        items.push(job_json(cx, repo, j).await);
    }
    Ok(cx.paginate_wrapped("jobs", items, json!({})))
}

/// `GET /repos/{owner}/{repo}/actions/runs/{id}/jobs`.
pub(crate) async fn run_jobs(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    let jobs = jobs_of(&cx, &repo, &id, 0).await?;
    jobs_reply(&cx, &repo, jobs).await
}

/// `GET /repos/{owner}/{repo}/actions/runs/{id}/attempts/{attempt}/jobs`.
pub(crate) async fn attempt_jobs(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    let jobs = jobs_of(&cx, &repo, &id, p.num("attempt")? as u32).await?;
    jobs_reply(&cx, &repo, jobs).await
}

async fn job_id(cx: &Ctx, repo: &Repo, n: u64) -> ApiResult<String> {
    if let Some(ix) = cx.hub.ids.get("job", n) {
        return Ok(ix.id);
    }
    for r in runs_of(cx, repo, "").await? {
        for j in jobs_of(cx, repo, &r.id, 0).await? {
            if crate::ids::num("job", &j.id) == n {
                return Ok(j.id);
            }
        }
    }
    Err(ApiError::not_found())
}

/// `GET /repos/{owner}/{repo}/actions/jobs/{id}`.
pub(crate) async fn job(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = job_id(&cx, &repo, p.num("id")?).await?;
    let j = ci(&cx)?
        .get_workflow_job(cx.req(GetWorkflowJobRequest {
            repo: rid(&repo.id),
            job_id: id,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(job_json(&cx, &repo, &j).await))
}

/// `GET /repos/{owner}/{repo}/actions/jobs/{id}/logs`: a redirect to the signed log URL.
pub(crate) async fn job_logs(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = job_id(&cx, &repo, p.num("id")?).await?;
    let u = ci(&cx)?
        .get_workflow_job_logs_url(cx.req(GetWorkflowJobLogsUrlRequest {
            repo: rid(&repo.id),
            job_id: id,
        }))
        .await?
        .into_inner();
    Ok(Reply::redirect(&u.url))
}

/// `GET /repos/{owner}/{repo}/actions/runs/{id}/logs`: a redirect to the signed archive.
pub(crate) async fn run_logs(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    let u = ci(&cx)?
        .get_workflow_run_logs_url(cx.req(GetWorkflowRunLogsUrlRequest {
            repo: rid(&repo.id),
            run_id: id,
            attempt: 0,
        }))
        .await?
        .into_inner();
    Ok(Reply::redirect(&u.url))
}

/// `DELETE /repos/{owner}/{repo}/actions/runs/{id}/logs`.
pub(crate) async fn delete_run_logs(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    ci(&cx)?
        .delete_workflow_run_logs(cx.req(DeleteWorkflowRunLogsRequest {
            repo: rid(&repo.id),
            run_id: id,
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `POST /repos/{owner}/{repo}/actions/runs/{id}/cancel`.
pub(crate) async fn cancel(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    ci(&cx)?
        .cancel_workflow_run(cx.req(CancelWorkflowRunRequest {
            repo: rid(&repo.id),
            run_id: id,
        }))
        .await
        .map_err(|st| {
            let e = ApiError::from(st);
            if e.status == 422 {
                e.with_status(409)
            } else {
                e
            }
        })?;
    Ok(Reply::accepted(json!({})))
}

/// `POST /repos/{owner}/{repo}/actions/runs/{id}/force-cancel`.
pub(crate) async fn force_cancel(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    ci(&cx)?
        .force_cancel_workflow_run(cx.req(ForceCancelWorkflowRunRequest {
            repo: rid(&repo.id),
            run_id: id,
        }))
        .await?;
    Ok(Reply::accepted(json!({})))
}

/// `POST /repos/{owner}/{repo}/actions/runs/{id}/rerun`.
pub(crate) async fn rerun(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    let body = cx.body_json()?;
    ci(&cx)?
        .rerun_workflow_run(cx.req(RerunWorkflowRunRequest {
            repo: rid(&repo.id),
            run_id: id,
            enable_debug_logging: b(&body, "enable_debug_logging").unwrap_or(false),
        }))
        .await?;
    Ok(Reply::created(json!({})))
}

/// `POST /repos/{owner}/{repo}/actions/runs/{id}/rerun-failed-jobs`.
pub(crate) async fn rerun_failed(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    let body = cx.body_json()?;
    ci(&cx)?
        .rerun_failed_workflow_jobs(cx.req(RerunFailedWorkflowJobsRequest {
            repo: rid(&repo.id),
            run_id: id,
            enable_debug_logging: b(&body, "enable_debug_logging").unwrap_or(false),
        }))
        .await?;
    Ok(Reply::created(json!({})))
}

/// `POST /repos/{owner}/{repo}/actions/jobs/{id}/rerun`.
pub(crate) async fn rerun_job(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = job_id(&cx, &repo, p.num("id")?).await?;
    let body = cx.body_json()?;
    ci(&cx)?
        .rerun_workflow_job(cx.req(RerunWorkflowJobRequest {
            repo: rid(&repo.id),
            job_id: id,
            enable_debug_logging: b(&body, "enable_debug_logging").unwrap_or(false),
        }))
        .await?;
    Ok(Reply::created(json!({})))
}

/// `POST /repos/{owner}/{repo}/actions/runs/{id}/approve`.
pub(crate) async fn approve(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    ci(&cx)?
        .approve_workflow_run(cx.req(ApproveWorkflowRunRequest {
            repo: rid(&repo.id),
            run_id: id,
        }))
        .await?;
    Ok(Reply::created(json!({})))
}

fn artifact_json(cx: &Ctx, repo: &Repo, a: &WorkflowArtifact) -> Value {
    let id = cx.hub.ids.remember("artifact", &a.id, &repo.id);
    let base = cx.api(&format!("/repos/{}/actions/artifacts/{id}", repo.full_name));
    json!({
        "id": id,
        "node_id": crate::ids::node("Artifact", &a.id),
        "name": a.name,
        "size_in_bytes": a.size_bytes,
        "url": base,
        "archive_download_url": format!("{base}/zip"),
        "expired": a.expired,
        "digest": if a.digest.is_empty() { Value::Null } else { json!(a.digest) },
        "created_at": iso(&a.created_at),
        "updated_at": iso_or(&a.updated_at, &a.created_at),
        "expires_at": iso(&a.expires_at),
        "workflow_run": {
            "id": crate::ids::num("run", &a.run_id),
            "repository_id": crate::ids::num("repo", &repo.id),
            "head_repository_id": crate::ids::num("repo", &repo.id),
            "head_branch": a.head_ref,
        },
    })
}

async fn artifacts_of(cx: &Ctx, repo: &Repo, run: &str) -> ApiResult<Vec<WorkflowArtifact>> {
    let svc = ci(cx)?.clone();
    let name = cx.q("name").unwrap_or("").to_string();
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListWorkflowArtifactsRequest {
            repo: rid(&repo.id),
            run_id: run.to_string(),
            name: name.clone(),
            page: Some(page),
        });
        async move {
            let r = svc.list_workflow_artifacts(req).await?.into_inner();
            Ok((r.artifacts, r.page))
        }
    })
    .await
}

/// `GET /repos/{owner}/{repo}/actions/artifacts`.
pub(crate) async fn artifacts(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let items = artifacts_of(&cx, &repo, "")
        .await?
        .iter()
        .map(|a| artifact_json(&cx, &repo, a))
        .collect();
    Ok(cx.paginate_wrapped("artifacts", items, json!({})))
}

/// `GET /repos/{owner}/{repo}/actions/runs/{id}/artifacts`.
pub(crate) async fn run_artifacts(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    let items = artifacts_of(&cx, &repo, &id)
        .await?
        .iter()
        .map(|a| artifact_json(&cx, &repo, a))
        .collect();
    Ok(cx.paginate_wrapped("artifacts", items, json!({})))
}

async fn artifact_id(cx: &Ctx, repo: &Repo, n: u64) -> ApiResult<String> {
    if let Some(ix) = cx.hub.ids.get("artifact", n) {
        return Ok(ix.id);
    }
    artifacts_of(cx, repo, "")
        .await?
        .into_iter()
        .find(|a| crate::ids::num("artifact", &a.id) == n)
        .map(|a| a.id)
        .ok_or_else(ApiError::not_found)
}

/// `GET /repos/{owner}/{repo}/actions/artifacts/{id}`.
pub(crate) async fn artifact(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = artifact_id(&cx, &repo, p.num("id")?).await?;
    let a = ci(&cx)?
        .get_workflow_artifact(cx.req(GetWorkflowArtifactRequest {
            repo: rid(&repo.id),
            artifact_id: id,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(artifact_json(&cx, &repo, &a)))
}

/// `DELETE /repos/{owner}/{repo}/actions/artifacts/{id}`.
pub(crate) async fn delete_artifact(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = artifact_id(&cx, &repo, p.num("id")?).await?;
    ci(&cx)?
        .delete_workflow_artifact(cx.req(DeleteWorkflowArtifactRequest {
            repo: rid(&repo.id),
            artifact_id: id,
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `GET /repos/{owner}/{repo}/actions/artifacts/{id}/zip`: a redirect to the signed URL.
pub(crate) async fn download_artifact(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    if p.get("format") != "zip" {
        return Err(ApiError::not_found());
    }
    let repo = repo_of(&cx, &p).await?;
    let id = artifact_id(&cx, &repo, p.num("id")?).await?;
    let u = ci(&cx)?
        .get_workflow_artifact_download_url(cx.req(GetWorkflowArtifactDownloadUrlRequest {
            repo: rid(&repo.id),
            artifact_id: id,
        }))
        .await?
        .into_inner();
    Ok(Reply::redirect(&u.url))
}

// ----- secrets and variables ----------------------------------------------------------------

fn box_public(cx: &Ctx) -> [u8; 32] {
    crate::sealed::public_key(&cx.hub.config.secret_box_key)
}

fn key_id(pk: &[u8; 32]) -> String {
    crate::ids::num("sealed-box-key", &STANDARD.encode(pk)).to_string()
}

fn public_key_json(cx: &Ctx) -> Value {
    let pk = box_public(cx);
    json!({"key_id": key_id(&pk), "key": STANDARD.encode(pk)})
}

/// Opens the sealed `encrypted_value` of a secret body.
fn open_secret(cx: &Ctx, body: &Value) -> ApiResult<Vec<u8>> {
    let enc = need(body, "encrypted_value", "Secret")?;
    if let Some(k) = s(body, "key_id")
        && k != key_id(&box_public(cx))
    {
        return Err(ApiError::validation(
            "Secret",
            "key_id",
            "invalid",
            "the key_id does not match the current public key",
        ));
    }
    let raw = STANDARD.decode(enc.trim()).map_err(|_| {
        ApiError::validation("Secret", "encrypted_value", "invalid", "not valid Base64")
    })?;
    crate::sealed::open(&cx.hub.config.secret_box_key, &raw).ok_or_else(|| {
        ApiError::validation(
            "Secret",
            "encrypted_value",
            "invalid",
            "the value does not decrypt with the repository public key",
        )
    })
}

fn secret_json(sec: &Secret) -> Value {
    json!({
        "name": sec.name,
        "created_at": iso(&sec.created_at),
        "updated_at": iso_or(&sec.updated_at, &sec.created_at),
    })
}

fn visibility_word(v: i32) -> &'static str {
    match SecretVisibility::try_from(v).unwrap_or_default() {
        SecretVisibility::Private => "private",
        SecretVisibility::Selected => "selected",
        _ => "all",
    }
}

fn visibility_of(body: &Value) -> SecretVisibility {
    match s(body, "visibility") {
        Some("private") => SecretVisibility::Private,
        Some("selected") => SecretVisibility::Selected,
        _ => SecretVisibility::All,
    }
}

fn variable_json(v: &Variable) -> Value {
    json!({
        "name": v.name,
        "value": v.value,
        "created_at": iso(&v.created_at),
        "updated_at": iso_or(&v.updated_at, &v.created_at),
    })
}

/// The repo scope and an authorized forge context for secrets and variables.
async fn repo_vault(cx: &Ctx, p: &Params) -> ApiResult<(Repo, VaultScope, Cx)> {
    ci(cx)?;
    let repo = repo_of(cx, p).await?;
    let fcx = super::issues::authorize(cx, &repo, Action::ManageSettings).await?;
    let scope = VaultScope::Repo(repo.id.clone(), repo.org_id.clone());
    Ok((repo, scope, fcx))
}

async fn env_vault(cx: &Ctx, p: &Params) -> ApiResult<(Repo, VaultScope, Cx)> {
    ci(cx)?;
    let repo = repo_of(cx, p).await?;
    let fcx = super::issues::authorize(cx, &repo, Action::ManageSettings).await?;
    let scope = VaultScope::Environment(
        repo.id.clone(),
        repo.org_id.clone(),
        p.get("env").to_string(),
    );
    Ok((repo, scope, fcx))
}

async fn org_vault(cx: &Ctx, p: &Params, perm: OrgPerm) -> ApiResult<(String, VaultScope, Cx)> {
    ci(cx)?;
    let org = cx
        .hub
        .svc
        .get_org(cx.req(GetOrgRequest {
            name_or_id: p.get("org").to_string(),
        }))
        .await?
        .into_inner();
    let caller = cx.caller()?;
    let fcx = cx.hub.forge.authz.org(caller, &org.id, perm).await?;
    Ok((org.name.clone(), VaultScope::Org(org.id.clone()), fcx))
}

/// `GET /repos/{owner}/{repo}/actions/secrets/public-key`.
pub(crate) async fn public_key(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    repo_vault(&cx, &p).await?;
    Ok(Reply::json(public_key_json(&cx)))
}

/// `GET /repos/{owner}/{repo}/environments/{env}/secrets/public-key`.
pub(crate) async fn env_public_key(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    env_vault(&cx, &p).await?;
    Ok(Reply::json(public_key_json(&cx)))
}

/// `GET /orgs/{org}/actions/secrets/public-key`.
pub(crate) async fn org_public_key(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    org_vault(&cx, &p, OrgPerm::Manage).await?;
    Ok(Reply::json(public_key_json(&cx)))
}

async fn list_secrets_in(cx: &Ctx, scope: &VaultScope, fcx: &Cx) -> ApiResult<Reply> {
    let list = ci(cx)?.ci().list_secrets(fcx, scope).await?;
    let items = list
        .iter()
        .map(|sec| {
            let mut v = secret_json(sec);
            if matches!(scope, VaultScope::Org(_)) {
                v["visibility"] = json!(visibility_word(sec.visibility));
            }
            v
        })
        .collect();
    Ok(cx.paginate_wrapped("secrets", items, json!({})))
}

/// `GET /repos/{owner}/{repo}/actions/secrets`.
pub(crate) async fn secrets(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = repo_vault(&cx, &p).await?;
    list_secrets_in(&cx, &scope, &fcx).await
}

/// `GET /repos/{owner}/{repo}/actions/organization-secrets`.
pub(crate) async fn repo_org_secrets(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (repo, _, fcx) = repo_vault(&cx, &p).await?;
    let list = ci(&cx)?.ci().repo_org_secrets(&fcx, &repo).await?;
    let items = list.iter().map(secret_json).collect();
    Ok(cx.paginate_wrapped("secrets", items, json!({})))
}

/// `GET /repos/{owner}/{repo}/environments/{env}/secrets`.
pub(crate) async fn env_secrets(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = env_vault(&cx, &p).await?;
    list_secrets_in(&cx, &scope, &fcx).await
}

/// `GET /orgs/{org}/actions/secrets`.
pub(crate) async fn org_secrets(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = org_vault(&cx, &p, OrgPerm::Manage).await?;
    list_secrets_in(&cx, &scope, &fcx).await
}

/// `GET /repos/{owner}/{repo}/actions/secrets/{name}`.
pub(crate) async fn secret(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = repo_vault(&cx, &p).await?;
    let sec = ci(&cx)?
        .ci()
        .get_secret(&fcx, &scope, p.get("name"))
        .await?;
    Ok(Reply::json(secret_json(&sec)))
}

/// `GET /orgs/{org}/actions/secrets/{name}`.
pub(crate) async fn org_secret(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (org, scope, fcx) = org_vault(&cx, &p, OrgPerm::Manage).await?;
    let sec = ci(&cx)?
        .ci()
        .get_secret(&fcx, &scope, p.get("name"))
        .await?;
    let mut v = secret_json(&sec);
    v["visibility"] = json!(visibility_word(sec.visibility));
    if sec.visibility == SecretVisibility::Selected as i32 {
        v["selected_repositories_url"] = json!(cx.api(&format!(
            "/orgs/{org}/actions/secrets/{}/repositories",
            sec.name
        )));
    }
    Ok(Reply::json(v))
}

async fn put_secret_in(
    cx: &Ctx,
    scope: &VaultScope,
    fcx: &Cx,
    name: &str,
    body: &Value,
    visibility: SecretVisibility,
    selected: Vec<String>,
) -> ApiResult<Reply> {
    let value = open_secret(cx, body)?;
    let ci = ci(cx)?.ci();
    let existed = ci.get_secret(fcx, scope, name).await.is_ok();
    ci.put_secret(fcx, scope, name, &value, visibility, selected)
        .await?;
    Ok(if existed {
        Reply::no_content()
    } else {
        Reply::created(json!({}))
    })
}

/// `PUT /repos/{owner}/{repo}/actions/secrets/{name}`.
pub(crate) async fn put_secret(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = repo_vault(&cx, &p).await?;
    let body = cx.body_json()?;
    put_secret_in(
        &cx,
        &scope,
        &fcx,
        p.get("name"),
        &body,
        SecretVisibility::Unspecified,
        Vec::new(),
    )
    .await
}

/// `PUT /repos/{owner}/{repo}/environments/{env}/secrets/{name}`.
pub(crate) async fn env_put_secret(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = env_vault(&cx, &p).await?;
    let body = cx.body_json()?;
    put_secret_in(
        &cx,
        &scope,
        &fcx,
        p.get("name"),
        &body,
        SecretVisibility::Unspecified,
        Vec::new(),
    )
    .await
}

async fn selected_repos(cx: &Ctx, org: &str, body: &Value) -> ApiResult<Vec<String>> {
    let Some(ids) = body
        .get("selected_repository_ids")
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };
    let repos = super::repos::list_org(cx, org).await?;
    let mut out = Vec::new();
    for n in ids.iter().filter_map(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|x| x.parse().ok()))
    }) {
        let r = repos
            .iter()
            .find(|r| crate::ids::num("repo", &r.id) == n)
            .ok_or_else(|| {
                ApiError::validation(
                    "Secret",
                    "selected_repository_ids",
                    "invalid",
                    &n.to_string(),
                )
            })?;
        out.push(r.id.clone());
    }
    Ok(out)
}

/// `PUT /orgs/{org}/actions/secrets/{name}`.
pub(crate) async fn org_put_secret(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (org, scope, fcx) = org_vault(&cx, &p, OrgPerm::Manage).await?;
    let body = cx.body_json()?;
    let selected = selected_repos(&cx, &org, &body).await?;
    put_secret_in(
        &cx,
        &scope,
        &fcx,
        p.get("name"),
        &body,
        visibility_of(&body),
        selected,
    )
    .await
}

/// `DELETE /repos/{owner}/{repo}/actions/secrets/{name}`.
pub(crate) async fn delete_secret(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = repo_vault(&cx, &p).await?;
    ci(&cx)?
        .ci()
        .delete_secret(&fcx, &scope, p.get("name"))
        .await?;
    Ok(Reply::no_content())
}

/// `DELETE /repos/{owner}/{repo}/environments/{env}/secrets/{name}`.
pub(crate) async fn env_delete_secret(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = env_vault(&cx, &p).await?;
    ci(&cx)?
        .ci()
        .delete_secret(&fcx, &scope, p.get("name"))
        .await?;
    Ok(Reply::no_content())
}

/// `DELETE /orgs/{org}/actions/secrets/{name}`.
pub(crate) async fn org_delete_secret(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = org_vault(&cx, &p, OrgPerm::Manage).await?;
    ci(&cx)?
        .ci()
        .delete_secret(&fcx, &scope, p.get("name"))
        .await?;
    Ok(Reply::no_content())
}

async fn variables_in(cx: &Ctx, scope: &VaultScope, fcx: &Cx) -> ApiResult<Reply> {
    let list = ci(cx)?.ci().list_variables(fcx, scope).await?;
    let items = list
        .iter()
        .map(|v| {
            let mut j = variable_json(v);
            if matches!(scope, VaultScope::Org(_)) {
                j["visibility"] = json!(visibility_word(v.visibility));
            }
            j
        })
        .collect();
    Ok(cx.paginate_wrapped("variables", items, json!({})))
}

/// `GET /repos/{owner}/{repo}/actions/variables`.
pub(crate) async fn variables(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = repo_vault(&cx, &p).await?;
    variables_in(&cx, &scope, &fcx).await
}

/// `GET /orgs/{org}/actions/variables`.
pub(crate) async fn org_variables(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = org_vault(&cx, &p, OrgPerm::Manage).await?;
    variables_in(&cx, &scope, &fcx).await
}

/// `POST /repos/{owner}/{repo}/actions/variables`.
pub(crate) async fn create_variable(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = repo_vault(&cx, &p).await?;
    let body = cx.body_json()?;
    ci(&cx)?
        .ci()
        .create_variable(
            &fcx,
            &scope,
            need(&body, "name", "Variable")?,
            s(&body, "value").unwrap_or(""),
            SecretVisibility::Unspecified,
            Vec::new(),
        )
        .await?;
    Ok(Reply::created(json!({})))
}

/// `POST /orgs/{org}/actions/variables`.
pub(crate) async fn org_create_variable(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (org, scope, fcx) = org_vault(&cx, &p, OrgPerm::Manage).await?;
    let body = cx.body_json()?;
    let selected = selected_repos(&cx, &org, &body).await?;
    ci(&cx)?
        .ci()
        .create_variable(
            &fcx,
            &scope,
            need(&body, "name", "Variable")?,
            s(&body, "value").unwrap_or(""),
            visibility_of(&body),
            selected,
        )
        .await?;
    Ok(Reply::created(json!({})))
}

/// `GET /repos/{owner}/{repo}/actions/variables/{name}`.
pub(crate) async fn variable(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = repo_vault(&cx, &p).await?;
    let v = ci(&cx)?
        .ci()
        .get_variable(&fcx, &scope, p.get("name"))
        .await?;
    Ok(Reply::json(variable_json(&v)))
}

/// `GET /orgs/{org}/actions/variables/{name}`.
pub(crate) async fn org_variable(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = org_vault(&cx, &p, OrgPerm::Manage).await?;
    let v = ci(&cx)?
        .ci()
        .get_variable(&fcx, &scope, p.get("name"))
        .await?;
    let mut j = variable_json(&v);
    j["visibility"] = json!(visibility_word(v.visibility));
    Ok(Reply::json(j))
}

async fn update_variable_in(
    cx: &Ctx,
    scope: &VaultScope,
    fcx: &Cx,
    name: &str,
    org: bool,
) -> ApiResult<Reply> {
    let body = cx.body_json()?;
    ci(cx)?
        .ci()
        .update_variable(
            fcx,
            scope,
            name,
            s(&body, "name").filter(|n| *n != name),
            s(&body, "value"),
            if org && body.get("visibility").is_some() {
                Some(visibility_of(&body))
            } else {
                None
            },
        )
        .await?;
    Ok(Reply::no_content())
}

/// `PATCH /repos/{owner}/{repo}/actions/variables/{name}`.
pub(crate) async fn update_variable(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = repo_vault(&cx, &p).await?;
    update_variable_in(&cx, &scope, &fcx, p.get("name"), false).await
}

/// `PATCH /orgs/{org}/actions/variables/{name}`.
pub(crate) async fn org_update_variable(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = org_vault(&cx, &p, OrgPerm::Manage).await?;
    update_variable_in(&cx, &scope, &fcx, p.get("name"), true).await
}

/// `DELETE /repos/{owner}/{repo}/actions/variables/{name}`.
pub(crate) async fn delete_variable(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = repo_vault(&cx, &p).await?;
    ci(&cx)?
        .ci()
        .delete_variable(&fcx, &scope, p.get("name"))
        .await?;
    Ok(Reply::no_content())
}

/// `DELETE /orgs/{org}/actions/variables/{name}`.
pub(crate) async fn org_delete_variable(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, scope, fcx) = org_vault(&cx, &p, OrgPerm::Manage).await?;
    ci(&cx)?
        .ci()
        .delete_variable(&fcx, &scope, p.get("name"))
        .await?;
    Ok(Reply::no_content())
}

// ----- environments and deployments ---------------------------------------------------------

async fn environment_json(cx: &Ctx, repo: &Repo, e: &DeploymentEnvironment) -> Value {
    let id = cx.hub.ids.remember("environment", &e.id, &repo.id);
    let mut rules = Vec::new();
    if !e.reviewers.is_empty() {
        let mut reviewers = Vec::new();
        for r in &e.reviewers {
            match DeploymentReviewerKind::try_from(r.kind).unwrap_or_default() {
                DeploymentReviewerKind::Team => reviewers.push(json!({
                    "type": "Team",
                    "reviewer": {"id": crate::ids::num("team", &r.team_id), "slug": r.team_slug, "name": r.team_slug},
                })),
                _ => reviewers.push(json!({
                    "type": "User",
                    "reviewer": shapes::user(cx, r.principal.as_ref().unwrap_or(&Principal::default())).await,
                })),
            }
        }
        rules.push(json!({
            "id": crate::ids::num("env_rule", &format!("{}:reviewers", e.id)),
            "node_id": crate::ids::node("EnvironmentProtectionRule", &format!("{}:reviewers", e.id)),
            "type": "required_reviewers",
            "prevent_self_review": e.prevent_self_review,
            "reviewers": reviewers,
        }));
    }
    if e.wait_timer_seconds > 0 {
        rules.push(json!({
            "id": crate::ids::num("env_rule", &format!("{}:wait", e.id)),
            "node_id": crate::ids::node("EnvironmentProtectionRule", &format!("{}:wait", e.id)),
            "type": "wait_timer",
            "wait_timer": e.wait_timer_seconds / 60,
        }));
    }
    let mode = DeploymentRefPolicyMode::try_from(e.ref_policy_mode).unwrap_or_default();
    let policy = match mode {
        DeploymentRefPolicyMode::ProtectedRefs => {
            json!({"protected_branches": true, "custom_branch_policies": false})
        }
        DeploymentRefPolicyMode::Custom => {
            json!({"protected_branches": false, "custom_branch_policies": true})
        }
        _ => Value::Null,
    };
    if !policy.is_null() {
        rules.push(json!({
            "id": crate::ids::num("env_rule", &format!("{}:branch", e.id)),
            "node_id": crate::ids::node("EnvironmentProtectionRule", &format!("{}:branch", e.id)),
            "type": "branch_policy",
        }));
    }
    json!({
        "id": id,
        "node_id": crate::ids::node("Environment", &e.id),
        "name": e.name,
        "url": cx.api(&format!("/repos/{}/environments/{}", repo.full_name, encode(&e.name))),
        "html_url": cx.html(&format!("/{}/deployments/activity_log?environments_filter={}", repo.full_name, encode(&e.name))),
        "created_at": iso(&e.created_at),
        "updated_at": iso_or(&e.updated_at, &e.created_at),
        "can_admins_bypass": e.can_admins_bypass,
        "protection_rules": rules,
        "deployment_branch_policy": policy,
    })
}

/// `GET /repos/{owner}/{repo}/environments`.
pub(crate) async fn environments(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    ci(&cx)?;
    let repo = repo_of(&cx, &p).await?;
    let fcx = super::issues::authorize(&cx, &repo, Action::Read).await?;
    let list = ci(&cx)?.ci().list_environments(&fcx, &repo).await?;
    let mut items = Vec::new();
    for e in &list {
        items.push(environment_json(&cx, &repo, e).await);
    }
    Ok(cx.paginate_wrapped("environments", items, json!({})))
}

/// `GET /repos/{owner}/{repo}/environments/{name}`.
pub(crate) async fn environment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    ci(&cx)?;
    let repo = repo_of(&cx, &p).await?;
    let fcx = super::issues::authorize(&cx, &repo, Action::Read).await?;
    let e = ci(&cx)?
        .ci()
        .get_environment(&fcx, &repo, p.get("name"))
        .await?;
    Ok(Reply::json(environment_json(&cx, &repo, &e).await))
}

async fn reviewer_of(cx: &Ctx, repo: &Repo, v: &Value) -> ApiResult<DeploymentEnvironmentReviewer> {
    let n = u(v, "id").ok_or_else(|| ApiError::missing_field("Environment", "reviewers.id"))?;
    if s(v, "type") == Some("Team") {
        let (owner, _) = shapes::split(repo);
        let team = cx.hub.ids.get("team", n).map(|ix| ix.id).ok_or_else(|| {
            ApiError::validation("Environment", "reviewers", "invalid", &n.to_string())
        })?;
        let _ = owner;
        return Ok(DeploymentEnvironmentReviewer {
            kind: DeploymentReviewerKind::Team as i32,
            team_id: team,
            ..Default::default()
        });
    }
    let pid = match cx.hub.ids.get("user", n) {
        Some(ix) => ix.id,
        None => {
            let svc = cx.hub.svc.clone();
            let list = all_pages(|page| {
                let svc = svc.clone();
                let req = cx.req(bee_proto::v1::ListCollaboratorsRequest {
                    repo: rid(&repo.id),
                    direct_only: false,
                    page: Some(page),
                });
                async move {
                    use bee_proto::v1::repo_service_server::RepoService as _;
                    let r = svc.list_collaborators(req).await?.into_inner();
                    Ok((r.collaborators, r.page))
                }
            })
            .await?;
            list.into_iter()
                .filter_map(|c| c.principal)
                .find(|p| crate::ids::num("user", &p.id) == n)
                .map(|p| p.id)
                .ok_or_else(|| {
                    ApiError::validation("Environment", "reviewers", "invalid", &n.to_string())
                })?
        }
    };
    Ok(DeploymentEnvironmentReviewer {
        kind: DeploymentReviewerKind::Principal as i32,
        principal: Some(cx.principal(&pid).await),
        ..Default::default()
    })
}

/// `PUT /repos/{owner}/{repo}/environments/{name}`: create or update.
pub(crate) async fn put_environment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    ci(&cx)?;
    let repo = repo_of(&cx, &p).await?;
    let fcx = super::issues::authorize(&cx, &repo, Action::ManageSettings).await?;
    let body = cx.body_json()?;
    let reviewers = match body.get("reviewers").and_then(Value::as_array) {
        Some(list) => {
            let mut out = Vec::new();
            for r in list {
                out.push(reviewer_of(&cx, &repo, r).await?);
            }
            Some(out)
        }
        None => None,
    };
    let mode = match body.get("deployment_branch_policy") {
        Some(Value::Null) => Some(DeploymentRefPolicyMode::All),
        Some(v) if b(v, "protected_branches") == Some(true) => {
            Some(DeploymentRefPolicyMode::ProtectedRefs)
        }
        Some(v) if b(v, "custom_branch_policies") == Some(true) => {
            Some(DeploymentRefPolicyMode::Custom)
        }
        _ => None,
    };
    let spec = EnvironmentSpec {
        reviewers,
        prevent_self_review: b(&body, "prevent_self_review"),
        wait_timer_seconds: u(&body, "wait_timer").map(|m| (m * 60) as u32),
        ref_policy_mode: mode,
        ref_policies: None,
        can_admins_bypass: b(&body, "can_admins_bypass"),
        require_green: None,
    };
    let ci_mod = ci(&cx)?.ci();
    let name = p.get("name");
    let e = match ci_mod.get_environment(&fcx, &repo, name).await {
        Ok(_) => ci_mod.update_environment(&fcx, &repo, name, &spec).await?,
        Err(_) => ci_mod.create_environment(&fcx, &repo, name, &spec).await?,
    };
    Ok(Reply::json(environment_json(&cx, &repo, &e).await))
}

/// `DELETE /repos/{owner}/{repo}/environments/{name}`.
pub(crate) async fn delete_environment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    ci(&cx)?;
    let repo = repo_of(&cx, &p).await?;
    let fcx = super::issues::authorize(&cx, &repo, Action::ManageSettings).await?;
    ci(&cx)?
        .ci()
        .delete_environment(&fcx, &repo, p.get("name"))
        .await?;
    Ok(Reply::no_content())
}

fn deployment_state_word(s: DeploymentState) -> &'static str {
    match s {
        DeploymentState::Queued => "queued",
        DeploymentState::InProgress => "in_progress",
        DeploymentState::Success => "success",
        DeploymentState::Failure => "failure",
        DeploymentState::Error => "error",
        DeploymentState::Inactive => "inactive",
        _ => "pending",
    }
}

async fn deployment_json(cx: &Ctx, repo: &Repo, d: &Deployment) -> Value {
    let id = cx.hub.ids.remember("deployment", &d.id, &repo.id);
    let url = cx.api(&format!("/repos/{}/deployments/{id}", repo.full_name));
    let sha = match &d.changeset {
        Some(c) => cx
            .hub
            .ports
            .git
            .git_id(repo, c)
            .await
            .ok()
            .flatten()
            .unwrap_or_default(),
        None => String::new(),
    };
    json!({
        "url": url,
        "id": id,
        "node_id": crate::ids::node("Deployment", &d.id),
        "sha": sha,
        "ref": d.r#ref,
        "task": if d.task.is_empty() { "deploy" } else { d.task.as_str() },
        "payload": serde_json::from_str::<Value>(&d.payload_json).unwrap_or(json!({})),
        "original_environment": if d.original_environment.is_empty() { d.environment.clone() } else { d.original_environment.clone() },
        "environment": d.environment,
        "description": if d.description.is_empty() { Value::Null } else { json!(d.description) },
        "creator": shapes::user(cx, d.creator.as_ref().unwrap_or(&Principal::default())).await,
        "created_at": iso(&d.created_at),
        "updated_at": iso_or(&d.updated_at, &d.created_at),
        "statuses_url": format!("{url}/statuses"),
        "repository_url": cx.api(&format!("/repos/{}", repo.full_name)),
        "transient_environment": d.transient_environment,
        "production_environment": d.production_environment,
        "performed_via_github_app": Value::Null,
    })
}

async fn deployments_of(cx: &Ctx, repo: &Repo) -> ApiResult<Vec<Deployment>> {
    let svc = ci(cx)?.clone();
    let changeset = match cx.q("sha") {
        Some(sha) => cx.hub.ports.git.changeset(repo, sha).await?,
        None => None,
    };
    if cx.q("sha").is_some() && changeset.is_none() {
        return Ok(Vec::new());
    }
    let env = cx.q("environment").unwrap_or("").to_string();
    let r#ref = cx.q("ref").unwrap_or("").to_string();
    let task = cx.q("task").unwrap_or("").to_string();
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListDeploymentsRequest {
            repo: rid(&repo.id),
            environment: env.clone(),
            r#ref: r#ref.clone(),
            changeset: changeset.clone(),
            task: task.clone(),
            state: 0,
            creator_id: String::new(),
            page: Some(page),
        });
        async move {
            let r = svc.list_deployments(req).await?.into_inner();
            Ok((r.deployments, r.page))
        }
    })
    .await
}

/// `GET /repos/{owner}/{repo}/deployments`.
pub(crate) async fn deployments(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let list = deployments_of(&cx, &repo).await?;
    let mut items = Vec::new();
    for d in &list {
        items.push(deployment_json(&cx, &repo, d).await);
    }
    Ok(cx.paginate(items))
}

/// The deployment of numeric id `n`.
async fn deployment_by_num(cx: &Ctx, repo: &Repo, n: u64) -> ApiResult<Deployment> {
    let svc = ci(cx)?;
    if let Some(ix) = cx.hub.ids.get("deployment", n)
        && let Ok(d) = svc
            .get_deployment(cx.req(GetDeploymentRequest {
                repo: rid(&repo.id),
                deployment_id: ix.id.clone(),
            }))
            .await
    {
        return Ok(d.into_inner());
    }
    let svc = svc.clone();
    let all = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListDeploymentsRequest {
            repo: rid(&repo.id),
            page: Some(page),
            ..Default::default()
        });
        async move {
            let r = svc.list_deployments(req).await?.into_inner();
            Ok((r.deployments, r.page))
        }
    })
    .await?;
    all.into_iter()
        .find(|d| crate::ids::num("deployment", &d.id) == n)
        .ok_or_else(ApiError::not_found)
}

/// `GET /repos/{owner}/{repo}/deployments/{id}`.
pub(crate) async fn deployment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let d = deployment_by_num(&cx, &repo, p.num("id")?).await?;
    Ok(Reply::json(deployment_json(&cx, &repo, &d).await))
}

/// `POST /repos/{owner}/{repo}/deployments`.
pub(crate) async fn create_deployment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let rev = need(&body, "ref", "Deployment")?.to_string();
    let git = &cx.hub.ports.git;
    let (r#ref, changeset) = match git.resolve(&repo, &rev).await? {
        Some(sha) if sha.starts_with(rev.as_str()) => {
            let cs = git.changeset(&repo, &sha).await?;
            let branch = if repo.default_ref.is_empty() {
                "main".to_string()
            } else {
                repo.default_ref.clone()
            };
            (branch, cs)
        }
        Some(_) => (
            rev.strip_prefix("refs/heads/").unwrap_or(&rev).to_string(),
            None,
        ),
        None => {
            return Err(ApiError::validation(
                "Deployment",
                "ref",
                "invalid",
                &format!("No ref found for: {rev}"),
            ));
        }
    };
    let required = body
        .get("required_contexts")
        .map(|_| DeploymentRequiredContexts {
            contexts: strs(&body, "required_contexts").unwrap_or_default(),
        });
    let payload = match body.get("payload") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "{}".into(),
    };
    let environment = s(&body, "environment").unwrap_or("production").to_string();
    let d = ci(&cx)?
        .create_deployment(cx.req(CreateDeploymentRequest {
            repo: rid(&repo.id),
            r#ref,
            changeset,
            task: s(&body, "task").unwrap_or("deploy").to_string(),
            environment: environment.clone(),
            description: s(&body, "description").unwrap_or("").to_string(),
            payload_json: payload,
            transient_environment: b(&body, "transient_environment").unwrap_or(false),
            production_environment:
                b(&body, "production_environment").unwrap_or(environment == "production"),
            auto_merge: b(&body, "auto_merge").unwrap_or(false),
            required_contexts: required,
            deploy_green_tip: false,
        }))
        .await
        .map_err(|st| {
            let e = ApiError::from(st);
            if e.reason == bee_proto::v1::ErrorReason::PolicyDenied {
                e.with_status(409)
            } else {
                e
            }
        })?
        .into_inner();
    Ok(Reply::created(deployment_json(&cx, &repo, &d).await))
}

/// `DELETE /repos/{owner}/{repo}/deployments/{id}`.
pub(crate) async fn delete_deployment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let d = deployment_by_num(&cx, &repo, p.num("id")?).await?;
    ci(&cx)?
        .delete_deployment(cx.req(DeleteDeploymentRequest {
            repo: rid(&repo.id),
            deployment_id: d.id,
        }))
        .await?;
    Ok(Reply::no_content())
}

async fn deployment_status_json(
    cx: &Ctx,
    repo: &Repo,
    dep_num: u64,
    st: &DeploymentStatus,
) -> Value {
    let id = cx.hub.ids.remember("deployment_status", &st.id, &repo.id);
    let dep = cx.api(&format!("/repos/{}/deployments/{dep_num}", repo.full_name));
    json!({
        "url": format!("{dep}/statuses/{id}"),
        "id": id,
        "node_id": crate::ids::node("DeploymentStatus", &st.id),
        "state": deployment_state_word(DeploymentState::try_from(st.state).unwrap_or_default()),
        "creator": shapes::user(cx, st.creator.as_ref().unwrap_or(&Principal::default())).await,
        "description": st.description,
        "environment": st.environment,
        "target_url": st.log_url,
        "log_url": st.log_url,
        "environment_url": st.environment_url,
        "created_at": iso(&st.created_at),
        "updated_at": iso(&st.created_at),
        "deployment_url": dep,
        "repository_url": cx.api(&format!("/repos/{}", repo.full_name)),
        "performed_via_github_app": Value::Null,
    })
}

/// `GET /repos/{owner}/{repo}/deployments/{id}/statuses`.
pub(crate) async fn deployment_statuses(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let n = p.num("id")?;
    let d = deployment_by_num(&cx, &repo, n).await?;
    let svc = ci(&cx)?.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListDeploymentStatusesRequest {
            repo: rid(&repo.id),
            deployment_id: d.id.clone(),
            page: Some(page),
        });
        async move {
            let r = svc.list_deployment_statuses(req).await?.into_inner();
            Ok((r.statuses, r.page))
        }
    })
    .await?;
    let mut sorted = list;
    sorted.sort_by(|a, b| {
        let t = |x: &DeploymentStatus| {
            x.created_at
                .as_ref()
                .map(|t| (t.seconds, t.nanos))
                .unwrap_or((0, 0))
        };
        t(b).cmp(&t(a)).then(b.id.cmp(&a.id))
    });
    let mut items = Vec::new();
    for st in &sorted {
        items.push(deployment_status_json(&cx, &repo, n, st).await);
    }
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/deployments/{id}/statuses/{status}`.
pub(crate) async fn deployment_status(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let n = p.num("id")?;
    let d = deployment_by_num(&cx, &repo, n).await?;
    let want = p.num("status")?;
    let svc = ci(&cx)?.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListDeploymentStatusesRequest {
            repo: rid(&repo.id),
            deployment_id: d.id.clone(),
            page: Some(page),
        });
        async move {
            let r = svc.list_deployment_statuses(req).await?.into_inner();
            Ok((r.statuses, r.page))
        }
    })
    .await?;
    let st = list
        .into_iter()
        .find(|s| crate::ids::num("deployment_status", &s.id) == want)
        .ok_or_else(ApiError::not_found)?;
    Ok(Reply::json(
        deployment_status_json(&cx, &repo, n, &st).await,
    ))
}

/// `POST /repos/{owner}/{repo}/deployments/{id}/statuses`.
pub(crate) async fn create_deployment_status(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let n = p.num("id")?;
    let d = deployment_by_num(&cx, &repo, n).await?;
    let body = cx.body_json()?;
    let state = match need(&body, "state", "DeploymentStatus")? {
        "pending" => DeploymentState::Pending,
        "queued" => DeploymentState::Queued,
        "in_progress" => DeploymentState::InProgress,
        "success" => DeploymentState::Success,
        "failure" => DeploymentState::Failure,
        "error" => DeploymentState::Error,
        "inactive" => DeploymentState::Inactive,
        other => {
            return Err(ApiError::validation(
                "DeploymentStatus",
                "state",
                "invalid",
                other,
            ));
        }
    };
    let st = ci(&cx)?
        .create_deployment_status(
            cx.req(CreateDeploymentStatusRequest {
                repo: rid(&repo.id),
                deployment_id: d.id,
                state: state as i32,
                description: s(&body, "description").unwrap_or("").to_string(),
                environment: s(&body, "environment").unwrap_or("").to_string(),
                log_url: s(&body, "log_url")
                    .or(s(&body, "target_url"))
                    .unwrap_or("")
                    .to_string(),
                environment_url: s(&body, "environment_url").unwrap_or("").to_string(),
                auto_inactive: b(&body, "auto_inactive").unwrap_or(true),
            }),
        )
        .await?
        .into_inner();
    Ok(Reply::created(
        deployment_status_json(&cx, &repo, n, &st).await,
    ))
}
