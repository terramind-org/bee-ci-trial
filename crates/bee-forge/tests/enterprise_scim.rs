//! SCIM 2.0: protocol behaviour (authentication, Users and Groups, filters, PATCH), group to
//! team provisioning, the deprovisioning cascade, and AS-ORG-01 (provision a team through
//! SCIM, grant it a custom role on three repos, verify access).

use crate::common;
use crate::enterprise_support as support;

use bee_forge::enterprise::common::{ResolvedScope, ScopeResolver};
use bee_forge::enterprise::network::AdmissionRequest;
use bee_forge::enterprise::scim::ScimRequest;
use bee_policy::Action;
use bee_proto::v1::{ErrorReason, PolicyRoleKind, RepoRole, Role, TeamMemberRole};
use serde_json::{Value, json};
use support::{World, add_member, cx, make_org, make_repo};

const BASE: &str = "/scim/v2/organizations/acme";

struct Setup {
    org: bee_proto::v1::Org,
    scope: ResolvedScope,
    token: String,
}

async fn setup(w: &World) -> Setup {
    let org = make_org(&w.env, "acme").await;
    for (id, role) in [
        ("admin", Role::Admin),
        ("writer", Role::Member),
        ("reader", Role::Member),
    ] {
        add_member(&w.env, &org, id, role).await;
    }
    w.env
        .forge
        .orgs
        .update_settings(&cx("owner"), &org.id, 0, |s| {
            s.base_repo_role = RepoRole::None as i32
        })
        .await
        .unwrap();
    let scope = w
        .ent
        .accounts
        .resolve_org(&cx("owner"), "acme")
        .await
        .unwrap();
    w.ent
        .scim
        .update_config(&cx("owner"), &scope, Some(true), Some(true))
        .await
        .unwrap();
    let (token, _, _) = w
        .ent
        .scim
        .create_token(&cx("owner"), &scope, "okta", 0)
        .await
        .unwrap();
    Setup { org, scope, token }
}

async fn call(
    w: &World,
    token: &str,
    method: &str,
    path: &str,
    query: &[(&str, &str)],
    body: Value,
) -> (u16, Value) {
    let r = w
        .ent
        .scim
        .handle(ScimRequest {
            method: method.into(),
            path: format!("{BASE}{path}"),
            query: query
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: if body.is_null() {
                Vec::new()
            } else {
                serde_json::to_vec(&body).unwrap()
            },
            authorization: if token.is_empty() {
                String::new()
            } else {
                format!("Bearer {token}")
            },
            ip: "203.0.113.9".into(),
            request_id: "scim-test".into(),
        })
        .await;
    (r.status, r.body)
}

fn user(name: &str) -> Value {
    json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
        "userName": name,
        "externalId": format!("ext-{name}"),
        "name": {"givenName": name, "familyName": "Tester"},
        "emails": [{"value": name, "type": "work", "primary": true}],
        "active": true
    })
}

fn patch(ops: Value) -> Value {
    json!({"schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": ops})
}

async fn principal_of_user(w: &World, s: &Setup, user_name: &str) -> String {
    let (ids, _) = w
        .ent
        .scim
        .list_identities(&cx("owner"), &s.scope, "", true, "", 100)
        .await
        .unwrap();
    ids.into_iter()
        .find(|u| u.user_name == user_name)
        .and_then(|u| u.principal)
        .map(|p| p.id)
        .unwrap_or_else(|| panic!("no SCIM identity {user_name}"))
}

async fn admit(w: &World, chain: &[&str]) -> Result<(), bee_forge::ForgeError> {
    let org = w
        .env
        .forge
        .orgs
        .get_org(&cx("owner"), "acme")
        .await
        .unwrap();
    w.ent
        .admit(
            &cx(chain[0]),
            &AdmissionRequest {
                principals: chain.iter().map(|s| s.to_string()).collect(),
                org_ids: vec![org.id],
                ip: "203.0.113.9".into(),
                app_token: false,
            },
        )
        .await
}

