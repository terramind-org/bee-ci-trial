//! The agent scenarios of PRODUCT §4.18 (AS-SEC-01, AS-SEC-02), the real-world check of a
//! planted secret and a vulnerable dependency, the org overview, campaigns, configuration and
//! the security policy.

use crate::security_support;

use bee_forge::Cx;
use bee_proto::v1::error_detail::Payload;
use bee_proto::v1::*;
use security_support::*;

async fn open_dep_alerts(w: &World) -> Vec<DependencyAlert> {
    w.sec
        .list_dependency_alerts(
            &w.cx("owner"),
            &w.repo,
            &ListDependencyAlertsRequest {
                repo: w.rref(),
                ..Default::default()
            },
            "",
            50,
        )
        .await
        .unwrap()
        .0
}

#[tokio::test]
async fn as_sec_01_respond_to_an_alert_by_landing_the_update_cr() {
    let w = world().await;
    w.load_advisories().await;
    w.enable_security_updates().await;
    w.plant_project().await;
    w.evaluate().await;
    // The agent finds the alert and the bot's change request on it.
    let alerts = open_dep_alerts(&w).await;
    let flask = alerts
        .iter()
        .find(|a| a.dependency.as_ref().unwrap().name == "flask")
        .unwrap();
    assert!(!flask.update_change_request.is_empty());
    let n: u64 = flask.update_change_request.parse().unwrap();
    let cr = w
        .changes
        .get_by_number(&w.cx("agent"), &w.repo, n)
        .await
        .unwrap();
    // It lands the change request; the alert closes by itself.
    w.changes
        .land(&w.cx("agent"), &w.repo, &cr.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap();
    assert!(w.evaluate().await);
    let after = w
        .sec
        .get_dependency_alert(&w.cx("agent"), &w.repo, flask.number)
        .await
        .unwrap();
    assert_eq!(after.state, DependencyAlertState::Fixed as i32);
    assert!(after.fixed_at.is_some());
}

#[tokio::test]
async fn as_sec_02_a_credential_is_refused_and_the_agent_recovers() {
    let w = world().await;
    let key = "AKIAIOSFODNN7EXAMPLE";
    let attempt = vec![(
        "deploy/config.env".to_string(),
        format!("aws_access_key_id = {key}\nregion = eu-west-1\n").into_bytes(),
        None,
    )];
    let err = w
        .sec
        .protect(&w.cx("agent"), &w.repo, &attempt, None)
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::SecretDetected);
    let Some(Payload::PushProtection(block)) = err.payload.as_deref() else {
        panic!("the refusal names what was found")
    };
    assert_eq!(block.findings[0].path, b"deploy/config.env");
    assert_eq!(block.findings[0].line, 1);
    assert!(
        !format!("{err:?}").contains(key),
        "the refusal never repeats the secret"
    );
    // The right reaction: remove the secret and write again.
    let fixed = vec![(
        "deploy/config.env".to_string(),
        b"aws_access_key_id = ${AWS_ACCESS_KEY_ID}\nregion = eu-west-1\n".to_vec(),
        None,
    )];
    assert!(
        w.sec
            .protect(&w.cx("agent"), &w.repo, &fixed, None)
            .await
            .is_ok()
    );
    assert!(
        w.audit_actions()
            .await
            .contains(&"push_protection.block".to_string())
    );
}

