//! Vulnerability matching, dependency review and update planning, including the
//! AS-SEC-01-shaped end-to-end test.

use crate::common;
use bee_deps::cvss::Severity;
use bee_deps::matching::{Finding, MatchOptions, highest_severity, match_graph};
use bee_deps::osv::{AdvisorySource, FileAdvisorySource};
use bee_deps::review::{ChangeType, Conclusion, ReviewPolicy, review};
use bee_deps::update::{apply_plan, plan_security_updates};
use bee_deps::{DependencyGraph, Ecosystem, Relationship, Scope, SourceFile};
use common::{fixture_files, fixture_graph, manifest_dir};

fn osv() -> FileAdvisorySource {
    let mut s = FileAdvisorySource::new();
    let n = s
        .load_dir(&manifest_dir().join("tests/fixtures/osv"))
        .unwrap();
    assert_eq!(n, 13);
    s
}

fn findings(g: &DependencyGraph) -> Vec<Finding> {
    match_graph(g, &osv(), &MatchOptions::default())
}

fn find<'a>(f: &'a [Finding], pkg: &str) -> Vec<&'a Finding> {
    f.iter().filter(|x| x.package == pkg).collect()
}

fn graph(files: &[SourceFile]) -> DependencyGraph {
    DependencyGraph::from_files(files)
}

#[test]
fn npm_findings_dedupe_aliases_and_skip_withdrawn() {
    let g = fixture_graph("npm-v3");
    let f = findings(&g);
    let lodash = find(&f, "lodash");
    assert_eq!(
        lodash.len(),
        1,
        "GHSA and CVE records of one vulnerability are one finding"
    );
    let l = lodash[0];
    assert_eq!(l.advisory_id, "GHSA-35jh-r3h4-6jhm");
    assert_eq!(l.aliases, vec!["CVE-2021-23337".to_owned()]);
    assert_eq!(l.installed_version, "4.17.20");
    assert_eq!(l.fixed_versions, vec!["4.17.21".to_owned()]);
    assert_eq!(l.severity, Severity::High);
    assert_eq!(l.cvss_score, Some(7.2));
    assert!(l.cvss_vector.as_deref().unwrap().starts_with("CVSS:3.1/"));
    assert_eq!(l.manifest_path, "package-lock.json");
    assert_eq!(l.dependency_path, vec!["pkg:npm/lodash@4.17.20".to_owned()]);
    assert!(
        f.iter().all(|x| x.advisory_id != "GHSA-aaaa-bbbb-cccc"),
        "withdrawn advisories never match"
    );
    assert_eq!(f.len(), 1, "express, jest and friends are clean");
}

#[test]
fn npm_nested_vulnerable_version_reports_the_path() {
    let g = fixture_graph("npm-v1");
    let f = findings(&g);
    let m = find(&f, "minimist");
    assert_eq!(m.len(), 4, "two advisories for each of two versions");
    let top: Vec<_> = m
        .iter()
        .filter(|x| x.installed_version == "1.2.0")
        .collect();
    assert_eq!(top.len(), 2);
    assert!(
        top.iter()
            .all(|x| x.relationship == Relationship::Direct && x.scope == Scope::Runtime)
    );
    let nested: Vec<_> = m
        .iter()
        .filter(|x| x.installed_version == "0.0.8")
        .collect();
    assert!(
        nested
            .iter()
            .all(|x| x.relationship == Relationship::Indirect && x.scope == Scope::Development)
    );
    assert_eq!(
        nested[0].dependency_path,
        vec![
            "pkg:npm/mocha@5.2.0".to_owned(),
            "pkg:npm/minimist@0.0.8".to_owned()
        ]
    );
    // Severity comes from the vector, and the label is the fallback only without one.
    let crit = top
        .iter()
        .find(|x| x.advisory_id == "GHSA-xvch-5gv4-984h")
        .unwrap();
    assert_eq!(crit.severity, Severity::Critical);
    assert_eq!(crit.cvss_score, Some(9.8));
    assert_eq!(
        top.iter()
            .map(|x| x.fixed_versions.clone())
            .collect::<Vec<_>>()
            .concat()
            .len(),
        2
    );
    assert_eq!(highest_severity(&f), Severity::Critical);
}

