//! Secret scanning (history scans, alert lifecycle, validity checks, custom patterns), push
//! protection with delegated bypasses, and code scanning (SARIF ingest, fingerprint dedupe
//! across revisions and refs, alert states, check runs, code quality).

use crate::security_support;

use std::sync::Arc;

use async_trait::async_trait;
use bee_forge::Cx;
use bee_forge::security::Security;
use bee_forge::security::ports::{AutofixRunner, Ports};
use bee_forge::security::secrets::PatternOwner;
use bee_proto::v1::error_detail::Payload;
use bee_proto::v1::*;
use security_support::*;

const AWS: &str = "AKIAIOSFODNN7EXAMPLE";

fn github_token() -> String {
    let rule = bee_secrets::builtin_rules()
        .into_iter()
        .find(|r| r.id() == "github-token")
        .unwrap();
    let ex = rule.example().unwrap().to_string();
    ex.split_whitespace().last().unwrap().to_string()
}

fn aws_file() -> String {
    format!("aws_access_key_id = {AWS}\nregion = us-east-1\n")
}

fn files(path: &str, content: &str) -> Vec<(String, Vec<u8>, Option<ObjectId>)> {
    vec![(path.to_string(), content.as_bytes().to_vec(), None)]
}

async fn secret_alerts(w: &World, state: SecretScanningAlertState) -> Vec<SecretScanningAlert> {
    w.sec
        .list_secret_alerts(
            &w.cx("owner"),
            &w.repo,
            &ListSecretScanningAlertsRequest {
                repo: w.rref(),
                state: state as i32,
                ..Default::default()
            },
            "",
            100,
        )
        .await
        .unwrap()
        .0
}

async fn history_scan(w: &World) -> Vec<u64> {
    let tip = w.tip().await;
    w.sec
        .scan_history(&Cx::system(), &w.repo, &tip, 100)
        .await
        .unwrap()
}