async fn scim_authentication_and_discovery(w: World) {
    let s = setup(&w).await;
    // Discovery.
    let (st, spc) = call(
        &w,
        &s.token,
        "GET",
        "/ServiceProviderConfig",
        &[],
        Value::Null,
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(spc["patch"]["supported"], json!(true));
    let (st, rt) = call(&w, &s.token, "GET", "/ResourceTypes", &[], Value::Null).await;
    assert_eq!((st, rt["totalResults"].clone()), (200, json!(2)));
    // Authentication: missing, unknown, other scope, revoked, expired, disabled.
    let (st, body) = call(&w, "", "GET", "/Users", &[], Value::Null).await;
    assert_eq!(st, 401);
    assert_eq!(
        body["schemas"][0],
        json!("urn:ietf:params:scim:api:messages:2.0:Error")
    );
    assert_eq!(
        call(&w, "scim_nope", "GET", "/Users", &[], Value::Null)
            .await
            .0,
        401
    );
    let other_org = make_org(&w.env, "other").await;
    let other = w
        .ent
        .accounts
        .resolve_org(&cx("owner"), "other")
        .await
        .unwrap();
    w.ent
        .scim
        .update_config(&cx("owner"), &other, Some(true), None)
        .await
        .unwrap();
    let (other_token, other_id, _) = w
        .ent
        .scim
        .create_token(&cx("owner"), &other, "x", 0)
        .await
        .unwrap();
    assert_eq!(
        call(&w, &other_token, "GET", "/Users", &[], Value::Null)
            .await
            .0,
        401,
        "a token is for one scope"
    );
    w.ent
        .scim
        .revoke_token(&cx("owner"), &other, &other_id)
        .await
        .unwrap();
    let path = "/scim/v2/organizations/other/Users";
    let mut r = ScimRequest {
        method: "GET".into(),
        path: path.into(),
        authorization: format!("Bearer {other_token}"),
        ..Default::default()
    };
    assert_eq!(w.ent.scim.handle(r.clone()).await.status, 401, "revoked");
    let (short, _, _) = w
        .ent
        .scim
        .create_token(&cx("owner"), &other, "short", 60)
        .await
        .unwrap();
    r.authorization = format!("Bearer {short}");
    assert_eq!(w.ent.scim.handle(r.clone()).await.status, 200);
    w.clock.advance(61_000);
    assert_eq!(w.ent.scim.handle(r.clone()).await.status, 401, "expired");
    w.ent
        .scim
        .update_config(&cx("owner"), &s.scope, Some(false), None)
        .await
        .unwrap();
    assert_eq!(
        call(&w, &s.token, "GET", "/Users", &[], Value::Null)
            .await
            .0,
        403,
        "disabled"
    );
    w.ent
        .scim
        .update_config(&cx("owner"), &s.scope, Some(true), None)
        .await
        .unwrap();
    assert_eq!(
        call(&w, &s.token, "GET", "/Users", &[], Value::Null)
            .await
            .0,
        200
    );
    assert_eq!(
        call(&w, &s.token, "GET", "/Nope", &[], Value::Null).await.0,
        404
    );
    assert_eq!(
        call(&w, &s.token, "DELETE", "/Schemas", &[], Value::Null)
            .await
            .0,
        405
    );
    let cfg = w.ent.scim.config(&cx("owner"), &s.scope).await.unwrap();
    assert!(cfg.enabled && cfg.active_token_count == 1 && cfg.last_request_at.is_some());
    assert_eq!(
        cfg.endpoint_url,
        "https://hub.example.com/scim/v2/organizations/acme"
    );
    drop(other_org);
}
on_both_backends!(scim_authentication_and_discovery);

async fn scim_users_filters_patch_and_paging(w: World) {
    let s = setup(&w).await;
    let t = &s.token;
    let (st, created) = call(&w, t, "POST", "/Users", &[], user("alice@example.com")).await;
    assert_eq!(st, 201, "{created}");
    let alice = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["userName"], json!("alice@example.com"));
    assert_eq!(created["meta"]["resourceType"], json!("User"));
    assert_eq!(created["groups"], json!([]));
    // Provisioning created a principal and an org membership.
    let pid = principal_of_user(&w, &s, "alice@example.com").await;
    assert!(
        w.env
            .forge
            .orgs
            .member(&cx("owner"), &s.org.id, &pid)
            .await
            .unwrap()
            .is_some()
    );
    // Uniqueness (case-insensitive).
    let (st, e) = call(&w, t, "POST", "/Users", &[], user("ALICE@example.com")).await;
    assert_eq!((st, e["scimType"].clone()), (409, json!("uniqueness")));
    let (st, e) = call(&w, t, "POST", "/Users", &[], json!({"displayName": "x"})).await;
    assert_eq!((st, e["scimType"].clone()), (400, json!("invalidValue")));
    for n in ["bob@example.com", "carol@corp.example", "dave@example.com"] {
        assert_eq!(call(&w, t, "POST", "/Users", &[], user(n)).await.0, 201);
    }
    // Lists, filters and paging.
    let (_, all) = call(&w, t, "GET", "/Users", &[], Value::Null).await;
    assert_eq!(all["totalResults"], json!(4));
    let list = |q: &'static str| {
        let w = &w;
        async move { call(w, t, "GET", "/Users", &[("filter", q)], Value::Null).await }
    };
    let (_, r) = list(r#"userName eq "BOB@example.com""#).await;
    assert_eq!(r["totalResults"], json!(1));
    let (_, r) = list(r#"emails.value co "corp""#).await;
    assert_eq!(r["Resources"][0]["userName"], json!("carol@corp.example"));
    let (_, r) = list(r#"userName ew "@example.com" and not (userName sw "alice")"#).await;
    assert_eq!(r["totalResults"], json!(2));
    let (_, r) =
        list(r#"externalId eq "ext-dave@example.com" or name.givenName eq "carol@corp.example""#)
            .await;
    assert_eq!(r["totalResults"], json!(2));
    let (_, r) = list(r#"emails[type eq "work" and primary eq true] and active eq true"#).await;
    assert_eq!(r["totalResults"], json!(4));
    let (st, e) = list(r#"userName bogus "x""#).await;
    assert_eq!((st, e["scimType"].clone()), (400, json!("invalidFilter")));
    let (_, page) = call(
        &w,
        t,
        "GET",
        "/Users",
        &[("startIndex", "2"), ("count", "2")],
        Value::Null,
    )
    .await;
    assert_eq!(page["totalResults"], json!(4));
    assert_eq!(page["startIndex"], json!(2));
    assert_eq!(page["itemsPerPage"], json!(2));
    assert_eq!(page["Resources"][0]["userName"], json!("bob@example.com"));
    let (_, zero) = call(&w, t, "GET", "/Users", &[("count", "0")], Value::Null).await;
    assert_eq!(zero["Resources"], json!([]));

    // GET, PUT and PATCH.
    let (st, got) = call(&w, t, "GET", &format!("/Users/{alice}"), &[], Value::Null).await;
    assert_eq!(
        (st, got["externalId"].clone()),
        (200, json!("ext-alice@example.com"))
    );
    assert_eq!(
        call(&w, t, "GET", "/Users/nope", &[], Value::Null).await.0,
        404
    );
    let mut put = user("alice@example.com");
    put["displayName"] = json!("Alice L");
    put["title"] = json!("Engineer");
    let (st, r) = call(&w, t, "PUT", &format!("/Users/{alice}"), &[], put).await;
    assert_eq!(
        (st, r["displayName"].clone(), r["title"].clone()),
        (200, json!("Alice L"), json!("Engineer"))
    );
    let ops = patch(json!([
        {"op": "Replace", "path": "name.givenName", "value": "Alicia"},
        {"op": "add", "path": "emails", "value": [{"value": "a@home.example", "type": "home"}]},
        {"op": "remove", "path": "title"},
        {"op": "replace", "path": "userName", "value": "alice.l@example.com"}
    ]));
    let (st, r) = call(&w, t, "PATCH", &format!("/Users/{alice}"), &[], ops).await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["name"]["givenName"], json!("Alicia"));
    assert_eq!(r["userName"], json!("alice.l@example.com"));
    assert_eq!(r["emails"].as_array().unwrap().len(), 2);
    assert!(r.get("title").is_none());
    // Renaming onto an existing userName is a uniqueness error; bad PATCH bodies are 400.
    let clash = patch(json!([{"op": "replace", "path": "userName", "value": "bob@example.com"}]));
    assert_eq!(
        call(&w, t, "PATCH", &format!("/Users/{alice}"), &[], clash)
            .await
            .0,
        409
    );
    assert_eq!(
        call(
            &w,
            t,
            "PATCH",
            &format!("/Users/{alice}"),
            &[],
            json!({"Operations": []})
        )
        .await
        .0,
        400
    );
    let ro = patch(json!([{"op": "replace", "path": "id", "value": "x"}]));
    let (st, e) = call(&w, t, "PATCH", &format!("/Users/{alice}"), &[], ro).await;
    assert_eq!((st, e["scimType"].clone()), (400, json!("mutability")));
    let actions = support::audit_actions(&w, &format!("org:{}", s.org.id)).await;
    for a in ["enterprise.scim_user_create", "enterprise.scim_user_update"] {
        assert!(actions.iter().any(|x| x == a), "{a} missing in {actions:?}");
    }
}
on_both_backends!(scim_users_filters_patch_and_paging);

async fn scim_groups_provision_and_sync_teams(w: World) {
    let s = setup(&w).await;
    let t = &s.token;
    let mut ids = Vec::new();
    for n in ["a@example.com", "b@example.com", "c@example.com"] {
        let (_, u) = call(&w, t, "POST", "/Users", &[], user(n)).await;
        ids.push(u["id"].as_str().unwrap().to_string());
    }
    let group = json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
        "displayName": "Release Engineers",
        "externalId": "okta-g1",
        "members": [{"value": ids[0]}, {"value": ids[1]}]
    });
    let (st, g) = call(&w, t, "POST", "/Groups", &[], group.clone()).await;
    assert_eq!(st, 201, "{g}");
    let gid = g["id"].as_str().unwrap().to_string();
    assert_eq!(g["members"].as_array().unwrap().len(), 2);
    assert_eq!(
        call(&w, t, "POST", "/Groups", &[], group.clone()).await.0,
        409
    );
    let bad = json!({"displayName": "Ghosts", "members": [{"value": "no-such-user"}]});
    assert_eq!(call(&w, t, "POST", "/Groups", &[], bad).await.0, 400);

    // The group became a team, linked to it, with the group's members.
    let owner = cx("owner");
    let viewer = bee_forge::enterprise::teams::Viewer {
        ids: vec!["owner".into()],
        org_owner: true,
    };
    let team = w
        .ent
        .teams
        .get(&owner, &s.org, "release-engineers", &viewer)
        .await
        .unwrap();
    assert!(team.idp_synced);
    assert_eq!(team.member_count, 2);
    let (groups, _) = w
        .ent
        .teams
        .linked_groups(&owner, &s.org, "release-engineers", "", 10)
        .await
        .unwrap();
    assert_eq!(
        (groups[0].provider.as_str(), groups[0].group_id.as_str()),
        ("scim", gid.as_str())
    );
    assert_eq!(groups[0].member_count, 2);
    let p0 = principal_of_user(&w, &s, "a@example.com").await;
    let p2 = principal_of_user(&w, &s, "c@example.com").await;
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "release-engineers", &p0)
            .await
            .is_ok()
    );
    // Users report their groups.
    let (_, u0) = call(
        &w,
        t,
        "GET",
        &format!("/Users/{}", ids[0]),
        &[],
        Value::Null,
    )
    .await;
    assert_eq!(u0["groups"][0]["display"], json!("Release Engineers"));
    let (_, by_group) = call(
        &w,
        t,
        "GET",
        "/Groups",
        &[("filter", r#"members.value eq "x""#)],
        Value::Null,
    )
    .await;
    assert_eq!(by_group["totalResults"], json!(0));
    let (_, by_name) = call(
        &w,
        t,
        "GET",
        "/Groups",
        &[("filter", r#"displayName sw "release""#)],
        Value::Null,
    )
    .await;
    assert_eq!(by_name["totalResults"], json!(1));

    // PATCH membership flows to the team.
    let add = patch(json!([{"op": "add", "path": "members", "value": [{"value": ids[2]}]}]));
    assert_eq!(
        call(&w, t, "PATCH", &format!("/Groups/{gid}"), &[], add)
            .await
            .0,
        200
    );
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "release-engineers", &p2)
            .await
            .is_ok()
    );
    let rm = patch(json!([{"op": "remove", "path": format!("members[value eq \"{}\"]", ids[0])}]));
    assert_eq!(
        call(&w, t, "PATCH", &format!("/Groups/{gid}"), &[], rm)
            .await
            .0,
        200
    );
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "release-engineers", &p0)
            .await
            .is_err()
    );
    // PUT replaces the whole membership.
    let put = json!({"displayName": "Release Engineers", "members": [{"value": ids[0]}]});
    assert_eq!(
        call(&w, t, "PUT", &format!("/Groups/{gid}"), &[], put)
            .await
            .0,
        200
    );
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "release-engineers", &p0)
            .await
            .is_ok()
    );
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "release-engineers", &p2)
            .await
            .is_err()
    );
    // Manual changes stay refused while the group drives the team.
    let manual = w
        .ent
        .teams
        .add_member(
            &owner,
            &s.org,
            "release-engineers",
            bee_forge::enterprise::teams::Invitee::Principal("writer".into()),
            TeamMemberRole::Member,
            None,
        )
        .await;
    assert_eq!(manual.unwrap_err().reason, ErrorReason::InvalidState);

    // Deleting a user drops it from groups and teams; deleting the group empties and unlinks.
    assert_eq!(
        call(
            &w,
            t,
            "DELETE",
            &format!("/Users/{}", ids[0]),
            &[],
            Value::Null
        )
        .await
        .0,
        204
    );
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "release-engineers", &p0)
            .await
            .is_err()
    );
    let (_, g_now) = call(&w, t, "GET", &format!("/Groups/{gid}"), &[], Value::Null).await;
    assert_eq!(g_now["members"], json!([]));
    assert_eq!(
        call(&w, t, "DELETE", &format!("/Groups/{gid}"), &[], Value::Null)
            .await
            .0,
        204
    );
    assert_eq!(
        call(&w, t, "GET", &format!("/Groups/{gid}"), &[], Value::Null)
            .await
            .0,
        404
    );
    assert!(
        !w.ent
            .teams
            .get(&owner, &s.org, "release-engineers", &viewer)
            .await
            .unwrap()
            .idp_synced
    );
}
on_both_backends!(scim_groups_provision_and_sync_teams);

