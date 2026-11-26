//! Enterprise hooks on the request path of the real server: SCIM deprovisioning, app
//! installation tokens, moderation blocks and IP allow lists, on VCS and forge RPCs alike.

mod common;

use base64::Engine as _;
use common::*;
use serde_json::{Value, json};

/// A JSON call with an explicit client address (`trust_forwarded_for` servers only).
async fn call_from(
    t: &TestServer,
    token: &str,
    service: &str,
    method: &str,
    body: Value,
    ip: &str,
) -> (u16, Value) {
    let auth = format!("Bearer {token}");
    let url = format!("{}/bee.v1.{service}/{method}", t.url());
    let headers = [
        ("content-type", "application/json"),
        ("authorization", auth.as_str()),
        ("x-forwarded-for", ip),
    ];
    let (status, _, body) = http("POST", &url, &headers, body.to_string().into_bytes()).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn reason_of(v: &Value) -> String {
    v["details"]
        .as_array()
        .and_then(|d| d.first())
        .and_then(|d| d["debug"]["reason"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}

async fn scim(t: &TestServer, token: &str, method: &str, path: &str, body: Value) -> (u16, Value) {
    let auth = format!("Bearer {token}");
    let url = format!("{}/scim/v2/organizations/dev{path}", t.url());
    let headers = [
        ("content-type", "application/scim+json"),
        ("authorization", auth.as_str()),
    ];
    let body = if body.is_null() {
        Vec::new()
    } else {
        body.to_string().into_bytes()
    };
    let (status, _, out) = http(method, &url, &headers, body).await;
    (status, serde_json::from_slice(&out).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scim_deprovisioning_removes_repo_access_immediately() {
    let t = start_default().await;
    let scope = json!({ "org": "dev" });
    t.call(
        "EnterpriseService",
        "UpdateScimConfig",
        json!({ "scope": scope, "enabled": true }),
    )
    .await
    .expect("enable scim");
    let tok = t
        .call(
            "EnterpriseService",
            "CreateScimToken",
            json!({ "scope": scope, "description": "okta" }),
        )
        .await
        .expect("scim token");
    let scim_token = tok["token"].as_str().unwrap().to_string();
    let (st, user) = scim(
        &t,
        &scim_token,
        "POST",
        "/Users",
        json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
            "userName": "alice@example.com",
            "externalId": "okta-1",
            "emails": [{ "value": "alice@example.com", "primary": true }],
            "active": true,
        }),
    )
    .await;
    assert_eq!(st, 201, "{user}");
    let scim_id = user["id"].as_str().unwrap().to_string();
    let ids = t
        .call(
            "EnterpriseService",
            "ListScimIdentities",
            json!({ "scope": scope, "includeInactive": true }),
        )
        .await
        .unwrap();
    let pid = ids["identities"][0]["principal"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let alice = t.token_for(&bee_auth::Principal::human(pid.clone(), "alice"), &[]);
    // Access before: a VCS read and a forge read.
    t.call_as(&alice, "RefService", "ListRefs", json!({ "repo": repo() }))
        .await
        .expect("vcs read before");
    t.call_as(
        &alice,
        "IssueService",
        "ListIssues",
        json!({ "repo": repo() }),
    )
    .await
    .expect("forge read before");

    // Deprovision through PATCH active=false: the very next request is refused.
    let (st, r) = scim(
        &t,
        &scim_token,
        "PATCH",
        &format!("/Users/{scim_id}"),
        json!({
            "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
            "Operations": [{ "op": "replace", "path": "active", "value": false }],
        }),
    )
    .await;
    assert_eq!(st, 200, "{r}");
    let e = t
        .call_as(&alice, "RefService", "ListRefs", json!({ "repo": repo() }))
        .await
        .expect_err("vcs read after");
    assert_eq!(
        (reason(&e), e.subcode.as_str()),
        ("PERMISSION_DENIED", "principal_blocked")
    );
    let e = t
        .call_as(
            &alice,
            "IssueService",
            "ListIssues",
            json!({ "repo": repo() }),
        )
        .await
        .expect_err("forge read after");
    assert_eq!(reason(&e), "PERMISSION_DENIED");
    // An agent acting for her is refused too.
    let agent = bee_auth::Principal::agent(
        bee_server::util::new_ulid(t.srv.state.now_ms())
            .unwrap()
            .to_text(),
        "helper",
        bee_auth::AgentInfo {
            owner: bee_policy::PrincipalId::new(pid.clone()),
            runtime: "claude-code".into(),
            purpose: "help".into(),
        },
    );
    let on_behalf = t.token_for(&agent, &[&pid]);
    let e = t
        .call_as(&on_behalf, "QueryService", "Log", json!({ "repo": repo() }))
        .await
        .expect_err("delegated read after");
    assert_eq!(reason(&e), "PERMISSION_DENIED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn installation_tokens_reach_only_their_repos() {
    let t = start_default().await;
    let other = t
        .call(
            "RepoService",
            "CreateRepo",
            json!({ "org": "dev", "name": "other", "autoInit": true }),
        )
        .await
        .expect("second repo");
    assert_eq!(other["fullName"], "dev/other");
    let manifest = json!({
        "name": "Lint Bot",
        "url": "https://example.com/lint",
        "public": false,
        "default_permissions": { "contents": "write", "issues": "write" },
        "default_events": [],
        "callback_urls": ["https://example.com/callback"],
    });
    let app = t
        .call(
            "AppService",
            "CreateAppFromManifest",
            json!({ "org": "dev", "manifestJson": manifest.to_string() }),
        )
        .await
        .expect("app");
    let slug = app["app"]["slug"].as_str().unwrap().to_string();
    let inst = t
        .call(
            "AppService",
            "CreateAppInstallation",
            json!({
                "app": slug, "org": "dev",
                "repositorySelection": "APP_REPOSITORY_SELECTION_SELECTED",
                "repos": [repo()],
            }),
        )
        .await
        .expect("install");
    let tok = t
        .call(
            "AppService",
            "CreateInstallationToken",
            json!({ "installationId": inst["id"], "ttlSeconds": 600 }),
        )
        .await
        .expect("installation token");
    let bot = tok["token"].as_str().unwrap().to_string();
    // The installed repo: VCS reads and writes, forge reads.
    t.call_as(&bot, "RefService", "ListRefs", json!({ "repo": repo() }))
        .await
        .expect("installed repo read");
    let (tip, _) = t.tip("main").await;
    t.commit_as(&bot, &tip, &[("lint.txt", b"ok\n")], 501, "lint")
        .await
        .expect("installed repo write");
    t.call_as(
        &bot,
        "IssueService",
        "ListIssues",
        json!({ "repo": repo() }),
    )
    .await
    .expect("installed repo issues");
    // Another repo of the same org: refused at the token and in the forge.
    let other_repo = json!({ "fullName": "dev/other" });
    let e = t
        .call_as(
            &bot,
            "RefService",
            "ListRefs",
            json!({ "repo": other_repo }),
        )
        .await
        .expect_err("other repo vcs");
    assert!(
        matches!(
            reason(&e),
            "POLICY_DENIED" | "PERMISSION_DENIED" | "NOT_FOUND"
        ),
        "{e:?}"
    );
    let e = t
        .call_as(
            &bot,
            "IssueService",
            "ListIssues",
            json!({ "repo": other_repo }),
        )
        .await
        .expect_err("other repo forge");
    assert!(
        matches!(
            reason(&e),
            "POLICY_DENIED" | "PERMISSION_DENIED" | "NOT_FOUND"
        ),
        "{e:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_moderation_block_prevents_comments_and_reviews() {
    let t = start_default().await;
    let (troll, ttok) = t.principal("human", "troll", "").await;
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": troll, "role": "ROLE_MEMBER" }),
    )
    .await
    .unwrap();
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[("m.txt", b"m\n")], 61, "m").await["id"]);
    let cr = t
        .call(
            "ChangeService",
            "OpenChange",
            json!({ "repo": repo(), "head": id_json(&head), "title": "m" }),
        )
        .await
        .unwrap();
    let cr_id = cr["id"].as_str().unwrap().to_string();
    t.call_as(
        &ttok,
        "ChangeService",
        "CreateChangeComment",
        json!({ "repo": repo(), "changeRequestId": cr_id, "body": "first" }),
    )
    .await
    .expect("comment before the block");
    t.call(
        "ModerationService",
        "BlockUser",
        json!({ "scope": { "org": "dev" }, "principalId": troll, "reason": "abuse" }),
    )
    .await
    .expect("block");
    let e = t
        .call_as(
            &ttok,
            "ChangeService",
            "CreateChangeComment",
            json!({ "repo": repo(), "changeRequestId": cr_id, "body": "again" }),
        )
        .await
        .expect_err("blocked comment");
    assert_eq!(reason(&e), "BLOCKED");
    let e = t
        .call_as(
            &ttok,
            "ChangeService",
            "SubmitReview",
            json!({
                "repo": repo(), "changeRequestId": cr_id, "revision": 1,
                "verdict": "VERDICT_COMMENT", "body": "x",
            }),
        )
        .await
        .expect_err("blocked review");
    assert_eq!(reason(&e), "BLOCKED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ip_allow_list_denies_other_addresses_on_every_surface() {
    let t = start_with(|c| c.server.trust_forwarded_for = true).await;
    let scope = json!({ "org": "dev" });
    let office = "10.1.2.3";
    let (st, r) = call_from(
        &t,
        &t.token,
        "EnterpriseService",
        "CreateIpAllowListEntry",
        json!({ "scope": scope, "cidr": "10.0.0.0/8", "active": true, "description": "office" }),
        office,
    )
    .await;
    assert_eq!(st, 200, "{r}");
    let (st, r) = call_from(
        &t,
        &t.token,
        "EnterpriseService",
        "UpdateIpAllowListSettings",
        json!({ "scope": scope, "enabled": true }),
        office,
    )
    .await;
    assert_eq!(st, 200, "{r}");
    for (service, method) in [("RefService", "ListRefs"), ("IssueService", "ListIssues")] {
        let (st, r) = call_from(
            &t,
            &t.token,
            service,
            method,
            json!({ "repo": repo() }),
            office,
        )
        .await;
        assert_eq!(st, 200, "{service} from the office: {r}");
        let (st, r) = call_from(
            &t,
            &t.token,
            service,
            method,
            json!({ "repo": repo() }),
            "192.0.2.7",
        )
        .await;
        assert_ne!(st, 200, "{service} from elsewhere");
        assert_eq!(reason_of(&r), "IP_NOT_ALLOWED", "{r}");
    }
    // git over smart HTTP is refused from elsewhere too.
    let basic = format!("Basic {}", B64.encode(format!("x:{}", t.token)));
    let url = format!(
        "{}/dev/playground.git/info/refs?service=git-upload-pack",
        t.url()
    );
    let (st, _, body) = http(
        "GET",
        &url,
        &[
            ("authorization", basic.as_str()),
            ("x-forwarded-for", "192.0.2.7"),
        ],
        Vec::new(),
    )
    .await;
    assert_ne!(st, 200, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("IP_NOT_ALLOWED"));
}
