//! The REST endpoint groups and the route table (`docs/spec/github-compat.md` §Endpoints).

pub(crate) mod actions;
pub(crate) mod activity;
pub(crate) mod checks;
pub(crate) mod git;
pub(crate) mod hooks;
pub(crate) mod issues;
pub(crate) mod meta;
pub(crate) mod orgs;
pub(crate) mod pulls;
pub(crate) mod releases;
pub(crate) mod repos;
pub(crate) mod search;
pub(crate) mod users;

use std::sync::OnceLock;

use bee_forge::changes::service::ChangeServices;
use bee_forge::ci::CiServices;
use bee_proto::v1::repo_service_server::RepoService as _;
use bee_proto::v1::{
    GetCollaboratorPermissionRequest, GetRepoRequest, GetRepoSettingsRequest, Repo, RepoRef,
    RepoRole, RepoSettings, repo_ref,
};
use http::Method;
use serde_json::Value;

use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Route};

/// `RepoRef` by full name.
pub(crate) fn rref(full: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::FullName(full.to_string())),
    })
}

/// `RepoRef` by id.
pub(crate) fn rid(id: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(id.to_string())),
    })
}

/// The repo named by `owner/name` (the forge checks read access).
pub(crate) async fn repo_named(cx: &Ctx, owner: &str, name: &str) -> ApiResult<Repo> {
    let name = name.strip_suffix(".git").unwrap_or(name);
    Ok(cx
        .hub
        .svc
        .get_repo(cx.req(GetRepoRequest {
            repo: rref(&format!("{owner}/{name}")),
        }))
        .await?
        .into_inner())
}

/// The repo of a route with `:owner` and `:repo`.
pub(crate) async fn repo_of(cx: &Ctx, p: &Params) -> ApiResult<Repo> {
    repo_named(cx, p.get("owner"), p.get("repo")).await
}

/// The repo's settings, when the caller may read them.
pub(crate) async fn settings(cx: &Ctx, repo: &Repo) -> Option<RepoSettings> {
    cx.hub
        .svc
        .get_repo_settings(cx.req(GetRepoSettingsRequest {
            repo: rid(&repo.id),
        }))
        .await
        .ok()
        .map(|r| r.into_inner())
}

/// The caller's role on the repo.
pub(crate) async fn my_role(cx: &Ctx, repo: &Repo) -> Option<RepoRole> {
    let me = cx.me();
    if me.is_empty() {
        return Some(RepoRole::Read);
    }
    cx.hub
        .svc
        .get_collaborator_permission(cx.req(GetCollaboratorPermissionRequest {
            repo: rid(&repo.id),
            principal_id: me.to_string(),
        }))
        .await
        .ok()
        .map(|r| RepoRole::try_from(r.into_inner().role).unwrap_or(RepoRole::Read))
}

/// The change-request services (`501` when this server does not serve pull requests).
pub(crate) fn changes(cx: &Ctx) -> ApiResult<&ChangeServices> {
    cx.hub.changes.as_ref().ok_or_else(|| {
        ApiError::new(
            501,
            bee_proto::v1::ErrorReason::Unimplemented,
            "pull requests are not served by this server",
        )
    })
}

/// The CI services (`501` when this server does not run Actions).
pub(crate) fn ci(cx: &Ctx) -> ApiResult<&CiServices> {
    cx.hub.ci.as_ref().ok_or_else(|| {
        ApiError::new(
            501,
            bee_proto::v1::ErrorReason::Unimplemented,
            "Actions are not served by this server",
        )
    })
}

/// The revision expression (SPEC revision syntax: the changeset's text id) the forge's
/// resolver takes for a changeset.
pub(crate) fn revision_text(cs: &bee_proto::v1::ObjectId) -> ApiResult<String> {
    Ok(bee_core::ObjectId::from_bytes(&cs.raw)?.to_text())
}

/// A string field of a JSON body.
pub(crate) fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// A boolean field.
pub(crate) fn b(v: &Value, k: &str) -> Option<bool> {
    v.get(k).and_then(Value::as_bool)
}

