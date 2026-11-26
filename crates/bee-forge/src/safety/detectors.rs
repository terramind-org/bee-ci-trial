//! Deterministic signals over evidence collected from forge storage.

use std::collections::{BTreeMap, BTreeSet};

use bee_proto::v1::*;
use prost::Message;

use crate::agents::context::instruction_source;
use crate::changes::model::PathClass;
use crate::ci::engine::is_ci_workflow_path;
use crate::ids::ts;

pub(super) fn verdict(findings: &[SafetyFinding], has_unfinished_tasks: bool) -> SafetyVerdict {
    let mut outside = false;
    let mut unfinished = has_unfinished_tasks;
    for finding in findings {
        if finding.state() == SafetyFindingState::Dismissed {
            continue;
        }
        let Some(report) = &finding.report else {
            continue;
        };
        match (report.kind(), report.severity()) {
            (SafetySignalKind::OutsideTask, SafetySeverity::Note) => outside = true,
            (SafetySignalKind::SaidVsShown, SafetySeverity::Note) => unfinished = true,
            _ => return SafetyVerdict::NeedsALook,
        }
    }
    match (outside, unfinished) {
        (true, true) => SafetyVerdict::SomethingElse,
        (true, false) => SafetyVerdict::MoreThanAsked,
        (false, true) => SafetyVerdict::LessThanAsked,
        (false, false) => SafetyVerdict::AsAsked,
    }
}

pub(super) fn finding(
    kind: SafetySignalKind,
    severity: SafetySeverity,
    observation: impl Into<String>,
    subjects: Vec<SubjectRef>,
    paths: Vec<String>,
    time_ms: u64,
) -> SafetyFinding {
    let report = SafetyFindingReport {
        kind: kind as i32,
        severity: severity as i32,
        evidence: Some(SafetyEvidence {
            observation: observation.into(),
            subjects,
            paths,
        }),
    };
    SafetyFinding {
        id: format!("det-{}", blake3::hash(&report.encode_to_vec()).to_hex()),
        report: Some(report),
        state: SafetyFindingState::Open as i32,
        created_at: Some(ts(time_ms)),
        ..Default::default()
    }
}

pub(super) fn outside_paths(paths: &[PathClass], claims: &[Vec<u8>]) -> Vec<String> {
    if claims.is_empty() {
        return Vec::new();
    }
    paths
        .iter()
        .filter(|p| {
            !claims.iter().any(|claim| {
                let claim = claim.strip_prefix(b"/").unwrap_or(claim);
                let claim = claim.strip_suffix(b"/").unwrap_or(claim);
                let path = p.path.as_bytes();
                claim.is_empty()
                    || path == claim
                    || path
                        .strip_prefix(claim)
                        .is_some_and(|tail| tail.starts_with(b"/"))
            })
        })
        .map(|p| p.path.clone())
        .collect()
}

fn skip_marker(line: &str, test_path: bool) -> bool {
    let line = line.trim();
    if line.starts_with("#[ignore]")
        || line.starts_with("#[ignore =")
        || line.starts_with("#[ignore=")
    {
        return true;
    }
    test_path
        && [
            "describe.skip(",
            "test.skip(",
            "it.skip(",
            "xdescribe(",
            "xit(",
            "xtest(",
            "@pytest.mark.skip",
            "@unittest.skip",
            "t.Skip(",
            "t.Skipf(",
            "t.SkipNow(",
        ]
        .iter()
        .any(|marker| line.starts_with(marker))
}

/// A changed workflow, a deleted classified test, or more skip markers in the new file.
/// Identical markers moved within the diff cancel out.
pub(super) fn weakened_tests(paths: &[PathClass], diff: &[DiffItem]) -> Vec<String> {
    let tests: BTreeSet<&str> = paths
        .iter()
        .filter(|path| path.class == "test_only")
        .map(|path| path.path.as_str())
        .collect();
    let mut weakened: BTreeSet<String> = paths
        .iter()
        .filter(|p| (p.deleted && tests.contains(p.path.as_str())) || is_ci_workflow_path(&p.path))
        .map(|p| p.path.clone())
        .collect();
    for item in diff {
        let path = String::from_utf8_lossy(&item.path);
        let old_path = String::from_utf8_lossy(&item.old_path);
        let test_path = tests.contains(path.as_ref()) || tests.contains(old_path.as_ref());
        if is_ci_workflow_path(&old_path) {
            weakened.insert(old_path.into_owned());
        }
        let mut markers = BTreeMap::<&str, i64>::new();
        for line in item.hunks.iter().flat_map(|h| &h.lines) {
            if let Some(body) = line.strip_prefix('+').filter(|s| skip_marker(s, test_path)) {
                *markers.entry(body.trim()).or_default() += 1;
            }
            if let Some(body) = line.strip_prefix('-').filter(|s| skip_marker(s, test_path)) {
                *markers.entry(body.trim()).or_default() -= 1;
            }
        }
        if markers.values().any(|count| *count > 0) {
            weakened.insert(path.into_owned());
        }
    }
    weakened.into_iter().collect()
}

