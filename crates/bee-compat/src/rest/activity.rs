//! Activity: stars, watching, notifications, repo events and gists.

use std::sync::Arc;

use bee_proto::v1::event_service_server::EventService as _;
use bee_proto::v1::gist_service_server::GistService as _;
use bee_proto::v1::notification_service_server::NotificationService as _;
use bee_proto::v1::repo_service_server::RepoService as _;
use bee_proto::v1::user_service_server::UserService as _;
use bee_proto::v1::{
    CreateGistCommentRequest, CreateGistRequest, DeleteGistRequest, DeleteRepoWatchRequest,
    ForkGistRequest, GetGistRequest, GetGistRevisionRequest, GetNotificationSubscriptionRequest,
    GetNotificationThreadRequest, GetRepoWatchRequest, Gist, GistFileChange, GistListFilter,
    IgnoreNotificationThreadRequest, IsGistStarredRequest, IsRepoStarredRequest, ListEventsRequest,
    ListGistCommentsRequest, ListGistForksRequest, ListGistRevisionsRequest, ListGistsRequest,
    ListNotificationThreadsRequest, ListRepoStargazersRequest, ListRepoWatchersRequest,
    ListStarredReposRequest, ListWatchedReposRequest, MarkNotificationThreadsDoneRequest,
    MarkNotificationThreadsReadRequest, NotificationReason, NotificationSubscriptionState,
    NotificationThread, NotificationThreadFilter, NotificationThreadSelector, ObjectId, Principal,
    RepoWatchLevel, SetRepoWatchRequest, StarGistRequest, StarRepoRequest, SubjectKind,
    SubscribeNotificationThreadRequest, UnstarGistRequest, UnstarRepoRequest,
    UnsubscribeNotificationThreadRequest, UpdateGistRequest,
};
use serde_json::{Map, Value, json};

use super::{b, repo_named, repo_of, rid, s};
use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Reply, all_pages};
use crate::shapes;
use crate::time::{iso, iso_or, parse_iso, ts_secs};

// ----- stars --------------------------------------------------------------------------------

async fn starred_of(cx: &Ctx, login: &str) -> ApiResult<Reply> {
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListStarredReposRequest {
            login: login.to_string(),
            language: String::new(),
            sort: cx.q("sort").unwrap_or("created").to_string(),
            page: Some(page),
        });
        async move {
            let r = svc.list_starred_repos(req).await?.into_inner();
            Ok((r.repos, r.page))
        }
    })
    .await?;
    let star_json = cx.accepts("star+json");
    let mut items = Vec::new();
    for st in list {
        let Some((o, n)) = st.full_name.split_once('/') else {
            continue;
        };
        let Ok(repo) = repo_named(cx, o, n).await else {
            continue;
        };
        let v = shapes::repo(cx, &repo, None, None);
        items.push(if star_json {
            json!({"starred_at": iso(&st.starred_at), "repo": v})
        } else {
            v
        });
    }
    Ok(cx.paginate(items))
}

/// `GET /user/starred`.
pub(crate) async fn my_starred(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    starred_of(&cx, "").await
}

/// `GET /users/{user}/starred`.
pub(crate) async fn user_starred(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    starred_of(&cx, p.get("user")).await
}

/// `GET /user/starred/{owner}/{repo}`: `204` or `404`.
pub(crate) async fn is_starred(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let r = cx
        .hub
        .svc
        .is_repo_starred(cx.req(IsRepoStarredRequest {
            repo: rid(&repo.id),
        }))
        .await?
        .into_inner();
    if r.starred {
        Ok(Reply::no_content())
    } else {
        Err(ApiError::not_found())
    }
}

