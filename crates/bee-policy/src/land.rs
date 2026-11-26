//! Land evaluation: land rules, rulesets (with evaluate mode), leases, push rules and access.

use std::collections::{BTreeMap, BTreeSet};

use crate::action::{Action, DiffClass};
use crate::error::Reason;
use crate::model::{AlertThreshold, ApproverSet, RuleBody, RulesetMode, SecurityThreshold};
use crate::owners::OwnerRef;
use crate::path::{normalize_request_path, prefix_covers};
use crate::pattern::most_specific;
use crate::policy::{Policy, PushedFile, owner_matches, push_violations};
use crate::subject::{Actor, PrincipalId, PrincipalKind};

/// One changed path in the stack being landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    /// Repository path.
    pub path: String,
    /// Diff class from the classifier. A path policy marks sensitive is treated as `sensitive`
    /// whatever the classifier said.
    pub class: DiffClass,
    /// New size in bytes for added or modified files; `None` for deletions.
    pub size: Option<u64>,
}

/// A review verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Approve.
    Approve,
    /// Request changes.
    RequestChanges,
    /// Comment only.
    Comment,
}

/// A review on the change request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// Who reviewed.
    pub reviewer: Actor,
    /// The revision (head changeset id) reviewed.
    pub revision: String,
    /// Verdict.
    pub verdict: Verdict,
}

/// Status of a check run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// Passed.
    Success,
    /// Failed.
    Failure,
    /// Not finished.
    Pending,
    /// Finished without a verdict; counts as passing.
    Neutral,
    /// Skipped; counts as passing.
    Skipped,
}

impl CheckStatus {
    fn passing(self) -> bool {
        matches!(
            self,
            CheckStatus::Success | CheckStatus::Neutral | CheckStatus::Skipped
        )
    }

    fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Success => "success",
            CheckStatus::Failure => "failure",
            CheckStatus::Pending => "pending",
            CheckStatus::Neutral => "neutral",
            CheckStatus::Skipped => "skipped",
        }
    }
}

/// A check result for the head revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Check name.
    pub name: String,
    /// Status.
    pub status: CheckStatus,
}

/// A live lease (the caller drops expired ones).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseInfo {
    /// Lease id.
    pub id: String,
    /// Holder.
    pub holder: PrincipalId,
    /// Leased path prefixes.
    pub paths: Vec<String>,
    /// Exclusive (true) or advisory.
    pub exclusive: bool,
}

/// Signature state of one changeset in the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureState {
    /// Signed with a key registered to the author, and verified.
    Verified,
    /// Signed, but the signature did not verify or the key is unknown.
    Unverified,
    /// Not signed.
    Missing,
}

/// One changeset in the stack, for the signed-commits rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// Changeset id.
    pub id: String,
    /// Signature state.
    pub signature: SignatureState,
}

/// An org-required workflow that must pass before the land (a required workflow with
/// `enforced` set; evaluate-only ones are not listed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowGate {
    /// The requirement's name.
    pub name: String,
    /// Whether its latest run for the head revision succeeded.
    pub passed: bool,
}

/// The level a code-scanning tool gave an alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertLevel {
    /// `note` (or no level).
    Note,
    /// `warning`.
    Warning,
    /// `error`.
    Error,
}

/// A security severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SecuritySeverity {
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
    /// Critical.
    Critical,
}

/// One open code-scanning alert on the code being landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeAlert {
    /// The tool that reported it.
    pub tool: String,
    /// Alert number (for the decision detail).
    pub number: u64,
    /// The tool's level.
    pub level: AlertLevel,
    /// The security severity, for security alerts.
    pub security: Option<SecuritySeverity>,
}

impl CodeAlert {
    fn blocks(&self, alerts: AlertThreshold, security: SecurityThreshold) -> bool {
        let by_level = match alerts {
            AlertThreshold::None => false,
            AlertThreshold::Errors => self.level == AlertLevel::Error,
            AlertThreshold::ErrorsAndWarnings => self.level >= AlertLevel::Warning,
            AlertThreshold::All => true,
        };
        let by_security = match (security, self.security) {
            (SecurityThreshold::None, _) | (_, None) => false,
            (SecurityThreshold::Critical, Some(s)) => s >= SecuritySeverity::Critical,
            (SecurityThreshold::HighOrHigher, Some(s)) => s >= SecuritySeverity::High,
            (SecurityThreshold::MediumOrHigher, Some(s)) => s >= SecuritySeverity::Medium,
            (SecurityThreshold::All, Some(_)) => true,
        };
        by_level || by_security
    }
}

