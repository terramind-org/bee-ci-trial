//! Operator scope (`docs/spec/operations.md` §1.1, SPEC §9.7): server-wide operator RPCs
//! answer only principals listed in `ops.operators` or `forge.site_admins`; administering an
//! org (any org) never reaches them, and org-scoped administration follows the org the
//! request names. Dev mode lists the dev user as an operator explicitly.

mod common;

use bee_proto::v1::ErrorDetail;
use common::*;
use serde_json::{Value, json};

/// Every server-wide operator RPC reachable over HTTP/JSON, with a request that names
/// nothing it could act on (so an operator call is harmless).
fn server_wide() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("OpsService", "GetOpsClusterStatus", json!({})),
        ("OpsService", "ListMetricDescriptors", json!({})),
        (
            "OpsService",
            "QueryMetrics",
            json!({ "name": "bee_rpc_requests_total" }),
        ),
        (
            "OpsService",
            "QueryMetricsRange",
            json!({ "name": "bee_rpc_requests_total" }),
        ),
        ("OpsService", "QueryOpsLogs", json!({})),
        ("OpsService", "GetOpsTrace", json!({ "traceId": "none" })),
        ("OpsService", "SearchOpsTraces", json!({})),
        ("OpsService", "ValidateOpsConfig", json!({ "toml": "" })),
        ("OpsService", "GetEffectiveOpsConfig", json!({})),
        ("OpsService", "ListOpsRollouts", json!({})),
        ("OpsService", "GetOpsRollout", json!({ "id": "none" })),
        ("OpsService", "PauseOpsRollout", json!({ "id": "none" })),
        ("OpsService", "ResumeOpsRollout", json!({ "id": "none" })),
        ("OpsService", "RollbackOpsRollout", json!({ "id": "none" })),
        ("OpsService", "CreateOpsIncident", json!({ "title": "" })),
        ("OpsService", "UpdateOpsIncident", json!({ "id": "none" })),
        (
            "OpsService",
            "SetOpsStatusComponent",
            json!({ "id": "none" }),
        ),
        ("OpsService", "ListOpsBackups", json!({})),
        ("OpsService", "ListOpsRestoreDrills", json!({})),
        ("OpsService", "GetOpsCacheStats", json!({})),
        ("AdminService", "ListBackups", json!({})),
        (
            "AdminService",
            "CreateBackup",
            json!({ "cell": "no-such-cell" }),
        ),
        ("AdminService", "StartRestore", json!({})),
        ("AdminService", "GetRestore", json!({ "id": "none" })),
        (
            "AdminService",
            "RotateSigningKey",
            json!({ "algorithm": "rsa" }),
        ),
        (
            "AdminService",
            "RetireSigningKey",
            json!({ "keyId": "not-a-number" }),
        ),
    ]
}

/// The reasons the operator and org checks answer.
fn denied(r: &Result<Value, ErrorDetail>) -> bool {
    matches!(r, Err(e) if e.reason == "POLICY_DENIED" || e.reason == "PERMISSION_DENIED")
}

/// A human principal with a fixed id stored in the server table, and a root token for it.
async fn human(t: &TestServer, id: &str, display: &str) -> String {
    let state = &t.srv.state;
    let rec = bee_server::auth::PrincipalRecord {
        id: id.into(),
        kind: "human".into(),
        display: display.into(),
        org: t.srv.dev.as_ref().unwrap().org.0.to_text(),
        owner: String::new(),
        runtime: String::new(),
        purpose: String::new(),
        external_ids: Vec::new(),
        groups: Vec::new(),
        deactivated: false,
    };
    state
        .kv
        .put(
            bee_server::auth::NS_PRINCIPAL,
            id.as_bytes(),
            serde_json::to_vec(&rec).unwrap(),
        )
        .await
        .unwrap();
    t.token_for(&rec.to_auth(), &[])
}

