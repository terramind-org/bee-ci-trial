//! Policy-as-code: path/ref ACLs, landing rules, review requirements.
//!
//! The format of `.beehive/policy.toml` and the evaluation rules are specified in SPEC.md §9.4.
//! This crate parses org defaults and repo files ([`Policy::from_sources`]), overlays them,
//! validates cross-references, and evaluates:
//!
//! - [`Policy::check_access`]: may this principal do this action on this path?
//! - [`Policy::check_ref`]: may it create, update, delete or force-update this ref?
//! - [`Policy::evaluate_land`]: may this stack land on this ref, and if not, what is missing?
//! - [`Policy::check_push`]: do these files break a push rule?
//!
//! Every evaluator is a pure function of the policy and its arguments. [`Policy::epoch`] is a
//! hash of the normalized policy, stable under formatting changes.
//!
//! An invalid policy is a [`PolicyError`] with a [`Location`]. The crate never falls back to an
//! older policy; keeping the last good one is the caller's job.

mod action;
mod canon;
mod error;
mod land;
mod model;
mod owners;
mod parse;
mod path;
mod pattern;
mod policy;
mod subject;

pub use action::{Action, BUILTIN_ROLES, DiffClass, PathMode, builtin_role_actions};
pub use canon::repo_epoch;
pub use error::{Location, Origin, PolicyError, PolicyErrorKind, Reason};
pub use land::{
    AlertLevel, ChangedPath, CheckResult, CheckStatus, CodeAlert, CommitInfo, LandContext,
    LandDecision, LeaseInfo, Requirement, RequirementKind, Review, RulesetLog, RulesetOutcome,
    SecuritySeverity, SignatureState, Verdict, WorkflowGate,
};
pub use model::{
    AccessRule, AlertThreshold, ApprovalRule, ApproverSet, CodeScanningRule, GroupDef, LandRule,
    MemberGrant, OwnerRule, PathSets, PolicyDoc, PushRules, RefRule, RoleDef, RuleBody, Ruleset,
    RulesetMode, SecurityThreshold,
};
pub use owners::{Codeowners, CodeownersEntry, OwnerRef};
pub use parse::parse_doc;
pub use path::{Glob, normalize_prefix, prefix_covers};
pub use pattern::{RefPattern, intersect_patterns, most_specific, reduce_patterns};
pub use policy::{
    AccessFilter, Decision, Policy, PushViolation, PushedFile, RefAction, overlay, push_violations,
};
pub use subject::{
    Actor, BUILTIN_GROUPS, PrincipalId, PrincipalKind, REPO_ROLE_GROUP_PREFIX, Subject,
    is_repo_role_group,
};
