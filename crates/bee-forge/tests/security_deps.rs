//! Dependency graph, SBOM, OSV matching with incremental re-evaluation, dependency review and
//! the update change requests the update bot opens through the changes domain.

use crate::security_support;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bee_deps::Ecosystem;
use bee_forge::Cx;
use bee_forge::changes::NewChange;
use bee_forge::error::ForgeError;
use bee_forge::security::Security;
use bee_forge::security::ports::{DocumentPublisher, Ports};
use bee_forge::security::updates::{cron_matches, parse_update_config};
use bee_proto::v1::*;
use security_support::*;

fn alert_filter(w: &World, states: Vec<i32>) -> ListDependencyAlertsRequest {
    ListDependencyAlertsRequest {
        repo: w.rref(),
        states,
        ..Default::default()
    }
}

async fn alerts(w: &World, states: &[DependencyAlertState]) -> Vec<DependencyAlert> {
    w.sec
        .list_dependency_alerts(
            &w.cx("owner"),
            &w.repo,
            &alert_filter(w, states.iter().map(|s| *s as i32).collect()),
            "",
            200,
        )
        .await
        .unwrap()
        .0
}

async fn open_alerts(w: &World) -> Vec<DependencyAlert> {
    alerts(w, &[DependencyAlertState::Open]).await
}

fn name_of(a: &DependencyAlert) -> String {
    a.dependency.clone().unwrap().name
}

#[tokio::test]
async fn graph_lists_manifests_and_dependencies_at_a_revision() {
    let w = world().await;
    w.plant_project().await;
    let cx = w.cx("reader");
    let (manifests, _) = w
        .sec
        .list_manifests(&cx, &w.repo, "", "", 50)
        .await
        .unwrap();
    let paths: Vec<&str> = manifests.iter().map(|m| m.path.as_str()).collect();
    for p in [
        "Cargo.lock",
        "Cargo.toml",
        "go.mod",
        "package-lock.json",
        "package.json",
        "requirements.txt",
    ] {
        assert!(paths.contains(&p), "{p} missing from {paths:?}");
    }
    let lock = manifests
        .iter()
        .find(|m| m.path == "package-lock.json")
        .unwrap();
    assert!(lock.is_lockfile);
    assert_eq!(lock.ecosystem, "npm");
    assert_eq!(lock.detector, "static");

    let all = w
        .sec
        .list_dependencies(&cx, &w.repo, &ListDependenciesRequest::default(), "", 100)
        .await
        .unwrap();
    assert!(all.changeset.is_some());
    let purls: Vec<&str> = all
        .dependencies
        .iter()
        .map(|d| d.package_url.as_str())
        .collect();
    for p in [
        "pkg:npm/lodash@4.17.20",
        "pkg:cargo/time@0.3.1",
        "pkg:pypi/flask@2.0.1",
        "pkg:golang/example.com/m@v1.0.0",
    ] {
        assert!(purls.contains(&p), "{p} missing from {purls:?}");
    }
    let npm = w
        .sec
        .list_dependencies(
            &cx,
            &w.repo,
            &ListDependenciesRequest {
                ecosystem: "npm".into(),
                manifest_path: "package-lock.json".into(),
                ..Default::default()
            },
            "",
            100,
        )
        .await
        .unwrap();
    assert!(npm.dependencies.iter().all(|d| d.ecosystem == "npm"));
    assert!(
        npm.dependencies
            .iter()
            .any(|d| d.name == "lodash" && d.relationship == DependencyRelationship::Direct as i32)
    );
    // Paging.
    let first = w
        .sec
        .list_dependencies(&cx, &w.repo, &ListDependenciesRequest::default(), "", 2)
        .await
        .unwrap();
    assert_eq!(first.dependencies.len(), 2);
    let token = first.page.unwrap().next_page_token;
    assert!(!token.is_empty());
    let second = w
        .sec
        .list_dependencies(&cx, &w.repo, &ListDependenciesRequest::default(), &token, 2)
        .await
        .unwrap();
    assert_ne!(
        first.dependencies[0].package_url,
        second.dependencies[0].package_url
    );
}

#[tokio::test]
async fn osv_ranges_decide_alerts_in_every_ecosystem() {
    let w = world().await;
    w.load_advisories().await;
    w.plant_project().await;
    assert!(w.evaluate().await);
    let open = open_alerts(&w).await;
    let names: Vec<String> = open.iter().map(name_of).collect();
    for n in ["lodash", "time", "flask", "example.com/m"] {
        assert!(names.contains(&n.to_string()), "{n} not in {names:?}");
    }
    let by = |n: &str| open.iter().find(|a| name_of(a) == n).unwrap().clone();
    assert_eq!(by("lodash").severity, SecuritySeverity::Critical as i32);
    assert_eq!(by("time").severity, SecuritySeverity::High as i32);
    assert_eq!(by("flask").severity, SecuritySeverity::Medium as i32);
    assert_eq!(by("example.com/m").severity, SecuritySeverity::Low as i32);
    let adv = by("lodash").advisory.unwrap();
    assert_eq!(adv.ghsa_id, "GHSA-npm1-aaaa-bbbb");
    assert_eq!(adv.cve_id, "CVE-2099-0001");
    assert_eq!(adv.affected[0].first_patched_version, "4.17.21");
    assert!(by("lodash").url.contains("/security/dependabot/"));

    // Boundaries, per ecosystem: (name, version, vulnerable?) planted as lockfile content.
    let cases: &[(&str, &str, bool)] = &[
        ("npm", "4.17.21", false),
        ("npm", "4.17.20", true),
        ("cargo", "0.3.36", false),
        ("cargo", "0.3.35", true),
        ("cargo", "0.2.9", false),
        ("pypi", "2.0.2", false),
        ("pypi", "2.0.2rc1", true),
        ("pypi", "2.0.1", true),
        ("go", "v1.0.1", false),
        ("go", "v1.0.0", true),
    ];
    for (eco, version, vulnerable) in cases {
        let files = match *eco {
            "npm" => vec![bee_deps::SourceFile::new(
                "package-lock.json",
                &PACKAGE_LOCK.replace("4.17.20", version),
            )],
            "cargo" => vec![bee_deps::SourceFile::new(
                "Cargo.lock",
                &CARGO_LOCK.replace("0.3.1", version),
            )],
            "pypi" => vec![bee_deps::SourceFile::new(
                "requirements.txt",
                &format!("flask=={version}\n"),
            )],
            _ => vec![bee_deps::SourceFile::new(
                "go.mod",
                &format!("module x\n\nrequire example.com/m {version}\n"),
            )],
        };
        let graph = bee_deps::DependencyGraph::from_files(&files);
        w.sec
            .evaluate_graph(&w.cx("owner"), &w.repo, &graph)
            .await
            .unwrap();
        let n = match *eco {
            "npm" => "lodash",
            "cargo" => "time",
            "pypi" => "flask",
            _ => "example.com/m",
        };
        let hit = open_alerts(&w).await.iter().any(|a| name_of(a) == n);
        assert_eq!(hit, *vulnerable, "{eco} {version}");
    }
}

