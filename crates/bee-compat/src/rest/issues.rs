//! Issues (pull requests included, as GitHub's issues API shows them), comments, labels,
//! milestones, assignees, events, timeline and reactions.

use std::sync::Arc;

use bee_forge::Cx;
use bee_forge::work::{Actor as WorkActor, RepoInfo};
use bee_policy::Action;
use bee_proto::v1::change_service_server::ChangeService as _;
use bee_proto::v1::issue_service_server::IssueService as _;
use bee_proto::v1::{
    AddIssueAssigneesRequest, AddIssueLabelsRequest, ChangeRequest, CloseIssueRequest, Comment,
    CreateChangeCommentRequest, CreateIssueCommentRequest, CreateIssueLabelRequest,
    CreateIssueMilestoneRequest, CreateIssueRequest, DeleteIssueLabelRequest,
    DeleteIssueMilestoneRequest, GetIssueByNumberRequest, GetIssueLabelRequest,
    GetIssueMilestoneRequest, Issue, IssueLockReason, IssueSort, IssueState, IssueStateReason,
    IssueTimelineEvent, IssueTimelineEventKind, ListChangeCommentsRequest,
    ListIssueAssignableUsersRequest, ListIssueCommentsRequest, ListIssueLabelsRequest,
    ListIssueMilestonesRequest, ListIssueTimelineRequest, ListIssuesRequest, LockIssueRequest,
    Milestone, MilestoneState, ReactionContent, RemoveIssueAssigneesRequest,
    RemoveIssueLabelsRequest, ReopenIssueRequest, Repo, SetChangeAssigneesRequest,
    SetChangeLabelsRequest, SetChangeMilestoneRequest, SetIssueLabelsRequest,
    SetIssueMilestoneRequest, UnlockIssueRequest, UpdateIssueLabelRequest,
    UpdateIssueMilestoneRequest, UpdateIssueRequest,
};
use serde_json::{Value, json};

use super::pulls::{cr_by_number, list_crs};
use super::{changes, need, repo_of, rid, s, strs, u};
use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Reply, all_pages};
use crate::shapes;
use crate::time::{iso, parse_iso, ts_secs};

/// An issue number's subject: an issue or a pull request.
pub(crate) enum Subject {
    Issue(Box<Issue>),
    Pull(Box<ChangeRequest>),
}

/// The issue or PR with number `n`.
pub(crate) async fn subject(cx: &Ctx, repo: &Repo, n: u64) -> ApiResult<Subject> {
    match cx
        .hub
        .svc
        .get_issue_by_number(cx.req(GetIssueByNumberRequest {
            repo: rid(&repo.id),
            number: n,
            include_html: false,
        }))
        .await
    {
        Ok(i) => Ok(Subject::Issue(Box::new(i.into_inner()))),
        Err(st) => {
            let e = ApiError::from(st);
            if e.status != 404 {
                return Err(e);
            }
            match cr_by_number(cx, repo, n).await? {
                Some(c) => Ok(Subject::Pull(Box::new(c))),
                None => Err(ApiError::not_found()),
            }
        }
    }
}

async fn subject_json(cx: &Ctx, repo: &Repo, sub: &Subject) -> Value {
    match sub {
        Subject::Issue(i) => shapes::issue(cx, repo, i).await,
        Subject::Pull(c) => shapes::issue_of_pull(cx, repo, c).await,
    }
}

/// Authorizes `action` on `repo` for the caller and returns the forge context.
pub(crate) async fn authorize(cx: &Ctx, repo: &Repo, action: Action) -> ApiResult<Cx> {
    let caller = cx.caller()?;
    Ok(cx
        .hub
        .forge
        .authz
        .repo(caller, repo, action, &[], None, None)
        .await?)
}

/// The issues module's actor for the caller.
async fn work_actor(cx: &Ctx, repo: &Repo, fcx: &Cx) -> ApiResult<WorkActor> {
    let caller = cx.caller()?;
    let mut principal = cx.principal(&fcx.actor_id).await;
    if principal.kind == 0 {
        principal.kind = fcx.actor_kind as i32;
    }
    let authz = &cx.hub.forge.authz;
    let triage = authz
        .repo(caller, repo, Action::Triage, &[], None, None)
        .await
        .is_ok();
    let admin = authz
        .repo(
            caller,
            repo,
            Action::ManageSettings,
            &[],
            None,
            Some(bee_proto::v1::RepoRole::Admin),
        )
        .await
        .is_ok();
    let association = match cx
        .hub
        .forge
        .orgs
        .member(fcx, &repo.org_id, &fcx.actor_id)
        .await?
    {
        Some(m) if m.role == bee_proto::v1::Role::Owner as i32 => "OWNER",
        Some(_) => "MEMBER",
        None if triage => "COLLABORATOR",
        None => "NONE",
    };
    Ok(WorkActor {
        principal,
        triage,
        admin,
        association: association.into(),
    })
}

/// Every issue of a repo for a GitHub list request.
async fn list_issues(cx: &Ctx, repo: &Repo, state: &str) -> ApiResult<Vec<Issue>> {
    let svc = cx.hub.svc.clone();
    let assignee = match cx.q("assignee") {
        Some("none") => "none".to_string(),
        Some("*") => "*".to_string(),
        Some(l) => cx
            .by_login(l)
            .await
            .map(|p| p.id)
            .unwrap_or_else(|_| l.to_string()),
        None => String::new(),
    };
    let author = match cx.q("creator") {
        Some(l) => cx
            .by_login(l)
            .await
            .map(|p| p.id)
            .unwrap_or_else(|_| l.to_string()),
        None => String::new(),
    };
    let mentioned = cx.q("mentioned").unwrap_or("").to_string();
    let labels: Vec<String> = cx
        .q("labels")
        .map(|l| {
            l.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let milestone = cx.q("milestone").unwrap_or("").to_string();
    let since = cx.q("since").and_then(parse_iso).map(ts_secs);
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListIssuesRequest {
            repo: rid(&repo.id),
            state: match state {
                "closed" => IssueState::Closed as i32,
                _ => IssueState::Open as i32,
            },
            all_states: state == "all",
            labels: labels.clone(),
            assignee: assignee.clone(),
            author: author.clone(),
            mentioned: mentioned.clone(),
            milestone: milestone.clone(),
            since,
            sort: IssueSort::Created as i32,
            page: Some(page),
            ..Default::default()
        });
        async move {
            let r = svc.list_issues(req).await?.into_inner();
            Ok((r.issues, r.page))
        }
    })
    .await
}

