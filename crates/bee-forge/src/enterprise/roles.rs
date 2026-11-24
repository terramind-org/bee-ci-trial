//! Custom repo and org roles: permission bundles on top of a built-in role, evaluated through
//! `bee-policy`.
//!
//! A custom *repo* role has a base role (`read`, `triage`, `write`, `maintain`) and adds
//! permissions from the catalog. Evaluation does not re-implement the action algebra: the role is
//! rendered as a `[roles.<id>]` section of a policy document and `bee_policy::Policy` resolves
//! it ([`custom_repo_actions`]), so a role means exactly what a role in `.beehive/policy.toml`
//! means. [`Roles::repo_actions`] answers "which policy actions does this principal hold on this
//! repo" from every source (built-in roles, team grants, direct custom grants), which is the
//! evaluation `CollaboratorPermission` cannot express (`docs/spec/forge-core.md` §11).
//!
//! A custom *org* role has base `none` or `member` and adds org permissions
//! ([`Roles::has_org_permission`]). Org roles are assigned to principals or teams.

use std::collections::BTreeSet;
use std::sync::Arc;

use bee_policy::{Action, Actor, Policy, PrincipalKind as PolicyKind, builtin_role_actions};
use bee_proto::v1::{
    CustomRole, Org, PolicyBuiltinRole, PolicyOrgRoleAssignment, PolicyPermission, PolicyRoleKind,
    Repo, RepoRole, Role, SubjectKind,
};

use super::SURFACE;
use super::common::{self, get_or_404, scan_all, scan_page};
use super::teams::{TEAM_REPOS, TEAMS};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::ts;
use crate::orgs::{role_name, role_rank};
use crate::store::{Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, TableDef, Tx};

/// Custom roles, scoped by org; `k1` lower-case name (unique).
pub static CUSTOM_ROLES: TableDef = TableDef {
    name: "forge_custom_roles",
    surface: SURFACE,
    unique_k1: true,
    unique_k2: false,
    indexes: &[],
};

/// Custom org role assignments, scoped by org; id `p:<principal>` or `t:<team>`; `k1` role id.
pub static ORG_ROLE_ASSIGNMENTS: TableDef = TableDef {
    name: "forge_custom_org_role_assignments",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Migration creating the role tables.
pub static M_ROLES: Migration = Migration {
    module: "enterprise",
    version: 2,
    name: "create_custom_roles",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&CUSTOM_ROLES),
        MigrationOp::CreateTable(&ORG_ROLE_ASSIGNMENTS),
    ],
};

/// Most custom roles in one org.
pub const MAX_CUSTOM_ROLES: u64 = 100;

struct Perm {
    name: &'static str,
    kind: PolicyRoleKind,
    category: &'static str,
    description: &'static str,
    allowed: bool,
    action: Option<Action>,
}

const fn repo_perm(
    name: &'static str,
    category: &'static str,
    description: &'static str,
    allowed: bool,
    action: Action,
) -> Perm {
    Perm {
        name,
        kind: PolicyRoleKind::Repo,
        category,
        description,
        allowed,
        action: Some(action),
    }
}

const fn org_perm(
    name: &'static str,
    category: &'static str,
    description: &'static str,
    allowed: bool,
) -> Perm {
    Perm {
        name,
        kind: PolicyRoleKind::Org,
        category,
        description,
        allowed,
        action: None,
    }
}

