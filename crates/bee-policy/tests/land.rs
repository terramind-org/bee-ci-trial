//! Land evaluation: rules, approvals, leases, signatures, rulesets and evaluate mode.

use crate::common;
use bee_policy::{
    CheckResult, CheckStatus, CommitInfo, DiffClass, LeaseInfo, Policy, PrincipalId, Reason,
    RequirementKind, Review, RulesetMode, RulesetOutcome, SignatureState, Verdict,
};
use common::{POLICY, agent, approve, change, ctx, human};

fn policy() -> Policy {
    Policy::parse(POLICY).unwrap()
}

#[test]
fn allow_list_restricts_who_may_land() {
    let src = POLICY.replace(
        "[[land]]\nref = \"main\"\n",
        "[[land]]\nref = \"main\"\nallow = [\"@core\"]\n",
    );
    let p = Policy::parse(&src).unwrap();
    let docs = || vec![change("docs/a.md", DiffClass::Docs)];
    assert!(
        p.evaluate_land(&ctx(vec![human("alice")], "main", docs()))
            .allowed
    );
    // sam may write everywhere but is not in @core.
    let d = p.evaluate_land(&ctx(vec![human("sam")], "main", docs()));
    assert_eq!(d.missing_ids(), vec!["allow:sam"]);
    // An agent acting for alice is not in @core: the allow list applies to the whole chain.
    let d = p.evaluate_land(&ctx(vec![agent("a1"), human("alice")], "main", docs()));
    assert_eq!(d.missing_ids(), vec!["allow:a1"]);
    assert_eq!(d.missing[0].kind, RequirementKind::Allow);
}

#[test]
fn human_logic_change_needs_owner_or_any_approval() {
    let p = policy();
    let alice = human("alice");
    let mut c = ctx(
        vec![alice.clone()],
        "main",
        vec![change("src/lib.rs", DiffClass::Logic)],
    );
    let d = p.evaluate_land(&c);
    assert!(!d.allowed);
    assert_eq!(d.rule.as_deref(), Some("land[main]"));
    assert_eq!(d.missing_ids(), vec!["approval:owner:src/lib.rs:human"]);
    assert_eq!(d.reason, Some(Reason::PolicyDenied));
    // Unowned path: any eligible approval counts.
    c.reviews.push(approve(human("bob"), "c2"));
    let d = p.evaluate_land(&c);
    assert!(d.allowed, "{:?}", d.missing);
    assert!(d.satisfied.iter().any(|r| r.id == "check:test"));
}

#[test]
fn self_approval_and_stale_approvals_do_not_count() {
    let p = policy();
    let alice = human("alice");
    let mut c = ctx(
        vec![alice.clone()],
        "main",
        vec![change("src/lib.rs", DiffClass::Logic)],
    );
    c.reviews.push(approve(alice.clone(), "c2"));
    assert!(!p.evaluate_land(&c).allowed, "author approval ignored");
    c.reviews = vec![approve(human("bob"), "c1")];
    assert!(!p.evaluate_land(&c).allowed, "stale approval dismissed");
    // A later request for changes overrides an earlier approval by the same reviewer.
    c.reviews = vec![
        approve(human("bob"), "c2"),
        Review {
            reviewer: human("bob"),
            revision: "c2".into(),
            verdict: Verdict::RequestChanges,
        },
    ];
    let d = p.evaluate_land(&c);
    assert!(d.missing_ids().contains(&"changes_requested:bob"));
}

#[test]
fn sensitive_paths_need_two_humans_and_owner() {
    let p = policy();
    let sam = human("sam");
    // The classifier said logic; policy marks /src/auth sensitive, so the class is upgraded.
    let mut c = ctx(
        vec![human("alice")],
        "main",
        vec![change("src/auth/token.rs", DiffClass::Logic)],
    );
    c.reviews = vec![approve(sam.clone(), "c2")];
    let d = p.evaluate_land(&c);
    assert_eq!(
        d.missing_ids(),
        vec!["approval:any:human"],
        "one of two humans"
    );
    // An agent approval does not count toward a humans-only rule.
    c.reviews.push(approve(agent("a9"), "c2"));
    assert!(!p.evaluate_land(&c).allowed);
    c.reviews.push(approve(human("bob"), "c2"));
    assert!(p.evaluate_land(&c).allowed);
}