/// PRs of a repo filtered like the issues list.
async fn list_pulls_as_issues(cx: &Ctx, repo: &Repo, state: &str) -> ApiResult<Vec<ChangeRequest>> {
    if cx.hub.changes.is_none() || cx.q("mentioned").is_some() {
        return Ok(Vec::new());
    }
    let mut crs = list_crs(cx, repo, state, None, "").await?;
    if let Some(l) = cx.q("labels") {
        let want: Vec<&str> = l
            .split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .collect();
        crs.retain(|c| {
            want.iter()
                .all(|w| c.labels.iter().any(|x| x.name.eq_ignore_ascii_case(w)))
        });
    }
    match cx.q("assignee") {
        Some("none") => crs.retain(|c| c.assignees.is_empty()),
        Some("*") => crs.retain(|c| !c.assignees.is_empty()),
        Some(l) => {
            let id = cx.by_login(l).await.map(|p| p.id).unwrap_or_default();
            crs.retain(|c| c.assignees.iter().any(|a| a.id == id));
        }
        None => {}
    }
    if let Some(l) = cx.q("creator") {
        let id = cx.by_login(l).await.map(|p| p.id).unwrap_or_default();
        crs.retain(|c| c.author.as_ref().is_some_and(|a| a.id == id));
    }
    match cx.q("milestone") {
        Some("none") => crs.retain(|c| c.milestone.is_none()),
        Some("*") => crs.retain(|c| c.milestone.is_some()),
        Some(n) => crs.retain(|c| {
            c.milestone
                .as_ref()
                .is_some_and(|m| m.number.to_string() == n || m.title == n)
        }),
        None => {}
    }
    if let Some(since) = cx.q("since").and_then(parse_iso) {
        crs.retain(|c| c.updated_at.as_ref().map(|t| t.seconds).unwrap_or(0) >= since);
    }
    Ok(crs)
}

fn sort_key(cx: &Ctx, created: i64, updated: i64, comments: u32) -> i64 {
    match cx.q("sort") {
        Some("updated") => updated,
        Some("comments") => comments as i64,
        _ => created,
    }
}

/// Issues and PRs of a repo as GitHub's issue list, sorted and cut to one page.
async fn issues_page(cx: &Ctx, repo: &Repo) -> ApiResult<Reply> {
    let state = cx.q("state").unwrap_or("open");
    let issues = list_issues(cx, repo, state).await?;
    let pulls = list_pulls_as_issues(cx, repo, state).await?;
    let mut keyed: Vec<(i64, u64, Subject)> = Vec::new();
    for i in issues {
        let k = sort_key(
            cx,
            i.created_at.as_ref().map(|t| t.seconds).unwrap_or(0),
            i.updated_at.as_ref().map(|t| t.seconds).unwrap_or(0),
            i.comment_count,
        );
        keyed.push((k, i.number, Subject::Issue(Box::new(i))));
    }
    for c in pulls {
        let k = sort_key(
            cx,
            c.created_at.as_ref().map(|t| t.seconds).unwrap_or(0),
            c.updated_at.as_ref().map(|t| t.seconds).unwrap_or(0),
            c.comment_count,
        );
        keyed.push((k, c.number, Subject::Pull(Box::new(c))));
    }
    let asc = cx.q("direction") == Some("asc");
    keyed.sort_by(|a, b| {
        let o = (a.0, a.1).cmp(&(b.0, b.1));
        if asc { o } else { o.reverse() }
    });
    let total = keyed.len();
    let (page, per) = cx.page();
    let mut items = Vec::new();
    for (_, _, sub) in keyed.iter().skip((page - 1) * per).take(per) {
        items.push(subject_json(cx, repo, sub).await);
    }
    Ok(cx.paged(items, total))
}

/// `GET /repos/{owner}/{repo}/issues`.
pub(crate) async fn list(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    issues_page(&cx, &repo).await
}

/// `GET /issues` and `/user/issues`: issues assigned to the caller across visible repos.
pub(crate) async fn my_issues(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let me = cx.caller()?.cx.actor_id.clone();
    let filter = cx.q("filter").unwrap_or("assigned");
    let state = cx.q("state").unwrap_or("open");
    let mut items = Vec::new();
    for repo in super::repos::visible_repos(&cx).await? {
        for i in list_issues(&cx, &repo, state).await.unwrap_or_default() {
            let mine = match filter {
                "created" => i.author.as_ref().is_some_and(|a| a.id == me),
                "all" | "subscribed" | "repos" => true,
                _ => i.assignees.iter().any(|a| a.id == me),
            };
            if mine {
                let mut v = shapes::issue(&cx, &repo, &i).await;
                v["repository"] = shapes::repo(&cx, &repo, None, None);
                items.push(v);
            }
        }
    }
    Ok(cx.paginate(items))
}