#[tokio::test]
async fn advisory_updates_reevaluate_only_the_repos_they_touch() {
    let w = world().await;
    w.plant_project().await;
    assert!(w.evaluate().await);
    assert!(open_alerts(&w).await.is_empty(), "no advisories yet");

    // Advisories arrive: the repo's alerts appear without any push.
    w.feed.set(advisories());
    let report = w.sec.refresh_advisories(&w.cx("owner")).await.unwrap();
    assert_eq!(report.added, 4);
    assert_eq!(report.repos_evaluated, 1);
    assert_eq!(open_alerts(&w).await.len(), 4);
    // The same feed again is a no-op.
    let again = w.sec.refresh_advisories(&w.cx("owner")).await.unwrap();
    assert_eq!(again, Default::default());

    // An advisory for a package nobody uses touches no repo.
    let mut more = advisories();
    more.push(osv(
        "GHSA-none-aaaa-bbbb",
        "CVE-2099-0100",
        "npm",
        "left-pad",
        "SEMVER",
        "0",
        "9.9.9",
        "LOW",
    ));
    w.feed.set(more.clone());
    let r = w.sec.refresh_advisories(&w.cx("owner")).await.unwrap();
    assert_eq!((r.added, r.repos_evaluated), (1, 0));

    // A changed record re-evaluates; a withdrawn one auto-dismisses its alert.
    let mut withdrawn: serde_json::Value = serde_json::from_str(&more[0]).unwrap();
    withdrawn["withdrawn"] = "2024-03-01T00:00:00Z".into();
    more[0] = withdrawn.to_string();
    w.feed.set(more);
    let r = w.sec.refresh_advisories(&w.cx("owner")).await.unwrap();
    assert_eq!((r.changed, r.repos_evaluated), (1, 1));
    let auto = alerts(&w, &[DependencyAlertState::AutoDismissed]).await;
    assert_eq!(auto.len(), 1);
    assert_eq!(name_of(&auto[0]), "lodash");
    assert_eq!(
        auto[0].dismissed_reason,
        DependencyAlertDismissReason::Inaccurate as i32
    );
    assert_eq!(open_alerts(&w).await.len(), 3);

    // An advisory dropped from the feed counts as removed.
    w.feed.set(advisories().into_iter().skip(1).collect());
    let r = w.sec.refresh_advisories(&w.cx("owner")).await.unwrap();
    assert_eq!(r.removed, 2, "the withdrawn lodash record and left-pad");
}

/// The W14 load run's forge stall: every `main` move listed the whole tree. After the first
/// graph, moves are evaluated from the changed paths only, and a move that touches dependency
/// files re-reads just those.
#[tokio::test]
async fn default_branch_moves_never_list_the_whole_tree_again() {
    use std::sync::atomic::Ordering;
    let w = world().await;
    w.load_advisories().await;
    w.plant_project().await;
    let many: Vec<(String, String)> = (0..300)
        .map(|i| (format!("src/m{:02}/f{i}.txt", i % 10), format!("{i}\n")))
        .collect();
    let edits: Vec<(&str, Option<&str>)> = many
        .iter()
        .map(|(p, c)| (p.as_str(), Some(c.as_str())))
        .collect();
    w.push(&edits, "A larger tree").await;
    assert!(w.evaluate().await);
    let first = w.listings.load(Ordering::SeqCst);
    assert_eq!(first, 1, "the first graph lists the tree once");
    assert_eq!(open_alerts(&w).await.len(), 4);

    // Twenty unrelated moves: nothing is listed and nothing is re-parsed.
    for i in 0..20 {
        let tip = w
            .push(&[(&format!("src/m0/new{i}.txt"), Some("x"))], "Unrelated")
            .await;
        assert!(
            !w.sec
                .on_default_branch_moved(&Cx::system(), &w.repo, &tip)
                .await
                .unwrap()
        );
    }
    // A move that skips over several changesets, one of them bumping lodash.
    w.push(
        &[(
            "package-lock.json",
            Some(&PACKAGE_LOCK.replace("4.17.20", "4.17.21")),
        )],
        "Bump lodash",
    )
    .await;
    let tip = w.push(&[("src/m1/after.txt", Some("y"))], "After").await;
    assert!(
        w.sec
            .on_default_branch_moved(&Cx::system(), &w.repo, &tip)
            .await
            .unwrap()
    );
    assert_eq!(open_alerts(&w).await.len(), 3, "the lodash alert is fixed");
    // A new manifest in a new directory is found from the changed paths too.
    let tip = w
        .push(&[("svc/go.mod", Some(GO_MOD))], "A second module")
        .await;
    assert!(
        w.sec
            .on_default_branch_moved(&Cx::system(), &w.repo, &tip)
            .await
            .unwrap()
    );
    assert_eq!(
        w.listings.load(Ordering::SeqCst),
        first,
        "moves never listed the whole tree"
    );
    let deps = w
        .sec
        .list_dependencies(
            &w.cx("owner"),
            &w.repo,
            &ListDependenciesRequest {
                repo: w.rref(),
                ..Default::default()
            },
            "",
            1000,
        )
        .await
        .unwrap();
    assert!(
        deps.dependencies
            .iter()
            .any(|d| d.manifest_path == "svc/go.mod"),
        "the new manifest is in the graph"
    );
}