const CATALOG: &[Perm] = &[
    repo_perm(
        "read",
        "code",
        "Read objects, trees and history",
        true,
        Action::Read,
    ),
    repo_perm(
        "issues_write",
        "issues",
        "Open issues and comment on them",
        true,
        Action::IssuesWrite,
    ),
    repo_perm(
        "triage",
        "change_requests",
        "Manage issues, labels and change-request metadata",
        true,
        Action::Triage,
    ),
    repo_perm(
        "review",
        "change_requests",
        "Review change requests",
        true,
        Action::Review,
    ),
    repo_perm(
        "commit",
        "code",
        "Create changesets and update workspace refs",
        true,
        Action::Commit,
    ),
    repo_perm("lease", "code", "Acquire path leases", true, Action::Lease),
    repo_perm(
        "land",
        "change_requests",
        "Land onto a ref, subject to land rules",
        true,
        Action::Land,
    ),
    repo_perm(
        "manage_refs",
        "code",
        "Create, update and delete unprotected refs",
        true,
        Action::ManageRefs,
    ),
    repo_perm(
        "force_update",
        "code",
        "Move a ref to a non-descendant",
        true,
        Action::ForceUpdate,
    ),
    repo_perm(
        "redact",
        "security",
        "Redact objects",
        false,
        Action::Redact,
    ),
    repo_perm(
        "purge",
        "security",
        "Purge redacted objects",
        false,
        Action::Purge,
    ),
    repo_perm(
        "manage_policy",
        "admin",
        "Change the repo policy",
        false,
        Action::ManagePolicy,
    ),
    repo_perm(
        "manage_settings",
        "admin",
        "Change repo settings",
        true,
        Action::ManageSettings,
    ),
    org_perm("create_repos", "admin", "Create repos in the org", true),
    org_perm("manage_teams", "admin", "Create and edit teams", true),
    org_perm("manage_members", "admin", "Invite and remove members", true),
    org_perm("manage_apps", "admin", "Install and configure apps", true),
    org_perm("manage_billing", "admin", "Read and change billing", true),
    org_perm(
        "moderate_content",
        "moderation",
        "Hide, lock, block and resolve reports",
        true,
    ),
    org_perm("read_audit_log", "security", "Read the org audit log", true),
    org_perm("delete_org", "admin", "Delete the org", false),
];

const BUILTIN_ORG_ROLES: &[(&str, &[&str])] = &[
    (
        "owner",
        &[
            "create_repos",
            "manage_teams",
            "manage_members",
            "manage_apps",
            "manage_billing",
            "moderate_content",
            "read_audit_log",
            "delete_org",
        ],
    ),
    (
        "admin",
        &[
            "create_repos",
            "manage_teams",
            "manage_members",
            "manage_apps",
            "manage_billing",
            "moderate_content",
            "read_audit_log",
        ],
    ),
    ("member", &["create_repos"]),
    ("billing_manager", &["manage_billing"]),
    ("security_manager", &["read_audit_log"]),
    ("moderator", &["moderate_content"]),
    ("app_manager", &["manage_apps"]),
];

/// The permission catalog (`ListPolicyPermissions`).
pub fn permission_catalog() -> Vec<PolicyPermission> {
    CATALOG
        .iter()
        .map(|p| {
            let included: Vec<String> = match p.kind {
                PolicyRoleKind::Repo => bee_policy::BUILTIN_ROLES
                    .iter()
                    .filter(|r| {
                        builtin_role_actions(r)
                            .is_some_and(|a| p.action.is_some_and(|x| a.contains(&x)))
                    })
                    .map(|r| r.to_string())
                    .collect(),
                _ => BUILTIN_ORG_ROLES
                    .iter()
                    .filter(|(_, perms)| perms.contains(&p.name))
                    .map(|(r, _)| r.to_string())
                    .collect(),
            };
            PolicyPermission {
                name: p.name.into(),
                kind: p.kind as i32,
                category: p.category.into(),
                description: p.description.into(),
                allowed_in_custom_roles: p.allowed,
                included_in_roles: included,
            }
        })
        .collect()
}

/// The built-in roles with the permissions each grants (`ListBuiltinRoles`).
pub fn builtin_roles() -> Vec<PolicyBuiltinRole> {
    let mut out = Vec::new();
    let mut prev = "";
    for name in bee_policy::BUILTIN_ROLES {
        let actions = builtin_role_actions(name).unwrap_or_default();
        out.push(PolicyBuiltinRole {
            name: name.to_string(),
            kind: PolicyRoleKind::Repo as i32,
            description: format!("Built-in repo role `{name}`"),
            permissions: Action::ALL
                .iter()
                .filter(|a| actions.contains(a))
                .map(|a| a.as_str().to_string())
                .collect(),
            inherits: prev.to_string(),
        });
        prev = name;
    }
    for (name, perms) in BUILTIN_ORG_ROLES {
        out.push(PolicyBuiltinRole {
            name: name.to_string(),
            kind: PolicyRoleKind::Org as i32,
            description: format!("Built-in org role `{name}`"),
            permissions: perms.iter().map(|p| p.to_string()).collect(),
            inherits: String::new(),
        });
    }
    out
}

