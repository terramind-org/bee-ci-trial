//! Repository security advisories: private vulnerability reports from outsiders, drafts,
//! collaborators and credits, CVE request records, publication to the global database (and
//! the alerts that follows), visibility and the state machine.

use crate::security_support;

use bee_forge::Cx;
use bee_forge::security::Security;
use bee_forge::security::ports::Ports;
use bee_forge::security::repo_advisories::Viewer;
use bee_proto::v1::*;
use security_support::*;

const CRITICAL_VECTOR: &str = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H";

fn viewer(w: &World, who: &str, privileged: bool) -> Viewer {
    Viewer {
        ids: w.cx(who).chain,
        privileged,
    }
}

fn lodash_range() -> AdvisoryAffectedPackage {
    AdvisoryAffectedPackage {
        ecosystem: "npm".into(),
        name: "lodash".into(),
        vulnerable_range: "< 4.17.21".into(),
        first_patched_version: "4.17.21".into(),
        vulnerable_functions: vec![],
    }
}

fn draft(w: &World) -> CreateSecurityAdvisoryRequest {
    CreateSecurityAdvisoryRequest {
        repo: w.pub_ref(),
        summary: "Prototype pollution in lodash".into(),
        description: "A crafted path pollutes Object.prototype.".into(),
        cvss_vector: CRITICAL_VECTOR.into(),
        cwe_ids: vec!["CWE-1321".into()],
        affected: vec![lodash_range()],
        credits: vec![SecurityAdvisoryCredit {
            principal_id: "reader".into(),
            team_id: String::new(),
            role: "finder".into(),
        }],
        ..Default::default()
    }
}

fn update(w: &World, ghsa: &str) -> UpdateSecurityAdvisoryRequest {
    UpdateSecurityAdvisoryRequest {
        repo: w.pub_ref(),
        ghsa_id: ghsa.into(),
        ..Default::default()
    }
}