#[tokio::test]
async fn manifest_changes_fix_alerts_and_unrelated_pushes_are_skipped() {
    let w = world().await;
    w.load_advisories().await;
    w.plant_project().await;
    assert!(w.evaluate().await);
    let n = open_alerts(&w).await.len();
    assert_eq!(n, 4);

    // A push that touches no dependency file does not re-parse.
    let tip = w.push(&[("docs/a.md", Some("a"))], "Docs").await;
    assert!(
        !w.sec
            .on_default_branch_moved(&Cx::system(), &w.repo, &tip)
            .await
            .unwrap()
    );

    // Bumping lodash in the lockfile closes exactly its alert.
    let tip = w
        .push(
            &[(
                "package-lock.json",
                Some(&PACKAGE_LOCK.replace("4.17.20", "4.17.21")),
            )],
            "Bump lodash",
        )
        .await;
    assert!(
        w.sec
            .on_default_branch_moved(&Cx::system(), &w.repo, &tip)
            .await
            .unwrap()
    );
    assert_eq!(open_alerts(&w).await.len(), 3);
    let fixed = alerts(&w, &[DependencyAlertState::Fixed]).await;
    assert_eq!(fixed.len(), 1);
    assert_eq!(name_of(&fixed[0]), "lodash");
    assert!(fixed[0].fixed_at.is_some());

    // A fixed alert can neither be dismissed nor reopened by hand; reintroducing the old
    // version reopens it.
    let err = w
        .sec
        .update_dependency_alert(
            &w.cx("owner"),
            &w.repo,
            &UpdateDependencyAlertRequest {
                repo: w.rref(),
                number: fixed[0].number,
                state: DependencyAlertState::Open as i32,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidState);
    let tip = w
        .push(&[("package-lock.json", Some(PACKAGE_LOCK))], "Revert")
        .await;
    w.sec
        .on_default_branch_moved(&Cx::system(), &w.repo, &tip)
        .await
        .unwrap();
    assert_eq!(open_alerts(&w).await.len(), 4);
    assert!(
        w.kinds()
            .await
            .contains(&"security_alert.reopened".to_string())
    );
}

#[tokio::test]
async fn dismissal_needs_a_reason_and_survives_reevaluation() {
    let w = world().await;
    w.load_advisories().await;
    w.plant_project().await;
    w.evaluate().await;
    let a = open_alerts(&w)
        .await
        .into_iter()
        .find(|a| name_of(a) == "flask")
        .unwrap();
    let mut req = UpdateDependencyAlertRequest {
        repo: w.rref(),
        number: a.number,
        state: DependencyAlertState::Dismissed as i32,
        ..Default::default()
    };
    let err = w
        .sec
        .update_dependency_alert(&w.cx("writer"), &w.repo, &req)
        .await
        .unwrap_err();
    assert_eq!(
        (err.reason, err.subcode.as_str()),
        (ErrorReason::InvalidArgument, "missing_reason")
    );
    req.dismissed_reason = DependencyAlertDismissReason::TolerableRisk as i32;
    req.dismissed_comment = "internal tool".into();
    let d = w
        .sec
        .update_dependency_alert(&w.cx("writer"), &w.repo, &req)
        .await
        .unwrap();
    assert_eq!(d.state, DependencyAlertState::Dismissed as i32);
    assert_eq!(d.dismissed_by.unwrap().id, "writer");
    assert_eq!(d.dismissed_comment, "internal tool");
    // Re-evaluating keeps it dismissed and does not open a duplicate.
    let tip = w
        .push(
            &[("requirements.txt", Some("flask==2.0.1\nrequests==2.31.1\n"))],
            "Bump requests",
        )
        .await;
    w.sec
        .on_default_branch_moved(&Cx::system(), &w.repo, &tip)
        .await
        .unwrap();
    let all = alerts(
        &w,
        &[DependencyAlertState::Open, DependencyAlertState::Dismissed],
    )
    .await;
    assert_eq!(all.iter().filter(|x| name_of(x) == "flask").count(), 1);
    assert_eq!(
        w.sec
            .get_dependency_alert(&w.cx("writer"), &w.repo, a.number)
            .await
            .unwrap()
            .state,
        DependencyAlertState::Dismissed as i32
    );
    // Reopen.
    req.state = DependencyAlertState::Open as i32;
    let o = w
        .sec
        .update_dependency_alert(&w.cx("writer"), &w.repo, &req)
        .await
        .unwrap();
    assert_eq!(o.state, DependencyAlertState::Open as i32);
    assert!(o.dismissed_by.is_none());
    let actions = w.audit_actions().await;
    assert!(actions.contains(&"security.dependency_alert.dismiss".to_string()));
    assert!(actions.contains(&"security.dependency_alert.reopen".to_string()));
}

#[tokio::test]
async fn snapshots_replace_by_job_and_feed_the_graph() {
    let w = world().await;
    w.feed.set(vec![osv(
        "GHSA-mvn1-aaaa-bbbb",
        "CVE-2099-0200",
        "Maven",
        "org.example:lib",
        "ECOSYSTEM",
        "0",
        "2.0",
        "HIGH",
    )]);
    w.sec.refresh_advisories(&w.cx("owner")).await.unwrap();
    let dep = |v: &str| Dependency {
        package_url: format!("pkg:maven/org.example/lib@{v}"),
        ecosystem: "maven".into(),
        name: "org.example:lib".into(),
        version: v.into(),
        relationship: DependencyRelationship::Direct as i32,
        ..Default::default()
    };
    let snapshot = |version: &str, deps: Vec<Dependency>| SubmitDependencySnapshotRequest {
        repo: w.rref(),
        detector_name: "gradle-plugin".into(),
        detector_version: version.into(),
        job_correlator: "build-7".into(),
        job_id: "1".into(),
        manifests: vec![DependencySubmittedManifest {
            path: "build.gradle".into(),
            ecosystem: "maven".into(),
            dependencies: deps,
        }],
        ..Default::default()
    };
    let r = w
        .sec
        .submit_snapshot(&w.cx("ci"), &w.repo, snapshot("1.0", vec![dep("1.0")]))
        .await
        .unwrap();
    assert_eq!(r.snapshot_id, "gradle-plugin:build-7");
    assert_eq!(r.dependency_count, 1);
    let listed = w
        .sec
        .list_dependencies(
            &w.cx("ci"),
            &w.repo,
            &ListDependenciesRequest::default(),
            "",
            50,
        )
        .await
        .unwrap();
    let d = listed
        .dependencies
        .iter()
        .find(|d| d.name == "org.example:lib")
        .unwrap();
    assert!(d.submitted);
    let manifests = w
        .sec
        .list_manifests(&w.cx("ci"), &w.repo, "", "", 50)
        .await
        .unwrap()
        .0;
    assert_eq!(
        manifests
            .iter()
            .find(|m| m.path == "build.gradle")
            .unwrap()
            .detector,
        "submitted"
    );
    assert_eq!(
        open_alerts(&w).await.len(),
        1,
        "the submitted package is vulnerable"
    );

    // The next snapshot of the same job replaces the first: the fixed version closes the alert.
    w.sec
        .submit_snapshot(&w.cx("ci"), &w.repo, snapshot("1.1", vec![dep("2.0")]))
        .await
        .unwrap();
    assert!(open_alerts(&w).await.is_empty());
    let all = w
        .sec
        .list_dependencies(
            &w.cx("ci"),
            &w.repo,
            &ListDependenciesRequest::default(),
            "",
            50,
        )
        .await
        .unwrap();
    assert_eq!(
        all.dependencies
            .iter()
            .filter(|d| d.name == "org.example:lib")
            .count(),
        1
    );

    // Malformed package URLs and oversized snapshots are refused.
    let mut bad = dep("1.0");
    bad.package_url = "not-a-purl".into();
    let err = w
        .sec
        .submit_snapshot(&w.cx("ci"), &w.repo, snapshot("1.2", vec![bad]))
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidArgument);
    assert_eq!(err.subcode, "invalid_package_url");
    let many: Vec<Dependency> = (0..20_001).map(|i| dep(&format!("0.{i}"))).collect();
    let err = w
        .sec
        .submit_snapshot(&w.cx("ci"), &w.repo, snapshot("1.3", many))
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::TooLarge);
}