#[test]
fn human_only_approval_ids_are_distinct_from_total_approval_ids() {
    let src = r#"
version = 1

[members]
write = ["alice", "agent-one", "agent-two"]

[[access]]
path = "/"
read = ["@everyone"]
write = ["alice"]

[[land]]
ref = "main"

[[land.approvals]]
count = 1

[[land.approvals]]
count = 3
include_agents = true
"#;
    let p = Policy::parse(src).unwrap();
    let mut c = ctx(
        vec![human("alice")],
        "main",
        vec![change("src/lib.rs", DiffClass::Logic)],
    );
    c.reviews = vec![
        approve(agent("agent-one"), "c2"),
        approve(agent("agent-two"), "c2"),
    ];

    let d = p.evaluate_land(&c);
    assert_eq!(d.missing_ids(), vec!["approval:any:human", "approval:any"]);
    assert_eq!(d.missing[0].detail, "0 of 1 approvals");
    assert_eq!(d.missing[1].detail, "2 of 3 approvals");

    let total_only = src.replace("[[land.approvals]]\ncount = 1\n\n", "");
    let d = Policy::parse(&total_only).unwrap().evaluate_land(&c);
    assert_eq!(d.missing_ids(), vec!["approval:any"]);
    assert_eq!(d.missing[0].detail, "2 of 3 approvals");
}

#[test]
fn owner_approval_required_for_owned_logic_paths() {
    let src = POLICY.replace(
        "sensitive = [\"/.beehive\", \"/src/auth\"]",
        "sensitive = [\"/.beehive\"]",
    );
    let p = Policy::parse(&src).unwrap();
    let mut c = ctx(
        vec![human("alice")],
        "main",
        vec![change("src/auth/token.rs", DiffClass::Logic)],
    );
    c.reviews = vec![approve(human("bob"), "c2")];
    let d = p.evaluate_land(&c);
    assert_eq!(
        d.missing_ids(),
        vec!["approval:owner:src/auth/token.rs:human"]
    );
    c.reviews.push(approve(human("sam"), "c2"));
    assert!(p.evaluate_land(&c).allowed);
}

#[test]
fn agents_land_docs_alone_but_need_a_human_for_logic() {
    let p = policy();
    let a1 = agent("a1");
    let owner = human("alice");
    let c = ctx(
        vec![a1.clone(), owner.clone()],
        "main",
        vec![change("docs/guide.md", DiffClass::Docs)],
    );
    let d = p.evaluate_land(&c);
    assert!(d.allowed, "{:?}", d.missing);
    let mut c = ctx(
        vec![a1.clone(), owner.clone()],
        "main",
        vec![change("src/lib.rs", DiffClass::Logic)],
    );
    c.reviews = vec![approve(agent("a2"), "c2")];
    let d = p.evaluate_land(&c);
    assert!(d.missing_ids().contains(&"approval:human"));
    // The owning human may approve its agent's change.
    c.reviews.push(approve(owner.clone(), "c2"));
    let d = p.evaluate_land(&c);
    assert!(d.allowed, "{:?}", d.missing);
}

#[test]
fn checks_must_pass() {
    let p = policy();
    let mut c = ctx(
        vec![human("alice")],
        "main",
        vec![change("docs/a.md", DiffClass::Docs)],
    );
    c.checks = vec![CheckResult {
        name: "test".into(),
        status: CheckStatus::Pending,
    }];
    let d = p.evaluate_land(&c);
    assert_eq!(d.missing_ids(), vec!["check:test"]);
    assert!(d.missing[0].detail.contains("pending"));
    c.checks.clear();
    assert!(
        p.evaluate_land(&c).missing[0]
            .detail
            .contains("not reported")
    );
    c.checks = vec![CheckResult {
        name: "test".into(),
        status: CheckStatus::Skipped,
    }];
    assert!(p.evaluate_land(&c).allowed);
}