#[test]
fn other_ecosystems_match_with_native_version_ordering() {
    let go = findings(&fixture_graph("go"));
    let text = find(&go, "golang.org/x/text");
    assert_eq!(text.len(), 2);
    assert!(
        text.iter()
            .all(|f| f.relationship == Relationship::Indirect)
    );
    assert!(
        find(&go, "gopkg.in/yaml.v3").is_empty(),
        "no advisory for yaml.v3 in the fixtures"
    );

    let py = findings(&fixture_graph("python-req"));
    assert_eq!(
        find(&py, "requests")[0].fixed_versions,
        vec!["2.31.0".to_owned()]
    );
    assert_eq!(
        find(&py, "urllib3")[0].fixed_versions,
        vec!["1.26.5".to_owned()]
    );
    assert!(find(&py, "Django").is_empty());

    let mvn = findings(&fixture_graph("maven"));
    let log4j = find(&mvn, "org.apache.logging.log4j:log4j-core");
    assert_eq!(
        log4j.len(),
        2,
        "CVE-2021-44228 and CVE-2021-45046 are different vulnerabilities"
    );
    assert!(log4j.iter().all(|f| f.installed_version == "2.14.1"));
    let first = log4j
        .iter()
        .find(|f| f.advisory_id == "GHSA-jfh8-c2jp-5v3q")
        .unwrap();
    assert_eq!(
        first.fixed_versions,
        vec!["2.15.0".to_owned()],
        "2.12.2 and 2.3.1 are older than 2.14.1"
    );
    assert_eq!(first.severity, Severity::Critical);
    assert_eq!(first.cvss_score, Some(10.0));

    let gems = findings(&fixture_graph("ruby"));
    let rack = find(&gems, "rack");
    assert_eq!(rack[0].fixed_versions, vec!["2.2.3.1".to_owned()]);
    assert_eq!(
        rack[0].severity,
        Severity::Critical,
        "label fallback without a CVSS vector"
    );
    assert_eq!(rack[0].cvss_score, None);

    let nu = findings(&fixture_graph("nuget"));
    assert_eq!(
        find(&nu, "Newtonsoft.Json").len(),
        2,
        "one per declaring file: the lock and the csproj"
    );
}

#[test]
fn fixed_installs_do_not_match() {
    let files = [SourceFile::new(
        "package.json",
        r#"{"dependencies":{"lodash":"4.17.21","minimist":"1.2.6"}}"#,
    )];
    assert!(findings(&graph(&files)).is_empty());
}

#[test]
fn explicit_version_lists_match_github_actions() {
    let mut s = FileAdvisorySource::new();
    s.load_json_str(
        "a.json",
        r#"{"id":"GHSA-mrrh-fwg8-r2c3","summary":"changed-files leaks secrets","affected":[{"package":{"ecosystem":"GitHub Actions","name":"tj-actions/changed-files"},"versions":["v35","v44.5.7"]}],"database_specific":{"severity":"HIGH"}}"#,
    )
    .unwrap();
    let wf = "jobs:\n  a:\n    steps:\n      - uses: tj-actions/changed-files@v35\n      - uses: tj-actions/changed-files@v46\n";
    let g = graph(&[SourceFile::new(".github/workflows/ci.yml", wf)]);
    let f = match_graph(&g, &s, &MatchOptions::default());
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].installed_version, "v35");
    assert_eq!(f[0].severity, Severity::High);
}