struct Capture(Mutex<Vec<(String, usize)>>);

#[async_trait]
impl DocumentPublisher for Capture {
    async fn publish(
        &self,
        name: &str,
        _ctype: &str,
        bytes: Vec<u8>,
    ) -> bee_forge::Result<(String, u64)> {
        self.0.lock().unwrap().push((name.to_string(), bytes.len()));
        Ok((
            format!("https://files.test/{name}?sig=abc"),
            4_102_444_800_000,
        ))
    }
}

#[tokio::test]
async fn sbom_exports_spdx_and_cyclonedx_inline_or_by_signed_url() {
    let w = world().await;
    w.plant_project().await;
    let ex = |format: SbomFormat, url: bool| ExportSbomRequest {
        repo: w.rref(),
        format: format as i32,
        prefer_signed_url: url,
        ..Default::default()
    };
    let spdx = w
        .sec
        .export_sbom(&w.cx("reader"), &w.repo, &ex(SbomFormat::SpdxJson, false))
        .await
        .unwrap();
    let Some(export_sbom_response::Body::Document(text)) = spdx.body else {
        panic!("inline")
    };
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["spdxVersion"], "SPDX-2.3");
    assert!(text.contains("pkg:npm/lodash@4.17.20"));
    let cdx = w
        .sec
        .export_sbom(
            &w.cx("reader"),
            &w.repo,
            &ex(SbomFormat::CyclonedxJson, false),
        )
        .await
        .unwrap();
    let Some(export_sbom_response::Body::Document(text)) = cdx.body else {
        panic!("inline")
    };
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["bomFormat"], "CycloneDX");
    assert!(
        doc["components"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["purl"] == "pkg:cargo/time@0.3.1")
    );
    let err = w
        .sec
        .export_sbom(
            &w.cx("reader"),
            &w.repo,
            &ExportSbomRequest {
                repo: w.rref(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidArgument);

    // With a publisher a large (or requested) document goes out as a signed URL.
    let cap = Arc::new(Capture(Mutex::default()));
    let sec = Security::new(
        &w.forge,
        None,
        Ports {
            vcs: w.sec.ports().vcs.clone(),
            publisher: Some(cap.clone()),
            ..Ports::default()
        },
    );
    let r = sec
        .export_sbom(&w.cx("reader"), &w.repo, &ex(SbomFormat::SpdxJson, true))
        .await
        .unwrap();
    assert!(
        matches!(r.body, Some(export_sbom_response::Body::SignedUrl(ref u)) if u.contains("files.test"))
    );
    assert!(r.url_expires_at.is_some());
    assert_eq!(cap.0.lock().unwrap().len(), 1);
}

async fn open_cr(w: &World, who: &str, head: ObjectId, title: &str) -> ChangeRequest {
    w.changes
        .open(
            &w.cx(who),
            &w.repo,
            NewChange {
                head: Some(head),
                title: title.into(),
                ..NewChange::default()
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn dependency_review_fails_new_vulnerabilities_and_reports_a_check() {
    let w = world().await;
    w.load_advisories().await;
    w.push(
        &[
            ("package.json", Some(PACKAGE_JSON)),
            (
                "package-lock.json",
                Some(&PACKAGE_LOCK.replace("4.17.20", "4.17.21")),
            ),
        ],
        "Project with a fixed lodash",
    )
    .await;
    let tip = w.tip().await;
    let bad = w
        .commit_in(
            &w.repo,
            Some(&tip),
            &[("requirements.txt", Some("flask==2.0.1\n"))],
            "Add flask",
            "writer",
        )
        .await;
    let cr = open_cr(&w, "writer", bad.clone(), "Add flask").await;
    let review = |n: u64| GetDependencyReviewRequest {
        repo: w.rref(),
        target: Some(get_dependency_review_request::Target::ChangeRequest(
            n.to_string(),
        )),
        ..Default::default()
    };
    let r = w
        .sec
        .dependency_review(&w.cx("writer"), &w.repo, &review(cr.number))
        .await
        .unwrap();
    assert!(!r.passes);
    assert_eq!(r.vulnerable_additions, 1);
    assert_eq!(r.highest_severity, SecuritySeverity::Medium as i32);
    let c = r.changes.iter().find(|c| c.name == "flask").unwrap();
    assert_eq!(c.change_type, DependencyChangeType::Added as i32);
    assert_eq!(c.vulnerabilities[0].ghsa_id, "GHSA-pypi-aaaa-bbbb");
    assert!(!r.check_run_id.is_empty());
    let run = w
        .forge
        .checks
        .get(&w.cx("owner"), &w.repo.id, &r.check_run_id)
        .await
        .unwrap();
    assert_eq!(
        (run.name.as_str(), run.status),
        ("dependency-review", CheckStatus::Failed as i32)
    );
    // Asking again does not pile up runs of the same verdict.
    let again = w
        .sec
        .dependency_review(&w.cx("writer"), &w.repo, &review(cr.number))
        .await
        .unwrap();
    assert_eq!(again.check_run_id, r.check_run_id);

    // The same range as a revision pair; fail_on_severity above the finding passes it.
    let by_range = w
        .sec
        .dependency_review(
            &w.cx("writer"),
            &w.repo,
            &GetDependencyReviewRequest {
                repo: w.rref(),
                target: Some(get_dependency_review_request::Target::Range(
                    DependencyReviewRange {
                        base: Some(Revision {
                            expr: "main".into(),
                        }),
                        head: Some(Revision { expr: hex_of(&bad) }),
                    },
                )),
                fail_on_severity: SecuritySeverity::Critical as i32,
            },
        )
        .await
        .unwrap();
    assert!(by_range.passes);
    assert!(by_range.check_run_id.is_empty());

    // A change that only bumps a vulnerable package up to its fix passes.
    w.push(
        &[("requirements.txt", Some("flask==2.0.1\n"))],
        "Land flask",
    )
    .await;
    let tip = w.tip().await;
    let good = w
        .commit_in(
            &w.repo,
            Some(&tip),
            &[("requirements.txt", Some("flask==2.0.2\n"))],
            "Bump flask",
            "writer",
        )
        .await;
    let cr2 = open_cr(&w, "writer", good, "Bump flask").await;
    let ok = w
        .sec
        .dependency_review(&w.cx("writer"), &w.repo, &review(cr2.number))
        .await
        .unwrap();
    assert!(ok.passes, "{ok:?}");
    assert_eq!(ok.vulnerable_additions, 0);
}

fn hex_of(id: &ObjectId) -> String {
    bee_forge::ids::hex(&id.raw)
}

async fn run_security_update(w: &World) -> DependencyUpdateRun {
    w.sec
        .trigger_run(&w.cx("maintainer"), &w.repo, "", "", true)
        .await
        .unwrap()
}

async fn security_runs(w: &World) -> Vec<DependencyUpdateRun> {
    w.sec
        .list_runs(&w.cx("owner"), &w.repo, 0, "", 20)
        .await
        .unwrap()
        .0
}

async fn assert_alerts_linked_once(w: &World, expected_alerts: usize) {
    let open = open_alerts(w).await;
    assert_eq!(open.len(), expected_alerts);
    assert!(
        open.iter()
            .all(|alert| !alert.update_change_request.is_empty()),
        "each open alert has its update change request: {open:?}"
    );
    assert_eq!(security_runs(w).await.len(), 1, "one reconciliation run");
}

#[tokio::test]
async fn security_update_reconciliation_is_order_independent_and_idempotent() {
    // Alert first, then the switch: enabling reconciles the already-open alerts.
    let alert_first = world().await;
    alert_first.load_advisories().await;
    alert_first.plant_project().await;
    assert!(alert_first.evaluate().await);
    assert!(
        open_alerts(&alert_first)
            .await
            .iter()
            .all(|alert| alert.update_change_request.is_empty())
    );
    alert_first.enable_security_updates().await;
    assert_alerts_linked_once(&alert_first, 4).await;
    alert_first.enable_security_updates().await;
    assert_alerts_linked_once(&alert_first, 4).await;

    // If a run is already queued, turning the switch on does not queue another one.
    let queued = world().await;
    queued.load_advisories().await;
    queued.plant_project().await;
    assert!(queued.evaluate().await);
    let run = queued
        .sec
        .queue_run(&queued.cx("owner"), &queued.repo, "", "", true)
        .await
        .unwrap();
    queued.enable_security_updates().await;
    assert_eq!(security_runs(&queued).await.len(), 1);
    queued
        .sec
        .execute_run(&queued.cx("owner"), &queued.repo, &run.id)
        .await
        .unwrap();
    assert_alerts_linked_once(&queued, 4).await;

    // The opposite ordering is also a single run, including two advisories for lodash.
    let enabled_first = world().await;
    let mut source = advisories();
    source.push(osv(
        "GHSA-npm2-aaaa-bbbb",
        "CVE-2099-0005",
        "npm",
        "lodash",
        "SEMVER",
        "0",
        "4.17.21",
        "HIGH",
    ));
    enabled_first.feed.set(source);
    enabled_first
        .sec
        .refresh_advisories(&enabled_first.cx("owner"))
        .await
        .unwrap();
    enabled_first.enable_security_updates().await;
    enabled_first.plant_project().await;
    assert!(enabled_first.evaluate().await);
    assert_alerts_linked_once(&enabled_first, 5).await;
    let lodash: Vec<_> = open_alerts(&enabled_first)
        .await
        .into_iter()
        .filter(|alert| {
            alert
                .dependency
                .as_ref()
                .is_some_and(|d| d.name == "lodash")
        })
        .collect();
    assert_eq!(lodash.len(), 2);
    assert!(
        lodash
            .iter()
            .all(|alert| !alert.update_change_request.is_empty())
    );
}

#[tokio::test]
async fn concurrent_alert_and_enable_triggers_create_one_security_update_run() {
    let w = world().await;
    w.load_advisories().await;
    w.plant_project().await;
    let tip = w.tip().await;
    let mut tasks = Vec::new();
    for i in 0..32 {
        let sec = w.sec.clone();
        let repo = w.repo.clone();
        if i % 2 == 0 {
            tasks.push(tokio::spawn(async move {
                sec.update_config(
                    &Cx::new("owner", PrincipalKind::Human),
                    &repo,
                    &UpdateSecurityConfigurationRequest {
                        security_updates: Some(true),
                        ..Default::default()
                    },
                )
                .await
                .map(|_| ())
            }));
        } else {
            let tip = tip.clone();
            tasks.push(tokio::spawn(async move {
                sec.on_default_branch_moved(&Cx::system(), &repo, &tip)
                    .await
                    .map(|_| ())
            }));
        }
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_alerts_linked_once(&w, 4).await;
}

#[tokio::test]
async fn update_crs_carry_the_manifest_and_lockfile_edits_and_close_the_alerts_when_landed() {
    let w = world().await;
    w.registry
        .set_integrity(Ecosystem::Npm, "lodash", "4.17.21", "sha512-newlodash");
    w.registry
        .set_integrity(Ecosystem::Cargo, "time", "0.3.36", "bbbb");
    w.load_advisories().await;
    w.enable_security_updates().await;
    w.plant_project().await;
    // New alerts with security updates on start the bot by themselves.
    assert!(w.evaluate().await);
    let (runs, _) = w
        .sec
        .list_runs(&w.cx("owner"), &w.repo, 0, "", 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].state,
        DependencyUpdateRunState::Succeeded as i32,
        "{:?}",
        runs[0].error
    );
    assert_eq!(runs[0].change_requests.len(), 4);
    assert!(runs[0].security_only);

    let open = open_alerts(&w).await;
    assert_eq!(open.len(), 4);
    assert!(
        open.iter().all(|a| !a.update_change_request.is_empty()),
        "every alert points at its CR"
    );
    let cr_of = |name: &str| {
        let n: u64 = open
            .iter()
            .find(|a| name_of(a) == name)
            .unwrap()
            .update_change_request
            .parse()
            .unwrap();
        n
    };
    let get = |n: u64| {
        let w = &w;
        async move {
            w.changes
                .get_by_number(&w.cx("owner"), &w.repo, n)
                .await
                .unwrap()
        }
    };
    let head = |cr: &ChangeRequest| cr.revisions.last().unwrap().head.clone().unwrap();

    // npm: the range already allows the fix, so only the lockfile changes.
    let cr = get(cr_of("lodash")).await;
    assert_eq!(cr.title, "Bump lodash from 4.17.20 to 4.17.21");
    assert_eq!(cr.author.as_ref().unwrap().id, "bee-update-bot");
    assert!(cr.labels.iter().any(|l| l.name == "security"));
    assert!(cr.body.contains("GHSA-npm1-aaaa-bbbb"));
    let h = head(&cr);
    let lock = w.file(&h, "package-lock.json").await.unwrap();
    assert!(lock.contains("\"version\": \"4.17.21\""));
    assert!(lock.contains("lodash-4.17.21.tgz"));
    assert!(lock.contains("sha512-newlodash") && !lock.contains("sha512-oldlodash"));
    assert!(serde_json::from_str::<serde_json::Value>(&lock).is_ok());
    assert_eq!(w.file(&h, "package.json").await.unwrap(), PACKAGE_JSON);
    // Cargo: lock only, checksum from the registry.
    let h = head(&get(cr_of("time")).await);
    let lock = w.file(&h, "Cargo.lock").await.unwrap();
    assert!(lock.contains("version = \"0.3.36\"") && lock.contains("checksum = \"bbbb\""));
    assert!(!lock.contains("aaaa"));
    assert_eq!(w.file(&h, "Cargo.toml").await.unwrap(), CARGO_TOML);
    // pip: the pin moves.
    let h = head(&get(cr_of("flask")).await);
    assert_eq!(
        w.file(&h, "requirements.txt").await.unwrap(),
        "flask==2.0.2\nrequests==2.31.0\n"
    );
    // go: the require moves.
    let h = head(&get(cr_of("example.com/m")).await);
    assert!(
        w.file(&h, "go.mod")
            .await
            .unwrap()
            .contains("require example.com/m v1.0.1")
    );

    // A second run opens nothing new.
    let again = run_security_update(&w).await;
    assert_eq!(again.state, DependencyUpdateRunState::Succeeded as i32);
    assert!(again.change_requests.is_empty());

    // AS-SEC-01: landing the update CR closes the alert.
    let lodash = open.iter().find(|a| name_of(a) == "lodash").unwrap().number;
    let cr = get(cr_of("lodash")).await;
    let (landed, _) = w
        .changes
        .land(&w.cx("writer"), &w.repo, &cr.id, LandMethod::Rebase, "", 0)
        .await
        .unwrap();
    assert_eq!(landed.state, ChangeState::Landed as i32);
    assert!(w.evaluate().await);
    let a = w
        .sec
        .get_dependency_alert(&w.cx("owner"), &w.repo, lodash)
        .await
        .unwrap();
    assert_eq!(a.state, DependencyAlertState::Fixed as i32);
    assert_eq!(open_alerts(&w).await.len(), 3);
    let kinds = w.kinds().await;
    for k in [
        "security_alert.created",
        "security_alert.fixed",
        "dependency_update.run_started",
        "dependency_update.run_finished",
        "dependency_update.change_opened",
        "cr.opened",
        "cr.landed",
    ] {
        assert!(kinds.contains(&k.to_string()), "missing {k}: {kinds:?}");
    }
    assert!(
        w.audit_actions()
            .await
            .contains(&"security.dependency_update.open_change".to_string())
    );
}

const NPM_TWO: (&str, &str) = (
    r#"{
  "name": "app",
  "version": "1.0.0",
  "dependencies": {
    "lodash": "^4.17.20",
    "minimist": "~1.2.0"
  }
}
"#,
    r#"{
  "name": "app",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "app",
      "version": "1.0.0",
      "dependencies": {
        "lodash": "^4.17.20",
        "minimist": "~1.2.0"
      }
    },
    "node_modules/lodash": {
      "version": "4.17.20",
      "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.20.tgz",
      "integrity": "sha512-oldlodash"
    },
    "node_modules/minimist": {
      "version": "1.2.0",
      "resolved": "https://registry.npmjs.org/minimist/-/minimist-1.2.0.tgz",
      "integrity": "sha512-oldmin"
    }
  }
}
"#,
);