#[test]
fn access_is_checked_for_every_principal_in_the_chain() {
    let p = policy();
    let reader = human("random");
    let c = ctx(
        vec![agent("a1"), reader],
        "main",
        vec![change("docs/a.md", DiffClass::Docs)],
    );
    let d = p.evaluate_land(&c);
    assert_eq!(d.missing_ids(), vec!["access:land:random"]);
    let c = ctx(
        vec![human("alice")],
        "dev",
        vec![change("secrets/k", DiffClass::Logic)],
    );
    let d = p.evaluate_land(&c);
    assert_eq!(d.missing_ids(), vec!["access:write:alice:secrets/k"]);
    assert_eq!(d.missing[0].rule, "access[/secrets].write");
    assert!(d.rule.is_none(), "no land rule for dev");
}

#[test]
fn exclusive_leases() {
    let p = policy();
    let alice = human("alice");
    let mut c = ctx(
        vec![alice.clone()],
        "dev",
        vec![change("schema/v1.sql", DiffClass::Logic)],
    );
    let d = p.evaluate_land(&c);
    assert_eq!(d.missing_ids(), vec!["lease:schema/v1.sql"]);
    assert_eq!(d.reason, Some(Reason::PolicyDenied));
    c.leases = vec![LeaseInfo {
        id: "L1".into(),
        holder: PrincipalId::new("alice"),
        paths: vec!["/schema".into()],
        exclusive: true,
    }];
    assert!(p.evaluate_land(&c).allowed);
    // Someone else's exclusive lease on any touched path: LEASED.
    c.changes.push(change("src/x.rs", DiffClass::Logic));
    c.leases.push(LeaseInfo {
        id: "L2".into(),
        holder: PrincipalId::new("bob"),
        paths: vec!["src".into()],
        exclusive: true,
    });
    let d = p.evaluate_land(&c);
    assert_eq!(d.reason, Some(Reason::Leased));
    assert_eq!(d.missing_ids(), vec!["leased:src/x.rs"]);
    // Advisory leases never block.
    c.leases[1].exclusive = false;
    assert!(p.evaluate_land(&c).allowed);
}

#[test]
fn push_rules_apply_at_land() {
    let p = policy();
    let mut big = change("docs/big.md", DiffClass::Docs);
    big.size = Some(5000);
    let mut deleted = change("docs/old.pem", DiffClass::Docs);
    deleted.size = None;
    let c = ctx(vec![human("alice")], "main", vec![big, deleted]);
    let d = p.evaluate_land(&c);
    assert_eq!(d.missing_ids(), vec!["push:max_file_size:docs/big.md"]);
    assert_eq!(d.missing[0].kind, RequirementKind::Push);
}

const RULESETS: &str = r#"
[[land]]
ref = "release/*"

[[rulesets]]
name = "signed-linear"
refs = ["release/*"]
require_signed = true
require_linear = true
require_last_push_approval = true
bypass = ["@admins"]

[[rulesets]]
name = "two-humans-everywhere"
mode = "evaluate"
refs = ["main", "release/*"]

[[rulesets.approvals]]
count = 2

[[rulesets]]
name = "off"
mode = "disabled"
refs = ["*"]
required_checks = ["never"]
"#;

fn with_rulesets() -> Policy {
    Policy::parse(&format!("{POLICY}{RULESETS}")).unwrap()
}

