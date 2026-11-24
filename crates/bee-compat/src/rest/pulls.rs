//! Pull requests over change requests (numbers preserved): list, get, create, update, merge,
//! files, commits, reviews, review comments, requested reviewers.

use std::sync::Arc;

use bee_proto::v1::change_service_server::ChangeService as _;
use bee_proto::v1::repo_service_server::RepoService as _;
use bee_proto::v1::{
    ChangeRequest, ChangeState, CloseChangeRequest, ConvertChangeToDraftRequest,
    CreateChangeCommentRequest, DismissReviewRequest, EditChangeRequest, ErrorReason,
    EvaluateLandingRequest, GetChangeByNumberRequest, LandChangeRequest, LandMethod,
    LandingEvaluation, ListChangesRequest, ListReviewThreadsRequest, ListReviewsRequest,
    MarkChangeReadyRequest, OpenChangeRequest, Principal, RemoveReviewRequestsRequest,
    ReopenChangeRequest, ReplyToReviewThreadRequest, Repo, RequestReviewersRequest, Review,
    ReviewComment, ReviewThread, SubmitReviewRequest, UpdateChangeBaseRequest, Verdict,
};
use serde_json::{Value, json};

use super::{b, changes, need, repo_of, rid, s, strs};
use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Reply, all_pages};
use crate::shapes;
use crate::time::{iso, iso_or};

/// The state key holding a PR's head branch name.
pub(crate) fn head_key(repo_id: &str, cr_id: &str) -> String {
    format!("pr-head/{repo_id}/{cr_id}")
}

/// The PR's head branch: the change request's `head_ref` (recorded by the forge), else the
/// name the gateway recorded at creation, else `pull/<n>/head`.
pub(crate) async fn head_ref(cx: &Ctx, repo: &Repo, c: &ChangeRequest) -> String {
    if !c.head_ref.is_empty() {
        return c.head_ref.clone();
    }
    match cx.hub.ports.state.get(&head_key(&repo.id, &c.id)).await {
        Ok(Some(h)) => h,
        _ => format!("pull/{}/head", c.number),
    }
}

/// Git ids of the PR's current head and base.
pub(crate) async fn shas(cx: &Ctx, repo: &Repo, c: &ChangeRequest) -> (String, String) {
    let git = &cx.hub.ports.git;
    let rev = c.revisions.last();
    let head = match rev.and_then(|r| r.head.as_ref()) {
        Some(h) => git.git_id(repo, h).await.ok().flatten().unwrap_or_default(),
        None => String::new(),
    };
    let base = match rev.and_then(|r| r.base.as_ref()) {
        Some(b) => git.git_id(repo, b).await.ok().flatten().unwrap_or_default(),
        None => String::new(),
    };
    let base = if base.is_empty() {
        git.resolve(repo, &c.target_ref)
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    } else {
        base
    };
    (head, base)
}

/// The CR with GitHub number `n`, `None` when the number is not a pull request.
pub(crate) async fn cr_by_number(
    cx: &Ctx,
    repo: &Repo,
    n: u64,
) -> ApiResult<Option<ChangeRequest>> {
    let Some(svc) = cx.hub.changes.as_ref() else {
        return Ok(None);
    };
    match svc
        .get_change_by_number(cx.req(GetChangeByNumberRequest {
            repo: rid(&repo.id),
            number: n,
        }))
        .await
    {
        Ok(r) => Ok(Some(r.into_inner())),
        Err(e) => {
            let e = ApiError::from(e);
            if e.status == 404 { Ok(None) } else { Err(e) }
        }
    }
}

/// The CR of `:number` (`404` when absent).
pub(crate) async fn cr_of(cx: &Ctx, repo: &Repo, p: &Params) -> ApiResult<ChangeRequest> {
    changes(cx)?;
    cr_by_number(cx, repo, p.num("number")?)
        .await?
        .ok_or_else(ApiError::not_found)
}

fn branch_side(cx: &Ctx, repo: &Repo, owner_user: &Value, name: &str, sha: &str) -> Value {
    let (owner, _) = shapes::split(repo);
    json!({
        "label": format!("{owner}:{name}"),
        "ref": name,
        "sha": sha,
        "user": owner_user,
        "repo": shapes::repo(cx, repo, None, None),
    })
}

/// The `head` side: the fork the PR comes from (when it does) or the repo itself.
async fn head_side(
    cx: &Ctx,
    repo: &Repo,
    c: &ChangeRequest,
    owner_user: &Value,
    name: &str,
    sha: &str,
) -> Value {
    if !c.head_repo_id.is_empty() && c.head_repo_id != repo.id {
        let fork = cx
            .hub
            .svc
            .get_repo(cx.req(bee_proto::v1::GetRepoRequest {
                repo: rid(&c.head_repo_id),
            }))
            .await;
        if let Ok(f) = fork {
            let f = f.into_inner();
            let (owner, _) = shapes::split(&f);
            let user = shapes::org_owner(cx, owner, &f.org_id);
            return branch_side(cx, &f, &user, name, sha);
        }
    }
    branch_side(cx, repo, owner_user, name, sha)
}

/// The fork of `repo` owned by `owner` (the `owner:branch` form of a PR head), if any.
async fn fork_of(cx: &Ctx, repo: &Repo, owner: &str) -> ApiResult<Option<Repo>> {
    let svc = cx.hub.svc.clone();
    let forks = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(bee_proto::v1::ListForksRequest {
            repo: rid(&repo.id),
            page: Some(page),
        });
        async move {
            let r = svc.list_forks(req).await?.into_inner();
            Ok((r.forks, r.page))
        }
    })
    .await?;
    Ok(forks
        .into_iter()
        .find(|f| shapes::split(f).0.eq_ignore_ascii_case(owner)))
}

