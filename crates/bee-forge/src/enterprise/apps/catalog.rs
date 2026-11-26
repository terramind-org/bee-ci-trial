//! The permission and OAuth scope catalogs.
//!
//! An app asks for *permissions* (`resource: read|write|admin`); an OAuth app asks for
//! *scopes* (`repo`, `read:org`, ...). Both end up as a set of [`bee_policy::Action`]s in a
//! token's [`bee_auth::Scope`]; the role an access source grants is the weakest built-in
//! role that covers those actions.
//!
//! | resource | read | write (adds) | admin (adds) |
//! |---|---|---|---|
//! | `metadata` | read | | |
//! | `contents` | read | commit, lease, land | manage_refs, force_update |
//! | `issues` | read | issues_write, triage | |
//! | `change_requests` | read | triage, review, commit | land |
//! | `checks` | read | commit | |
//! | `workflows` | read | commit | |
//! | `members` | (org-level; no repo actions) | | |
//! | `administration` | read | manage_settings | manage_policy, manage_refs, force_update |
//! | `packages` | read | commit | |
//! | `security_events` | read | triage | |
//! | `agents` | read | commit, lease | |
//! | `deployments` | read | commit | |
//!
//! A higher level includes the lower ones. Every installation also holds `metadata: read`.

use std::collections::{BTreeMap, BTreeSet};

use bee_policy::{Action, builtin_role_actions};
use bee_proto::v1::{AppPermission, AppPermissionLevel, RepoRole};

use crate::error::{ForgeError, Result};

/// Access level on one resource, ordered `Read < Write < Admin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Read.
    Read,
    /// Read and write.
    Write,
    /// Everything on the resource.
    Admin,
}

impl Level {
    /// `read`, `write` or `admin`.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Read => "read",
            Level::Write => "write",
            Level::Admin => "admin",
        }
    }

    /// Parses `read`, `write` or `admin`.
    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "read" => Some(Level::Read),
            "write" => Some(Level::Write),
            "admin" => Some(Level::Admin),
            _ => None,
        }
    }

    /// The proto enum.
    pub fn to_proto(self) -> AppPermissionLevel {
        match self {
            Level::Read => AppPermissionLevel::Read,
            Level::Write => AppPermissionLevel::Write,
            Level::Admin => AppPermissionLevel::Admin,
        }
    }

    /// From the proto enum (`None` for unspecified).
    pub fn from_proto(v: i32) -> Option<Level> {
        match AppPermissionLevel::try_from(v).ok()? {
            AppPermissionLevel::Read => Some(Level::Read),
            AppPermissionLevel::Write => Some(Level::Write),
            AppPermissionLevel::Admin => Some(Level::Admin),
            AppPermissionLevel::Unspecified => None,
        }
    }
}

/// Resources an app may ask permissions on.
pub const RESOURCES: &[&str] = &[
    "metadata",
    "contents",
    "issues",
    "change_requests",
    "checks",
    "workflows",
    "members",
    "administration",
    "packages",
    "security_events",
    "agents",
    "deployments",
];

/// A permission set: resource to level.
pub type Permissions = BTreeMap<String, Level>;

/// Actions a resource grants at exactly `level` (not counting the lower levels).
fn step(resource: &str, level: Level) -> &'static [Action] {
    use Action::*;
    match (resource, level) {
        ("metadata", Level::Read) => &[Read],
        ("contents", Level::Read) => &[Read],
        ("contents", Level::Write) => &[Commit, Lease, Land],
        ("contents", Level::Admin) => &[ManageRefs, ForceUpdate],
        ("issues", Level::Read) => &[Read],
        ("issues", Level::Write) => &[IssuesWrite, Triage],
        ("change_requests", Level::Read) => &[Read],
        ("change_requests", Level::Write) => &[Triage, Review, Commit],
        ("change_requests", Level::Admin) => &[Land],
        ("checks" | "workflows" | "packages" | "deployments", Level::Read) => &[Read],
        ("checks" | "workflows" | "packages" | "deployments", Level::Write) => &[Commit],
        ("administration", Level::Read) => &[Read],
        ("administration", Level::Write) => &[ManageSettings],
        ("administration", Level::Admin) => &[ManagePolicy, ManageRefs, ForceUpdate],
        ("security_events", Level::Read) => &[Read],
        ("security_events", Level::Write) => &[Triage],
        ("agents", Level::Read) => &[Read],
        ("agents", Level::Write) => &[Commit, Lease],
        _ => &[],
    }
}

/// Whether `resource` is in the catalog.
pub fn known_resource(resource: &str) -> bool {
    RESOURCES.contains(&resource)
}

/// Actions a permission grants (the level and every lower one).
pub fn actions_for(resource: &str, level: Level) -> BTreeSet<Action> {
    [Level::Read, Level::Write, Level::Admin]
        .into_iter()
        .filter(|l| *l <= level)
        .flat_map(|l| step(resource, l).iter().copied())
        .collect()
}

/// Actions of a whole permission set.
pub fn actions_of(perms: &Permissions) -> BTreeSet<Action> {
    perms.iter().flat_map(|(r, l)| actions_for(r, *l)).collect()
}

/// Adds the mandatory `metadata: read`.
pub fn with_metadata(mut perms: Permissions) -> Permissions {
    perms.entry("metadata".to_string()).or_insert(Level::Read);
    perms
}

