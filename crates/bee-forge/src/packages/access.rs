//! Who may read, write and administer a package.
//!
//! A package has an effective role per principal, the highest of:
//!
//! 1. **Ownership.** The user who owns it, or an org owner or admin for an org package: admin.
//! 2. **Explicit grants.** A grant naming the principal, a team the principal is in, or a repo
//!    whose workflows may use the package. A repo grant applies only to a caller whose token is
//!    restricted to that repo (a workflow job token) and who holds at least read (write for a
//!    write grant) on the repo, so attenuating a token never widens access.
//! 3. **Repo inheritance.** For a package linked to a repo with inheritance on: repo read or
//!    triage is package read, write or maintain is package write, repo admin is package admin.
//! 4. **Visibility.** Public packages are readable by anyone, anonymous included; internal
//!    packages by every member of the owning org.
//!
//! As everywhere in the forge, every principal of the delegation chain must hold the role. The
//! token must also allow the matching policy action (`read`, `commit`, `manage_settings`), and
//! for a package linked to a repo the token's repo and path scope must admit that repo.
//! Without a role a private package is `NOT_FOUND`, so its existence stays hidden.

use bee_auth::Request as ScopeRequest;
use bee_policy::Action;
use bee_proto::v1::{
    ErrorReason, PackageAccessGrant, PackageGranteeKind, PackageOwnerKind, PackageRole,
    PackageVisibility, PrincipalKind, Repo, RepoRole, Role, repo_ref,
};

use super::manage::{Owner, Packages};
use super::model::PackageRec;
use crate::auth::{Caller, authorize_repo_scope};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::orgs::role_rank;

/// What a call needs on a package.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Need {
    /// Download, list, read metadata.
    Read,
    /// Publish, delete versions, move tags.
    Write,
    /// Visibility, links, grants, deleting the package.
    Admin,
}

impl Need {
    fn action(self) -> Action {
        match self {
            Need::Read => Action::Read,
            Need::Write => Action::Commit,
            Need::Admin => Action::ManageSettings,
        }
    }

    fn rank(self) -> u8 {
        match self {
            Need::Read => 1,
            Need::Write => 2,
            Need::Admin => 3,
        }
    }
}

fn role_rank_pkg(r: PackageRole) -> u8 {
    match r {
        PackageRole::Unspecified => 0,
        PackageRole::Read => 1,
        PackageRole::Write => 2,
        PackageRole::Admin => 3,
    }
}

/// The context of an unauthenticated caller reading a public package.
pub fn anonymous_cx() -> Cx {
    let mut cx = Cx::new("anonymous", PrincipalKind::Human);
    cx.decision = "public".into();
    cx
}

impl Packages {
    fn now_secs(&self) -> u64 {
        self.db.now_ms() / 1000
    }

    fn repo_grant_matches(caller: &Caller, repo_id: &str) -> bool {
        caller
            .token
            .scope
            .repos
            .as_ref()
            .is_some_and(|pats| pats.iter().any(|p| p.matches(repo_id)))
    }

    /// The package role of `principal_id` and the sources that gave it. `None` rank 0 means no
    /// access at all.
    async fn role_of(
        &self,
        caller: &Caller,
        p: &PackageRec,
        grants: &[PackageAccessGrant],
        repo: &Option<Repo>,
        principal_id: &str,
    ) -> Result<(u8, Vec<String>)> {
        let cx = &caller.cx;
        let mut best = 0u8;
        let mut via = Vec::new();
        let mut bump = |rank: u8, why: String| {
            if rank > 0 {
                via.push(why);
            }
            best = best.max(rank);
        };
        match p.kind() {
            PackageOwnerKind::User => {
                if p.owner_id == principal_id {
                    bump(3, "owner".into());
                }
            }
            _ => {
                if let Some(m) = self.orgs.member(cx, &p.owner_id, principal_id).await? {
                    match Role::try_from(m.role).unwrap_or(Role::Member) {
                        Role::Owner | Role::Admin => bump(3, "org:admin".into()),
                        _ => {
                            if p.vis() == PackageVisibility::Internal {
                                bump(1, "internal".into());
                            }
                        }
                    }
                }
            }
        }
        for g in grants {
            let role = role_rank_pkg(PackageRole::try_from(g.role).unwrap_or_default());
            match PackageGranteeKind::try_from(g.kind).unwrap_or_default() {
                PackageGranteeKind::Principal => {
                    if g.principal.as_ref().is_some_and(|x| x.id == principal_id) {
                        bump(role, "grant:principal".into());
                    }
                }
                PackageGranteeKind::Team => {
                    if p.kind() == PackageOwnerKind::Org
                        && self
                            .ports
                            .teams
                            .is_member(&p.owner_id, &g.team_id, principal_id)
                            .await
                    {
                        bump(role, format!("grant:team:{}", g.team_id));
                    }
                }
                PackageGranteeKind::Repo => {
                    let Some(rid) = g.repo.as_ref().and_then(|r| match &r.repo {
                        Some(repo_ref::Repo::Id(i)) => Some(i.clone()),
                        _ => None,
                    }) else {
                        continue;
                    };
                    if !Self::repo_grant_matches(caller, &rid) {
                        continue;
                    }
                    let Ok(grantee) = self.orgs.repo_any(cx, &rid).await else {
                        continue;
                    };
                    if grantee.deleted_at.is_some() {
                        continue;
                    }
                    let held = self.orgs.effective_role(cx, &grantee, principal_id).await?;
                    let have = role_rank(RepoRole::try_from(held.role).unwrap_or(RepoRole::None));
                    let want = if role >= 2 { 3 } else { 1 };
                    if have >= want {
                        bump(role, format!("grant:repo:{rid}"));
                    }
                }
                PackageGranteeKind::Unspecified => {}
            }
        }
        if p.inherit_access
            && let Some(repo) = repo
        {
            let held = self.orgs.effective_role(cx, repo, principal_id).await?;
            let rank = match RepoRole::try_from(held.role).unwrap_or(RepoRole::None) {
                RepoRole::Read | RepoRole::Triage => 1,
                RepoRole::Write | RepoRole::Maintain => 2,
                RepoRole::Admin => 3,
                _ => 0,
            };
            bump(rank, format!("repo:{}", repo.full_name));
        }
        Ok((best, via))
    }