/// GitHub's pull request object. `eval` adds the mergeability fields of a single GET.
pub(crate) async fn pull(
    cx: &Ctx,
    repo: &Repo,
    c: &ChangeRequest,
    eval: Option<&LandingEvaluation>,
    stats: Option<(u64, u64, u64, u64)>,
) -> Value {
    let base = cx.api(&format!("/repos/{}/pulls/{}", repo.full_name, c.number));
    let issue = cx.api(&format!("/repos/{}/issues/{}", repo.full_name, c.number));
    let html = cx.html(&format!("/{}/pull/{}", repo.full_name, c.number));
    let (state, merged) = shapes::cr_state(c);
    let (head_sha, base_sha) = shas(cx, repo, c).await;
    let hr = head_ref(cx, repo, c).await;
    let (owner, _) = shapes::split(repo);
    let owner_user = shapes::org_owner(cx, owner, &repo.org_id);
    let assignees = shapes::users(cx, &c.assignees).await;
    let mut reviewers = Vec::new();
    let mut teams = Vec::new();
    for rr in &c.review_requests {
        match &rr.reviewer {
            Some(p) if !p.id.is_empty() => reviewers.push(shapes::user(cx, p).await),
            _ if !rr.team_id.is_empty() => {
                teams.push(super::orgs::team_ref(cx, owner, &rr.team_id).await)
            }
            _ => {}
        }
    }
    let statuses = cx.api(&format!("/repos/{}/statuses/{head_sha}", repo.full_name));
    let mut v = json!({
        "url": base,
        "id": cx.hub.ids.remember("pull", &c.id, &format!("{}:{}", repo.id, c.number)),
        "node_id": crate::ids::node("PullRequest", &format!("{}:{}", repo.id, c.number)),
        "html_url": html,
        "diff_url": format!("{html}.diff"),
        "patch_url": format!("{html}.patch"),
        "issue_url": issue,
        "commits_url": format!("{base}/commits"),
        "review_comments_url": format!("{base}/comments"),
        "review_comment_url": cx.api(&format!("/repos/{}/pulls/comments{{/number}}", repo.full_name)),
        "comments_url": format!("{issue}/comments"),
        "statuses_url": statuses,
        "number": c.number,
        "state": state,
        "locked": c.locked,
        "title": c.title,
        "user": shapes::user(cx, c.author.as_ref().unwrap_or(&Principal::default())).await,
        "body": if c.body.is_empty() { Value::Null } else { Value::String(c.body.clone()) },
        "labels": c.labels.iter().map(|l| shapes::label(cx, repo, l)).collect::<Vec<_>>(),
        "milestone": c.milestone.as_ref().map(|m| shapes::milestone(cx, repo, m)).unwrap_or(Value::Null),
        "active_lock_reason": Value::Null,
        "created_at": iso(&c.created_at),
        "updated_at": iso_or(&c.updated_at, &c.created_at),
        "closed_at": iso_or(&c.closed_at, &c.landed_at),
        "merged_at": if merged { iso_or(&c.landed_at, &c.closed_at) } else { Value::Null },
        "merge_commit_sha": Value::Null,
        "assignee": assignees.first().cloned().unwrap_or(Value::Null),
        "assignees": assignees,
        "requested_reviewers": reviewers,
        "requested_teams": teams,
        "head": head_side(cx, repo, c, &owner_user, &hr, &head_sha).await,
        "base": branch_side(cx, repo, &owner_user, &c.target_ref, &base_sha),
        "_links": {
            "self": {"href": base},
            "html": {"href": html},
            "issue": {"href": issue},
            "comments": {"href": format!("{issue}/comments")},
            "review_comments": {"href": format!("{base}/comments")},
            "review_comment": {"href": cx.api(&format!("/repos/{}/pulls/comments{{/number}}", repo.full_name))},
            "commits": {"href": format!("{base}/commits")},
            "statuses": {"href": statuses},
        },
        "author_association": "MEMBER",
        "auto_merge": match &c.auto_merge {
            Some(a) => json!({
                "enabled_by": shapes::user(cx, a.enabled_by.as_ref().unwrap_or(&Principal::default())).await,
                "merge_method": method_word(LandMethod::try_from(a.method).unwrap_or_default()),
                "commit_title": a.commit_message.lines().next().unwrap_or(""),
                "commit_message": a.commit_message,
            }),
            None => Value::Null,
        },
        "draft": c.draft,
    });
    if merged
        && let Some(r) = &c.landed_ref
        && let Some(t) = &r.target
        && let Ok(Some(g)) = cx.hub.ports.git.git_id(repo, t).await
    {
        v["merge_commit_sha"] = json!(g);
    }
    if let Some(e) = eval {
        let open = state == "open";
        v["merged"] = json!(merged);
        v["mergeable"] = if open {
            json!(e.mergeable)
        } else {
            Value::Null
        };
        v["rebaseable"] = if open {
            json!(e.mergeable)
        } else {
            Value::Null
        };
        v["mergeable_state"] = json!(if !open {
            "unknown"
        } else if !e.mergeable {
            "dirty"
        } else if c.draft {
            "draft"
        } else if e.allowed {
            "clean"
        } else {
            "blocked"
        });
        v["merged_by"] = Value::Null;
        v["comments"] = json!(c.comment_count);
        v["maintainer_can_modify"] = json!(c.allow_maintainer_edits);
    }
    if let Some((commits, adds, dels, files)) = stats {
        v["commits"] = json!(commits);
        v["additions"] = json!(adds);
        v["deletions"] = json!(dels);
        v["changed_files"] = json!(files);
        v["review_comments"] = json!(0);
    }
    v
}

/// GitHub's merge method word.
pub(crate) fn method_word(m: LandMethod) -> &'static str {
    match m {
        LandMethod::Squash => "squash",
        LandMethod::MergeCommit => "merge",
        _ => "rebase",
    }
}