/// `POST /repos/{owner}/{repo}/issues`.
pub(crate) async fn create(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let title = match body.get("title") {
        Some(Value::String(t)) if !t.is_empty() => t.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return Err(ApiError::missing_field("Issue", "title")),
    };
    let mut assignees = strs(&body, "assignees").unwrap_or_default();
    if let Some(a) = s(&body, "assignee") {
        assignees.push(a.to_string());
    }
    let mut ids = Vec::new();
    for a in &assignees {
        ids.push(cx.id_of_login(a, "assignees").await?);
    }
    let i = cx
        .hub
        .svc
        .create_issue(cx.req(CreateIssueRequest {
            repo: rid(&repo.id),
            title,
            body: s(&body, "body").unwrap_or("").to_string(),
            assignees: ids,
            labels: strs(&body, "labels").unwrap_or_default(),
            milestone_number: u(&body, "milestone").unwrap_or(0),
            issue_type: s(&body, "type").unwrap_or("").to_string(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    let v = shapes::issue(&cx, &repo, &i).await;
    let loc = v["url"].as_str().unwrap_or("").to_string();
    Ok(Reply::created(v).header("location", loc))
}

/// `GET /repos/{owner}/{repo}/issues/{number}`.
pub(crate) async fn get(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sub = subject(&cx, &repo, p.num("number")?).await?;
    Ok(Reply::json(subject_json(&cx, &repo, &sub).await))
}

pub(crate) async fn milestone_id(cx: &Ctx, repo: &Repo, number: u64) -> ApiResult<String> {
    Ok(cx
        .hub
        .svc
        .get_issue_milestone(cx.req(GetIssueMilestoneRequest {
            repo: rid(&repo.id),
            number,
        }))
        .await
        .map_err(|_| ApiError::validation("Issue", "milestone", "invalid", ""))?
        .into_inner()
        .id)
}

/// Applies an issue edit body to a PR (title, body, state, labels, assignees, milestone).
pub(crate) async fn update_pull(
    cx: &Ctx,
    repo: &Repo,
    mut c: ChangeRequest,
    body: &Value,
) -> ApiResult<ChangeRequest> {
    let svc = changes(cx)?;
    if s(body, "title").is_some() || s(body, "body").is_some() {
        c = svc
            .edit_change(cx.req(bee_proto::v1::EditChangeRequest {
                repo: rid(&repo.id),
                id: c.id.clone(),
                title: s(body, "title").map(str::to_string),
                body: s(body, "body").map(str::to_string),
                target_ref: None,
                depends_on: None,
                expected_version: 0,
                allow_maintainer_edits: None,
            }))
            .await?
            .into_inner();
    }
    match s(body, "state") {
        Some("closed") if shapes::cr_state(&c).0 == "open" => {
            c = svc
                .close_change(cx.req(bee_proto::v1::CloseChangeRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                    reason: String::new(),
                }))
                .await?
                .into_inner();
        }
        Some("open") if shapes::cr_state(&c) == ("closed", false) => {
            c = svc
                .reopen_change(cx.req(bee_proto::v1::ReopenChangeRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                }))
                .await?
                .into_inner();
        }
        _ => {}
    }
    if let Some(labels) = strs(body, "labels") {
        c = svc
            .set_change_labels(cx.req(SetChangeLabelsRequest {
                repo: rid(&repo.id),
                id: c.id.clone(),
                labels,
            }))
            .await?
            .into_inner();
    }
    if let Some(logins) = strs(body, "assignees") {
        let mut ids = Vec::new();
        for l in &logins {
            ids.push(cx.id_of_login(l, "assignees").await?);
        }
        c = svc
            .set_change_assignees(cx.req(SetChangeAssigneesRequest {
                repo: rid(&repo.id),
                id: c.id.clone(),
                assignee_ids: ids,
            }))
            .await?
            .into_inner();
    }
    if let Some(m) = body.get("milestone") {
        let id = match m.as_u64() {
            Some(n) => milestone_id(cx, repo, n).await?,
            None => String::new(),
        };
        c = svc
            .set_change_milestone(cx.req(SetChangeMilestoneRequest {
                repo: rid(&repo.id),
                id: c.id.clone(),
                milestone_id: id,
            }))
            .await?
            .into_inner();
    }
    Ok(c)
}

/// Applies an issue edit body.
pub(crate) async fn update_issue(
    cx: &Ctx,
    repo: &Repo,
    mut i: Issue,
    body: &Value,
) -> ApiResult<Issue> {
    let svc = &cx.hub.svc;
    let n = i.number;
    if s(body, "title").is_some() || body.get("body").is_some() {
        i = svc
            .update_issue(
                cx.req(UpdateIssueRequest {
                    repo: rid(&repo.id),
                    number: n,
                    title: s(body, "title").map(str::to_string),
                    body: body
                        .get("body")
                        .map(|b| b.as_str().unwrap_or("").to_string()),
                    expected_version: 0,
                }),
            )
            .await?
            .into_inner();
    }
    let closed = IssueState::try_from(i.state).unwrap_or_default() == IssueState::Closed;
    match s(body, "state") {
        Some("closed") if !closed => {
            let reason = match s(body, "state_reason") {
                Some("not_planned") => IssueStateReason::NotPlanned,
                Some("duplicate") => IssueStateReason::Duplicate,
                _ => IssueStateReason::Completed,
            };
            i = svc
                .close_issue(cx.req(CloseIssueRequest {
                    repo: rid(&repo.id),
                    number: n,
                    reason: reason as i32,
                    ..Default::default()
                }))
                .await?
                .into_inner();
        }
        Some("open") if closed => {
            i = svc
                .reopen_issue(cx.req(ReopenIssueRequest {
                    repo: rid(&repo.id),
                    number: n,
                    ..Default::default()
                }))
                .await?
                .into_inner();
        }
        Some("open" | "closed") | None => {}
        Some(other) => return Err(ApiError::validation("Issue", "state", "invalid", other)),
    }
    if let Some(labels) = strs(body, "labels") {
        i = svc
            .set_issue_labels(cx.req(SetIssueLabelsRequest {
                repo: rid(&repo.id),
                number: n,
                labels,
                expected_version: 0,
            }))
            .await?
            .into_inner();
    }
    let wanted = strs(body, "assignees").or_else(|| match body.get("assignee") {
        Some(Value::String(a)) => Some(vec![a.clone()]),
        Some(Value::Null) => Some(Vec::new()),
        _ => None,
    });
    if let Some(logins) = wanted {
        let mut ids = Vec::new();
        for l in &logins {
            ids.push(cx.id_of_login(l, "assignees").await?);
        }
        let cur: Vec<String> = i.assignees.iter().map(|a| a.id.clone()).collect();
        let remove: Vec<String> = cur.iter().filter(|c| !ids.contains(c)).cloned().collect();
        let add: Vec<String> = ids.iter().filter(|x| !cur.contains(x)).cloned().collect();
        if !remove.is_empty() {
            i = svc
                .remove_issue_assignees(cx.req(RemoveIssueAssigneesRequest {
                    repo: rid(&repo.id),
                    number: n,
                    assignees: remove,
                    stop_agent_runs: false,
                }))
                .await?
                .into_inner();
        }
        if !add.is_empty() {
            i = svc
                .add_issue_assignees(cx.req(AddIssueAssigneesRequest {
                    repo: rid(&repo.id),
                    number: n,
                    assignees: add,
                    ..Default::default()
                }))
                .await?
                .into_inner()
                .issue
                .unwrap_or(i);
        }
    }
    if let Some(m) = body.get("milestone") {
        i = svc
            .set_issue_milestone(
                cx.req(SetIssueMilestoneRequest {
                    repo: rid(&repo.id),
                    number: n,
                    milestone_number: m
                        .as_u64()
                        .or_else(|| m.as_str().and_then(|x| x.parse().ok()))
                        .unwrap_or(0),
                }),
            )
            .await?
            .into_inner();
    }
    Ok(i)
}

/// `PATCH /repos/{owner}/{repo}/issues/{number}`.
pub(crate) async fn update(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let v = match subject(&cx, &repo, p.num("number")?).await? {
        Subject::Issue(i) => {
            shapes::issue(&cx, &repo, &update_issue(&cx, &repo, *i, &body).await?).await
        }
        Subject::Pull(c) => {
            shapes::issue_of_pull(&cx, &repo, &update_pull(&cx, &repo, *c, &body).await?).await
        }
    };
    Ok(Reply::json(v))
}

/// `PUT /repos/{owner}/{repo}/issues/{number}/lock`.
pub(crate) async fn lock(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    match subject(&cx, &repo, p.num("number")?).await? {
        Subject::Issue(i) => {
            let reason = match s(&body, "lock_reason") {
                Some("off-topic") => IssueLockReason::OffTopic,
                Some("too heated") => IssueLockReason::TooHeated,
                Some("resolved") => IssueLockReason::Resolved,
                Some("spam") => IssueLockReason::Spam,
                _ => IssueLockReason::Unspecified,
            };
            cx.hub
                .svc
                .lock_issue(cx.req(LockIssueRequest {
                    repo: rid(&repo.id),
                    number: i.number,
                    reason: reason as i32,
                }))
                .await?;
        }
        Subject::Pull(c) => {
            let fcx = authorize(&cx, &repo, Action::Triage).await?;
            changes(&cx)?
                .changes()
                .set_locked(&fcx, &repo, &c.id, true)
                .await?;
        }
    }
    Ok(Reply::no_content())
}

/// `DELETE /repos/{owner}/{repo}/issues/{number}/lock`.
pub(crate) async fn unlock(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    match subject(&cx, &repo, p.num("number")?).await? {
        Subject::Issue(i) => {
            cx.hub
                .svc
                .unlock_issue(cx.req(UnlockIssueRequest {
                    repo: rid(&repo.id),
                    number: i.number,
                }))
                .await?;
        }
        Subject::Pull(c) => {
            let fcx = authorize(&cx, &repo, Action::Triage).await?;
            changes(&cx)?
                .changes()
                .set_locked(&fcx, &repo, &c.id, false)
                .await?;
        }
    }
    Ok(Reply::no_content())
}

/// Every conversation comment of an issue or PR.
pub(crate) async fn comments_of(cx: &Ctx, repo: &Repo, sub: &Subject) -> ApiResult<Vec<Comment>> {
    match sub {
        Subject::Issue(i) => {
            let svc = cx.hub.svc.clone();
            let since = cx.q("since").and_then(parse_iso).map(ts_secs);
            all_pages(|page| {
                let svc = svc.clone();
                let req = cx.req(ListIssueCommentsRequest {
                    repo: rid(&repo.id),
                    number: i.number,
                    since,
                    include_html: false,
                    page: Some(page),
                });
                async move {
                    let r = svc.list_issue_comments(req).await?.into_inner();
                    Ok((r.comments, r.page))
                }
            })
            .await
        }
        Subject::Pull(c) => {
            let svc = changes(cx)?.clone();
            all_pages(|page| {
                let svc = svc.clone();
                let req = cx.req(ListChangeCommentsRequest {
                    repo: rid(&repo.id),
                    change_request_id: c.id.clone(),
                    page: Some(page),
                });
                async move {
                    let r = svc.list_change_comments(req).await?.into_inner();
                    Ok((r.comments, r.page))
                }
            })
            .await
        }
    }
}

fn number_of(sub: &Subject) -> u64 {
    match sub {
        Subject::Issue(i) => i.number,
        Subject::Pull(c) => c.number,
    }
}

/// `GET /repos/{owner}/{repo}/issues/{number}/comments`.
pub(crate) async fn comments(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sub = subject(&cx, &repo, p.num("number")?).await?;
    let n = number_of(&sub);
    let mut items = Vec::new();
    for c in comments_of(&cx, &repo, &sub).await? {
        if !c.deleted {
            items.push(shapes::comment(&cx, &repo, n, &c).await);
        }
    }
    Ok(cx.paginate(items))
}

/// Adds a conversation comment to an issue or PR; shared with GraphQL `addComment`.
pub(crate) async fn add_comment(
    cx: &Ctx,
    repo: &Repo,
    sub: &Subject,
    body: &str,
) -> ApiResult<Comment> {
    Ok(match sub {
        Subject::Issue(i) => cx
            .hub
            .svc
            .create_issue_comment(cx.req(CreateIssueCommentRequest {
                repo: rid(&repo.id),
                number: i.number,
                body: body.to_string(),
                ..Default::default()
            }))
            .await?
            .into_inner(),
        Subject::Pull(c) => changes(cx)?
            .create_change_comment(cx.req(CreateChangeCommentRequest {
                repo: rid(&repo.id),
                change_request_id: c.id.clone(),
                body: body.to_string(),
                ..Default::default()
            }))
            .await?
            .into_inner(),
    })
}

/// `POST /repos/{owner}/{repo}/issues/{number}/comments`.
pub(crate) async fn create_comment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sub = subject(&cx, &repo, p.num("number")?).await?;
    let body = cx.body_json()?;
    let text = need(&body, "body", "IssueComment")?;
    let c = add_comment(&cx, &repo, &sub, text).await?;
    Ok(Reply::created(
        shapes::comment(&cx, &repo, number_of(&sub), &c).await,
    ))
}

/// Every issue and PR number of a repo (for repo-wide comment lists and id scans).
async fn all_subjects(cx: &Ctx, repo: &Repo) -> ApiResult<Vec<Subject>> {
    let mut out: Vec<Subject> = list_issues(cx, repo, "all")
        .await?
        .into_iter()
        .map(|i| Subject::Issue(Box::new(i)))
        .collect();
    if cx.hub.changes.is_some() {
        out.extend(
            list_crs(cx, repo, "all", None, "")
                .await?
                .into_iter()
                .map(|c| Subject::Pull(Box::new(c))),
        );
    }
    Ok(out)
}

/// `GET /repos/{owner}/{repo}/issues/comments`.
pub(crate) async fn repo_comments(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let mut all: Vec<(i64, Value)> = Vec::new();
    for sub in all_subjects(&cx, &repo).await? {
        let n = number_of(&sub);
        for c in comments_of(&cx, &repo, &sub).await? {
            if !c.deleted {
                let t = c.created_at.as_ref().map(|t| t.seconds).unwrap_or(0);
                all.push((t, shapes::comment(&cx, &repo, n, &c).await));
            }
        }
    }
    let desc = cx.q("direction") == Some("desc");
    all.sort_by(|a, b| if desc { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) });
    Ok(cx.paginate(all.into_iter().map(|(_, v)| v).collect()))
}

