//! `/user`, `/users/{user}`, followers and following.

use std::sync::Arc;

use bee_proto::v1::org_service_server::OrgService as _;
use bee_proto::v1::user_service_server::UserService as _;
use bee_proto::v1::{
    FollowUserRequest, GetOrgRequest, GetUserRequest, GetViewerRequest, IsFollowingUserRequest,
    ListUserEmailsRequest, ListUserFollowersRequest, ListUserFollowingRequest, Principal,
    UnfollowUserRequest, UpdateUserProfileRequest, UserProfile, UserSummary, get_user_request,
};
use serde_json::{Value, json};

use super::s;
use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Reply, all_pages};
use crate::shapes;
use crate::time::{iso, iso_or};

/// GitHub's full user object from a profile.
pub(crate) async fn full(cx: &Ctx, prof: &UserProfile, private: bool) -> Value {
    let p = prof.principal.clone().unwrap_or_default();
    let mut v = shapes::user(cx, &p).await;
    if !prof.login.is_empty() {
        v["login"] = json!(prof.login);
    }
    let opt = |s: &str| {
        if s.is_empty() {
            Value::Null
        } else {
            Value::String(s.to_string())
        }
    };
    let extra = json!({
        "name": if prof.name.is_empty() { opt(&p.display) } else { opt(&prof.name) },
        "company": opt(&prof.company),
        "blog": prof.website,
        "location": opt(&prof.location),
        "email": opt(&prof.public_email),
        "hireable": if prof.hireable { Value::Bool(true) } else { Value::Null },
        "bio": opt(&prof.bio),
        "twitter_username": Value::Null,
        "public_repos": prof.public_repo_count,
        "public_gists": prof.public_gist_count,
        "followers": prof.follower_count,
        "following": prof.following_count,
        "created_at": iso(&prof.created_at),
        "updated_at": iso_or(&prof.updated_at, &prof.created_at),
    });
    if let (Value::Object(m), Value::Object(e)) = (&mut v, extra) {
        m.extend(e);
    }
    if !prof.avatar_url.is_empty() {
        v["avatar_url"] = json!(prof.avatar_url);
    }
    if prof.is_site_admin {
        v["site_admin"] = json!(true);
    }
    if private {
        v["private_gists"] = json!(0);
        v["total_private_repos"] = json!(0);
        v["owned_private_repos"] = json!(0);
        v["disk_usage"] = json!(0);
        v["collaborators"] = json!(0);
        v["two_factor_authentication"] = json!(false);
        v["plan"] =
            json!({"name": "enterprise", "space": 0, "private_repos": 0, "collaborators": 0});
    }
    v
}

/// A profile for a principal, falling back to a bare one when the users module has none.
async fn profile_of(cx: &Ctx, p: &Principal) -> UserProfile {
    match cx
        .hub
        .svc
        .get_user(cx.req(GetUserRequest {
            user: Some(get_user_request::User::Id(p.id.clone())),
        }))
        .await
    {
        Ok(r) => r.into_inner(),
        Err(_) => UserProfile {
            principal: Some(p.clone()),
            login: cx.login(p).await,
            ..Default::default()
        },
    }
}

/// `GET /user`.
pub(crate) async fn me(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let caller = cx.caller()?.clone();
    let prof = match cx.hub.svc.get_viewer(cx.req(GetViewerRequest {})).await {
        Ok(r) => r.into_inner(),
        Err(_) => profile_of(&cx, &cx.principal(&caller.cx.actor_id).await).await,
    };
    Ok(Reply::json(full(&cx, &prof, true).await))
}

/// `PATCH /user`.
pub(crate) async fn update_me(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    let body = cx.body_json()?;
    let opt = |k: &str| s(&body, k).map(str::to_string);
    let prof = cx
        .hub
        .svc
        .update_user_profile(cx.req(UpdateUserProfileRequest {
            name: opt("name"),
            bio: opt("bio"),
            company: opt("company"),
            location: opt("location"),
            website: opt("blog"),
            public_email: opt("email"),
            hireable: body.get("hireable").and_then(Value::as_bool),
            ..Default::default()
        }))
        .await?
        .into_inner();
    Ok(Reply::json(full(&cx, &prof, true).await))
}