    /// Authorizes `need` on `p` for `caller` (`None` = anonymous). Returns the call context
    /// carrying the decision for the audit log.
    pub async fn authorize(
        &self,
        caller: Option<&Caller>,
        p: &PackageRec,
        need: Need,
    ) -> Result<Cx> {
        if need == Need::Read && p.vis() == PackageVisibility::Public {
            return Ok(match caller {
                Some(c) => {
                    let mut cx = c.cx.clone();
                    cx.decision = "public".into();
                    cx
                }
                None => anonymous_cx(),
            });
        }
        let Some(caller) = caller else {
            return Err(ForgeError::new(
                ErrorReason::Unauthenticated,
                "this package needs a credential",
            ));
        };
        let action = need.action();
        if !caller.token.allows_action(action) {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                format!("token does not allow `{action}`"),
            ));
        }
        let cx = &caller.cx;
        let repo = if p.repo_id.is_empty() {
            None
        } else {
            self.orgs
                .repo_any(cx, &p.repo_id)
                .await
                .ok()
                .filter(|r| r.deleted_at.is_none())
        };
        let grants = self.grants_all(cx, p).await?;
        // A token restricted to repos must either be scoped to the linked repo or hold a repo
        // grant; one that is not restricted passes the dimension.
        let restricted = caller.token.scope.repos.is_some();
        let linked_scope_ok = match &repo {
            Some(r) if restricted => authorize_repo_scope(
                &caller.token,
                r,
                |key| ScopeRequest::new(key, action),
                self.now_secs(),
            )
            .is_ok(),
            _ => !restricted,
        };
        let mut decision = vec!["token:ok".to_string()];
        let mut lowest = u8::MAX;
        for id in &cx.chain {
            let (rank, via) = self.role_of(caller, p, &grants, &repo, id).await?;
            // A repo-restricted token that is not scoped to the linked repo only counts repo
            // grants.
            let rank = if linked_scope_ok || via.iter().any(|v| v.starts_with("grant:repo:")) {
                rank
            } else {
                0
            };
            lowest = lowest.min(rank);
            decision.push(format!("{id}:pkg:{rank} via {}", via.join("+")));
        }
        if lowest >= need.rank() {
            let mut out = cx.clone();
            out.decision = decision.join(" ");
            return Ok(out);
        }
        if lowest == 0 {
            return Err(ForgeError::not_found(format!("package {}", p.name)));
        }
        Err(ForgeError::permission_denied(format!(
            "{} does not hold {:?} on package {}",
            cx.actor_id, need, p.name
        )))
    }

    /// Whether `caller` may create a package under `owner`, optionally linked to `repo`: the
    /// user owner, an org owner or admin, or anyone who may write to the linked repo. The token
    /// must allow `commit`; one restricted to repos must be scoped to the linked repo.
    pub async fn authorize_create(
        &self,
        caller: &Caller,
        owner: &Owner,
        repo: Option<&Repo>,
    ) -> Result<Cx> {
        if !caller.token.allows_action(Action::Commit) {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "token does not allow `commit`",
            ));
        }
        if let Some(r) = repo
            && let Ok(cx) = self
                .authz
                .repo(caller, r, Action::Commit, &[], None, Some(RepoRole::Write))
                .await
            && (owner.kind == PackageOwnerKind::Org && r.org_id == owner.id
                || owner.kind == PackageOwnerKind::User)
        {
            return Ok(cx);
        }
        if caller.token.scope.repos.is_some() {
            return Err(ForgeError::permission_denied(
                "a token restricted to repos can create a package only for a repo it can write",
            ));
        }
        let cx = &caller.cx;
        for id in &cx.chain {
            let ok = match owner.kind {
                PackageOwnerKind::User => *id == owner.id,
                _ => matches!(
                    self.orgs
                        .member(cx, &owner.id, id)
                        .await?
                        .map(|m| Role::try_from(m.role).unwrap_or(Role::Member)),
                    Some(Role::Owner | Role::Admin)
                ),
            };
            if !ok {
                return Err(ForgeError::permission_denied(format!(
                    "{id} cannot create packages under {}",
                    owner.name
                )));
            }
        }
        let mut out = cx.clone();
        out.decision = format!("token:ok owner:{}", owner.name);
        Ok(out)
    }
}