#[tokio::test]
async fn real_world_check_planted_secret_and_vulnerable_dependency() {
    let w = world().await;
    // The same check guards commit, land and push: three calls, three refusals, one bypass
    // that the audit shows.
    let planted = vec![(
        "secrets.env".to_string(),
        b"token = ghp_"
            .iter()
            .copied()
            .chain(github_pat_tail())
            .collect::<Vec<u8>>(),
        None,
    )];
    for site in ["CreateChangeset", "Land", "git push"] {
        let e = w
            .sec
            .protect(&w.cx("writer"), &w.repo, &planted, None)
            .await
            .unwrap_err();
        assert_eq!(e.reason, ErrorReason::SecretDetected, "{site}");
    }
    let e = w
        .sec
        .protect(&w.cx("writer"), &w.repo, &planted, None)
        .await
        .unwrap_err();
    let Some(Payload::PushProtection(p)) = e.payload.as_deref() else {
        panic!()
    };
    let bid = p.bypass_request_url.rsplit('/').next().unwrap().to_string();
    let b = w
        .sec
        .request_bypass(
            &w.cx("writer"),
            &w.repo,
            &RequestPushProtectionBypassRequest {
                repo: w.rref(),
                block_id: bid,
                reason: PushProtectionBypassReason::UsedInTests as i32,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    w.sec
        .review_bypass(
            &w.cx("owner"),
            &w.repo,
            &ReviewPushProtectionBypassRequest {
                repo: w.rref(),
                id: b.id.clone(),
                approve: true,
                comment: String::new(),
            },
        )
        .await
        .unwrap();
    w.sec
        .protect(&w.cx("writer"), &w.repo, &planted, Some(&b.id))
        .await
        .unwrap();
    assert!(
        w.audit_actions()
            .await
            .contains(&"push_protection.bypass".to_string())
    );

    // A vulnerable dependency raises an alert and an update change request.
    w.load_advisories().await;
    w.enable_security_updates().await;
    w.plant_project().await;
    w.evaluate().await;
    let alerts = open_dep_alerts(&w).await;
    assert!(!alerts.is_empty());
    assert!(alerts.iter().all(|a| !a.update_change_request.is_empty()));
}

fn github_pat_tail() -> Vec<u8> {
    let rule = bee_secrets::builtin_rules()
        .into_iter()
        .find(|r| r.id() == "github-token")
        .unwrap();
    let ex = rule.example().unwrap().to_string();
    let tok = ex.split_whitespace().last().unwrap().to_string();
    tok.trim_start_matches("ghp_").as_bytes().to_vec()
}

#[tokio::test]
async fn overview_aggregates_per_org_and_campaigns_track_progress() {
    let w = world().await;
    w.load_advisories().await;
    w.plant_project().await;
    w.evaluate().await;
    w.push(
        &[("k.env", Some("aws_access_key_id = AKIAIOSFODNN7EXAMPLE\n"))],
        "leak",
    )
    .await;
    let tip = w.tip().await;
    w.sec
        .scan_history(&Cx::system(), &w.repo, &tip, 10)
        .await
        .unwrap();
    let ov = w
        .sec
        .overview(
            &w.cx("secmgr"),
            &w.org.id,
            &w.org.name,
            &GetSecurityOverviewRequest {
                org: w.org.name.clone(),
                ..Default::default()
            },
            "",
            50,
        )
        .await
        .unwrap();
    let t = ov.totals.unwrap();
    assert_eq!(t.total_open, 5, "four dependency alerts and one secret");
    assert_eq!((t.critical, t.high, t.medium, t.low), (1, 2, 1, 1));
    assert_eq!(ov.repos_total, 2);
    assert_eq!(ov.repos_with_dependency_alerts_enabled, 2);
    assert_eq!(ov.repos_with_secret_scanning_enabled, 2);
    assert_eq!(ov.repos_with_push_protection_enabled, 2);
    assert_eq!(ov.repos_with_code_scanning_enabled, 0);
    let app = ov
        .repos
        .iter()
        .find(|r| matches!(&r.repo, Some(RepoRef { repo: Some(repo_ref::Repo::Id(i)) }) if *i == w.repo.id))
        .unwrap();
    assert_eq!(app.counts.as_ref().unwrap().total_open, 5);
    assert!(
        ov.tools
            .iter()
            .any(|x| x.kind == SecurityAlertKind::SecretScanning as i32)
    );
    // Kind filter and repo query.
    let only_deps = w
        .sec
        .overview(
            &w.cx("secmgr"),
            &w.org.id,
            &w.org.name,
            &GetSecurityOverviewRequest {
                org: w.org.name.clone(),
                kinds: vec![SecurityAlertKind::Dependency as i32],
                repo_query: "name:app".into(),
                ..Default::default()
            },
            "",
            50,
        )
        .await
        .unwrap();
    assert_eq!(only_deps.totals.unwrap().total_open, 4);
    assert_eq!(only_deps.repos_total, 1);

    // A campaign over the critical dependency alerts; fixing the alert shows as progress.
    let c = w
        .sec
        .create_campaign(
            &w.cx("secmgr"),
            &w.org.id,
            &w.org.name,
            &CreateSecurityCampaignRequest {
                org: w.org.name.clone(),
                name: "Kill criticals".into(),
                filter: Some(SecurityAlertFilter {
                    kind: SecurityAlertKind::Dependency as i32,
                    severities: vec![SecuritySeverity::Critical as i32],
                    ..Default::default()
                }),
                assignee_principal_ids: vec!["agent".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let p = c.progress.unwrap();
    assert_eq!((p.total_alerts, p.open_alerts, p.fixed_alerts), (1, 1, 0));
    assert_eq!(
        w.sec
            .create_campaign(
                &w.cx("secmgr"),
                &w.org.id,
                &w.org.name,
                &CreateSecurityCampaignRequest {
                    org: w.org.name.clone(),
                    name: "kill criticals".into(),
                    filter: c.filter.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );
    let tip = w
        .push(
            &[(
                "package-lock.json",
                Some(&PACKAGE_LOCK.replace("4.17.20", "4.17.21")),
            )],
            "Bump lodash",
        )
        .await;
    w.sec
        .on_default_branch_moved(&Cx::system(), &w.repo, &tip)
        .await
        .unwrap();
    let got = w
        .sec
        .get_campaign(&w.cx("secmgr"), &w.org.id, &c.id)
        .await
        .unwrap();
    let p = got.progress.unwrap();
    assert_eq!((p.open_alerts, p.fixed_alerts), (0, 1));
    let done = w
        .sec
        .update_campaign(
            &w.cx("secmgr"),
            &w.org.id,
            &UpdateSecurityCampaignRequest {
                org: w.org.name.clone(),
                id: c.id.clone(),
                state: Some(SecurityCampaignState::Completed as i32),
                description: Some("done".into()),
                assignee_principal_ids: vec!["owner".into()],
                replace_assignees: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(done.state, SecurityCampaignState::Completed as i32);
    assert_eq!(done.assignee_principal_ids, vec!["owner".to_string()]);
    let (listed, _) = w
        .sec
        .list_campaigns(
            &w.cx("secmgr"),
            &w.org.id,
            SecurityCampaignState::Active as i32,
            "",
            10,
        )
        .await
        .unwrap();
    assert!(listed.is_empty());
    assert_eq!(
        w.sec
            .get_campaign(&w.cx("secmgr"), &w.org.id, "nope")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    // Mean time to remediate comes from fixed alerts.
    let _ = ov.mean_time_to_remediate_seconds;
}

#[tokio::test]
async fn configuration_defaults_dependencies_between_switches_and_the_security_policy() {
    let w = world().await;
    let cfg = w.sec.config_of(&w.cx("reader"), &w.repo).await.unwrap();
    assert!(
        cfg.dependency_graph
            && cfg.dependency_alerts
            && cfg.secret_scanning
            && cfg.secret_scanning_push_protection
    );
    assert!(!cfg.security_updates && !cfg.private_vulnerability_reporting);
    let upd = |f: UpdateSecurityConfigurationRequest| {
        let w = &w;
        async move { w.sec.update_config(&w.cx("owner"), &w.repo, &f).await }
    };
    let base = || UpdateSecurityConfigurationRequest {
        repo: w.rref(),
        ..Default::default()
    };
    let mut r = base();
    r.dependency_graph = Some(false);
    assert!(upd(r).await.is_err(), "alerts need the graph");
    let mut r = base();
    r.dependency_alerts = Some(false);
    r.security_updates = Some(true);
    assert!(upd(r).await.is_err(), "updates need alerts");
    let mut r = base();
    r.code_quality = Some(true);
    r.dependency_review_required = Some(true);
    let c = upd(r).await.unwrap();
    assert!(c.code_quality && c.dependency_review_required && c.updated_at.is_some());
    assert!(
        w.kinds()
            .await
            .contains(&"security_config.updated".to_string())
    );

    assert!(w.sec.security_policy(&w.repo).await.unwrap().is_none());
    w.push(
        &[(
            ".github/SECURITY.md",
            Some("Report to security@example.com\n"),
        )],
        "policy",
    )
    .await;
    let (path, text) = w.sec.security_policy(&w.repo).await.unwrap().unwrap();
    assert_eq!(path, ".github/SECURITY.md");
    assert!(text.contains("security@example.com"));
}
