//! Orgs, members, teams and repo collaborators.

use std::sync::Arc;

use bee_forge::enterprise::service::EnterpriseApi;
use bee_proto::v1::org_service_server::OrgService as _;
use bee_proto::v1::repo_service_server::RepoService as _;
use bee_proto::v1::team_service_server::TeamService as _;
use bee_proto::v1::{
    AddCollaboratorRequest, AddTeamMemberRequest, CreateTeamRequest, DeleteTeamRequest,
    GetCollaboratorPermissionRequest, GetOrgRequest, GetTeamMembershipRequest, GetTeamRequest,
    GrantTeamRepoAccessRequest, ListCollaboratorsRequest, ListMembersRequest,
    ListOutsideCollaboratorsRequest, ListPrincipalTeamsRequest, ListRepoTeamsRequest,
    ListTeamMembersRequest, ListTeamReposRequest, ListTeamsRequest, Member, Org,
    RemoveCollaboratorRequest, RemoveTeamMemberRequest, Repo, RepoRole,
    RevokeTeamRepoAccessRequest, Role, SetMemberRequest, Team, TeamMemberRole, TeamPrivacy,
    UpdateOrgRequest, UpdateTeamRequest, add_team_member_request,
};
use serde_json::{Value, json};

use super::{need, repo_named, repo_of, rid, s};
use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Reply, all_pages};
use crate::shapes;

fn team_api(cx: &Ctx) -> ApiResult<&EnterpriseApi> {
    cx.hub.teams.as_ref().ok_or_else(|| {
        ApiError::new(
            501,
            bee_proto::v1::ErrorReason::Unimplemented,
            "teams are not served by this server",
        )
    })
}

async fn org_named(cx: &Ctx, name: &str) -> ApiResult<Org> {
    Ok(cx
        .hub
        .svc
        .get_org(cx.req(GetOrgRequest {
            name_or_id: name.to_string(),
        }))
        .await?
        .into_inner())
}

/// `GET /orgs/{org}`.
pub(crate) async fn get(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = org_named(&cx, p.get("org")).await?;
    let mut v = shapes::org(&cx, &o);
    let repos = super::repos::list_org(&cx, &o.name)
        .await
        .unwrap_or_default();
    v["public_repos"] = json!(
        repos
            .iter()
            .filter(|r| shapes::visibility(r) == "public")
            .count()
    );
    v["followers"] = json!(0);
    v["following"] = json!(0);
    Ok(Reply::json(v))
}