async fn scim_deprovisioning_removes_access_everywhere(w: World) {
    let s = setup(&w).await;
    let t = &s.token;
    let owner = cx("owner");
    let r1 = make_repo(&w.env, &s.org, "one").await;
    let r2 = make_repo(&w.env, &s.org, "two").await;
    let (_, u) = call(&w, t, "POST", "/Users", &[], user("leaver@example.com")).await;
    let uid = u["id"].as_str().unwrap().to_string();
    let (_, keep) = call(&w, t, "POST", "/Users", &[], user("stayer@example.com")).await;
    let keep_id = keep["id"].as_str().unwrap().to_string();
    let group = json!({"displayName": "devs", "members": [{"value": uid}, {"value": keep_id}]});
    let (_, g) = call(&w, t, "POST", "/Groups", &[], group).await;
    let gid = g["id"].as_str().unwrap().to_string();
    let pid = principal_of_user(&w, &s, "leaver@example.com").await;
    let keeper = principal_of_user(&w, &s, "stayer@example.com").await;
    w.ent
        .teams
        .grant_repo(&owner, &s.org, "devs", &r1, "write")
        .await
        .unwrap();
    let who = w.env.forge.hooks.directory.get(&pid).await.unwrap();
    w.env
        .forge
        .orgs
        .add_collaborator(&owner, &r2.id, who, RepoRole::Read, "")
        .await
        .unwrap();
    // Access before.
    let role = |repo: &bee_proto::v1::Repo, p: &str| {
        let (f, o) = (w.env.forge.clone(), owner.clone());
        let (repo, p) = (repo.clone(), p.to_string());
        async move { f.orgs.effective_role(&o, &repo, &p).await.unwrap().role }
    };
    assert_eq!(role(&r1, &pid).await, RepoRole::Write as i32);
    assert_eq!(role(&r2, &pid).await, RepoRole::Read as i32);
    assert!(admit(&w, &[&pid]).await.is_ok());

    // Deprovision through PATCH active=false.
    let off = patch(json!([{"op": "Replace", "path": "active", "value": "False"}]));
    let (st, r) = call(&w, t, "PATCH", &format!("/Users/{uid}"), &[], off).await;
    assert_eq!((st, r["active"].clone()), (200, json!(false)), "{r}");
    // Everything is gone at once: membership, team, grants, and admission for the principal
    // and for anything acting on its behalf.
    assert!(
        w.env
            .forge
            .orgs
            .member(&owner, &s.org.id, &pid)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "devs", &pid)
            .await
            .is_err()
    );
    assert_eq!(role(&r1, &pid).await, RepoRole::None as i32);
    assert_eq!(role(&r2, &pid).await, RepoRole::None as i32);
    let denied = admit(&w, &[&pid]).await.unwrap_err();
    assert_eq!(
        (denied.reason, denied.subcode.as_str()),
        (ErrorReason::PermissionDenied, "principal_blocked")
    );
    assert_eq!(
        admit(&w, &["agent", &pid]).await.unwrap_err().subcode,
        "principal_blocked",
        "agents acting for it too"
    );
    // The other user is untouched.
    assert_eq!(role(&r1, &keeper).await, RepoRole::Write as i32);
    assert!(admit(&w, &[&keeper]).await.is_ok());
    // It is listed as inactive and can be found by filter.
    let (_, inactive) = call(
        &w,
        t,
        "GET",
        "/Users",
        &[("filter", "active eq false")],
        Value::Null,
    )
    .await;
    assert_eq!(inactive["totalResults"], json!(1));
    let (listed, _) = w
        .ent
        .scim
        .list_identities(&owner, &s.scope, "", false, "", 50)
        .await
        .unwrap();
    assert_eq!(
        listed.len(),
        1,
        "inactive identities are hidden unless asked for"
    );
    let actions = support::audit_actions(&w, &format!("org:{}", s.org.id)).await;
    assert!(actions.iter().any(|a| a == "enterprise.scim_deprovision"));
    assert!(actions.iter().any(|a| a == "team.member_remove"));

    // Reactivation lifts the block, restores membership and the group's team seats.
    let on = patch(json!([{"op": "replace", "path": "active", "value": true}]));
    assert_eq!(
        call(&w, t, "PATCH", &format!("/Users/{uid}"), &[], on)
            .await
            .0,
        200
    );
    assert!(admit(&w, &[&pid]).await.is_ok());
    assert!(
        w.env
            .forge
            .orgs
            .member(&owner, &s.org.id, &pid)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "devs", &pid)
            .await
            .is_ok()
    );
    assert_eq!(role(&r1, &pid).await, RepoRole::Write as i32);
    assert_eq!(
        role(&r2, &pid).await,
        RepoRole::None as i32,
        "the direct grant is gone for good"
    );

    // DELETE deprovisions too, and the block stays.
    assert_eq!(
        call(&w, t, "DELETE", &format!("/Users/{uid}"), &[], Value::Null)
            .await
            .0,
        204
    );
    assert_eq!(
        call(&w, t, "GET", &format!("/Users/{uid}"), &[], Value::Null)
            .await
            .0,
        404
    );
    assert_eq!(
        admit(&w, &[&pid]).await.unwrap_err().subcode,
        "principal_blocked"
    );
    assert_eq!(role(&r1, &pid).await, RepoRole::None as i32);
    let (_, g) = call(&w, t, "GET", &format!("/Groups/{gid}"), &[], Value::Null).await;
    assert_eq!(g["members"].as_array().unwrap().len(), 1);

    // The last owner of an org keeps the membership (an org needs an owner) but is blocked.
    let (_, lo) = call(&w, t, "POST", "/Users", &[], user("lastowner@example.com")).await;
    let lo_id = lo["id"].as_str().unwrap().to_string();
    let lo_pid = principal_of_user(&w, &s, "lastowner@example.com").await;
    let p = w.env.forge.hooks.directory.get(&lo_pid).await.unwrap();
    w.env
        .forge
        .orgs
        .set_member(&owner, &s.org.id, p, Role::Owner)
        .await
        .unwrap();
    let owner_p = w.env.forge.hooks.directory.get("owner").await.unwrap();
    w.env
        .forge
        .orgs
        .set_member(&owner, &s.org.id, owner_p, Role::Member)
        .await
        .unwrap();
    let off = patch(json!([{"op": "replace", "path": "active", "value": false}]));
    assert_eq!(
        call(&w, t, "PATCH", &format!("/Users/{lo_id}"), &[], off)
            .await
            .0,
        200
    );
    assert!(
        w.env
            .forge
            .orgs
            .member(&owner, &s.org.id, &lo_pid)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        admit(&w, &[&lo_pid]).await.unwrap_err().subcode,
        "principal_blocked"
    );
}
on_both_backends!(scim_deprovisioning_removes_access_everywhere);