#[test]
fn matching_is_deterministic() {
    let mut files = fixture_files("npm-v1");
    files.extend(fixture_files("go").into_iter().map(|mut f| {
        f.path = format!("svc/{}", f.path);
        f
    }));
    let a = findings(&graph(&files));
    files.reverse();
    let b = findings(&graph(&files));
    assert_eq!(a, b);
    let mut sorted = a.clone();
    sorted.sort_by(|x, y| {
        (&x.manifest_path, &x.purl, &x.advisory_id).cmp(&(
            &y.manifest_path,
            &y.purl,
            &y.advisory_id,
        ))
    });
    assert_eq!(a, sorted);
}

#[test]
fn file_source_lookup_by_alias_and_dir_loading() {
    let s = osv();
    assert_eq!(s.get("CVE-2021-44228").unwrap().id, "GHSA-jfh8-c2jp-5v3q");
    assert_eq!(s.candidates(Ecosystem::Npm, "minimist").len(), 2);
    assert_eq!(s.candidates(Ecosystem::Go, "golang.org/x/text").len(), 2);
    assert_eq!(s.len(), 13);
}

// ---------------------------------------------------------------------------------------------
// Dependency review
// ---------------------------------------------------------------------------------------------

/// A package-lock v3 with `(name, version, license, dev)` entries, all declared at the root.
fn lock(entries: &[(&str, &str, Option<&str>, bool)]) -> Vec<SourceFile> {
    let mut deps = String::new();
    let mut dev = String::new();
    let mut pk = String::new();
    for (n, v, l, d) in entries {
        let target = if *d { &mut dev } else { &mut deps };
        if !target.is_empty() {
            target.push(',');
        }
        target.push_str(&format!("\"{n}\":\"{v}\""));
        let lic = l.map_or(String::new(), |l| format!(",\"license\":\"{l}\""));
        pk.push_str(&format!(
            ",\"node_modules/{n}\":{{\"version\":\"{v}\"{}{lic}}}",
            if *d { ",\"dev\":true" } else { "" }
        ));
    }
    let text = format!(
        "{{\"lockfileVersion\":3,\"packages\":{{\"\":{{\"dependencies\":{{{deps}}},\"devDependencies\":{{{dev}}}}}{pk}}}}}"
    );
    vec![SourceFile::new("package-lock.json", &text)]
}

fn run_review(
    base: &[SourceFile],
    head: &[SourceFile],
    policy: &ReviewPolicy,
) -> bee_deps::review::DependencyReview {
    review(
        &graph(base),
        &graph(head),
        &osv(),
        policy,
        &MatchOptions::default(),
    )
}

#[test]
fn review_reports_added_removed_changed_with_new_vulnerabilities() {
    let base = lock(&[
        ("lodash", "4.17.21", Some("MIT"), false),
        ("left-pad", "1.3.0", Some("WTFPL"), false),
    ]);
    let head = lock(&[
        ("lodash", "4.17.20", Some("MIT"), false),
        ("minimist", "1.2.0", Some("MIT"), false),
    ]);
    let r = run_review(&base, &head, &ReviewPolicy::default());
    let by = |n: &str| r.changes.iter().find(|c| c.name == n).unwrap();
    assert_eq!(by("lodash").change_type, ChangeType::Changed);
    assert_eq!(by("lodash").base_version.as_deref(), Some("4.17.21"));
    assert_eq!(by("lodash").head_version.as_deref(), Some("4.17.20"));
    assert_eq!(by("lodash").vulnerabilities.len(), 1);
    assert_eq!(by("minimist").change_type, ChangeType::Added);
    assert_eq!(by("minimist").vulnerabilities.len(), 2);
    assert_eq!(by("left-pad").change_type, ChangeType::Removed);
    assert!(by("left-pad").vulnerabilities.is_empty());
    assert_eq!(r.vulnerable_additions, 2);
    assert_eq!(r.highest_severity, Severity::Critical);
    assert!(!r.passes);
    assert_eq!(r.reasons.len(), 3);
    let check = r.to_check_result();
    assert_eq!(check.conclusion, Conclusion::Failure);
    assert!(check.title.contains("failed"));
    assert!(check.summary.contains("GHSA-xvch-5gv4-984h"));
    assert!(check.summary.contains("| Added | minimist |"));
}