/// Validates and normalizes proto permissions (known resources, specified levels; the
/// highest level wins for a repeated resource).
pub fn from_proto(list: &[AppPermission], subcode: &str) -> Result<Permissions> {
    let mut out = Permissions::new();
    for p in list {
        if !known_resource(&p.resource) {
            return Err(ForgeError::invalid(
                subcode,
                format!("unknown permission resource {:?}", p.resource),
            ));
        }
        let Some(level) = Level::from_proto(p.level) else {
            return Err(ForgeError::invalid(
                subcode,
                format!("permission {:?} has no level", p.resource),
            ));
        };
        let e = out.entry(p.resource.clone()).or_insert(level);
        *e = (*e).max(level);
    }
    Ok(out)
}

/// The proto form, sorted by resource.
pub fn to_proto(perms: &Permissions) -> Vec<AppPermission> {
    perms
        .iter()
        .map(|(r, l)| AppPermission {
            resource: r.clone(),
            level: l.to_proto() as i32,
        })
        .collect()
}

/// Whether `held` covers every permission in `wanted` (same or higher level).
pub fn covers(held: &Permissions, wanted: &Permissions) -> bool {
    wanted
        .iter()
        .all(|(r, l)| held.get(r).is_some_and(|h| h >= l))
}

/// The weakest built-in repo role whose actions include all of `actions`
/// (`None` role for an empty set).
pub fn role_for(actions: &BTreeSet<Action>) -> RepoRole {
    if actions.is_empty() {
        return RepoRole::None;
    }
    for (name, role) in [
        ("read", RepoRole::Read),
        ("triage", RepoRole::Triage),
        ("write", RepoRole::Write),
        ("maintain", RepoRole::Maintain),
        ("admin", RepoRole::Admin),
    ] {
        if builtin_role_actions(name).is_some_and(|set| actions.iter().all(|a| set.contains(a))) {
            return role;
        }
    }
    RepoRole::Admin
}

/// OAuth scopes and the actions they allow.
pub const SCOPES: &[(&str, &[Action])] = &[
    ("read:user", &[]),
    // OpenID Connect: an id_token for the user, and its profile claims.
    ("openid", &[]),
    ("profile", &[]),
    ("user", &[]),
    ("repo:read", &[Action::Read]),
    ("read:org", &[Action::Read]),
    (
        "repo:write",
        &[
            Action::Read,
            Action::IssuesWrite,
            Action::Triage,
            Action::Review,
            Action::Commit,
            Action::Lease,
            Action::Land,
        ],
    ),
    (
        "repo",
        &[
            Action::Read,
            Action::IssuesWrite,
            Action::Triage,
            Action::Review,
            Action::Commit,
            Action::Lease,
            Action::Land,
            Action::ManageRefs,
        ],
    ),
    ("workflow", &[Action::Read, Action::Commit]),
    ("checks", &[Action::Read, Action::Commit]),
    ("admin:org", &[Action::Read, Action::ManageSettings]),
    ("delete_repo", &[Action::Read, Action::ManageSettings]),
    ("admin:repo", &Action::ALL),
];

/// Validates scope names; empty means `read:user`. Returns the sorted, de-duplicated list.
pub fn check_scopes(scopes: &[String]) -> Result<Vec<String>> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for s in scopes {
        for part in s.split([' ', ',']).filter(|p| !p.is_empty()) {
            if !SCOPES.iter().any(|(n, _)| *n == part) {
                return Err(ForgeError::invalid(
                    "unknown_scope",
                    format!("unknown OAuth scope {part:?}"),
                ));
            }
            out.insert(part.to_string());
        }
    }
    if out.is_empty() {
        out.insert("read:user".into());
    }
    Ok(out.into_iter().collect())
}

/// Actions allowed by a set of scope names.
pub fn scope_actions(scopes: &[String]) -> BTreeSet<Action> {
    scopes
        .iter()
        .filter_map(|s| SCOPES.iter().find(|(n, _)| n == s))
        .flat_map(|(_, a)| a.iter().copied())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_accumulate() {
        let w = actions_for("contents", Level::Write);
        assert!(w.contains(&Action::Read) && w.contains(&Action::Commit));
        assert!(!w.contains(&Action::ManageRefs));
        assert!(actions_for("contents", Level::Admin).contains(&Action::ForceUpdate));
        assert!(actions_for("members", Level::Admin).is_empty());
    }

    #[test]
    fn roles_cover_actions() {
        assert_eq!(
            role_for(&actions_for("contents", Level::Read)),
            RepoRole::Read
        );
        assert_eq!(
            role_for(&actions_for("contents", Level::Write)),
            RepoRole::Write
        );
        assert_eq!(
            role_for(&actions_for("administration", Level::Write)),
            RepoRole::Maintain
        );
        assert_eq!(
            role_for(&actions_for("administration", Level::Admin)),
            RepoRole::Admin
        );
        assert_eq!(role_for(&BTreeSet::new()), RepoRole::None);
    }

    #[test]
    fn scopes_validate() {
        assert_eq!(check_scopes(&[]).unwrap(), vec!["read:user".to_string()]);
        assert!(check_scopes(&["nope".into()]).is_err());
        let s = check_scopes(&["repo read:org".into()]).unwrap();
        assert!(scope_actions(&s).contains(&Action::Commit));
    }
}