/// Finds a comment by numeric id: through the index hint, else by scanning the repo.
async fn find_comment(cx: &Ctx, repo: &Repo, id: u64) -> ApiResult<(Subject, Comment)> {
    if let Some(h) = cx.hub.ids.get("comment", id)
        && let Some(n) = h.context.rsplit(':').next().and_then(|n| n.parse().ok())
        && let Ok(sub) = subject(cx, repo, n).await
        && let Some(c) = comments_of(cx, repo, &sub)
            .await?
            .into_iter()
            .find(|c| c.id == h.id)
    {
        return Ok((sub, c));
    }
    for sub in all_subjects(cx, repo).await? {
        if let Some(c) = comments_of(cx, repo, &sub)
            .await?
            .into_iter()
            .find(|c| crate::ids::num("comment", &c.id) == id)
        {
            return Ok((sub, c));
        }
    }
    Err(ApiError::not_found())
}

/// `GET /repos/{owner}/{repo}/issues/comments/{id}`.
pub(crate) async fn get_comment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (sub, c) = find_comment(&cx, &repo, p.num("id")?).await?;
    Ok(Reply::json(
        shapes::comment(&cx, &repo, number_of(&sub), &c).await,
    ))
}

fn pr_comments_unsupported(what: &str) -> ApiError {
    ApiError::new(
        422,
        bee_proto::v1::ErrorReason::Unimplemented,
        format!("{what} of pull request conversation comments is not supported"),
    )
}