/// The land method for GitHub's word (`merge` when absent).
pub(crate) fn method_of(word: Option<&str>) -> ApiResult<LandMethod> {
    Ok(match word.map(str::to_ascii_lowercase).as_deref() {
        None | Some("merge") => LandMethod::MergeCommit,
        Some("squash") => LandMethod::Squash,
        Some("rebase") => LandMethod::Rebase,
        Some(other) => {
            return Err(ApiError::validation(
                "PullRequest",
                "merge_method",
                "invalid",
                other,
            ));
        }
    })
}

/// Every CR of a repo matching the GitHub `state` word (`open`, `closed`, `all`).
pub(crate) async fn list_crs(
    cx: &Ctx,
    repo: &Repo,
    state: &str,
    base: Option<&str>,
    query: &str,
) -> ApiResult<Vec<ChangeRequest>> {
    let svc = changes(cx)?.clone();
    let st = match state {
        "open" => ChangeState::Open,
        _ => ChangeState::Unspecified,
    };
    let mut list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListChangesRequest {
            repo: rid(&repo.id),
            state: st as i32,
            target_ref: base.unwrap_or("").to_string(),
            query: query.to_string(),
            page: Some(page),
            ..Default::default()
        });
        async move {
            let r = svc.list_changes(req).await?.into_inner();
            Ok((r.changes, r.page))
        }
    })
    .await?;
    match state {
        "closed" => list.retain(|c| shapes::cr_state(c).0 == "closed"),
        "open" => list.retain(|c| shapes::cr_state(c).0 == "open"),
        _ => {}
    }
    Ok(list)
}

/// `GET /repos/{owner}/{repo}/pulls`.
pub(crate) async fn list(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let state = cx.q("state").unwrap_or("open");
    let mut crs = list_crs(&cx, &repo, state, cx.q("base"), "").await?;
    if let Some(h) = cx.q("head") {
        let want = h.rsplit_once(':').map(|(_, b)| b).unwrap_or(h).to_string();
        let mut keep = Vec::new();
        for c in crs {
            if head_ref(&cx, &repo, &c).await == want {
                keep.push(c);
            }
        }
        crs = keep;
    }
    let sort = cx.q("sort").unwrap_or("created");
    let asc = cx.q("direction") == Some("asc");
    let key = |c: &ChangeRequest| -> i64 {
        match sort {
            "updated" | "popularity" | "long-running" => {
                c.updated_at.as_ref().map(|t| t.seconds).unwrap_or(0)
            }
            _ => c.created_at.as_ref().map(|t| t.seconds).unwrap_or(0),
        }
    };
    crs.sort_by(|a, b| {
        let o = key(a).cmp(&key(b)).then(a.number.cmp(&b.number));
        if asc { o } else { o.reverse() }
    });
    let (page, per) = cx.page();
    let window: Vec<&ChangeRequest> = crs.iter().skip((page - 1) * per).take(per).collect();
    let mut items = Vec::new();
    for c in window {
        items.push(pull(&cx, &repo, c, None, None).await);
    }
    Ok(cx.paged(items, crs.len()))
}

/// Statistics of a single PR: commits, additions, deletions, files.
async fn stats(cx: &Ctx, repo: &Repo, c: &ChangeRequest) -> Option<(u64, u64, u64, u64)> {
    let (head, base) = shas(cx, repo, c).await;
    if head.is_empty() || base.is_empty() {
        return Some((0, 0, 0, 0));
    }
    let cmp = cx.hub.ports.git.compare(repo, &base, &head).await.ok()?;
    let adds = cmp.files.iter().map(|f| f.additions).sum();
    let dels = cmp.files.iter().map(|f| f.deletions).sum();
    Some((cmp.ahead.len() as u64, adds, dels, cmp.files.len() as u64))
}

/// The single-PR view.
pub(crate) async fn pull_full(cx: &Ctx, repo: &Repo, c: &ChangeRequest) -> Value {
    let eval = match changes(cx) {
        Ok(svc) => svc
            .evaluate_landing(cx.req(EvaluateLandingRequest {
                repo: rid(&repo.id),
                change_request_id: c.id.clone(),
            }))
            .await
            .ok()
            .map(|r| r.into_inner()),
        Err(_) => None,
    };
    let eval = eval.unwrap_or(LandingEvaluation {
        mergeable: true,
        ..Default::default()
    });
    let st = stats(cx, repo, c).await;
    pull(cx, repo, c, Some(&eval), st).await
}

/// `GET /repos/{owner}/{repo}/pulls/{number}` (also `.diff` and `.patch` media types).
pub(crate) async fn get(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    if cx.accepts("diff") || cx.accepts("patch") {
        let (head, base) = shas(&cx, &repo, &c).await;
        let cmp = cx.hub.ports.git.compare(&repo, &base, &head).await?;
        return Ok(Reply::raw(
            super::git::diff_text(&cmp.files),
            "text/plain; charset=utf-8",
        ));
    }
    Ok(Reply::json(pull_full(&cx, &repo, &c).await))
}