/// `GET /users/{user}`: a user, or an org shown as an account.
pub(crate) async fn get(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    let login = p.get("user");
    if let Ok(o) = cx
        .hub
        .svc
        .get_org(cx.req(GetOrgRequest {
            name_or_id: login.to_string(),
        }))
        .await
    {
        let o = o.into_inner();
        let mut v = shapes::org(&cx, &o);
        v["public_repos"] = json!(0);
        v["followers"] = json!(0);
        v["following"] = json!(0);
        return Ok(Reply::json(v));
    }
    let prof = match cx
        .hub
        .svc
        .get_user(cx.req(GetUserRequest {
            user: Some(get_user_request::User::Login(login.to_string())),
        }))
        .await
    {
        Ok(r) => r.into_inner(),
        Err(e) => {
            let e = ApiError::from(e);
            if e.status != 404 {
                return Err(e);
            }
            let pr = cx.by_login(login).await?;
            profile_of(&cx, &pr).await
        }
    };
    Ok(Reply::json(full(&cx, &prof, false).await))
}

/// `GET /user/emails`.
pub(crate) async fn emails(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    let r = cx
        .hub
        .svc
        .list_user_emails(cx.req(ListUserEmailsRequest {}))
        .await?
        .into_inner();
    let items = r
        .emails
        .iter()
        .map(|e| {
            json!({
                "email": e.email, "primary": e.primary, "verified": e.verified,
                "visibility": if e.public { Value::String("public".into()) } else { Value::String("private".into()) },
            })
        })
        .collect();
    Ok(cx.paginate(items))
}

async fn summaries(cx: &Ctx, users: Vec<UserSummary>) -> Vec<Value> {
    let mut out = Vec::new();
    for u in users {
        let p = u.principal.clone().unwrap_or_default();
        let mut v = shapes::user(cx, &p).await;
        if !u.login.is_empty() {
            v["login"] = json!(u.login);
        }
        out.push(v);
    }
    out
}

async fn followers_of(cx: &Ctx, login: String) -> ApiResult<Reply> {
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListUserFollowersRequest {
            login: login.clone(),
            page: Some(page),
        });
        async move {
            let r = svc.list_user_followers(req).await?.into_inner();
            Ok((r.users, r.page))
        }
    })
    .await?;
    Ok(cx.paginate(summaries(cx, list).await))
}

async fn following_of(cx: &Ctx, login: String) -> ApiResult<Reply> {
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(ListUserFollowingRequest {
            login: login.clone(),
            page: Some(page),
        });
        async move {
            let r = svc.list_user_following(req).await?.into_inner();
            Ok((r.users, r.page))
        }
    })
    .await?;
    Ok(cx.paginate(summaries(cx, list).await))
}

/// `GET /user/followers`.
pub(crate) async fn my_followers(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    followers_of(&cx, String::new()).await
}

/// `GET /user/following`.
pub(crate) async fn my_following(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    cx.caller()?;
    following_of(&cx, String::new()).await
}

/// `GET /users/{user}/followers`.
pub(crate) async fn followers(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    followers_of(&cx, p.get("user").to_string()).await
}

/// `GET /users/{user}/following`.
pub(crate) async fn following(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    following_of(&cx, p.get("user").to_string()).await
}

/// `GET /user/following/{user}`: `204` or `404`.
pub(crate) async fn am_following(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    cx.caller()?;
    let r = cx
        .hub
        .svc
        .is_following_user(cx.req(IsFollowingUserRequest {
            login: String::new(),
            target_login: p.get("user").to_string(),
        }))
        .await?
        .into_inner();
    if r.following {
        Ok(Reply::no_content())
    } else {
        Err(ApiError::not_found())
    }
}

/// `PUT /user/following/{user}`.
pub(crate) async fn follow(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    cx.hub
        .svc
        .follow_user(cx.req(FollowUserRequest {
            login: p.get("user").to_string(),
        }))
        .await?;
    Ok(Reply::no_content())
}

/// `DELETE /user/following/{user}`.
pub(crate) async fn unfollow(cx: Arc<Ctx>, p: Params) -> ApiResult<Reply> {
    cx.hub
        .svc
        .unfollow_user(cx.req(UnfollowUserRequest {
            login: p.get("user").to_string(),
        }))
        .await?;
    Ok(Reply::no_content())
}
