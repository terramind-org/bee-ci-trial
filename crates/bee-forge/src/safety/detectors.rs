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
            (SafetySignalKind::HarmToCode, SafetySeverity::Note)
                if finding.reported_by.is_none() => {}
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
        id: subject_id(
            kind,
            report
                .evidence
                .as_ref()
                .map_or(&[], |e| e.subjects.as_slice()),
            "",
        ),
        report: Some(report),
        state: SafetyFindingState::Open as i32,
        created_at: Some(ts(time_ms)),
        ..Default::default()
    }
}

pub(super) fn subject_id(
    kind: SafetySignalKind,
    subjects: &[SubjectRef],
    identity: &str,
) -> String {
    let mut hash = blake3::Hasher::new_derive_key("Beehive safety finding subject");
    hash.update(&(kind as i32).to_be_bytes());
    hash.update(&(identity.len() as u64).to_be_bytes());
    hash.update(identity.as_bytes());
    for subject in subjects {
        let bytes = subject.encode_to_vec();
        hash.update(&(bytes.len() as u64).to_be_bytes());
        hash.update(&bytes);
    }
    format!("det-v2-{}", hash.finalize().to_hex())
}

pub(super) fn automatic(finding: &SafetyFinding) -> bool {
    finding.reported_by.is_none() && finding.id.starts_with("det-")
}

fn legacy_subject(finding: &SafetyFinding, candidate: &SafetyFinding) -> bool {
    if !automatic(finding) || finding.id.starts_with("det-v2-") {
        return false;
    }
    let (Some(previous), Some(next)) = (&finding.report, &candidate.report) else {
        return false;
    };
    if previous.kind != next.kind {
        return false;
    }
    let (Some(previous), Some(next)) = (&previous.evidence, &next.evidence) else {
        return false;
    };
    previous.subjects == next.subjects
        && (matches!(
            candidate.report.as_ref().map(SafetyFindingReport::kind),
            Some(
                SafetySignalKind::OutsideTask
                    | SafetySignalKind::SaidVsShown
                    | SafetySignalKind::TestsWeakened
                    | SafetySignalKind::HarmToCode
            )
        ) || (previous.observation == next.observation && previous.paths == next.paths))
}

pub(super) fn reconcile(
    existing: &mut Vec<SafetyFinding>,
    observed: Vec<SafetyFinding>,
) -> (Vec<SafetyFinding>, Vec<String>) {
    let mut added = Vec::new();
    let mut active = BTreeSet::new();
    for finding in observed {
        active.insert(finding.id.clone());
        if let Some(old) = existing
            .iter_mut()
            .find(|old| old.id == finding.id || legacy_subject(old, &finding))
        {
            // Preserve the human decision and original observation time.
            old.id.clone_from(&finding.id);
            old.report = finding.report;
        } else {
            added.push(finding.clone());
            existing.push(finding);
        }
    }
    let mut retired = Vec::new();
    existing.retain(|finding| {
        let remove = automatic(finding)
            && finding.state() == SafetyFindingState::Open
            && !active.contains(&finding.id);
        if remove {
            retired.push(finding.id.clone());
        }
        !remove
    });
    (added, retired)
}

pub(super) fn outside_paths(paths: &[PathClass], claims: &[Vec<u8>]) -> Vec<String> {
    if claims.is_empty() {
        return Vec::new();
    }
    paths
        .iter()
        .filter(|p| !claimed_path(&p.path, claims))
        .map(|p| p.path.clone())
        .collect()
}