/// Everything land evaluation needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandContext {
    /// The acting principal first, then each delegator back to the root.
    pub chain: Vec<Actor>,
    /// Target ref.
    pub ref_name: String,
    /// Changed paths with their classes.
    pub changes: Vec<ChangedPath>,
    /// Current head revision of the change.
    pub head_revision: String,
    /// Who pushed the head revision, for the last-push approval rule.
    pub last_pusher: Option<PrincipalId>,
    /// Reviews in chronological order (a reviewer's last review counts).
    pub reviews: Vec<Review>,
    /// Check results for the head revision (the last result per name counts).
    pub checks: Vec<CheckResult>,
    /// Live leases on the target ref.
    pub leases: Vec<LeaseInfo>,
    /// Changesets in the stack.
    pub commits: Vec<CommitInfo>,
    /// Whether the stack contains merge changesets.
    pub has_merges: bool,
    /// Enforced org-required workflows that apply to this land.
    pub required_workflows: Vec<WorkflowGate>,
    /// Open code-scanning alerts on the code being landed.
    pub code_alerts: Vec<CodeAlert>,
}

/// What kind of requirement an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequirementKind {
    /// Role and path access for each principal in the chain.
    Access,
    /// The land rule's `allow` list.
    Allow,
    /// A required check.
    Check,
    /// An approval rule.
    Approval,
    /// An agent landing a class outside `agents_may_land_without_human`.
    HumanApproval,
    /// Approval of the last push by someone else.
    LastPushApproval,
    /// An outstanding request for changes.
    ChangesRequested,
    /// A verified signature on every changeset.
    Signed,
    /// No merge changesets.
    Linear,
    /// An exclusive lease the lander must hold.
    Lease,
    /// A path exclusively leased by someone else.
    Leased,
    /// A push rule.
    Push,
    /// An enforced org-required workflow.
    Workflow,
    /// Open code-scanning alerts at or above a threshold.
    CodeScanning,
    /// A review by an agent.
    AgentReview,
}

/// One requirement, satisfied or missing. `id` is the stable string reported in
/// `PolicyDenial.missing` (e.g. `check:test`, `approval:owner:src/auth`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    /// Stable id.
    pub id: String,
    /// Kind.
    pub kind: RequirementKind,
    /// The rule that imposed it (`land[main]`, `ruleset[name]`, `access`, `push`, `paths`).
    pub rule: String,
    /// Explanation.
    pub detail: String,
    /// Reason reported if it is missing.
    pub reason: Reason,
}

/// Outcome of one ruleset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RulesetOutcome {
    /// Every requirement held.
    Passed,
    /// Something was missing.
    Failed,
    /// The actor is on the bypass list; requirements were evaluated and logged anyway.
    Bypassed,
}

/// One entry of the decision log: what a ruleset concluded and whether it was enforced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesetLog {
    /// Ruleset name.
    pub ruleset: String,
    /// Its mode.
    pub mode: RulesetMode,
    /// Outcome.
    pub outcome: RulesetOutcome,
    /// Whether a failure blocked the land (active mode and not bypassed).
    pub enforced: bool,
    /// Requirements that held.
    pub satisfied: Vec<Requirement>,
    /// Requirements that were missing.
    pub missing: Vec<Requirement>,
}

impl RulesetLog {
    /// Whether this ruleset would block the land if it were active and not bypassed.
    pub fn would_block(&self) -> bool {
        !self.missing.is_empty()
    }
}

/// The result of [`Policy::evaluate_land`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandDecision {
    /// Whether the land may proceed.
    pub allowed: bool,
    /// Requirements that held.
    pub satisfied: Vec<Requirement>,
    /// Requirements that are missing (enforced only).
    pub missing: Vec<Requirement>,
    /// The land rule that applied (`land[main]`), if any.
    pub rule: Option<String>,
    /// `LEASED` if a missing requirement is a lease conflict, otherwise `POLICY_DENIED`; `None`
    /// when allowed.
    pub reason: Option<Reason>,
    /// Decision log for every matching ruleset, in name order.
    pub log: Vec<RulesetLog>,
}

impl LandDecision {
    /// The missing ids, as reported in `PolicyDenial.missing`.
    pub fn missing_ids(&self) -> Vec<&str> {
        self.missing.iter().map(|r| r.id.as_str()).collect()
    }
}