/// `PATCH /orgs/{org}`.
pub(crate) async fn update(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let body = cx.body_json()?;
    let o = cx
        .hub
        .svc
        .update_org(cx.req(UpdateOrgRequest {
            org: p.get("org").to_string(),
            display: s(&body, "name").map(str::to_string),
            description: s(&body, "description").map(str::to_string),
            website: s(&body, "blog").map(str::to_string),
            email: s(&body, "email").map(str::to_string),
            location: s(&body, "location").map(str::to_string),
            expected_version: 0,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(shapes::org(&cx, &o)))
}

fn org_summary(cx: &Ctx, o: &Org) -> Value {
    let full = shapes::org(cx, o);
    let keep = [
        "login",
        "id",
        "node_id",
        "url",
        "repos_url",
        "events_url",
        "hooks_url",
        "issues_url",
        "members_url",
        "public_members_url",
        "avatar_url",
        "description",
    ];
    let mut m = serde_json::Map::new();
    for k in keep {
        if let Some(v) = full.get(k) {
            m.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(m)
}

/// `GET /user/orgs`.
pub(crate) async fn my_orgs(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    let orgs = super::repos::orgs_of(&cx, "").await?;
    Ok(cx.paginate(orgs.iter().map(|o| org_summary(&cx, o)).collect()))
}

/// `GET /users/{user}/orgs`.
pub(crate) async fn user_orgs(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let who = cx.by_login(p.get("user")).await?;
    let orgs = super::repos::orgs_of(&cx, &who.id).await?;
    Ok(cx.paginate(orgs.iter().map(|o| org_summary(&cx, o)).collect()))
}

/// `GET /organizations`: the orgs the caller belongs to.
pub(crate) async fn list_all(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let orgs = super::repos::orgs_of(&cx, "").await?;
    Ok(cx.paginate(orgs.iter().map(|o| org_summary(&cx, o)).collect()))
}

fn role_word(r: i32) -> &'static str {
    match Role::try_from(r).unwrap_or(Role::Member) {
        Role::Owner | Role::Admin => "admin",
        Role::BillingManager => "billing_manager",
        _ => "member",
    }
}

async fn membership_json(cx: &Ctx, o: &Org, m: &Member) -> Value {
    json!({
        "url": cx.api(&format!("/orgs/{}/memberships/{}", o.name, cx.login(m.principal.as_ref().unwrap_or(&Default::default())).await)),
        "state": "active",
        "role": role_word(m.role),
        "organization_url": cx.api(&format!("/orgs/{}", o.name)),
        "organization": org_summary(cx, o),
        "user": shapes::user(cx, m.principal.as_ref().unwrap_or(&Default::default())).await,
    })
}

async fn members_of(cx: &Ctx, org: &str) -> ApiResult<Vec<Member>> {
    let svc = cx.hub.svc.clone();
    all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListMembersRequest {
            org: org.to_string(),
            page: Some(page),
        });
        async move {
            let r = svc.list_members(req).await?.into_inner();
            Ok((r.members, r.page))
        }
    })
    .await
}

/// `GET /user/memberships/orgs`.
pub(crate) async fn my_memberships(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let me = cx.caller()?.cx.actor_id.clone();
    let mut items = Vec::new();
    for o in super::repos::orgs_of(&cx, "").await? {
        if let Some(m) = members_of(&cx, &o.name)
            .await?
            .into_iter()
            .find(|m| m.principal.as_ref().is_some_and(|p| p.id == me))
        {
            items.push(membership_json(&cx, &o, &m).await);
        }
    }
    Ok(cx.paginate(items))
}

/// `GET /orgs/{org}/members`.
pub(crate) async fn members(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let role = cx.q("role").unwrap_or("all");
    let mut items = Vec::new();
    for m in members_of(&cx, p.get("org")).await? {
        if role == "all" || role_word(m.role) == role {
            items
                .push(shapes::user(&cx, m.principal.as_ref().unwrap_or(&Default::default())).await);
        }
    }
    Ok(cx.paginate(items))
}

async fn find_member(cx: &Ctx, org: &str, login: &str) -> ApiResult<Option<Member>> {
    let who = cx.by_login(login).await?;
    Ok(members_of(cx, org)
        .await?
        .into_iter()
        .find(|m| m.principal.as_ref().is_some_and(|p| p.id == who.id)))
}

/// `GET /orgs/{org}/members/{user}`: `204` or `404`.
pub(crate) async fn is_member(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    match find_member(&cx, p.get("org"), p.get("user")).await? {
        Some(_) => Ok(Reply::no_content()),
        None => Err(ApiError::not_found()),
    }
}

/// `GET /orgs/{org}/memberships/{user}`.
pub(crate) async fn membership(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = org_named(&cx, p.get("org")).await?;
    let m = find_member(&cx, &o.name, p.get("user"))
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Reply::json(membership_json(&cx, &o, &m).await))
}