pub(super) fn claimed_path(path: &str, claims: &[Vec<u8>]) -> bool {
    claims.iter().any(|claim| {
        let claim = claim.strip_prefix(b"/").unwrap_or(claim);
        let claim = claim.strip_suffix(b"/").unwrap_or(claim);
        let path = path.as_bytes();
        claim.is_empty()
            || path == claim
            || path
                .strip_prefix(claim)
                .is_some_and(|tail| tail.starts_with(b"/"))
    })
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

// Use the same action constants at mutation sites and when reading the audit log.
macro_rules! leash_actions {
    ($($name:ident = $value:literal),+ $(,)?) => {
        /// Audit actions observed by the own-leash detector.
        pub mod audit_actions {
            $(#[doc = $value] pub const $name: &str = $value;)+
        }
        pub(super) const LEASH_ACTIONS: &[&str] = &[$(audit_actions::$name),+];
    };
}

leash_actions! {
    MINT_TOKEN = "mint_token",
    REVOKE_TOKEN = "revoke",
    CREATE_AGENT_CREDENTIAL = "create_agent",
    UPDATE_AGENT_CREDENTIAL = "update_agent",
    AGENT_BUDGET = "agent.budget_set",
    AGENT_UPDATE = "agent.update",
    AGENT_DELETE = "agent.delete",
    RULESET_CREATE = "ruleset.create",
    RULESET_UPDATE = "ruleset.update",
    RULESET_DELETE = "ruleset.delete",
    ACTIONS_POLICY_UPDATE = "actions_policy.update",
    ACTIONS_POLICY_ENABLED_REPOS = "actions_policy.enabled_repos",
    WORKFLOW_REQUIRED_CREATE = "workflow.required_create",
    WORKFLOW_REQUIRED_UPDATE = "workflow.required_update",
    WORKFLOW_REQUIRED_DELETE = "workflow.required_delete",
    WORKSPACE_ORG_POLICY = "workspace.org_policy_set",
    WORKSPACE_EGRESS = "workspace.egress_set",
    WORKSPACE_DOTFILES = "workspace.dotfiles_set",
    ROLE_CREATE = "role.create",
    ROLE_UPDATE = "role.update",
    ROLE_DELETE = "role.delete",
    ROLE_ASSIGN = "role.assign",
    ROLE_UNASSIGN = "role.unassign",
    COLLABORATOR_ADD = "collaborator.add",
    COLLABORATOR_REMOVE = "collaborator.remove",
    TEAM_REPO_GRANT = "team.repo_grant",
    TEAM_REPO_REVOKE = "team.repo_revoke",
    TEAM_MEMBER_ADD = "team.member_add",
    TEAM_MEMBER_REMOVE = "team.member_remove",
    TEAM_MEMBER_ROLE = "team.member_role_change",
    DEPLOY_KEY_CREATE = "deploy_key.create",
    DEPLOY_KEY_DELETE = "deploy_key.delete",
    APP_PERMISSIONS_ACCEPT = "app.permissions_accept",
    APP_TOKEN_CREATE = "app.token_create",
    APP_REPOS_ADD = "app.installation_repos_add",
    APP_REPOS_REMOVE = "app.installation_repos_remove",
    ORG_SETTINGS = "org.settings_update",
}

/// Whether the successful audit operation can change a run's governing settings.
pub fn leash_audit_action(action: &str) -> bool {
    LEASH_ACTIONS.contains(&action)
}

pub(super) fn audit_credential(entry: &AuditEntry) -> Option<serde_json::Value> {
    let mut after: serde_json::Value = serde_json::from_str(&entry.after).ok()?;
    after.as_object_mut()?.remove("run_credential")
}

pub(super) fn leash_action(entry: &AuditEntry, token_id: &str) -> bool {
    !token_id.is_empty()
        && entry.result == "ok"
        && leash_audit_action(&entry.action)
        && (entry.token_id == token_id
            || audit_credential(entry).is_some_and(|credential| {
                credential["blocks"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|id| id.as_str() == Some(token_id)))
            }))
}

pub(super) fn native_effect(effect: &str) -> &str {
    // Older native callers included the head after the last colon.
    if let Some((reference, _)) = effect.rsplit_once(':')
        && let Some(name) = reference.strip_prefix("ref:")
        && !name.is_empty()
    {
        return reference;
    }
    effect
}

pub(super) fn refusal(error: &crate::error::ForgeError) -> Option<PolicyDenial> {
    if !matches!(
        error.reason,
        ErrorReason::PolicyDenied | ErrorReason::ApprovalRequired
    ) || error.subcode == super::control::FROZEN
    {
        return None;
    }
    match error.payload.as_deref() {
        Some(error_detail::Payload::PolicyDenial(denial))
            if !denial.rule.is_empty()
                && denial.rule != super::control::FROZEN
                && !denial.missing.is_empty() =>
        {
            Some(denial.clone())
        }
        _ => None,
    }
}

pub(super) struct Attempt {
    pub id: String,
    pub effect: String,
    pub reason: ErrorReason,
    pub denial: Option<PolicyDenial>,
    pub time_ms: u64,
}

pub(super) fn repeated_after_refusal(attempts: &[Attempt]) -> Vec<(&Attempt, &Attempt, String)> {
    let mut pending = BTreeMap::new();
    let mut reported = BTreeSet::new();
    let mut results = Vec::new();
    for attempt in attempts {
        let effect = native_effect(&attempt.effect);
        if attempt.reason == ErrorReason::Unspecified {
            pending.retain(|(earlier, _, _), _| *earlier != effect);
        }
        let Some(denial) = &attempt.denial else {
            continue;
        };
        for requirement in &denial.missing {
            let key = (effect, denial.rule.as_str(), requirement.as_str());
            let first = pending.entry(key).or_insert(attempt);
            if first.id != attempt.id && reported.insert(key) {
                results.push((*first, attempt, format!("{}: {}", denial.rule, requirement)));
            }
        }
    }
    results
}

pub(super) fn missing_checks(
    required: &BTreeSet<String>,
    observed: &[bee_policy::CheckResult],
    workflows: &[(String, Option<bee_policy::CheckStatus>)],
    completed_suite: bool,
) -> Vec<String> {
    use bee_policy::CheckStatus::{Failure, Pending};
    let completed = completed_suite
        && observed.iter().all(|check| check.status != Pending)
        && workflows.iter().all(|(_, status)| *status != Some(Pending));
    let mut contradicted: BTreeSet<String> = required
        .iter()
        .filter(|name| {
            match observed
                .iter()
                .find(|check| check.name.as_str() == name.as_str())
            {
                Some(check) => check.status == Failure,
                None => completed,
            }
        })
        .cloned()
        .collect();
    contradicted.extend(workflows.iter().filter_map(|(name, status)| {
        (*status == Some(Failure) || (status.is_none() && completed)).then_some(name.clone())
    }));
    contradicted.into_iter().collect()
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
    fn safety_around_refusal_requires_the_same_unmet_requirement() {
        use crate::error::ForgeError;
        let approval =
            ForgeError::policy_denied("main", vec!["approval".into()], "Approval required");
        let checks =
            ForgeError::policy_denied("main", vec!["check:build".into()], "Build required");
        let frozen = ForgeError::policy_denied(
            super::super::control::FROZEN,
            vec!["organization_resume".into()],
            "Frozen",
        );
        let attempt = |id: &str, effect: &str, error: Option<&ForgeError>| Attempt {
            id: id.into(),
            effect: effect.into(),
            reason: error.map_or(ErrorReason::Unspecified, |error| error.reason),
            denial: error.and_then(refusal),
            time_ms: 1,
        };
        let attempts = [
            attempt("1", "ref:main:old", Some(&approval)),
            attempt("2", "ref:release:old", Some(&approval)),
            attempt("3", "ref:main:new", Some(&checks)),
            attempt("4", "ref:main:new", Some(&approval)),
        ];
        assert!(repeated_after_refusal(&attempts[..3]).is_empty());
        let found = repeated_after_refusal(&attempts);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].0.id.as_str(), found[0].1.id.as_str()), ("1", "4"));
        assert!(
            repeated_after_refusal(&[
                attempt("1", "ref:main:old", Some(&approval)),
                attempt("2", "ref:main:new", None),
                attempt("3", "ref:main:newer", Some(&approval)),
            ])
            .is_empty()
        );
        assert!(
            repeated_after_refusal(&[
                attempt("1", "ref:main:old", Some(&frozen)),
                attempt("2", "ref:main:new", Some(&frozen)),
            ])
            .is_empty()
        );
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
        entry.action = audit_actions::MINT_TOKEN.into();
        entry.token_id = "child".into();
        entry.after =
            serde_json::json!({"run_credential": {"blocks": ["token", "child"]}}).to_string();
        assert!(leash_action(&entry, "token"));
        entry.action = audit_actions::REVOKE_TOKEN.into();
        assert!(leash_action(&entry, "token"));
        assert!(!leash_action(&entry, "unrelated"));
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
        let workflows = [("required workflow".into(), Some(Failure))];
        assert_eq!(
            missing_checks(&required, &reported, &workflows, true),
            ["missing", "required workflow", "tests"]
        );
        assert_eq!(
            missing_checks(&required, &reported, &workflows, false),
            ["required workflow", "tests"]
        );
        let pending = [bee_policy::CheckResult {
            name: "tests".into(),
            status: bee_policy::CheckStatus::Pending,
        }];
        assert!(missing_checks(&required, &pending, &[("verify".into(), None)], true).is_empty());
        assert!(missing_checks(&BTreeSet::from(["build".into()]), &reported, &[], true).is_empty());
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
    fn safety_refresh_updates_evidence_and_preserves_human_decisions() {
        let signal = |text| {
            finding(
                SafetySignalKind::SaidVsShown,
                SafetySeverity::Note,
                text,
                vec![],
                vec![],
                1,
            )
        };
        let initial = signal("build and tests failed");
        let updated = signal("tests failed");
        assert_eq!(initial.id, updated.id);
        let mut existing = vec![initial];
        assert_eq!(
            reconcile(&mut existing, vec![updated.clone()]),
            (vec![], vec![])
        );
        assert_eq!(existing, vec![updated.clone()]);
        assert_eq!(reconcile(&mut existing, vec![]).1, [updated.id.clone()]);
        assert!(existing.is_empty());
        for state in [SafetyFindingState::Dismissed, SafetyFindingState::Confirmed] {
            let mut decided = updated.clone();
            decided.state = state as i32;
            let mut existing = vec![decided];
            assert!(reconcile(&mut existing, vec![]).1.is_empty());
            reconcile(&mut existing, vec![signal("build failed")]);
            assert_eq!(existing.len(), 1);
            assert_eq!(existing[0].state(), state);
        }
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