/// Opens a PR from a branch (of this repo, or of a fork: `owner:branch` or `head_repo`);
/// shared by REST and GraphQL `createPullRequest`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open(
    cx: &Ctx,
    repo: &Repo,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
    draft: bool,
    head_repo: Option<Repo>,
    maintainer_can_modify: bool,
) -> ApiResult<ChangeRequest> {
    let svc = changes(cx)?;
    let (head_owner, head) = match head.rsplit_once(':') {
        Some((o, b)) => (Some(o), b.to_string()),
        None => (None, head.to_string()),
    };
    // `owner:branch` naming another owner, or an explicit head repo: a PR from a fork.
    let fork = match (head_repo, head_owner) {
        (Some(f), _) if f.id != repo.id => Some(f),
        (Some(_), _) => None,
        (None, Some(o)) if !o.eq_ignore_ascii_case(shapes::split(repo).0) => Some(
            fork_of(cx, repo, o)
                .await?
                .ok_or_else(|| ApiError::validation("PullRequest", "head", "invalid", ""))?,
        ),
        (None, _) => None,
    };
    let head_in = fork.as_ref().unwrap_or(repo);
    let git = &cx.hub.ports.git;
    let Some(head_sha) = git.resolve(head_in, &head).await? else {
        return Err(ApiError::validation("PullRequest", "head", "invalid", ""));
    };
    if git.resolve(repo, base).await?.is_none() {
        return Err(ApiError::validation("PullRequest", "base", "invalid", ""));
    }
    let fork_id = fork.as_ref().map(|f| f.id.clone()).unwrap_or_default();
    for c in list_crs(cx, repo, "open", Some(base), "").await? {
        if head_ref(cx, repo, &c).await == head && c.head_repo_id == fork_id {
            let (owner, _) = shapes::split(head_in);
            return Err(ApiError::validation(
                "PullRequest",
                "",
                "custom",
                &format!("A pull request already exists for {owner}:{head}."),
            ));
        }
    }
    let cs = git
        .changeset(head_in, &head_sha)
        .await?
        .ok_or_else(|| ApiError::validation("PullRequest", "head", "invalid", ""))?;
    let c = svc
        .open_change(cx.req(OpenChangeRequest {
            repo: rid(&repo.id),
            target_ref: base.to_string(),
            head: Some(cs),
            title: title.to_string(),
            body: body.to_string(),
            draft,
            head_repo: fork.as_ref().and_then(|f| rid(&f.id)),
            head_ref: head.clone(),
            allow_maintainer_edits: maintainer_can_modify && fork.is_some(),
            ..Default::default()
        }))
        .await
        .map_err(|st| {
            let e = ApiError::from(st);
            if e.reason == ErrorReason::EmptyChange {
                ApiError::validation(
                    "PullRequest",
                    "",
                    "custom",
                    &format!("No commits between {base} and {head}"),
                )
            } else {
                e
            }
        })?
        .into_inner();
    cx.hub
        .ports
        .state
        .put(&head_key(&repo.id, &c.id), &head)
        .await?;
    Ok(c)
}

/// `POST /repos/{owner}/{repo}/pulls`.
pub(crate) async fn create(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    if body.get("issue").is_some() && s(&body, "title").is_none() {
        return Err(ApiError::validation(
            "PullRequest",
            "issue",
            "invalid",
            "converting an issue into a pull request is not supported; give a title",
        ));
    }
    let head = need(&body, "head", "PullRequest")?.to_string();
    let base = need(&body, "base", "PullRequest")?.to_string();
    let title = need(&body, "title", "PullRequest")?.to_string();
    // `head_repo` names the fork explicitly (`owner/name`), for forks in the same namespace.
    let head_repo = match s(&body, "head_repo").and_then(|n| n.split_once('/')) {
        Some((o, n)) => Some(super::repo_named(&cx, o, n).await?),
        None => None,
    };
    let c = open(
        &cx,
        &repo,
        &head,
        &base,
        &title,
        s(&body, "body").unwrap_or(""),
        b(&body, "draft").unwrap_or(false),
        head_repo,
        b(&body, "maintainer_can_modify").unwrap_or(false),
    )
    .await?;
    let v = pull_full(&cx, &repo, &c).await;
    let loc = v["url"].as_str().unwrap_or("").to_string();
    Ok(Reply::created(v).header("location", loc))
}

/// `PATCH /repos/{owner}/{repo}/pulls/{number}`.
pub(crate) async fn update(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let mut c = cr_of(&cx, &repo, &p).await?;
    let svc = changes(&cx)?;
    let body = cx.body_json()?;
    let title = s(&body, "title").map(str::to_string);
    let text = s(&body, "body").map(str::to_string);
    let base = s(&body, "base").map(str::to_string);
    let maintainer = b(&body, "maintainer_can_modify");
    if title.is_some() || text.is_some() || base.is_some() || maintainer.is_some() {
        c = svc
            .edit_change(cx.req(EditChangeRequest {
                repo: rid(&repo.id),
                id: c.id.clone(),
                title,
                body: text,
                target_ref: base,
                depends_on: None,
                expected_version: 0,
                allow_maintainer_edits: maintainer,
            }))
            .await?
            .into_inner();
    }
    match s(&body, "state") {
        Some("closed") if shapes::cr_state(&c).0 == "open" => {
            c = svc
                .close_change(cx.req(CloseChangeRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                    reason: String::new(),
                }))
                .await?
                .into_inner();
        }
        Some("open") if shapes::cr_state(&c) == ("closed", false) => {
            c = svc
                .reopen_change(cx.req(ReopenChangeRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                }))
                .await?
                .into_inner();
        }
        Some("open" | "closed") | None => {}
        Some(other) => {
            return Err(ApiError::validation(
                "PullRequest",
                "state",
                "invalid",
                other,
            ));
        }
    }
    match b(&body, "draft") {
        Some(true) if !c.draft => {
            c = svc
                .convert_change_to_draft(cx.req(ConvertChangeToDraftRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                }))
                .await?
                .into_inner();
        }
        Some(false) if c.draft => {
            c = svc
                .mark_change_ready(cx.req(MarkChangeReadyRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                }))
                .await?
                .into_inner();
        }
        _ => {}
    }
    Ok(Reply::json(pull_full(&cx, &repo, &c).await))
}