async fn npm_two(w: &World) {
    w.feed.set(vec![
        osv(
            "GHSA-npm1-aaaa-bbbb",
            "CVE-2099-0001",
            "npm",
            "lodash",
            "SEMVER",
            "0",
            "4.17.21",
            "CRITICAL",
        ),
        osv(
            "GHSA-npm2-aaaa-bbbb",
            "CVE-2099-0005",
            "npm",
            "minimist",
            "SEMVER",
            "0",
            "1.2.6",
            "HIGH",
        ),
    ]);
    w.sec.refresh_advisories(&w.cx("owner")).await.unwrap();
    w.push(
        &[
            ("package.json", Some(NPM_TWO.0)),
            ("package-lock.json", Some(NPM_TWO.1)),
        ],
        "Project",
    )
    .await;
}

#[tokio::test]
async fn updates_group_by_config_file_and_switch() {
    // Default: one change request per package.
    let w = world().await;
    npm_two(&w).await;
    w.enable_security_updates().await;
    w.evaluate().await;
    let (runs, _) = w
        .sec
        .list_runs(&w.cx("owner"), &w.repo, 0, "", 10)
        .await
        .unwrap();
    assert_eq!(runs[0].change_requests.len(), 2);

    // The config file's group puts both in one request; `.beehive` wins over `.github`.
    let w = world().await;
    npm_two(&w).await;
    w.push(
        &[
            (".github/dependabot.yml", Some("version: 2\nupdates:\n  - package-ecosystem: npm\n    directory: \"/\"\n    schedule:\n      interval: daily\n")),
            (
                ".beehive/dependabot.yml",
                Some("version: 2\nupdates:\n  - package-ecosystem: npm\n    directory: \"/\"\n    schedule:\n      interval: weekly\n    open-pull-requests-limit: 3\n    groups:\n      npm-security:\n        patterns:\n          - \"*\"\n"),
            ),
        ],
        "Config",
    )
    .await;
    let cfg = w
        .sec
        .get_update_config(&w.cx("writer"), &w.repo)
        .await
        .unwrap();
    assert_eq!(cfg.config_path, ".beehive/dependabot.yml");
    assert_eq!(cfg.entries.len(), 1);
    assert_eq!(cfg.entries[0].schedule, "weekly");
    assert_eq!(cfg.entries[0].open_change_request_limit, 3);
    assert_eq!(cfg.entries[0].groups, vec!["npm-security".to_string()]);
    assert!(cfg.config_errors.is_empty(), "{:?}", cfg.config_errors);
    w.enable_security_updates().await;
    w.evaluate().await;
    let (runs, _) = w
        .sec
        .list_runs(&w.cx("owner"), &w.repo, 0, "", 10)
        .await
        .unwrap();
    assert_eq!(runs[0].change_requests.len(), 1, "{:?}", runs[0]);
    let cr = w
        .changes
        .get_by_number(
            &w.cx("owner"),
            &w.repo,
            runs[0].change_requests[0].parse().unwrap(),
        )
        .await
        .unwrap();
    assert!(cr.title.contains("npm-security"));
    let lock = w
        .file(
            &cr.revisions.last().unwrap().head.clone().unwrap(),
            "package-lock.json",
        )
        .await
        .unwrap();
    assert!(lock.contains("\"version\": \"4.17.21\"") && lock.contains("\"version\": \"1.2.6\""));

    // The repo-wide switch groups a whole ecosystem without a config file.
    let w = world().await;
    npm_two(&w).await;
    w.sec
        .set_update_config(
            &w.cx("owner"),
            &w.repo,
            &SetDependencyUpdateConfigRequest {
                repo: w.rref(),
                security_updates_enabled: Some(true),
                grouped_security_updates: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    w.evaluate().await;
    let (runs, _) = w
        .sec
        .list_runs(&w.cx("owner"), &w.repo, 0, "", 10)
        .await
        .unwrap();
    assert_eq!(runs[0].change_requests.len(), 1);
}

#[tokio::test]
async fn open_request_limit_queued_runs_and_enablement_are_enforced() {
    let w = world().await;
    npm_two(&w).await;
    w.push(
        &[(
            ".github/dependabot.yml",
            Some("version: 2\nupdates:\n  - package-ecosystem: npm\n    directory: /\n    schedule:\n      interval: daily\n    open-pull-requests-limit: 1\n"),
        )],
        "Config",
    )
    .await;
    // Updates are off: refused.
    let err = w
        .sec
        .trigger_run(&w.cx("maintainer"), &w.repo, "", "", true)
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidState);
    w.enable_security_updates().await;
    w.evaluate().await;
    let (runs, _) = w
        .sec
        .list_runs(&w.cx("owner"), &w.repo, 0, "", 10)
        .await
        .unwrap();
    assert_eq!(
        runs[0].change_requests.len(),
        1,
        "limit of one open request"
    );
    // A queued run blocks another for the same entry.
    let queued = w
        .sec
        .queue_run(&w.cx("maintainer"), &w.repo, "npm", "/", true)
        .await
        .unwrap();
    assert_eq!(queued.state, DependencyUpdateRunState::Queued as i32);
    let err = w
        .sec
        .queue_run(&w.cx("maintainer"), &w.repo, "npm", "/", true)
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::ResourceExhausted);
    // A different entry is free; executing the queued one frees its entry.
    w.sec
        .queue_run(&w.cx("maintainer"), &w.repo, "cargo", "/", true)
        .await
        .unwrap();
    let done = w
        .sec
        .execute_run(&w.cx("maintainer"), &w.repo, &queued.id)
        .await
        .unwrap();
    assert_eq!(done.state, DependencyUpdateRunState::Succeeded as i32);
    assert!(done.finished_at.is_some());
    w.sec
        .queue_run(&w.cx("maintainer"), &w.repo, "npm", "/", true)
        .await
        .unwrap();
    // Unknown ecosystems are invalid.
    let err = w
        .sec
        .trigger_run(&w.cx("maintainer"), &w.repo, "cobol", "", true)
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidArgument);
    assert_eq!(
        w.sec
            .get_run(&w.cx("owner"), &w.repo, &done.id)
            .await
            .unwrap()
            .id,
        done.id
    );
    let missing: ForgeError = w
        .sec
        .get_run(&w.cx("owner"), &w.repo, "nope")
        .await
        .unwrap_err();
    assert_eq!(missing.reason, ErrorReason::NotFound);
}

