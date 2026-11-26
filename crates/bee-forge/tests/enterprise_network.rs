//! IP allow lists, verified domains, required 2FA, enterprise accounts (owners, policies,
//! managed users, overrides) and the `EnterpriseService` permission matrix.

use crate::common;
use crate::enterprise_support as support;

use bee_forge::enterprise::common::ScopeResolver;
use bee_forge::enterprise::network::{AdmissionRequest, IpSettingsPatch};
use bee_forge::enterprise::service::EnterpriseApi;
use bee_proto::v1::enterprise_service_server::EnterpriseService;
use bee_proto::v1::team_service_server::TeamService;
use bee_proto::v1::{
    AddVerifiedDomainRequest, CreateScimTokenRequest, CreateTeamRequest, EnterprisePolicyToggle,
    EnterpriseScope, ErrorReason, GetSamlSsoConfigRequest, GetTwoFactorRequirementRequest,
    ListVerifiedDomainsRequest, Role, UpdateEnterprisePoliciesRequest,
    UpdateIpAllowListSettingsRequest, VerifiedDomainState, enterprise_scope,
};
use support::{World, add_member, cx, make_org};

async fn admit(
    w: &World,
    org: &str,
    who: &[&str],
    ip: &str,
    app: bool,
) -> Result<(), bee_forge::ForgeError> {
    let org = w.env.forge.orgs.get_org(&cx("owner"), org).await.unwrap();
    w.ent
        .admit(
            &cx(who[0]),
            &AdmissionRequest {
                principals: who.iter().map(|s| s.to_string()).collect(),
                org_ids: vec![org.id],
                ip: ip.into(),
                app_token: app,
            },
        )
        .await
}

fn at(ip: &str) -> bee_forge::Cx {
    let mut c = cx("owner");
    c.ip = ip.into();
    c
}

