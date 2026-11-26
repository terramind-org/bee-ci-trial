//! Repositories: get, create, edit, delete, list, fork, topics, languages, contributors, tags,
//! transfer.

use std::collections::BTreeMap;
use std::sync::Arc;

use bee_proto::v1::org_service_server::OrgService as _;
use bee_proto::v1::repo_service_server::RepoService as _;
use bee_proto::v1::{
    ArchiveRepoRequest, CreateRepoRequest, DeleteRepoRequest, ForkRepoRequest, GetOrgRequest,
    LandMethod, ListForksRequest, ListOrgsRequest, ListReposRequest, Repo, RepoRole,
    SetRepoTopicsRequest, TransferRepoRequest, UpdateRepoRequest, UpdateRepoSettingsRequest,
    Visibility,
};
use serde_json::{Value, json};

use super::{b, my_role, repo_of, rid, s, settings, strs};
use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Reply, all_pages};
use crate::shapes;

fn visibility_of(body: &Value) -> Option<Visibility> {
    match s(body, "visibility") {
        Some("public") => Some(Visibility::Public),
        Some("internal") => Some(Visibility::Internal),
        Some("private") => Some(Visibility::Private),
        _ => b(body, "private").map(|p| {
            if p {
                Visibility::Private
            } else {
                Visibility::Public
            }
        }),
    }
}

/// The full repo object (settings and the caller's permissions included).
pub(crate) async fn full(cx: &Ctx, r: &Repo) -> Value {
    let st = settings(cx, r).await;
    let role = my_role(cx, r).await;
    let mut v = shapes::repo(cx, r, st.as_ref(), role);
    if !r.fork_parent_id.is_empty()
        && let Ok(parent) = cx
            .hub
            .svc
            .get_repo(cx.req(bee_proto::v1::GetRepoRequest {
                repo: rid(&r.fork_parent_id),
            }))
            .await
    {
        let parent = parent.into_inner();
        let pv = shapes::repo(cx, &parent, None, None);
        v["parent"] = pv.clone();
        v["source"] = pv;
    }
    v
}

/// `GET /repos/{owner}/{repo}`.
pub(crate) async fn get(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let r = repo_of(&cx, &p).await?;
    Ok(Reply::json(full(&cx, &r).await))
}

/// Applies the merge and feature settings GitHub's create/edit bodies carry.
async fn apply_settings(cx: &Ctx, repo: &Repo, body: &Value) -> ApiResult<()> {
    let mut req = UpdateRepoSettingsRequest {
        repo: rid(&repo.id),
        has_issues: b(body, "has_issues"),
        has_projects: b(body, "has_projects"),
        has_wiki: b(body, "has_wiki"),
        has_discussions: b(body, "has_discussions"),
        allow_forking: b(body, "allow_forking"),
        allow_auto_merge: b(body, "allow_auto_merge"),
        delete_workspace_ref_on_land: b(body, "delete_branch_on_merge"),
        suggest_update_base: b(body, "allow_update_branch"),
        web_commit_signoff_required: b(body, "web_commit_signoff_required"),
        ..Default::default()
    };
    let methods = [
        ("allow_merge_commit", LandMethod::MergeCommit),
        ("allow_squash_merge", LandMethod::Squash),
        ("allow_rebase_merge", LandMethod::Rebase),
    ];
    if methods.iter().any(|(k, _)| body.get(*k).is_some()) {
        let cur = settings(cx, repo).await.unwrap_or_default();
        let mut allowed: Vec<i32> = if cur.allowed_land_methods.is_empty() {
            methods.iter().map(|(_, m)| *m as i32).collect()
        } else {
            cur.allowed_land_methods.clone()
        };
        for (k, m) in methods {
            match b(body, k) {
                Some(true) if !allowed.contains(&(m as i32)) => allowed.push(m as i32),
                Some(false) => allowed.retain(|x| *x != m as i32),
                _ => {}
            }
        }
        if allowed.is_empty() {
            return Err(ApiError::validation(
                "Repository",
                "merge_methods",
                "invalid",
                "at least one merge method must be allowed",
            ));
        }
        if !allowed.contains(&cur.default_land_method) {
            req.default_land_method = Some(allowed[0]);
        }
        req.allowed_land_methods = allowed;
        req.set_allowed_land_methods = true;
    }
    let touched = req.has_issues.is_some()
        || req.has_projects.is_some()
        || req.has_wiki.is_some()
        || req.has_discussions.is_some()
        || req.allow_forking.is_some()
        || req.allow_auto_merge.is_some()
        || req.delete_workspace_ref_on_land.is_some()
        || req.suggest_update_base.is_some()
        || req.web_commit_signoff_required.is_some()
        || req.set_allowed_land_methods;
    if touched {
        cx.hub.svc.update_repo_settings(cx.req(req)).await?;
    }
    Ok(())
}