#[tokio::test]
async fn scheduled_version_updates_use_the_registry_and_the_schedule() {
    let w = world().await;
    w.push(
        &[
            ("package.json", Some(PACKAGE_JSON)),
            ("package-lock.json", Some(PACKAGE_LOCK)),
            (
                ".beehive/dependabot.yml",
                Some("version: 2\nupdates:\n  - package-ecosystem: npm\n    directory: /\n    schedule:\n      interval: weekly\n"),
            ),
        ],
        "Project",
    )
    .await;
    w.registry.set_latest(Ecosystem::Npm, "lodash", "4.17.21");
    w.sec
        .set_update_config(
            &w.cx("owner"),
            &w.repo,
            &SetDependencyUpdateConfigRequest {
                repo: w.rref(),
                version_updates_enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(w.sec.run_due_updates(&Cx::system()).await.unwrap(), 1);
    let (runs, _) = w
        .sec
        .list_runs(&w.cx("owner"), &w.repo, 0, "", 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert!(!runs[0].security_only);
    assert_eq!(runs[0].change_requests.len(), 1, "{:?}", runs[0].error);
    let cr = w
        .changes
        .get_by_number(
            &w.cx("owner"),
            &w.repo,
            runs[0].change_requests[0].parse().unwrap(),
        )
        .await
        .unwrap();
    assert!(
        cr.labels.iter().any(|l| l.name == "dependencies")
            && !cr.labels.iter().any(|l| l.name == "security")
    );
    // Not due again until a week passes.
    assert_eq!(w.sec.run_due_updates(&Cx::system()).await.unwrap(), 0);
    w.clock.advance(7 * 24 * 3600 * 1000 + 1);
    w.registry.set_latest(Ecosystem::Npm, "lodash", "4.17.22");
    assert_eq!(w.sec.run_due_updates(&Cx::system()).await.unwrap(), 1);
    let (runs, _) = w
        .sec
        .list_runs(&w.cx("owner"), &w.repo, 0, "", 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 2);
}

#[test]
fn update_config_files_parse_in_both_formats() {
    let (entries, errors) = parse_update_config(
        ".github/dependabot.yml",
        r#"version: 2
updates:
  - package-ecosystem: "gomod"
    directory: "/svc"
    schedule:
      interval: "monthly"
    target-branch: release
    labels: [deps]
    ignore:
      - dependency-name: "golang.org/*"
  - package-ecosystem: pip
    directories:
      - /a
      - /b
    schedule:
      interval: daily
      cronjob: "0 6 * * 1"
  - package-ecosystem: cobol
    directory: /
    schedule:
      interval: daily
"#,
    );
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].ecosystem, "go");
    assert_eq!(entries[0].directory, "/svc");
    assert_eq!(entries[0].target_ref, "release");
    assert_eq!(entries[0].ignore, vec!["golang.org/*".to_string()]);
    assert_eq!(
        (entries[1].directory.as_str(), entries[2].directory.as_str()),
        ("/a", "/b")
    );
    assert_eq!(entries[1].schedule, "0 6 * * 1");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("cobol"));
    let (_, errors) = parse_update_config("x.yml", "updates: nope\n");
    assert!(errors.iter().any(|e| e.contains("version: 2")));
    assert!(cron_matches("0 6 * * 1", 1_736_143_200_000)); // Monday 06:00 UTC
    assert!(!cron_matches("0 6 * * 1", 1_736_146_800_000)); // 07:00
    assert!(cron_matches(
        "*/15 * * * *",
        1_736_143_200_000 + 15 * 60_000
    ));
}