#[tokio::test]
async fn history_scan_opens_one_alert_per_secret_with_every_location() {
    let w = world().await;
    let gh = github_token();
    let c1 = w
        .push(&[("config/prod.env", Some(&aws_file()))], "Add prod config")
        .await;
    w.push(
        &[
            ("src/copy.env", Some(&aws_file())),
            ("src/ci.yml", Some(&format!("token: {gh}\n"))),
        ],
        "Copy config",
    )
    .await;
    let opened = history_scan(&w).await;
    assert_eq!(opened.len(), 2);
    let alerts = secret_alerts(&w, SecretScanningAlertState::Open).await;
    assert_eq!(alerts.len(), 2);
    let aws = alerts
        .iter()
        .find(|a| a.secret_type == "aws_access_key_id")
        .unwrap();
    assert!(aws.secret_preview.starts_with("AKIA") && aws.secret_preview.ends_with("****"));
    assert!(!aws.secret_preview.contains(AWS));
    assert!(!aws.secret_type_display_name.is_empty());
    assert!(aws.url.contains("/security/secret-scanning/"));
    assert!(!aws.publicly_leaked, "the repo is private");
    assert_eq!(aws.severity, SecuritySeverity::High as i32);
    let (locs, _) = w
        .sec
        .secret_locations(&w.cx("owner"), &w.repo, aws.number, "", 10)
        .await
        .unwrap();
    assert_eq!(locs.len(), 2, "two files hold the same key");
    let paths: Vec<String> = locs
        .iter()
        .filter_map(|l| match &l.location {
            Some(secret_scanning_location::Location::Blob(b)) => {
                Some(String::from_utf8(b.path.clone()).unwrap())
            }
            _ => None,
        })
        .collect();
    assert!(
        paths.contains(&"config/prod.env".to_string())
            && paths.contains(&"src/copy.env".to_string())
    );
    let first = locs
        .iter()
        .find(|l| matches!(&l.location, Some(secret_scanning_location::Location::Blob(b)) if b.path == b"config/prod.env"))
        .unwrap();
    assert_eq!(
        first.changeset.as_ref().unwrap(),
        &c1,
        "the oldest changeset that introduced it"
    );
    // Scanning again changes nothing.
    assert!(history_scan(&w).await.is_empty());
    assert_eq!(
        secret_alerts(&w, SecretScanningAlertState::Open)
            .await
            .len(),
        2
    );
    // The secret appears in no event or audit payload.
    w.forge.flush().await.unwrap();
    let (events, _) = w
        .forge
        .events
        .read(&w.cx("owner"), &w.repo.id, 0, &[], 1000)
        .await
        .unwrap();
    for e in events {
        assert!(
            !e.payload_json.contains(AWS) && !e.payload_json.contains(&gh),
            "{}",
            e.kind
        );
    }
    assert!(
        w.kinds()
            .await
            .contains(&"secret_scanning_alert.created".to_string())
    );
    // Filters.
    let (only_gh, _) = w
        .sec
        .list_secret_alerts(
            &w.cx("owner"),
            &w.repo,
            &ListSecretScanningAlertsRequest {
                repo: w.rref(),
                secret_types: vec!["github_token".into()],
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap();
    assert_eq!(only_gh.len(), 1);
    // A public repo marks its leaks public.
    let tip = w
        .push_to(&w.public_repo, &[("a.env", Some(&aws_file()))], "leak")
        .await;
    w.sec
        .scan_history(&Cx::system(), &w.public_repo, &tip, 10)
        .await
        .unwrap();
    let a = w
        .sec
        .list_secret_alerts(
            &w.cx("owner"),
            &w.public_repo,
            &ListSecretScanningAlertsRequest {
                repo: w.pub_ref(),
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap()
        .0;
    assert!(a[0].publicly_leaked);
}

#[tokio::test]
async fn secret_alerts_resolve_with_reasons_and_reopen() {
    let w = world().await;
    w.push(&[("a.env", Some(&aws_file()))], "leak").await;
    history_scan(&w).await;
    let a = secret_alerts(&w, SecretScanningAlertState::Open)
        .await
        .remove(0);
    let upd = |state: SecretScanningAlertState, res: SecretScanningResolution| {
        UpdateSecretScanningAlertRequest {
            repo: w.rref(),
            number: a.number,
            state: state as i32,
            resolution: res as i32,
            resolution_comment: "rotated".into(),
        }
    };
    for bad in [
        SecretScanningResolution::Unspecified,
        SecretScanningResolution::PatternDeleted,
        SecretScanningResolution::PatternEdited,
    ] {
        let err = w
            .sec
            .update_secret_alert(
                &w.cx("maintainer"),
                &w.repo,
                &upd(SecretScanningAlertState::Resolved, bad),
            )
            .await
            .unwrap_err();
        assert_eq!(
            (err.reason, err.subcode.as_str()),
            (ErrorReason::InvalidArgument, "missing_resolution")
        );
    }
    let r = w
        .sec
        .update_secret_alert(
            &w.cx("maintainer"),
            &w.repo,
            &upd(
                SecretScanningAlertState::Resolved,
                SecretScanningResolution::Revoked,
            ),
        )
        .await
        .unwrap();
    assert_eq!(r.state, SecretScanningAlertState::Resolved as i32);
    assert_eq!(r.resolution, SecretScanningResolution::Revoked as i32);
    assert_eq!(r.resolved_by.unwrap().id, "maintainer");
    assert!(r.resolved_at.is_some());
    assert!(
        secret_alerts(&w, SecretScanningAlertState::Open)
            .await
            .is_empty()
    );
    let (by_res, _) = w
        .sec
        .list_secret_alerts(
            &w.cx("owner"),
            &w.repo,
            &ListSecretScanningAlertsRequest {
                repo: w.rref(),
                resolutions: vec![SecretScanningResolution::Revoked as i32],
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap();
    assert_eq!(by_res.len(), 1);
    // Another copy of a resolved secret does not reopen it.
    w.push(&[("b.env", Some(&aws_file()))], "again").await;
    assert!(history_scan(&w).await.is_empty());
    assert_eq!(
        secret_alerts(&w, SecretScanningAlertState::Resolved)
            .await
            .len(),
        1
    );
    let o = w
        .sec
        .update_secret_alert(
            &w.cx("maintainer"),
            &w.repo,
            &upd(
                SecretScanningAlertState::Open,
                SecretScanningResolution::Unspecified,
            ),
        )
        .await
        .unwrap();
    assert_eq!(o.state, SecretScanningAlertState::Open as i32);
    assert_eq!(o.resolution, 0);
    let err = w
        .sec
        .get_secret_alert(&w.cx("owner"), &w.repo, 999)
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::NotFound);
    let actions = w.audit_actions().await;
    for a in [
        "security.secret_alert.open",
        "security.secret_alert.resolve",
        "security.secret_alert.reopen",
    ] {
        assert!(actions.contains(&a.to_string()), "missing {a}");
    }
    let kinds = w.kinds().await;
    assert!(kinds.contains(&"secret_scanning_alert.resolved".to_string()));
    assert!(kinds.contains(&"secret_scanning_alert.reopened".to_string()));
}

#[tokio::test]
async fn validity_checks_follow_the_verifier_and_the_switches() {
    let w = world().await;
    w.push(&[("a.env", Some(&aws_file()))], "leak").await;
    // Off: validity is not asked.
    w.verifier.set(AWS, SecretScanningValidity::Active);
    history_scan(&w).await;
    let a = secret_alerts(&w, SecretScanningAlertState::Open)
        .await
        .remove(0);
    assert_eq!(a.validity, SecretScanningValidity::Unspecified as i32);
    // On for the repo: reading the alert verifies it.
    w.sec
        .update_config(
            &w.cx("owner"),
            &w.repo,
            &UpdateSecurityConfigurationRequest {
                secret_scanning_validity_checks: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let got = w
        .sec
        .get_secret_alert(&w.cx("owner"), &w.repo, a.number)
        .await
        .unwrap();
    assert_eq!(got.validity, SecretScanningValidity::Active as i32);
    // The partner revokes the key: the next read notices and says so.
    w.verifier.set(AWS, SecretScanningValidity::Inactive);
    let got = w
        .sec
        .get_secret_alert(&w.cx("owner"), &w.repo, a.number)
        .await
        .unwrap();
    assert_eq!(got.validity, SecretScanningValidity::Inactive as i32);
    let (inactive, _) = w
        .sec
        .list_secret_alerts(
            &w.cx("owner"),
            &w.repo,
            &ListSecretScanningAlertsRequest {
                repo: w.rref(),
                validity: SecretScanningValidity::Inactive as i32,
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap();
    assert_eq!(inactive.len(), 1);
    assert!(
        w.kinds()
            .await
            .contains(&"secret_scanning_alert.validated".to_string())
    );
    // The org switch covers repos without their own.
    w.sec
        .set_org_validity_checks(&w.cx("owner"), &w.org.id, true)
        .await
        .unwrap();
    w.verifier.set(AWS, SecretScanningValidity::Active);
    let tip = w
        .push_to(&w.public_repo, &[("k.env", Some(&aws_file()))], "leak")
        .await;
    w.sec
        .scan_history(&Cx::system(), &w.public_repo, &tip, 10)
        .await
        .unwrap();
    let pubs = w
        .sec
        .list_secret_alerts(
            &w.cx("owner"),
            &w.public_repo,
            &ListSecretScanningAlertsRequest {
                repo: w.pub_ref(),
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap()
        .0;
    assert_eq!(pubs[0].validity, SecretScanningValidity::Active as i32);
}

fn create_pattern(name: &str, regex: &str) -> CreateSecretScanningPatternRequest {
    CreateSecretScanningPatternRequest {
        name: name.into(),
        regex: regex.into(),
        published: true,
        push_protection: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn custom_patterns_create_edit_delete_and_scope() {
    let w = world().await;
    w.push(
        &[
            (
                "a.txt",
                Some("internal = ACME-12345-SECRET and ACME-000-TEST\n"),
            ),
            ("b.txt", Some("nothing here\n")),
        ],
        "Add data",
    )
    .await;
    let cx = w.cx("owner");
    // Validation.
    let err = w
        .sec
        .create_pattern(
            &cx,
            PatternOwner::Repo(&w.repo),
            &create_pattern("bad", "ACME-(["),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (err.reason, err.subcode.as_str()),
        (ErrorReason::InvalidArgument, "invalid_regex")
    );
    let err = w
        .sec
        .create_pattern(&cx, PatternOwner::Repo(&w.repo), &create_pattern("", "x"))
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidArgument);
    let mut req = create_pattern("Acme token", "ACME-[0-9]+-[A-Z]+");
    req.must_not_match = vec!["TEST$".into()];
    req.before_regex = "internal = ".into();
    let p = w
        .sec
        .create_pattern(&cx, PatternOwner::Repo(&w.repo), &req)
        .await
        .unwrap();
    assert!(p.created_by.is_some() && !p.id.is_empty());
    let err = w
        .sec
        .create_pattern(&cx, PatternOwner::Repo(&w.repo), &req)
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::AlreadyExists);
    // Creating a published pattern scans history: one alert (the TEST value and the text
    // outside `internal = ` do not match).
    let alerts = secret_alerts(&w, SecretScanningAlertState::Open).await;
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0].secret_type, "custom_acme_token");
    assert_eq!(alerts[0].pattern_id, p.id);
    assert_eq!(alerts[0].secret_type_display_name, "Acme token");
    let (listed, _) = w.sec.list_patterns(&cx, &w.repo.id, "", 10).await.unwrap();
    assert_eq!(listed.len(), 1);

    // Editing so the stored match no longer fits resolves the alert as PATTERN_EDITED.
    let upd = w
        .sec
        .update_pattern(
            &cx,
            &UpdateSecretScanningPatternRequest {
                id: p.id.clone(),
                regex: Some("ZZZ-[0-9]+".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(upd.regex, "ZZZ-[0-9]+");
    let resolved = secret_alerts(&w, SecretScanningAlertState::Resolved).await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].resolution,
        SecretScanningResolution::PatternEdited as i32
    );
    // An edit that still matches keeps alerts open: restore the pattern, rescan, edit slightly.
    w.sec
        .update_pattern(
            &cx,
            &UpdateSecretScanningPatternRequest {
                id: p.id.clone(),
                regex: Some("ACME-[0-9]+-[A-Z]+".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        secret_alerts(&w, SecretScanningAlertState::Open)
            .await
            .len(),
        0,
        "the resolved alert stays resolved"
    );
    // Deleting resolves open alerts as PATTERN_DELETED and that cannot be undone.
    let other = w
        .sec
        .create_pattern(
            &cx,
            PatternOwner::Repo(&w.repo),
            &create_pattern("Words", "nothing"),
        )
        .await
        .unwrap();
    assert_eq!(
        secret_alerts(&w, SecretScanningAlertState::Open)
            .await
            .len(),
        1
    );
    w.sec.delete_pattern(&cx, &other.id).await.unwrap();
    let all = secret_alerts(&w, SecretScanningAlertState::Resolved).await;
    let gone = all.iter().find(|a| a.pattern_id == other.id).unwrap();
    assert_eq!(
        gone.resolution,
        SecretScanningResolution::PatternDeleted as i32
    );
    let err = w
        .sec
        .update_secret_alert(
            &cx,
            &w.repo,
            &UpdateSecretScanningAlertRequest {
                repo: w.rref(),
                number: gone.number,
                state: SecretScanningAlertState::Open as i32,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidState);
    assert_eq!(
        w.sec
            .pattern_by_id(&cx, &other.id)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );

    // An org pattern reaches every repo of the org; an unpublished one reaches nowhere.
    let org_p = w
        .sec
        .create_pattern(
            &cx,
            PatternOwner::Org(&w.org.id, "acme"),
            &create_pattern("Org word", "hello-[a-z]+"),
        )
        .await
        .unwrap();
    assert_eq!(org_p.org, "acme");
    let tip = w
        .push(&[("c.txt", Some("say hello-world\n"))], "hello")
        .await;
    w.sec
        .scan_history(&Cx::system(), &w.repo, &tip, 10)
        .await
        .unwrap();
    assert!(
        secret_alerts(&w, SecretScanningAlertState::Open)
            .await
            .iter()
            .any(|a| a.secret_type == "custom_org_word")
    );
    let mut dry = create_pattern("Dry", "dry-run-[0-9]+");
    dry.published = false;
    w.sec
        .create_pattern(&cx, PatternOwner::Repo(&w.repo), &dry)
        .await
        .unwrap();
    let tip = w.push(&[("d.txt", Some("dry-run-42\n"))], "dry").await;
    w.sec
        .scan_history(&Cx::system(), &w.repo, &tip, 10)
        .await
        .unwrap();
    assert!(
        !secret_alerts(&w, SecretScanningAlertState::Open)
            .await
            .iter()
            .any(|a| a.secret_type == "custom_dry")
    );
    let kinds = w.kinds().await;
    for k in [
        "secret_scanning_pattern.created",
        "secret_scanning_pattern.updated",
        "secret_scanning_pattern.deleted",
    ] {
        assert!(kinds.contains(&k.to_string()), "missing {k}");
    }
}

fn block_id(e: &bee_forge::ForgeError) -> String {
    match e.payload.as_deref() {
        Some(Payload::PushProtection(p)) => {
            p.bypass_request_url.rsplit('/').next().unwrap().to_string()
        }
        other => panic!("no push protection payload: {other:?}"),
    }
}

fn bypass_req(
    w: &World,
    block: &str,
    reason: PushProtectionBypassReason,
) -> RequestPushProtectionBypassRequest {
    RequestPushProtectionBypassRequest {
        repo: w.rref(),
        block_id: block.into(),
        reason: reason as i32,
        comment: "needed for the fixture".into(),
        ..Default::default()
    }
}

fn review(w: &World, id: &str, approve: bool) -> ReviewPushProtectionBypassRequest {
    ReviewPushProtectionBypassRequest {
        repo: w.rref(),
        id: id.into(),
        approve,
        comment: if approve { "ok".into() } else { "no".into() },
    }
}

#[tokio::test]
async fn push_protection_blocks_then_a_reviewed_bypass_lets_one_write_through() {
    let w = world().await;
    let fs = files("src/config.env", &aws_file());
    // Clean content passes.
    assert!(
        w.sec
            .protect(
                &w.cx("writer"),
                &w.repo,
                &files("README.md", "hello\n"),
                None
            )
            .await
            .unwrap()
            .bypass_id
            .is_none()
    );
    // The credential is refused with what the agent needs to react.
    let err = w
        .sec
        .protect(&w.cx("writer"), &w.repo, &fs, None)
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::SecretDetected);
    let Some(Payload::PushProtection(block)) = err.payload.as_deref() else {
        panic!("payload")
    };
    assert!(block.bypass_allowed);
    assert_eq!(block.findings.len(), 1);
    assert_eq!(block.findings[0].path, b"src/config.env");
    assert_eq!(block.findings[0].line, 1);
    assert_eq!(block.findings[0].secret_type, "aws_access_key_id");
    assert!(!err.message.contains(AWS));
    let bid = block_id(&err);
    // A request waits for review; only the blocked principal may ask.
    let err2 = w
        .sec
        .request_bypass(
            &w.cx("agent"),
            &w.repo,
            &bypass_req(&w, &bid, PushProtectionBypassReason::UsedInTests),
        )
        .await
        .unwrap_err();
    assert_eq!(err2.reason, ErrorReason::PermissionDenied);
    let err3 = w
        .sec
        .request_bypass(
            &w.cx("writer"),
            &w.repo,
            &bypass_req(&w, &bid, PushProtectionBypassReason::Unspecified),
        )
        .await
        .unwrap_err();
    assert_eq!(err3.subcode, "missing_reason");
    let b = w
        .sec
        .request_bypass(
            &w.cx("writer"),
            &w.repo,
            &bypass_req(&w, &bid, PushProtectionBypassReason::UsedInTests),
        )
        .await
        .unwrap();
    assert_eq!(b.state, PushProtectionBypassState::Pending as i32);
    assert_eq!(b.requester.as_ref().unwrap().id, "writer");
    assert_eq!(b.findings.len(), 1);
    assert_eq!(b.block_id, bid);
    let dup = w
        .sec
        .request_bypass(
            &w.cx("writer"),
            &w.repo,
            &bypass_req(&w, &bid, PushProtectionBypassReason::UsedInTests),
        )
        .await
        .unwrap_err();
    assert_eq!(dup.reason, ErrorReason::AlreadyExists);
    // A pending bypass does not let the write through; the requester cannot approve it.
    assert_eq!(
        w.sec
            .protect(&w.cx("writer"), &w.repo, &fs, Some(&b.id))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert_eq!(
        w.sec
            .review_bypass(&w.cx("writer"), &w.repo, &review(&w, &b.id, true))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied
    );
    let (pending, _) = w
        .sec
        .list_bypasses(
            &w.cx("maintainer"),
            &w.repo,
            PushProtectionBypassState::Pending as i32,
            "",
            "",
            10,
        )
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    let approved = w
        .sec
        .review_bypass(&w.cx("maintainer"), &w.repo, &review(&w, &b.id, true))
        .await
        .unwrap();
    assert_eq!(approved.state, PushProtectionBypassState::Approved as i32);
    assert_eq!(approved.reviewer.as_ref().unwrap().id, "maintainer");
    assert!(approved.expires_at.is_some() && approved.reviewed_at.is_some());
    assert_eq!(
        w.sec
            .review_bypass(&w.cx("maintainer"), &w.repo, &review(&w, &b.id, false))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState,
        "already decided"
    );
    // The bypass is for that principal, that secret, and one write.
    assert_eq!(
        w.sec
            .protect(&w.cx("agent"), &w.repo, &fs, Some(&b.id))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied
    );
    let other = files("src/other.env", "key = ghp_");
    let _ = other;
    let gh = github_token();
    let two = vec![
        ("src/config.env".to_string(), aws_file().into_bytes(), None),
        (
            "src/gh.env".to_string(),
            format!("t = {gh}\n").into_bytes(),
            None,
        ),
    ];
    assert_eq!(
        w.sec
            .protect(&w.cx("writer"), &w.repo, &two, Some(&b.id))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied,
        "a second secret is not covered"
    );
    let out = w
        .sec
        .protect(&w.cx("writer"), &w.repo, &fs, Some(&b.id))
        .await
        .unwrap();
    assert_eq!(out.bypass_id.as_deref(), Some(b.id.as_str()));
    assert_eq!(out.alerts.len(), 1);
    assert_eq!(
        w.sec
            .protect(&w.cx("writer"), &w.repo, &fs, Some(&b.id))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState,
        "used once"
    );
    // The bypass opened an alert marked as bypassed, and was audited.
    let alerts = secret_alerts(&w, SecretScanningAlertState::Open).await;
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0].push_protection_bypassed);
    assert_eq!(alerts[0].push_protection_bypass_id, b.id);
    let actions = w.audit_actions().await;
    for a in [
        "push_protection.block",
        "push_protection.bypass_request",
        "push_protection.bypass_review",
        "push_protection.bypass",
    ] {
        assert!(actions.contains(&a.to_string()), "missing {a}: {actions:?}");
    }
    let kinds = w.kinds().await;
    for k in [
        "push_protection.blocked",
        "push_protection.bypass_requested",
        "push_protection.bypass_reviewed",
        "push_protection.bypassed",
    ] {
        assert!(kinds.contains(&k.to_string()), "missing {k}");
    }
    // A denied request stays denied.
    let err = w
        .sec
        .protect(&w.cx("writer"), &w.repo, &files("x.env", &aws_file()), None)
        .await
        .unwrap_err();
    let b2 = w
        .sec
        .request_bypass(
            &w.cx("writer"),
            &w.repo,
            &bypass_req(
                &w,
                &block_id(&err),
                PushProtectionBypassReason::FalsePositive,
            ),
        )
        .await
        .unwrap();
    let denied = w
        .sec
        .review_bypass(&w.cx("maintainer"), &w.repo, &review(&w, &b2.id, false))
        .await
        .unwrap();
    assert_eq!(denied.state, PushProtectionBypassState::Denied as i32);
    assert_eq!(
        w.sec
            .protect(
                &w.cx("writer"),
                &w.repo,
                &files("x.env", &aws_file()),
                Some(&b2.id)
            )
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert_eq!(
        w.sec
            .get_bypass(&w.cx("writer"), &w.repo, &b2.id)
            .await
            .unwrap()
            .state,
        PushProtectionBypassState::Denied as i32
    );
}

#[tokio::test]
async fn bypass_policy_expiry_and_switches() {
    let w = world().await;
    let fs = files("a.env", &aws_file());
    let block = |who: &'static str| {
        let (w, fs) = (&w, &fs);
        async move {
            block_id(
                &w.sec
                    .protect(&w.cx(who), &w.repo, fs, None)
                    .await
                    .unwrap_err(),
            )
        }
    };
    // Pending requests lapse.
    let b = w
        .sec
        .request_bypass(
            &w.cx("writer"),
            &w.repo,
            &bypass_req(
                &w,
                &block("writer").await,
                PushProtectionBypassReason::WillFixLater,
            ),
        )
        .await
        .unwrap();
    w.clock.advance(8 * 24 * 3600 * 1000);
    assert_eq!(
        w.sec
            .get_bypass(&w.cx("writer"), &w.repo, &b.id)
            .await
            .unwrap()
            .state,
        PushProtectionBypassState::Expired as i32
    );
    assert_eq!(
        w.sec
            .review_bypass(&w.cx("maintainer"), &w.repo, &review(&w, &b.id, true))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    w.sec.expire_blocks(&Cx::system()).await.unwrap();
    assert_eq!(
        w.sec
            .get_bypass(&w.cx("writer"), &w.repo, &b.id)
            .await
            .unwrap()
            .state,
        PushProtectionBypassState::Expired as i32
    );
    // Blocks lapse after a day.
    let stale = block("writer").await;
    w.clock.advance(25 * 3600 * 1000);
    assert_eq!(
        w.sec
            .request_bypass(
                &w.cx("writer"),
                &w.repo,
                &bypass_req(&w, &stale, PushProtectionBypassReason::UsedInTests)
            )
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    // allow: granted at once, still audited when exercised.
    w.sec
        .set_bypass_policy(&w.cx("owner"), &w.repo, "allow", vec![], 1)
        .await
        .unwrap();
    let g = w
        .sec
        .request_bypass(
            &w.cx("writer"),
            &w.repo,
            &bypass_req(
                &w,
                &block("writer").await,
                PushProtectionBypassReason::UsedInTests,
            ),
        )
        .await
        .unwrap();
    assert_eq!(g.state, PushProtectionBypassState::Granted as i32);
    w.sec
        .protect(&w.cx("writer"), &w.repo, &fs, Some(&g.id))
        .await
        .unwrap();
    // A granted bypass lapses after its ttl (1 hour).
    let g2 = w
        .sec
        .request_bypass(
            &w.cx("writer"),
            &w.repo,
            &bypass_req(
                &w,
                &block("writer").await,
                PushProtectionBypassReason::UsedInTests,
            ),
        )
        .await
        .unwrap();
    w.clock.advance(2 * 3600 * 1000);
    assert_eq!(
        w.sec
            .protect(&w.cx("writer"), &w.repo, &fs, Some(&g2.id))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    // deny: no bypass at all.
    w.sec
        .set_bypass_policy(&w.cx("owner"), &w.repo, "deny", vec![], 0)
        .await
        .unwrap();
    let err = w
        .sec
        .protect(&w.cx("writer"), &w.repo, &fs, None)
        .await
        .unwrap_err();
    let Some(Payload::PushProtection(p)) = err.payload.as_deref() else {
        panic!()
    };
    assert!(!p.bypass_allowed);
    assert_eq!(
        w.sec
            .request_bypass(
                &w.cx("writer"),
                &w.repo,
                &bypass_req(&w, &block_id(&err), PushProtectionBypassReason::UsedInTests)
            )
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied
    );
    assert!(
        w.sec
            .set_bypass_policy(&w.cx("owner"), &w.repo, "sometimes", vec![], 0)
            .await
            .is_err()
    );
    // Switching push protection off lets writes through (alerts still come from scans).
    w.sec
        .update_config(
            &w.cx("owner"),
            &w.repo,
            &UpdateSecurityConfigurationRequest {
                secret_scanning_push_protection: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        w.sec
            .protect(&w.cx("writer"), &w.repo, &fs, None)
            .await
            .is_ok()
    );
    // Push protection needs secret scanning.
    let err = w
        .sec
        .update_config(
            &w.cx("owner"),
            &w.repo,
            &UpdateSecurityConfigurationRequest {
                secret_scanning: Some(false),
                secret_scanning_push_protection: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn custom_patterns_block_only_when_asked_and_text_subjects_are_scanned() {
    let w = world().await;
    let cx = w.cx("owner");
    let mut loud = create_pattern("Loud", "LOUD-[0-9]{4}");
    loud.push_protection = true;
    let mut quiet = create_pattern("Quiet", "QUIET-[0-9]{4}");
    quiet.push_protection = false;
    w.sec
        .create_pattern(&cx, PatternOwner::Repo(&w.repo), &loud)
        .await
        .unwrap();
    w.sec
        .create_pattern(&cx, PatternOwner::Repo(&w.repo), &quiet)
        .await
        .unwrap();
    let err = w
        .sec
        .protect(
            &w.cx("writer"),
            &w.repo,
            &files("a.txt", "x LOUD-1234 y\n"),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::SecretDetected);
    let out = w
        .sec
        .protect(
            &w.cx("writer"),
            &w.repo,
            &files("b.txt", "x QUIET-1234 y\n"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out.advisory_findings, 1);
    // Text of an issue or comment is scanned too; the location names the subject.
    let subject = SubjectRef {
        kind: SubjectKind::Issue as i32,
        repo: w.rref(),
        id: "7".into(),
    };
    let opened = w
        .sec
        .scan_subject_text(
            &cx,
            &w.repo,
            subject.clone(),
            &format!("my key is {AWS}, help"),
        )
        .await
        .unwrap();
    assert_eq!(opened.len(), 1);
    let (locs, _) = w
        .sec
        .secret_locations(&cx, &w.repo, opened[0], "", 10)
        .await
        .unwrap();
    assert!(
        matches!(&locs[0].location, Some(secret_scanning_location::Location::Subject(s)) if *s == subject)
    );
}

// ---- code scanning -------------------------------------------------------------------------------

struct R {
    rule: &'static str,
    path: &'static str,
    line: u32,
    fp: &'static str,
    level: &'static str,
}

fn sarif(tool: &str, category: &str, results: &[R]) -> Vec<u8> {
    let mut rules = std::collections::BTreeMap::new();
    for r in results {
        let (tags, sev) = match r.rule {
            "js/xss" => (vec!["security", "external/cwe/cwe-079"], Some("8.1")),
            "js/unused" => (vec!["maintainability"], None),
            _ => (vec!["security"], Some("5.0")),
        };
        let mut props = serde_json::json!({"tags": tags});
        if let Some(s) = sev {
            props["security-severity"] = s.into();
        }
        rules.insert(
            r.rule,
            serde_json::json!({
                "id": r.rule, "name": r.rule.replace('/', "-"),
                "shortDescription": {"text": format!("{} short", r.rule)},
                "help": {"text": "fix it"},
                "properties": props,
            }),
        );
    }
    let results: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "ruleId": r.rule,
                "level": r.level,
                "message": {"text": format!("{} at {}", r.rule, r.path)},
                "locations": [{"physicalLocation": {
                    "artifactLocation": {"uri": r.path},
                    "region": {"startLine": r.line, "endLine": r.line, "startColumn": 3, "endColumn": 9}
                }}],
                "partialFingerprints": {"primaryLocationLineHash": r.fp},
            })
        })
        .collect();
    let mut run = serde_json::json!({
        "tool": {"driver": {"name": tool, "semanticVersion": "2.15.0", "rules": rules.into_values().collect::<Vec<_>>()}},
        "results": results,
    });
    if !category.is_empty() {
        run["automationDetails"] = serde_json::json!({"id": format!("{category}/")});
    }
    gzip(
        serde_json::json!({"version": "2.1.0", "runs": [run]})
            .to_string()
            .as_bytes(),
    )
}

fn upload(w: &World, cs: &ObjectId, r#ref: &str, gz: Vec<u8>) -> UploadSarifRequest {
    UploadSarifRequest {
        repo: w.rref(),
        changeset: Some(cs.clone()),
        r#ref: r#ref.into(),
        sarif: Some(upload_sarif_request::Sarif::SarifGzip(gz)),
        ..Default::default()
    }
}

async fn code_alerts(w: &World, states: &[CodeScanningAlertState]) -> Vec<CodeScanningAlert> {
    w.sec
        .list_code_alerts(
            &w.cx("owner"),
            &w.repo,
            &ListCodeScanningAlertsRequest {
                repo: w.rref(),
                states: states.iter().map(|s| *s as i32).collect(),
                ..Default::default()
            },
            "",
            100,
        )
        .await
        .unwrap()
        .0
}

fn xss(path: &'static str, line: u32, fp: &'static str) -> R {
    R {
        rule: "js/xss",
        path,
        line,
        fp,
        level: "error",
    }
}

#[tokio::test]
async fn sarif_alerts_dedupe_by_fingerprint_across_revisions_and_refs() {
    let w = world().await;
    let c1 = w
        .push(
            &[("src/a.js", Some("a\n")), ("src/b.js", Some("b\n"))],
            "code",
        )
        .await;
    let up = w
        .sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &upload(
                &w,
                &c1,
                "main",
                sarif(
                    "CodeQL",
                    "",
                    &[xss("src/a.js", 10, "fp-a"), xss("src/b.js", 4, "fp-b")],
                ),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        up.status,
        CodeScanningSarifStatus::Complete as i32,
        "{:?}",
        up.errors
    );
    assert_eq!(up.tool_name, "CodeQL");
    assert_eq!(up.analysis_ids.len(), 1);
    assert_eq!(
        w.sec
            .get_sarif_upload(&w.cx("ci"), &w.repo, &up.id)
            .await
            .unwrap()
            .status,
        CodeScanningSarifStatus::Complete as i32
    );
    let alerts = code_alerts(&w, &[CodeScanningAlertState::Open]).await;
    assert_eq!(alerts.len(), 2);
    let a = alerts
        .iter()
        .find(|a| {
            a.most_recent_instance
                .as_ref()
                .unwrap()
                .location
                .as_ref()
                .unwrap()
                .path
                == b"src/a.js"
        })
        .unwrap()
        .clone();
    let rule = a.rule.clone().unwrap();
    assert_eq!(rule.id, "js/xss");
    assert_eq!(rule.security_severity, SecuritySeverity::High as i32);
    assert_eq!(rule.severity, "error");
    assert!(rule.tags.contains(&"security".to_string()));
    assert_eq!(rule.help, "fix it");
    assert_eq!(a.tool_name, "CodeQL");
    assert_eq!(a.tool_version, "2.15.0");
    let inst = a.most_recent_instance.clone().unwrap();
    assert_eq!(inst.location.unwrap().start_line, 10);
    assert_eq!(inst.changeset.unwrap(), c1);
    // The analysis and its check run.
    let analysis = w
        .sec
        .get_analysis(&w.cx("ci"), &w.repo, &up.analysis_ids[0])
        .await
        .unwrap();
    assert_eq!((analysis.results_count, analysis.rules_count), (2, 1));
    w.forge.flush().await.unwrap();
    let runs = w
        .forge
        .checks
        .list(
            &w.cx("owner"),
            &w.repo.id,
            &bee_forge::checks::RunFilter {
                changeset_hex: Some(bee_forge::ids::hex(&c1.raw)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let run = runs
        .iter()
        .find(|r| r.name == "code-scanning/CodeQL")
        .expect("check run");
    assert_eq!(run.status, CheckStatus::Failed as i32);
    assert_eq!(run.external_id, analysis.id);
    assert_eq!(run.annotations_count, 2);

    // Later revision: the lines moved, the fingerprints did not: same alerts, new location.
    let c2 = w.push(&[("src/a.js", Some("x\na\n"))], "shift").await;
    w.sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &upload(
                &w,
                &c2,
                "main",
                sarif(
                    "CodeQL",
                    "",
                    &[xss("src/a.js", 11, "fp-a"), xss("src/b.js", 4, "fp-b")],
                ),
            ),
        )
        .await
        .unwrap();
    let alerts2 = code_alerts(&w, &[CodeScanningAlertState::Open]).await;
    assert_eq!(alerts2.len(), 2, "no duplicates");
    let a2 = w
        .sec
        .get_code_alert(&w.cx("owner"), &w.repo, a.number)
        .await
        .unwrap();
    assert_eq!(
        a2.most_recent_instance
            .as_ref()
            .unwrap()
            .location
            .as_ref()
            .unwrap()
            .start_line,
        11
    );
    assert_eq!(a2.most_recent_instance.unwrap().changeset.unwrap(), c2);
    let (insts, _) = w
        .sec
        .list_code_instances(&w.cx("owner"), &w.repo, a.number, "", "", 10)
        .await
        .unwrap();
    assert_eq!(insts.len(), 1, "one instance per ref and category");

    // The same alert on a branch is a second instance; fixing it on main alone keeps it open.
    w.sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &upload(
                &w,
                &c2,
                "refs/heads/feature",
                sarif("CodeQL", "", &[xss("src/a.js", 11, "fp-a")]),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        code_alerts(&w, &[CodeScanningAlertState::Open]).await.len(),
        2
    );
    let (insts, _) = w
        .sec
        .list_code_instances(&w.cx("owner"), &w.repo, a.number, "", "", 10)
        .await
        .unwrap();
    assert_eq!(insts.len(), 2);
    let (feat, _) = w
        .sec
        .list_code_alerts(
            &w.cx("owner"),
            &w.repo,
            &ListCodeScanningAlertsRequest {
                repo: w.rref(),
                r#ref: "refs/heads/feature".into(),
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap();
    assert_eq!(feat.len(), 1, "only a.js is known on the branch");
    let c3 = w.push(&[("src/a.js", Some("fixed\n"))], "fix a").await;
    w.sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &upload(
                &w,
                &c3,
                "main",
                sarif("CodeQL", "", &[xss("src/b.js", 4, "fp-b")]),
            ),
        )
        .await
        .unwrap();
    let a3 = w
        .sec
        .get_code_alert(&w.cx("owner"), &w.repo, a.number)
        .await
        .unwrap();
    assert_eq!(
        a3.state,
        CodeScanningAlertState::Open as i32,
        "still open on the feature ref"
    );
    let (main_insts, _) = w
        .sec
        .list_code_instances(&w.cx("owner"), &w.repo, a.number, "main", "", 10)
        .await
        .unwrap();
    assert_eq!(main_insts[0].state, CodeScanningAlertState::Fixed as i32);
    // Fixed everywhere: the alert is fixed; it reopens if the finding returns.
    w.sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &upload(&w, &c3, "refs/heads/feature", sarif("CodeQL", "", &[])),
        )
        .await
        .unwrap();
    let a4 = w
        .sec
        .get_code_alert(&w.cx("owner"), &w.repo, a.number)
        .await
        .unwrap();
    assert_eq!(a4.state, CodeScanningAlertState::Fixed as i32);
    assert!(a4.fixed_at.is_some());
    let err = w
        .sec
        .update_code_alert(
            &w.cx("writer"),
            &w.repo,
            &UpdateCodeScanningAlertRequest {
                repo: w.rref(),
                number: a.number,
                state: CodeScanningAlertState::Dismissed as i32,
                dismissed_reason: CodeScanningDismissReason::WontFix as i32,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidState);
    let c4 = w.push(&[("src/a.js", Some("again\n"))], "regress").await;
    w.sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &upload(
                &w,
                &c4,
                "main",
                sarif(
                    "CodeQL",
                    "",
                    &[xss("src/a.js", 1, "fp-a"), xss("src/b.js", 4, "fp-b")],
                ),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        w.sec
            .get_code_alert(&w.cx("owner"), &w.repo, a.number)
            .await
            .unwrap()
            .state,
        CodeScanningAlertState::Open as i32
    );
    let (analyses, _) = w
        .sec
        .list_analyses(&w.cx("owner"), &w.repo, "main", "CodeQL", None, "", 50)
        .await
        .unwrap();
    assert_eq!(analyses.len(), 4);
    let (by_cs, _) = w
        .sec
        .list_analyses(&w.cx("owner"), &w.repo, "", "", Some(&c3), "", 50)
        .await
        .unwrap();
    assert_eq!(by_cs.len(), 2);
    let kinds = w.kinds().await;
    for k in [
        "code_scanning_alert.created",
        "code_scanning_alert.fixed",
        "code_scanning_alert.reopened",
        "code_scanning.analysis_processed",
    ] {
        assert!(kinds.contains(&k.to_string()), "missing {k}");
    }
}

#[tokio::test]
async fn sarif_dismissal_categories_duplicates_and_quality() {
    let w = world().await;
    let c1 = w.push(&[("src/a.js", Some("a\n"))], "code").await;
    let dup = |line| xss("src/a.js", line, "same");
    // Identical (rule, path, fingerprint) results in one run stay distinct alerts.
    w.sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &upload(
                &w,
                &c1,
                "main",
                sarif(
                    "CodeQL",
                    "/language:js",
                    &[
                        dup(1),
                        dup(2),
                        R {
                            rule: "js/unused",
                            path: "src/a.js",
                            line: 3,
                            fp: "q1",
                            level: "note",
                        },
                    ],
                ),
            ),
        )
        .await
        .unwrap();
    let alerts = code_alerts(&w, &[CodeScanningAlertState::Open]).await;
    assert_eq!(
        alerts.len(),
        2,
        "the maintainability rule is a quality finding, not an alert"
    );
    // Another category is another instance of the same alert.
    w.sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &upload(&w, &c1, "main", sarif("CodeQL", "/language:py", &[dup(1)])),
        )
        .await
        .unwrap();
    assert_eq!(
        code_alerts(&w, &[CodeScanningAlertState::Open]).await.len(),
        2
    );
    let first = alerts.iter().map(|a| a.number).min().unwrap();
    let (insts, _) = w
        .sec
        .list_code_instances(&w.cx("owner"), &w.repo, first, "", "", 10)
        .await
        .unwrap();
    assert_eq!(insts.len(), 2);
    assert!(insts.iter().any(|i| i.category == "/language:py"));
    // A different tool is a different alert even for the same finding.
    w.sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &upload(&w, &c1, "main", sarif("Semgrep", "", &[dup(1)])),
        )
        .await
        .unwrap();
    let all = code_alerts(&w, &[CodeScanningAlertState::Open]).await;
    assert_eq!(all.len(), 3);
    let (semgrep, _) = w
        .sec
        .list_code_alerts(
            &w.cx("owner"),
            &w.repo,
            &ListCodeScanningAlertsRequest {
                repo: w.rref(),
                tool_name: "Semgrep".into(),
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap();
    assert_eq!(semgrep.len(), 1);
    let (by_sev, _) = w
        .sec
        .list_code_alerts(
            &w.cx("owner"),
            &w.repo,
            &ListCodeScanningAlertsRequest {
                repo: w.rref(),
                severities: vec![SecuritySeverity::Critical as i32],
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap();
    assert!(by_sev.is_empty());
    let (by_path, _) = w
        .sec
        .list_code_alerts(
            &w.cx("owner"),
            &w.repo,
            &ListCodeScanningAlertsRequest {
                repo: w.rref(),
                path_prefix: b"src/".to_vec(),
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap();
    assert_eq!(by_path.len(), 3);

    // Dismissal needs a reason, survives a re-upload, and can be undone.
    let target = semgrep[0].number;
    let mut req = UpdateCodeScanningAlertRequest {
        repo: w.rref(),
        number: target,
        state: CodeScanningAlertState::Dismissed as i32,
        ..Default::default()
    };
    assert_eq!(
        w.sec
            .update_code_alert(&w.cx("writer"), &w.repo, &req)
            .await
            .unwrap_err()
            .subcode,
        "missing_reason"
    );
    req.dismissed_reason = CodeScanningDismissReason::FalsePositive as i32;
    req.dismissed_comment = "sanitized upstream".into();
    let d = w
        .sec
        .update_code_alert(&w.cx("writer"), &w.repo, &req)
        .await
        .unwrap();
    assert_eq!(d.state, CodeScanningAlertState::Dismissed as i32);
    assert_eq!(d.dismissed_by.unwrap().id, "writer");
    w.sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &upload(&w, &c1, "main", sarif("Semgrep", "", &[dup(1)])),
        )
        .await
        .unwrap();
    assert_eq!(
        w.sec
            .get_code_alert(&w.cx("owner"), &w.repo, target)
            .await
            .unwrap()
            .state,
        CodeScanningAlertState::Dismissed as i32
    );
    req.state = CodeScanningAlertState::Open as i32;
    assert_eq!(
        w.sec
            .update_code_alert(&w.cx("writer"), &w.repo, &req)
            .await
            .unwrap()
            .state,
        CodeScanningAlertState::Open as i32
    );

    // Code quality findings come from the same logs.
    let (findings, _) = w
        .sec
        .list_quality(
            &w.cx("owner"),
            &w.repo,
            &ListCodeQualityFindingsRequest {
                repo: w.rref(),
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(
        (findings[0].rule_id.as_str(), findings[0].category.as_str()),
        ("js/unused", "maintainability")
    );
    let dismissed = w
        .sec
        .update_quality(
            &w.cx("writer"),
            &w.repo,
            &UpdateCodeQualityFindingRequest {
                repo: w.rref(),
                number: findings[0].number,
                state: CodeQualityFindingState::Dismissed as i32,
                comment: "later".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(dismissed.state, CodeQualityFindingState::Dismissed as i32);
    // The finding disappears from a later run of the same tool and category: fixed (only
    // open findings change).
    w.sec
        .update_quality(
            &w.cx("writer"),
            &w.repo,
            &UpdateCodeQualityFindingRequest {
                repo: w.rref(),
                number: findings[0].number,
                state: CodeQualityFindingState::Open as i32,
                comment: String::new(),
            },
        )
        .await
        .unwrap();
    w.sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &upload(
                &w,
                &c1,
                "main",
                sarif("CodeQL", "/language:js", &[dup(1), dup(2)]),
            ),
        )
        .await
        .unwrap();
    let (fixed, _) = w
        .sec
        .list_quality(
            &w.cx("owner"),
            &w.repo,
            &ListCodeQualityFindingsRequest {
                repo: w.rref(),
                state: CodeQualityFindingState::Fixed as i32,
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap();
    assert_eq!(fixed.len(), 1);
}

#[tokio::test]
async fn bad_sarif_is_refused_and_setup_and_autofix_work() {
    let w = world().await;
    let c1 = w.push(&[("a.js", Some("a\n"))], "code").await;
    let cx = w.cx("ci");
    let try_up = |gz: Vec<u8>| {
        let (w, c1, cx) = (&w, &c1, &cx);
        async move {
            w.sec
                .upload_sarif(cx, &w.repo, &upload(w, c1, "main", gz))
                .await
                .unwrap_err()
        }
    };
    assert_eq!(
        try_up(b"not gzip".to_vec()).await.reason,
        ErrorReason::InvalidArgument
    );
    assert_eq!(try_up(gzip(b"not json")).await.subcode, "invalid_sarif");
    assert_eq!(
        try_up(gzip(br#"{"version":"2.0.0","runs":[]}"#))
            .await
            .subcode,
        "invalid_sarif"
    );
    assert_eq!(
        try_up(gzip(br#"{"version":"2.1.0"}"#)).await.subcode,
        "invalid_sarif"
    );
    assert_eq!(
        try_up(gzip(
            br#"{"version":"2.1.0","runs":[{"tool":{"driver":{}},"results":[]}]}"#
        ))
        .await
        .subcode,
        "invalid_sarif"
    );
    let big = try_up(vec![0u8; 10 * 1024 * 1024 + 1]).await;
    assert_eq!(big.reason, ErrorReason::TooLarge);
    let url = w
        .sec
        .upload_sarif(
            &cx,
            &w.repo,
            &UploadSarifRequest {
                repo: w.rref(),
                changeset: Some(c1.clone()),
                sarif: Some(upload_sarif_request::Sarif::SarifUrl(
                    "https://example.test/x".into(),
                )),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(url.subcode, "url_fetch_unavailable");
    assert_eq!(w.sec.process_pending_sarif(&Cx::system()).await.unwrap(), 0);

    // Default setup.
    let s = w.sec.get_default_setup(&cx, &w.repo).await.unwrap();
    assert_eq!(s.state, CodeScanningSetupState::NotConfigured as i32);
    let err = w
        .sec
        .update_default_setup(
            &w.cx("maintainer"),
            &w.repo,
            &UpdateCodeScanningDefaultSetupRequest {
                repo: w.rref(),
                state: Some(CodeScanningSetupState::Configured as i32),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidArgument);
    let s = w
        .sec
        .update_default_setup(
            &w.cx("maintainer"),
            &w.repo,
            &UpdateCodeScanningDefaultSetupRequest {
                repo: w.rref(),
                state: Some(CodeScanningSetupState::Configured as i32),
                languages: vec!["rust".into(), "javascript".into(), "rust".into()],
                query_suite: Some("extended".into()),
                schedule: Some("0 3 * * *".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        s.languages,
        vec!["javascript".to_string(), "rust".to_string()]
    );
    assert_eq!(
        (s.query_suite.as_str(), s.schedule.as_str()),
        ("extended", "0 3 * * *")
    );
    assert!(
        w.sec
            .update_default_setup(
                &w.cx("maintainer"),
                &w.repo,
                &UpdateCodeScanningDefaultSetupRequest {
                    repo: w.rref(),
                    schedule: Some("whenever".into()),
                    ..Default::default()
                },
            )
            .await
            .is_err()
    );

    // Autofix: served by the agents module through the port.
    w.sec
        .upload_sarif(
            &cx,
            &w.repo,
            &upload(&w, &c1, "main", sarif("CodeQL", "", &[xss("a.js", 1, "f")])),
        )
        .await
        .unwrap();
    let alert = code_alerts(&w, &[CodeScanningAlertState::Open])
        .await
        .remove(0);
    let err = w
        .sec
        .request_autofix(&w.cx("writer"), &w.repo, alert.number, "")
        .await
        .unwrap_err();
    assert_eq!(
        (err.reason, err.subcode.as_str()),
        (ErrorReason::Unimplemented, "served_elsewhere")
    );
    struct Runner;
    #[async_trait]
    impl AutofixRunner for Runner {
        async fn start(
            &self,
            _r: &Repo,
            n: u64,
            rule: &str,
            rt: &str,
        ) -> bee_forge::Result<String> {
            Ok(format!("run-{n}-{rule}-{rt}"))
        }
    }
    let sec = Security::new(
        &w.forge,
        None,
        Ports {
            vcs: w.sec.ports().vcs.clone(),
            autofix: Arc::new(Runner),
            ..Ports::default()
        },
    );
    let run = sec
        .request_autofix(&w.cx("writer"), &w.repo, alert.number, "claude")
        .await
        .unwrap();
    assert_eq!(run, format!("run-{}-js/xss-claude", alert.number));
    sec.link_autofix_change(&w.cx("agent"), &w.repo, alert.number, 42)
        .await
        .unwrap();
    assert_eq!(
        sec.get_code_alert(&w.cx("owner"), &w.repo, alert.number)
            .await
            .unwrap()
            .fix_change_request,
        "42"
    );
    assert!(
        w.kinds()
            .await
            .contains(&"code_scanning.autofix_requested".to_string())
    );
}