pub(super) fn leash_paths(paths: &[PathClass], configured: &str) -> Vec<String> {
    paths
        .iter()
        .filter(|p| {
            p.path == ".beehive/policy.toml"
                || p.path == configured.trim_start_matches('/')
                || instruction_source(&p.path).is_some()
        })
        .map(|p| p.path.clone())
        .collect()
}

pub(super) const LEASH_ACTIONS: &[&str] = &[
    "agent.budget_set",
    "agent.updated",
    "agent.deleted",
    "policy.update",
    "ruleset.create",
    "ruleset.update",
    "ruleset.delete",
    "actions_policy.update",
    "actions_policy.enabled_repos",
    "workflow.required_create",
    "workflow.required_update",
    "workflow.required_delete",
    "workspace.org_policy_set",
    "workspace.egress_set",
    "role.create",
    "role.update",
    "role.delete",
    "role.assign",
    "role.unassign",
    "collaborator.add",
    "collaborator.remove",
    "team.repo_grant",
    "team.repo_revoke",
    "deploy_key.create",
    "deploy_key.delete",
    "token.mint",
    "token.revoke",
    "token.create",
    "app.permissions_accept",
    "app.token_create",
];

pub(super) fn leash_action(entry: &AuditEntry, token_id: &str) -> bool {
    !token_id.is_empty()
        && entry.token_id == token_id
        && entry.result == "ok"
        && LEASH_ACTIONS.contains(&entry.action.as_str())
}

pub(super) struct Attempt {
    pub id: String,
    pub effect: String,
    pub reason: ErrorReason,
    pub time_ms: u64,
}

pub(super) fn repeated_after_refusal(attempts: &[Attempt]) -> Vec<(&Attempt, &Attempt)> {
    let mut pending = BTreeMap::new();
    let mut reported = BTreeSet::new();
    let mut results = Vec::new();
    for attempt in attempts {
        if reported.contains(attempt.effect.as_str()) {
            continue;
        }
        if let Some(first) = pending.remove(attempt.effect.as_str()) {
            results.push((first, attempt));
            reported.insert(attempt.effect.as_str());
        } else if matches!(
            attempt.reason,
            ErrorReason::PolicyDenied | ErrorReason::ApprovalRequired
        ) {
            pending.insert(attempt.effect.as_str(), attempt);
        }
    }
    results
}

pub(super) fn missing_checks(
    required: &BTreeSet<String>,
    observed: &[bee_policy::CheckResult],
    workflows: &[bee_policy::WorkflowGate],
) -> Vec<String> {
    use bee_policy::CheckStatus::{Neutral, Skipped, Success};
    let mut missing: BTreeSet<String> = required
        .iter()
        .filter(|name| {
            !observed.iter().any(|check| {
                check.name == **name && matches!(check.status, Success | Neutral | Skipped)
            })
        })
        .cloned()
        .collect();
    missing.extend(
        workflows
            .iter()
            .filter(|w| !w.passed)
            .map(|w| w.name.clone()),
    );
    missing.into_iter().collect()
}