async fn enable_reporting(w: &World) {
    w.sec
        .update_config(
            &w.cx("owner"),
            &w.public_repo,
            &UpdateSecurityConfigurationRequest {
                private_vulnerability_reporting: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

fn report(w: &World, summary: &str) -> SubmitVulnerabilityReportRequest {
    SubmitVulnerabilityReportRequest {
        repo: w.pub_ref(),
        summary: summary.into(),
        description: "steps to reproduce".into(),
        severity: SecuritySeverity::High as i32,
        cwe_ids: vec!["CWE-79".into()],
        ..Default::default()
    }
}

#[tokio::test]
async fn private_reports_become_drafts_only_maintainers_and_collaborators_see() {
    let w = world().await;
    let err = w
        .sec
        .submit_report(&w.cx("outsider"), &w.public_repo, &report(&w, "XSS"))
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidState, "reporting is off");
    enable_reporting(&w).await;
    let a = w
        .sec
        .submit_report(
            &w.cx("outsider"),
            &w.public_repo,
            &report(&w, "XSS in the editor"),
        )
        .await
        .unwrap();
    assert_eq!(a.state, SecurityAdvisoryState::Triage as i32);
    assert!(
        a.ghsa_id.starts_with("GHSA-") && a.ghsa_id.len() == 19,
        "{}",
        a.ghsa_id
    );
    assert_eq!(a.reporter.as_ref().unwrap().id, "outsider");
    assert!(
        a.credits
            .iter()
            .any(|c| c.principal_id == "outsider" && c.role == "reporter")
    );
    assert!(a.url.contains("/security/advisories/"));

    // The reporter and maintainers see it; nobody else does, not even through a list.
    let reporter = viewer(&w, "outsider", false);
    assert_eq!(
        w.sec
            .get_repo_advisory(&w.cx("outsider"), &w.public_repo, &a.ghsa_id, &reporter)
            .await
            .unwrap()
            .ghsa_id,
        a.ghsa_id
    );
    let stranger = viewer(&w, "reader", false);
    assert_eq!(
        w.sec
            .get_repo_advisory(&w.cx("reader"), &w.public_repo, &a.ghsa_id, &stranger)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    let (seen, _) = w
        .sec
        .list_repo_advisories(&w.cx("reader"), &w.public_repo, 0, &stranger, "", 10)
        .await
        .unwrap();
    assert!(seen.is_empty());
    let maintainer = viewer(&w, "maintainer", true);
    let (seen, _) = w
        .sec
        .list_repo_advisories(&w.cx("maintainer"), &w.public_repo, 0, &maintainer, "", 10)
        .await
        .unwrap();
    assert_eq!(seen.len(), 1);
    let (by_state, _) = w
        .sec
        .list_repo_advisories(
            &w.cx("maintainer"),
            &w.public_repo,
            SecurityAdvisoryState::Draft as i32,
            &maintainer,
            "",
            10,
        )
        .await
        .unwrap();
    assert!(by_state.is_empty());

    // The reporter cannot move it along; a maintainer accepts it.
    let mut req = update(&w, &a.ghsa_id);
    req.state = Some(SecurityAdvisoryState::Draft as i32);
    assert_eq!(
        w.sec
            .update_repo_advisory(&w.cx("outsider"), &w.public_repo, &req, &reporter)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied
    );
    let accepted = w
        .sec
        .update_repo_advisory(&w.cx("maintainer"), &w.public_repo, &req, &maintainer)
        .await
        .unwrap();
    assert_eq!(accepted.state, SecurityAdvisoryState::Draft as i32);
    // A collaborator may edit the draft, a reader may not.
    let collab = AddSecurityAdvisoryCollaboratorRequest {
        repo: w.pub_ref(),
        ghsa_id: a.ghsa_id.clone(),
        collaborator: Some(SecurityAdvisoryCredit {
            principal_id: "writer".into(),
            team_id: String::new(),
            role: String::new(),
        }),
    };
    let with = w
        .sec
        .add_advisory_collaborator(&w.cx("maintainer"), &w.public_repo, &collab, &maintainer)
        .await
        .unwrap();
    assert_eq!(with.collaborators[0].role, "collaborator");
    assert_eq!(
        w.sec
            .add_advisory_collaborator(&w.cx("maintainer"), &w.public_repo, &collab, &maintainer)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );
    let mut edit = update(&w, &a.ghsa_id);
    edit.summary = Some("XSS in the markdown editor".into());
    let writer = viewer(&w, "writer", false);
    let edited = w
        .sec
        .update_repo_advisory(&w.cx("writer"), &w.public_repo, &edit, &writer)
        .await
        .unwrap();
    assert_eq!(edited.summary, "XSS in the markdown editor");
    assert_eq!(
        w.sec
            .update_repo_advisory(&w.cx("reader"), &w.public_repo, &edit, &stranger)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound,
        "a stranger does not even learn it exists"
    );
    // The collaborator now sees it; removing them hides it again.
    assert!(
        w.sec
            .get_repo_advisory(&w.cx("writer"), &w.public_repo, &a.ghsa_id, &writer)
            .await
            .is_ok()
    );
    let rm = RemoveSecurityAdvisoryCollaboratorRequest {
        repo: w.pub_ref(),
        ghsa_id: a.ghsa_id.clone(),
        principal_id: "writer".into(),
        team_id: String::new(),
    };
    w.sec
        .remove_advisory_collaborator(&w.cx("maintainer"), &w.public_repo, &rm, &maintainer)
        .await
        .unwrap();
    assert_eq!(
        w.sec
            .remove_advisory_collaborator(&w.cx("maintainer"), &w.public_repo, &rm, &maintainer)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    // Close and reopen a report; a closed advisory is still private.
    let mut close = update(&w, &a.ghsa_id);
    close.state = Some(SecurityAdvisoryState::Closed as i32);
    assert_eq!(
        w.sec
            .update_repo_advisory(&w.cx("maintainer"), &w.public_repo, &close, &maintainer)
            .await
            .unwrap()
            .state,
        SecurityAdvisoryState::Closed as i32
    );
    assert_eq!(
        w.sec
            .get_repo_advisory(&w.cx("reader"), &w.public_repo, &a.ghsa_id, &stranger)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    close.state = Some(SecurityAdvisoryState::Draft as i32);
    w.sec
        .update_repo_advisory(&w.cx("maintainer"), &w.public_repo, &close, &maintainer)
        .await
        .unwrap();
    // A reporter cannot flood the triage queue.
    for i in 0..10 {
        w.sec
            .submit_report(
                &w.cx("outsider"),
                &w.public_repo,
                &report(&w, &format!("report {i}")),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        w.sec
            .submit_report(&w.cx("outsider"), &w.public_repo, &report(&w, "one more"))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::ResourceExhausted
    );
    let kinds = w.kinds_of(&w.public_repo).await;
    for k in [
        "security_advisory.reported",
        "security_advisory.updated",
        "security_advisory.collaborator_changed",
    ] {
        assert!(kinds.contains(&k.to_string()), "missing {k}: {kinds:?}");
    }
    let actions = w.audit_actions().await;
    for a in [
        "security.advisory.report",
        "security.advisory.update",
        "security.advisory.collaborator_add",
        "security.advisory.collaborator_remove",
    ] {
        assert!(actions.contains(&a.to_string()), "missing {a}");
    }
}

async fn pub_alerts(w: &World) -> Vec<DependencyAlert> {
    w.sec
        .list_dependency_alerts(
            &w.cx("owner"),
            &w.public_repo,
            &ListDependencyAlertsRequest {
                repo: w.pub_ref(),
                ..Default::default()
            },
            "",
            50,
        )
        .await
        .unwrap()
        .0
}

impl World {
    async fn kinds_of(&self, repo: &Repo) -> Vec<String> {
        self.forge.flush().await.unwrap();
        let (events, _) = self
            .forge
            .events
            .read(&self.cx("owner"), &repo.id, 0, &[], 10_000)
            .await
            .unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }
}

#[tokio::test]
async fn drafts_are_validated_and_state_changes_follow_the_machine() {
    let w = world().await;
    let m = w.cx("maintainer");
    let bad = |f: &dyn Fn(&mut CreateSecurityAdvisoryRequest)| {
        let mut d = draft(&w);
        f(&mut d);
        d
    };
    for (name, req) in [
        ("summary", bad(&|d| d.summary.clear())),
        ("cwe", bad(&|d| d.cwe_ids = vec!["79".into()])),
        ("cvss", bad(&|d| d.cvss_vector = "CVSS:9/nonsense".into())),
        (
            "ecosystem",
            bad(&|d| d.affected[0].ecosystem = "cobol".into()),
        ),
        (
            "range",
            bad(&|d| d.affected[0].vulnerable_range = "around 4".into()),
        ),
        ("credit", bad(&|d| d.credits[0].role = "overlord".into())),
        (
            "credit target",
            bad(&|d| d.credits[0].team_id = "t1".into()),
        ),
        (
            "unknown principal",
            bad(&|d| d.credits[0].principal_id = "nobody".into()),
        ),
    ] {
        let err = w
            .sec
            .create_advisory(&m, &w.public_repo, &req)
            .await
            .unwrap_err();
        assert_eq!(err.reason, ErrorReason::InvalidArgument, "{name}: {err}");
    }
    let a = w
        .sec
        .create_advisory(&m, &w.public_repo, &draft(&w))
        .await
        .unwrap();
    assert_eq!(a.state, SecurityAdvisoryState::Draft as i32);
    assert_eq!(
        a.severity,
        SecuritySeverity::Critical as i32,
        "from the CVSS vector"
    );
    assert_eq!(a.author.as_ref().unwrap().id, "maintainer");
    assert_eq!(a.credits[0].role, "finder");
    let v = viewer(&w, "maintainer", true);
    // Private fork: made through the port; unavailable without one.
    let mut with_fork = draft(&w);
    with_fork.start_private_fork = true;
    let f = w
        .sec
        .create_advisory(&m, &w.public_repo, &with_fork)
        .await
        .unwrap();
    assert!(f.private_fork.is_some());
    let bare = Security::new(
        &w.forge,
        None,
        Ports {
            vcs: w.sec.ports().vcs.clone(),
            ..Ports::default()
        },
    );
    let err = bare
        .create_advisory(&m, &w.public_repo, &with_fork)
        .await
        .unwrap_err();
    assert_eq!(
        (err.reason, err.subcode.as_str()),
        (ErrorReason::Unimplemented, "served_elsewhere")
    );
    // Illegal moves.
    for target in [
        SecurityAdvisoryState::Withdrawn,
        SecurityAdvisoryState::Published,
        SecurityAdvisoryState::Triage,
    ] {
        let mut r = update(&w, &a.ghsa_id);
        r.state = Some(target as i32);
        assert_eq!(
            w.sec
                .update_repo_advisory(&m, &w.public_repo, &r, &v)
                .await
                .unwrap_err()
                .reason,
            ErrorReason::InvalidState,
            "{target:?}"
        );
    }
    // Updates validate too and keep unset fields.
    let mut r = update(&w, &a.ghsa_id);
    r.cvss_vector = Some("garbage".into());
    assert_eq!(
        w.sec
            .update_repo_advisory(&m, &w.public_repo, &r, &v)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidArgument
    );
    let mut r = update(&w, &a.ghsa_id);
    r.severity = Some(SecuritySeverity::Low as i32);
    r.cwe_ids = vec!["CWE-20".into()];
    let u = w
        .sec
        .update_repo_advisory(&m, &w.public_repo, &r, &v)
        .await
        .unwrap();
    assert_eq!(
        (u.severity, u.cwe_ids.clone()),
        (SecuritySeverity::Low as i32, vec!["CWE-20".to_string()])
    );
    assert_eq!(u.summary, a.summary);
    assert!(
        w.sec
            .get_repo_advisory(&m, &w.public_repo, "GHSA-nope-nope-nope", &v)
            .await
            .is_err()
    );
    // A draft needs content to publish.
    let empty = w
        .sec
        .create_advisory(
            &m,
            &w.public_repo,
            &CreateSecurityAdvisoryRequest {
                repo: w.pub_ref(),
                summary: "thin".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        w.sec
            .publish_advisory(&m, &w.public_repo, &empty.ghsa_id, &v)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidArgument
    );
}

#[tokio::test]
async fn cve_requests_publication_and_withdrawal_reach_the_global_database_and_alerts() {
    let w = world().await;
    let m = w.cx("maintainer");
    let v = viewer(&w, "maintainer", true);
    // The public repo depends on the vulnerable lodash.
    w.push_to(
        &w.public_repo,
        &[
            ("package.json", Some(PACKAGE_JSON)),
            ("package-lock.json", Some(PACKAGE_LOCK)),
        ],
        "deps",
    )
    .await;
    let tip = w.tip_of(&w.public_repo).await;
    w.sec
        .on_default_branch_moved(&Cx::system(), &w.public_repo, &tip)
        .await
        .unwrap();
    assert!(
        pub_alerts(&w).await.is_empty(),
        "no advisory knows lodash yet"
    );

    let a = w
        .sec
        .create_advisory(&m, &w.public_repo, &draft(&w))
        .await
        .unwrap();
    // CVE request: a record for the CNA, once per advisory.
    let req = w
        .sec
        .request_cve(&m, &w.public_repo, &a.ghsa_id, &v)
        .await
        .unwrap();
    assert!(req.cve_requested && req.cve_id.is_empty());
    assert_eq!(
        w.sec
            .request_cve(&m, &w.public_repo, &a.ghsa_id, &v)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );
    assert!(
        w.sec
            .fulfil_cve_request(&m, &w.public_repo, &a.ghsa_id, "nope")
            .await
            .is_err()
    );
    let with_cve = w
        .sec
        .fulfil_cve_request(&m, &w.public_repo, &a.ghsa_id, "CVE-2099-7777")
        .await
        .unwrap();
    assert_eq!(with_cve.cve_id, "CVE-2099-7777");
    // The database does not know it before publication.
    let err = w
        .sec
        .get_advisory(
            &m,
            &GetAdvisoryRequest {
                key: Some(get_advisory_request::Key::GhsaId(a.ghsa_id.clone())),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::NotFound);

    // Publishing makes it global, matches the repo's graph at once, and freezes collaborators.
    let p = w
        .sec
        .publish_advisory(&m, &w.public_repo, &a.ghsa_id, &v)
        .await
        .unwrap();
    assert_eq!(p.state, SecurityAdvisoryState::Published as i32);
    assert!(p.published_at.is_some());
    let g = w
        .sec
        .get_advisory(
            &w.cx("outsider"),
            &GetAdvisoryRequest {
                key: Some(get_advisory_request::Key::CveId("CVE-2099-7777".into())),
            },
        )
        .await
        .unwrap();
    assert_eq!(g.ghsa_id, a.ghsa_id);
    assert_eq!(g.severity, SecuritySeverity::Critical as i32);
    assert_eq!(g.affected[0].name, "lodash");
    assert_eq!(g.affected[0].first_patched_version, "4.17.21");
    assert!(g.source_repo.is_some());
    let (listed, _) = w
        .sec
        .list_advisories(
            &w.cx("outsider"),
            &ListAdvisoriesRequest {
                ecosystem: "npm".into(),
                package_name: "lodash".into(),
                severity: SecuritySeverity::Critical as i32,
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    let alerts = pub_alerts(&w).await;
    assert_eq!(alerts.len(), 1, "the published advisory raised an alert");
    assert_eq!(alerts[0].advisory.as_ref().unwrap().ghsa_id, a.ghsa_id);
    assert_eq!(
        w.sec
            .publish_advisory(&m, &w.public_repo, &a.ghsa_id, &v)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert_eq!(
        w.sec
            .add_advisory_collaborator(
                &m,
                &w.public_repo,
                &AddSecurityAdvisoryCollaboratorRequest {
                    repo: w.pub_ref(),
                    ghsa_id: a.ghsa_id.clone(),
                    collaborator: Some(SecurityAdvisoryCredit {
                        principal_id: "writer".into(),
                        ..Default::default()
                    }),
                },
                &v
            )
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    // Anyone who can read the repo sees it published.
    let stranger = viewer(&w, "reader", false);
    assert!(
        w.sec
            .get_repo_advisory(&w.cx("reader"), &w.public_repo, &a.ghsa_id, &stranger)
            .await
            .is_ok()
    );
    // It survives an index rebuild (a restart): published advisories are stored.
    let again = Security::new(
        &w.forge,
        None,
        Ports {
            vcs: w.sec.ports().vcs.clone(),
            ..Ports::default()
        },
    );
    assert_eq!(
        again
            .get_advisory(
                &m,
                &GetAdvisoryRequest {
                    key: Some(get_advisory_request::Key::GhsaId(a.ghsa_id.clone()))
                }
            )
            .await
            .unwrap()
            .cve_id,
        "CVE-2099-7777"
    );
    // Edits to a published advisory follow into the global record; withdrawing it dismisses
    // the alert it raised.
    let mut r = update(&w, &a.ghsa_id);
    r.summary = Some("Prototype pollution in lodash (updated)".into());
    w.sec
        .update_repo_advisory(&m, &w.public_repo, &r, &v)
        .await
        .unwrap();
    let g = w
        .sec
        .get_advisory(
            &m,
            &GetAdvisoryRequest {
                key: Some(get_advisory_request::Key::GhsaId(a.ghsa_id.clone())),
            },
        )
        .await
        .unwrap();
    assert!(g.summary.ends_with("(updated)"));
    let mut r = update(&w, &a.ghsa_id);
    r.state = Some(SecurityAdvisoryState::Withdrawn as i32);
    assert_eq!(
        w.sec
            .update_repo_advisory(&m, &w.public_repo, &r, &v)
            .await
            .unwrap()
            .state,
        SecurityAdvisoryState::Withdrawn as i32
    );
    let g = w
        .sec
        .get_advisory(
            &m,
            &GetAdvisoryRequest {
                key: Some(get_advisory_request::Key::GhsaId(a.ghsa_id.clone())),
            },
        )
        .await
        .unwrap();
    assert!(g.withdrawn_at.is_some());
    let all = w
        .sec
        .list_dependency_alerts(
            &w.cx("owner"),
            &w.public_repo,
            &ListDependencyAlertsRequest {
                repo: w.pub_ref(),
                states: vec![DependencyAlertState::AutoDismissed as i32],
                ..Default::default()
            },
            "",
            10,
        )
        .await
        .unwrap()
        .0;
    assert_eq!(all.len(), 1);
    let mut r = update(&w, &a.ghsa_id);
    r.summary = Some("too late".into());
    assert_eq!(
        w.sec
            .update_repo_advisory(&m, &w.public_repo, &r, &v)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    let kinds = w.kinds_of(&w.public_repo).await;
    for k in [
        "security_advisory.created",
        "security_advisory.published",
        "security_advisory.cve_requested",
    ] {
        assert!(kinds.contains(&k.to_string()), "missing {k}: {kinds:?}");
    }
    let actions = w.audit_actions().await;
    for a in [
        "security.advisory.create",
        "security.advisory.publish",
        "security.advisory.cve_request",
        "security.advisory.withdraw",
    ] {
        assert!(actions.contains(&a.to_string()), "missing {a}");
    }
}