/// `PUT /user/starred/{owner}/{repo}`.
pub(crate) async fn star(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    cx.hub
        .svc
        .star_repo(cx.req(StarRepoRequest {
            repo: rid(&repo.id),
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `DELETE /user/starred/{owner}/{repo}`.
pub(crate) async fn unstar(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    cx.hub
        .svc
        .unstar_repo(cx.req(UnstarRepoRequest {
            repo: rid(&repo.id),
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `GET /repos/{owner}/{repo}/stargazers`.
pub(crate) async fn stargazers(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListRepoStargazersRequest {
            repo: rid(&repo.id),
            page: Some(page),
        });
        async move {
            let r = svc.list_repo_stargazers(req).await?.into_inner();
            Ok((r.stargazers, r.page))
        }
    })
    .await?;
    let star_json = cx.accepts("star+json");
    let mut items = Vec::new();
    for g in list {
        let u = g.user.unwrap_or_default();
        let mut v = shapes::user(&cx, &u.principal.unwrap_or_default()).await;
        if !u.login.is_empty() {
            v["login"] = json!(u.login);
        }
        items.push(if star_json {
            json!({"starred_at": iso(&g.starred_at), "user": v})
        } else {
            v
        });
    }
    Ok(cx.paginate(items))
}

// ----- watching -----------------------------------------------------------------------------

/// `GET /repos/{owner}/{repo}/subscribers`.
pub(crate) async fn watchers(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListRepoWatchersRequest {
            repo: rid(&repo.id),
            page: Some(page),
        });
        async move {
            let r = svc.list_repo_watchers(req).await?.into_inner();
            Ok((r.watchers, r.page))
        }
    })
    .await?;
    Ok(cx.paginate(shapes::users(&cx, &list).await))
}

fn subscription_json(
    cx: &Ctx,
    full: &str,
    level: RepoWatchLevel,
    at: &Option<pbjson_types::Timestamp>,
) -> Value {
    json!({
        "subscribed": level == RepoWatchLevel::All || level == RepoWatchLevel::Custom,
        "ignored": level == RepoWatchLevel::Ignore,
        "reason": Value::Null,
        "created_at": iso(at),
        "url": cx.api(&format!("/repos/{full}/subscription")),
        "repository_url": cx.api(&format!("/repos/{full}")),
    })
}

/// `GET /repos/{owner}/{repo}/subscription`.
pub(crate) async fn subscription(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let w = cx
        .hub
        .svc
        .get_repo_watch(cx.req(GetRepoWatchRequest {
            repo: rid(&repo.id),
        }))
        .await?
        .into_inner();
    let level = RepoWatchLevel::try_from(w.level).unwrap_or_default();
    if level == RepoWatchLevel::Unspecified || level == RepoWatchLevel::Participating {
        return Err(ApiError::not_found());
    }
    Ok(Reply::json(subscription_json(
        &cx,
        &repo.full_name,
        level,
        &w.created_at,
    )))
}

/// `PUT /repos/{owner}/{repo}/subscription`.
pub(crate) async fn set_subscription(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let level = if b(&body, "ignored") == Some(true) {
        RepoWatchLevel::Ignore
    } else if b(&body, "subscribed") == Some(false) {
        RepoWatchLevel::Participating
    } else {
        RepoWatchLevel::All
    };
    let w = cx
        .hub
        .svc
        .set_repo_watch(cx.req(SetRepoWatchRequest {
            repo: rid(&repo.id),
            level: level as i32,
            custom_events: Vec::new(),
        }))
        .await?
        .into_inner();
    Ok(Reply::json(subscription_json(
        &cx,
        &repo.full_name,
        level,
        &w.created_at,
    )))
}

/// `DELETE /repos/{owner}/{repo}/subscription`.
pub(crate) async fn delete_subscription(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    cx.hub
        .svc
        .delete_repo_watch(cx.req(DeleteRepoWatchRequest {
            repo: rid(&repo.id),
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `GET /user/subscriptions`.
pub(crate) async fn my_subscriptions(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListWatchedReposRequest {
            level: RepoWatchLevel::All as i32,
            page: Some(page),
        });
        async move {
            let r = svc.list_watched_repos(req).await?.into_inner();
            Ok((r.watches, r.page))
        }
    })
    .await?;
    let mut items = Vec::new();
    for w in list {
        if let Ok(r) = cx
            .hub
            .svc
            .get_repo(cx.req(bee_proto::v1::GetRepoRequest {
                repo: w.repo.clone(),
            }))
            .await
        {
            items.push(shapes::repo(&cx, &r.into_inner(), None, None));
        }
    }
    Ok(cx.paginate(items))
}

// ----- notifications ------------------------------------------------------------------------

fn reason_word(r: NotificationReason) -> &'static str {
    match r {
        NotificationReason::ReviewRequested => "review_requested",
        NotificationReason::Mention => "mention",
        NotificationReason::TeamMention => "team_mention",
        NotificationReason::Assign => "assign",
        NotificationReason::Author => "author",
        NotificationReason::Comment => "comment",
        NotificationReason::StateChange => "state_change",
        NotificationReason::CiActivity => "ci_activity",
        NotificationReason::SecurityAlert => "security_alert",
        NotificationReason::Manual => "manual",
        NotificationReason::AgentRun => "agent_run",
        _ => "subscribed",
    }
}

fn subject_type(k: SubjectKind) -> &'static str {
    match k {
        SubjectKind::Issue => "Issue",
        SubjectKind::ChangeRequest => "PullRequest",
        SubjectKind::Release => "Release",
        SubjectKind::Discussion => "Discussion",
        SubjectKind::CheckRun | SubjectKind::WorkflowRun => "CheckSuite",
        SubjectKind::Changeset => "Commit",
        SubjectKind::SecurityAlert => "RepositoryVulnerabilityAlert",
        _ => "Issue",
    }
}

async fn thread_json(cx: &Ctx, t: &NotificationThread) -> Value {
    let id = cx.hub.ids.remember("thread", &t.id, "").to_string();
    let repo = match t.repo_full_name.split_once('/') {
        Some((o, n)) => repo_named(cx, o, n).await.ok(),
        None => None,
    };
    let sub = t.subject.clone().unwrap_or_default();
    json!({
        "id": id,
        "repository": repo.as_ref().map(|r| shapes::repo(cx, r, None, None)).unwrap_or(Value::Null),
        "subject": {
            "title": t.title,
            "url": repo.as_ref().map(|r| json!(cx.api(&format!("/repos/{}", r.full_name)))).unwrap_or(Value::Null),
            "latest_comment_url": Value::Null,
            "type": subject_type(SubjectKind::try_from(sub.kind).unwrap_or_default()),
        },
        "reason": reason_word(NotificationReason::try_from(t.reason).unwrap_or_default()),
        "unread": t.unread,
        "updated_at": iso(&t.updated_at),
        "last_read_at": iso(&t.last_read_at),
        "url": cx.api(&format!("/notifications/threads/{id}")),
        "subscription_url": cx.api(&format!("/notifications/threads/{id}/subscription")),
    })
}

async fn threads(cx: &Ctx, filter: NotificationThreadFilter) -> ApiResult<Vec<NotificationThread>> {
    let svc = cx.hub.svc.clone();
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListNotificationThreadsRequest {
            filter: Some(filter.clone()),
            page: Some(page),
        });
        async move {
            let r = svc.list_notification_threads(req).await?.into_inner();
            Ok((r.threads, r.page))
        }
    })
    .await
}