async fn ip_allow_lists_are_enforced_at_admission(w: World) {
    let org = make_org(&w.env, "acme").await;
    make_org(&w.env, "other").await;
    add_member(&w.env, &org, "reader", Role::Member).await;
    let scope = w
        .ent
        .accounts
        .resolve_org(&cx("owner"), "acme")
        .await
        .unwrap();
    let n = &w.ent.network;
    let enable = IpSettingsPatch {
        enabled: Some(true),
        ..Default::default()
    };

    // Nothing enforced: any address.
    assert!(
        admit(&w, "acme", &["reader"], "198.51.100.1", false)
            .await
            .is_ok()
    );
    let e = n
        .create_ip_entry(&cx("owner"), &scope, "not-an-ip", "", true)
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "invalid_cidr");
    let office = n
        .create_ip_entry(&cx("owner"), &scope, "203.0.113.0/24", "office", true)
        .await
        .unwrap();
    n.create_ip_entry(&cx("owner"), &scope, "2001:db8::/32", "v6", true)
        .await
        .unwrap();
    n.create_ip_entry(&cx("owner"), &scope, "192.0.2.0/24", "off", false)
        .await
        .unwrap();

    // Turning it on from an address the list does not allow would lock the caller out.
    let e = n
        .update_ip_settings(&at("198.51.100.7"), &scope, enable)
        .await
        .unwrap_err();
    assert_eq!(
        (e.reason, e.subcode.as_str()),
        (ErrorReason::InvalidArgument, "would_lock_out")
    );
    n.update_ip_settings(&at("203.0.113.50"), &scope, enable)
        .await
        .unwrap();
    assert!(n.ip_settings(&cx("owner"), &scope).await.unwrap().enabled);

    // Admission: allowed, not allowed, v6, inactive entry, unknown address; other orgs unaffected.
    assert!(
        admit(&w, "acme", &["reader"], "203.0.113.200", false)
            .await
            .is_ok()
    );
    assert!(
        admit(&w, "acme", &["reader"], "2001:db8:1::5", false)
            .await
            .is_ok()
    );
    for ip in ["198.51.100.7", "192.0.2.4", "", "garbage"] {
        let e = admit(&w, "acme", &["reader"], ip, false).await.unwrap_err();
        assert_eq!(e.reason, ErrorReason::IpNotAllowed, "{ip:?}");
    }
    assert!(
        admit(&w, "other", &["reader"], "198.51.100.7", false)
            .await
            .is_ok()
    );
    // Entries cannot be removed or deactivated from under the caller.
    let e = n
        .delete_ip_entry(&at("203.0.113.50"), &scope, &office.id)
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "would_lock_out");
    let e = n
        .update_ip_entry(
            &at("203.0.113.50"),
            &scope,
            &office.id,
            None,
            None,
            Some(false),
        )
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "would_lock_out");
    let wider = n
        .update_ip_entry(
            &at("203.0.113.50"),
            &scope,
            &office.id,
            Some("203.0.0.0/16"),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(wider.cidr, "203.0.0.0/16");

    // Installed apps: covered only when the settings say so; their ranges only when included.
    assert!(
        admit(&w, "acme", &["reader"], "198.51.100.7", true)
            .await
            .is_ok()
    );
    n.update_ip_settings(
        &at("203.0.113.50"),
        &scope,
        IpSettingsPatch {
            applies_to_installed_apps: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        admit(&w, "acme", &["reader"], "198.51.100.7", true)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::IpNotAllowed
    );
    n.set_app_ranges(
        &cx("owner"),
        &scope,
        "app1",
        &["198.51.100.0/24".to_string()],
    )
    .await
    .unwrap();
    assert!(
        admit(&w, "acme", &["reader"], "198.51.100.7", true)
            .await
            .is_err(),
        "app ranges are ignored until included"
    );
    n.update_ip_settings(
        &at("203.0.113.50"),
        &scope,
        IpSettingsPatch {
            include_installed_app_ranges: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        admit(&w, "acme", &["reader"], "198.51.100.7", true)
            .await
            .is_ok()
    );
    assert!(
        n.update_ip_entry(
            &cx("owner"),
            &scope,
            &n.ip_entries(&cx("owner"), &scope, "", 50)
                .await
                .unwrap()
                .0
                .iter()
                .find(|e| !e.app_id.is_empty())
                .unwrap()
                .id,
            None,
            None,
            Some(false)
        )
        .await
        .is_err(),
        "app entries are read only"
    );

    // Off again.
    n.update_ip_settings(
        &cx("owner"),
        &scope,
        IpSettingsPatch {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        admit(&w, "acme", &["reader"], "198.51.100.7", false)
            .await
            .is_ok()
    );
    let actions = support::audit_actions(&w, &format!("org:{}", org.id)).await;
    assert!(actions.iter().any(|a| a == "enterprise.ip_settings_update"));
    assert!(actions.iter().any(|a| a == "enterprise.ip_entry_create"));
}
on_both_backends!(ip_allow_lists_are_enforced_at_admission);

async fn domains_verify_through_a_dns_challenge(w: World) {
    let org = make_org(&w.env, "acme").await;
    let other = make_org(&w.env, "other").await;
    let scope = w
        .ent
        .accounts
        .resolve_org(&cx("owner"), "acme")
        .await
        .unwrap();
    let other_scope = w
        .ent
        .accounts
        .resolve_org(&cx("owner"), "other")
        .await
        .unwrap();
    let n = &w.ent.network;
    let owner = cx("owner");
    assert_eq!(
        n.add_domain(&owner, &scope, "localhost", false)
            .await
            .unwrap_err()
            .subcode,
        "invalid_domain"
    );
    let d = n
        .add_domain(&owner, &scope, "Example.com", false)
        .await
        .unwrap();
    assert_eq!(d.domain, "example.com");
    assert_eq!(d.state, VerifiedDomainState::Pending as i32);
    assert_eq!(d.dns_txt_record_name, "_beehive-challenge-acme.example.com");
    assert!(d.dns_txt_record_value.starts_with("beehive-challenge="));
    assert_eq!(
        n.add_domain(&owner, &scope, "example.com", false)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );
    // No record yet: still pending. A wrong value does not verify. The right one does.
    assert_eq!(
        n.check_domain(&owner, &scope, &d.id).await.unwrap().state,
        VerifiedDomainState::Pending as i32
    );
    w.dns
        .publish(&d.dns_txt_record_name, "beehive-challenge=wrong");
    assert_eq!(
        n.check_domain(&owner, &scope, &d.id).await.unwrap().state,
        VerifiedDomainState::Pending as i32
    );
    w.dns.publish(
        &d.dns_txt_record_name,
        &format!("\"{}\"", d.dns_txt_record_value),
    );
    let v = n.check_domain(&owner, &scope, &d.id).await.unwrap();
    assert_eq!(v.state, VerifiedDomainState::Verified as i32);
    assert!(v.verified_at.is_some() && v.last_checked_at.is_some());
    assert!(
        n.is_domain_verified(&owner, &org.id, "EXAMPLE.com")
            .await
            .unwrap()
    );
    assert!(
        !n.is_domain_verified(&owner, &other.id, "example.com")
            .await
            .unwrap()
    );
    // One owner at a time: another scope with the same record cannot claim it.
    let d2 = n
        .add_domain(&owner, &other_scope, "example.com", false)
        .await
        .unwrap();
    w.dns
        .publish(&d2.dns_txt_record_name, &d2.dns_txt_record_value);
    assert_eq!(
        n.check_domain(&owner, &other_scope, &d2.id)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );
    // Challenges expire.
    let d3 = n
        .add_domain(&owner, &scope, "late.example", false)
        .await
        .unwrap();
    w.clock.advance(8 * 86_400_000);
    assert_eq!(
        n.check_domain(&owner, &scope, &d3.id).await.unwrap().state,
        VerifiedDomainState::Failed as i32
    );
    // Approved domains skip the check and only count for notifications.
    let ap = n
        .add_domain(&owner, &other_scope, "partner.example", true)
        .await
        .unwrap();
    assert_eq!(ap.state, VerifiedDomainState::Approved as i32);
    assert!(ap.dns_txt_record_name.is_empty());
    assert_eq!(
        n.check_domain(&owner, &other_scope, &ap.id)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert!(
        !n.is_domain_verified(&owner, &other.id, "partner.example")
            .await
            .unwrap()
    );
    // Restricting notifications needs a verified or approved domain.
    let bare = make_org(&w.env, "bare").await;
    let bare_scope = w.ent.accounts.resolve_org(&owner, "bare").await.unwrap();
    assert_eq!(
        n.update_domain_settings(&owner, &bare_scope, Some(true))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert!(
        n.update_domain_settings(&owner, &scope, Some(true))
            .await
            .unwrap()
            .restrict_notifications
    );
    assert!(
        n.update_domain_settings(&owner, &other_scope, Some(true))
            .await
            .unwrap()
            .restrict_notifications
    );
    let (list, _) = n.domains(&owner, &scope, "", 10).await.unwrap();
    assert_eq!(list.len(), 2);
    n.delete_domain(&owner, &scope, &d3.id).await.unwrap();
    assert_eq!(
        n.delete_domain(&owner, &scope, &d3.id)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    drop(bare);
}
on_both_backends!(domains_verify_through_a_dns_challenge);

async fn two_factor_blocks_or_removes_noncompliant_members(w: World) {
    let org = make_org(&w.env, "acme").await;
    for id in ["admin", "writer", "reader"] {
        add_member(&w.env, &org, id, Role::Member).await;
    }
    let scope = w
        .ent
        .accounts
        .resolve_org(&cx("owner"), "acme")
        .await
        .unwrap();
    let n = &w.ent.network;
    let owner = cx("owner");
    w.twofa.enable("writer", false);
    w.twofa.enable("admin", true);
    // Required: noncompliant members are blocked at admission, others pass.
    let (req, removed) = n
        .update_two_factor(&owner, &scope, Some(true), None, false)
        .await
        .unwrap();
    assert!(req.required && req.required_since.is_some() && removed.is_empty());
    assert_eq!(req.noncompliant_count, 2, "owner and reader");
    assert_eq!(
        admit(&w, "acme", &["reader"], "1.1.1.1", false)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::TwoFactorRequired
    );
    assert!(
        admit(&w, "acme", &["writer"], "1.1.1.1", false)
            .await
            .is_ok()
    );
    assert!(
        admit(&w, "acme", &["outsider"], "1.1.1.1", false)
            .await
            .is_ok()
    );
    // Only secure methods: TOTP/SMS no longer count.
    n.update_two_factor(&owner, &scope, None, Some(true), false)
        .await
        .unwrap();
    assert_eq!(
        admit(&w, "acme", &["writer"], "1.1.1.1", false)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::TwoFactorRequired
    );
    assert!(
        admit(&w, "acme", &["admin"], "1.1.1.1", false)
            .await
            .is_ok()
    );
    let list = n.noncompliant(&owner, &scope, true).await.unwrap();
    assert_eq!(list.len(), 3);
    // Removing noncompliant members: the last owner stays (and stays blocked).
    let (_, removed) = n
        .update_two_factor(&owner, &scope, None, None, true)
        .await
        .unwrap();
    assert_eq!(removed, vec!["reader", "writer"]);
    assert!(
        w.env
            .forge
            .orgs
            .member(&owner, &org.id, "reader")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        w.env
            .forge
            .orgs
            .member(&owner, &org.id, "owner")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        w.env
            .forge
            .orgs
            .member(&owner, &org.id, "admin")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        admit(&w, "acme", &["owner"], "1.1.1.1", false)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::TwoFactorRequired
    );
    // Not required any more.
    n.update_two_factor(&owner, &scope, Some(false), None, false)
        .await
        .unwrap();
    assert!(
        admit(&w, "acme", &["owner"], "1.1.1.1", false)
            .await
            .is_ok()
    );
    assert_eq!(
        n.two_factor(&owner, &scope)
            .await
            .unwrap()
            .noncompliant_count,
        0
    );
}
on_both_backends!(two_factor_blocks_or_removes_noncompliant_members);

fn ent_scope(slug: &str) -> Option<EnterpriseScope> {
    Some(EnterpriseScope {
        scope: Some(enterprise_scope::Scope::Enterprise(slug.into())),
    })
}

fn org_scope(name: &str) -> Option<EnterpriseScope> {
    Some(EnterpriseScope {
        scope: Some(enterprise_scope::Scope::Org(name.into())),
    })
}

async fn enterprise_accounts_policies_overrides_and_managed_users(w: World) {
    let acme = make_org(&w.env, "acme").await;
    let other = make_org(&w.env, "other").await;
    add_member(&w.env, &acme, "reader", Role::Member).await;
    let a = &w.ent.accounts;
    let owner = cx("owner");
    let admin = w.env.forge.hooks.directory.get("admin").await.unwrap();
    let ent = a
        .create_enterprise(
            &owner,
            "globex",
            "Globex",
            w.env.forge.hooks.directory.get("owner").await.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        a.create_enterprise(&owner, "globex", "", admin.clone())
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );
    assert_eq!(
        a.create_enterprise(&owner, "Bad Slug", "", admin.clone())
            .await
            .unwrap_err()
            .subcode,
        "invalid_name"
    );

    // Owners: the last one stays.
    a.add_owner(&owner, "globex", "admin").await.unwrap();
    assert_eq!(
        a.add_owner(&owner, "globex", "admin")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );
    assert!(a.is_owner(&owner, &ent.id, "admin").await.unwrap());
    a.remove_owner(&owner, "globex", "owner").await.unwrap();
    assert_eq!(
        a.remove_owner(&owner, "globex", "admin")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    a.add_owner(&owner, "globex", "owner").await.unwrap();
    // Orgs.
    a.add_org(&owner, "globex", &acme).await.unwrap();
    a.add_org(&owner, "globex", &other).await.unwrap();
    assert_eq!(
        a.add_org(&owner, "globex", &acme).await.unwrap_err().reason,
        ErrorReason::AlreadyExists
    );
    let second = a
        .create_enterprise(&owner, "initech", "", admin.clone())
        .await
        .unwrap();
    assert_eq!(
        a.add_org(&owner, "initech", &acme)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    let got = a.get(&owner, "globex").await.unwrap();
    assert_eq!((got.org_count, got.member_count), (2, 2));
    let (orgs, _) = a.list_orgs(&owner, "globex", "acm", "", 10).await.unwrap();
    assert_eq!(orgs.len(), 1);
    assert_eq!(orgs[0].member_count, 2);
    let rs = a.resolve_org(&owner, "acme").await.unwrap();
    assert_eq!(rs.enterprise_id.as_deref(), Some(ent.id.as_str()));
    assert_eq!(
        a.resolve(&owner, &ent_scope("globex"))
            .await
            .unwrap()
            .org_ids
            .len(),
        2
    );

    // The service: enterprise owners manage the enterprise and, through it, its orgs' security
    // settings; org members and outsiders cannot.
    let api = EnterpriseApi::new(w.ent.clone());
    let tok = |who: &str| w.env.full(who);
    let sso = |who: &str, scope: Option<EnterpriseScope>| {
        let r = common::req(&tok(who), GetSamlSsoConfigRequest { scope });
        let api = api.clone();
        async move { api.get_saml_sso_config(r).await }
    };
    assert!(
        sso("admin", ent_scope("globex")).await.is_ok(),
        "an enterprise owner"
    );
    assert!(
        sso("admin", org_scope("other")).await.is_ok(),
        "an enterprise owner reaches the org's settings"
    );
    assert_eq!(
        common::reason(sso("reader", ent_scope("globex")).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        common::reason(sso("reader", org_scope("acme")).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        common::reason(sso("outsider", org_scope("acme")).await),
        "PERMISSION_DENIED"
    );
    assert!(sso("owner", org_scope("acme")).await.is_ok());
    assert_eq!(
        common::reason(sso("owner", ent_scope("nope")).await),
        "NOT_FOUND"
    );

    // Enterprise policies bind the orgs: members may not create teams when the policy says so.
    let create = |who: &str| {
        let r = common::req(
            &tok(who),
            CreateTeamRequest {
                org: "acme".into(),
                slug: format!("t-{who}"),
                name: "t".into(),
                ..Default::default()
            },
        );
        let api = api.clone();
        async move { api.create_team(r).await }
    };
    assert!(
        create("reader").await.is_ok(),
        "org settings allow it by default"
    );
    let set = |toggle: EnterprisePolicyToggle| {
        let r = common::req(
            &tok("owner"),
            UpdateEnterprisePoliciesRequest {
                enterprise: "globex".into(),
                members_can_create_teams: Some(toggle as i32),
                ..Default::default()
            },
        );
        let api = api.clone();
        async move { api.update_enterprise_policies(r).await }
    };
    set(EnterprisePolicyToggle::Disabled).await.unwrap();
    add_member(&w.env, &acme, "writer", Role::Member).await;
    assert_eq!(common::reason(create("writer").await), "PERMISSION_DENIED");
    assert!(create("owner").await.is_ok(), "owners still can");
    w.env
        .forge
        .orgs
        .update_settings(&owner, &acme.id, 0, |s| s.members_can_create_teams = false)
        .await
        .unwrap();
    set(EnterprisePolicyToggle::Allowed).await.unwrap();
    assert!(
        create("writer").await.is_ok(),
        "the enterprise overrides the org setting"
    );
    let bad = common::req(
        &tok("owner"),
        UpdateEnterprisePoliciesRequest {
            enterprise: "globex".into(),
            repo_creation: Some(99),
            ..Default::default()
        },
    );
    assert_eq!(
        common::reason(api.update_enterprise_policies(bad).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        a.policies(&owner, "globex")
            .await
            .unwrap()
            .members_can_create_teams,
        EnterprisePolicyToggle::Allowed as i32
    );

    // An enterprise list overrides the orgs'.
    let ip = |who: &str, enabled: bool| {
        let mut req = common::req(
            &tok(who),
            UpdateIpAllowListSettingsRequest {
                scope: ent_scope("globex"),
                enabled: Some(enabled),
                ..Default::default()
            },
        );
        req.metadata_mut()
            .insert("x-forwarded-for", "203.0.113.5".parse().unwrap());
        let api = api.clone();
        async move { api.update_ip_allow_list_settings(req).await }
    };
    let escope = a.resolve(&owner, &ent_scope("globex")).await.unwrap();
    w.ent
        .network
        .create_ip_entry(&owner, &escope, "203.0.113.0/24", "hq", true)
        .await
        .unwrap();
    let oscope = a.resolve_org(&owner, "acme").await.unwrap();
    w.ent
        .network
        .create_ip_entry(&owner, &oscope, "198.51.100.0/24", "branch", true)
        .await
        .unwrap();
    w.ent
        .network
        .update_ip_settings(
            &at("198.51.100.1"),
            &oscope,
            IpSettingsPatch {
                enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        admit(&w, "acme", &["reader"], "198.51.100.9", false)
            .await
            .is_ok()
    );
    assert_eq!(
        common::reason(ip("reader", true).await),
        "PERMISSION_DENIED"
    );
    ip("owner", true).await.unwrap();
    assert_eq!(
        admit(&w, "acme", &["reader"], "198.51.100.9", false)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::IpNotAllowed
    );
    assert!(
        admit(&w, "acme", &["reader"], "203.0.113.9", false)
            .await
            .is_ok()
    );
    assert!(
        admit(&w, "other", &["reader"], "203.0.113.9", false)
            .await
            .is_ok()
    );
    assert!(
        admit(&w, "other", &["reader"], "198.51.100.9", false)
            .await
            .is_err()
    );
    // Leaving the enterprise ends its policies.
    a.remove_org(&owner, "globex", &acme).await.unwrap();
    assert!(
        admit(&w, "acme", &["reader"], "198.51.100.9", false)
            .await
            .is_ok()
    );

    // Managed users need every member provisioned through SCIM.
    a.add_org(&owner, "initech", &acme).await.unwrap();
    let e = a
        .update(
            &owner,
            "initech",
            bee_forge::enterprise::accounts::EnterprisePatch {
                managed_users: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        (e.reason, e.subcode.as_str()),
        (ErrorReason::InvalidState, "members_not_provisioned")
    );
    a.remove_org(&owner, "initech", &acme).await.unwrap();
    let m = a
        .update(
            &owner,
            "initech",
            bee_forge::enterprise::accounts::EnterprisePatch {
                managed_users: Some(true),
                managed_users_shortcode: Some("_init".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(m.managed_users);
    // A managed user (SCIM-provisioned at the enterprise) cannot join orgs outside it.
    let escope2 = a.resolve(&owner, &ent_scope("initech")).await.unwrap();
    w.ent
        .scim
        .update_config(&owner, &escope2, Some(true), None)
        .await
        .unwrap();
    let (token, _, _) = w
        .ent
        .scim
        .create_token(&owner, &escope2, "idp", 0)
        .await
        .unwrap();
    let res = w
        .ent
        .scim
        .handle(bee_forge::enterprise::scim::ScimRequest {
            method: "POST".into(),
            path: "/scim/v2/enterprises/initech/Users".into(),
            body: br#"{"userName":"mia"}"#.to_vec(),
            authorization: format!("Bearer {token}"),
            ..Default::default()
        })
        .await;
    assert_eq!(res.status, 201, "{}", res.body);
    let (ids, _) = w
        .ent
        .scim
        .list_identities(&owner, &escope2, "", true, "", 10)
        .await
        .unwrap();
    let mia = ids[0].principal.as_ref().unwrap().id.clone();
    assert!(
        ids[0]
            .principal
            .as_ref()
            .unwrap()
            .display
            .ends_with("_init")
    );
    let e = a.check_can_join(&owner, &acme.id, &mia).await.unwrap_err();
    assert_eq!(e.subcode, "managed_user");
    a.add_org(&owner, "initech", &acme).await.unwrap();
    assert!(a.check_can_join(&owner, &acme.id, &mia).await.is_ok());
    assert!(
        a.check_can_join(&owner, &acme.id, "someone-else")
            .await
            .is_ok()
    );
    let actions = support::audit_actions(&w, &format!("enterprise:{}", ent.id)).await;
    for x in [
        "enterprise.create",
        "enterprise.owner_add",
        "enterprise.org_add",
        "enterprise.policies_update",
    ] {
        assert!(actions.iter().any(|a| a == x), "{x} missing in {actions:?}");
    }
    drop(second);
}
on_both_backends!(enterprise_accounts_policies_overrides_and_managed_users);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enterprise_service_permission_matrix() {
    let w = support::world(support::Backend::Memory).await;
    let org = make_org(&w.env, "acme").await;
    add_member(&w.env, &org, "admin", Role::Admin).await;
    add_member(&w.env, &org, "reader", Role::Member).await;
    let api = EnterpriseApi::new(w.ent.clone());
    let scope = org_scope("acme");
    let req = |who: &str| {
        common::req(
            &w.env.full(who),
            GetTwoFactorRequirementRequest {
                scope: scope.clone(),
            },
        )
    };
    assert!(api.get_two_factor_requirement(req("owner")).await.is_ok());
    assert!(api.get_two_factor_requirement(req("admin")).await.is_ok());
    assert_eq!(
        common::reason(api.get_two_factor_requirement(req("reader")).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        common::reason(api.get_two_factor_requirement(req("outsider")).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        common::reason(
            api.get_two_factor_requirement(tonic::Request::new(GetTwoFactorRequirementRequest {
                scope: scope.clone()
            }))
            .await
        ),
        "UNAUTHENTICATED"
    );
    let ro = common::req(
        &w.env
            .token("owner", common::actions(&[bee_policy::Action::Read])),
        GetTwoFactorRequirementRequest {
            scope: scope.clone(),
        },
    );
    assert_eq!(
        common::reason(api.get_two_factor_requirement(ro).await),
        "POLICY_DENIED"
    );
    // A SCIM token is shown once and works; domains and a missing scope behave.
    let t = api
        .create_scim_token(common::req(
            &w.env.full("admin"),
            CreateScimTokenRequest {
                scope: scope.clone(),
                description: "x".into(),
                ttl_seconds: 0,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(t.token.starts_with("scim_") && !t.token_id.is_empty());
    let d = api
        .add_verified_domain(common::req(
            &w.env.full("owner"),
            AddVerifiedDomainRequest {
                scope: scope.clone(),
                domain: "example.org".into(),
                approved_only: false,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!d.dns_txt_record_value.is_empty());
    let l = api
        .list_verified_domains(common::req(
            &w.env.full("owner"),
            ListVerifiedDomainsRequest {
                scope: scope.clone(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(l.domains.len(), 1);
    let none = common::req(
        &w.env.full("owner"),
        GetTwoFactorRequirementRequest { scope: None },
    );
    assert_eq!(
        common::reason(api.get_two_factor_requirement(none).await),
        "INVALID_ARGUMENT"
    );
    // Denied state changes are audited.
    let denied = api
        .add_verified_domain(common::req(
            &w.env.full("reader"),
            AddVerifiedDomainRequest {
                scope: scope.clone(),
                domain: "evil.example".into(),
                approved_only: true,
            },
        ))
        .await;
    assert_eq!(common::reason(denied), "PERMISSION_DENIED");
    let actions = support::audit_actions(&w, &format!("org:{}", org.id)).await;
    assert!(actions.iter().any(|a| a == "enterprise.scim_token_create"));
    assert!(actions.iter().any(|a| a == "enterprise.domain_add"));
}
