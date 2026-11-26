//! Commit statuses (combined and per context), check runs and check suites.

use std::collections::BTreeMap;
use std::sync::Arc;

use bee_proto::v1::check_service_server::CheckService as _;
use bee_proto::v1::{
    AnnotationLevel, CheckAnnotation, CheckOutput, CheckRun, CheckStatus, CheckSuite, CommitStatus,
    CommitStatusState, CreateCheckSuiteRequest, CreateCommitStatusRequest, GetCheckRunRequest,
    ListCheckAnnotationsRequest, ListCheckSuitesRequest, ListChecksRequest,
    ListCommitStatusesRequest, ObjectId, Principal, Repo, ReportCheckRequest,
    RerequestCheckRunRequest, RerequestCheckSuiteRequest, UpdateCheckRunRequest,
};
use serde_json::{Value, json};

use super::{need, repo_of, rid, s, u};
use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Reply, all_pages};
use crate::shapes;
use crate::time::{iso, parse_iso, ts_secs};

/// The changeset and git id of a ref or sha.
async fn changeset_of(cx: &Ctx, repo: &Repo, rev: &str) -> ApiResult<(ObjectId, String)> {
    let sha = super::git::resolve(cx, repo, rev).await?;
    let cs = cx
        .hub
        .ports
        .git
        .changeset(repo, &sha)
        .await?
        .ok_or_else(|| ApiError::unprocessable(format!("No commit found for SHA: {rev}")))?;
    Ok((cs, sha))
}

/// GitHub's `(status, conclusion)` for a check status.
pub(crate) fn check_words(st: CheckStatus) -> (&'static str, Option<&'static str>) {
    match st {
        CheckStatus::Queued | CheckStatus::Unspecified => ("queued", None),
        CheckStatus::Running => ("in_progress", None),
        CheckStatus::Passed => ("completed", Some("success")),
        CheckStatus::Failed => ("completed", Some("failure")),
        CheckStatus::Skipped => ("completed", Some("skipped")),
        CheckStatus::Cancelled => ("completed", Some("cancelled")),
        CheckStatus::TimedOut => ("completed", Some("timed_out")),
        CheckStatus::Neutral => ("completed", Some("neutral")),
        CheckStatus::ActionRequired => ("completed", Some("action_required")),
        CheckStatus::Stale => ("completed", Some("stale")),
    }
}

fn status_of(status: Option<&str>, conclusion: Option<&str>) -> ApiResult<Option<CheckStatus>> {
    if let Some(c) = conclusion {
        return Ok(Some(match c {
            "success" => CheckStatus::Passed,
            "failure" => CheckStatus::Failed,
            "skipped" => CheckStatus::Skipped,
            "cancelled" => CheckStatus::Cancelled,
            "timed_out" => CheckStatus::TimedOut,
            "neutral" => CheckStatus::Neutral,
            "action_required" => CheckStatus::ActionRequired,
            "stale" => CheckStatus::Stale,
            other => {
                return Err(ApiError::validation(
                    "CheckRun",
                    "conclusion",
                    "invalid",
                    other,
                ));
            }
        }));
    }
    Ok(match status {
        Some("queued") | Some("requested") | Some("waiting") | Some("pending") => {
            Some(CheckStatus::Queued)
        }
        Some("in_progress") => Some(CheckStatus::Running),
        Some("completed") => {
            return Err(ApiError::validation(
                "CheckRun",
                "conclusion",
                "missing_field",
                "conclusion is required when status is completed",
            ));
        }
        Some(other) => return Err(ApiError::validation("CheckRun", "status", "invalid", other)),
        None => None,
    })
}