#[derive(Default)]
struct Acc {
    satisfied: Vec<Requirement>,
    missing: Vec<Requirement>,
}

impl Acc {
    fn add(&mut self, ok: bool, id: String, kind: RequirementKind, rule: &str, detail: String) {
        self.add_reason(ok, id, kind, rule, detail, Reason::PolicyDenied);
    }

    fn add_reason(
        &mut self,
        ok: bool,
        id: String,
        kind: RequirementKind,
        rule: &str,
        detail: String,
        reason: Reason,
    ) {
        let r = Requirement {
            id,
            kind,
            rule: rule.to_string(),
            detail,
            reason,
        };
        if ok {
            self.satisfied.push(r)
        } else {
            self.missing.push(r)
        }
    }
}

struct Prepared<'a> {
    author: &'a Actor,
    changes: Vec<(String, DiffClass)>,
    /// Each reviewer's last review, with the reviewer's resolved groups and actions.
    latest: Vec<(&'a Review, BTreeSet<String>, bool)>,
}

impl Policy {
    /// The rule bodies that apply to landing on `ref_name`: the most specific land rule and
    /// every ruleset that is not disabled and matches the ref.
    pub fn land_bodies(&self, ref_name: &str) -> Vec<&RuleBody> {
        let mut out: Vec<&RuleBody> = most_specific(self.doc.land.iter(), |(p, _)| *p, ref_name)
            .map(|(_, r)| &r.body)
            .into_iter()
            .collect();
        out.extend(
            self.doc
                .rulesets
                .values()
                .filter(|r| {
                    r.mode != RulesetMode::Disabled && r.refs.iter().any(|p| p.matches(ref_name))
                })
                .map(|r| &r.body),
        );
        out
    }

    /// Whether landing on `ref_name` evaluates signatures (`require_signed` somewhere).
    pub fn requires_signatures(&self, ref_name: &str) -> bool {
        self.land_bodies(ref_name).iter().any(|b| b.require_signed)
    }

    /// Whether landing on `ref_name` evaluates code-scanning alerts.
    pub fn requires_code_scanning(&self, ref_name: &str) -> bool {
        self.land_bodies(ref_name)
            .iter()
            .any(|b| !b.code_scanning.is_empty())
    }

    /// Whether landing on `ref_name` needs an agent review.
    pub fn requires_agent_review(&self, ref_name: &str) -> bool {
        self.land_bodies(ref_name)
            .iter()
            .any(|b| b.require_agent_review)
    }

    /// Evaluates a land against the effective policy.
    ///
    /// Order: access for every principal in the chain; the most specific land rule; exclusive
    /// leases; push rules; then every matching ruleset. Active rulesets add their missing
    /// requirements; evaluate-mode rulesets and bypassed rulesets only write the decision log.
    pub fn evaluate_land(&self, ctx: &LandContext) -> LandDecision {
        let mut acc = Acc::default();
        let Some(author) = ctx.chain.first() else {
            acc.add(
                false,
                "access:chain".into(),
                RequirementKind::Access,
                "access",
                "empty principal chain".into(),
            );
            return finish(acc, None, Vec::new());
        };
        let changes: Vec<(String, DiffClass)> = ctx
            .changes
            .iter()
            .map(|c| {
                let path = normalize_request_path(&c.path).to_string();
                let class = if self.is_sensitive(&path) {
                    DiffClass::Sensitive
                } else {
                    c.class
                };
                (path, class)
            })
            .collect();
        let mut by_reviewer: BTreeMap<&PrincipalId, &Review> = BTreeMap::new();
        for r in &ctx.reviews {
            by_reviewer.insert(&r.reviewer.id, r);
        }
        let latest = by_reviewer
            .into_values()
            .map(|r| {
                let groups = self.groups_of(&r.reviewer);
                let can_review = self
                    .actions_with(&r.reviewer, &groups)
                    .contains(&Action::Review);
                (r, groups, can_review)
            })
            .collect();
        let prep = Prepared {
            author,
            changes,
            latest,
        };

        self.eval_access(ctx, &prep, &mut acc);

        let rule = most_specific(self.doc.land.iter(), |(p, _)| *p, &ctx.ref_name).map(|(p, r)| {
            let label = format!("land[{p}]");
            self.eval_body(ctx, &prep, &r.body, &label, &mut acc);
            label
        });

        self.eval_leases(ctx, &prep, &mut acc);
        self.eval_push(ctx, &self.doc.push, "push", &mut acc);
        for w in &ctx.required_workflows {
            acc.add(
                w.passed,
                format!("workflow:{}", w.name),
                RequirementKind::Workflow,
                "required_workflows",
                if w.passed {
                    format!("required workflow `{}` passed", w.name)
                } else {
                    format!(
                        "required workflow `{}` has not passed for this revision",
                        w.name
                    )
                },
            );
        }

        let mut log = Vec::new();
        for (name, rs) in &self.doc.rulesets {
            if rs.mode == RulesetMode::Disabled || !rs.refs.iter().any(|p| p.matches(&ctx.ref_name))
            {
                continue;
            }
            let label = format!("ruleset[{name}]");
            let mut r_acc = Acc::default();
            self.eval_body(ctx, &prep, &rs.body, &label, &mut r_acc);
            let bypassed = ctx.chain.iter().all(|a| self.is_member(a, &rs.bypass));
            let outcome = if bypassed {
                RulesetOutcome::Bypassed
            } else if r_acc.missing.is_empty() {
                RulesetOutcome::Passed
            } else {
                RulesetOutcome::Failed
            };
            let enforced = rs.mode == RulesetMode::Active && !bypassed;
            if enforced {
                acc.satisfied.extend(r_acc.satisfied.iter().cloned());
                acc.missing.extend(r_acc.missing.iter().cloned());
            }
            log.push(RulesetLog {
                ruleset: name.clone(),
                mode: rs.mode,
                outcome,
                enforced,
                satisfied: r_acc.satisfied,
                missing: r_acc.missing,
            });
        }
        finish(acc, rule, log)
    }