/// `PATCH /repos/{owner}/{repo}/issues/comments/{id}`.
pub(crate) async fn update_comment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (sub, c) = find_comment(&cx, &repo, p.num("id")?).await?;
    let body = cx.body_json()?;
    let text = need(&body, "body", "IssueComment")?;
    if matches!(sub, Subject::Pull(_)) {
        return Err(pr_comments_unsupported("Editing"));
    }
    let fcx = authorize(&cx, &repo, Action::Read).await?;
    let actor = work_actor(&cx, &repo, &fcx).await?;
    let edited = cx
        .hub
        .forge
        .work
        .update_comment(
            &fcx,
            &RepoInfo::from(&repo),
            &actor,
            &c.id,
            text,
            0,
            &Default::default(),
        )
        .await?;
    Ok(Reply::json(
        shapes::comment(&cx, &repo, number_of(&sub), &edited).await,
    ))
}

/// `DELETE /repos/{owner}/{repo}/issues/comments/{id}`.
pub(crate) async fn delete_comment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (sub, c) = find_comment(&cx, &repo, p.num("id")?).await?;
    if matches!(sub, Subject::Pull(_)) {
        return Err(pr_comments_unsupported("Deleting"));
    }
    let fcx = authorize(&cx, &repo, Action::Read).await?;
    let actor = work_actor(&cx, &repo, &fcx).await?;
    cx.hub
        .forge
        .work
        .delete_comment(&fcx, &RepoInfo::from(&repo), &actor, &c.id)
        .await?;
    Ok(Reply::no_content())
}

fn reaction_json(id: &str, who: Value, content: ReactionContent, at: Value) -> Value {
    json!({
        "id": crate::ids::num("reaction", id),
        "node_id": crate::ids::node("Reaction", id),
        "user": who,
        "content": shapes::reaction_name(content),
        "created_at": at,
    })
}

async fn reactions_list(cx: &Ctx, subject_id: &str) -> ApiResult<Vec<Value>> {
    let fcx = Cx::system();
    let content = cx
        .q("content")
        .and_then(shapes::reaction_of)
        .unwrap_or(ReactionContent::Unspecified);
    let (list, _, _) = cx
        .hub
        .forge
        .work
        .list_reactions(&fcx, subject_id, content, 1000, "")
        .await?;
    let mut out = Vec::new();
    for r in list {
        let who = match &r.user {
            Some(p) => shapes::user(cx, p).await,
            None => Value::Null,
        };
        let content = ReactionContent::try_from(r.content).unwrap_or_default();
        let v = reaction_json(
            &format!("{subject_id}:{}", r.content),
            who,
            content,
            iso(&r.created_at),
        );
        out.push(v);
    }
    Ok(out)
}

async fn react_on(
    cx: &Ctx,
    repo: &Repo,
    subject_id: &str,
    pr: Option<(&ChangeRequest, Option<&str>)>,
) -> ApiResult<Reply> {
    let body = cx.body_json()?;
    let word = need(&body, "content", "Reaction")?;
    let content = shapes::reaction_of(word)
        .ok_or_else(|| ApiError::validation("Reaction", "content", "invalid", word))?;
    let fcx = authorize(cx, repo, Action::Read).await?;
    let me = shapes::user(cx, &cx.principal(&fcx.actor_id).await).await;
    match pr {
        Some((c, None)) => {
            changes(cx)?
                .changes()
                .react(&fcx, repo, &c.id, content, true)
                .await?;
        }
        Some((c, Some(comment))) => {
            changes(cx)?
                .changes()
                .react_comment(&fcx, repo, &c.id, comment, content, true)
                .await?;
        }
        None => {
            let actor = work_actor(cx, repo, &fcx).await?;
            cx.hub
                .forge
                .work
                .add_reaction(&fcx, &RepoInfo::from(repo), &actor, subject_id, content)
                .await?;
        }
    }
    let v = reaction_json(
        &format!("{subject_id}:{}:{}", fcx.actor_id, content as i32),
        me,
        content,
        json!(crate::time::iso_ms(cx.hub.ports.clock.now_ms())),
    );
    Ok(Reply::created(v))
}

/// `GET /repos/{owner}/{repo}/issues/{number}/reactions`.
pub(crate) async fn reactions(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    match subject(&cx, &repo, p.num("number")?).await? {
        Subject::Issue(i) => {
            let items = reactions_list(&cx, &i.id).await?;
            Ok(cx.paginate(items))
        }
        Subject::Pull(c) => {
            let mut items = Vec::new();
            for g in &c.reactions {
                let content = ReactionContent::try_from(g.content).unwrap_or_default();
                for k in 0..g.count {
                    let v = reaction_json(
                        &format!("{}:{}:{k}", c.id, g.content),
                        Value::Null,
                        content,
                        Value::Null,
                    );
                    items.push(v);
                }
            }
            Ok(cx.paginate(items))
        }
    }
}

/// `POST /repos/{owner}/{repo}/issues/{number}/reactions`.
pub(crate) async fn react(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    match subject(&cx, &repo, p.num("number")?).await? {
        Subject::Issue(i) => react_on(&cx, &repo, &i.id, None).await,
        Subject::Pull(c) => react_on(&cx, &repo, &c.id, Some((&c, None))).await,
    }
}

/// `GET /repos/{owner}/{repo}/issues/comments/{id}/reactions`.
pub(crate) async fn comment_reactions(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (_, c) = find_comment(&cx, &repo, p.num("id")?).await?;
    let items = reactions_list(&cx, &c.id).await?;
    Ok(cx.paginate(items))
}

/// `POST /repos/{owner}/{repo}/issues/comments/{id}/reactions`.
pub(crate) async fn react_comment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (sub, c) = find_comment(&cx, &repo, p.num("id")?).await?;
    match &sub {
        Subject::Issue(_) => react_on(&cx, &repo, &c.id, None).await,
        Subject::Pull(cr) => react_on(&cx, &repo, &c.id, Some((cr, Some(&c.id)))).await,
    }
}

async fn labels_of(cx: &Ctx, repo: &Repo, sub: &Subject) -> Vec<Value> {
    let labels = match sub {
        Subject::Issue(i) => &i.labels,
        Subject::Pull(c) => &c.labels,
    };
    labels.iter().map(|l| shapes::label(cx, repo, l)).collect()
}

/// `GET /repos/{owner}/{repo}/issues/{number}/labels`.
pub(crate) async fn issue_labels(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sub = subject(&cx, &repo, p.num("number")?).await?;
    Ok(cx.paginate(labels_of(&cx, &repo, &sub).await))
}

fn label_names(body: &Value) -> Vec<String> {
    match body {
        Value::Array(_) => strs(&json!({"l": body}), "l").unwrap_or_default(),
        _ => strs(body, "labels").unwrap_or_default(),
    }
}