async fn app_json(cx: &Ctx, p: &Principal) -> Value {
    let login = cx.login(p).await;
    json!({
        "id": crate::ids::num("app", &p.id),
        "slug": login,
        "node_id": crate::ids::node("App", &p.id),
        "owner": Value::Null,
        "name": if p.display.is_empty() { login.clone() } else { p.display.clone() },
        "description": "",
        "external_url": cx.html(&format!("/{login}")),
        "html_url": cx.html(&format!("/apps/{login}")),
        "created_at": Value::Null,
        "updated_at": Value::Null,
        "permissions": {},
        "events": [],
    })
}

/// GitHub's check run object.
pub(crate) async fn run_json(cx: &Ctx, repo: &Repo, r: &CheckRun) -> Value {
    let id = cx.hub.ids.remember("check_run", &r.id, &repo.id);
    let (status, conclusion) = check_words(CheckStatus::try_from(r.status).unwrap_or_default());
    let sha = match &r.changeset {
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
    let url = cx.api(&format!("/repos/{}/check-runs/{id}", repo.full_name));
    let out = r.output.clone().unwrap_or_default();
    let reporter = r.reporter.clone().unwrap_or_default();
    json!({
        "id": id,
        "head_sha": sha,
        "node_id": crate::ids::node("CheckRun", &r.id),
        "external_id": r.external_id,
        "url": url,
        "html_url": if r.url.is_empty() { cx.html(&format!("/{}/runs/{id}", repo.full_name)) } else { r.url.clone() },
        "details_url": if r.url.is_empty() { Value::Null } else { json!(r.url) },
        "status": status,
        "conclusion": conclusion,
        "started_at": iso(&r.started_at),
        "completed_at": iso(&r.finished_at),
        "output": {
            "title": if out.title.is_empty() { Value::Null } else { json!(out.title) },
            "summary": if out.summary.is_empty() { if r.summary.is_empty() { Value::Null } else { json!(r.summary) } } else { json!(out.summary) },
            "text": if out.text.is_empty() { Value::Null } else { json!(out.text) },
            "annotations_count": r.annotations_count,
            "annotations_url": format!("{url}/annotations"),
        },
        "name": r.name,
        "check_suite": if r.suite_id.is_empty() { Value::Null } else { json!({"id": cx.hub.ids.remember("check_suite", &r.suite_id, &repo.id)}) },
        "app": app_json(cx, &reporter).await,
        "pull_requests": [],
    })
}

async fn runs_for(
    cx: &Ctx,
    repo: &Repo,
    cs: Option<ObjectId>,
    suite: &str,
) -> ApiResult<Vec<CheckRun>> {
    Ok(cx
        .hub
        .svc
        .list_checks(cx.req(ListChecksRequest {
            repo: rid(&repo.id),
            changeset: cs,
            name: cx.q("check_name").unwrap_or("").to_string(),
            suite_id: suite.to_string(),
            latest_only: cx.q("filter") != Some("all"),
        }))
        .await?
        .into_inner()
        .checks
        .into_iter()
        .filter(|r| !is_status_shim(r))
        .collect())
}

/// Whether a check run is the forge's mirror of a commit status (`external_id` =
/// `status:<context>`): statuses are listed as statuses, not twice.
pub(crate) fn is_status_shim(r: &CheckRun) -> bool {
    r.external_id.starts_with("status:")
}

/// Check runs of a changeset, filtered like GitHub's list.
async fn runs_reply(cx: &Ctx, repo: &Repo, runs: Vec<CheckRun>) -> ApiResult<Reply> {
    let want = cx.q("status");
    let mut items = Vec::new();
    for r in runs {
        let (st, _) = check_words(CheckStatus::try_from(r.status).unwrap_or_default());
        if want.is_none_or(|w| w == st) {
            items.push(run_json(cx, repo, &r).await);
        }
    }
    Ok(cx.paginate_wrapped("check_runs", items, json!({})))
}

/// `GET /repos/{owner}/{repo}/commits/{ref}/check-runs`.
pub(crate) async fn runs_for_ref(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (cs, _) = changeset_of(&cx, &repo, p.get("ref")).await?;
    let runs = runs_for(&cx, &repo, Some(cs), "").await?;
    runs_reply(&cx, &repo, runs).await
}

fn output_of(v: Option<&Value>) -> Option<CheckOutput> {
    let v = v?;
    let annotations = v
        .get("annotations")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|x| CheckAnnotation {
                    path: s(x, "path").unwrap_or("").as_bytes().to_vec(),
                    start_line: u(x, "start_line").unwrap_or(0) as u32,
                    end_line: u(x, "end_line").or_else(|| u(x, "start_line")).unwrap_or(0) as u32,
                    start_column: u(x, "start_column").unwrap_or(0) as u32,
                    end_column: u(x, "end_column").unwrap_or(0) as u32,
                    level: match s(x, "annotation_level") {
                        Some("failure") => AnnotationLevel::Failure as i32,
                        Some("warning") => AnnotationLevel::Warning as i32,
                        _ => AnnotationLevel::Notice as i32,
                    },
                    message: s(x, "message").unwrap_or("").to_string(),
                    title: s(x, "title").unwrap_or("").to_string(),
                    raw_details: s(x, "raw_details").unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(CheckOutput {
        title: s(v, "title").unwrap_or("").to_string(),
        summary: s(v, "summary").unwrap_or("").to_string(),
        text: s(v, "text").unwrap_or("").to_string(),
        annotations,
        images: Vec::new(),
    })
}

fn actions_of(v: &Value) -> Vec<bee_proto::v1::CheckAction> {
    v.get("actions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|x| bee_proto::v1::CheckAction {
                    label: s(x, "label").unwrap_or("").to_string(),
                    description: s(x, "description").unwrap_or("").to_string(),
                    identifier: s(x, "identifier").unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `POST /repos/{owner}/{repo}/check-runs`.
pub(crate) async fn create_run(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let name = need(&body, "name", "CheckRun")?.to_string();
    let head = need(&body, "head_sha", "CheckRun")?.to_string();
    let (cs, _) = changeset_of(&cx, &repo, &head).await?;
    let status =
        status_of(s(&body, "status"), s(&body, "conclusion"))?.unwrap_or(CheckStatus::Queued);
    let r = cx
        .hub
        .svc
        .report_check(cx.req(ReportCheckRequest {
            repo: rid(&repo.id),
            changeset: Some(cs),
            name,
            status: status as i32,
            url: s(&body, "details_url").unwrap_or("").to_string(),
            summary: String::new(),
            post_land: false,
            external_id: s(&body, "external_id").unwrap_or("").to_string(),
            output: output_of(body.get("output")),
            suite_id: String::new(),
            actions: actions_of(&body),
            started_at: s(&body, "started_at").and_then(parse_iso).map(ts_secs),
            finished_at: s(&body, "completed_at").and_then(parse_iso).map(ts_secs),
        }))
        .await?
        .into_inner();
    Ok(Reply::created(run_json(&cx, &repo, &r).await))
}

async fn run_id(cx: &Ctx, repo: &Repo, n: u64) -> ApiResult<String> {
    if let Some(ix) = cx.hub.ids.get("check_run", n) {
        return Ok(ix.id);
    }
    let all = cx
        .hub
        .svc
        .list_checks(cx.req(ListChecksRequest {
            repo: rid(&repo.id),
            ..Default::default()
        }))
        .await?
        .into_inner()
        .checks;
    all.into_iter()
        .find(|r| crate::ids::num("check_run", &r.id) == n)
        .map(|r| r.id)
        .ok_or_else(ApiError::not_found)
}

/// `GET /repos/{owner}/{repo}/check-runs/{id}`.
pub(crate) async fn get_run(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    let r = cx
        .hub
        .svc
        .get_check_run(cx.req(GetCheckRunRequest {
            repo: rid(&repo.id),
            id,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(run_json(&cx, &repo, &r).await))
}

/// `PATCH /repos/{owner}/{repo}/check-runs/{id}`.
pub(crate) async fn update_run(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    let body = cx.body_json()?;
    let status = status_of(s(&body, "status"), s(&body, "conclusion"))?;
    let r = cx
        .hub
        .svc
        .update_check_run(cx.req(UpdateCheckRunRequest {
            repo: rid(&repo.id),
            id,
            status: status.map(|x| x as i32),
            url: s(&body, "details_url").map(str::to_string),
            summary: None,
            output: output_of(body.get("output")),
            actions: actions_of(&body),
            finished_at: s(&body, "completed_at").and_then(parse_iso).map(ts_secs),
        }))
        .await?
        .into_inner();
    Ok(Reply::json(run_json(&cx, &repo, &r).await))
}

/// `GET /repos/{owner}/{repo}/check-runs/{id}/annotations`.
pub(crate) async fn annotations(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListCheckAnnotationsRequest {
            repo: rid(&repo.id),
            check_run_id: id.clone(),
            page: Some(page),
        });
        async move {
            let r = svc.list_check_annotations(req).await?.into_inner();
            Ok((r.annotations, r.page))
        }
    })
    .await?;
    let items = list
        .iter()
        .map(|a| {
            json!({
                "path": String::from_utf8_lossy(&a.path),
                "start_line": a.start_line,
                "end_line": a.end_line,
                "start_column": if a.start_column == 0 { Value::Null } else { json!(a.start_column) },
                "end_column": if a.end_column == 0 { Value::Null } else { json!(a.end_column) },
                "annotation_level": match AnnotationLevel::try_from(a.level).unwrap_or_default() {
                    AnnotationLevel::Failure => "failure",
                    AnnotationLevel::Warning => "warning",
                    _ => "notice",
                },
                "title": a.title,
                "message": a.message,
                "raw_details": a.raw_details,
                "blob_href": "",
            })
        })
        .collect();
    Ok(cx.paginate(items))
}

/// `POST /repos/{owner}/{repo}/check-runs/{id}/rerequest`.
pub(crate) async fn rerequest_run(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let id = run_id(&cx, &repo, p.num("id")?).await?;
    cx.hub
        .svc
        .rerequest_check_run(cx.req(RerequestCheckRunRequest {
            repo: rid(&repo.id),
            id,
        }))
        .await?;
    Ok(Reply::created(json!({})))
}

async fn suite_json(cx: &Ctx, repo: &Repo, su: &CheckSuite) -> Value {
    let id = cx.hub.ids.remember("check_suite", &su.id, &repo.id);
    let (status, conclusion) = check_words(CheckStatus::try_from(su.status).unwrap_or_default());
    let sha = match &su.changeset {
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
    let url = cx.api(&format!("/repos/{}/check-suites/{id}", repo.full_name));
    json!({
        "id": id,
        "node_id": crate::ids::node("CheckSuite", &su.id),
        "head_branch": if su.head_ref.is_empty() { Value::Null } else { json!(su.head_ref) },
        "head_sha": sha,
        "status": status,
        "conclusion": conclusion,
        "url": url,
        "before": Value::Null,
        "after": sha,
        "pull_requests": [],
        "app": json!({"id": crate::ids::num("app", &su.app_id), "slug": su.app_id, "name": su.app_id}),
        "repository": shapes::repo(cx, repo, None, None),
        "created_at": iso(&su.created_at),
        "updated_at": iso(&su.updated_at),
        "latest_check_runs_count": su.runs_count,
        "check_runs_url": format!("{url}/check-runs"),
        "rerequestable": true,
        "runs_rerequestable": true,
    })
}

/// `GET /repos/{owner}/{repo}/commits/{ref}/check-suites`.
pub(crate) async fn suites_for_ref(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (cs, _) = changeset_of(&cx, &repo, p.get("ref")).await?;
    let suites = cx
        .hub
        .svc
        .list_check_suites(cx.req(ListCheckSuitesRequest {
            repo: rid(&repo.id),
            changeset: Some(cs),
            app_id: String::new(),
        }))
        .await?
        .into_inner()
        .suites;
    let mut items = Vec::new();
    for su in &suites {
        items.push(suite_json(&cx, &repo, su).await);
    }
    Ok(cx.paginate_wrapped("check_suites", items, json!({})))
}

/// `POST /repos/{owner}/{repo}/check-suites`.
pub(crate) async fn create_suite(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let head = need(&body, "head_sha", "CheckSuite")?.to_string();
    let (cs, _) = changeset_of(&cx, &repo, &head).await?;
    let su = cx
        .hub
        .svc
        .create_check_suite(cx.req(CreateCheckSuiteRequest {
            repo: rid(&repo.id),
            changeset: Some(cs),
            head_ref: String::new(),
        }))
        .await?
        .into_inner();
    Ok(Reply::created(suite_json(&cx, &repo, &su).await))
}

async fn suite_by_num(cx: &Ctx, repo: &Repo, n: u64) -> ApiResult<CheckSuite> {
    let all = cx
        .hub
        .svc
        .list_check_suites(cx.req(ListCheckSuitesRequest {
            repo: rid(&repo.id),
            changeset: None,
            app_id: String::new(),
        }))
        .await?
        .into_inner()
        .suites;
    all.into_iter()
        .find(|su| crate::ids::num("check_suite", &su.id) == n)
        .ok_or_else(ApiError::not_found)
}

/// `GET /repos/{owner}/{repo}/check-suites/{id}`.
pub(crate) async fn get_suite(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let su = suite_by_num(&cx, &repo, p.num("id")?).await?;
    Ok(Reply::json(suite_json(&cx, &repo, &su).await))
}

/// `GET /repos/{owner}/{repo}/check-suites/{id}/check-runs`.
pub(crate) async fn suite_runs(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let su = suite_by_num(&cx, &repo, p.num("id")?).await?;
    let runs = runs_for(&cx, &repo, su.changeset.clone(), &su.id).await?;
    runs_reply(&cx, &repo, runs).await
}

/// `POST /repos/{owner}/{repo}/check-suites/{id}/rerequest`.
pub(crate) async fn rerequest_suite(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let su = suite_by_num(&cx, &repo, p.num("id")?).await?;
    cx.hub
        .svc
        .rerequest_check_suite(cx.req(RerequestCheckSuiteRequest {
            repo: rid(&repo.id),
            id: su.id,
        }))
        .await?;
    Ok(Reply::created(json!({})))
}

/// GitHub's word for a status state.
pub(crate) fn state_word(st: CommitStatusState) -> &'static str {
    match st {
        CommitStatusState::Success => "success",
        CommitStatusState::Failure => "failure",
        CommitStatusState::Error => "error",
        _ => "pending",
    }
}

async fn status_json(cx: &Ctx, repo: &Repo, st: &CommitStatus) -> Value {
    let id = cx.hub.ids.remember("status", &st.id, &repo.id);
    json!({
        "url": cx.api(&format!("/repos/{}/statuses/{id}", repo.full_name)),
        "avatar_url": Value::Null,
        "id": id,
        "node_id": crate::ids::node("StatusContext", &st.id),
        "state": state_word(CommitStatusState::try_from(st.state).unwrap_or_default()),
        "description": if st.description.is_empty() { Value::Null } else { json!(st.description) },
        "target_url": if st.target_url.is_empty() { Value::Null } else { json!(st.target_url) },
        "context": st.context,
        "created_at": iso(&st.created_at),
        "updated_at": crate::time::iso_or(&st.updated_at, &st.created_at),
        "creator": shapes::user(cx, st.creator.as_ref().unwrap_or(&Principal::default())).await,
    })
}

/// Every status of a changeset, newest first.
pub(crate) async fn statuses_of(
    cx: &Ctx,
    repo: &Repo,
    cs: &ObjectId,
) -> ApiResult<Vec<CommitStatus>> {
    let svc = cx.hub.svc.clone();
    let mut list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListCommitStatusesRequest {
            repo: rid(&repo.id),
            changeset: Some(cs.clone()),
            page: Some(page),
        });
        async move {
            let r = svc.list_commit_statuses(req).await?.into_inner();
            Ok((r.statuses, r.page))
        }
    })
    .await?;
    list.sort_by(|a, b| {
        let t = |x: &CommitStatus| {
            x.updated_at
                .as_ref()
                .or(x.created_at.as_ref())
                .map(|t| (t.seconds, t.nanos))
                .unwrap_or((0, 0))
        };
        t(b).cmp(&t(a)).then(b.id.cmp(&a.id))
    });
    Ok(list)
}