#[test]
fn active_ruleset_enforces_signed_linear_last_push() {
    let p = with_rulesets();
    let alice = human("alice");
    let mut c = ctx(
        vec![alice.clone()],
        "release/2.0",
        vec![change("docs/a.md", DiffClass::Docs)],
    );
    c.commits = vec![
        CommitInfo {
            id: "c1".into(),
            signature: SignatureState::Verified,
        },
        CommitInfo {
            id: "c2".into(),
            signature: SignatureState::Missing,
        },
    ];
    c.has_merges = true;
    c.last_pusher = Some(PrincipalId::new("alice"));
    let d = p.evaluate_land(&c);
    assert_eq!(
        d.missing_ids(),
        vec!["signed:c2", "linear", "approval:last_push"]
    );
    assert!(d.missing.iter().all(|r| r.rule == "ruleset[signed-linear]"));
    c.commits[1].signature = SignatureState::Verified;
    c.has_merges = false;
    c.reviews = vec![approve(human("bob"), "c2")];
    let d = p.evaluate_land(&c);
    assert!(d.allowed, "{:?}", d.missing);
}

#[test]
fn bypass_actors_skip_rulesets_and_the_skip_is_logged() {
    let p = with_rulesets();
    let mut c = ctx(
        vec![human("root")],
        "release/2.0",
        vec![change("docs/a.md", DiffClass::Docs)],
    );
    c.has_merges = true;
    let d = p.evaluate_land(&c);
    assert!(d.allowed, "{:?}", d.missing);
    let log = d.log.iter().find(|l| l.ruleset == "signed-linear").unwrap();
    assert_eq!(log.outcome, RulesetOutcome::Bypassed);
    assert!(!log.enforced);
    assert!(log.would_block());
    // An agent acting for an admin is not on the bypass list itself: no bypass.
    let mut c2 = ctx(
        vec![agent("a1"), human("root")],
        "release/2.0",
        vec![change("docs/a.md", DiffClass::Docs)],
    );
    c2.has_merges = true;
    assert!(!p.evaluate_land(&c2).allowed);
}

/// AS-AUTH-02: change a ruleset in evaluate mode and report which recent lands it would have
/// blocked, from the decision log alone.
#[test]
fn as_auth_02_evaluate_mode_reports_lands_it_would_block() {
    let p = with_rulesets();
    let recent = vec![
        ("L1", {
            let mut c = ctx(
                vec![human("alice")],
                "main",
                vec![change("docs/a.md", DiffClass::Docs)],
            );
            c.reviews = vec![approve(human("bob"), "c2"), approve(human("sam"), "c2")];
            c
        }),
        (
            "L2",
            ctx(
                vec![human("alice")],
                "main",
                vec![change("docs/b.md", DiffClass::Docs)],
            ),
        ),
        (
            "L3",
            ctx(
                vec![human("alice")],
                "dev",
                vec![change("docs/c.md", DiffClass::Docs)],
            ),
        ),
    ];
    let mut would_block = Vec::new();
    for (id, c) in &recent {
        let d = p.evaluate_land(c);
        assert!(
            d.allowed,
            "evaluate mode never blocks: {id} {:?}",
            d.missing
        );
        for entry in &d.log {
            assert_ne!(entry.ruleset, "off", "disabled rulesets are not evaluated");
            if entry.mode == RulesetMode::Evaluate && entry.would_block() {
                assert_eq!(entry.outcome, RulesetOutcome::Failed);
                assert!(!entry.enforced);
                would_block.push((*id, entry.ruleset.clone(), entry.missing[0].id.clone()));
            }
        }
    }
    assert_eq!(
        would_block,
        vec![(
            "L2",
            "two-humans-everywhere".to_string(),
            "approval:any:human".to_string()
        )]
    );
}

#[test]
fn evaluation_is_deterministic() {
    let p = with_rulesets();
    let mut c = ctx(
        vec![agent("a1"), human("alice")],
        "main",
        vec![
            change("src/b.rs", DiffClass::Logic),
            change("src/a.rs", DiffClass::Logic),
        ],
    );
    c.reviews = vec![approve(agent("a2"), "c2"), approve(agent("a3"), "c1")];
    let a = p.evaluate_land(&c);
    let b = p.evaluate_land(&c);
    assert_eq!(a, b);
}