#[test]
fn review_of_an_upgrade_passes_and_lists_fixed_advisories() {
    let base = lock(&[("lodash", "4.17.20", Some("MIT"), false)]);
    let head = lock(&[("lodash", "4.17.21", Some("MIT"), false)]);
    let r = run_review(&base, &head, &ReviewPolicy::default());
    assert!(r.passes);
    assert_eq!(r.changes.len(), 1);
    assert_eq!(
        r.changes[0].fixed_advisories,
        vec!["GHSA-35jh-r3h4-6jhm".to_owned()]
    );
    assert_eq!(r.to_check_result().conclusion, Conclusion::Success);
    assert!(
        r.to_check_result()
            .summary
            .contains("fixes GHSA-35jh-r3h4-6jhm")
    );
}

#[test]
fn a_vulnerability_already_present_at_base_is_not_new() {
    let base = lock(&[("minimist", "1.2.0", None, false)]);
    let head = lock(&[("minimist", "1.2.2", None, false)]);
    let r = run_review(&base, &head, &ReviewPolicy::default());
    assert!(
        r.changes[0].vulnerabilities.is_empty(),
        "both versions are in the advisory ranges"
    );
    assert!(r.passes);
    assert_eq!(r.vulnerable_additions, 0);
}

#[test]
fn review_policy_severity_scope_and_licenses() {
    let base = lock(&[]);
    let dev_vuln = lock(&[("minimist", "1.2.0", Some("MIT"), true)]);
    let default = run_review(&base, &dev_vuln, &ReviewPolicy::default());
    assert!(
        default.passes,
        "development dependencies are ignored by default"
    );
    assert_eq!(default.vulnerable_additions, 1);
    let strict = ReviewPolicy {
        include_development: true,
        ..ReviewPolicy::default()
    };
    assert!(!run_review(&base, &dev_vuln, &strict).passes);
    let critical_only = ReviewPolicy {
        fail_on_severity: Some(Severity::Critical),
        include_development: true,
        ..ReviewPolicy::default()
    };
    assert!(
        !run_review(&base, &dev_vuln, &critical_only).passes,
        "minimist 1.2.0 has a critical advisory"
    );
    let moderate = lock(&[("lodash", "4.17.20", None, false)]);
    let crit = ReviewPolicy {
        fail_on_severity: Some(Severity::Critical),
        ..ReviewPolicy::default()
    };
    assert!(
        run_review(&base, &moderate, &crit).passes,
        "a high advisory is below the critical threshold"
    );
    let never = ReviewPolicy {
        fail_on_severity: None,
        ..ReviewPolicy::default()
    };
    assert!(run_review(&base, &moderate, &never).passes);

    // Licenses.
    let gpl = lock(&[
        ("copyleft", "1.0.0", Some("GPL-3.0-only"), false),
        ("either", "1.0.0", Some("MIT OR GPL-3.0-only"), false),
    ]);
    let deny = ReviewPolicy {
        deny_licenses: vec!["gpl-3.0-only".into()],
        ..ReviewPolicy::default()
    };
    let r = run_review(&base, &gpl, &deny);
    assert!(
        r.changes
            .iter()
            .find(|c| c.name == "copyleft")
            .unwrap()
            .license_denied
    );
    assert!(
        !r.changes
            .iter()
            .find(|c| c.name == "either")
            .unwrap()
            .license_denied,
        "MIT satisfies the OR"
    );
    assert!(!r.passes);
    assert!(r.reasons[0].contains("disallowed license"));
    let allow = ReviewPolicy {
        allow_licenses: vec!["MIT".into(), "Apache-2.0".into()],
        ..ReviewPolicy::default()
    };
    let r = run_review(&base, &gpl, &allow);
    assert!(
        r.changes
            .iter()
            .find(|c| c.name == "copyleft")
            .unwrap()
            .license_denied
    );
    assert!(
        !r.changes
            .iter()
            .find(|c| c.name == "either")
            .unwrap()
            .license_denied
    );
    let unknown = lock(&[("mystery", "1.0.0", None, false)]);
    let strict_unknown = ReviewPolicy {
        allow_licenses: vec!["MIT".into()],
        deny_unknown_licenses: true,
        ..ReviewPolicy::default()
    };
    assert!(!run_review(&base, &unknown, &strict_unknown).passes);
    assert!(run_review(&base, &unknown, &allow).passes);
}