/// The latest status per context and the combined state.
pub(crate) fn combine(list: &[CommitStatus]) -> (CommitStatusState, Vec<CommitStatus>) {
    let mut latest: BTreeMap<String, CommitStatus> = BTreeMap::new();
    for st in list {
        latest
            .entry(st.context.clone())
            .or_insert_with(|| st.clone());
    }
    let states: Vec<CommitStatusState> = latest
        .values()
        .map(|s| CommitStatusState::try_from(s.state).unwrap_or_default())
        .collect();
    let state = if states
        .iter()
        .any(|s| matches!(s, CommitStatusState::Failure | CommitStatusState::Error))
    {
        CommitStatusState::Failure
    } else if states.is_empty() || states.iter().any(|s| *s != CommitStatusState::Success) {
        CommitStatusState::Pending
    } else {
        CommitStatusState::Success
    };
    (state, latest.into_values().collect())
}

/// `POST /repos/{owner}/{repo}/statuses/{sha}`.
pub(crate) async fn create_status(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let state = match need(&body, "state", "Status")? {
        "pending" => CommitStatusState::Pending,
        "success" => CommitStatusState::Success,
        "failure" => CommitStatusState::Failure,
        "error" => CommitStatusState::Error,
        other => return Err(ApiError::validation("Status", "state", "invalid", other)),
    };
    let (cs, _) = changeset_of(&cx, &repo, p.get("sha")).await?;
    let st = cx
        .hub
        .svc
        .create_commit_status(cx.req(CreateCommitStatusRequest {
            repo: rid(&repo.id),
            changeset: Some(cs),
            context: s(&body, "context").unwrap_or("default").to_string(),
            state: state as i32,
            target_url: s(&body, "target_url").unwrap_or("").to_string(),
            description: s(&body, "description").unwrap_or("").to_string(),
        }))
        .await?
        .into_inner();
    Ok(Reply::created(status_json(&cx, &repo, &st).await))
}

/// `GET /repos/{owner}/{repo}/commits/{ref}/statuses` and `/statuses/{sha}`.
pub(crate) async fn statuses(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let rev = if p.get("ref").is_empty() {
        p.get("sha")
    } else {
        p.get("ref")
    };
    let (cs, _) = changeset_of(&cx, &repo, rev).await?;
    let mut items = Vec::new();
    for st in statuses_of(&cx, &repo, &cs).await? {
        items.push(status_json(&cx, &repo, &st).await);
    }
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/commits/{ref}/status`.
pub(crate) async fn combined_status(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (cs, sha) = changeset_of(&cx, &repo, p.get("ref")).await?;
    let list = statuses_of(&cx, &repo, &cs).await?;
    let (state, latest) = combine(&list);
    let mut items = Vec::new();
    for st in &latest {
        items.push(status_json(&cx, &repo, st).await);
    }
    let base = cx.api(&format!("/repos/{}/commits/{sha}", repo.full_name));
    Ok(Reply::json(json!({
        "state": state_word(state),
        "statuses": items,
        "sha": sha,
        "total_count": latest.len(),
        "repository": shapes::repo(&cx, &repo, None, None),
        "commit_url": base,
        "url": format!("{base}/status"),
    })))
}