async fn create(cx: &Ctx, org: &str) -> ApiResult<Reply> {
    let body = cx.body_json()?;
    let name = super::need(&body, "name", "Repository")?;
    let repo = cx
        .hub
        .svc
        .create_repo(cx.req(CreateRepoRequest {
            org: org.to_string(),
            name: name.to_string(),
            visibility: visibility_of(&body).unwrap_or(Visibility::Public) as i32,
            description: s(&body, "description").unwrap_or("").to_string(),
            is_template: b(&body, "is_template").unwrap_or(false),
            topics: strs(&body, "topics").unwrap_or_default(),
            homepage: s(&body, "homepage").unwrap_or("").to_string(),
            auto_init: b(&body, "auto_init").unwrap_or(false),
            license_template: s(&body, "license_template").unwrap_or("").to_string(),
        }))
        .await?
        .into_inner();
    apply_settings(cx, &repo, &body).await?;
    Ok(Reply::created(full(cx, &repo).await))
}

/// `POST /orgs/{org}/repos`.
pub(crate) async fn create_in_org(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    create(&cx, p.get("org")).await
}

/// `POST /user/repos`: into the caller's personal org (the org named after the login).
pub(crate) async fn create_mine(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let me = cx.caller()?.cx.actor_id.clone();
    let login = cx.login(&cx.principal(&me).await).await;
    if cx
        .hub
        .svc
        .get_org(cx.req(GetOrgRequest {
            name_or_id: login.clone(),
        }))
        .await
        .is_err()
    {
        return Err(ApiError::unprocessable(format!(
            "no personal namespace: Beehive repos live in orgs; create the org `{login}` or use POST /orgs/{{org}}/repos"
        )));
    }
    create(&cx, &login).await
}

/// `PATCH /repos/{owner}/{repo}`.
pub(crate) async fn update(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let mut repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let rename = s(&body, "name").filter(|n| *n != repo.name);
    let vis = visibility_of(&body);
    let wants_update = rename.is_some()
        || vis.is_some()
        || body.get("description").is_some()
        || body.get("homepage").is_some()
        || body.get("default_branch").is_some()
        || body.get("is_template").is_some();
    if wants_update {
        repo = cx
            .hub
            .svc
            .update_repo(
                cx.req(UpdateRepoRequest {
                    repo: rid(&repo.id),
                    name: rename.unwrap_or("").to_string(),
                    visibility: vis.map(|v| v as i32).unwrap_or(0),
                    description: s(&body, "description")
                        .map(str::to_string)
                        .unwrap_or_else(|| repo.description.clone()),
                    homepage: s(&body, "homepage").map(str::to_string),
                    default_ref: s(&body, "default_branch").map(str::to_string),
                    is_template: b(&body, "is_template"),
                    expected_version: 0,
                }),
            )
            .await?
            .into_inner();
    }
    apply_settings(&cx, &repo, &body).await?;
    match b(&body, "archived") {
        Some(true) if !repo.archived => {
            repo = cx
                .hub
                .svc
                .archive_repo(cx.req(ArchiveRepoRequest {
                    repo: rid(&repo.id),
                }))
                .await?
                .into_inner();
        }
        Some(false) if repo.archived => {
            repo = cx
                .hub
                .svc
                .unarchive_repo(cx.req(bee_proto::v1::UnarchiveRepoRequest {
                    repo: rid(&repo.id),
                }))
                .await?
                .into_inner();
        }
        _ => {}
    }
    Ok(Reply::json(full(&cx, &repo).await))
}

