//! `SecurityService` called in-process through its tonic trait with tokens of different
//! principals and scopes: the permission matrix, denial audit and archived repos.

use crate::common;
use crate::security_support;

use bee_policy::Action;
use bee_proto::v1::security_service_server::SecurityService;
use bee_proto::v1::*;
use common::{reason, req};
use security_support::*;

fn outcome<T: std::fmt::Debug>(r: Result<tonic::Response<T>, tonic::Status>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(_) => reason(r),
    }
}

#[tokio::test]
async fn permission_matrix_over_rpc() {
    let w = world().await;
    w.sec
        .set_bypass_policy(
            &w.cx("owner"),
            &w.repo,
            "review",
            vec!["reviewer".into()],
            0,
        )
        .await
        .unwrap();
    let readonly = w.scoped("reader", &[Action::Read]);
    let tokens: Vec<(&str, String)> = vec![
        ("owner", w.full("owner")),
        ("maintainer", w.full("maintainer")),
        ("writer", w.full("writer")),
        ("reader", w.full("reader")),
        ("secmgr", w.full("secmgr")),
        ("outsider", w.full("outsider")),
        ("reviewer", w.full("reviewer")),
        ("readonly-token", readonly),
    ];
    let r = w.rref();
    let s = &w.svc;

    macro_rules! row {
        ($name:expr, $call:ident, $msg:expr, [$(($who:expr, $want:expr)),*]) => {
            $(
                let t = &tokens.iter().find(|(n, _)| *n == $who).unwrap().1;
                let got = outcome(s.$call(req(t, $msg)).await);
                assert_eq!(got, $want, "{} as {}", $name, $who);
            )*
        };
    }

    row!(
        "ListDependencies",
        list_dependencies,
        ListDependenciesRequest {
            repo: r.clone(),
            ..Default::default()
        },
        [
            ("reader", "ok"),
            ("readonly-token", "ok"),
            ("outsider", "NOT_FOUND"),
            ("secmgr", "ok")
        ]
    );
    row!(
        "ListDependencyAlerts",
        list_dependency_alerts,
        ListDependencyAlertsRequest {
            repo: r.clone(),
            ..Default::default()
        },
        [
            ("reader", "PERMISSION_DENIED"),
            ("readonly-token", "POLICY_DENIED"),
            ("writer", "ok"),
            ("secmgr", "ok"),
            ("outsider", "NOT_FOUND")
        ]
    );
    row!(
        "SubmitDependencySnapshot",
        submit_dependency_snapshot,
        SubmitDependencySnapshotRequest {
            repo: r.clone(),
            detector_name: "d".into(),
            job_correlator: "j".into(),
            ..Default::default()
        },
        [
            ("reader", "PERMISSION_DENIED"),
            ("writer", "ok"),
            ("secmgr", "ok")
        ]
    );
    row!(
        "ListSecretScanningAlerts",
        list_secret_scanning_alerts,
        ListSecretScanningAlertsRequest {
            repo: r.clone(),
            ..Default::default()
        },
        [
            ("writer", "PERMISSION_DENIED"),
            ("maintainer", "ok"),
            ("secmgr", "ok"),
            ("owner", "ok")
        ]
    );
    row!(
        "CreateSecretScanningPattern(repo)",
        create_secret_scanning_pattern,
        CreateSecretScanningPatternRequest {
            scope: Some(create_secret_scanning_pattern_request::Scope::Repo(
                r.clone().unwrap()
            )),
            name: "p1".into(),
            regex: "p1-[0-9]+".into(),
            published: true,
            ..Default::default()
        },
        [("maintainer", "PERMISSION_DENIED"), ("owner", "ok")]
    );
    row!(
        "CreateSecretScanningPattern(org)",
        create_secret_scanning_pattern,
        CreateSecretScanningPatternRequest {
            scope: Some(create_secret_scanning_pattern_request::Scope::Org(
                w.org.name.clone()
            )),
            name: "o1".into(),
            regex: "o1-[0-9]+".into(),
            published: true,
            ..Default::default()
        },
        [
            ("maintainer", "PERMISSION_DENIED"),
            ("outsider", "PERMISSION_DENIED"),
            ("secmgr", "ok")
        ]
    );
    row!(
        "GetSecurityOverview",
        get_security_overview,
        GetSecurityOverviewRequest {
            org: w.org.name.clone(),
            ..Default::default()
        },
        [
            ("owner", "ok"),
            ("secmgr", "ok"),
            ("maintainer", "PERMISSION_DENIED"),
            ("outsider", "PERMISSION_DENIED")
        ]
    );
    row!(
        "UpdateSecurityConfiguration",
        update_security_configuration,
        UpdateSecurityConfigurationRequest {
            repo: r.clone(),
            code_quality: Some(true),
            ..Default::default()
        },
        [
            ("maintainer", "PERMISSION_DENIED"),
            ("writer", "PERMISSION_DENIED"),
            ("owner", "ok")
        ]
    );
    row!(
        "GetSecurityConfiguration",
        get_security_configuration,
        GetSecurityConfigurationRequest { repo: r.clone() },
        [("reader", "ok"), ("outsider", "NOT_FOUND")]
    );
    row!(
        "ListAdvisories",
        list_advisories,
        ListAdvisoriesRequest::default(),
        [("outsider", "ok"), ("reader", "ok")]
    );
    row!(
        "CreateSecurityAdvisory",
        create_security_advisory,
        CreateSecurityAdvisoryRequest {
            repo: r.clone(),
            summary: "s".into(),
            ..Default::default()
        },
        [
            ("writer", "PERMISSION_DENIED"),
            ("maintainer", "ok"),
            ("secmgr", "ok")
        ]
    );
    row!(
        "SubmitVulnerabilityReport(private)",
        submit_vulnerability_report,
        SubmitVulnerabilityReportRequest {
            repo: r.clone(),
            summary: "s".into(),
            ..Default::default()
        },
        [("outsider", "NOT_FOUND")]
    );
    row!(
        "SubmitVulnerabilityReport(public, off)",
        submit_vulnerability_report,
        SubmitVulnerabilityReportRequest {
            repo: w.pub_ref(),
            summary: "s".into(),
            ..Default::default()
        },
        [("outsider", "INVALID_STATE")]
    );

    // Bypass review: admin, security manager or a listed reviewer; never the requester.
    let fs = vec![(
        "a.env".to_string(),
        b"k = AKIAIOSFODNN7EXAMPLE\n".to_vec(),
        None,
    )];
    for (who, want) in [
        ("reader", "PERMISSION_DENIED"),
        ("writer", "PERMISSION_DENIED"),
        ("maintainer", "PERMISSION_DENIED"),
        ("reviewer", "ok"),
        ("secmgr", "ok"),
        ("owner", "ok"),
    ] {
        let err = w
            .sec
            .protect(&w.cx("agent"), &w.repo, &fs, None)
            .await
            .unwrap_err();
        let bid = match err.payload.as_deref() {
            Some(bee_proto::v1::error_detail::Payload::PushProtection(p)) => {
                p.bypass_request_url.rsplit('/').next().unwrap().to_string()
            }
            _ => panic!(),
        };
        let b = w
            .sec
            .request_bypass(
                &w.cx("agent"),
                &w.repo,
                &RequestPushProtectionBypassRequest {
                    repo: r.clone(),
                    block_id: bid,
                    reason: PushProtectionBypassReason::UsedInTests as i32,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let t = &tokens.iter().find(|(n, _)| *n == who).unwrap().1;
        let got = outcome(
            s.review_push_protection_bypass(req(
                t,
                ReviewPushProtectionBypassRequest {
                    repo: r.clone(),
                    id: b.id.clone(),
                    approve: true,
                    comment: String::new(),
                },
            ))
            .await,
        );
        assert_eq!(got, want, "review as {who}");
        // Clear the pending request so the next iteration can block again.
        if got != "ok" {
            w.sec
                .review_bypass(
                    &w.cx("owner"),
                    &w.repo,
                    &ReviewPushProtectionBypassRequest {
                        repo: r.clone(),
                        id: b.id,
                        approve: false,
                        comment: String::new(),
                    },
                )
                .await
                .unwrap();
        }
    }

    // Unauthenticated calls.
    let bare = tonic::Request::new(ListAdvisoriesRequest::default());
    assert_eq!(outcome(s.list_advisories(bare).await), "UNAUTHENTICATED");

    // Denials of state changes are audited with the reason.
    w.forge.flush().await.unwrap();
    let (entries, _) = w
        .forge
        .audit
        .query(
            &w.cx("owner"),
            &bee_forge::audit::org_scope(&w.org.id),
            &Default::default(),
            false,
            "",
            500,
        )
        .await
        .unwrap();
    for action in [
        "security.config.update",
        "push_protection.bypass_review",
        "security.secret_pattern.create",
    ] {
        assert!(
            entries
                .iter()
                .any(|e| e.action == action && e.result == "PERMISSION_DENIED"),
            "no audited denial for {action}"
        );
    }

    // Archived repos refuse writes.
    w.forge
        .orgs
        .archive_repo(&w.cx("owner"), &w.repo.id)
        .await
        .unwrap();
    row!(
        "UpdateSecurityConfiguration(archived)",
        update_security_configuration,
        UpdateSecurityConfigurationRequest {
            repo: r.clone(),
            code_quality: Some(false),
            ..Default::default()
        },
        [("owner", "ARCHIVED")]
    );
}

#[tokio::test]
async fn rpc_flow_for_uploads_alerts_and_errors_with_request_ids() {
    let w = world().await;
    let c1 = w.push(&[("a.js", Some("a\n"))], "code").await;
    let t = w.full("ci");
    let doc = serde_json::json!({"version":"2.1.0","runs":[{"tool":{"driver":{"name":"Lint","rules":[{"id":"r1"}]}},
        "results":[{"ruleId":"r1","level":"warning","message":{"text":"m"},
        "locations":[{"physicalLocation":{"artifactLocation":{"uri":"a.js"},"region":{"startLine":1}}}]}]}]});
    let up = w
        .svc
        .upload_sarif(req(
            &t,
            UploadSarifRequest {
                repo: w.rref(),
                changeset: Some(c1),
                sarif: Some(upload_sarif_request::Sarif::SarifGzip(gzip(
                    doc.to_string().as_bytes(),
                ))),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(up.status, CodeScanningSarifStatus::Complete as i32);
    let alerts = w
        .svc
        .list_code_scanning_alerts(req(
            &w.full("writer"),
            ListCodeScanningAlertsRequest {
                repo: w.rref(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .alerts;
    assert_eq!(alerts.len(), 1);
    let st = w
        .svc
        .get_code_scanning_alert(req(
            &w.full("writer"),
            GetCodeScanningAlertRequest {
                repo: w.rref(),
                number: 99,
            },
        ))
        .await
        .unwrap_err();
    let d = bee_proto::errors::detail_from_status(&st).unwrap();
    assert_eq!(
        (d.reason.as_str(), d.request_id.as_str()),
        ("NOT_FOUND", "test-request")
    );
    // The module's server registers with a tonic router.
    let mut routes = tonic::service::RoutesBuilder::default();
    w.svc.register(&mut routes);
}