    fn eval_access(&self, ctx: &LandContext, prep: &Prepared<'_>, acc: &mut Acc) {
        for actor in &ctx.chain {
            let groups = self.groups_of(actor);
            let before = acc.missing.len();
            if !self.actions_with(actor, &groups).contains(&Action::Land) {
                acc.add(
                    false,
                    format!("access:land:{}", actor.id),
                    RequirementKind::Access,
                    "members",
                    format!("{} holds no role granting `land`", actor.id),
                );
                continue;
            }
            for (path, _) in &prep.changes {
                let d = self.check_access_with(actor, &groups, Action::Land, path);
                if !d.allowed {
                    acc.add(
                        false,
                        format!("access:write:{}:{path}", actor.id),
                        RequirementKind::Access,
                        &d.rule,
                        d.detail,
                    );
                }
            }
            if acc.missing.len() == before {
                acc.add(
                    true,
                    format!("access:{}", actor.id),
                    RequirementKind::Access,
                    "access",
                    format!("{} may land every changed path", actor.id),
                );
            }
        }
    }

    fn eval_body(
        &self,
        ctx: &LandContext,
        prep: &Prepared<'_>,
        body: &RuleBody,
        rule: &str,
        acc: &mut Acc,
    ) {
        if let Some(allow) = &body.allow {
            for actor in &ctx.chain {
                let ok = self.is_member(actor, allow);
                acc.add(
                    ok,
                    format!("allow:{}", actor.id),
                    RequirementKind::Allow,
                    rule,
                    format!(
                        "{} {} on the allow list",
                        actor.id,
                        if ok { "is" } else { "is not" }
                    ),
                );
            }
        }

        let mut status: BTreeMap<&str, CheckStatus> = BTreeMap::new();
        for c in &ctx.checks {
            status.insert(c.name.as_str(), c.status);
        }
        for name in &body.required_checks {
            let s = status.get(name.as_str()).copied();
            let ok = s.is_some_and(CheckStatus::passing);
            let detail = match s {
                Some(s) => format!("check `{name}` is {}", s.as_str()),
                None => format!("check `{name}` has not reported"),
            };
            acc.add(
                ok,
                format!("check:{name}"),
                RequirementKind::Check,
                rule,
                detail,
            );
        }

        if body.require_signed {
            let mut all = true;
            for c in &ctx.commits {
                if c.signature != SignatureState::Verified {
                    all = false;
                    acc.add(
                        false,
                        format!("signed:{}", c.id),
                        RequirementKind::Signed,
                        rule,
                        format!("changeset {} has no verified signature", c.id),
                    );
                }
            }
            if all {
                acc.add(
                    true,
                    "signed".into(),
                    RequirementKind::Signed,
                    rule,
                    "every changeset is signed".into(),
                );
            }
        }

        if body.require_linear {
            acc.add(
                !ctx.has_merges,
                "linear".into(),
                RequirementKind::Linear,
                rule,
                if ctx.has_merges {
                    "the stack contains merges".into()
                } else {
                    "the stack is linear".into()
                },
            );
        }

        // Reviews that count under this rule.
        let valid = |r: &Review| {
            r.reviewer.id != prep.author.id
                && (!body.dismiss_stale_approvals || r.revision == ctx.head_revision)
        };
        let approvers: Vec<(&Actor, &BTreeSet<String>)> = prep
            .latest
            .iter()
            .filter(|(r, _, can)| *can && r.verdict == Verdict::Approve && valid(r))
            .map(|(r, g, _)| (&r.reviewer, g))
            .collect();

        for (r, _, can) in &prep.latest {
            if *can && r.verdict == Verdict::RequestChanges && valid(r) {
                acc.add(
                    false,
                    format!("changes_requested:{}", r.reviewer.id),
                    RequirementKind::ChangesRequested,
                    rule,
                    format!("{} requested changes", r.reviewer.id),
                );
            }
        }

        for a in &body.approvals {
            let approval_id = |id: String| {
                if a.include_agents {
                    id
                } else {
                    format!("{id}:human")
                }
            };
            let matched: Vec<&str> = prep
                .changes
                .iter()
                .filter(|(path, class)| {
                    (a.classes.is_empty() || a.classes.contains(class))
                        && (a.paths.is_empty() || a.paths.iter().any(|p| prefix_covers(p, path)))
                })
                .map(|(p, _)| p.as_str())
                .collect();
            if matched.is_empty() {
                continue;
            }
            let eligible: Vec<(&Actor, &BTreeSet<String>)> = approvers
                .iter()
                .copied()
                .filter(|(actor, _)| a.include_agents || actor.kind == PrincipalKind::Human)
                .collect();
            let need = a.count as usize;
            match &a.from {
                ApproverSet::Any => {
                    let have = eligible.len();
                    acc.add(
                        have >= need,
                        approval_id("approval:any".into()),
                        RequirementKind::Approval,
                        rule,
                        format!("{have} of {need} approvals"),
                    );
                }
                ApproverSet::Subjects(subjects) => {
                    let have = eligible
                        .iter()
                        .filter(|(actor, groups)| Policy::matches_any(actor, groups, subjects))
                        .count();
                    let names: Vec<String> = subjects.iter().map(ToString::to_string).collect();
                    acc.add(
                        have >= need,
                        approval_id(format!("approval:group:{}", names.join(","))),
                        RequirementKind::Approval,
                        rule,
                        format!("{have} of {need} approvals from {}", names.join(", ")),
                    );
                }
                ApproverSet::Owners => {
                    // Group matched paths by their owner set; each group needs `count` approvals
                    // from its owners (unowned paths accept any eligible approver).
                    let mut groups: BTreeMap<Option<(String, BTreeSet<OwnerRef>)>, Vec<&str>> =
                        BTreeMap::new();
                    for p in matched {
                        groups.entry(self.owners_of(p)).or_default().push(p);
                    }
                    for (owners, paths) in groups {
                        let have = match &owners {
                            None => eligible.len(),
                            Some((_, set)) => eligible
                                .iter()
                                .filter(|(actor, groups)| owner_matches(actor, groups, set))
                                .count(),
                        };
                        let who = match &owners {
                            None => "anyone (unowned)".to_string(),
                            Some((src, set)) => {
                                let names: Vec<String> =
                                    set.iter().map(ToString::to_string).collect();
                                format!("{} ({src})", names.join(", "))
                            }
                        };
                        acc.add(
                            have >= need,
                            approval_id(format!("approval:owner:{}", paths[0])),
                            RequirementKind::Approval,
                            rule,
                            format!(
                                "{have} of {need} approvals from {who} for {} path(s)",
                                paths.len()
                            ),
                        );
                    }
                }
            }
        }

        if prep.author.kind == PrincipalKind::Agent {
            let needs_human = prep
                .changes
                .iter()
                .any(|(_, c)| !body.agents_may_land_without_human.contains(c));
            if needs_human {
                let have = approvers
                    .iter()
                    .any(|(a, _)| a.kind == PrincipalKind::Human);
                acc.add(
                    have,
                    "approval:human".into(),
                    RequirementKind::HumanApproval,
                    rule,
                    "an agent landing these classes needs a human approval".into(),
                );
            }
        }

        if body.require_last_push_approval
            && let Some(pusher) = &ctx.last_pusher
        {
            let have = prep.latest.iter().any(|(r, _, can)| {
                *can && r.verdict == Verdict::Approve
                    && r.revision == ctx.head_revision
                    && r.reviewer.id != *pusher
                    && r.reviewer.id != prep.author.id
            });
            acc.add(
                have,
                "approval:last_push".into(),
                RequirementKind::LastPushApproval,
                rule,
                format!("the last push by {pusher} needs approval from someone else"),
            );
        }

        for c in &body.code_scanning {
            let blocking: Vec<u64> = ctx
                .code_alerts
                .iter()
                .filter(|a| a.tool.eq_ignore_ascii_case(&c.tool))
                .filter(|a| a.blocks(c.alerts, c.security_alerts))
                .map(|a| a.number)
                .collect();
            acc.add(
                blocking.is_empty(),
                format!("code_scanning:{}", c.tool),
                RequirementKind::CodeScanning,
                rule,
                if blocking.is_empty() {
                    format!("no open `{}` alerts at the blocking thresholds", c.tool)
                } else {
                    format!(
                        "{} open `{}` alert(s) at or above alerts={} / security_alerts={}: #{}",
                        blocking.len(),
                        c.tool,
                        c.alerts.as_str(),
                        c.security_alerts.as_str(),
                        blocking
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", #")
                    )
                },
            );
        }