/// An integer field (numbers or numeric strings).
pub(crate) fn u(v: &Value, k: &str) -> Option<u64> {
    match v.get(k) {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// A list of strings (also accepts one string, or objects with `name`).
pub(crate) fn strs(v: &Value, k: &str) -> Option<Vec<String>> {
    match v.get(k)? {
        Value::Array(a) => Some(
            a.iter()
                .filter_map(|x| match x {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(_) => x.get("name").and_then(Value::as_str).map(str::to_string),
                    _ => None,
                })
                .collect(),
        ),
        Value::String(s) => Some(vec![s.clone()]),
        _ => None,
    }
}

/// A required string field (`422 missing_field` when absent).
pub(crate) fn need<'a>(v: &'a Value, k: &str, resource: &str) -> ApiResult<&'a str> {
    s(v, k)
        .filter(|x| !x.is_empty())
        .ok_or_else(|| ApiError::missing_field(resource, k))
}

macro_rules! r {
    ($m:ident $p:literal => $h:path) => {
        Route::new(Method::$m, $p, |cx, p| Box::pin($h(cx, p)))
    };
}

/// Every REST route, most specific first where patterns overlap.
pub(crate) fn routes() -> &'static [Route] {
    static ROUTES: OnceLock<Vec<Route>> = OnceLock::new();
    ROUTES.get_or_init(|| {
        vec![
            // meta
            r!(GET "/" => meta::root),
            r!(GET "/meta" => meta::meta),
            r!(GET "/rate_limit" => meta::rate_limit),
            r!(GET "/zen" => meta::zen),
            r!(GET "/octocat" => meta::octocat),
            r!(GET "/versions" => meta::versions),
            r!(GET "/emojis" => meta::emojis),
            // users
            r!(GET "/user" => users::me),
            r!(PATCH "/user" => users::update_me),
            r!(GET "/user/emails" => users::emails),
            r!(GET "/user/orgs" => orgs::my_orgs),
            r!(GET "/user/memberships/orgs" => orgs::my_memberships),
            r!(GET "/user/repos" => repos::my_repos),
            r!(POST "/user/repos" => repos::create_mine),
            r!(GET "/user/starred" => activity::my_starred),
            r!(GET "/user/starred/:owner/:repo" => activity::is_starred),
            r!(PUT "/user/starred/:owner/:repo" => activity::star),
            r!(DELETE "/user/starred/:owner/:repo" => activity::unstar),
            r!(GET "/user/subscriptions" => activity::my_subscriptions),
            r!(GET "/user/followers" => users::my_followers),
            r!(GET "/user/following" => users::my_following),
            r!(GET "/user/following/:user" => users::am_following),
            r!(PUT "/user/following/:user" => users::follow),
            r!(DELETE "/user/following/:user" => users::unfollow),
            r!(GET "/user/issues" => issues::my_issues),
            r!(GET "/user/teams" => orgs::my_teams),
            r!(GET "/issues" => issues::my_issues),
            r!(GET "/users/:user" => users::get),
            r!(GET "/users/:user/orgs" => orgs::user_orgs),
            r!(GET "/users/:user/repos" => repos::user_repos),
            r!(GET "/users/:user/followers" => users::followers),
            r!(GET "/users/:user/following" => users::following),
            r!(GET "/users/:user/starred" => activity::user_starred),
            r!(GET "/users/:user/gists" => activity::user_gists),
            // orgs, members, teams
            r!(GET "/organizations" => orgs::list_all),
            r!(GET "/orgs/:org" => orgs::get),
            r!(PATCH "/orgs/:org" => orgs::update),
            r!(GET "/orgs/:org/repos" => repos::org_repos),
            r!(POST "/orgs/:org/repos" => repos::create_in_org),
            r!(GET "/orgs/:org/members" => orgs::members),
            r!(GET "/orgs/:org/members/:user" => orgs::is_member),
            r!(DELETE "/orgs/:org/members/:user" => orgs::remove_member),
            r!(GET "/orgs/:org/memberships/:user" => orgs::membership),
            r!(PUT "/orgs/:org/memberships/:user" => orgs::set_membership),
            r!(DELETE "/orgs/:org/memberships/:user" => orgs::remove_member),
            r!(GET "/orgs/:org/public_members" => orgs::members),
            r!(GET "/orgs/:org/outside_collaborators" => orgs::outside_collaborators),
            r!(GET "/orgs/:org/teams" => orgs::teams),
            r!(POST "/orgs/:org/teams" => orgs::create_team),
            r!(GET "/orgs/:org/teams/:slug" => orgs::team),
            r!(PATCH "/orgs/:org/teams/:slug" => orgs::update_team),
            r!(DELETE "/orgs/:org/teams/:slug" => orgs::delete_team),
            r!(GET "/orgs/:org/teams/:slug/members" => orgs::team_members),
            r!(GET "/orgs/:org/teams/:slug/memberships/:user" => orgs::team_membership),
            r!(PUT "/orgs/:org/teams/:slug/memberships/:user" => orgs::add_team_member),
            r!(DELETE "/orgs/:org/teams/:slug/memberships/:user" => orgs::remove_team_member),
            r!(GET "/orgs/:org/teams/:slug/repos" => orgs::team_repos),
            r!(PUT "/orgs/:org/teams/:slug/repos/:owner/:repo" => orgs::team_add_repo),
            r!(DELETE "/orgs/:org/teams/:slug/repos/:owner/:repo" => orgs::team_remove_repo),
            r!(GET "/orgs/:org/hooks" => hooks::org_list),
            r!(POST "/orgs/:org/hooks" => hooks::org_create),
            r!(GET "/orgs/:org/hooks/:id" => hooks::org_get),
            r!(PATCH "/orgs/:org/hooks/:id" => hooks::org_update),
            r!(DELETE "/orgs/:org/hooks/:id" => hooks::org_delete),
            r!(POST "/orgs/:org/hooks/:id/pings" => hooks::org_ping),
            r!(GET "/orgs/:org/hooks/:id/deliveries" => hooks::org_deliveries),
            r!(GET "/orgs/:org/actions/secrets/public-key" => actions::org_public_key),
            r!(GET "/orgs/:org/actions/secrets" => actions::org_secrets),
            r!(GET "/orgs/:org/actions/secrets/:name" => actions::org_secret),
            r!(PUT "/orgs/:org/actions/secrets/:name" => actions::org_put_secret),
            r!(DELETE "/orgs/:org/actions/secrets/:name" => actions::org_delete_secret),
            r!(GET "/orgs/:org/actions/variables" => actions::org_variables),
            r!(POST "/orgs/:org/actions/variables" => actions::org_create_variable),
            r!(GET "/orgs/:org/actions/variables/:name" => actions::org_variable),
            r!(PATCH "/orgs/:org/actions/variables/:name" => actions::org_update_variable),
            r!(DELETE "/orgs/:org/actions/variables/:name" => actions::org_delete_variable),
            // search
            r!(GET "/search/issues" => search::issues),
            r!(GET "/search/repositories" => search::repositories),
            r!(GET "/search/code" => search::code),
            r!(GET "/search/users" => search::users),
            r!(GET "/search/commits" => search::commits),
            r!(GET "/search/labels" => search::labels),
            r!(GET "/search/topics" => search::topics),
            // notifications
            r!(GET "/notifications" => activity::notifications),
            r!(PUT "/notifications" => activity::mark_all_read),
            r!(GET "/notifications/threads/:id" => activity::thread),
            r!(PATCH "/notifications/threads/:id" => activity::mark_thread_read),
            r!(DELETE "/notifications/threads/:id" => activity::mark_thread_done),
            r!(GET "/notifications/threads/:id/subscription" => activity::thread_subscription),
            r!(PUT "/notifications/threads/:id/subscription" => activity::set_thread_subscription),
            r!(DELETE "/notifications/threads/:id/subscription" => activity::delete_thread_subscription),
            // gists
            r!(GET "/gists" => activity::my_gists),
            r!(POST "/gists" => activity::create_gist),
            r!(GET "/gists/public" => activity::public_gists),
            r!(GET "/gists/starred" => activity::starred_gists),
            r!(GET "/gists/:id" => activity::gist),
            r!(PATCH "/gists/:id" => activity::update_gist),
            r!(DELETE "/gists/:id" => activity::delete_gist),
            r!(GET "/gists/:id/commits" => activity::gist_commits),
            r!(GET "/gists/:id/forks" => activity::gist_forks),
            r!(POST "/gists/:id/forks" => activity::fork_gist),
            r!(GET "/gists/:id/star" => activity::is_gist_starred),
            r!(PUT "/gists/:id/star" => activity::star_gist),
            r!(DELETE "/gists/:id/star" => activity::unstar_gist),
            r!(GET "/gists/:id/comments" => activity::gist_comments),
            r!(POST "/gists/:id/comments" => activity::create_gist_comment),
            r!(GET "/gists/:id/:sha" => activity::gist_revision),
            // repos
            r!(GET "/repos/:owner/:repo" => repos::get),
            r!(PATCH "/repos/:owner/:repo" => repos::update),
            r!(DELETE "/repos/:owner/:repo" => repos::delete),
            r!(GET "/repos/:owner/:repo/forks" => repos::forks),
            r!(POST "/repos/:owner/:repo/forks" => repos::fork),
            r!(GET "/repos/:owner/:repo/topics" => repos::topics),
            r!(PUT "/repos/:owner/:repo/topics" => repos::set_topics),
            r!(GET "/repos/:owner/:repo/languages" => repos::languages),
            r!(GET "/repos/:owner/:repo/contributors" => repos::contributors),
            r!(GET "/repos/:owner/:repo/tags" => repos::tags),
            r!(POST "/repos/:owner/:repo/dispatches" => actions::repository_dispatch),
            r!(POST "/repos/:owner/:repo/transfer" => repos::transfer),
            r!(GET "/repos/:owner/:repo/collaborators" => orgs::collaborators),
            r!(GET "/repos/:owner/:repo/collaborators/:user" => orgs::is_collaborator),
            r!(PUT "/repos/:owner/:repo/collaborators/:user" => orgs::add_collaborator),
            r!(DELETE "/repos/:owner/:repo/collaborators/:user" => orgs::remove_collaborator),
            r!(GET "/repos/:owner/:repo/collaborators/:user/permission" => orgs::collaborator_permission),
            r!(GET "/repos/:owner/:repo/teams" => orgs::repo_teams),
            r!(GET "/repos/:owner/:repo/stargazers" => activity::stargazers),
            r!(GET "/repos/:owner/:repo/subscribers" => activity::watchers),
            r!(GET "/repos/:owner/:repo/subscription" => activity::subscription),
            r!(PUT "/repos/:owner/:repo/subscription" => activity::set_subscription),
            r!(DELETE "/repos/:owner/:repo/subscription" => activity::delete_subscription),
            r!(GET "/repos/:owner/:repo/notifications" => activity::repo_notifications),
            r!(GET "/repos/:owner/:repo/events" => activity::repo_events),
            // contents and git data
            r!(GET "/repos/:owner/:repo/readme" => git::readme),
            r!(GET "/repos/:owner/:repo/readme/*dir" => git::readme),
            r!(GET "/repos/:owner/:repo/contents" => git::contents),
            r!(GET "/repos/:owner/:repo/contents/*path" => git::contents),
            r!(PUT "/repos/:owner/:repo/contents/*path" => git::put_contents),
            r!(DELETE "/repos/:owner/:repo/contents/*path" => git::delete_contents),
            r!(GET "/repos/:owner/:repo/tarball" => git::tarball),
            r!(GET "/repos/:owner/:repo/tarball/*ref" => git::tarball),
            r!(GET "/repos/:owner/:repo/zipball" => git::zipball),
            r!(GET "/repos/:owner/:repo/zipball/*ref" => git::zipball),
            r!(GET "/repos/:owner/:repo/git/refs" => git::refs),
            r!(GET "/repos/:owner/:repo/git/refs/*ref" => git::refs),
            r!(GET "/repos/:owner/:repo/git/matching-refs/*ref" => git::matching_refs),
            r!(GET "/repos/:owner/:repo/git/ref/*ref" => git::get_ref),
            r!(POST "/repos/:owner/:repo/git/refs" => git::create_ref),
            r!(PATCH "/repos/:owner/:repo/git/refs/*ref" => git::update_ref),
            r!(DELETE "/repos/:owner/:repo/git/refs/*ref" => git::delete_ref),
            r!(GET "/repos/:owner/:repo/git/commits/:sha" => git::git_commit),
            r!(POST "/repos/:owner/:repo/git/commits" => git::create_commit),
            r!(GET "/repos/:owner/:repo/git/trees/*sha" => git::tree),
            r!(POST "/repos/:owner/:repo/git/trees" => git::create_tree),
            r!(GET "/repos/:owner/:repo/git/blobs/:sha" => git::blob),
            r!(POST "/repos/:owner/:repo/git/blobs" => git::create_blob),
            r!(GET "/repos/:owner/:repo/branches" => git::branches),
            r!(GET "/repos/:owner/:repo/branches/*rest" => git::branch_get),
            r!(PUT "/repos/:owner/:repo/branches/*rest" => git::branch_put),
            r!(DELETE "/repos/:owner/:repo/branches/*rest" => git::branch_delete),
            r!(POST "/repos/:owner/:repo/branches/*rest" => git::branch_post),
            r!(GET "/repos/:owner/:repo/commits" => git::commits),
            r!(GET "/repos/:owner/:repo/commits/:ref/status" => checks::combined_status),
            r!(GET "/repos/:owner/:repo/commits/:ref/statuses" => checks::statuses),
            r!(GET "/repos/:owner/:repo/commits/:ref/check-runs" => checks::runs_for_ref),
            r!(GET "/repos/:owner/:repo/commits/:ref/check-suites" => checks::suites_for_ref),
            r!(GET "/repos/:owner/:repo/commits/:ref/pulls" => pulls::for_commit),
            r!(GET "/repos/:owner/:repo/commits/*ref" => git::commit),
            r!(GET "/repos/:owner/:repo/compare/*basehead" => git::compare),
            // statuses and checks
            r!(POST "/repos/:owner/:repo/statuses/:sha" => checks::create_status),
            r!(GET "/repos/:owner/:repo/statuses/:sha" => checks::statuses),
            r!(POST "/repos/:owner/:repo/check-runs" => checks::create_run),
            r!(GET "/repos/:owner/:repo/check-runs/:id" => checks::get_run),
            r!(PATCH "/repos/:owner/:repo/check-runs/:id" => checks::update_run),
            r!(GET "/repos/:owner/:repo/check-runs/:id/annotations" => checks::annotations),
            r!(POST "/repos/:owner/:repo/check-runs/:id/rerequest" => checks::rerequest_run),
            r!(POST "/repos/:owner/:repo/check-suites" => checks::create_suite),
            r!(GET "/repos/:owner/:repo/check-suites/:id" => checks::get_suite),
            r!(GET "/repos/:owner/:repo/check-suites/:id/check-runs" => checks::suite_runs),
            r!(POST "/repos/:owner/:repo/check-suites/:id/rerequest" => checks::rerequest_suite),
            // deployments and environments
            r!(GET "/repos/:owner/:repo/deployments" => actions::deployments),
            r!(POST "/repos/:owner/:repo/deployments" => actions::create_deployment),
            r!(GET "/repos/:owner/:repo/deployments/:id" => actions::deployment),
            r!(DELETE "/repos/:owner/:repo/deployments/:id" => actions::delete_deployment),
            r!(GET "/repos/:owner/:repo/deployments/:id/statuses" => actions::deployment_statuses),
            r!(POST "/repos/:owner/:repo/deployments/:id/statuses" => actions::create_deployment_status),
            r!(GET "/repos/:owner/:repo/deployments/:id/statuses/:status" => actions::deployment_status),
            r!(GET "/repos/:owner/:repo/environments" => actions::environments),
            r!(GET "/repos/:owner/:repo/environments/:name" => actions::environment),
            r!(PUT "/repos/:owner/:repo/environments/:name" => actions::put_environment),
            r!(DELETE "/repos/:owner/:repo/environments/:name" => actions::delete_environment),
            // hooks
            r!(GET "/repos/:owner/:repo/hooks" => hooks::list),
            r!(POST "/repos/:owner/:repo/hooks" => hooks::create),
            r!(GET "/repos/:owner/:repo/hooks/:id" => hooks::get),
            r!(PATCH "/repos/:owner/:repo/hooks/:id" => hooks::update),
            r!(DELETE "/repos/:owner/:repo/hooks/:id" => hooks::delete),
            r!(POST "/repos/:owner/:repo/hooks/:id/pings" => hooks::ping),
            r!(POST "/repos/:owner/:repo/hooks/:id/tests" => hooks::ping),
            r!(GET "/repos/:owner/:repo/hooks/:id/config" => hooks::config),
            r!(PATCH "/repos/:owner/:repo/hooks/:id/config" => hooks::update_config),
            r!(GET "/repos/:owner/:repo/hooks/:id/deliveries" => hooks::deliveries),
            r!(GET "/repos/:owner/:repo/hooks/:id/deliveries/:delivery" => hooks::delivery),
            r!(POST "/repos/:owner/:repo/hooks/:id/deliveries/:delivery/attempts" => hooks::redeliver),
            // pulls
            r!(GET "/repos/:owner/:repo/pulls" => pulls::list),
            r!(POST "/repos/:owner/:repo/pulls" => pulls::create),
            r!(GET "/repos/:owner/:repo/pulls/comments" => pulls::repo_review_comments),
            r!(GET "/repos/:owner/:repo/pulls/comments/:id" => pulls::review_comment),
            r!(GET "/repos/:owner/:repo/pulls/:number" => pulls::get),
            r!(PATCH "/repos/:owner/:repo/pulls/:number" => pulls::update),
            r!(GET "/repos/:owner/:repo/pulls/:number/commits" => pulls::commits),
            r!(GET "/repos/:owner/:repo/pulls/:number/files" => pulls::files),
            r!(GET "/repos/:owner/:repo/pulls/:number/merge" => pulls::is_merged),
            r!(PUT "/repos/:owner/:repo/pulls/:number/merge" => pulls::merge),
            r!(PUT "/repos/:owner/:repo/pulls/:number/update-branch" => pulls::update_branch),
            r!(GET "/repos/:owner/:repo/pulls/:number/reviews" => pulls::reviews),
            r!(POST "/repos/:owner/:repo/pulls/:number/reviews" => pulls::create_review),
            r!(GET "/repos/:owner/:repo/pulls/:number/reviews/:id" => pulls::review),
            r!(PUT "/repos/:owner/:repo/pulls/:number/reviews/:id/dismissals" => pulls::dismiss_review),
            r!(GET "/repos/:owner/:repo/pulls/:number/reviews/:id/comments" => pulls::review_comments_of),
            r!(GET "/repos/:owner/:repo/pulls/:number/comments" => pulls::review_comments),
            r!(POST "/repos/:owner/:repo/pulls/:number/comments" => pulls::create_review_comment),
            r!(POST "/repos/:owner/:repo/pulls/:number/comments/:id/replies" => pulls::reply_review_comment),
            r!(GET "/repos/:owner/:repo/pulls/:number/requested_reviewers" => pulls::requested_reviewers),
            r!(POST "/repos/:owner/:repo/pulls/:number/requested_reviewers" => pulls::request_reviewers),
            r!(DELETE "/repos/:owner/:repo/pulls/:number/requested_reviewers" => pulls::remove_reviewers),
            // issues, labels, milestones
            r!(GET "/repos/:owner/:repo/issues" => issues::list),
            r!(POST "/repos/:owner/:repo/issues" => issues::create),
            r!(GET "/repos/:owner/:repo/issues/comments" => issues::repo_comments),
            r!(GET "/repos/:owner/:repo/issues/comments/:id" => issues::get_comment),
            r!(PATCH "/repos/:owner/:repo/issues/comments/:id" => issues::update_comment),
            r!(DELETE "/repos/:owner/:repo/issues/comments/:id" => issues::delete_comment),
            r!(GET "/repos/:owner/:repo/issues/comments/:id/reactions" => issues::comment_reactions),
            r!(POST "/repos/:owner/:repo/issues/comments/:id/reactions" => issues::react_comment),
            r!(GET "/repos/:owner/:repo/issues/events" => issues::repo_events),
            r!(GET "/repos/:owner/:repo/issues/:number" => issues::get),
            r!(PATCH "/repos/:owner/:repo/issues/:number" => issues::update),
            r!(PUT "/repos/:owner/:repo/issues/:number/lock" => issues::lock),
            r!(DELETE "/repos/:owner/:repo/issues/:number/lock" => issues::unlock),
            r!(GET "/repos/:owner/:repo/issues/:number/comments" => issues::comments),
            r!(POST "/repos/:owner/:repo/issues/:number/comments" => issues::create_comment),
            r!(GET "/repos/:owner/:repo/issues/:number/labels" => issues::issue_labels),
            r!(POST "/repos/:owner/:repo/issues/:number/labels" => issues::add_labels),
            r!(PUT "/repos/:owner/:repo/issues/:number/labels" => issues::set_labels),
            r!(DELETE "/repos/:owner/:repo/issues/:number/labels" => issues::clear_labels),
            r!(DELETE "/repos/:owner/:repo/issues/:number/labels/:name" => issues::remove_label),
            r!(POST "/repos/:owner/:repo/issues/:number/assignees" => issues::add_assignees),
            r!(DELETE "/repos/:owner/:repo/issues/:number/assignees" => issues::remove_assignees),
            r!(GET "/repos/:owner/:repo/issues/:number/events" => issues::events),
            r!(GET "/repos/:owner/:repo/issues/:number/timeline" => issues::timeline),
            r!(GET "/repos/:owner/:repo/issues/:number/reactions" => issues::reactions),
            r!(POST "/repos/:owner/:repo/issues/:number/reactions" => issues::react),
            r!(GET "/repos/:owner/:repo/assignees" => issues::assignees),
            r!(GET "/repos/:owner/:repo/assignees/:user" => issues::is_assignee),
            r!(GET "/repos/:owner/:repo/labels" => issues::labels),
            r!(POST "/repos/:owner/:repo/labels" => issues::create_label),
            r!(GET "/repos/:owner/:repo/labels/:name" => issues::label),
            r!(PATCH "/repos/:owner/:repo/labels/:name" => issues::update_label),
            r!(DELETE "/repos/:owner/:repo/labels/:name" => issues::delete_label),
            r!(GET "/repos/:owner/:repo/milestones" => issues::milestones),
            r!(POST "/repos/:owner/:repo/milestones" => issues::create_milestone),
            r!(GET "/repos/:owner/:repo/milestones/:number" => issues::milestone),
            r!(PATCH "/repos/:owner/:repo/milestones/:number" => issues::update_milestone),
            r!(DELETE "/repos/:owner/:repo/milestones/:number" => issues::delete_milestone),
            // releases
            r!(GET "/repos/:owner/:repo/releases" => releases::list),
            r!(POST "/repos/:owner/:repo/releases" => releases::create),
            r!(GET "/repos/:owner/:repo/releases/latest" => releases::latest),
            r!(GET "/repos/:owner/:repo/releases/tags/:tag" => releases::by_tag),
            r!(POST "/repos/:owner/:repo/releases/generate-notes" => releases::generate_notes),
            r!(GET "/repos/:owner/:repo/releases/assets/:id" => releases::asset),
            r!(PATCH "/repos/:owner/:repo/releases/assets/:id" => releases::update_asset),
            r!(DELETE "/repos/:owner/:repo/releases/assets/:id" => releases::delete_asset),
            r!(GET "/repos/:owner/:repo/releases/:id" => releases::get),
            r!(PATCH "/repos/:owner/:repo/releases/:id" => releases::update),
            r!(DELETE "/repos/:owner/:repo/releases/:id" => releases::delete),
            r!(GET "/repos/:owner/:repo/releases/:id/assets" => releases::assets),
            // actions
            r!(GET "/repos/:owner/:repo/actions/workflows" => actions::workflows),
            r!(GET "/repos/:owner/:repo/actions/workflows/:id" => actions::workflow),
            r!(PUT "/repos/:owner/:repo/actions/workflows/:id/enable" => actions::enable_workflow),
            r!(PUT "/repos/:owner/:repo/actions/workflows/:id/disable" => actions::disable_workflow),
            r!(POST "/repos/:owner/:repo/actions/workflows/:id/dispatches" => actions::dispatch),
            r!(GET "/repos/:owner/:repo/actions/workflows/:id/runs" => actions::workflow_runs),
            r!(GET "/repos/:owner/:repo/actions/runs" => actions::runs),
            r!(GET "/repos/:owner/:repo/actions/runs/:id" => actions::run),
            r!(DELETE "/repos/:owner/:repo/actions/runs/:id" => actions::delete_run),
            r!(GET "/repos/:owner/:repo/actions/runs/:id/attempts/:attempt" => actions::run_attempt),
            r!(GET "/repos/:owner/:repo/actions/runs/:id/attempts/:attempt/jobs" => actions::attempt_jobs),
            r!(GET "/repos/:owner/:repo/actions/runs/:id/jobs" => actions::run_jobs),
            r!(GET "/repos/:owner/:repo/actions/runs/:id/logs" => actions::run_logs),
            r!(DELETE "/repos/:owner/:repo/actions/runs/:id/logs" => actions::delete_run_logs),
            r!(GET "/repos/:owner/:repo/actions/runs/:id/artifacts" => actions::run_artifacts),
            r!(POST "/repos/:owner/:repo/actions/runs/:id/cancel" => actions::cancel),
            r!(POST "/repos/:owner/:repo/actions/runs/:id/force-cancel" => actions::force_cancel),
            r!(POST "/repos/:owner/:repo/actions/runs/:id/rerun" => actions::rerun),
            r!(POST "/repos/:owner/:repo/actions/runs/:id/rerun-failed-jobs" => actions::rerun_failed),
            r!(POST "/repos/:owner/:repo/actions/runs/:id/approve" => actions::approve),
            r!(GET "/repos/:owner/:repo/actions/jobs/:id" => actions::job),
            r!(GET "/repos/:owner/:repo/actions/jobs/:id/logs" => actions::job_logs),
            r!(POST "/repos/:owner/:repo/actions/jobs/:id/rerun" => actions::rerun_job),
            r!(GET "/repos/:owner/:repo/actions/artifacts" => actions::artifacts),
            r!(GET "/repos/:owner/:repo/actions/artifacts/:id" => actions::artifact),
            r!(DELETE "/repos/:owner/:repo/actions/artifacts/:id" => actions::delete_artifact),
            r!(GET "/repos/:owner/:repo/actions/artifacts/:id/:format" => actions::download_artifact),
            r!(GET "/repos/:owner/:repo/actions/secrets/public-key" => actions::public_key),
            r!(GET "/repos/:owner/:repo/actions/secrets" => actions::secrets),
            r!(GET "/repos/:owner/:repo/actions/organization-secrets" => actions::repo_org_secrets),
            r!(GET "/repos/:owner/:repo/actions/secrets/:name" => actions::secret),
            r!(PUT "/repos/:owner/:repo/actions/secrets/:name" => actions::put_secret),
            r!(DELETE "/repos/:owner/:repo/actions/secrets/:name" => actions::delete_secret),
            r!(GET "/repos/:owner/:repo/actions/variables" => actions::variables),
            r!(POST "/repos/:owner/:repo/actions/variables" => actions::create_variable),
            r!(GET "/repos/:owner/:repo/actions/variables/:name" => actions::variable),
            r!(PATCH "/repos/:owner/:repo/actions/variables/:name" => actions::update_variable),
            r!(DELETE "/repos/:owner/:repo/actions/variables/:name" => actions::delete_variable),
            r!(GET "/repos/:owner/:repo/environments/:env/secrets/public-key" => actions::env_public_key),
            r!(GET "/repos/:owner/:repo/environments/:env/secrets" => actions::env_secrets),
            r!(PUT "/repos/:owner/:repo/environments/:env/secrets/:name" => actions::env_put_secret),
            r!(DELETE "/repos/:owner/:repo/environments/:env/secrets/:name" => actions::env_delete_secret),
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Seg;

    #[test]
    fn every_route_is_documented_once() {
        let doc = include_str!("../../../../docs/spec/github-compat.md");
        let mut seen = std::collections::BTreeSet::new();
        for r in routes() {
            let path: String = r
                .segs
                .iter()
                .map(|s| match s {
                    Seg::Lit(l) => format!("/{l}"),
                    Seg::Param(p) | Seg::Rest(p) => format!("/{{{p}}}"),
                })
                .collect();
            let path = if path.is_empty() {
                "/".to_string()
            } else {
                path
            };
            let row = format!("| `{}` | `{path}` |", r.method);
            assert!(doc.contains(&row), "docs/spec/github-compat.md lacks {row}");
            assert!(seen.insert(row.clone()), "route listed twice: {row}");
        }
    }
}
