//! Actions, built-in roles and diff classes.

use std::collections::BTreeSet;
use std::fmt;

/// Something a principal may do in a repository. Names are the strings used in policy files,
/// token scopes and `CheckAccess`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    /// Read objects, trees, history and derived data under a path.
    Read,
    /// Manage issues, labels and change-request metadata.
    Triage,
    /// Review change requests.
    Review,
    /// Create changesets and update one's own workspace refs.
    Commit,
    /// Acquire path leases.
    Lease,
    /// Land onto a ref (subject to land rules).
    Land,
    /// Create, update and delete refs that are not protected.
    ManageRefs,
    /// Move a ref to a non-descendant (force update).
    ForceUpdate,
    /// Redact objects (SPEC §13).
    Redact,
    /// Purge redacted objects.
    Purge,
    /// Change the policy itself (in addition to write access on `.beehive/policy.toml`).
    ManagePolicy,
    /// Change repository settings.
    ManageSettings,
    /// Open issues and comment on issues (and edit or close one's own). Every built-in role
    /// has it; a token without it cannot write to issues.
    IssuesWrite,
}

/// Which path permission an action needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMode {
    /// Needs read access on the path.
    Read,
    /// Needs write access on the path.
    Write,
    /// Repository-wide; the path is ignored.
    None,
}

impl Action {
    /// Every action, in canonical order.
    pub const ALL: [Action; 13] = [
        Action::Read,
        Action::Triage,
        Action::Review,
        Action::Commit,
        Action::Lease,
        Action::Land,
        Action::ManageRefs,
        Action::ForceUpdate,
        Action::Redact,
        Action::Purge,
        Action::ManagePolicy,
        Action::ManageSettings,
        Action::IssuesWrite,
    ];

    /// The policy-file name of the action.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Triage => "triage",
            Action::Review => "review",
            Action::Commit => "commit",
            Action::Lease => "lease",
            Action::Land => "land",
            Action::ManageRefs => "manage_refs",
            Action::ForceUpdate => "force_update",
            Action::Redact => "redact",
            Action::Purge => "purge",
            Action::ManagePolicy => "manage_policy",
            Action::ManageSettings => "manage_settings",
            Action::IssuesWrite => "issues_write",
        }
    }

    /// Parses an action name.
    pub fn parse(s: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.as_str() == s)
    }

    /// Which path permission the action needs.
    pub fn path_mode(self) -> PathMode {
        match self {
            Action::Read | Action::Review => PathMode::Read,
            Action::Commit | Action::Lease | Action::Land => PathMode::Write,
            _ => PathMode::None,
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Names of the built-in roles, weakest first.
pub const BUILTIN_ROLES: [&str; 5] = ["read", "triage", "write", "maintain", "admin"];

/// Actions granted by a built-in role, or `None` if `name` is not built in.
pub fn builtin_role_actions(name: &str) -> Option<BTreeSet<Action>> {
    use Action::*;
    let list: &[Action] = match name {
        "read" => &[Read, IssuesWrite],
        "triage" => &[Read, IssuesWrite, Triage],
        "write" => &[Read, IssuesWrite, Triage, Review, Commit, Lease, Land],
        "maintain" => &[
            Read,
            IssuesWrite,
            Triage,
            Review,
            Commit,
            Lease,
            Land,
            ManageRefs,
            ManageSettings,
        ],
        "admin" => &Action::ALL,
        _ => return None,
    };
    Some(list.iter().copied().collect())
}

/// Semantic diff class of a path (SPEC §11.3), lowest risk first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiffClass {
    /// Matches a generated-path rule.
    Generated,
    /// Documentation.
    Docs,
    /// Only comments and whitespace changed.
    CommentOnly,
    /// Only test paths changed.
    TestOnly,
    /// Content-identical move.
    RenameOnly,
    /// Dependency manifests and lockfiles.
    Dependency,
    /// Anything else.
    Logic,
    /// A path policy marks sensitive.
    Sensitive,
}

impl DiffClass {
    /// Every class, lowest risk first.
    pub const ALL: [DiffClass; 8] = [
        DiffClass::Generated,
        DiffClass::Docs,
        DiffClass::CommentOnly,
        DiffClass::TestOnly,
        DiffClass::RenameOnly,
        DiffClass::Dependency,
        DiffClass::Logic,
        DiffClass::Sensitive,
    ];

    /// The name used in policy files.
    pub fn as_str(self) -> &'static str {
        match self {
            DiffClass::Generated => "generated",
            DiffClass::Docs => "docs",
            DiffClass::CommentOnly => "comment_only",
            DiffClass::TestOnly => "test_only",
            DiffClass::RenameOnly => "rename_only",
            DiffClass::Dependency => "dependency",
            DiffClass::Logic => "logic",
            DiffClass::Sensitive => "sensitive",
        }
    }

    /// Parses a class name.
    pub fn parse(s: &str) -> Option<DiffClass> {
        DiffClass::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

impl fmt::Display for DiffClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_nested() {
        let mut prev = BTreeSet::new();
        for r in BUILTIN_ROLES {
            let cur = builtin_role_actions(r).unwrap();
            assert!(prev.is_subset(&cur), "{r} must include the weaker role");
            prev = cur;
        }
        assert!(builtin_role_actions("owner").is_none());
    }

    #[test]
    fn names_round_trip() {
        for a in Action::ALL {
            assert_eq!(Action::parse(a.as_str()), Some(a));
        }
        for c in DiffClass::ALL {
            assert_eq!(DiffClass::parse(c.as_str()), Some(c));
        }
    }
}