#[test]
fn review_detects_license_changes() {
    let base = lock(&[("pkg", "1.0.0", Some("MIT"), false)]);
    let head = lock(&[("pkg", "2.0.0", Some("AGPL-3.0-only"), false)]);
    let r = run_review(&base, &head, &ReviewPolicy::default());
    let c = &r.changes[0];
    assert!(c.license_changed);
    assert_eq!(c.base_licenses, vec!["MIT".to_owned()]);
    assert_eq!(c.licenses, vec!["AGPL-3.0-only".to_owned()]);
    assert!(r.to_check_result().summary.contains("license changed"));
    let same = lock(&[("pkg", "2.0.0", Some("MIT"), false)]);
    assert!(!run_review(&base, &same, &ReviewPolicy::default()).changes[0].license_changed);
}

#[test]
fn identical_graphs_review_clean() {
    let g = lock(&[("lodash", "4.17.20", Some("MIT"), false)]);
    let r = run_review(&g, &g, &ReviewPolicy::default());
    assert!(r.changes.is_empty());
    assert!(r.passes);
    assert_eq!(r.highest_severity, Severity::None);
    assert!(r.to_check_result().title.contains("0 added"));
}

// ---------------------------------------------------------------------------------------------
// Update planning
// ---------------------------------------------------------------------------------------------

fn plan_for(files: &[SourceFile]) -> (DependencyGraph, Vec<Finding>, bee_deps::update::UpdatePlan) {
    let g = graph(files);
    let s = osv();
    let f = match_graph(&g, &s, &MatchOptions::default());
    let p = plan_security_updates(files, &g, &f, &s);
    (g, f, p)
}

/// AS-SEC-01: a known-vulnerable dependency raises an alert, the planner proposes an update that
/// a bot can land, and after the update the alert is gone.
#[test]
fn as_sec_01_vulnerable_dependency_raises_alert_and_update() {
    let files = vec![
        SourceFile::new(
            "package.json",
            "{\n  \"name\": \"shop\",\n  \"dependencies\": {\n    \"lodash\": \"4.17.20\"\n  }\n}\n",
        ),
        SourceFile::new("README.md", "docs"),
    ];
    // 1. The alert.
    let (g, alerts, plan) = plan_for(&files);
    assert_eq!(alerts.len(), 1);
    let alert = &alerts[0];
    assert_eq!(alert.advisory_id, "GHSA-35jh-r3h4-6jhm");
    assert!(alert.severity >= Severity::High);
    assert_eq!(alert.fixed_versions, vec!["4.17.21".to_owned()]);
    assert_eq!(alert.relationship, Relationship::Direct);
    // 2. The update change request content.
    assert!(plan.unfixable.is_empty());
    assert_eq!(plan.groups.len(), 1);
    let group = &plan.groups[0];
    assert_eq!(group.ecosystem, Ecosystem::Npm);
    assert!(group.title.contains("lodash"));
    let u = &group.updates[0];
    assert_eq!((u.from.as_str(), u.to.as_str()), ("4.17.20", "4.17.21"));
    assert_eq!(u.advisories, vec!["GHSA-35jh-r3h4-6jhm".to_owned()]);
    let edits = group.edits();
    assert_eq!(edits.len(), 1);
    assert_eq!(
        (
            edits[0].path.as_str(),
            edits[0].old.as_str(),
            edits[0].new.as_str()
        ),
        ("package.json", "4.17.20", "4.17.21")
    );
    // 3. Landing the update closes the alert.
    let updated = apply_plan(&files, &plan).unwrap();
    assert_eq!(updated.len(), 1);
    assert!(updated[0].content.contains("\"lodash\": \"4.17.21\""));
    let mut after = files.clone();
    after[0] = updated[0].clone();
    let g2 = graph(&after);
    assert!(
        match_graph(&g2, &osv(), &MatchOptions::default()).is_empty(),
        "the alert is resolved"
    );
    // 4. The dependency review of the update change passes and credits the fix.
    let r = review(
        &g,
        &g2,
        &osv(),
        &ReviewPolicy::default(),
        &MatchOptions::default(),
    );
    assert!(r.passes);
    assert_eq!(
        r.changes[0].fixed_advisories,
        vec!["GHSA-35jh-r3h4-6jhm".to_owned()]
    );
}

