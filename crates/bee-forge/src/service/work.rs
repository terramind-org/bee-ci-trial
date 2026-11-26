//! Shared pieces of `IssueService` and `ProjectService`: access levels, principal and
//! reference resolution, and the repo-name placeholders the store leaves for repos a call is
//! not about.

use std::collections::{BTreeMap, BTreeSet};

use bee_policy::Action;
use bee_proto::v1::{
    ErrorReason, Issue, IssueLinkedChange, IssueSummary, IssueTimelineEvent, Principal,
    ProjectChartSeries, ProjectItem, Repo, RepoRef, RepoRole, repo_ref,
};

use super::ForgeServices;
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::work::text::{self, IssueReference};
use crate::work::{Actor, NAME_PLACEHOLDER, RepoInfo, ResolvedRef, TextEffects, is_agent};

/// How much a call needs on the repo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Level {
    /// Reads: the `read` action; no audit, works on archived repos and with issues disabled.
    View,
    /// Opening issues and commenting: `issues_write`, which every role has (any reader may, as
    /// on GitHub); the store checks authorship.
    Read,
    /// Managing issues: labels, assignees, milestones, locking, pinning, relations.
    Triage,
    /// Changing repo content or moving issues between repos: the write role.
    Write,
    /// Deleting issues: repo admin.
    Admin,
}

/// Something that holds repo names the store may have left as placeholders.
pub(super) trait Slots {
    fn slots(&mut self, f: &mut dyn FnMut(&mut String));
}

impl Slots for IssueSummary {
    fn slots(&mut self, f: &mut dyn FnMut(&mut String)) {
        f(&mut self.repo_full_name);
        f(&mut self.url);
    }
}

impl Slots for Issue {
    fn slots(&mut self, f: &mut dyn FnMut(&mut String)) {
        f(&mut self.repo_full_name);
        f(&mut self.url);
        if let Some(p) = self.parent.as_mut() {
            p.slots(f);
        }
    }
}

impl Slots for IssueTimelineEvent {
    fn slots(&mut self, f: &mut dyn FnMut(&mut String)) {
        if let Some(s) = self.related_issue.as_mut() {
            s.slots(f);
        }
    }
}

impl Slots for IssueLinkedChange {
    fn slots(&mut self, f: &mut dyn FnMut(&mut String)) {
        f(&mut self.repo_full_name);
    }
}

impl Slots for ProjectItem {
    fn slots(&mut self, f: &mut dyn FnMut(&mut String)) {
        f(&mut self.repo_full_name);
        for v in &mut self.field_values {
            f(&mut v.display_value);
            if let Some(bee_proto::v1::project_field_value::Value::Text(t)) = v.value.as_mut() {
                f(t);
            }
        }
    }
}

impl Slots for ProjectChartSeries {
    fn slots(&mut self, f: &mut dyn FnMut(&mut String)) {
        f(&mut self.name);
        for p in &mut self.points {
            f(&mut p.label);
        }
    }
}

fn collect_ids(s: &str, out: &mut BTreeSet<String>) {
    for (i, _) in s.match_indices(NAME_PLACEHOLDER) {
        let start = i + NAME_PLACEHOLDER.len();
        if let Some(id) = s.get(start..start + 26) {
            out.insert(id.to_string());
        }
    }
}