/// `DELETE /repos/{owner}/{repo}` (soft delete with the forge's restore window).
pub(crate) async fn delete(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    cx.hub
        .svc
        .delete_repo(cx.req(DeleteRepoRequest {
            repo: rid(&repo.id),
            confirm_full_name: repo.full_name.clone(),
        }))
        .await?;
    Ok(Reply::no_content())
}

/// Every repo of an org the caller can see.
pub(crate) async fn list_org(cx: &Ctx, org: &str) -> ApiResult<Vec<Repo>> {
    let svc = cx.hub.svc.clone();
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListReposRequest {
            org: org.to_string(),
            page: Some(page),
        });
        async move {
            let r = svc.list_repos(req).await?.into_inner();
            Ok((r.repos, r.page))
        }
    })
    .await
}

/// The orgs of a principal (empty = the caller).
pub(crate) async fn orgs_of(cx: &Ctx, principal: &str) -> ApiResult<Vec<bee_proto::v1::Org>> {
    let svc = cx.hub.svc.clone();
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListOrgsRequest {
            principal_id: principal.to_string(),
            page: Some(page),
        });
        async move {
            let r = svc.list_orgs(req).await?.into_inner();
            Ok((r.orgs, r.page))
        }
    })
    .await
}

/// Every repo the caller can see across its orgs.
pub(crate) async fn visible_repos(cx: &Ctx) -> ApiResult<Vec<Repo>> {
    let mut out = Vec::new();
    for o in orgs_of(cx, "").await? {
        out.extend(list_org(cx, &o.name).await?);
    }
    Ok(out)
}

fn sort_repos(cx: &Ctx, repos: &mut [Repo]) {
    let key = cx.q("sort").unwrap_or("full_name");
    let desc = match cx.q("direction") {
        Some("asc") => false,
        Some("desc") => true,
        _ => key != "full_name",
    };
    repos.sort_by(|a, b| {
        let o = match key {
            "created" => a
                .created_at
                .as_ref()
                .map(|t| t.seconds)
                .cmp(&b.created_at.as_ref().map(|t| t.seconds)),
            "updated" | "pushed" => a
                .pushed_at
                .as_ref()
                .map(|t| t.seconds)
                .cmp(&b.pushed_at.as_ref().map(|t| t.seconds)),
            _ => a.full_name.to_lowercase().cmp(&b.full_name.to_lowercase()),
        };
        if desc { o.reverse() } else { o }
    });
}

fn filter_type(cx: &Ctx, repos: Vec<Repo>) -> Vec<Repo> {
    let t = cx.q("type").or(cx.q("visibility")).unwrap_or("all");
    repos
        .into_iter()
        .filter(|r| match t {
            "public" => shapes::visibility(r) == "public",
            "private" => shapes::visibility(r) == "private",
            "internal" => shapes::visibility(r) == "internal",
            "forks" => !r.fork_parent_id.is_empty(),
            "sources" => r.fork_parent_id.is_empty(),
            _ => true,
        })
        .collect()
}

async fn render(cx: &Ctx, mut repos: Vec<Repo>) -> Reply {
    repos = filter_type(cx, repos);
    sort_repos(cx, &mut repos);
    let items = repos
        .iter()
        .map(|r| shapes::repo(cx, r, None, None))
        .collect();
    cx.paginate(items)
}

/// `GET /orgs/{org}/repos`.
pub(crate) async fn org_repos(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repos = list_org(&cx, p.get("org")).await?;
    Ok(render(&cx, repos).await)
}

/// `GET /user/repos`.
pub(crate) async fn my_repos(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    let repos = visible_repos(&cx).await?;
    Ok(render(&cx, repos).await)
}

/// `GET /users/{user}/repos`: the repos of the org named `user` (personal namespace).
pub(crate) async fn user_repos(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repos = match list_org(&cx, p.get("user")).await {
        Ok(r) => r,
        Err(e) if e.status == 404 => Vec::new(),
        Err(e) => return Err(e),
    };
    Ok(render(&cx, repos).await)
}