/// `GET /repos/{owner}/{repo}/pulls/{number}/commits`.
pub(crate) async fn commits(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let (head, base) = shas(&cx, &repo, &c).await;
    let cmp = cx.hub.ports.git.compare(&repo, &base, &head).await?;
    let mut items = Vec::new();
    for gc in &cmp.ahead {
        items.push(shapes::commit(&cx, &repo, gc).await);
    }
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/pulls/{number}/files`.
pub(crate) async fn files(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let (head, base) = shas(&cx, &repo, &c).await;
    let cmp = cx.hub.ports.git.compare(&repo, &base, &head).await?;
    let items = cmp
        .files
        .iter()
        .map(|f| super::git::file_json(&cx, &repo, &head, f))
        .collect();
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/pulls/{number}/merge`: `204` when merged.
pub(crate) async fn is_merged(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    if shapes::cr_state(&c).1 {
        Ok(Reply::no_content())
    } else {
        Err(ApiError::not_found())
    }
}

/// Lands a PR; shared by REST and GraphQL `mergePullRequest`. Returns the CR and the git id
/// of the landed tip.
pub(crate) async fn land(
    cx: &Ctx,
    repo: &Repo,
    c: &ChangeRequest,
    method: LandMethod,
    title: Option<&str>,
    message: Option<&str>,
    expected_head: Option<&str>,
) -> ApiResult<(ChangeRequest, String)> {
    let svc = changes(cx)?;
    match shapes::cr_state(c) {
        (_, true) => {
            return Err(ApiError::new(
                405,
                ErrorReason::AlreadyLanded,
                "Pull Request is already merged",
            ));
        }
        ("closed", _) => {
            return Err(ApiError::new(
                405,
                ErrorReason::InvalidState,
                "Pull Request is closed",
            ));
        }
        _ => {}
    }
    if let Some(want) = expected_head {
        let (head, _) = shas(cx, repo, c).await;
        if !want.is_empty() && want != head {
            return Err(ApiError::new(
                409,
                ErrorReason::VersionMismatch,
                "Head branch was modified. Review and try the merge again.",
            ));
        }
    }
    let msg = match (title, message) {
        (Some(t), Some(m)) if !m.is_empty() => format!("{t}\n\n{m}"),
        (Some(t), _) => t.to_string(),
        (None, Some(m)) => m.to_string(),
        (None, None) => String::new(),
    };
    let r = svc
        .land_change(cx.req(LandChangeRequest {
            repo: rid(&repo.id),
            id: c.id.clone(),
            method: method as i32,
            message: msg,
            expected_revision: 0,
        }))
        .await
        .map_err(|st| {
            let e = ApiError::from(st);
            match e.reason {
                ErrorReason::PolicyDenied | ErrorReason::PermissionDenied | ErrorReason::Draft => {
                    e.with_status(405)
                }
                ErrorReason::MergeConflict => e
                    .with_status(405)
                    .with_message("Pull Request is not mergeable"),
                _ => e,
            }
        })?
        .into_inner();
    let cr = r.change.unwrap_or_else(|| c.clone());
    let sha = match r.land.and_then(|l| l.r#ref).and_then(|rf| rf.target) {
        Some(t) => cx.hub.ports.git.git_id(repo, &t).await?.unwrap_or_default(),
        None => cx
            .hub
            .ports
            .git
            .resolve(repo, &cr.target_ref)
            .await?
            .unwrap_or_default(),
    };
    Ok((cr, sha))
}

/// `PUT /repos/{owner}/{repo}/pulls/{number}/merge`.
pub(crate) async fn merge(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let body = cx.body_json()?;
    let method = method_of(s(&body, "merge_method"))?;
    let (_, sha) = land(
        &cx,
        &repo,
        &c,
        method,
        s(&body, "commit_title"),
        s(&body, "commit_message"),
        s(&body, "sha"),
    )
    .await?;
    Ok(Reply::json(json!({
        "sha": sha,
        "merged": true,
        "message": "Pull Request successfully merged",
    })))
}

/// `PUT /repos/{owner}/{repo}/pulls/{number}/update-branch`.
pub(crate) async fn update_branch(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let body = cx.body_json()?;
    if let Some(want) = s(&body, "expected_head_sha") {
        let (head, _) = shas(&cx, &repo, &c).await;
        if want != head {
            return Err(ApiError::unprocessable(
                "expected head sha didn't match current head ref.",
            ));
        }
    }
    changes(&cx)?
        .update_change_base(cx.req(UpdateChangeBaseRequest {
            repo: rid(&repo.id),
            id: c.id.clone(),
            method: LandMethod::MergeCommit as i32,
        }))
        .await?;
    Ok(Reply::accepted(json!({
        "message": "Updating pull request branch.",
        "url": cx.html(&format!("/{}/pull/{}", repo.full_name, c.number)),
    })))
}

fn verdict_state(r: &Review) -> &'static str {
    if r.dismissed {
        return "DISMISSED";
    }
    match Verdict::try_from(r.verdict).unwrap_or_default() {
        Verdict::Approve => "APPROVED",
        Verdict::RequestChanges => "CHANGES_REQUESTED",
        _ => "COMMENTED",
    }
}

/// GitHub's review object.
pub(crate) async fn review_json(cx: &Ctx, repo: &Repo, c: &ChangeRequest, r: &Review) -> Value {
    let id = cx
        .hub
        .ids
        .remember("review", &r.id, &format!("{}:{}", repo.id, c.number));
    let html = cx.html(&format!(
        "/{}/pull/{}#pullrequestreview-{id}",
        repo.full_name, c.number
    ));
    let pr = cx.api(&format!("/repos/{}/pulls/{}", repo.full_name, c.number));
    let commit = match c
        .revisions
        .iter()
        .find(|rv| rv.n == r.revision)
        .and_then(|rv| rv.head.as_ref())
    {
        Some(h) => cx.hub.ports.git.git_id(repo, h).await.ok().flatten(),
        None => None,
    };
    json!({
        "id": id,
        "node_id": crate::ids::node("PullRequestReview", &r.id),
        "user": shapes::user(cx, r.reviewer.as_ref().unwrap_or(&Principal::default())).await,
        "body": r.body,
        "state": verdict_state(r),
        "html_url": html,
        "pull_request_url": pr,
        "author_association": "MEMBER",
        "_links": {"html": {"href": html}, "pull_request": {"href": pr}},
        "submitted_at": iso(&r.created_at),
        "commit_id": commit,
    })
}

