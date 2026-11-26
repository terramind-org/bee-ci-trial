//! `CreateAgent` keeps a human or service within its recorded org or a forge membership.
#![allow(clippy::result_large_err)]

mod common;

use bee_proto::v1::ErrorDetail;
use common::*;
use serde_json::json;

async fn set_recorded_org(t: &TestServer, id: &str, org: &str) {
    let mut record = t.srv.state.principal(id).await.unwrap().unwrap();
    record.org = org.into();
    t.srv
        .state
        .kv
        .put(
            bee_server::auth::NS_PRINCIPAL,
            id.as_bytes(),
            serde_json::to_vec(&record).unwrap(),
        )
        .await
        .unwrap();
}

async fn create(
    t: &TestServer,
    token: &str,
    display: &str,
    org: &str,
) -> Result<serde_json::Value, ErrorDetail> {
    t.call_as(
        token,
        "AuthService",
        "CreateAgent",
        json!({ "display": display, "runtime": "codex", "purpose": "test", "org": org }),
    )
    .await
}

async fn add_member(t: &TestServer, org: &str, principal: &str) {
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": org, "principalId": principal, "role": "ROLE_MEMBER" }),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creators_need_membership_except_for_their_own_org_or_server_admins() {
    let t = start_default().await;
    t.call("OrgService", "CreateOrg", json!({ "name": "acme" }))
        .await
        .unwrap();
    let acme_id = t
        .call("OrgService", "GetOrg", json!({ "nameOrId": "acme" }))
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (stranger, stranger_token) = t.principal("human", "stranger", "").await;
    set_recorded_org(&t, &stranger, "personal").await;
    let existing = create(&t, &stranger_token, "existing", "acme")
        .await
        .expect_err("stranger denied for existing org");
    let absent = create(&t, &stranger_token, "absent", "not-an-org")
        .await
        .expect_err("stranger denied for absent org");
    assert_eq!(existing.reason, "PERMISSION_DENIED", "{existing:?}");
    assert_eq!(
        (
            existing.reason.as_str(),
            existing.message.as_str(),
            existing.subcode.as_str()
        ),
        (
            absent.reason.as_str(),
            absent.message.as_str(),
            absent.subcode.as_str()
        ),
        "existing and absent orgs must not be distinguishable"
    );

    add_member(&t, "acme", &stranger).await;
    for (display, org) in [("by-name", "acme"), ("by-id", acme_id.as_str())] {
        let created = create(&t, &stranger_token, display, org).await.unwrap();
        let record = t
            .srv
            .state
            .principal(created["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.org, "acme");
    }
    let empty = create(&t, &stranger_token, "own", "").await.unwrap();
    assert_eq!(
        t.srv
            .state
            .principal(empty["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap()
            .org,
        "personal"
    );

    // The dev root is configured as a server operator and remains able to bootstrap any org.
    let admin = create(&t, &t.token, "admin", "acme").await.unwrap();
    assert_eq!(
        t.srv
            .state
            .principal(admin["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap()
            .org,
        "acme"
    );

    // Operators may still bootstrap an org label before the forge has a record for it.
    let bootstrap = create(&t, &t.token, "bootstrap", "not-yet-an-org")
        .await
        .expect("operator may use an unresolved org label");
    assert_eq!(
        t.srv
            .state
            .principal(bootstrap["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap()
            .org,
        "not-yet-an-org"
    );

    // A recorded org remains usable even if this server has no forge record for it.
    let (label_owner, label_owner_token) = t.principal("human", "label-owner", "").await;
    set_recorded_org(&t, &label_owner, "personal-label").await;
    let own_label = create(&t, &label_owner_token, "own-label", "personal-label")
        .await
        .expect("caller may use their recorded unresolved org");
    assert_eq!(
        t.srv
            .state
            .principal(own_label["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap()
            .org,
        "personal-label"
    );

    let (service, service_token) = t.principal("service", "ci", "").await;
    set_recorded_org(&t, &service, "personal").await;
    assert_eq!(
        create(&t, &service_token, "service-denied", "acme")
            .await
            .unwrap_err()
            .reason,
        "PERMISSION_DENIED"
    );
    add_member(&t, "acme", &service).await;
    assert!(
        create(&t, &service_token, "service-member", "acme")
            .await
            .is_ok()
    );

    let agent = create(&t, &t.token, "worker", "dev").await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let agent_token = t
        .call(
            "AuthService",
            "MintToken",
            json!({ "principalId": agent, "scope": {} }),
        )
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        create(&t, &agent_token, "other-org", "acme")
            .await
            .unwrap_err()
            .reason,
        "PERMISSION_DENIED"
    );
    let child = create(&t, &agent_token, "child", "").await.unwrap();
    assert_eq!(child["ownerId"], agent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forge_disabled_only_allows_operators_and_callers_own_orgs() {
    let t = start_with(|cfg| cfg.forge.enabled = false).await;
    let admin = create(&t, &t.token, "admin", "unresolved-org")
        .await
        .expect("operator may use any org with forge disabled");
    assert_eq!(
        t.srv
            .state
            .principal(admin["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap()
            .org,
        "unresolved-org"
    );

    let (_, stranger_token) = t.principal("human", "stranger", "").await;
    assert_eq!(
        create(&t, &stranger_token, "stranger", "unresolved-org")
            .await
            .expect_err("non-member is denied with forge disabled")
            .reason,
        "PERMISSION_DENIED"
    );
}