async fn change_labels(
    cx: &Ctx,
    repo: &Repo,
    sub: Subject,
    f: impl Fn(Vec<String>) -> Vec<String>,
    add_only: Option<Vec<String>>,
) -> ApiResult<Subject> {
    match sub {
        Subject::Issue(i) => {
            let svc = &cx.hub.svc;
            let out = match add_only {
                Some(add) => svc
                    .add_issue_labels(cx.req(AddIssueLabelsRequest {
                        repo: rid(&repo.id),
                        number: i.number,
                        labels: add,
                        create_missing: true,
                    }))
                    .await?
                    .into_inner(),
                None => {
                    let cur: Vec<String> = i.labels.iter().map(|l| l.name.clone()).collect();
                    svc.set_issue_labels(cx.req(SetIssueLabelsRequest {
                        repo: rid(&repo.id),
                        number: i.number,
                        labels: f(cur),
                        expected_version: 0,
                    }))
                    .await?
                    .into_inner()
                }
            };
            Ok(Subject::Issue(Box::new(out)))
        }
        Subject::Pull(c) => {
            let cur: Vec<String> = c.labels.iter().map(|l| l.name.clone()).collect();
            let next = match add_only {
                Some(add) => {
                    let mut v = cur;
                    for a in add {
                        if !v.iter().any(|x| x.eq_ignore_ascii_case(&a)) {
                            v.push(a);
                        }
                    }
                    v
                }
                None => f(cur),
            };
            let out = changes(cx)?
                .set_change_labels(cx.req(SetChangeLabelsRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                    labels: next,
                }))
                .await?
                .into_inner();
            Ok(Subject::Pull(Box::new(out)))
        }
    }
}

/// `POST /repos/{owner}/{repo}/issues/{number}/labels`.
pub(crate) async fn add_labels(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sub = subject(&cx, &repo, p.num("number")?).await?;
    let names = label_names(&cx.body_json()?);
    let sub = change_labels(&cx, &repo, sub, |v| v, Some(names)).await?;
    Ok(Reply::json(Value::Array(labels_of(&cx, &repo, &sub).await)))
}

/// `PUT /repos/{owner}/{repo}/issues/{number}/labels`.
pub(crate) async fn set_labels(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sub = subject(&cx, &repo, p.num("number")?).await?;
    let names = label_names(&cx.body_json()?);
    let sub = change_labels(&cx, &repo, sub, move |_| names.clone(), None).await?;
    Ok(Reply::json(Value::Array(labels_of(&cx, &repo, &sub).await)))
}

/// `DELETE /repos/{owner}/{repo}/issues/{number}/labels`.
pub(crate) async fn clear_labels(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sub = subject(&cx, &repo, p.num("number")?).await?;
    change_labels(&cx, &repo, sub, |_| Vec::new(), None).await?;
    Ok(Reply::no_content())
}

/// `DELETE /repos/{owner}/{repo}/issues/{number}/labels/{name}`.
pub(crate) async fn remove_label(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sub = subject(&cx, &repo, p.num("number")?).await?;
    let name = p.get("name").to_string();
    let has = match &sub {
        Subject::Issue(i) => i.labels.iter().any(|l| l.name.eq_ignore_ascii_case(&name)),
        Subject::Pull(c) => c.labels.iter().any(|l| l.name.eq_ignore_ascii_case(&name)),
    };
    if !has {
        return Err(ApiError::not_found().with_message("Label does not exist"));
    }
    let sub = match sub {
        Subject::Issue(i) => Subject::Issue(Box::new(
            cx.hub
                .svc
                .remove_issue_labels(cx.req(RemoveIssueLabelsRequest {
                    repo: rid(&repo.id),
                    number: i.number,
                    labels: vec![name],
                }))
                .await?
                .into_inner(),
        )),
        pr => {
            change_labels(
                &cx,
                &repo,
                pr,
                move |v| {
                    v.into_iter()
                        .filter(|x| !x.eq_ignore_ascii_case(&name))
                        .collect()
                },
                None,
            )
            .await?
        }
    };
    Ok(Reply::json(Value::Array(labels_of(&cx, &repo, &sub).await)))
}

async fn assignee_ids(cx: &Ctx, body: &Value) -> ApiResult<Vec<String>> {
    let mut ids = Vec::new();
    for l in strs(body, "assignees").unwrap_or_default() {
        ids.push(cx.id_of_login(&l, "assignees").await?);
    }
    Ok(ids)
}

/// `POST /repos/{owner}/{repo}/issues/{number}/assignees`.
pub(crate) async fn add_assignees(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let ids = assignee_ids(&cx, &body).await?;
    let v = match subject(&cx, &repo, p.num("number")?).await? {
        Subject::Issue(i) => {
            let out = cx
                .hub
                .svc
                .add_issue_assignees(cx.req(AddIssueAssigneesRequest {
                    repo: rid(&repo.id),
                    number: i.number,
                    assignees: ids,
                    ..Default::default()
                }))
                .await?
                .into_inner()
                .issue
                .unwrap_or(*i);
            shapes::issue(&cx, &repo, &out).await
        }
        Subject::Pull(c) => {
            let mut all: Vec<String> = c.assignees.iter().map(|a| a.id.clone()).collect();
            for x in ids {
                if !all.contains(&x) {
                    all.push(x);
                }
            }
            let out = changes(&cx)?
                .set_change_assignees(cx.req(SetChangeAssigneesRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                    assignee_ids: all,
                }))
                .await?
                .into_inner();
            shapes::issue_of_pull(&cx, &repo, &out).await
        }
    };
    Ok(Reply::created(v))
}

/// `DELETE /repos/{owner}/{repo}/issues/{number}/assignees`.
pub(crate) async fn remove_assignees(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let ids = assignee_ids(&cx, &body).await?;
    let v = match subject(&cx, &repo, p.num("number")?).await? {
        Subject::Issue(i) => {
            let out = cx
                .hub
                .svc
                .remove_issue_assignees(cx.req(RemoveIssueAssigneesRequest {
                    repo: rid(&repo.id),
                    number: i.number,
                    assignees: ids,
                    stop_agent_runs: false,
                }))
                .await?
                .into_inner();
            shapes::issue(&cx, &repo, &out).await
        }
        Subject::Pull(c) => {
            let keep: Vec<String> = c
                .assignees
                .iter()
                .map(|a| a.id.clone())
                .filter(|a| !ids.contains(a))
                .collect();
            let out = changes(&cx)?
                .set_change_assignees(cx.req(SetChangeAssigneesRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                    assignee_ids: keep,
                }))
                .await?
                .into_inner();
            shapes::issue_of_pull(&cx, &repo, &out).await
        }
    };
    Ok(Reply::json(v))
}