/// `GET /repos/{owner}/{repo}/forks`.
pub(crate) async fn forks(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListForksRequest {
            repo: rid(&repo.id),
            page: Some(page),
        });
        async move {
            let r = svc.list_forks(req).await?.into_inner();
            Ok((r.forks, r.page))
        }
    })
    .await?;
    Ok(render(&cx, list).await)
}

/// `POST /repos/{owner}/{repo}/forks`.
pub(crate) async fn fork(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let f = cx
        .hub
        .svc
        .fork_repo(cx.req(ForkRepoRequest {
            repo: rid(&repo.id),
            org: s(&body, "organization").unwrap_or("").to_string(),
            name: s(&body, "name").unwrap_or("").to_string(),
            default_ref_only: b(&body, "default_branch_only").unwrap_or(false),
        }))
        .await?
        .into_inner();
    Ok(Reply::accepted(full(&cx, &f).await))
}

/// `GET /repos/{owner}/{repo}/topics`.
pub(crate) async fn topics(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    Ok(Reply::json(json!({"names": repo.topics})))
}

/// `PUT /repos/{owner}/{repo}/topics`.
pub(crate) async fn set_topics(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let names =
        strs(&body, "names").ok_or_else(|| ApiError::missing_field("Repository", "names"))?;
    let r = cx
        .hub
        .svc
        .set_repo_topics(cx.req(SetRepoTopicsRequest {
            repo: rid(&repo.id),
            topics: names,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(json!({"names": r.topics})))
}

/// `GET /repos/{owner}/{repo}/languages`: the detected primary language and its share.
pub(crate) async fn languages(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let mut m = serde_json::Map::new();
    if !repo.primary_language.is_empty() {
        m.insert(repo.primary_language.clone(), json!(repo.size_bytes.max(1)));
    }
    Ok(Reply::json(Value::Object(m)))
}

/// `GET /repos/{owner}/{repo}/contributors`: commit authors of the default branch.
pub(crate) async fn contributors(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let git = &cx.hub.ports.git;
    let branch = if repo.default_ref.is_empty() {
        "main"
    } else {
        repo.default_ref.as_str()
    };
    let Some(head) = git.resolve(&repo, branch).await? else {
        return Ok(Reply::no_content());
    };
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for c in git.log(&repo, &head, None, 0, 10_000).await? {
        *counts.entry(c.author.name.clone()).or_default() += 1;
    }
    let mut ranked: Vec<(String, u64)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut items = Vec::new();
    for (name, n) in ranked {
        let mut v = match cx.by_login(&name).await {
            Ok(pr) => shapes::user(&cx, &pr).await,
            Err(_) => {
                json!({"login": name, "type": "Anonymous", "name": name, "id": crate::ids::num("anon", &name)})
            }
        };
        v["contributions"] = json!(n);
        items.push(v);
    }
    Ok(cx.paginate(items))
}

/// `GET /repos/{owner}/{repo}/tags`.
pub(crate) async fn tags(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let refs = cx.hub.ports.git.refs(&repo, "refs/tags/").await?;
    let base = cx.api(&format!("/repos/{}", repo.full_name));
    let items = refs
        .iter()
        .rev()
        .map(|r| {
            let name = r.name.trim_start_matches("refs/tags/");
            json!({
                "name": name,
                "commit": {"sha": r.sha, "url": format!("{base}/commits/{}", r.sha)},
                "zipball_url": format!("{base}/zipball/refs/tags/{name}"),
                "tarball_url": format!("{base}/tarball/refs/tags/{name}"),
                "node_id": crate::ids::node("Ref", &format!("{}:{}", repo.id, r.name)),
            })
        })
        .collect();
    Ok(cx.paginate(items))
}

/// `POST /repos/{owner}/{repo}/transfer`.
pub(crate) async fn transfer(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let new_owner = super::need(&body, "new_owner", "Repository")?;
    let r = cx
        .hub
        .svc
        .transfer_repo(cx.req(TransferRepoRequest {
            repo: rid(&repo.id),
            new_org: new_owner.to_string(),
            new_name: s(&body, "new_name").unwrap_or("").to_string(),
            team_ids: Vec::new(),
        }))
        .await?
        .into_inner();
    Ok(Reply::accepted(shapes::repo(
        &cx,
        &r,
        None,
        Some(RepoRole::Admin),
    )))
}