#[test]
fn npm_with_lockfile_and_admitting_range_needs_only_a_lock_refresh() {
    let files = fixture_files("npm-v1");
    let (_, _, plan) = plan_for(&files);
    let g = &plan.groups[0];
    let top = g.updates.iter().find(|u| u.from == "1.2.0").unwrap();
    assert_eq!(
        top.to, "1.2.6",
        "1.2.3 would still be hit by the second advisory"
    );
    assert!(top.edits.is_empty(), "^1.2.0 already allows 1.2.6");
    assert_eq!(
        top.commands,
        vec!["npm install minimist@1.2.6 --package-lock-only".to_owned()]
    );
    assert_eq!(top.advisories.len(), 2);
    assert_eq!(top.severity, Severity::Critical);
    let nested = g.updates.iter().find(|u| u.from == "0.0.8").unwrap();
    assert_eq!(nested.to, "0.2.4");
    assert_eq!(nested.relationship, Relationship::Indirect);
    assert!(nested.notes[0].contains("transitive dependency (via pkg:npm/mocha@5.2.0"));
}

#[test]
fn npm_range_that_excludes_the_fix_is_edited_keeping_its_operator() {
    let files = vec![
        SourceFile::new(
            "package.json",
            "{\"dependencies\":{\"minimist\":\"~1.2.0\"},\"devDependencies\":{\"lodash\":\"^4.17.20\"}}",
        ),
        SourceFile::new(
            "package-lock.json",
            &lock(&[
                ("minimist", "1.2.0", None, false),
                ("lodash", "4.17.20", None, true),
            ])[0]
                .content,
        ),
    ];
    let (_, _, plan) = plan_for(&files);
    let edits = plan.edits();
    // ~1.2.0 allows 1.2.6, ^4.17.20 allows 4.17.21: only commands.
    assert!(edits.is_empty());
    assert_eq!(plan.groups[0].commands().len(), 2);
    // Pin below the fix: the manifest changes.
    let pinned = vec![
        SourceFile::new(
            "package.json",
            "{\"dependencies\":{\"minimist\":\"1.2.0\"}}",
        ),
        SourceFile::new(
            "package-lock.json",
            &lock(&[("minimist", "1.2.0", None, false)])[0].content,
        ),
    ];
    let (_, _, plan) = plan_for(&pinned);
    let edits = plan.edits();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new, "1.2.6");
    let patched = apply_plan(&pinned, &plan).unwrap();
    assert!(patched[0].content.contains("\"minimist\":\"1.2.6\""));
}

#[test]
fn complex_requirements_are_reported_not_guessed() {
    let files = vec![SourceFile::new(
        "package.json",
        "{\"dependencies\":{\"lodash\":\">=4.17.0 <4.17.21\"}}",
    )];
    let (_, f, plan) = plan_for(&files);
    assert_eq!(f.len(), 1);
    assert!(plan.groups.is_empty());
    assert_eq!(plan.unfixable.len(), 1);
    assert!(
        plan.unfixable[0]
            .reason
            .contains("not a single version comparator")
    );
    assert_eq!(
        plan.unfixable[0].advisories,
        vec!["GHSA-35jh-r3h4-6jhm".to_owned()]
    );
}