fn perm(name: &str) -> Option<&'static Perm> {
    CATALOG.iter().find(|p| p.name == name)
}

/// The policy actions a custom repo role grants: the base role's plus its permissions',
/// resolved by `bee-policy` from a generated policy document.
pub fn custom_repo_actions(role: &CustomRole) -> Result<BTreeSet<Action>> {
    let mut actions = Vec::new();
    for p in &role.permissions {
        if let Some(a) = perm(p).and_then(|p| p.action) {
            actions.push(format!("\"{}\"", a.as_str()));
        }
    }
    let base = if role.base_role.is_empty() {
        "read"
    } else {
        role.base_role.as_str()
    };
    let doc = format!(
        "version = 1\n[roles.r{id}]\nbase = \"{base}\"\nactions = [{actions}]\n\
         [members]\nr{id} = [\"@everyone\"]\n\
         [[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n",
        id = role.id,
        actions = actions.join(", "),
    );
    let policy = Policy::parse(&doc).map_err(|e| {
        ForgeError::invalid(
            "invalid_role",
            format!("custom role does not evaluate: {e}"),
        )
    })?;
    Ok(policy.actions_of(&Actor::new("evaluator", PolicyKind::Human)))
}

/// A role string resolved against the built-ins and the org's custom roles.
#[derive(Clone, Debug)]
pub struct ResolvedRole {
    /// The base built-in role (for custom roles, the role they extend).
    pub base: RepoRole,
    /// The custom role, when `role` named one.
    pub custom: Option<CustomRole>,
}

impl ResolvedRole {
    /// The policy actions the role grants.
    pub fn actions(&self) -> Result<BTreeSet<Action>> {
        match &self.custom {
            Some(c) => custom_repo_actions(c),
            None => Ok(role_name(self.base)
                .and_then(builtin_role_actions)
                .unwrap_or_default()),
        }
    }
}

fn repo_role_of(name: &str) -> Option<RepoRole> {
    Some(match name {
        "read" => RepoRole::Read,
        "triage" => RepoRole::Triage,
        "write" => RepoRole::Write,
        "maintain" => RepoRole::Maintain,
        "admin" => RepoRole::Admin,
        _ => return None,
    })
}

/// A partial custom role update.
#[derive(Clone, Debug, Default)]
pub struct RolePatch {
    /// New name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// New base role.
    pub base_role: Option<String>,
    /// Replacement permission list.
    pub permissions: Option<Vec<String>>,
}

/// What a principal may do on a repo, and why.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoAccess {
    /// Policy actions held.
    pub actions: BTreeSet<Action>,
    /// Sources: `org:owner`, `team:core`, `custom:<name>`, `direct`, ...
    pub sources: Vec<String>,
}

/// Custom roles over the enterprise surface.
pub struct Roles {
    db: Db,
    forge: Arc<Forge>,
}

impl Roles {
    /// Roles over the forge's store.
    pub fn new(forge: Arc<Forge>) -> Roles {
        Roles {
            db: forge.db.clone(),
            forge,
        }
    }