fn replace_ids(s: &mut String, names: &BTreeMap<String, String>) {
    if !s.contains(NAME_PLACEHOLDER) {
        return;
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = rest.find(NAME_PLACEHOLDER) {
        out.push_str(&rest[..i]);
        let after = &rest[i + NAME_PLACEHOLDER.len()..];
        match after
            .get(..26)
            .and_then(|id| names.get(id).map(|n| (id.len(), n)))
        {
            Some((len, name)) => {
                out.push_str(name);
                rest = &after[len..];
            }
            None => {
                out.push_str(NAME_PLACEHOLDER);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    *s = out;
}

impl ForgeServices {
    /// Authorizes a repo call at `level`. Everything but `View` is state changing: denials are
    /// audited as `audit`, archived repos answer `ARCHIVED` and repos with issues disabled
    /// `LOCKED`.
    pub(super) async fn issue_access(
        &self,
        caller: &Caller,
        repo: &Repo,
        level: Level,
        audit: &str,
    ) -> Result<(Cx, Actor)> {
        let (action, min_role) = match level {
            Level::View => (Action::Read, None),
            Level::Read => (Action::IssuesWrite, None),
            Level::Triage => (Action::Triage, None),
            Level::Write => (Action::Commit, None),
            Level::Admin => (Action::ManageSettings, Some(RepoRole::Admin)),
        };
        let auth = self
            .forge
            .authz
            .repo(caller, repo, action, &[], None, min_role)
            .await;
        let cx = if level == Level::View {
            auth?
        } else {
            self.guarded(caller, &repo.org_id, &repo.id, audit, auth)
                .await?
        };
        if level != Level::View {
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            let settings = self.forge.orgs.repo_settings(&cx, &repo.id).await?;
            if !settings.has_issues {
                return Err(ForgeError::new(
                    ErrorReason::Locked,
                    format!("issues are disabled for {}", repo.full_name),
                ));
            }
        }
        let actor = self.actor_of(&cx, caller, repo, level).await?;
        Ok((cx, actor))
    }

    async fn actor_of(&self, cx: &Cx, caller: &Caller, repo: &Repo, level: Level) -> Result<Actor> {
        let mut principal = self
            .forge
            .hooks
            .directory
            .get(&cx.actor_id)
            .await
            .unwrap_or_else(|| cx.principal());
        if principal.kind == 0 {
            principal.kind = cx.actor_kind as i32;
        }
        let triage = match level {
            Level::View => false,
            Level::Read => self
                .forge
                .authz
                .repo(caller, repo, Action::Triage, &[], None, None)
                .await
                .is_ok(),
            _ => true,
        };
        let association = match self
            .forge
            .orgs
            .member(cx, &repo.org_id, &cx.actor_id)
            .await?
        {
            Some(m) if m.role == bee_proto::v1::Role::Owner as i32 => "OWNER",
            Some(_) => "MEMBER",
            None if triage => "COLLABORATOR",
            None => "NONE",
        };
        Ok(Actor {
            principal,
            triage,
            admin: level == Level::Admin,
            association: association.into(),
        })
    }

    /// Org-level management (issue types and fields): org owners and admins.
    pub(super) async fn org_manage(
        &self,
        caller: &Caller,
        org: &str,
        audit: &str,
    ) -> Result<(Cx, String)> {
        let org_id = self.org_id(caller, org).await?;
        let auth = self.forge.authz.org(caller, &org_id, OrgPerm::Manage).await;
        let cx = self.guarded(caller, &org_id, "", audit, auth).await?;
        Ok((cx, org_id))
    }

    /// Org-level reads: members.
    pub(super) async fn org_view(&self, caller: &Caller, org: &str) -> Result<(Cx, String)> {
        let org_id = self.org_id(caller, org).await?;
        let cx = self
            .forge
            .authz
            .org(caller, &org_id, OrgPerm::ViewMembers)
            .await?;
        Ok((cx, org_id))
    }

    /// A principal by id or login.
    pub(super) async fn principal_of(&self, handle: &str) -> Result<Principal> {
        let dir = &self.forge.hooks.directory;
        if let Some(p) = dir.get(handle).await {
            return Ok(p);
        }
        dir.by_login(handle.trim_start_matches('@'))
            .await
            .ok_or_else(|| {
                ForgeError::invalid("unknown_principal", format!("no user or agent {handle:?}"))
            })
    }

    /// Principals that may be assigned in `repo`: members and collaborators with a role.
    pub(super) async fn assignable(&self, cx: &Cx, repo: &Repo) -> Result<Vec<Principal>> {
        let all = self.forge.orgs.collaborators(cx, &repo.id, false).await?;
        let mut out = Vec::new();
        for c in all {
            if c.role == RepoRole::None as i32 {
                continue;
            }
            let Some(mut p) = c.principal else { continue };
            if let Some(known) = self.forge.hooks.directory.get(&p.id).await {
                p = known;
            }
            out.push(p);
        }
        Ok(out)
    }

    /// Resolves assignee handles and checks each may be assigned.
    pub(super) async fn assignees_of(
        &self,
        cx: &Cx,
        repo: &Repo,
        handles: &[String],
    ) -> Result<Vec<Principal>> {
        if handles.is_empty() {
            return Ok(Vec::new());
        }
        let ok = self.assignable(cx, repo).await?;
        let mut out: Vec<Principal> = Vec::new();
        for h in handles {
            let p = self.principal_of(h).await?;
            if !ok.iter().any(|a| a.id == p.id) {
                return Err(ForgeError::invalid(
                    "not_assignable",
                    format!("{:?} cannot be assigned in {}", h, repo.full_name),
                ));
            }
            if !out.iter().any(|x| x.id == p.id) {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// The agent runs to start for the agents among `principals`; a funding failure aborts.
    pub(super) async fn agent_runs(
        &self,
        repo: &Repo,
        principals: &[Principal],
        instructions: &str,
        base_ref: &str,
    ) -> Result<Vec<crate::work::AgentAssignment>> {
        let gate = self.forge.work.hooks().agents;
        let mut out = Vec::new();
        for p in principals.iter().filter(|p| is_agent(p)) {
            if gate.admit(repo, p).await? {
                out.push(crate::work::AgentAssignment {
                    agent: p.clone(),
                    instructions: instructions.to_string(),
                    base_ref: base_ref.to_string(),
                });
            }
        }
        Ok(out)
    }

    /// The caller's login for `@me` (the first registered login, else the id).
    pub(super) async fn login_of(&self, id: &str) -> String {
        let (logins, _) = self.forge.hooks.directory.identities(id).await;
        logins.into_iter().next().unwrap_or_else(|| id.to_string())
    }

    /// Mentions and references found in `body`, resolved. A reference is recorded on the
    /// target issue only when the target is readable by the caller and the source repo is
    /// public, the same repo or in the same org (no private repo name leaks).
    pub(super) async fn effects_of(&self, caller: &Caller, repo: &Repo, body: &str) -> TextEffects {
        let mut fx = TextEffects::default();
        for h in text::mentions(body).into_iter().take(50) {
            if let Ok(p) = self.principal_of(&h).await {
                fx.mentions.push(p);
            }
        }
        let public = repo.visibility == bee_proto::v1::Visibility::Public as i32;
        for IssueReference {
            repo: named,
            number,
        } in text::references(body).into_iter().take(50)
        {
            let target = match named {
                None => repo.clone(),
                Some(full) => {
                    let r = RepoRef {
                        repo: Some(repo_ref::Repo::FullName(full)),
                    };
                    match self.forge.orgs.resolve_repo(&caller.cx, &r).await {
                        Ok(t) => t,
                        Err(_) => continue,
                    }
                }
            };
            let allowed = target.id == repo.id || target.org_id == repo.org_id || public;
            if !allowed
                || self
                    .forge
                    .authz
                    .repo(caller, &target, Action::Read, &[], None, None)
                    .await
                    .is_err()
            {
                continue;
            }
            fx.refs.push(ResolvedRef {
                repo_id: target.id,
                number,
            });
        }
        fx
    }

    /// Replaces repo-name placeholders in `items` with the repos' current names.
    pub(super) async fn fix_names<T: Slots>(&self, cx: &Cx, items: &mut [T]) {
        let mut ids = BTreeSet::new();
        for it in items.iter_mut() {
            it.slots(&mut |s| collect_ids(s, &mut ids));
        }
        if ids.is_empty() {
            return;
        }
        let mut names = BTreeMap::new();
        for id in ids {
            if let Ok(r) = self.forge.orgs.repo_any(cx, &id).await {
                names.insert(id, r.full_name);
            }
        }
        for it in items.iter_mut() {
            it.slots(&mut |s| replace_ids(s, &names));
        }
    }

    /// One value, same as [`Self::fix_names`].
    pub(super) async fn fix_one<T: Slots>(&self, cx: &Cx, item: &mut T) {
        self.fix_names(cx, std::slice::from_mut(item)).await;
    }

    /// `RepoInfo` of a repo.
    pub(super) fn info(repo: &Repo) -> RepoInfo {
        RepoInfo::from(repo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_replaced() {
        let id = "01JABCDEFGHJKMNPQRSTVWXYZ0";
        assert_eq!(id.len(), 26);
        let mut names = BTreeMap::new();
        names.insert(id.to_string(), "org/repo".to_string());
        let mut s = format!("/#id:{id}/issues/3 and #id:{id}");
        replace_ids(&mut s, &names);
        assert_eq!(s, "/org/repo/issues/3 and org/repo");
        let mut unknown = "#id:01JZZZZZZZZZZZZZZZZZZZZZZZ".to_string();
        replace_ids(&mut unknown, &names);
        assert!(unknown.starts_with("#id:"));
        let mut set = BTreeSet::new();
        collect_ids(&format!("x #id:{id} y"), &mut set);
        assert!(set.contains(id));
    }
}