#[test]
fn go_modules_are_bumped_in_go_mod_even_when_indirect() {
    let files = fixture_files("go");
    let (_, _, plan) = plan_for(&files);
    let g = &plan.groups[0];
    assert_eq!(g.ecosystem, Ecosystem::Go);
    let u = &g.updates[0];
    assert_eq!((u.from.as_str(), u.to.as_str()), ("v0.3.6", "v0.3.8"));
    assert_eq!(u.edits.len(), 1);
    assert_eq!(u.commands, vec!["go mod tidy".to_owned()]);
    let patched = apply_plan(&files, &plan).unwrap();
    assert!(
        patched[0]
            .content
            .contains("golang.org/x/text v0.3.8 // indirect")
    );
    let mut after = files.clone();
    let i = after.iter().position(|f| f.path == "go.mod").unwrap();
    after[i] = patched[0].clone();
    assert!(findings(&graph(&after)).is_empty());
}

#[test]
fn pip_requirements_are_edited_in_place() {
    let files = fixture_files("python-req");
    let (_, f, plan) = plan_for(&files);
    assert_eq!(f.len(), 2);
    let g = &plan.groups[0];
    assert_eq!(g.ecosystem, Ecosystem::PyPI);
    let edits = g.edits();
    assert_eq!(edits.len(), 2);
    let patched = apply_plan(&files, &plan).unwrap();
    let text = &patched[0].content;
    assert!(
        text.contains("requests==2.31.0 \\\n    --hash=sha256:aaaaaaaa"),
        "{text}"
    );
    assert!(text.contains("urllib3==1.26.5\n"));
    assert!(
        text.contains("Django>=3.2,<4.0 ; python_version >= \"3.8\""),
        "unrelated lines are untouched"
    );
    let mut after = files.clone();
    let i = after
        .iter()
        .position(|f| f.path == "requirements.txt")
        .unwrap();
    after[i] = patched[0].clone();
    assert!(findings(&graph(&after)).is_empty());
}

fn cargo_files(manifest_dep: &str, with_lock: bool) -> Vec<SourceFile> {
    let mut v = vec![SourceFile::new(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nregex = {manifest_dep}\n"
        ),
    )];
    if with_lock {
        v.push(SourceFile::new(
            "Cargo.lock",
            "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"regex\",\n]\n\n[[package]]\nname = \"regex\"\nversion = \"1.5.4\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        ));
    }
    v
}

fn regex_source() -> FileAdvisorySource {
    let mut s = FileAdvisorySource::new();
    s.load_json_str(
        "regex.json",
        r#"{"id":"GHSA-m5pq-gvj9-9vr8","aliases":["CVE-2022-24713"],"summary":"Regexes with large repetitions can cause DoS",
        "severity":[{"type":"CVSS_V3","score":"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H"}],
        "affected":[{"package":{"ecosystem":"crates.io","name":"regex"},"ranges":[{"type":"SEMVER","events":[{"introduced":"0"},{"fixed":"1.5.5"}]}]}]}"#,
    )
    .unwrap();
    s
}

fn plan_cargo(files: &[SourceFile]) -> bee_deps::update::UpdatePlan {
    let g = graph(files);
    let s = regex_source();
    let f = match_graph(&g, &s, &MatchOptions::default());
    assert_eq!(f.len(), 1, "regex 1.5.4 is vulnerable");
    plan_security_updates(files, &g, &f, &s)
}

