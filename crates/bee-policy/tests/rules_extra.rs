//! Land rules added after the first policy format: code-scanning thresholds (F49-008),
//! required agent reviews (F49-009), enforced org-required workflows (F49-007) and rulesets
//! limited by repo custom properties (F416-020). Policies that do not use them keep their
//! canonical form (the frozen golden test in `parse_epoch.rs` covers that).

use crate::common;
use std::collections::BTreeMap;

use bee_policy::{
    AlertLevel, CodeAlert, DiffClass, Policy, PolicyErrorKind, RequirementKind, Review,
    SecuritySeverity, Verdict, WorkflowGate,
};
use common::{POLICY, agent, change, ctx, human};

fn alert(
    tool: &str,
    number: u64,
    level: AlertLevel,
    security: Option<SecuritySeverity>,
) -> CodeAlert {
    CodeAlert {
        tool: tool.into(),
        number,
        level,
        security,
    }
}

const SCANNING: &str = r#"
[[rulesets]]
name = "scanning"
refs = ["main"]

[[rulesets.code_scanning]]
tool = "CodeQL"
alerts = "errors"
security_alerts = "high_or_higher"

[[rulesets.code_scanning]]
tool = "semgrep"
alerts = "none"
security_alerts = "critical"
"#;

#[test]
fn code_scanning_rule_blocks_open_alerts_at_or_above_the_thresholds() {
    let p = Policy::parse(&format!("{POLICY}{SCANNING}")).unwrap();
    assert!(p.requires_code_scanning("main"));
    assert!(!p.requires_code_scanning("dev"));
    let mut c = ctx(
        vec![human("alice")],
        "main",
        vec![change("docs/a.md", DiffClass::Docs)],
    );
    // Below every threshold: a codeql warning with medium severity, a semgrep high.
    c.code_alerts = vec![
        alert(
            "codeql",
            1,
            AlertLevel::Warning,
            Some(SecuritySeverity::Medium),
        ),
        alert(
            "semgrep",
            2,
            AlertLevel::Error,
            Some(SecuritySeverity::High),
        ),
        alert(
            "other",
            3,
            AlertLevel::Error,
            Some(SecuritySeverity::Critical),
        ),
    ];
    let d = p.evaluate_land(&c);
    assert!(d.allowed, "{:?}", d.missing);
    assert!(
        d.satisfied
            .iter()
            .any(|r| r.id == "code_scanning:CodeQL" && r.kind == RequirementKind::CodeScanning)
    );
    // An error-level codeql alert (tool names match case-insensitively) and a critical
    // semgrep alert block, each naming its alert numbers.
    c.code_alerts
        .push(alert("codeql", 4, AlertLevel::Error, None));
    c.code_alerts.push(alert(
        "Semgrep",
        5,
        AlertLevel::Note,
        Some(SecuritySeverity::Critical),
    ));
    let d = p.evaluate_land(&c);
    assert_eq!(
        d.missing_ids(),
        vec!["code_scanning:CodeQL", "code_scanning:semgrep"]
    );
    assert!(d.missing[0].detail.contains("#4"));
    assert!(d.missing[1].detail.contains("#5"));
    assert_eq!(d.missing[0].rule, "ruleset[scanning]");
    // A high-severity codeql note blocks through security_alerts alone.
    c.code_alerts = vec![alert(
        "codeql",
        6,
        AlertLevel::Note,
        Some(SecuritySeverity::High),
    )];
    assert_eq!(
        p.evaluate_land(&c).missing_ids(),
        vec!["code_scanning:CodeQL"]
    );

    // Defaults: alerts = errors, security_alerts = high_or_higher; bad values are typed errors.
    let dflt = Policy::parse(&format!(
        "{POLICY}[[land]]\nref = \"dev\"\n[[land.code_scanning]]\ntool = \"codeql\"\n"
    ))
    .unwrap();
    let mut c = ctx(
        vec![human("alice")],
        "dev",
        vec![change("docs/a.md", DiffClass::Docs)],
    );
    c.code_alerts = vec![alert(
        "codeql",
        7,
        AlertLevel::Warning,
        Some(SecuritySeverity::High),
    )];
    assert_eq!(
        dflt.evaluate_land(&c).missing_ids(),
        vec!["code_scanning:codeql"]
    );
    let err = Policy::parse(&format!(
        "{POLICY}[[land]]\nref = \"dev\"\n[[land.code_scanning]]\ntool = \"x\"\nalerts = \"loud\"\n"
    ))
    .unwrap_err();
    assert_eq!(err.kind, PolicyErrorKind::InvalidValue);
    assert!(
        err.location.key.ends_with("code_scanning[0].alerts"),
        "{}",
        err.location.key
    );
}

