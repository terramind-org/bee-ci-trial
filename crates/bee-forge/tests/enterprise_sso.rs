//! SAML SSO end to end: configuration rules, the test sign-in, assertion consumption (replay,
//! tampering, unknown identities), sessions and enforcement at admission, SCIM linking and
//! group-driven team sync, deprovisioning.

use crate::enterprise_support as support;
#[path = "enterprise/saml_signer.rs"]
mod signer;

use bee_forge::enterprise::common::{ResolvedScope, ScopeResolver};
use bee_forge::enterprise::network::AdmissionRequest;
use bee_forge::enterprise::saml::{b64_decode, b64_encode};
use bee_forge::enterprise::scim::ScimRequest;
use bee_forge::enterprise::sso::SsoPatch;
use bee_forge::enterprise::teams::{NewTeam, Viewer};
use bee_proto::v1::{ErrorReason, Role};
use serde_json::{Value, json};
use support::{World, add_member, cx, make_org};

struct Setup {
    org: bee_proto::v1::Org,
    scope: ResolvedScope,
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
    let scope = w
        .ent
        .accounts
        .resolve_org(&cx("owner"), "acme")
        .await
        .unwrap();
    Setup { org, scope }
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

fn tamper(b64: &str, from: &str, to: &str) -> String {
    let xml = String::from_utf8(b64_decode(b64).unwrap()).unwrap();
    assert!(xml.contains(from));
    b64_encode(xml.replace(from, to).as_bytes())
}

async fn sso_configuration_test_sign_in_and_enforcement(w: World) {
    let s = setup(&w).await;
    let sso = &w.ent.sso;
    let owner = cx("owner");

    // Service provider metadata is generated for the scope.
    assert_eq!(
        sso.sp_entity_id(&s.scope),
        "https://hub.example.com/saml/orgs/acme"
    );
    assert!(
        sso.sp_metadata(&s.scope)
            .contains("https://hub.example.com/saml/orgs/acme/acs")
    );

    // Enabling needs an IdP; enforcement needs SSO enabled and a successful test sign-in.
    let e = sso
        .update_config(
            &owner,
            &s.scope,
            SsoPatch {
                enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        (e.reason, e.subcode.as_str()),
        (ErrorReason::InvalidState, "sso_incomplete")
    );
    let e = sso
        .update_config(
            &owner,
            &s.scope,
            SsoPatch {
                idp_metadata_xml: Some("<nope/>".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "bad_saml");
    let cfg = sso
        .update_config(
            &owner,
            &s.scope,
            SsoPatch {
                idp_metadata_xml: Some(signer::metadata()),
                enabled: Some(true),
                session_lifetime_seconds: Some(8 * 3600),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(cfg.idp_entity_id, signer::IDP_ENTITY);
    assert_eq!(cfg.idp_sso_url, signer::IDP_SSO);
    assert!(cfg.idp_certificate_expires_at.is_some());
    assert_eq!(cfg.sp_acs_url, "https://hub.example.com/saml/orgs/acme/acs");
    let e = sso
        .update_config(
            &owner,
            &s.scope,
            SsoPatch {
                enforced: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        (e.reason, e.subcode.as_str()),
        (ErrorReason::InvalidState, "sso_untested")
    );
    assert!(
        sso.update_config(
            &owner,
            &s.scope,
            SsoPatch {
                session_lifetime_seconds: Some(0),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );

    // The test: metadata is checked, a sign-in URL returned, and completing it marks the config.
    let t = sso.test_config(&owner, &s.scope, "").await.unwrap();
    assert!(t.metadata_ok && t.problems.is_empty(), "{:?}", t.problems);
    assert!(
        t.test_sign_in_url
            .starts_with("https://idp.example.com/sso?SAMLRequest=")
    );
    let bad = sso.test_config(&owner, &s.scope, "<x/>").await.unwrap();
    assert!(!bad.metadata_ok && !bad.problems.is_empty() && bad.test_sign_in_url.is_empty());
    let relay = signer::relay_state_of(&t.test_sign_in_url);
    let now = w.clock_ms();
    let reply = signer::Reply::for_org("acme", "owner@example.com", now, Some(&relay));
    // A response for another SP's audience is refused.
    let mut wrong = reply.clone();
    wrong.audience = "https://hub.example.com/saml/orgs/other".into();
    wrong.assertion_id = "_wrong".into();
    assert_eq!(
        sso.consume_response(&owner, &s.scope, &signer::sign(&wrong), &relay)
            .await
            .unwrap_err()
            .subcode,
        "bad_saml"
    );
    // A modified response fails its digest.
    let good = signer::sign(&reply);
    let forged = tamper(&good, "owner@example.com", "admin@example.com");
    let e = sso
        .consume_response(&owner, &s.scope, &forged, &relay)
        .await
        .unwrap_err();
    assert!(e.message.contains("digest mismatch"), "{e}");
    // A relay state that was never issued is refused.
    assert_eq!(
        sso.consume_response(&owner, &s.scope, &good, "_bogus")
            .await
            .unwrap_err()
            .subcode,
        "bad_relay_state"
    );
    let out = sso
        .consume_response(&owner, &s.scope, &good, &relay)
        .await
        .unwrap();
    assert!(out.test && out.linked);
    assert_eq!(out.principal_id, "owner");
    assert!(
        sso.config(&owner, &s.scope)
            .await
            .unwrap()
            .last_tested_at
            .is_some()
    );
    // The same pending request is single use.
    assert!(
        sso.consume_response(&owner, &s.scope, &good, &relay)
            .await
            .is_err()
    );

    // Now enforcement can be turned on; changing the IdP while enforced needs a re-test first.
    let cfg = sso
        .update_config(
            &owner,
            &s.scope,
            SsoPatch {
                enforced: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(cfg.enforced);
    let e = sso
        .update_config(
            &owner,
            &s.scope,
            SsoPatch {
                idp_entity_id: Some("https://other".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "sso_untested");

    // Admission: members need a live session; non-members and the signed-in owner do not.
    assert!(admit(&w, &["owner"]).await.is_ok());
    let e = admit(&w, &["reader"]).await.unwrap_err();
    assert_eq!(e.reason, ErrorReason::SsoRequired);
    assert_eq!(
        admit(&w, &["agent", "reader"]).await.unwrap_err().reason,
        ErrorReason::SsoRequired,
        "the delegator counts"
    );
    assert!(
        admit(&w, &["outsider"]).await.is_ok(),
        "SSO applies to members"
    );

    // Login: an authenticated principal links its identity; later IdP-initiated sign-ins work.
    let (url, rid) = sso
        .start_sign_in(&cx("writer"), &s.scope, "login", "writer", "")
        .await
        .unwrap();
    assert!(url.contains("RelayState="));
    let r = signer::Reply::for_org("acme", "writer@corp.example", now, Some(&rid));
    let out = sso
        .consume_response(&cx("writer"), &s.scope, &signer::sign(&r), &rid)
        .await
        .unwrap();
    assert!(out.linked && !out.test);
    assert!(admit(&w, &["writer"]).await.is_ok());
    assert_eq!(out.session_expires_ms, now + 8 * 3600 * 1000);
    let idp_initiated = signer::Reply::for_org("acme", "writer@corp.example", now, None);
    let signed = signer::sign(&idp_initiated);
    let out = sso
        .consume_response(&owner, &s.scope, &signed, "")
        .await
        .unwrap();
    assert!(!out.linked);
    // Replays are refused, and an unknown identity cannot sign in without linking.
    let e = sso
        .consume_response(&owner, &s.scope, &signed, "")
        .await
        .unwrap_err();
    assert_eq!(
        (e.reason, e.subcode.as_str()),
        (ErrorReason::PermissionDenied, "assertion_replayed")
    );
    let stranger = signer::Reply::for_org("acme", "stranger@corp.example", now, None);
    let e = sso
        .consume_response(&owner, &s.scope, &signer::sign(&stranger), "")
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "no_identity");
    // Another principal cannot claim an identity that is already linked.
    let (_, rid2) = sso
        .start_sign_in(&cx("reader"), &s.scope, "login", "reader", "")
        .await
        .unwrap();
    let steal = signer::Reply::for_org("acme", "writer@corp.example", now, Some(&rid2));
    assert_eq!(
        sso.consume_response(&cx("reader"), &s.scope, &signer::sign(&steal), &rid2)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );

    // Identities and sessions.
    let (ids, _) = sso
        .identities(&owner, &s.scope, "corp", "", 50)
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);
    assert!(ids[0].session_expires_at.is_some());
    // Sessions end: by time, or when an admin revokes them.
    w.clock.advance(8 * 3600 * 1000 + 1000);
    assert_eq!(
        admit(&w, &["writer"]).await.unwrap_err().reason,
        ErrorReason::SsoRequired
    );
    let again = signer::Reply::for_org("acme", "writer@corp.example", w.clock_ms(), None);
    sso.consume_response(&owner, &s.scope, &signer::sign(&again), "")
        .await
        .unwrap();
    assert!(admit(&w, &["writer"]).await.is_ok());
    sso.revoke_session(&owner, &s.scope, "writer", false)
        .await
        .unwrap();
    assert_eq!(
        admit(&w, &["writer"]).await.unwrap_err().reason,
        ErrorReason::SsoRequired
    );
    assert!(
        sso.identities(&owner, &s.scope, "corp", "", 50)
            .await
            .unwrap()
            .0[0]
            .session_expires_at
            .is_none()
    );
    sso.revoke_session(&owner, &s.scope, "writer", true)
        .await
        .unwrap();
    assert!(
        sso.identities(&owner, &s.scope, "corp", "", 50)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert_eq!(
        sso.revoke_session(&owner, &s.scope, "writer", true)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );

    // Turning enforcement off lifts the requirement.
    sso.update_config(
        &owner,
        &s.scope,
        SsoPatch {
            enforced: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(admit(&w, &["reader"]).await.is_ok());
    assert!(sso.sweep(&owner).await.is_ok());
    let actions = support::audit_actions(&w, &format!("org:{}", s.org.id)).await;
    for a in [
        "enterprise.sso_update",
        "enterprise.sso_test_sign_in",
        "enterprise.sso_sign_in",
        "enterprise.sso_session_revoke",
    ] {
        assert!(actions.iter().any(|x| x == a), "{a} missing in {actions:?}");
    }
}
on_both_backends!(sso_configuration_test_sign_in_and_enforcement);

fn scim_user(name: &str) -> Value {
    json!({"schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"], "userName": name, "emails": [{"value": name, "primary": true}]})
}

async fn scim(w: &World, token: &str, method: &str, path: &str, body: Value) -> (u16, Value) {
    let r = w
        .ent
        .scim
        .handle(ScimRequest {
            method: method.into(),
            path: format!("/scim/v2/organizations/acme{path}"),
            body: if body.is_null() {
                Vec::new()
            } else {
                serde_json::to_vec(&body).unwrap()
            },
            authorization: format!("Bearer {token}"),
            ..Default::default()
        })
        .await;
    (r.status, r.body)
}

async fn sso_links_scim_identities_syncs_groups_and_ends_with_deprovisioning(w: World) {
    let s = setup(&w).await;
    let sso = &w.ent.sso;
    let owner = cx("owner");
    sso.update_config(
        &owner,
        &s.scope,
        SsoPatch {
            idp_metadata_xml: Some(signer::metadata()),
            enabled: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    w.ent
        .scim
        .update_config(&owner, &s.scope, Some(true), None)
        .await
        .unwrap();
    let (token, _, _) = w
        .ent
        .scim
        .create_token(&owner, &s.scope, "idp", 0)
        .await
        .unwrap();
    let (st, u) = scim(&w, &token, "POST", "/Users", scim_user("ann@corp.example")).await;
    assert_eq!(st, 201);
    let uid = u["id"].as_str().unwrap().to_string();
    let (ids, _) = w
        .ent
        .scim
        .list_identities(&owner, &s.scope, "", true, "", 10)
        .await
        .unwrap();
    let ann = ids[0].principal.as_ref().unwrap().id.clone();

    // A team linked to the SAML group "core".
    w.ent
        .teams
        .create(
            &owner,
            &s.org,
            NewTeam {
                slug: "core".into(),
                name: "core".into(),
                maintainers: vec!["owner".into()],
                ..NewTeam::default()
            },
        )
        .await
        .unwrap();
    w.ent
        .teams
        .link_group(&owner, &s.org, "core", "saml", "core", "Core")
        .await
        .unwrap();
    let viewer = Viewer {
        ids: vec!["owner".into()],
        org_owner: true,
    };

    // The SCIM identity is found by NameID; the groups the IdP sends drive the team.
    let mut r = signer::Reply::for_org("acme", "ann@corp.example", w.clock_ms(), None);
    r.groups = vec!["core".into(), "everyone".into()];
    let out = sso
        .consume_response(&owner, &s.scope, &signer::sign(&r), "")
        .await
        .unwrap();
    assert_eq!(out.principal_id, ann);
    assert!(out.linked);
    assert_eq!(out.groups, vec!["core", "everyone"]);
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "core", &ann)
            .await
            .is_ok(),
        "joined the team through the group"
    );
    assert_eq!(
        w.ent
            .teams
            .get(&owner, &s.org, "core", &viewer)
            .await
            .unwrap()
            .member_count,
        2,
        "maintainer plus the synced member"
    );
    // Dropping the group from the next assertion removes her again.
    let mut r = signer::Reply::for_org("acme", "ann@corp.example", w.clock_ms(), None);
    r.groups = vec!["everyone".into()];
    sso.consume_response(&owner, &s.scope, &signer::sign(&r), "")
        .await
        .unwrap();
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "core", &ann)
            .await
            .is_err()
    );
    let mut r = signer::Reply::for_org("acme", "ann@corp.example", w.clock_ms(), None);
    r.groups = vec!["core".into()];
    sso.consume_response(&owner, &s.scope, &signer::sign(&r), "")
        .await
        .unwrap();
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "core", &ann)
            .await
            .is_ok()
    );
    assert!(
        sso.identities(&owner, &s.scope, "ann", "", 10)
            .await
            .unwrap()
            .0[0]
            .scim_provisioned
    );

    // Deprovisioning through SCIM removes the SSO identity and session too.
    let off = json!({"schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": [{"op": "replace", "path": "active", "value": false}]});
    assert_eq!(
        scim(&w, &token, "PATCH", &format!("/Users/{uid}"), off)
            .await
            .0,
        200
    );
    assert!(
        sso.identities(&owner, &s.scope, "ann", "", 10)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert!(
        w.ent
            .teams
            .membership(&owner, &s.org, "core", &ann)
            .await
            .is_err()
    );
    assert!(admit(&w, &[&ann]).await.is_err());
    // A deprovisioned identity cannot sign in again.
    let again = signer::Reply::for_org("acme", "ann@corp.example", w.clock_ms(), None);
    assert!(
        sso.consume_response(&owner, &s.scope, &signer::sign(&again), "")
            .await
            .is_err()
    );
}
on_both_backends!(sso_links_scim_identities_syncs_groups_and_ends_with_deprovisioning);
