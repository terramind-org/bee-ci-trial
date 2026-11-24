//! The normalized policy model shared by parsing, overlaying, evaluation and the epoch.
//!
//! Every set-like list is a `BTreeSet` and every keyed rule list is a `BTreeMap`, so two files
//! that differ only in formatting, comments, ordering, duplicates or path spelling produce equal
//! models (and equal epochs).

use std::collections::{BTreeMap, BTreeSet};

use crate::action::{Action, DiffClass};
use crate::error::Location;
use crate::owners::OwnerRef;
use crate::path::Glob;
use crate::pattern::RefPattern;
use crate::subject::Subject;

/// A group defined in the policy file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupDef {
    /// Members: principals and nested `@groups`.
    pub members: BTreeSet<Subject>,
    pub(crate) loc: Location,
}

/// A custom role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleDef {
    /// Role this one extends (built-in or custom).
    pub base: Option<String>,
    /// Actions added on top of the base.
    pub actions: BTreeSet<Action>,
    pub(crate) loc: Location,
}

/// The subjects holding one role (`[members]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberGrant {
    /// Who holds the role.
    pub subjects: BTreeSet<Subject>,
    pub(crate) loc: Location,
}

/// An access rule for one path prefix. `read` and `write` are matched independently: a rule
/// that leaves `read` unset does not affect read decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRule {
    /// Who may read under the prefix (`None`: this rule says nothing about reads).
    pub read: Option<BTreeSet<Subject>>,
    /// Who may write under the prefix (`None`: this rule says nothing about writes).
    pub write: Option<BTreeSet<Subject>>,
    pub(crate) loc: Location,
}

/// Who may create, update, delete or force-update refs matching a pattern. An unset list falls
/// back to the role check (`manage_refs`, or `force_update` for force).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefRule {
    /// Who may create matching refs.
    pub create: Option<BTreeSet<Subject>>,
    /// Who may update matching refs directly (never allowed on protected refs).
    pub update: Option<BTreeSet<Subject>>,
    /// Who may delete matching refs.
    pub delete: Option<BTreeSet<Subject>>,
    /// Who may force-update matching refs.
    pub force: Option<BTreeSet<Subject>>,
    pub(crate) loc: Location,
}

/// Whose approvals count toward an approval rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ApproverSet {
    /// Anyone holding the `review` action.
    Any,
    /// The owners of each matched path (unowned paths accept anyone).
    Owners,
    /// Members of these subjects.
    Subjects(BTreeSet<Subject>),
}

/// Approvals required when a land touches matching paths.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ApprovalRule {
    /// Diff classes the rule applies to (empty: every class).
    pub classes: BTreeSet<DiffClass>,
    /// Path prefixes the rule applies to (empty: every path).
    pub paths: BTreeSet<String>,
    /// Distinct approvers required (per matched path for `Owners`).
    pub count: u32,
    /// Whose approvals count.
    pub from: ApproverSet,
    /// Whether approvals from agents and services count (default: humans only).
    pub include_agents: bool,
}

/// Push rules: checked for every added or modified file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushRules {
    /// Paths that may not be written.
    pub deny_paths: BTreeSet<Glob>,
    /// File extensions that may not be written (lower case, no leading dot).
    pub deny_extensions: BTreeSet<String>,
    /// Largest allowed file, in bytes.
    pub max_file_size: Option<u64>,
    /// Longest allowed path, in bytes.
    pub max_path_length: Option<u64>,
}

impl PushRules {
    /// True when no rule is set.
    pub fn is_empty(&self) -> bool {
        self.deny_paths.is_empty()
            && self.deny_extensions.is_empty()
            && self.max_file_size.is_none()
            && self.max_path_length.is_none()
    }
}

/// Which tool-reported alert levels block a land under a code-scanning rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertThreshold {
    /// No level blocks.
    None,
    /// `error` alerts block.
    Errors,
    /// `error` and `warning` alerts block.
    ErrorsAndWarnings,
    /// Every alert blocks.
    All,
}

impl AlertThreshold {
    /// Policy-file name.
    pub fn as_str(self) -> &'static str {
        match self {
            AlertThreshold::None => "none",
            AlertThreshold::Errors => "errors",
            AlertThreshold::ErrorsAndWarnings => "errors_and_warnings",
            AlertThreshold::All => "all",
        }
    }

    /// Parses a policy-file name.
    pub fn parse(s: &str) -> Option<AlertThreshold> {
        Some(match s {
            "none" => AlertThreshold::None,
            "errors" => AlertThreshold::Errors,
            "errors_and_warnings" => AlertThreshold::ErrorsAndWarnings,
            "all" => AlertThreshold::All,
            _ => return None,
        })
    }
}

/// Which security severities block a land under a code-scanning rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SecurityThreshold {
    /// No severity blocks.
    None,
    /// `critical` alerts block.
    Critical,
    /// `high` and `critical` alerts block.
    HighOrHigher,
    /// `medium`, `high` and `critical` alerts block.
    MediumOrHigher,
    /// Every security alert blocks.
    All,
}

impl SecurityThreshold {
    /// Policy-file name.
    pub fn as_str(self) -> &'static str {
        match self {
            SecurityThreshold::None => "none",
            SecurityThreshold::Critical => "critical",
            SecurityThreshold::HighOrHigher => "high_or_higher",
            SecurityThreshold::MediumOrHigher => "medium_or_higher",
            SecurityThreshold::All => "all",
        }
    }

    /// Parses a policy-file name.
    pub fn parse(s: &str) -> Option<SecurityThreshold> {
        Some(match s {
            "none" => SecurityThreshold::None,
            "critical" => SecurityThreshold::Critical,
            "high_or_higher" => SecurityThreshold::HighOrHigher,
            "medium_or_higher" => SecurityThreshold::MediumOrHigher,
            "all" => SecurityThreshold::All,
            _ => return None,
        })
    }
}

