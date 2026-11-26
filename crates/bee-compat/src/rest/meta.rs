//! `/`, `/meta`, `/rate_limit`, `/zen`, `/octocat`, `/versions`, `/emojis`.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::error::ApiResult;
use crate::http::{Ctx, Params, Reply, rate_key};
use crate::hub::API_VERSION;

/// `GET /`: the URL templates of the API root.
pub(crate) async fn root(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let a = |p: &str| cx.api(p);
    Ok(Reply::json(json!({
        "current_user_url": a("/user"),
        "current_user_authorizations_html_url": cx.html("/settings/connections/applications{/client_id}"),
        "authorizations_url": a("/authorizations"),
        "code_search_url": a("/search/code?q={query}{&page,per_page,sort,order}"),
        "commit_search_url": a("/search/commits?q={query}{&page,per_page,sort,order}"),
        "emails_url": a("/user/emails"),
        "emojis_url": a("/emojis"),
        "events_url": a("/events"),
        "feeds_url": a("/feeds"),
        "followers_url": a("/user/followers"),
        "following_url": a("/user/following{/target}"),
        "gists_url": a("/gists{/gist_id}"),
        "hub_url": a("/hub"),
        "issue_search_url": a("/search/issues?q={query}{&page,per_page,sort,order}"),
        "issues_url": a("/issues"),
        "keys_url": a("/user/keys"),
        "label_search_url": a("/search/labels?q={query}&repository_id={repository_id}{&page,per_page}"),
        "notifications_url": a("/notifications"),
        "organization_url": a("/orgs/{org}"),
        "organization_repositories_url": a("/orgs/{org}/repos{?type,page,per_page,sort}"),
        "organization_teams_url": a("/orgs/{org}/teams"),
        "public_gists_url": a("/gists/public"),
        "rate_limit_url": a("/rate_limit"),
        "repository_url": a("/repos/{owner}/{repo}"),
        "repository_search_url": a("/search/repositories?q={query}{&page,per_page,sort,order}"),
        "current_user_repositories_url": a("/user/repos{?type,page,per_page,sort}"),
        "starred_url": a("/user/starred{/owner}{/repo}"),
        "starred_gists_url": a("/gists/starred"),
        "topic_search_url": a("/search/topics?q={query}{&page,per_page}"),
        "user_url": a("/users/{user}"),
        "user_organizations_url": a("/user/orgs"),
        "user_repositories_url": a("/users/{user}/repos{?type,page,per_page,sort}"),
        "user_search_url": a("/search/users?q={query}{&page,per_page,sort,order}"),
    })))
}

/// `GET /meta`.
pub(crate) async fn meta(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    Ok(Reply::json(json!({
        "verifiable_password_authentication": false,
        "installed_version": cx.hub.config.installed_version,
        "ssh_key_fingerprints": {},
        "ssh_keys": [],
        "hooks": [],
        "web": [],
        "api": [],
        "git": [],
        "packages": [],
        "pages": [],
        "importer": [],
        "actions": [],
        "dependabot": [],
        "domains": {"website": [cx.web.clone()], "codespaces": [], "copilot": [], "packages": []},
    })))
}

/// `GET /rate_limit` (free, as on GitHub).
pub(crate) async fn rate_limit(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let hub = &cx.hub;
    let now = hub.ports.clock.now_ms() / 1000;
    let key = rate_key(&cx);
    let anon = cx.caller.is_none();
    let res = |name: &'static str, limit: u32| {
        let limit = if anon {
            hub.config.anonymous_limit
        } else {
            limit
        };
        let s = hub.limiter.peek(&key, name, limit, now);
        json!({"limit": s.limit, "used": s.used, "remaining": s.remaining(), "reset": s.reset, "resource": name})
    };
    let core = res("core", hub.config.core_limit);
    Ok(Reply::json(json!({
        "resources": {
            "core": core,
            "search": res("search", hub.config.search_limit),
            "graphql": res("graphql", hub.config.graphql_limit),
            "code_search": res("search", hub.config.search_limit),
            "integration_manifest": res("integration_manifest", 5000),
            "source_import": res("source_import", 100),
            "code_scanning_upload": res("code_scanning_upload", 1000),
            "actions_runner_registration": res("actions_runner_registration", 10000),
            "scim": res("scim", 15000),
            "dependency_snapshots": res("dependency_snapshots", 100),
        },
        "rate": core,
    })))
}

/// `GET /zen`.
pub(crate) async fn zen(_: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    Ok(Reply::raw(
        "Approachable is better than simple.",
        "text/plain;charset=utf-8",
    ))
}

/// `GET /octocat`.
pub(crate) async fn octocat(_: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    Ok(Reply::raw(
        "  bee: many hands, one history\n",
        "application/octocat-stream",
    ))
}

/// `GET /versions`.
pub(crate) async fn versions(_: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    Ok(Reply::json(json!([API_VERSION])))
}

/// `GET /emojis`: the names GitHub reactions use.
pub(crate) async fn emojis(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let mut m = serde_json::Map::new();
    for name in [
        "+1", "-1", "laugh", "hooray", "confused", "heart", "rocket", "eyes",
    ] {
        m.insert(
            name.into(),
            Value::String(cx.html(&format!("/emoji/{name}.png"))),
        );
    }
    Ok(Reply::json(Value::Object(m)))
}