#[test]
fn agent_review_rule_needs_a_current_agent_review_that_does_not_request_changes() {
    let src = POLICY.replace(
        "[[land]]\nref = \"main\"\n",
        "[[land]]\nref = \"main\"\nrequire_agent_review = true\n",
    );
    let p = Policy::parse(&src).unwrap();
    assert!(p.requires_agent_review("main"));
    let mut c = ctx(
        vec![human("alice")],
        "main",
        vec![change("docs/a.md", DiffClass::Docs)],
    );
    assert_eq!(p.evaluate_land(&c).missing_ids(), vec!["review:agent"]);
    assert_eq!(
        p.evaluate_land(&c).missing[0].kind,
        RequirementKind::AgentReview
    );
    // A human approval does not satisfy it; an agent's comment review of the head does.
    c.reviews = vec![Review {
        reviewer: human("bob"),
        revision: "c2".into(),
        verdict: Verdict::Approve,
    }];
    assert_eq!(p.evaluate_land(&c).missing_ids(), vec!["review:agent"]);
    c.reviews.push(Review {
        reviewer: agent("reviewbot"),
        revision: "c2".into(),
        verdict: Verdict::Comment,
    });
    assert!(p.evaluate_land(&c).allowed);
    // A stale agent review does not count under dismiss_stale_approvals.
    c.reviews[1].revision = "c1".into();
    assert_eq!(p.evaluate_land(&c).missing_ids(), vec!["review:agent"]);
}

#[test]
fn enforced_required_workflows_block_until_they_pass() {
    let p = Policy::parse(POLICY).unwrap();
    let mut c = ctx(
        vec![human("alice")],
        "main",
        vec![change("docs/a.md", DiffClass::Docs)],
    );
    c.required_workflows = vec![
        WorkflowGate {
            name: "org-security".into(),
            passed: false,
        },
        WorkflowGate {
            name: "license".into(),
            passed: true,
        },
    ];
    let d = p.evaluate_land(&c);
    assert_eq!(d.missing_ids(), vec!["workflow:org-security"]);
    assert_eq!(d.missing[0].kind, RequirementKind::Workflow);
    assert_eq!(d.missing[0].rule, "required_workflows");
    c.required_workflows[0].passed = true;
    assert!(p.evaluate_land(&c).allowed);
}

const PROPS: &str = r#"
[[rulesets]]
name = "prod-only"
refs = ["main"]
required_checks = ["deploy-check"]

[rulesets.repo_properties]
environment = ["Production", "staging"]
team = ["payments"]
"#;

#[test]
fn rulesets_limited_by_repo_properties_apply_only_to_matching_repos() {
    let p = Policy::parse(&format!("{POLICY}{PROPS}")).unwrap();
    assert!(p.has_property_conditions());
    let props = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), vec![v.to_string()]))
            .collect::<BTreeMap<_, _>>()
    };
    let c = ctx(
        vec![human("alice")],
        "main",
        vec![change("docs/a.md", DiffClass::Docs)],
    );
    // Every key must match one of its values; names and values compare case-insensitively
    // and multi-select values (comma-separated) match one by one.
    let matching = p.for_repo_properties(&props(&[
        ("Environment", "production"),
        ("team", "infra,payments"),
    ]));
    assert_eq!(
        matching.evaluate_land(&c).missing_ids(),
        vec!["check:deploy-check"]
    );
    assert_eq!(matching.epoch(), p.epoch());
    for other in [
        props(&[("environment", "dev"), ("team", "payments")]),
        props(&[("environment", "production")]),
        BTreeMap::new(),
    ] {
        let scoped = p.for_repo_properties(&other);
        assert!(scoped.doc().rulesets.is_empty());
        assert!(scoped.evaluate_land(&c).allowed);
        assert_ne!(
            scoped.epoch(),
            p.epoch(),
            "the scoped policy has its own epoch"
        );
    }
    // A condition needs values.
    let bad = Policy::parse(&format!(
        "{POLICY}[[rulesets]]\nname = \"x\"\nrefs = [\"main\"]\n[rulesets.repo_properties]\nenv = []\n"
    ));
    assert!(bad.is_err());
}