async fn reviews_of(cx: &Ctx, repo: &Repo, c: &ChangeRequest) -> ApiResult<Vec<Review>> {
    Ok(changes(cx)?
        .list_reviews(cx.req(ListReviewsRequest {
            repo: rid(&repo.id),
            change_request_id: c.id.clone(),
        }))
        .await?
        .into_inner()
        .reviews)
}

/// `GET /repos/{owner}/{repo}/pulls/{number}/reviews`.
pub(crate) async fn reviews(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let mut items = Vec::new();
    for r in reviews_of(&cx, &repo, &c).await? {
        items.push(review_json(&cx, &repo, &c, &r).await);
    }
    Ok(cx.paginate(items))
}

async fn review_by_id(cx: &Ctx, repo: &Repo, c: &ChangeRequest, id: u64) -> ApiResult<Review> {
    reviews_of(cx, repo, c)
        .await?
        .into_iter()
        .find(|r| crate::ids::num("review", &r.id) == id)
        .ok_or_else(ApiError::not_found)
}

/// `GET /repos/{owner}/{repo}/pulls/{number}/reviews/{id}`.
pub(crate) async fn review(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let r = review_by_id(&cx, &repo, &c, p.num("id")?).await?;
    Ok(Reply::json(review_json(&cx, &repo, &c, &r).await))
}

/// Line comments of a review body (`comments: [{path, line, side, body, start_line}]`).
fn draft_comments(body: &Value) -> ApiResult<Vec<ReviewComment>> {
    let mut out = Vec::new();
    for c in body
        .get("comments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = need(c, "path", "PullRequestReviewComment")?;
        let text = need(c, "body", "PullRequestReviewComment")?;
        let line = super::u(c, "line")
            .or_else(|| super::u(c, "position"))
            .unwrap_or(0) as u32;
        let start = super::u(c, "start_line").map(|v| v as u32).unwrap_or(line);
        let side = if s(c, "side") == Some("LEFT") {
            "old"
        } else {
            "new"
        };
        out.push(ReviewComment {
            path: path.as_bytes().to_vec(),
            side: side.into(),
            start_line: start,
            end_line: line,
            body: text.to_string(),
            file_level: line == 0,
            ..Default::default()
        });
    }
    Ok(out)
}

/// Submits a review; shared by REST and GraphQL `addPullRequestReview`.
pub(crate) async fn submit_review(
    cx: &Ctx,
    repo: &Repo,
    c: &ChangeRequest,
    event: &str,
    text: &str,
    comments: Vec<ReviewComment>,
) -> ApiResult<Review> {
    let verdict = match event {
        "APPROVE" => Verdict::Approve,
        "REQUEST_CHANGES" => Verdict::RequestChanges,
        "COMMENT" => Verdict::Comment,
        "" | "PENDING" => {
            return Err(ApiError::validation(
                "PullRequestReview",
                "event",
                "invalid",
                "pending reviews are not supported; give event APPROVE, REQUEST_CHANGES or COMMENT",
            ));
        }
        other => {
            return Err(ApiError::validation(
                "PullRequestReview",
                "event",
                "invalid",
                other,
            ));
        }
    };
    if verdict != Verdict::Approve && text.is_empty() && comments.is_empty() {
        return Err(ApiError::unprocessable(format!(
            "Review Can not {} without a body.",
            if verdict == Verdict::RequestChanges {
                "request changes"
            } else {
                "comment"
            }
        )));
    }
    Ok(changes(cx)?
        .submit_review(cx.req(SubmitReviewRequest {
            repo: rid(&repo.id),
            change_request_id: c.id.clone(),
            revision: 0,
            verdict: verdict as i32,
            body: text.to_string(),
            comments,
        }))
        .await?
        .into_inner())
}

/// `POST /repos/{owner}/{repo}/pulls/{number}/reviews`.
pub(crate) async fn create_review(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let body = cx.body_json()?;
    if let Some(want) = s(&body, "commit_id") {
        let (head, _) = shas(&cx, &repo, &c).await;
        if want != head && !head.is_empty() {
            return Err(ApiError::validation(
                "PullRequestReview",
                "commit_id",
                "invalid",
                "commit_id is not the head of the pull request",
            ));
        }
    }
    let r = submit_review(
        &cx,
        &repo,
        &c,
        s(&body, "event").unwrap_or(""),
        s(&body, "body").unwrap_or(""),
        draft_comments(&body)?,
    )
    .await?;
    Ok(Reply::json(review_json(&cx, &repo, &c, &r).await))
}

/// `PUT /repos/{owner}/{repo}/pulls/{number}/reviews/{id}/dismissals`.
pub(crate) async fn dismiss_review(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let r = review_by_id(&cx, &repo, &c, p.num("id")?).await?;
    let body = cx.body_json()?;
    let r = changes(&cx)?
        .dismiss_review(cx.req(DismissReviewRequest {
            repo: rid(&repo.id),
            change_request_id: c.id.clone(),
            review_id: r.id.clone(),
            reason: need(&body, "message", "PullRequestReview")?.to_string(),
        }))
        .await?
        .into_inner();
    Ok(Reply::json(review_json(&cx, &repo, &c, &r).await))
}