fn event_word(k: IssueTimelineEventKind) -> String {
    use IssueTimelineEventKind as K;
    match k {
        K::Commented => "commented".into(),
        K::CrossReferenced => "cross-referenced".into(),
        K::MarkedDuplicate => "marked_as_duplicate".into(),
        K::UnmarkedDuplicate => "unmarked_as_duplicate".into(),
        K::ChangeLinked => "connected".into(),
        K::ChangeUnlinked => "disconnected".into(),
        K::TypeChanged => "issue_type_changed".into(),
        K::AddedToProject => "added_to_project_v2".into(),
        K::RemovedFromProject => "removed_from_project_v2".into(),
        K::ProjectStatusChanged => "project_v2_item_status_changed".into(),
        K::SubIssueAdded => "sub_issue_added".into(),
        K::SubIssueRemoved => "sub_issue_removed".into(),
        K::ParentAdded => "parent_issue_added".into(),
        K::ParentRemoved => "parent_issue_removed".into(),
        K::ConvertedToDiscussion => "converted_to_discussion".into(),
        k => k
            .as_str_name()
            .trim_start_matches("ISSUE_TIMELINE_EVENT_KIND_")
            .to_lowercase(),
    }
}

async fn event_json(cx: &Ctx, repo: &Repo, n: u64, e: &IssueTimelineEvent) -> Value {
    let kind = IssueTimelineEventKind::try_from(e.kind).unwrap_or_default();
    let id = cx
        .hub
        .ids
        .remember("event", &e.id, &format!("{}:{n}", repo.id));
    let actor = match &e.actor {
        Some(p) => shapes::user(cx, p).await,
        None => Value::Null,
    };
    let mut v = json!({
        "id": id,
        "node_id": crate::ids::node("IssueEvent", &e.id),
        "url": cx.api(&format!("/repos/{}/issues/events/{id}", repo.full_name)),
        "actor": actor,
        "event": event_word(kind),
        "commit_id": Value::Null,
        "commit_url": Value::Null,
        "created_at": iso(&e.created_at),
        "performed_via_github_app": Value::Null,
    });
    if let Some(l) = &e.label {
        v["label"] = json!({"name": l.name, "color": l.color});
    }
    if let Some(a) = &e.assignee {
        v["assignee"] = shapes::user(cx, a).await;
    }
    if !e.milestone_title.is_empty() {
        v["milestone"] = json!({"title": e.milestone_title});
    }
    if kind == IssueTimelineEventKind::Renamed {
        v["rename"] = json!({"from": e.previous_title, "to": e.current_title});
    }
    if kind == IssueTimelineEventKind::Closed {
        v["state_reason"] = match IssueStateReason::try_from(e.state_reason).unwrap_or_default() {
            IssueStateReason::NotPlanned => json!("not_planned"),
            IssueStateReason::Duplicate => json!("duplicate"),
            _ => json!("completed"),
        };
    }
    if let Some(c) = &e.comment {
        let cj = shapes::comment(cx, repo, n, c).await;
        if let (Value::Object(m), Value::Object(cm)) = (&mut v, cj) {
            for k in [
                "body",
                "html_url",
                "issue_url",
                "user",
                "author_association",
                "reactions",
                "updated_at",
            ] {
                if let Some(x) = cm.get(k) {
                    m.insert(k.to_string(), x.clone());
                }
            }
        }
    }
    v
}

async fn timeline_of(cx: &Ctx, repo: &Repo, n: u64) -> ApiResult<Vec<IssueTimelineEvent>> {
    let svc = cx.hub.svc.clone();
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListIssueTimelineRequest {
            repo: rid(&repo.id),
            number: n,
            kinds: Vec::new(),
            page: Some(page),
        });
        async move {
            let r = svc.list_issue_timeline(req).await?.into_inner();
            Ok((r.events, r.page))
        }
    })
    .await
}

/// `GET /repos/{owner}/{repo}/issues/{number}/events`.
pub(crate) async fn events(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sub = subject(&cx, &repo, p.num("number")?).await?;
    let n = number_of(&sub);
    let mut items = Vec::new();
    if let Subject::Issue(_) = sub {
        for e in timeline_of(&cx, &repo, n).await? {
            if e.kind != IssueTimelineEventKind::Commented as i32 {
                items.push(event_json(&cx, &repo, n, &e).await);
            }
        }
    }
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/issues/{number}/timeline`.
pub(crate) async fn timeline(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sub = subject(&cx, &repo, p.num("number")?).await?;
    let n = number_of(&sub);
    let mut items = Vec::new();
    match &sub {
        Subject::Issue(_) => {
            for e in timeline_of(&cx, &repo, n).await? {
                items.push(event_json(&cx, &repo, n, &e).await);
            }
        }
        Subject::Pull(c) => {
            let mut keyed: Vec<(i64, Value)> = Vec::new();
            for cm in comments_of(&cx, &repo, &sub).await? {
                let mut v = shapes::comment(&cx, &repo, n, &cm).await;
                v["event"] = json!("commented");
                v["actor"] = v["user"].clone();
                keyed.push((cm.created_at.as_ref().map(|t| t.seconds).unwrap_or(0), v));
            }
            if let Ok(svc) = changes(&cx) {
                let reviews = svc
                    .list_reviews(cx.req(bee_proto::v1::ListReviewsRequest {
                        repo: rid(&repo.id),
                        change_request_id: c.id.clone(),
                    }))
                    .await?
                    .into_inner()
                    .reviews;
                for r in reviews {
                    let mut v = super::pulls::review_json(&cx, &repo, c, &r).await;
                    v["event"] = json!("reviewed");
                    keyed.push((r.created_at.as_ref().map(|t| t.seconds).unwrap_or(0), v));
                }
            }
            keyed.sort_by_key(|(t, _)| *t);
            items = keyed.into_iter().map(|(_, v)| v).collect();
        }
    }
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/issues/events`.
pub(crate) async fn repo_events(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let mut keyed: Vec<(i64, Value)> = Vec::new();
    for i in list_issues(&cx, &repo, "all").await? {
        for e in timeline_of(&cx, &repo, i.number).await? {
            if e.kind == IssueTimelineEventKind::Commented as i32 {
                continue;
            }
            let mut v = event_json(&cx, &repo, i.number, &e).await;
            v["issue"] = shapes::issue(&cx, &repo, &i).await;
            keyed.push((e.created_at.as_ref().map(|t| t.seconds).unwrap_or(0), v));
        }
    }
    keyed.sort_by_key(|k| std::cmp::Reverse(k.0));
    Ok(cx.paginate(keyed.into_iter().map(|(_, v)| v).collect()))
}

/// `GET /repos/{owner}/{repo}/assignees`.
pub(crate) async fn assignees(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListIssueAssignableUsersRequest {
            repo: rid(&repo.id),
            query: String::new(),
            kind: 0,
            page: Some(page),
        });
        async move {
            let r = svc.list_issue_assignable_users(req).await?.into_inner();
            Ok((r.principals, r.page))
        }
    })
    .await?;
    Ok(cx.paginate(shapes::users(&cx, &list).await))
}