fn filter_of(cx: &Ctx) -> NotificationThreadFilter {
    NotificationThreadFilter {
        unread_only: cx.q("all") != Some("true"),
        participating_only: cx.q("participating") == Some("true"),
        since: cx.q("since").and_then(parse_iso).map(ts_secs),
        before: cx.q("before").and_then(parse_iso).map(ts_secs),
        ..Default::default()
    }
}

/// `GET /notifications`.
pub(crate) async fn notifications(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    let list = threads(&cx, filter_of(&cx)).await?;
    let mut items = Vec::new();
    for t in &list {
        items.push(thread_json(&cx, t).await);
    }
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/notifications`.
pub(crate) async fn repo_notifications(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let mut f = filter_of(&cx);
    f.repos = vec![rid(&repo.id).unwrap_or_default()];
    let list = threads(&cx, f).await?;
    let mut items = Vec::new();
    for t in &list {
        items.push(thread_json(&cx, t).await);
    }
    Ok(cx.paginate(items))
}

/// `PUT /notifications`: marks everything (before `last_read_at`) read.
pub(crate) async fn mark_all_read(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    let body = cx.body_json()?;
    let before = s(&body, "last_read_at")
        .and_then(parse_iso)
        .map(ts_secs)
        .unwrap_or_else(|| ts_secs((cx.hub.ports.clock.now_ms() / 1000) as i64 + 1));
    cx.hub
        .svc
        .mark_notification_threads_read(cx.req(MarkNotificationThreadsReadRequest {
            selector: Some(NotificationThreadSelector {
                thread_ids: Vec::new(),
                filter: Some(NotificationThreadFilter::default()),
                before: Some(before),
            }),
        }))
        .await?;
    Ok(Reply::accepted(json!({"message": "Notifications marked as read."})).status(205))
}

async fn thread_id(cx: &Ctx, n: &str) -> ApiResult<String> {
    let num: u64 = n.parse().map_err(|_| ApiError::not_found())?;
    if let Some(ix) = cx.hub.ids.get("thread", num) {
        return Ok(ix.id);
    }
    threads(
        cx,
        NotificationThreadFilter {
            include_done: true,
            ..Default::default()
        },
    )
    .await?
    .into_iter()
    .find(|t| crate::ids::num("thread", &t.id) == num)
    .map(|t| t.id)
    .ok_or_else(ApiError::not_found)
}

/// `GET /notifications/threads/{id}`.
pub(crate) async fn thread(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let id = thread_id(&cx, p.get("id")).await?;
    let t = cx
        .hub
        .svc
        .get_notification_thread(cx.req(GetNotificationThreadRequest { thread_id: id }))
        .await?
        .into_inner();
    Ok(Reply::json(thread_json(&cx, &t).await))
}

/// `PATCH /notifications/threads/{id}`.
pub(crate) async fn mark_thread_read(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let id = thread_id(&cx, p.get("id")).await?;
    cx.hub
        .svc
        .mark_notification_threads_read(cx.req(MarkNotificationThreadsReadRequest {
            selector: Some(NotificationThreadSelector {
                thread_ids: vec![id],
                ..Default::default()
            }),
        }))
        .await?;
    Ok(Reply::no_content().status(205))
}

/// `DELETE /notifications/threads/{id}`: marks the thread done.
pub(crate) async fn mark_thread_done(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let id = thread_id(&cx, p.get("id")).await?;
    cx.hub
        .svc
        .mark_notification_threads_done(cx.req(MarkNotificationThreadsDoneRequest {
            selector: Some(NotificationThreadSelector {
                thread_ids: vec![id],
                ..Default::default()
            }),
        }))
        .await?;
    Ok(Reply::no_content())
}

async fn thread_subject(cx: &Ctx, n: &str) -> ApiResult<(String, bee_proto::v1::SubjectRef)> {
    let id = thread_id(cx, n).await?;
    let t = cx
        .hub
        .svc
        .get_notification_thread(cx.req(GetNotificationThreadRequest { thread_id: id }))
        .await?
        .into_inner();
    Ok((n.to_string(), t.subject.unwrap_or_default()))
}

fn thread_sub_json(
    cx: &Ctx,
    n: &str,
    st: NotificationSubscriptionState,
    at: &Option<pbjson_types::Timestamp>,
) -> Value {
    json!({
        "subscribed": st == NotificationSubscriptionState::Subscribed,
        "ignored": st == NotificationSubscriptionState::Ignored,
        "reason": Value::Null,
        "created_at": iso(at),
        "url": cx.api(&format!("/notifications/threads/{n}/subscription")),
        "thread_url": cx.api(&format!("/notifications/threads/{n}")),
    })
}

/// `GET /notifications/threads/{id}/subscription`.
pub(crate) async fn thread_subscription(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (n, sub) = thread_subject(&cx, p.get("id")).await?;
    let st = cx
        .hub
        .svc
        .get_notification_subscription(
            cx.req(GetNotificationSubscriptionRequest { subject: Some(sub) }),
        )
        .await?
        .into_inner();
    let state = NotificationSubscriptionState::try_from(st.state).unwrap_or_default();
    Ok(Reply::json(thread_sub_json(&cx, &n, state, &st.updated_at)))
}

/// `PUT /notifications/threads/{id}/subscription`.
pub(crate) async fn set_thread_subscription(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (n, sub) = thread_subject(&cx, p.get("id")).await?;
    let body = cx.body_json()?;
    let st = if b(&body, "ignored") == Some(true) {
        cx.hub
            .svc
            .ignore_notification_thread(
                cx.req(IgnoreNotificationThreadRequest { subject: Some(sub) }),
            )
            .await?
            .into_inner()
    } else {
        cx.hub
            .svc
            .subscribe_notification_thread(
                cx.req(SubscribeNotificationThreadRequest { subject: Some(sub) }),
            )
            .await?
            .into_inner()
    };
    let state = NotificationSubscriptionState::try_from(st.state).unwrap_or_default();
    Ok(Reply::json(thread_sub_json(&cx, &n, state, &st.updated_at)))
}

/// `DELETE /notifications/threads/{id}/subscription`.
pub(crate) async fn delete_thread_subscription(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let (_, sub) = thread_subject(&cx, p.get("id")).await?;
    cx.hub
        .svc
        .unsubscribe_notification_thread(
            cx.req(UnsubscribeNotificationThreadRequest { subject: Some(sub) }),
        )
        .await?;
    Ok(Reply::no_content())
}

// ----- repo events --------------------------------------------------------------------------

fn camel(name: &str) -> String {
    name.split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// `GET /repos/{owner}/{repo}/events`: the repo event log, newest first, in GitHub's event
/// shape (`type` from the event kind's GitHub name).
pub(crate) async fn repo_events(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let mut events = Vec::new();
    let mut from = 0;
    loop {
        let r = cx
            .hub
            .svc
            .list_events(cx.req(ListEventsRequest {
                repo: rid(&repo.id),
                org: String::new(),
                from_seq: from,
                kinds: Vec::new(),
                limit: 500,
            }))
            .await?
            .into_inner();
        let n = r.events.len();
        events.extend(r.events);
        if n == 0 || r.next_seq <= from || events.len() >= 3000 {
            break;
        }
        from = r.next_seq;
    }
    events.reverse();
    let (owner, _) = shapes::split(&repo);
    let _ = owner;
    let mut items = Vec::new();
    for e in events.iter().take(300) {
        let ty = match cx
            .hub
            .forge
            .registry
            .event_kind(&e.kind)
            .and_then(|k| k.github)
        {
            Some((name, _)) => format!("{}Event", camel(name)),
            None => format!("Bee{}Event", camel(&e.kind.replace('.', "_"))),
        };
        let action = cx
            .hub
            .forge
            .registry
            .event_kind(&e.kind)
            .and_then(|k| k.github)
            .map(|(_, a)| a)
            .unwrap_or("");
        let actor = shapes::user(
            &cx,
            &Principal {
                id: e.principal.clone(),
                ..Default::default()
            },
        )
        .await;
        items.push(json!({
            "id": e.seq.to_string(),
            "type": ty,
            "actor": actor,
            "repo": {"id": crate::ids::num("repo", &repo.id), "name": repo.full_name, "url": cx.api(&format!("/repos/{}", repo.full_name))},
            "payload": if action.is_empty() { json!({}) } else { json!({"action": action}) },
            "public": shapes::visibility(&repo) == "public",
            "created_at": iso(&e.time),
        }));
    }
    Ok(cx.paginate(items))
}

// ----- gists --------------------------------------------------------------------------------

async fn gist_json(cx: &Ctx, g: &Gist, with_content: bool) -> Value {
    let base = cx.api(&format!("/gists/{}", g.id));
    let mut files = Map::new();
    for f in &g.files {
        let mut v = json!({
            "filename": f.filename,
            "type": if f.content_type.is_empty() { "text/plain" } else { f.content_type.as_str() },
            "language": if f.language.is_empty() { Value::Null } else { json!(f.language) },
            "raw_url": cx.html(&format!("/gist/{}/raw/{}", g.id, f.filename)),
            "size": f.size,
        });
        if with_content {
            v["truncated"] = json!(f.truncated);
            v["content"] = json!(f.content);
        }
        files.insert(f.filename.clone(), v);
    }
    let owner = shapes::user(cx, g.owner.as_ref().unwrap_or(&Principal::default())).await;
    json!({
        "url": base,
        "forks_url": format!("{base}/forks"),
        "commits_url": format!("{base}/commits"),
        "id": g.id,
        "node_id": crate::ids::node("Gist", &g.id),
        "git_pull_url": g.clone_url_https,
        "git_push_url": g.clone_url_https,
        "html_url": if g.url.is_empty() { cx.html(&format!("/gist/{}", g.id)) } else { g.url.clone() },
        "files": Value::Object(files),
        "public": g.public,
        "created_at": iso(&g.created_at),
        "updated_at": iso_or(&g.updated_at, &g.created_at),
        "description": if g.description.is_empty() { Value::Null } else { json!(g.description) },
        "comments": g.comment_count,
        "comments_enabled": !g.comments_locked,
        "user": Value::Null,
        "comments_url": format!("{base}/comments"),
        "owner": owner,
        "truncated": false,
    })
}

async fn gists_list(cx: &Ctx, filter: GistListFilter, user: &str) -> ApiResult<Reply> {
    let svc = cx.hub.svc.clone();
    let since = cx.q("since").and_then(parse_iso).map(ts_secs);
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListGistsRequest {
            filter: filter as i32,
            user: user.to_string(),
            since,
            page: Some(page),
        });
        async move {
            let r = svc.list_gists(req).await?.into_inner();
            Ok((r.gists, r.page))
        }
    })
    .await?;
    let mut items = Vec::new();
    for g in &list {
        items.push(gist_json(cx, g, false).await);
    }
    Ok(cx.paginate(items))
}

/// `GET /gists`.
pub(crate) async fn my_gists(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    gists_list(&cx, GistListFilter::Mine, "").await
}

/// `GET /gists/public`.
pub(crate) async fn public_gists(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    gists_list(&cx, GistListFilter::Public, "").await
}

/// `GET /gists/starred`.
pub(crate) async fn starred_gists(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    gists_list(&cx, GistListFilter::Starred, "").await
}

/// `GET /users/{user}/gists`.
pub(crate) async fn user_gists(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    gists_list(&cx, GistListFilter::User, p.get("user")).await
}

/// `POST /gists`.
pub(crate) async fn create_gist(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let body = cx.body_json()?;
    let files = body
        .get("files")
        .and_then(Value::as_object)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ApiError::missing_field("Gist", "files"))?;
    let mut map = std::collections::HashMap::new();
    for (name, f) in files {
        let content =
            s(f, "content").ok_or_else(|| ApiError::missing_field("Gist", "files.content"))?;
        map.insert(name.clone(), content.to_string());
    }
    let public = match body.get("public") {
        Some(Value::Bool(v)) => *v,
        Some(Value::String(v)) => v == "true",
        _ => false,
    };
    let g = cx
        .hub
        .svc
        .create_gist(cx.req(CreateGistRequest {
            files: map,
            description: s(&body, "description").unwrap_or("").to_string(),
            public,
        }))
        .await?
        .into_inner();
    Ok(Reply::created(gist_json(&cx, &g, true).await))
}

async fn get_gist(cx: &Ctx, id: &str) -> ApiResult<Gist> {
    Ok(cx
        .hub
        .svc
        .get_gist(cx.req(GetGistRequest {
            gist_id: id.to_string(),
            max_inline_bytes: 0,
        }))
        .await?
        .into_inner())
}

/// `GET /gists/{id}`.
pub(crate) async fn gist(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let g = get_gist(&cx, p.get("id")).await?;
    Ok(Reply::json(gist_json(&cx, &g, true).await))
}

/// `PATCH /gists/{id}`.
pub(crate) async fn update_gist(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let body = cx.body_json()?;
    let mut changes = Vec::new();
    if let Some(files) = body.get("files").and_then(Value::as_object) {
        for (name, f) in files {
            if f.is_null() {
                changes.push(GistFileChange {
                    filename: name.clone(),
                    delete: true,
                    ..Default::default()
                });
                continue;
            }
            changes.push(GistFileChange {
                filename: name.clone(),
                content: s(f, "content").map(str::to_string),
                new_filename: s(f, "filename").filter(|n| n != name).map(str::to_string),
                delete: false,
            });
        }
    }
    let g = cx
        .hub
        .svc
        .update_gist(cx.req(UpdateGistRequest {
            gist_id: p.get("id").to_string(),
            expected_head: None,
            description: s(&body, "description").map(str::to_string),
            file_changes: changes,
            comments_locked: None,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(gist_json(&cx, &g, true).await))
}

/// `DELETE /gists/{id}`.
pub(crate) async fn delete_gist(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    cx.hub
        .svc
        .delete_gist(cx.req(DeleteGistRequest {
            gist_id: p.get("id").to_string(),
        }))
        .await?;
    Ok(Reply::no_content())
}

fn hex(id: &Option<ObjectId>) -> String {
    id.as_ref()
        .map(|o| o.raw.iter().map(|b| format!("{b:02x}")).collect())
        .unwrap_or_default()
}

/// `GET /gists/{id}/commits`.
pub(crate) async fn gist_commits(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let id = p.get("id").to_string();
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListGistRevisionsRequest {
            gist_id: id.clone(),
            page: Some(page),
        });
        async move {
            let r = svc.list_gist_revisions(req).await?.into_inner();
            Ok((r.revisions, r.page))
        }
    })
    .await?;
    let mut items = Vec::new();
    for r in &list {
        let v = hex(&r.changeset);
        items.push(json!({
            "url": cx.api(&format!("/gists/{id}/{v}")),
            "version": v,
            "user": shapes::user(&cx, r.author.as_ref().unwrap_or(&Principal::default())).await,
            "change_status": {"total": r.additions + r.deletions, "additions": r.additions, "deletions": r.deletions},
            "committed_at": iso(&r.created_at),
        }));
    }
    Ok(cx.paginate(items))
}

/// `GET /gists/{id}/{sha}`.
pub(crate) async fn gist_revision(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let raw: Vec<u8> = (0..p.get("sha").len() / 2)
        .filter_map(|i| u8::from_str_radix(p.get("sha").get(i * 2..i * 2 + 2)?, 16).ok())
        .collect();
    if raw.is_empty() {
        return Err(ApiError::not_found());
    }
    let g = cx
        .hub
        .svc
        .get_gist_revision(cx.req(GetGistRevisionRequest {
            gist_id: p.get("id").to_string(),
            changeset: Some(ObjectId { raw }),
            max_inline_bytes: 0,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(gist_json(&cx, &g, true).await))
}

/// `GET /gists/{id}/forks`.
pub(crate) async fn gist_forks(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let id = p.get("id").to_string();
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListGistForksRequest {
            gist_id: id.clone(),
            page: Some(page),
        });
        async move {
            let r = svc.list_gist_forks(req).await?.into_inner();
            Ok((r.forks, r.page))
        }
    })
    .await?;
    let mut items = Vec::new();
    for g in &list {
        items.push(gist_json(&cx, g, false).await);
    }
    Ok(cx.paginate(items))
}

/// `POST /gists/{id}/forks`.
pub(crate) async fn fork_gist(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let g = cx
        .hub
        .svc
        .fork_gist(cx.req(ForkGistRequest {
            gist_id: p.get("id").to_string(),
        }))
        .await?
        .into_inner();
    Ok(Reply::created(gist_json(&cx, &g, false).await))
}

/// `GET /gists/{id}/star`: `204` or `404`.
pub(crate) async fn is_gist_starred(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let r = cx
        .hub
        .svc
        .is_gist_starred(cx.req(IsGistStarredRequest {
            gist_id: p.get("id").to_string(),
        }))
        .await?
        .into_inner();
    if r.starred {
        Ok(Reply::no_content())
    } else {
        Err(ApiError::not_found())
    }
}

/// `PUT /gists/{id}/star`.
pub(crate) async fn star_gist(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    cx.hub
        .svc
        .star_gist(cx.req(StarGistRequest {
            gist_id: p.get("id").to_string(),
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `DELETE /gists/{id}/star`.
pub(crate) async fn unstar_gist(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    cx.hub
        .svc
        .unstar_gist(cx.req(UnstarGistRequest {
            gist_id: p.get("id").to_string(),
        }))
        .await?;
    Ok(Reply::no_content())
}

async fn gist_comment_json(cx: &Ctx, gist: &str, c: &bee_proto::v1::Comment) -> Value {
    let id = cx.hub.ids.remember("gist_comment", &c.id, gist);
    json!({
        "id": id,
        "node_id": crate::ids::node("GistComment", &c.id),
        "url": cx.api(&format!("/gists/{gist}/comments/{id}")),
        "body": c.body,
        "user": shapes::user(cx, c.author.as_ref().unwrap_or(&Principal::default())).await,
        "created_at": iso(&c.created_at),
        "updated_at": iso_or(&c.updated_at, &c.created_at),
        "author_association": if c.author_association.is_empty() { "NONE" } else { c.author_association.as_str() },
    })
}

/// `GET /gists/{id}/comments`.
pub(crate) async fn gist_comments(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let id = p.get("id").to_string();
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListGistCommentsRequest {
            gist_id: id.clone(),
            include_html: false,
            page: Some(page),
        });
        async move {
            let r = svc.list_gist_comments(req).await?.into_inner();
            Ok((r.comments, r.page))
        }
    })
    .await?;
    let mut items = Vec::new();
    for c in &list {
        items.push(gist_comment_json(&cx, &id, c).await);
    }
    Ok(cx.paginate(items))
}

/// `POST /gists/{id}/comments`.
pub(crate) async fn create_gist_comment(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let body = cx.body_json()?;
    let id = p.get("id").to_string();
    let c = cx
        .hub
        .svc
        .create_gist_comment(cx.req(CreateGistCommentRequest {
            gist_id: id.clone(),
            body: super::need(&body, "body", "GistComment")?.to_string(),
        }))
        .await?
        .into_inner();
    Ok(Reply::created(gist_comment_json(&cx, &id, &c).await))
}

#[cfg(test)]
mod tests {
    #[test]
    fn camel_cases_event_names() {
        assert_eq!(super::camel("pull_request"), "PullRequest");
        assert_eq!(super::camel("push"), "Push");
    }
}