/// GitHub's review comment object.
pub(crate) async fn review_comment_json(
    cx: &Ctx,
    repo: &Repo,
    c: &ChangeRequest,
    rc: &ReviewComment,
    review_id: Option<u64>,
) -> Value {
    let id = cx.hub.ids.remember(
        "review_comment",
        &rc.id,
        &format!("{}:{}", repo.id, c.number),
    );
    let url = cx.api(&format!("/repos/{}/pulls/comments/{id}", repo.full_name));
    let pr = cx.api(&format!("/repos/{}/pulls/{}", repo.full_name, c.number));
    let html = cx.html(&format!(
        "/{}/pull/{}#discussion_r{id}",
        repo.full_name, c.number
    ));
    let (head, _) = shas(cx, repo, c).await;
    let side = if rc.side == "old" { "LEFT" } else { "RIGHT" };
    let line = if rc.file_level {
        Value::Null
    } else {
        json!(rc.end_line)
    };
    let start = if rc.file_level || rc.start_line == rc.end_line {
        Value::Null
    } else {
        json!(rc.start_line)
    };
    json!({
        "url": url,
        "pull_request_review_id": review_id,
        "id": id,
        "node_id": crate::ids::node("PullRequestReviewComment", &rc.id),
        "diff_hunk": "",
        "path": String::from_utf8_lossy(&rc.path),
        "position": line,
        "original_position": line,
        "commit_id": head,
        "original_commit_id": head,
        "in_reply_to_id": if rc.in_reply_to.is_empty() { Value::Null } else { json!(crate::ids::num("review_comment", &rc.in_reply_to)) },
        "user": shapes::user(cx, rc.author.as_ref().unwrap_or(&Principal::default())).await,
        "body": rc.body,
        "created_at": iso(&rc.created_at),
        "updated_at": iso(&rc.created_at),
        "html_url": html,
        "pull_request_url": pr,
        "author_association": "MEMBER",
        "_links": {"self": {"href": url}, "html": {"href": html}, "pull_request": {"href": pr}},
        "start_line": start,
        "original_start_line": start,
        "start_side": if start.is_null() { Value::Null } else { json!(side) },
        "line": line,
        "original_line": line,
        "side": side,
        "subject_type": if rc.file_level { "file" } else { "line" },
        "reactions": shapes::reactions(&format!("{url}/reactions"), &rc.reactions),
    })
}

/// Every review thread of a CR.
pub(crate) async fn threads(
    cx: &Ctx,
    repo: &Repo,
    c: &ChangeRequest,
) -> ApiResult<Vec<ReviewThread>> {
    let svc = changes(cx)?.clone();
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListReviewThreadsRequest {
            repo: rid(&repo.id),
            change_request_id: c.id.clone(),
            resolved: None,
            page: Some(page),
        });
        async move {
            let r = svc.list_review_threads(req).await?.into_inner();
            Ok((r.threads, r.page))
        }
    })
    .await
}

async fn comments_json(
    cx: &Ctx,
    repo: &Repo,
    c: &ChangeRequest,
    only_review: Option<&Review>,
) -> ApiResult<Vec<Value>> {
    let reviews = reviews_of(cx, repo, c).await?;
    let review_of = |id: &str| {
        reviews
            .iter()
            .find(|r| r.comments.iter().any(|rc| rc.id == id))
            .map(|r| crate::ids::num("review", &r.id))
    };
    let mut items = Vec::new();
    for t in threads(cx, repo, c).await? {
        for rc in &t.comments {
            if let Some(r) = only_review
                && !r.comments.iter().any(|x| x.id == rc.id)
            {
                continue;
            }
            items.push(review_comment_json(cx, repo, c, rc, review_of(&rc.id)).await);
        }
    }
    Ok(items)
}

/// `GET /repos/{owner}/{repo}/pulls/{number}/comments`.
pub(crate) async fn review_comments(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let items = comments_json(&cx, &repo, &c, None).await?;
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/pulls/{number}/reviews/{id}/comments`.
pub(crate) async fn review_comments_of(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let r = review_by_id(&cx, &repo, &c, p.num("id")?).await?;
    let items = comments_json(&cx, &repo, &c, Some(&r)).await?;
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/pulls/comments`: review comments across the repo.
pub(crate) async fn repo_review_comments(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let mut items = Vec::new();
    for c in list_crs(&cx, &repo, "all", None, "").await? {
        items.extend(comments_json(&cx, &repo, &c, None).await?);
    }
    Ok(cx.paginate(items))
}

/// Finds a review comment by numeric id across the repo's CRs.
async fn find_review_comment(
    cx: &Ctx,
    repo: &Repo,
    id: u64,
) -> ApiResult<(ChangeRequest, ReviewThread, ReviewComment)> {
    let hint = cx.hub.ids.get("review_comment", id);
    let mut crs = Vec::new();
    if let Some(h) = &hint
        && let Some(n) = h.context.rsplit(':').next().and_then(|n| n.parse().ok())
        && let Some(c) = cr_by_number(cx, repo, n).await?
    {
        crs.push(c);
    }
    if crs.is_empty() {
        crs = list_crs(cx, repo, "all", None, "").await?;
    }
    for c in crs {
        for t in threads(cx, repo, &c).await? {
            if let Some(rc) = t
                .comments
                .iter()
                .find(|rc| crate::ids::num("review_comment", &rc.id) == id)
                .cloned()
            {
                return Ok((c, t, rc));
            }
        }
    }
    Err(ApiError::not_found())
}

/// `GET /repos/{owner}/{repo}/pulls/comments/{id}`.
pub(crate) async fn review_comment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (c, _, rc) = find_review_comment(&cx, &repo, p.num("id")?).await?;
    Ok(Reply::json(
        review_comment_json(&cx, &repo, &c, &rc, None).await,
    ))
}