/// `GET /repos/{owner}/{repo}/assignees/{user}`: `204` when assignable.
pub(crate) async fn is_assignee(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let who = cx.by_login(p.get("user")).await?;
    let list = cx
        .hub
        .svc
        .list_issue_assignable_users(cx.req(ListIssueAssignableUsersRequest {
            repo: rid(&repo.id),
            query: p.get("user").to_string(),
            kind: 0,
            page: None,
        }))
        .await?
        .into_inner()
        .principals;
    if list.iter().any(|x| x.id == who.id) {
        Ok(Reply::no_content())
    } else {
        Err(ApiError::not_found())
    }
}

/// `GET /repos/{owner}/{repo}/labels`.
pub(crate) async fn labels(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListIssueLabelsRequest {
            repo: rid(&repo.id),
            query: String::new(),
            page: Some(page),
        });
        async move {
            let r = svc.list_issue_labels(req).await?.into_inner();
            Ok((r.labels, r.page))
        }
    })
    .await?;
    Ok(cx.paginate(list.iter().map(|l| shapes::label(&cx, &repo, l)).collect()))
}

/// `POST /repos/{owner}/{repo}/labels`.
pub(crate) async fn create_label(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let l = cx
        .hub
        .svc
        .create_issue_label(
            cx.req(CreateIssueLabelRequest {
                repo: rid(&repo.id),
                name: need(&body, "name", "Label")?.to_string(),
                color: s(&body, "color")
                    .unwrap_or("ededed")
                    .trim_start_matches('#')
                    .to_string(),
                description: s(&body, "description").unwrap_or("").to_string(),
            }),
        )
        .await?
        .into_inner();
    Ok(Reply::created(shapes::label(&cx, &repo, &l)))
}

/// `GET /repos/{owner}/{repo}/labels/{name}`.
pub(crate) async fn label(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let l = cx
        .hub
        .svc
        .get_issue_label(cx.req(GetIssueLabelRequest {
            repo: rid(&repo.id),
            name: p.get("name").to_string(),
        }))
        .await?
        .into_inner();
    Ok(Reply::json(shapes::label(&cx, &repo, &l)))
}

/// `PATCH /repos/{owner}/{repo}/labels/{name}`.
pub(crate) async fn update_label(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let l = cx
        .hub
        .svc
        .update_issue_label(cx.req(UpdateIssueLabelRequest {
            repo: rid(&repo.id),
            name: p.get("name").to_string(),
            new_name: s(&body, "new_name").map(str::to_string),
            color: s(&body, "color").map(|c| c.trim_start_matches('#').to_string()),
            description: s(&body, "description").map(str::to_string),
        }))
        .await?
        .into_inner();
    Ok(Reply::json(shapes::label(&cx, &repo, &l)))
}

/// `DELETE /repos/{owner}/{repo}/labels/{name}`.
pub(crate) async fn delete_label(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    cx.hub
        .svc
        .delete_issue_label(cx.req(DeleteIssueLabelRequest {
            repo: rid(&repo.id),
            name: p.get("name").to_string(),
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `GET /repos/{owner}/{repo}/milestones`.
pub(crate) async fn milestones(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let state = cx.q("state").unwrap_or("open");
    let svc = cx.hub.svc.clone();
    let mut list: Vec<Milestone> = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListIssueMilestonesRequest {
            repo: rid(&repo.id),
            state: if state == "closed" {
                MilestoneState::Closed as i32
            } else {
                MilestoneState::Open as i32
            },
            all_states: state == "all",
            page: Some(page),
        });
        async move {
            let r = svc.list_issue_milestones(req).await?.into_inner();
            Ok((r.milestones, r.page))
        }
    })
    .await?;
    let desc = cx.q("direction") == Some("desc");
    match cx.q("sort").unwrap_or("due_on") {
        "completeness" => list.sort_by_key(|m| {
            let total = (m.open_items + m.closed_items).max(1);
            m.closed_items * 1000 / total
        }),
        _ => list.sort_by_key(|m| {
            (
                m.due_on.as_ref().map(|t| t.seconds).unwrap_or(i64::MAX),
                m.number,
            )
        }),
    }
    if desc {
        list.reverse();
    }
    Ok(cx.paginate(
        list.iter()
            .map(|m| shapes::milestone(&cx, &repo, m))
            .collect(),
    ))
}

/// `POST /repos/{owner}/{repo}/milestones`.
pub(crate) async fn create_milestone(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let mut m = cx
        .hub
        .svc
        .create_issue_milestone(cx.req(CreateIssueMilestoneRequest {
            repo: rid(&repo.id),
            title: need(&body, "title", "Milestone")?.to_string(),
            description: s(&body, "description").unwrap_or("").to_string(),
            due_on: s(&body, "due_on").and_then(parse_iso).map(ts_secs),
        }))
        .await?
        .into_inner();
    if s(&body, "state") == Some("closed") {
        m = cx
            .hub
            .svc
            .update_issue_milestone(cx.req(UpdateIssueMilestoneRequest {
                repo: rid(&repo.id),
                number: m.number,
                state: MilestoneState::Closed as i32,
                ..Default::default()
            }))
            .await?
            .into_inner();
    }
    Ok(Reply::created(shapes::milestone(&cx, &repo, &m)))
}

/// `GET /repos/{owner}/{repo}/milestones/{number}`.
pub(crate) async fn milestone(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let m = cx
        .hub
        .svc
        .get_issue_milestone(cx.req(GetIssueMilestoneRequest {
            repo: rid(&repo.id),
            number: p.num("number")?,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(shapes::milestone(&cx, &repo, &m)))
}

/// `PATCH /repos/{owner}/{repo}/milestones/{number}`.
pub(crate) async fn update_milestone(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let due = body.get("due_on");
    let m = cx
        .hub
        .svc
        .update_issue_milestone(cx.req(UpdateIssueMilestoneRequest {
            repo: rid(&repo.id),
            number: p.num("number")?,
            title: s(&body, "title").map(str::to_string),
            description: s(&body, "description").map(str::to_string),
            due_on: due.and_then(Value::as_str).and_then(parse_iso).map(ts_secs),
            clear_due_on: matches!(due, Some(Value::Null)),
            state: match s(&body, "state") {
                Some("closed") => MilestoneState::Closed as i32,
                Some("open") => MilestoneState::Open as i32,
                _ => 0,
            },
        }))
        .await?
        .into_inner();
    Ok(Reply::json(shapes::milestone(&cx, &repo, &m)))
}

/// `DELETE /repos/{owner}/{repo}/milestones/{number}`.
pub(crate) async fn delete_milestone(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    cx.hub
        .svc
        .delete_issue_milestone(cx.req(DeleteIssueMilestoneRequest {
            repo: rid(&repo.id),
            number: p.num("number")?,
        }))
        .await?;
    Ok(Reply::no_content())
}