#[test]
fn cargo_plans_cover_lock_refresh_pins_and_lockless_manifests() {
    // Lock present and the caret range admits the fix: refresh the lock only.
    let p = plan_cargo(&cargo_files("\"1.5\"", true));
    let u = &p.groups[0].updates[0];
    assert!(u.edits.is_empty());
    assert_eq!(
        u.commands,
        vec!["cargo update -p regex@1.5.4 --precise 1.5.5".to_owned()]
    );
    assert_eq!(u.to, "1.5.5");

    // An exact pin has to change in Cargo.toml.
    let files = cargo_files("\"=1.5.4\"", true);
    let p = plan_cargo(&files);
    let u = &p.groups[0].updates[0];
    assert_eq!(u.edits.len(), 1);
    assert_eq!(u.edits[0].new, "=1.5.5");
    let patched = apply_plan(&files, &p).unwrap();
    assert!(patched[0].content.contains("regex = \"=1.5.5\""));

    // No lockfile: the declared minimum is what an install may resolve to, so raise it.
    let files = cargo_files("{ version = \"1.5\", default-features = false }", false);
    let p = plan_cargo(&files);
    let u = &p.groups[0].updates[0];
    assert_eq!(u.edits.len(), 1);
    assert!(u.commands.is_empty());
    let patched = apply_plan(&files, &p).unwrap();
    assert!(
        patched[0]
            .content
            .contains("version = \"1.5.5\", default-features = false")
    );
}

#[test]
fn unsupported_ecosystems_and_missing_fixes_are_unfixable() {
    let (_, f, plan) = plan_for(&fixture_files("maven"));
    assert!(!f.is_empty());
    assert!(plan.groups.is_empty());
    assert!(
        plan.unfixable
            .iter()
            .all(|u| u.reason.contains("does not support maven"))
    );

    let mut s = FileAdvisorySource::new();
    s.load_json_str(
        "a.json",
        r#"{"id":"X-1","affected":[{"package":{"ecosystem":"npm","name":"left-pad"},"ranges":[{"type":"SEMVER","events":[{"introduced":"0"}]}]}]}"#,
    )
    .unwrap();
    let files = vec![SourceFile::new(
        "package.json",
        "{\"dependencies\":{\"left-pad\":\"1.3.0\"}}",
    )];
    let g = graph(&files);
    let f = match_graph(&g, &s, &MatchOptions::default());
    let plan = plan_security_updates(&files, &g, &f, &s);
    assert_eq!(plan.unfixable.len(), 1);
    assert!(
        plan.unfixable[0]
            .reason
            .contains("no published fixed version")
    );
}

#[test]
fn updates_are_grouped_by_ecosystem_in_a_stable_order() {
    let mut files = Vec::new();
    for (dir, name) in [("web", "npm-v1"), ("svc", "go"), ("py", "python-req")] {
        for mut f in fixture_files(name) {
            f.path = format!("{dir}/{}", f.path);
            files.push(f);
        }
    }
    let (_, _, plan) = plan_for(&files);
    let ecos: Vec<Ecosystem> = plan.groups.iter().map(|g| g.ecosystem).collect();
    assert_eq!(ecos, vec![Ecosystem::Npm, Ecosystem::PyPI, Ecosystem::Go]);
    assert!(
        plan.groups
            .iter()
            .all(|g| !g.updates.is_empty() && g.title.starts_with("Security: bump"))
    );
    for g in &plan.groups {
        let edits = g.edits();
        assert!(
            edits
                .windows(2)
                .all(|w| (&w[0].path, w[0].start) <= (&w[1].path, w[1].start))
        );
        assert!(edits.iter().all(|e| e.path.starts_with(match g.ecosystem {
            Ecosystem::Npm => "web/",
            Ecosystem::Go => "svc/",
            _ => "py/",
        })));
    }
    let again = plan_for(&files).2;
    assert_eq!(plan, again, "planning is deterministic");
    let patched = apply_plan(&files, &plan).unwrap();
    assert_eq!(
        patched.len(),
        2,
        "go.mod and requirements.txt change; npm only needs lock commands"
    );
}