/// `PUT /orgs/{org}/memberships/{user}`.
pub(crate) async fn set_membership(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let o = org_named(&cx, p.get("org")).await?;
    let body = cx.body_json()?;
    let who = cx.by_login(p.get("user")).await?;
    let role = match s(&body, "role").unwrap_or("member") {
        "admin" => Role::Admin,
        "member" => Role::Member,
        other => return Err(ApiError::validation("Membership", "role", "invalid", other)),
    };
    let m = cx
        .hub
        .svc
        .set_member(cx.req(SetMemberRequest {
            org: o.name.clone(),
            principal_id: who.id.clone(),
            role: role as i32,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(membership_json(&cx, &o, &m).await))
}

/// `DELETE /orgs/{org}/members/{user}` and `/memberships/{user}`.
pub(crate) async fn remove_member(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let who = cx.by_login(p.get("user")).await?;
    cx.hub
        .svc
        .set_member(cx.req(SetMemberRequest {
            org: p.get("org").to_string(),
            principal_id: who.id,
            role: Role::Unspecified as i32,
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `GET /orgs/{org}/outside_collaborators`.
pub(crate) async fn outside_collaborators(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let svc = cx.hub.svc.clone();
    let org = p.get("org").to_string();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListOutsideCollaboratorsRequest {
            org: org.clone(),
            page: Some(page),
        });
        async move {
            let r = svc.list_outside_collaborators(req).await?.into_inner();
            Ok((r.collaborators, r.page))
        }
    })
    .await?;
    let mut items = Vec::new();
    for c in list {
        items.push(shapes::user(&cx, c.principal.as_ref().unwrap_or(&Default::default())).await);
    }
    Ok(cx.paginate(items))
}

/// GitHub's team object.
pub(crate) fn team_json(cx: &Ctx, t: &Team) -> Value {
    let base = cx.api(&format!("/orgs/{}/teams/{}", t.org, t.slug));
    let secret = TeamPrivacy::try_from(t.privacy).unwrap_or_default() == TeamPrivacy::Secret;
    json!({
        "id": cx.hub.ids.remember("team", &t.id, &t.org),
        "node_id": crate::ids::node("Team", &t.id),
        "url": base,
        "html_url": cx.html(&format!("/orgs/{}/teams/{}", t.org, t.slug)),
        "name": t.name,
        "slug": t.slug,
        "description": if t.description.is_empty() { Value::Null } else { Value::String(t.description.clone()) },
        "privacy": if secret { "secret" } else { "closed" },
        "notification_setting": if t.notifications_enabled { "notifications_enabled" } else { "notifications_disabled" },
        "permission": "pull",
        "members_url": format!("{base}/members{{/member}}"),
        "repositories_url": format!("{base}/repos"),
        "parent": Value::Null,
        "members_count": t.member_count,
        "repos_count": t.repo_count,
        "created_at": crate::time::iso(&t.created_at),
        "updated_at": crate::time::iso_or(&t.updated_at, &t.created_at),
    })
}

/// A team for a review request (by id), or a bare reference when teams are not served.
pub(crate) async fn team_ref(cx: &Ctx, org: &str, team_id: &str) -> Value {
    if let Ok(api) = team_api(cx)
        && let Ok(t) = api
            .get_team(cx.req(GetTeamRequest {
                org: org.to_string(),
                team: team_id.to_string(),
            }))
            .await
    {
        return team_json(cx, &t.into_inner());
    }
    json!({"id": crate::ids::num("team", team_id), "slug": team_id, "name": team_id})
}

/// The team id of a slug.
pub(crate) async fn team_id(cx: &Ctx, org: &str, slug: &str) -> ApiResult<String> {
    Ok(team_api(cx)?
        .get_team(cx.req(GetTeamRequest {
            org: org.to_string(),
            team: slug.to_string(),
        }))
        .await
        .map_err(|_| ApiError::validation("Team", "team_reviewers", "invalid", slug))?
        .into_inner()
        .id)
}

/// `GET /orgs/{org}/teams`.
pub(crate) async fn teams_list(cx: &Ctx, org: &str) -> ApiResult<Vec<Team>> {
    let api = team_api(cx)?.clone();
    all_pages(|page| {
        let api = api.clone();
        let req = cx.req(ListTeamsRequest {
            org: org.to_string(),
            query: String::new(),
            top_level_only: false,
            page: Some(page),
        });
        async move {
            let r = api.list_teams(req).await?.into_inner();
            Ok((r.teams, r.page))
        }
    })
    .await
}

/// `GET /orgs/{org}/teams`.
pub(crate) async fn teams(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let list = teams_list(&cx, p.get("org")).await?;
    Ok(cx.paginate(list.iter().map(|t| team_json(&cx, t)).collect()))
}

/// `GET /user/teams`.
pub(crate) async fn my_teams(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    let api = team_api(&cx)?.clone();
    let mut items = Vec::new();
    for o in super::repos::orgs_of(&cx, "").await? {
        let list = all_pages(|page| {
            let api = api.clone();
            let req = cx.req(ListPrincipalTeamsRequest {
                org: o.name.clone(),
                principal_id: String::new(),
                include_inherited: false,
                page: Some(page),
            });
            async move {
                let r = api.list_principal_teams(req).await?.into_inner();
                Ok((r.teams, r.page))
            }
        })
        .await?;
        for t in list {
            let mut v = team_json(&cx, &t);
            v["organization"] = org_summary(&cx, &o);
            items.push(v);
        }
    }
    Ok(cx.paginate(items))
}

fn privacy_of(body: &Value) -> Option<TeamPrivacy> {
    match s(body, "privacy") {
        Some("secret") => Some(TeamPrivacy::Secret),
        Some("closed") => Some(TeamPrivacy::Visible),
        _ => None,
    }
}

/// `POST /orgs/{org}/teams`.
pub(crate) async fn create_team(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let body = cx.body_json()?;
    let mut maintainers = Vec::new();
    for l in super::strs(&body, "maintainers").unwrap_or_default() {
        maintainers.push(cx.id_of_login(&l, "maintainers").await?);
    }
    let t = team_api(&cx)?
        .create_team(
            cx.req(CreateTeamRequest {
                org: p.get("org").to_string(),
                slug: String::new(),
                name: need(&body, "name", "Team")?.to_string(),
                description: s(&body, "description").unwrap_or("").to_string(),
                privacy: privacy_of(&body).unwrap_or(TeamPrivacy::Visible) as i32,
                parent_team: super::u(&body, "parent_team_id")
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                notifications_enabled: s(&body, "notification_setting")
                    != Some("notifications_disabled"),
                maintainer_ids: maintainers,
            }),
        )
        .await?
        .into_inner();
    Ok(Reply::created(team_json(&cx, &t)))
}

/// `GET /orgs/{org}/teams/{slug}`.
pub(crate) async fn team(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let t = team_api(&cx)?
        .get_team(cx.req(GetTeamRequest {
            org: p.get("org").to_string(),
            team: p.get("slug").to_string(),
        }))
        .await?
        .into_inner();
    Ok(Reply::json(team_json(&cx, &t)))
}

/// `PATCH /orgs/{org}/teams/{slug}`.
pub(crate) async fn update_team(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let body = cx.body_json()?;
    let t = team_api(&cx)?
        .update_team(cx.req(UpdateTeamRequest {
            org: p.get("org").to_string(),
            team: p.get("slug").to_string(),
            slug: None,
            name: s(&body, "name").map(str::to_string),
            description: s(&body, "description").map(str::to_string),
            privacy: privacy_of(&body).map(|x| x as i32),
            parent_team: match body.get("parent_team_id") {
                Some(Value::Null) => Some(String::new()),
                Some(v) => v.as_u64().map(|n| n.to_string()),
                None => None,
            },
            notifications_enabled:
                s(&body, "notification_setting").map(|v| v == "notifications_enabled"),
        }))
        .await?
        .into_inner();
    Ok(Reply::json(team_json(&cx, &t)))
}

/// `DELETE /orgs/{org}/teams/{slug}`.
pub(crate) async fn delete_team(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    team_api(&cx)?
        .delete_team(cx.req(DeleteTeamRequest {
            org: p.get("org").to_string(),
            team: p.get("slug").to_string(),
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `GET /orgs/{org}/teams/{slug}/members`.
pub(crate) async fn team_members(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let api = team_api(&cx)?.clone();
    let role = match cx.q("role") {
        Some("maintainer") => TeamMemberRole::Maintainer,
        Some("member") => TeamMemberRole::Member,
        _ => TeamMemberRole::Unspecified,
    };
    let (org, slug) = (p.get("org").to_string(), p.get("slug").to_string());
    let list = all_pages(|page| {
        let api = api.clone();
        let req = cx.req(ListTeamMembersRequest {
            org: org.clone(),
            team: slug.clone(),
            role: role as i32,
            include_child_teams: false,
            page: Some(page),
        });
        async move {
            let r = api.list_team_members(req).await?.into_inner();
            Ok((r.members, r.page))
        }
    })
    .await?;
    let mut items = Vec::new();
    for m in list {
        items.push(shapes::user(&cx, m.principal.as_ref().unwrap_or(&Default::default())).await);
    }
    Ok(cx.paginate(items))
}

fn team_role_word(r: i32) -> &'static str {
    if TeamMemberRole::try_from(r).unwrap_or_default() == TeamMemberRole::Maintainer {
        "maintainer"
    } else {
        "member"
    }
}

/// `GET /orgs/{org}/teams/{slug}/memberships/{user}`.
pub(crate) async fn team_membership(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let who = cx.by_login(p.get("user")).await?;
    let m = team_api(&cx)?
        .get_team_membership(cx.req(GetTeamMembershipRequest {
            org: p.get("org").to_string(),
            team: p.get("slug").to_string(),
            principal_id: who.id,
        }))
        .await?
        .into_inner();
    Ok(Reply::json(json!({
        "url": cx.api(&format!("/orgs/{}/teams/{}/memberships/{}", p.get("org"), p.get("slug"), p.get("user"))),
        "role": team_role_word(m.role),
        "state": "active",
    })))
}

/// `PUT /orgs/{org}/teams/{slug}/memberships/{user}`.
pub(crate) async fn add_team_member(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let who = cx.by_login(p.get("user")).await?;
    let body = cx.body_json()?;
    let role = if s(&body, "role") == Some("maintainer") {
        TeamMemberRole::Maintainer
    } else {
        TeamMemberRole::Member
    };
    team_api(&cx)?
        .add_team_member(cx.req(AddTeamMemberRequest {
            org: p.get("org").to_string(),
            team: p.get("slug").to_string(),
            invitee: Some(add_team_member_request::Invitee::PrincipalId(who.id)),
            role: role as i32,
        }))
        .await?;
    Ok(Reply::json(json!({
        "url": cx.api(&format!("/orgs/{}/teams/{}/memberships/{}", p.get("org"), p.get("slug"), p.get("user"))),
        "role": team_role_word(role as i32),
        "state": "active",
    })))
}

/// `DELETE /orgs/{org}/teams/{slug}/memberships/{user}`.
pub(crate) async fn remove_team_member(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let who = cx.by_login(p.get("user")).await?;
    team_api(&cx)?
        .remove_team_member(cx.req(RemoveTeamMemberRequest {
            org: p.get("org").to_string(),
            team: p.get("slug").to_string(),
            principal_id: who.id,
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `GET /orgs/{org}/teams/{slug}/repos`.
pub(crate) async fn team_repos(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let api = team_api(&cx)?.clone();
    let (org, slug) = (p.get("org").to_string(), p.get("slug").to_string());
    let list = all_pages(|page| {
        let api = api.clone();
        let req = cx.req(ListTeamReposRequest {
            org: org.clone(),
            team: slug.clone(),
            include_inherited: false,
            page: Some(page),
        });
        async move {
            let r = api.list_team_repos(req).await?.into_inner();
            Ok((r.repos, r.page))
        }
    })
    .await?;
    let mut items = Vec::new();
    for g in list {
        let Ok(r) = cx
            .hub
            .svc
            .get_repo(cx.req(bee_proto::v1::GetRepoRequest {
                repo: g.repo.clone(),
            }))
            .await
        else {
            continue;
        };
        let r = r.into_inner();
        let mut v = shapes::repo(&cx, &r, None, Some(role_of_word(&g.role)));
        v["role_name"] = json!(g.role);
        items.push(v);
    }
    Ok(cx.paginate(items))
}

fn role_of_word(w: &str) -> RepoRole {
    match w {
        "admin" => RepoRole::Admin,
        "maintain" => RepoRole::Maintain,
        "write" | "push" => RepoRole::Write,
        "triage" => RepoRole::Triage,
        _ => RepoRole::Read,
    }
}

fn word_of_permission(w: &str) -> &'static str {
    match w {
        "admin" => "admin",
        "maintain" => "maintain",
        "push" | "write" => "write",
        "triage" => "triage",
        _ => "read",
    }
}

/// `PUT /orgs/{org}/teams/{slug}/repos/{owner}/{repo}`.
pub(crate) async fn team_add_repo(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_named(&cx, p.get("owner"), p.get("repo")).await?;
    let body = cx.body_json()?;
    team_api(&cx)?
        .grant_team_repo_access(cx.req(GrantTeamRepoAccessRequest {
            org: p.get("org").to_string(),
            team: p.get("slug").to_string(),
            repo: rid(&repo.id),
            role: word_of_permission(s(&body, "permission").unwrap_or("push")).to_string(),
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `DELETE /orgs/{org}/teams/{slug}/repos/{owner}/{repo}`.
pub(crate) async fn team_remove_repo(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_named(&cx, p.get("owner"), p.get("repo")).await?;
    team_api(&cx)?
        .revoke_team_repo_access(cx.req(RevokeTeamRepoAccessRequest {
            org: p.get("org").to_string(),
            team: p.get("slug").to_string(),
            repo: rid(&repo.id),
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `GET /repos/{owner}/{repo}/teams`.
pub(crate) async fn repo_teams(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let api = team_api(&cx)?.clone();
    let list = all_pages(|page| {
        let api = api.clone();
        let req = cx.req(ListRepoTeamsRequest {
            repo: rid(&repo.id),
            page: Some(page),
        });
        async move {
            let r = api.list_repo_teams(req).await?.into_inner();
            Ok((r.grants, r.page))
        }
    })
    .await?;
    let (owner, _) = shapes::split(&repo);
    let mut items = Vec::new();
    for g in list {
        let mut v = team_ref(&cx, owner, &g.team_id).await;
        v["permission"] = json!(match role_of_word(&g.role) {
            RepoRole::Admin => "admin",
            RepoRole::Maintain => "maintain",
            RepoRole::Write => "push",
            RepoRole::Triage => "triage",
            _ => "pull",
        });
        items.push(v);
    }
    Ok(cx.paginate(items))
}

fn role_name(r: RepoRole) -> &'static str {
    match r {
        RepoRole::Admin => "admin",
        RepoRole::Maintain => "maintain",
        RepoRole::Write => "write",
        RepoRole::Triage => "triage",
        RepoRole::Read => "read",
        _ => "none",
    }
}

fn legacy_permission(r: RepoRole) -> &'static str {
    match r {
        RepoRole::Admin => "admin",
        RepoRole::Maintain | RepoRole::Write => "write",
        RepoRole::Triage | RepoRole::Read => "read",
        _ => "none",
    }
}

/// `GET /repos/{owner}/{repo}/collaborators`.
pub(crate) async fn collaborators(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let svc = cx.hub.svc.clone();
    let direct = cx.q("affiliation") == Some("direct");
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListCollaboratorsRequest {
            repo: rid(&repo.id),
            direct_only: direct,
            page: Some(page),
        });
        async move {
            let r = svc.list_collaborators(req).await?.into_inner();
            Ok((r.collaborators, r.page))
        }
    })
    .await?;
    let mut items = Vec::new();
    for c in list {
        if direct && !c.outside_collaborator && cx.q("affiliation") == Some("outside") {
            continue;
        }
        let role = RepoRole::try_from(c.role).unwrap_or(RepoRole::Read);
        let mut v = shapes::user(&cx, c.principal.as_ref().unwrap_or(&Default::default())).await;
        v["permissions"] = shapes::permissions(role);
        v["role_name"] = json!(role_name(role));
        items.push(v);
    }
    Ok(cx.paginate(items))
}

async fn permission_of(
    cx: &Ctx,
    repo: &Repo,
    login: &str,
) -> ApiResult<(bee_proto::v1::Principal, RepoRole)> {
    let who = cx.by_login(login).await?;
    let perm = cx
        .hub
        .svc
        .get_collaborator_permission(cx.req(GetCollaboratorPermissionRequest {
            repo: rid(&repo.id),
            principal_id: who.id.clone(),
        }))
        .await?
        .into_inner();
    Ok((who, RepoRole::try_from(perm.role).unwrap_or(RepoRole::None)))
}

/// `GET /repos/{owner}/{repo}/collaborators/{user}`: `204` when the user has access.
pub(crate) async fn is_collaborator(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (_, role) = permission_of(&cx, &repo, p.get("user")).await?;
    if (role as i32) >= RepoRole::Read as i32 {
        Ok(Reply::no_content())
    } else {
        Err(ApiError::not_found())
    }
}

/// `GET /repos/{owner}/{repo}/collaborators/{user}/permission`.
pub(crate) async fn collaborator_permission(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let (who, role) = permission_of(&cx, &repo, p.get("user")).await?;
    let mut user = shapes::user(&cx, &who).await;
    user["permissions"] = shapes::permissions(role);
    user["role_name"] = json!(role_name(role));
    Ok(Reply::json(json!({
        "permission": legacy_permission(role),
        "role_name": role_name(role),
        "user": user,
    })))
}

/// `PUT /repos/{owner}/{repo}/collaborators/{user}`.
pub(crate) async fn add_collaborator(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let body = cx.body_json()?;
    let role = role_of_word(s(&body, "permission").unwrap_or("push"));
    let login = p.get("user").to_string();
    let (pid, invitee) = match cx.by_login(&login).await {
        Ok(pr) => (pr.id, String::new()),
        Err(_) => (String::new(), login.clone()),
    };
    let r = cx
        .hub
        .svc
        .add_collaborator(cx.req(AddCollaboratorRequest {
            repo: rid(&repo.id),
            principal_id: pid,
            invitee_login: invitee,
            role: role as i32,
            custom_role_id: String::new(),
        }))
        .await?
        .into_inner();
    match r.invitation {
        Some(inv) => {
            let id = cx.hub.ids.remember("invitation", &inv.id, &repo.id);
            Ok(Reply::created(json!({
                "id": id,
                "node_id": crate::ids::node("RepositoryInvitation", &inv.id),
                "repository": shapes::repo(&cx, &repo, None, None),
                "invitee": shapes::user(&cx, inv.invitee.as_ref().unwrap_or(&Default::default())).await,
                "inviter": shapes::user(&cx, inv.inviter.as_ref().unwrap_or(&Default::default())).await,
                "permissions": legacy_permission(RepoRole::try_from(inv.role).unwrap_or(RepoRole::Read)),
                "created_at": crate::time::iso(&inv.created_at),
                "expired": false,
                "url": cx.api(&format!("/user/repository_invitations/{id}")),
                "html_url": cx.html(&format!("/{}/invitations", repo.full_name)),
            })))
        }
        None => Ok(Reply::no_content()),
    }
}

/// `DELETE /repos/{owner}/{repo}/collaborators/{user}`.
pub(crate) async fn remove_collaborator(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let repo = repo_of(&cx, &p).await?;
    let who = cx.by_login(p.get("user")).await?;
    cx.hub
        .svc
        .remove_collaborator(cx.req(RemoveCollaboratorRequest {
            repo: rid(&repo.id),
            principal_id: who.id,
        }))
        .await?;
    Ok(Reply::no_content())
}