/// `alice` creates org `acme` (its forge owner) and also gets a server-store org admin row
/// for it: the row the old any-org shortcut accepted for operator RPCs.
async fn acme_admin(t: &TestServer) -> (String, String) {
    let (alice, token) = t.principal("human", "alice", "").await;
    t.call_as(&token, "OrgService", "CreateOrg", json!({ "name": "acme" }))
        .await
        .expect("alice creates acme");
    let org = t
        .srv
        .state
        .meta
        .org_by_name("acme")
        .await
        .unwrap()
        .expect("acme has a VCS org");
    t.srv
        .state
        .kv
        .put(
            bee_server::auth::NS_ORG_ADMIN,
            format!("{}/{alice}", org.id.0.to_text()).as_bytes(),
            Vec::new(),
        )
        .await
        .unwrap();
    (alice, token)
}

/// A token for `principal` limited to `actions` (30 minutes).
fn token_with_actions(
    t: &TestServer,
    principal: &bee_auth::Principal,
    actions: &[bee_policy::Action],
) -> String {
    let state = &t.srv.state;
    let now = state.now_ms() / 1000;
    state
        .ring
        .read()
        .unwrap()
        .mint(
            &bee_auth::MintRequest {
                principal: principal.clone(),
                on_behalf_of: Vec::new(),
                scope: bee_auth::Scope::new()
                    .actions(actions.iter().copied())
                    .expires_at(now + 1800),
            },
            now,
        )
        .unwrap()
        .text
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_org_admin_is_refused_on_every_server_wide_rpc() {
    let t = start_default().await;
    let (alice, token) = acme_admin(&t).await;
    for (svc, method, req) in server_wide() {
        let r = t.call_as(&token, svc, method, req).await;
        let e = r.expect_err(&format!("{svc}.{method} must refuse an org admin"));
        assert_eq!(e.reason, "POLICY_DENIED", "{svc}.{method}: {e:?}");
        assert!(
            e.message.contains("not an operator"),
            "{svc}.{method}: {}",
            e.message
        );
    }
    // The streaming tail too (gRPC).
    let mut ops = bee_proto::v1::ops_service_client::OpsServiceClient::new(t.grpc_channel().await);
    let s = ops
        .tail_ops_logs(t.bearer(&token, bee_proto::v1::TailOpsLogsRequest::default()))
        .await
        .expect_err("TailOpsLogs must refuse an org admin");
    assert_eq!(detail(&s).reason, "POLICY_DENIED");
    // No key was rotated or retired, no backup started.
    let keys = t.srv.state.kv.scan("backup", b"", None, 10).await.unwrap();
    assert!(keys.is_empty(), "an org admin started a backup");
    // The dev user, an operator, still gets through on the same calls.
    for (svc, method, req) in server_wide() {
        let r = t.call(svc, method, req).await;
        assert!(!denied(&r), "dev operator on {svc}.{method}: {r:?}");
    }
    assert_ne!(alice, t.dev_user());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn org_scoped_admin_follows_the_named_org() {
    let t = start_default().await;
    let (_, alice) = acme_admin(&t).await;
    // Org A (acme): its admin succeeds.
    let f = t
        .call_as(&alice, "AdminService", "RunFsck", json!({ "org": "acme" }))
        .await
        .expect("acme admin runs fsck on acme");
    assert_eq!(f["clean"], true, "{f}");
    t.call_as(
        &alice,
        "AdminService",
        "RunGc",
        json!({ "org": "acme", "dryRun": true }),
    )
    .await
    .expect("acme admin runs gc on acme");
    // Org B (dev): refused, however many orgs alice administers.
    for (method, req) in [
        ("RunFsck", json!({ "org": "dev" })),
        ("RunGc", json!({ "org": "dev", "dryRun": true })),
        ("RunGc", json!({ "org": "dev" })),
    ] {
        let r = t.call_as(&alice, "AdminService", method, req).await;
        assert!(denied(&r), "acme admin on dev {method}: {r:?}");
    }
    // Policy rulesets of org B are not alice's to write either.
    let r = t
        .call_as(
            &alice,
            "PolicyService",
            "CreateRuleset",
            json!({ "org": "dev", "name": "x" }),
        )
        .await;
    assert!(denied(&r), "acme admin writes dev rulesets: {r:?}");
    // Being an operator does not make the dev user an admin of acme.
    let r = t
        .call("AdminService", "RunGc", json!({ "org": "acme" }))
        .await;
    assert!(denied(&r), "operator purges acme: {r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configured_operators_and_site_admins_run_server_wide_rpcs() {
    let ops_id = bee_server::util::new_ulid(1).unwrap().to_text();
    let site_id = bee_server::util::new_ulid(2).unwrap().to_text();
    let (o, s) = (ops_id.clone(), site_id.clone());
    let t = start_with(move |c| {
        c.ops.operators = vec![o];
        c.forge.site_admins = vec![s];
    })
    .await;
    let ops = human(&t, &ops_id, "ops").await;
    let site = human(&t, &site_id, "site").await;
    for token in [&ops, &site] {
        for (svc, method, req) in server_wide() {
            let r = t.call_as(token, svc, method, req).await;
            assert!(!denied(&r), "operator on {svc}.{method}: {r:?}");
        }
        for (svc, method) in [
            ("OpsService", "GetEffectiveOpsConfig"),
            ("OpsService", "ListMetricDescriptors"),
            ("OpsService", "QueryOpsLogs"),
            ("OpsService", "SearchOpsTraces"),
            ("AdminService", "ListBackups"),
        ] {
            t.call_as(token, svc, method, json!({}))
                .await
                .unwrap_or_else(|e| panic!("{svc}.{method}: {e:?}"));
        }
    }
    let k = t
        .call_as(&ops, "AdminService", "RotateSigningKey", json!({}))
        .await
        .expect("operator rotates the signing key");
    let id = k["key"]["keyId"].as_str().expect("key id").to_string();
    assert!(!id.is_empty());
    // Operator rights are not org rights: no org admin row, no forge role in `dev`.
    let r = t
        .call_as(&ops, "AdminService", "RunGc", json!({ "org": "dev" }))
        .await;
    assert!(denied(&r), "operator purges dev: {r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dev_mode_lists_the_dev_user_as_an_operator() {
    let t = start_default().await;
    let cfg = t
        .call("OpsService", "GetEffectiveOpsConfig", json!({}))
        .await
        .expect("dev user reads the effective config");
    let toml = cfg["toml"].as_str().expect("toml");
    let parsed: toml::Value = toml::from_str(toml).expect("effective config parses");
    let operators: Vec<&str> = parsed["ops"]["operators"]
        .as_array()
        .expect("ops.operators")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(operators, vec![t.dev_user().as_str()]);
    // Rotate twice and retire the middle key (it signed nothing; the dev token's key stays).
    let k = t
        .call("AdminService", "RotateSigningKey", json!({}))
        .await
        .expect("dev user rotates the signing key");
    let id = k["key"]["keyId"].as_str().expect("key id").to_string();
    t.call("AdminService", "RotateSigningKey", json!({}))
        .await
        .expect("dev user rotates again");
    t.call("AdminService", "RetireSigningKey", json!({ "keyId": id }))
        .await
        .expect("dev user retires the superseded key");
    t.call("AdminService", "RunFsck", json!({ "org": "dev" }))
        .await
        .expect("dev user still administers dev");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_tokens_still_narrow_and_delegation_does_not_widen() {
    let t = start_default().await;
    let dev = bee_auth::Principal::human(t.dev_user(), "dev");
    // A dev-user token without `manage_settings` is refused by its scope.
    let narrow = token_with_actions(&t, &dev, &[bee_policy::Action::Read]);
    for (svc, method, req) in server_wide() {
        let e = t
            .call_as(&narrow, svc, method, req)
            .await
            .expect_err("a read-only token reaches an operator RPC");
        assert_eq!(e.reason, "POLICY_DENIED", "{svc}.{method}");
        assert!(
            e.message.contains("manage_settings"),
            "{svc}.{method}: {}",
            e.message
        );
    }
    let wide = token_with_actions(&t, &dev, &[bee_policy::Action::ManageSettings]);
    t.call_as(&wide, "OpsService", "GetEffectiveOpsConfig", json!({}))
        .await
        .expect("manage_settings token reaches operator RPCs");
    // An unlisted agent acting for the operator is no operator.
    let agent = bee_auth::Principal::agent(
        bee_server::util::new_ulid(t.srv.state.now_ms())
            .unwrap()
            .to_text(),
        "helper",
        bee_auth::AgentInfo {
            owner: bee_policy::PrincipalId::new(t.dev_user()),
            runtime: "claude-code".into(),
            purpose: "tests".into(),
        },
    );
    let dev_user = t.dev_user();
    let delegated = t.token_for(&agent, &[dev_user.as_str()]);
    let e = t
        .call_as(&delegated, "OpsService", "GetEffectiveOpsConfig", json!({}))
        .await
        .expect_err("an unlisted agent of the operator");
    assert_eq!(e.reason, "POLICY_DENIED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_listed_agent_acting_for_an_org_admin_is_no_operator() {
    let agent_id = bee_server::util::new_ulid(3).unwrap().to_text();
    let listed = agent_id.clone();
    let t = start_with(move |c| c.ops.operators = vec![listed]).await;
    let (alice, _) = acme_admin(&t).await;
    let agent = bee_auth::Principal::agent(
        agent_id,
        "ops-bot",
        bee_auth::AgentInfo {
            owner: bee_policy::PrincipalId::new(alice.clone()),
            runtime: "claude-code".into(),
            purpose: "tests".into(),
        },
    );
    let token = t.token_for(&agent, &[alice.as_str()]);
    for (svc, method, req) in server_wide() {
        let e = t
            .call_as(&token, svc, method, req)
            .await
            .expect_err("a listed agent acting for an org admin");
        assert_eq!(e.reason, "POLICY_DENIED", "{svc}.{method}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enterprise_rulesets_answer_its_owners_and_its_own_org_admins() {
    let t = start_default().await;
    let (_, alice) = acme_admin(&t).await;
    let (bob_id, bob) = t.principal("human", "bob", "").await;
    t.call_as(&bob, "OrgService", "CreateOrg", json!({ "name": "bobco" }))
        .await
        .expect("bob creates bobco");
    let (boss_id, boss) = t.principal("human", "boss", "").await;
    let w = t.srv.state.hub.get().expect("forge runs").clone();
    let sys = bee_forge::Cx::system();
    w.enterprise
        .accounts
        .create_enterprise(
            &sys,
            "globex",
            "Globex",
            bee_proto::v1::Principal {
                id: boss_id,
                display: "boss".into(),
                ..Default::default()
            },
        )
        .await
        .expect("enterprise");
    let acme = w.forge.orgs.get_org(&sys, "acme").await.expect("acme");
    w.enterprise
        .accounts
        .add_org(&sys, "globex", &acme)
        .await
        .expect("acme joins globex");
    let list = json!({ "enterprise": "globex" });
    // The owner reads.
    t.call_as(&boss, "PolicyService", "ListRulesets", list.clone())
        .await
        .expect("owner lists");
    // An admin of one of the enterprise's orgs reads, but does not write.
    t.call_as(&alice, "PolicyService", "ListRulesets", list.clone())
        .await
        .expect("acme admin lists globex rulesets");
    let r = t
        .call_as(
            &alice,
            "PolicyService",
            "CreateRuleset",
            json!({ "enterprise": "globex", "name": "x" }),
        )
        .await;
    assert!(denied(&r), "acme admin writes globex rulesets: {r:?}");
    // An admin of an org outside the enterprise does neither.
    let r = t
        .call_as(&bob, "PolicyService", "ListRulesets", list.clone())
        .await;
    assert!(
        denied(&r),
        "bobco admin {bob_id} lists globex rulesets: {r:?}"
    );
}