pub(super) fn scan_matches(
    alert: &CodeScanningAlert,
    instance: &CodeScanningAlertInstance,
    head_ids: &BTreeSet<Vec<u8>>,
) -> bool {
    alert.state() == CodeScanningAlertState::Open
        && instance.state() == CodeScanningAlertState::Open
        && instance
            .changeset
            .as_ref()
            .is_some_and(|id| head_ids.contains(&id.raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str, class: &str, deleted: bool) -> PathClass {
        PathClass {
            path: name.into(),
            class: class.into(),
            deleted,
            ..Default::default()
        }
    }

    #[test]
    fn safety_outside_task_requires_a_component_boundary_and_claim() {
        let paths = [
            path("src/api.rs", "logic", false),
            path("src2/main.rs", "logic", false),
        ];
        assert_eq!(outside_paths(&paths, &[b"src".to_vec()]), ["src2/main.rs"]);
        assert!(outside_paths(&paths, &[b"/".to_vec()]).is_empty());
        assert!(outside_paths(&paths, &[]).is_empty());
    }

    #[test]
    fn safety_around_refusal_requires_the_same_effect_after_a_real_refusal() {
        let attempts = [
            Attempt {
                id: "1".into(),
                effect: "a".into(),
                reason: ErrorReason::PolicyDenied,
                time_ms: 1,
            },
            Attempt {
                id: "2".into(),
                effect: "b".into(),
                reason: ErrorReason::Unspecified,
                time_ms: 2,
            },
            Attempt {
                id: "3".into(),
                effect: "c".into(),
                reason: ErrorReason::VersionMismatch,
                time_ms: 3,
            },
            Attempt {
                id: "4".into(),
                effect: "c".into(),
                reason: ErrorReason::Unspecified,
                time_ms: 4,
            },
            Attempt {
                id: "5".into(),
                effect: "a".into(),
                reason: ErrorReason::Unspecified,
                time_ms: 5,
            },
        ];
        assert!(repeated_after_refusal(&attempts[..4]).is_empty());
        let repeated = repeated_after_refusal(&attempts);
        assert_eq!(repeated.len(), 1);
        assert_eq!((&repeated[0].0.id[..], &repeated[0].1.id[..]), ("1", "5"));
    }

    #[test]
    fn safety_detects_only_classified_test_deletions_and_added_skip_markers() {
        let paths = [
            path("tests/removed.rs", "test_only", true),
            path("tests/moved.rs", "test_only", false),
            path("src/api.rs", "logic", true),
            path("src/tests.rs", "logic", false),
            path(".github/workflows/ci.yml", "logic", false),
        ];
        let item = |path: &str, lines: &[&str]| DiffItem {
            path: path.as_bytes().to_vec(),
            hunks: vec![DiffHunk {
                lines: lines.iter().map(|s| (*s).into()).collect(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let diff = [
            item("tests/moved.rs", &["-#[ignore]", "+  #[ignore]"]),
            item(
                "src/tests.rs",
                &["+   #[ignore = \"unreliable\"]", "+   // #[ignore]"],
            ),
        ];
        assert_eq!(
            weakened_tests(&paths, &diff),
            [
                ".github/workflows/ci.yml",
                "src/tests.rs",
                "tests/removed.rs"
            ]
        );
    }

    #[test]
    fn safety_detects_configured_instructions_and_verified_successful_rule_mutations() {
        let paths = [
            path(".beehive/policy.toml", "sensitive", false),
            path("src/AGENTS.md", "docs", false),
            path("agent.txt", "docs", false),
            path("docs/policy.toml", "docs", false),
        ];
        assert_eq!(
            leash_paths(&paths, "agent.txt"),
            [".beehive/policy.toml", "src/AGENTS.md", "agent.txt"]
        );
        let mut entry = AuditEntry {
            action: "agent.budget_set".into(),
            token_id: "token".into(),
            result: "ok".into(),
            ..Default::default()
        };
        assert!(leash_action(&entry, "token"));
        assert!(!leash_action(&entry, "another-token"));
        entry.result = "PERMISSION_DENIED".into();
        assert!(!leash_action(&entry, "token"));
    }

    #[test]
    fn safety_said_vs_shown_uses_only_required_checks_and_policy_pass_rules() {
        use bee_policy::CheckStatus::{Failure, Neutral};
        let required = BTreeSet::from(["build".into(), "missing".into(), "tests".into()]);
        let reported = [
            bee_policy::CheckResult {
                name: "build".into(),
                status: Neutral,
            },
            bee_policy::CheckResult {
                name: "tests".into(),
                status: Failure,
            },
            bee_policy::CheckResult {
                name: "optional".into(),
                status: Failure,
            },
        ];
        let workflow = [bee_policy::WorkflowGate {
            name: "required workflow".into(),
            passed: false,
        }];
        assert_eq!(
            missing_checks(&required, &reported, &workflow),
            ["missing", "required workflow", "tests"]
        );
        assert!(missing_checks(&BTreeSet::from(["build".into()]), &reported, &[]).is_empty());
    }

    #[test]
    fn safety_harm_to_code_checks_open_alert_and_exact_revision() {
        let alert = CodeScanningAlert {
            state: CodeScanningAlertState::Open as i32,
            ..Default::default()
        };
        let mut instance = CodeScanningAlertInstance {
            state: CodeScanningAlertState::Open as i32,
            changeset: Some(ObjectId { raw: vec![1] }),
            ..Default::default()
        };
        assert!(scan_matches(&alert, &instance, &BTreeSet::from([vec![1]])));
        assert!(!scan_matches(&alert, &instance, &BTreeSet::from([vec![2]])));
        instance.state = CodeScanningAlertState::Fixed as i32;
        assert!(!scan_matches(&alert, &instance, &BTreeSet::from([vec![1]])));
    }

    #[test]
    fn safety_verdict_uses_severity_and_active_findings() {
        let mut outside = finding(
            SafetySignalKind::OutsideTask,
            SafetySeverity::Note,
            "Outside",
            vec![],
            vec![],
            0,
        );
        assert_eq!(verdict(&[], false), SafetyVerdict::AsAsked);
        assert_eq!(
            verdict(std::slice::from_ref(&outside), false),
            SafetyVerdict::MoreThanAsked
        );
        assert_eq!(
            verdict(std::slice::from_ref(&outside), true),
            SafetyVerdict::SomethingElse
        );
        outside.state = SafetyFindingState::Dismissed as i32;
        assert_eq!(verdict(&[outside], true), SafetyVerdict::LessThanAsked);
        let severe = finding(
            SafetySignalKind::OwnLeash,
            SafetySeverity::Severe,
            "Rules",
            vec![],
            vec![],
            0,
        );
        assert_eq!(verdict(&[severe], false), SafetyVerdict::NeedsALook);
    }
}