/// `POST /repos/{owner}/{repo}/pulls/{number}/comments`.
pub(crate) async fn create_review_comment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let body = cx.body_json()?;
    let text = need(&body, "body", "PullRequestReviewComment")?.to_string();
    if let Some(reply_to) = super::u(&body, "in_reply_to") {
        return reply_to_comment(&cx, &repo, &c, reply_to, &text).await;
    }
    let path = need(&body, "path", "PullRequestReviewComment")?.to_string();
    let file_level = s(&body, "subject_type") == Some("file");
    let line = super::u(&body, "line")
        .or_else(|| super::u(&body, "position"))
        .unwrap_or(0) as u32;
    if line == 0 && !file_level {
        return Err(ApiError::missing_field("PullRequestReviewComment", "line"));
    }
    let start = super::u(&body, "start_line")
        .map(|v| v as u32)
        .unwrap_or(line);
    let created = changes(&cx)?
        .create_change_comment(cx.req(CreateChangeCommentRequest {
            repo: rid(&repo.id),
            change_request_id: c.id.clone(),
            body: text,
            path: path.into_bytes(),
            side: if s(&body, "side") == Some("LEFT") {
                "old".into()
            } else {
                "new".into()
            },
            start_line: start,
            end_line: line,
            file_level,
            revision: 0,
            suggestion: None,
        }))
        .await?
        .into_inner();
    let found = threads(&cx, &repo, &c)
        .await?
        .into_iter()
        .flat_map(|t| t.comments)
        .find(|rc| rc.id == created.id);
    let rc = found.unwrap_or_else(|| ReviewComment {
        id: created.id.clone(),
        body: created.body.clone(),
        author: created.author.clone(),
        created_at: created.created_at,
        ..Default::default()
    });
    Ok(Reply::created(
        review_comment_json(&cx, &repo, &c, &rc, None).await,
    ))
}

async fn reply_to_comment(
    cx: &Ctx,
    repo: &Repo,
    c: &ChangeRequest,
    to: u64,
    text: &str,
) -> ApiResult<Reply> {
    let (_, thread, _) = find_review_comment(cx, repo, to).await?;
    let rc = changes(cx)?
        .reply_to_review_thread(cx.req(ReplyToReviewThreadRequest {
            repo: rid(&repo.id),
            thread_id: thread.id.clone(),
            body: text.to_string(),
            suggestion: None,
        }))
        .await?
        .into_inner();
    Ok(Reply::created(
        review_comment_json(cx, repo, c, &rc, None).await,
    ))
}

/// `POST /repos/{owner}/{repo}/pulls/{number}/comments/{id}/replies`.
pub(crate) async fn reply_review_comment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let body = cx.body_json()?;
    let text = need(&body, "body", "PullRequestReviewComment")?.to_string();
    reply_to_comment(&cx, &repo, &c, p.num("id")?, &text).await
}

async fn requested_json(cx: &Ctx, repo: &Repo, c: &ChangeRequest) -> Value {
    let (owner, _) = shapes::split(repo);
    let mut users = Vec::new();
    let mut teams = Vec::new();
    for rr in &c.review_requests {
        match &rr.reviewer {
            Some(p) if !p.id.is_empty() => users.push(shapes::user(cx, p).await),
            _ if !rr.team_id.is_empty() => {
                teams.push(super::orgs::team_ref(cx, owner, &rr.team_id).await)
            }
            _ => {}
        }
    }
    json!({"users": users, "teams": teams})
}

/// `GET /repos/{owner}/{repo}/pulls/{number}/requested_reviewers`.
pub(crate) async fn requested_reviewers(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    Ok(Reply::json(requested_json(&cx, &repo, &c).await))
}

/// Principal and team ids for GitHub's `reviewers` / `team_reviewers`.
pub(crate) async fn reviewer_ids(
    cx: &Ctx,
    repo: &Repo,
    logins: &[String],
    slugs: &[String],
) -> ApiResult<(Vec<String>, Vec<String>)> {
    let mut ids = Vec::new();
    for l in logins {
        ids.push(cx.id_of_login(l, "reviewers").await?);
    }
    let (owner, _) = shapes::split(repo);
    let mut teams = Vec::new();
    for t in slugs {
        teams.push(super::orgs::team_id(cx, owner, t).await?);
    }
    Ok((ids, teams))
}

/// `POST /repos/{owner}/{repo}/pulls/{number}/requested_reviewers`.
pub(crate) async fn request_reviewers(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let body = cx.body_json()?;
    let (ids, teams) = reviewer_ids(
        &cx,
        &repo,
        &strs(&body, "reviewers").unwrap_or_default(),
        &strs(&body, "team_reviewers").unwrap_or_default(),
    )
    .await?;
    let c = changes(&cx)?
        .request_reviewers(cx.req(RequestReviewersRequest {
            repo: rid(&repo.id),
            id: c.id.clone(),
            reviewer_ids: ids,
            team_ids: teams,
        }))
        .await?
        .into_inner();
    Ok(Reply::created(pull(&cx, &repo, &c, None, None).await))
}

/// `DELETE /repos/{owner}/{repo}/pulls/{number}/requested_reviewers`.
pub(crate) async fn remove_reviewers(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let c = cr_of(&cx, &repo, &p).await?;
    let body = cx.body_json()?;
    let (ids, teams) = reviewer_ids(
        &cx,
        &repo,
        &strs(&body, "reviewers").unwrap_or_default(),
        &strs(&body, "team_reviewers").unwrap_or_default(),
    )
    .await?;
    let c = changes(&cx)?
        .remove_review_requests(cx.req(RemoveReviewRequestsRequest {
            repo: rid(&repo.id),
            id: c.id.clone(),
            reviewer_ids: ids,
            team_ids: teams,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(pull(&cx, &repo, &c, None, None).await))
}

/// `GET /repos/{owner}/{repo}/commits/{sha}/pulls`: PRs whose head or landed tip is the
/// commit.
pub(crate) async fn for_commit(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let sha = super::git::resolve(&cx, &repo, p.get("ref")).await?;
    let mut items = Vec::new();
    if cx.hub.changes.is_some() {
        for c in list_crs(&cx, &repo, "all", None, "").await? {
            let (head, _) = shas(&cx, &repo, &c).await;
            if head == sha {
                items.push(pull(&cx, &repo, &c, None, None).await);
            }
        }
    }
    Ok(cx.paginate(items))
}