/// AS-ORG-01: provision a team through SCIM, grant it a custom role on three repos, verify
/// access.
async fn as_org_01_scim_team_custom_role_on_three_repos(w: World) {
    let s = setup(&w).await;
    let t = &s.token;
    let owner = cx("owner");
    let repos = [
        make_repo(&w.env, &s.org, "alpha").await,
        make_repo(&w.env, &s.org, "beta").await,
        make_repo(&w.env, &s.org, "gamma").await,
    ];
    let unrelated = make_repo(&w.env, &s.org, "delta").await;

    // 1. The IdP provisions two users and a group; the group becomes a synced team.
    let mut uids = Vec::new();
    for n in ["ann@example.com", "ben@example.com"] {
        let (st, u) = call(&w, t, "POST", "/Users", &[], user(n)).await;
        assert_eq!(st, 201);
        uids.push(u["id"].as_str().unwrap().to_string());
    }
    let group = json!({"displayName": "Platform Releasers", "members": [{"value": uids[0]}, {"value": uids[1]}]});
    let (st, g) = call(&w, t, "POST", "/Groups", &[], group).await;
    assert_eq!(st, 201);
    let gid = g["id"].as_str().unwrap().to_string();
    let ann = principal_of_user(&w, &s, "ann@example.com").await;
    let ben = principal_of_user(&w, &s, "ben@example.com").await;

    // 2. A custom role, granted to the team on three repos.
    let role = w
        .ent
        .roles
        .create(
            &owner,
            &s.org,
            PolicyRoleKind::Repo,
            "Releaser",
            "lands and manages refs",
            "write",
            vec!["manage_refs".into(), "lease".into()],
        )
        .await
        .unwrap();
    for repo in &repos {
        w.ent
            .teams
            .grant_repo(&owner, &s.org, "platform-releasers", repo, &role.id)
            .await
            .unwrap();
    }

    // 3. Verify access: the team's members hold the role's actions on exactly those repos.
    for p in [&ann, &ben] {
        for repo in &repos {
            let access = w.ent.roles.repo_actions(&owner, repo, p).await.unwrap();
            for a in [
                Action::Read,
                Action::Commit,
                Action::Land,
                Action::ManageRefs,
                Action::Lease,
            ] {
                assert!(
                    access.actions.contains(&a),
                    "{p} lacks {a} on {}",
                    repo.full_name
                );
            }
            assert!(!access.actions.contains(&Action::ForceUpdate));
            assert!(!access.actions.contains(&Action::ManageSettings));
            let perm = w
                .env
                .forge
                .orgs
                .effective_role(&owner, repo, p)
                .await
                .unwrap();
            assert_eq!(perm.role, RepoRole::Write as i32);
            assert!(
                perm.sources
                    .iter()
                    .any(|s| s == "team:platform-releasers[Releaser]"),
                "{:?}",
                perm.sources
            );
        }
        assert!(
            w.ent
                .roles
                .repo_actions(&owner, &unrelated, p)
                .await
                .unwrap()
                .actions
                .is_empty(),
            "the fourth repo is out of reach"
        );
    }
    // Through the real authorization path with the member's token.
    let ann_token = w.env.full(&ann);
    let caller = w
        .env
        .forge
        .authn
        .authenticate(&common::req(&ann_token, ()))
        .unwrap();
    for repo in &repos {
        w.env
            .forge
            .authz
            .repo(&caller, repo, Action::Commit, &[], None, None)
            .await
            .unwrap();
        w.ent
            .roles
            .check_chain(&caller.cx, repo, &caller.cx.chain, Action::ManageRefs)
            .await
            .unwrap();
    }
    assert!(
        w.env
            .forge
            .authz
            .repo(&caller, &unrelated, Action::Read, &[], None, None)
            .await
            .is_err()
    );
    // Someone outside the group has nothing; a token narrowed to one repo cannot touch another.
    let reader_token = w.env.full("reader");
    let reader = w
        .env
        .forge
        .authn
        .authenticate(&common::req(&reader_token, ()))
        .unwrap();
    assert!(
        w.env
            .forge
            .authz
            .repo(&reader, &repos[0], Action::Read, &[], None, None)
            .await
            .is_err()
    );
    let narrow = w.env.token(
        &ann,
        bee_auth::Scope::new().repos([repos[0].id.clone()]).unwrap(),
    );
    let narrow = w
        .env
        .forge
        .authn
        .authenticate(&common::req(&narrow, ()))
        .unwrap();
    assert!(
        w.env
            .forge
            .authz
            .repo(&narrow, &repos[0], Action::Commit, &[], None, None)
            .await
            .is_ok()
    );
    assert!(
        w.env
            .forge
            .authz
            .repo(&narrow, &repos[1], Action::Commit, &[], None, None)
            .await
            .is_err()
    );

    // 4. The IdP removes Ben from the group: his access disappears on all three repos at once.
    let rm = patch(json!([{"op": "remove", "path": format!("members[value eq \"{}\"]", uids[1])}]));
    assert_eq!(
        call(&w, t, "PATCH", &format!("/Groups/{gid}"), &[], rm)
            .await
            .0,
        200
    );
    for repo in &repos {
        assert!(
            w.ent
                .roles
                .repo_actions(&owner, repo, &ben)
                .await
                .unwrap()
                .actions
                .is_empty()
        );
        assert!(
            w.ent
                .roles
                .repo_actions(&owner, repo, &ann)
                .await
                .unwrap()
                .actions
                .contains(&Action::ManageRefs)
        );
    }
    // 5. Deprovisioning Ann removes everything everywhere, and admission refuses her token.
    let off = patch(json!([{"op": "replace", "path": "active", "value": false}]));
    assert_eq!(
        call(&w, t, "PATCH", &format!("/Users/{}", uids[0]), &[], off)
            .await
            .0,
        200
    );
    for repo in &repos {
        assert!(
            w.ent
                .roles
                .repo_actions(&owner, repo, &ann)
                .await
                .unwrap()
                .actions
                .is_empty()
        );
    }
    assert!(admit(&w, &[&ann]).await.is_err());

    // Everything left a trail.
    let actions = support::audit_actions(&w, &format!("org:{}", s.org.id)).await;
    for a in [
        "enterprise.scim_user_create",
        "enterprise.scim_group_create",
        "team.create",
        "team.group_link",
        "role.create",
        "team.repo_grant",
        "enterprise.scim_group_update",
        "enterprise.scim_deprovision",
    ] {
        assert!(actions.iter().any(|x| x == a), "{a} missing in {actions:?}");
    }
}
on_both_backends!(as_org_01_scim_team_custom_role_on_three_repos);