        if body.require_agent_review {
            let reviewed = prep.latest.iter().any(|(r, _, _)| {
                r.reviewer.kind == PrincipalKind::Agent
                    && r.verdict != Verdict::RequestChanges
                    && valid(r)
            });
            acc.add(
                reviewed,
                "review:agent".into(),
                RequirementKind::AgentReview,
                rule,
                if reviewed {
                    "an agent reviewed the change".into()
                } else {
                    "an agent review is required".into()
                },
            );
        }

        self.eval_push(ctx, &body.push, &format!("{rule}.push"), acc);
    }

    fn eval_leases(&self, ctx: &LandContext, prep: &Prepared<'_>, acc: &mut Acc) {
        let in_chain = |p: &PrincipalId| ctx.chain.iter().any(|a| a.id == *p);
        let covers = |l: &LeaseInfo, path: &str| {
            l.paths
                .iter()
                .any(|lp| prefix_covers(normalize_request_path(lp), path))
        };
        for (path, _) in &prep.changes {
            let conflict = ctx
                .leases
                .iter()
                .find(|l| l.exclusive && !in_chain(&l.holder) && covers(l, path));
            if let Some(l) = conflict {
                acc.add_reason(
                    false,
                    format!("leased:{path}"),
                    RequirementKind::Leased,
                    "leases",
                    format!(
                        "/{path} is exclusively leased by {} (lease {})",
                        l.holder, l.id
                    ),
                    Reason::Leased,
                );
                continue;
            }
            if self.requires_exclusive_lease(path) {
                let held = ctx
                    .leases
                    .iter()
                    .any(|l| l.exclusive && in_chain(&l.holder) && covers(l, path));
                acc.add(
                    held,
                    format!("lease:{path}"),
                    RequirementKind::Lease,
                    "paths.exclusive_lease",
                    format!("/{path} needs an exclusive lease held by the lander"),
                );
            }
        }
    }

    fn eval_push(
        &self,
        ctx: &LandContext,
        rules: &crate::model::PushRules,
        rule: &str,
        acc: &mut Acc,
    ) {
        if rules.is_empty() {
            return;
        }
        let files: Vec<PushedFile> = ctx
            .changes
            .iter()
            .filter_map(|c| {
                c.size.map(|size| PushedFile {
                    path: c.path.clone(),
                    size,
                })
            })
            .collect();
        for v in push_violations(rules, &files) {
            acc.add(false, v.id, RequirementKind::Push, rule, v.detail);
        }
    }
}

fn finish(acc: Acc, rule: Option<String>, log: Vec<RulesetLog>) -> LandDecision {
    let allowed = acc.missing.is_empty();
    let reason = if allowed {
        None
    } else if acc.missing.iter().any(|r| r.reason == Reason::Leased) {
        Some(Reason::Leased)
    } else {
        Some(Reason::PolicyDenied)
    };
    LandDecision {
        allowed,
        satisfied: acc.satisfied,
        missing: acc.missing,
        rule,
        reason,
        log,
    }
}