/// Open code-scanning alerts of one tool block a land (GitHub's "require code scanning
/// results"): an alert blocks when its level meets `alerts` or its security severity meets
/// `security_alerts`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CodeScanningRule {
    /// Tool name as reported by analyses (compared case-insensitively).
    pub tool: String,
    /// Blocking alert levels.
    pub alerts: AlertThreshold,
    /// Blocking security severities.
    pub security_alerts: SecurityThreshold,
}

/// The requirements shared by land rules and rulesets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleBody {
    /// Who may land (`None`: anyone with the `land` action and write access).
    pub allow: Option<BTreeSet<Subject>>,
    /// Checks that must pass.
    pub required_checks: BTreeSet<String>,
    /// Approval requirements (all must hold), in canonical order.
    pub approvals: Vec<ApprovalRule>,
    /// Classes an agent may land without any human approval. Anything else an agent lands
    /// needs at least one human approval.
    pub agents_may_land_without_human: BTreeSet<DiffClass>,
    /// Every landed changeset must carry a verified signature.
    pub require_signed: bool,
    /// The stack may not contain merge changesets.
    pub require_linear: bool,
    /// Approvals of an older revision do not count.
    pub dismiss_stale_approvals: bool,
    /// Someone other than the last pusher must approve the current revision.
    pub require_last_push_approval: bool,
    /// Extra push rules for this ref.
    pub push: PushRules,
    /// Code-scanning tools whose open alerts block the land, in canonical order.
    pub code_scanning: Vec<CodeScanningRule>,
    /// A review by an agent principal (not requesting changes) is required; the forge
    /// requests one from the repo's review agent when a change request opens.
    pub require_agent_review: bool,
}

/// A land rule for one ref pattern; the most specific matching rule applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandRule {
    /// The requirements.
    pub body: RuleBody,
    pub(crate) loc: Location,
}

/// Owners of a path prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRule {
    /// Owners (principals or groups).
    pub owners: BTreeSet<OwnerRef>,
    pub(crate) loc: Location,
}

/// Path sets used by classification and landing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathSets {
    /// Paths classified `sensitive`.
    pub sensitive: BTreeSet<Glob>,
    /// Paths classified `generated`.
    pub generated: BTreeSet<Glob>,
    /// Paths that need an exclusive lease to land.
    pub exclusive_lease: BTreeSet<Glob>,
}

/// How a ruleset is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RulesetMode {
    /// Enforced.
    Active,
    /// Evaluated and logged, never enforced.
    Evaluate,
    /// Ignored.
    Disabled,
}

impl RulesetMode {
    /// Policy-file name.
    pub fn as_str(self) -> &'static str {
        match self {
            RulesetMode::Active => "active",
            RulesetMode::Evaluate => "evaluate",
            RulesetMode::Disabled => "disabled",
        }
    }
}

/// A named ruleset. Every matching ruleset applies on top of the selected land rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruleset {
    /// Mode.
    pub mode: RulesetMode,
    /// Ref patterns the ruleset applies to (any match).
    pub refs: BTreeSet<RefPattern>,
    /// Actors that skip the ruleset (the skip is logged).
    pub bypass: BTreeSet<Subject>,
    /// Requirements.
    pub body: RuleBody,
    /// Repo custom properties the ruleset is limited to: every key must hold one of its
    /// values (lower-cased; multi-select values match one by one). Empty: every repo.
    pub repo_properties: BTreeMap<String, BTreeSet<String>>,
    pub(crate) loc: Location,
}

impl Ruleset {
    /// Whether a repo with `properties` (name to values) is in scope.
    pub fn covers_properties(&self, properties: &BTreeMap<String, Vec<String>>) -> bool {
        self.repo_properties.iter().all(|(key, wanted)| {
            properties
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .is_some_and(|(_, values)| {
                    values
                        .iter()
                        .flat_map(|v| v.split(','))
                        .any(|v| wanted.contains(&v.trim().to_lowercase()))
                })
        })
    }
}

/// One policy document after parsing and normalization (an org default or a repo file), or the
/// overlay of the two.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyDoc {
    /// Repository path of a `CODEOWNERS` file to consult for paths without `[[owners]]`.
    pub codeowners: Option<String>,
    /// Groups.
    pub groups: BTreeMap<String, GroupDef>,
    /// Custom roles.
    pub roles: BTreeMap<String, RoleDef>,
    /// Role grants, keyed by role name.
    pub members: BTreeMap<String, MemberGrant>,
    /// Access rules, keyed by normalized prefix (`""` is `/`).
    pub access: BTreeMap<String, AccessRule>,
    /// Ref rules.
    pub refs: BTreeMap<RefPattern, RefRule>,
    /// Land rules.
    pub land: BTreeMap<RefPattern, LandRule>,
    /// Owner rules, keyed by normalized prefix.
    pub owners: BTreeMap<String, OwnerRule>,
    /// Push rules.
    pub push: PushRules,
    /// Path sets.
    pub paths: PathSets,
    /// Rulesets by name.
    pub rulesets: BTreeMap<String, Ruleset>,
}