    fn validate_name(name: &str) -> Result<()> {
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '.' | '-'));
        if !ok {
            return Err(ForgeError::invalid(
                "invalid_name",
                "role names are 1-64 characters of letters, digits, space, `_`, `.`, `-`",
            ));
        }
        let lower = name.to_lowercase();
        if bee_policy::BUILTIN_ROLES.contains(&lower.as_str())
            || BUILTIN_ORG_ROLES.iter().any(|(r, _)| *r == lower)
        {
            return Err(ForgeError::invalid(
                "reserved_name",
                format!("`{name}` is a built-in role"),
            ));
        }
        Ok(())
    }

    fn validate_shape(kind: PolicyRoleKind, base: &str, perms: &[String]) -> Result<String> {
        let base = match (kind, base) {
            (PolicyRoleKind::Repo, "") => "read".to_string(),
            (PolicyRoleKind::Repo, b @ ("read" | "triage" | "write" | "maintain")) => b.to_string(),
            (PolicyRoleKind::Org, "") => "none".to_string(),
            (PolicyRoleKind::Org, b @ ("none" | "member")) => b.to_string(),
            (PolicyRoleKind::Unspecified, _) => {
                return Err(ForgeError::missing("kind"));
            }
            _ => {
                return Err(ForgeError::invalid(
                    "invalid_base_role",
                    format!("`{base}` is not a valid base role for this kind"),
                ));
            }
        };
        for p in perms {
            match perm(p) {
                Some(entry) if entry.kind == kind => {
                    if !entry.allowed {
                        return Err(ForgeError::invalid(
                            "permission_not_allowed",
                            format!("`{p}` cannot be part of a custom role"),
                        ));
                    }
                }
                _ => {
                    return Err(ForgeError::invalid(
                        "unknown_permission",
                        format!("`{p}` is not a permission for this role kind"),
                    ));
                }
            }
        }
        Ok(base)
    }

    fn fill(mut r: CustomRole, org: &Org) -> CustomRole {
        r.org = org.name.clone();
        r
    }

    /// Creates a custom role.
    pub async fn create(
        &self,
        cx: &Cx,
        org: &Org,
        kind: PolicyRoleKind,
        name: &str,
        description: &str,
        base_role: &str,
        permissions: Vec<String>,
    ) -> Result<CustomRole> {
        Self::validate_name(name)?;
        let base = Self::validate_shape(kind, base_role, &permissions)?;
        let mut permissions = permissions;
        permissions.sort();
        permissions.dedup();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let lower = name.to_lowercase();
        let existing = tx
            .scan(
                &CUSTOM_ROLES,
                &Scan::scope(org.id.clone(), 1).key(Key::K1(lower.clone())),
            )
            .await?;
        if !existing.is_empty() {
            return Err(ForgeError::already_exists(format!("custom role {name}")));
        }
        if tx.count(&CUSTOM_ROLES, &org.id, None).await? >= MAX_CUSTOM_ROLES {
            return Err(ForgeError::too_large(
                "custom_roles_per_org",
                MAX_CUSTOM_ROLES,
                MAX_CUSTOM_ROLES + 1,
            ));
        }
        let now = tx.now_ms();
        let role = CustomRole {
            id: tx.new_id()?,
            org: org.name.clone(),
            kind: kind as i32,
            name: name.into(),
            description: description.into(),
            base_role: base,
            permissions,
            assignment_count: 0,
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
        };
        if kind == PolicyRoleKind::Repo {
            custom_repo_actions(&role)?;
        }
        tx.insert(
            &CUSTOM_ROLES,
            &Row::new(org.id.clone(), role.id.clone())
                .k1(lower)
                .msg(&role),
        )
        .await?;
        common::emit(&mut tx, "role.created", &org.id, None, common::json(&role)).await?;
        tx.audit(
            AuditDraft::new("role.create", &org.id)
                .target_id(&role.id)
                .after(&common::json(&role)),
        )
        .await?;
        tx.commit().await?;
        Ok(role)
    }

    async fn assignments_of(tx: &mut Tx, org_id: &str, role_id: &str) -> Result<u32> {
        let teams = tx
            .scan(
                &TEAM_REPOS,
                &Scan {
                    key: Some(Key::K2(role_id.to_string())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
            .len() as u64;
        let org_roles = tx
            .count(
                &ORG_ROLE_ASSIGNMENTS,
                org_id,
                Some(&Key::K1(role_id.to_string())),
            )
            .await?;
        Ok((teams + org_roles) as u32)
    }

    /// Reads one custom role with its assignment count.
    pub async fn get(&self, cx: &Cx, org: &Org, role_id: &str) -> Result<CustomRole> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut r, _) =
            get_or_404::<CustomRole>(&mut tx, &CUSTOM_ROLES, &org.id, role_id, "custom role")
                .await?;
        r.assignment_count = Self::assignments_of(&mut tx, &org.id, &r.id).await?;
        Ok(Self::fill(r, org))
    }

    /// Lists custom roles, optionally of one kind.
    pub async fn list(
        &self,
        cx: &Cx,
        org: &Org,
        kind: PolicyRoleKind,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<CustomRole>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) =
            scan_page::<CustomRole>(&mut tx, &CUSTOM_ROLES, &org.id, None, after, limit).await?;
        let mut out = Vec::new();
        for (mut r, _) in rows {
            if kind != PolicyRoleKind::Unspecified && r.kind != kind as i32 {
                continue;
            }
            r.assignment_count = Self::assignments_of(&mut tx, &org.id, &r.id).await?;
            out.push(Self::fill(r, org));
        }
        Ok((out, next))
    }

    /// Changes a custom role; holders' access changes at once.
    pub async fn update(
        &self,
        cx: &Cx,
        org: &Org,
        role_id: &str,
        patch: RolePatch,
    ) -> Result<CustomRole> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut r, row) =
            get_or_404::<CustomRole>(&mut tx, &CUSTOM_ROLES, &org.id, role_id, "custom role")
                .await?;
        let before = common::json(&r);
        let mut next = row.clone();
        if let Some(n) = &patch.name {
            Self::validate_name(n)?;
            let lower = n.to_lowercase();
            let dup = tx
                .scan(
                    &CUSTOM_ROLES,
                    &Scan::scope(org.id.clone(), 1).key(Key::K1(lower.clone())),
                )
                .await?;
            if dup.iter().any(|d| d.id != r.id) {
                return Err(ForgeError::already_exists(format!("custom role {n}")));
            }
            r.name = n.clone();
            next = next.k1(lower);
        }
        if let Some(d) = patch.description {
            r.description = d;
        }
        let kind = PolicyRoleKind::try_from(r.kind).unwrap_or(PolicyRoleKind::Repo);
        let base_in = patch
            .base_role
            .clone()
            .unwrap_or_else(|| r.base_role.clone());
        let mut perms = patch
            .permissions
            .clone()
            .unwrap_or_else(|| r.permissions.clone());
        perms.sort();
        perms.dedup();
        r.base_role = Self::validate_shape(kind, &base_in, &perms)?;
        r.permissions = perms;
        r.updated_at = Some(ts(tx.now_ms()));
        if kind == PolicyRoleKind::Repo {
            custom_repo_actions(&r)?;
        }
        tx.update(&CUSTOM_ROLES, &next.msg(&r)).await?;
        common::emit(&mut tx, "role.updated", &org.id, None, common::json(&r)).await?;
        tx.audit(
            AuditDraft::new("role.update", &org.id)
                .target_id(&r.id)
                .before(&before)
                .after(&common::json(&r)),
        )
        .await?;
        tx.commit().await?;
        Ok(Self::fill(r, org))
    }

    /// Deletes a custom role. Team grants of it fall back to the base role; org role
    /// assignments are removed.
    pub async fn delete(&self, cx: &Cx, org: &Org, role_id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (r, _) =
            get_or_404::<CustomRole>(&mut tx, &CUSTOM_ROLES, &org.id, role_id, "custom role")
                .await?;
        let grants = tx
            .scan_msgs::<bee_proto::v1::TeamRepoAccess>(
                &TEAM_REPOS,
                &Scan {
                    key: Some(Key::K2(role_id.to_string())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        for (mut g, row) in grants {
            g.role = r.base_role.clone();
            let mut next = row.clone().k2(r.base_role.clone());
            next = next.msg(&g);
            tx.update(&TEAM_REPOS, &next).await?;
        }
        let assigned = scan_all::<PolicyOrgRoleAssignment>(
            &mut tx,
            &ORG_ROLE_ASSIGNMENTS,
            &org.id,
            Some(Key::K1(role_id.to_string())),
        )
        .await?;
        for (_, row) in assigned {
            tx.delete(&ORG_ROLE_ASSIGNMENTS, &org.id, &row.id).await?;
        }
        tx.delete(&CUSTOM_ROLES, &org.id, role_id).await?;
        common::emit(&mut tx, "role.deleted", &org.id, None, common::json(&r)).await?;
        tx.audit(
            AuditDraft::new("role.delete", &org.id)
                .target_id(&r.id)
                .before(&common::json(&r)),
        )
        .await?;
        tx.commit().await
    }

    /// Resolves a role string inside a transaction: a built-in repo role name, or the id of a
    /// custom repo role of the org.
    pub async fn resolve_in(tx: &mut Tx, org_id: &str, role: &str) -> Result<ResolvedRole> {
        if let Some(b) = repo_role_of(role) {
            return Ok(ResolvedRole {
                base: b,
                custom: None,
            });
        }
        match tx
            .get_msg::<CustomRole>(&CUSTOM_ROLES, org_id, role)
            .await?
        {
            Some((c, _)) if c.kind == PolicyRoleKind::Repo as i32 => Ok(ResolvedRole {
                base: repo_role_of(&c.base_role).unwrap_or(RepoRole::Read),
                custom: Some(c),
            }),
            _ => Err(ForgeError::invalid(
                "unknown_role",
                format!("`{role}` is neither a built-in repo role nor a custom repo role"),
            )),
        }
    }

    /// Resolves a role string outside a transaction.
    pub async fn resolve(&self, cx: &Cx, org_id: &str, role: &str) -> Result<ResolvedRole> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::resolve_in(&mut tx, org_id, role).await
    }

    /// Assigns a custom org role to a principal or a team.
    pub async fn assign_org_role(
        &self,
        cx: &Cx,
        org: &Org,
        role_id: &str,
        principal_id: Option<&str>,
        team_id: Option<&str>,
    ) -> Result<PolicyOrgRoleAssignment> {
        use bee_proto::v1::policy_org_role_assignment::Assignee;
        let (key, assignee) = match (principal_id, team_id) {
            (Some(p), None) if !p.is_empty() => (format!("p:{p}"), Assignee::PrincipalId(p.into())),
            (None, Some(t)) if !t.is_empty() => (format!("t:{t}"), Assignee::TeamId(t.into())),
            _ => return Err(ForgeError::missing("assignee")),
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (role, _) =
            get_or_404::<CustomRole>(&mut tx, &CUSTOM_ROLES, &org.id, role_id, "custom role")
                .await?;
        if role.kind != PolicyRoleKind::Org as i32 {
            return Err(ForgeError::invalid(
                "wrong_role_kind",
                "only custom org roles are assigned here",
            ));
        }
        if let Some(t) = team_id
            && tx.get(&TEAMS, &org.id, t).await?.is_none()
        {
            return Err(ForgeError::not_found(format!("team {t}")));
        }
        let a = PolicyOrgRoleAssignment {
            org: org.name.clone(),
            role: role_id.into(),
            assignee: Some(assignee),
            assigned_by: Some(cx.principal()),
            assigned_at: Some(ts(tx.now_ms())),
        };
        tx.put(
            &ORG_ROLE_ASSIGNMENTS,
            &Row::new(org.id.clone(), key.clone())
                .k1(role_id.to_string())
                .msg(&a),
        )
        .await?;
        common::emit(&mut tx, "role.assigned", &org.id, None, common::json(&a)).await?;
        tx.audit(
            AuditDraft::new("role.assign", &org.id)
                .target(SubjectKind::Unspecified, &key)
                .after(&common::json(&a)),
        )
        .await?;
        tx.commit().await?;
        Ok(a)
    }

    /// Removes a custom org role assignment.
    pub async fn unassign_org_role(
        &self,
        cx: &Cx,
        org: &Org,
        role_id: &str,
        principal_id: Option<&str>,
        team_id: Option<&str>,
    ) -> Result<()> {
        let key = match (principal_id, team_id) {
            (Some(p), None) if !p.is_empty() => format!("p:{p}"),
            (None, Some(t)) if !t.is_empty() => format!("t:{t}"),
            _ => return Err(ForgeError::missing("assignee")),
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        match tx
            .get_msg::<PolicyOrgRoleAssignment>(&ORG_ROLE_ASSIGNMENTS, &org.id, &key)
            .await?
        {
            Some((a, row)) if a.role == role_id => {
                tx.delete(&ORG_ROLE_ASSIGNMENTS, &org.id, &row.id).await?;
            }
            _ => return Err(ForgeError::not_found(format!("assignment {key}"))),
        }
        common::emit(
            &mut tx,
            "role.unassigned",
            &org.id,
            None,
            serde_json::json!({"role": role_id, "assignee": key}).to_string(),
        )
        .await?;
        tx.audit(AuditDraft::new("role.unassign", &org.id).target(SubjectKind::Unspecified, &key))
            .await?;
        tx.commit().await
    }

    /// Lists custom org role assignments, optionally of one role.
    pub async fn list_org_assignments(
        &self,
        cx: &Cx,
        org: &Org,
        role_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<PolicyOrgRoleAssignment>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let key = (!role_id.is_empty()).then(|| Key::K1(role_id.to_string()));
        let (rows, next) = scan_page::<PolicyOrgRoleAssignment>(
            &mut tx,
            &ORG_ROLE_ASSIGNMENTS,
            &org.id,
            key,
            after,
            limit,
        )
        .await?;
        Ok((rows.into_iter().map(|(a, _)| a).collect(), next))
    }

    /// Whether a principal holds an org permission: through a built-in org role, or a custom
    /// org role assigned to it or to one of its teams (directly or through nesting).
    pub async fn has_org_permission(
        &self,
        cx: &Cx,
        org_id: &str,
        principal_id: &str,
        permission: &str,
    ) -> Result<bool> {
        let member = self.forge.orgs.member(cx, org_id, principal_id).await?;
        let Some(m) = member else { return Ok(false) };
        let builtin = match Role::try_from(m.role).unwrap_or(Role::Member) {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
            Role::BillingManager => "billing_manager",
            Role::SecurityManager => "security_manager",
            Role::Moderator => "moderator",
            Role::AppManager => "app_manager",
            Role::Unspecified => "",
        };
        if BUILTIN_ORG_ROLES
            .iter()
            .any(|(r, perms)| *r == builtin && perms.contains(&permission))
        {
            return Ok(true);
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut keys = vec![format!("p:{principal_id}")];
        for team in super::teams::Teams::team_ids_of(&mut tx, org_id, principal_id, true).await? {
            keys.push(format!("t:{team}"));
        }
        for k in keys {
            if let Some((a, _)) = tx
                .get_msg::<PolicyOrgRoleAssignment>(&ORG_ROLE_ASSIGNMENTS, org_id, &k)
                .await?
                && let Some((role, _)) = tx
                    .get_msg::<CustomRole>(&CUSTOM_ROLES, org_id, &a.role)
                    .await?
                && role.permissions.iter().any(|p| p == permission)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Custom repo roles a principal holds on a repo through teams (including parent teams'
    /// grants) and direct collaborator grants, each with a source label.
    pub async fn custom_grants(
        &self,
        cx: &Cx,
        repo: &Repo,
        principal_id: &str,
    ) -> Result<Vec<(CustomRole, String)>> {
        let mut out = Vec::new();
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            for (role, label) in
                super::teams::Teams::team_grants(&mut tx, repo, principal_id).await?
            {
                if let Some(c) = role.custom {
                    out.push((c, label));
                }
            }
        }
        let direct = self.forge.orgs.collaborators(cx, &repo.id, true).await?;
        for c in direct {
            if c.principal.as_ref().is_some_and(|p| p.id == principal_id)
                && !c.custom_role_id.is_empty()
            {
                let mut tx = self.db.begin(SURFACE, cx).await?;
                if let Some((role, _)) = tx
                    .get_msg::<CustomRole>(&CUSTOM_ROLES, &repo.org_id, &c.custom_role_id)
                    .await?
                {
                    out.push((role, "direct".into()));
                }
            }
        }
        Ok(out)
    }

    /// The policy actions `principal_id` holds on `repo`: the union of its built-in repo role
    /// (any source) and every custom repo role it holds, evaluated through `bee-policy`.
    pub async fn repo_actions(
        &self,
        cx: &Cx,
        repo: &Repo,
        principal_id: &str,
    ) -> Result<RepoAccess> {
        let perm = self
            .forge
            .orgs
            .effective_role(cx, repo, principal_id)
            .await?;
        let role = RepoRole::try_from(perm.role).unwrap_or(RepoRole::None);
        let mut access = RepoAccess {
            actions: role_name(role)
                .and_then(builtin_role_actions)
                .unwrap_or_default(),
            sources: perm.sources.clone(),
        };
        if role_rank(role) == 0 {
            access.sources.clear();
        }
        for (custom, label) in self.custom_grants(cx, repo, principal_id).await? {
            access.actions.extend(custom_repo_actions(&custom)?);
            access
                .sources
                .push(format!("custom:{}@{label}", custom.name));
        }
        Ok(access)
    }

    /// Checks `action` for every principal of a delegation chain (all of them must hold it,
    /// as in `Authz::repo`), counting custom roles. `PERMISSION_DENIED` otherwise.
    pub async fn check_chain(
        &self,
        cx: &Cx,
        repo: &Repo,
        chain: &[String],
        action: Action,
    ) -> Result<()> {
        for id in chain {
            let access = self.repo_actions(cx, repo, id).await?;
            if !access.actions.contains(&action) {
                return Err(ForgeError::permission_denied(format!(
                    "{id} does not hold `{action}` on {}",
                    repo.full_name
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(base: &str, perms: &[&str]) -> CustomRole {
        CustomRole {
            id: "01HZ".into(),
            kind: PolicyRoleKind::Repo as i32,
            base_role: base.into(),
            permissions: perms.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn custom_roles_evaluate_through_policy() {
        let a = custom_repo_actions(&role("write", &["manage_refs"])).unwrap();
        assert!(a.contains(&Action::Commit));
        assert!(a.contains(&Action::ManageRefs));
        assert!(!a.contains(&Action::ForceUpdate));
        assert!(!a.contains(&Action::ManageSettings));
        let r = custom_repo_actions(&role("read", &["review"])).unwrap();
        assert!(r.contains(&Action::Review) && !r.contains(&Action::Commit));
    }

    #[test]
    fn shape_validation() {
        let ok = Roles::validate_shape(PolicyRoleKind::Repo, "write", &["lease".into()]);
        assert_eq!(ok.unwrap(), "write");
        assert!(Roles::validate_shape(PolicyRoleKind::Repo, "admin", &[]).is_err());
        assert!(Roles::validate_shape(PolicyRoleKind::Repo, "", &["purge".into()]).is_err());
        assert!(
            Roles::validate_shape(PolicyRoleKind::Repo, "", &["manage_billing".into()]).is_err()
        );
        assert!(Roles::validate_shape(PolicyRoleKind::Org, "", &["manage_billing".into()]).is_ok());
        assert!(Roles::validate_name("admin").is_err());
        assert!(Roles::validate_name("Release Manager").is_ok());
    }

    #[test]
    fn catalog_is_consistent() {
        let cat = permission_catalog();
        let land = cat.iter().find(|p| p.name == "land").unwrap();
        assert_eq!(land.included_in_roles, vec!["write", "maintain", "admin"]);
        assert!(builtin_roles().iter().any(|r| r.name == "billing_manager"));
    }
}
