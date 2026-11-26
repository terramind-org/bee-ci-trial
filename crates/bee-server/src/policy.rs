//! Effective policy per repo and ref (SPEC §9.4): org defaults overlaid by
//! `.beehive/policy.toml` at the tip of the ref, with the last good policy kept when a tip
//! carries an invalid one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bee_core::{ObjectId, RepoPath};
use bee_history::{History, PathContent};
use bee_policy::{Codeowners, Policy};
use bee_store::{OrgId, RepoId};

use crate::errors::{ApiError, ApiResult};
use crate::logging::{self, Level};

/// Where the repo policy file lives.
pub const POLICY_PATH: &str = ".beehive/policy.toml";

/// The built-in org default: every authenticated principal reads and writes, org admins and
/// principals whose own forge repo role is admin administer (`@repo-role/admin`, SPEC §9.4),
/// those whose own role is maintain maintain (`@repo-role/maintain`), and `main` and
/// `release/*` are protected (they move only through Land, with no checks or approvals
/// required and agents allowed to land every class). Repos tighten it in
/// `.beehive/policy.toml`.
pub fn builtin_org_default(admins: &[String]) -> String {
    let admins = admins
        .iter()
        .map(|a| format!("{a:?}"))
        .chain(std::iter::once(format!(
            "\"@{}admin\"",
            bee_policy::REPO_ROLE_GROUP_PREFIX
        )))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "version = 1\n\
         [members]\n\
         admin = [{admins}]\n\
         maintain = [\"@{prefix}maintain\"]\n\
         write = [\"@humans\", \"@agents\", \"@services\"]\n\
         read = [\"@everyone\"]\n\
         [[access]]\n\
         path = \"/\"\n\
         read = [\"@everyone\"]\n\
         write = [\"@everyone\"]\n\
         [[land]]\n\
         ref = \"main\"\n\
         agents_may_land_without_human = [{all}]\n\
         [[land]]\n\
         ref = \"release/*\"\n\
         agents_may_land_without_human = [{all}]\n",
        prefix = bee_policy::REPO_ROLE_GROUP_PREFIX,
        all = bee_policy::DiffClass::ALL
            .iter()
            .map(|c| format!("{:?}", c.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Reads a text file at `path` in `changeset`, `None` when absent or not a file.
pub async fn read_text(
    history: &History,
    changeset: &ObjectId,
    path: &str,
) -> ApiResult<Option<String>> {
    let p = RepoPath::new(path).map_err(ApiError::from)?;
    match history.read_path(changeset, Some(&p)).await {
        Ok(PathContent::File { content, .. }) => Ok(Some(
            String::from_utf8_lossy(content.bytes_or_empty()).into_owned(),
        )),
        Ok(_) => Ok(None),
        Err(e) if e.reason == bee_core::Reason::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Builds the effective policy for org text plus the repo file (and its `CODEOWNERS`) at
/// `tip`. Errors are `INVALID_POLICY`-shaped `INVALID_ARGUMENT` with the location.
pub async fn evaluate_at(
    history: &History,
    org_text: &str,
    tip: Option<&ObjectId>,
) -> ApiResult<Policy> {
    let repo_text = match tip {
        Some(t) => read_text(history, t, POLICY_PATH).await?,
        None => None,
    };
    let mut policy = Policy::from_sources(Some(org_text), repo_text.as_deref())
        .map_err(|e| ApiError::invalid("invalid_policy", format!("invalid policy: {e}")))?;
    if let (Some(t), Some(file)) = (tip, policy.doc().codeowners.clone()) {
        let file = file.trim_start_matches('/').to_string();
        if let Some(text) = read_text(history, t, &file).await? {
            let co = Codeowners::parse(&text).map_err(|e| {
                ApiError::invalid("invalid_policy", format!("invalid CODEOWNERS: {e}"))
            })?;
            policy = policy.with_codeowners(co);
        }
    }
    Ok(policy)
}

/// Cache key: repo, ref name and tip.
type TipKey = (RepoId, String, Option<ObjectId>);

/// Cached effective policies keyed by repo, ref and tip.
#[derive(Default)]
pub struct PolicyCache {
    by_tip: Mutex<HashMap<TipKey, Arc<Policy>>>,
    last_good: Mutex<HashMap<(RepoId, String), Arc<Policy>>>,
    /// Org default texts with when they were read: other processes sharing the store change
    /// the inputs (the org admin list) without telling this one, so a text is reused only
    /// while it is younger than the caller's bound.
    org_texts: Mutex<HashMap<OrgId, (std::time::Instant, String)>>,
}

impl PolicyCache {
    /// Sets the org default text (validated by the caller). Cached policies are built from
    /// it, so they are dropped when it changed; the same text again only renews its age.
    pub fn set_org_text(&self, org: OrgId, text: String) {
        let changed = self
            .org_texts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(org, (std::time::Instant::now(), text.clone()))
            .is_none_or(|(_, old)| old != text);
        if !changed {
            return;
        }
        self.by_tip
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// Drops the cached org default (for example after the admin list changed).
    pub fn forget_org(&self, org: OrgId) {
        self.org_texts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&org);
        self.by_tip
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        self.last_good
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// Drops every cached policy (after an API ruleset or custom role changed).
    pub fn forget_all(&self) {
        self.org_texts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        self.by_tip
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        self.last_good
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// The org default text, if set less than `max_age` ago.
    pub fn org_text(&self, org: OrgId, max_age: std::time::Duration) -> Option<String> {
        self.org_texts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&org)
            .filter(|(at, _)| at.elapsed() < max_age)
            .map(|(_, t)| t.clone())
    }

    /// The effective policy of `ref_name` whose tip is `tip`. An invalid policy at the tip
    /// falls back to the last good one for that ref (or the org default alone).
    pub async fn get(
        &self,
        history: &History,
        org_text: &str,
        ref_name: &str,
        tip: Option<&ObjectId>,
    ) -> ApiResult<Arc<Policy>> {
        let key = (history.repo(), ref_name.to_string(), tip.copied());
        if let Some(p) = self
            .by_tip
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
        {
            return Ok(p.clone());
        }
        let policy = match evaluate_at(history, org_text, tip).await {
            Ok(p) => Arc::new(p),
            Err(e) => {
                logging::log(
                    Level::Warn,
                    "policy",
                    "invalid policy at tip; serving the last good policy",
                    &[
                        ("repo", history.repo().0.to_text().into()),
                        ("error", e.detail.message.clone().into()),
                    ],
                );
                let last = self
                    .last_good
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&(history.repo(), ref_name.to_string()))
                    .cloned();
                match last {
                    Some(p) => p,
                    None => Arc::new(evaluate_at(history, org_text, None).await?),
                }
            }
        };
        let mut m = self.by_tip.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() > 4096 {
            m.clear();
        }
        m.insert(key, policy.clone());
        self.last_good
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((history.repo(), ref_name.to_string()), policy.clone());
        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_default_parses_and_protects_main() {
        let p =
            Policy::from_sources(Some(&builtin_org_default(&["01J0ADMIN".into()])), None).unwrap();
        assert!(p.is_protected("main"));
        assert!(p.is_protected("release/1"));
        assert!(!p.is_protected("ws/x/y"));
        let p = Policy::from_sources(Some(&builtin_org_default(&[])), None).unwrap();
        assert!(!p.is_protected("green/ci"));
    }

    #[test]
    fn builtin_default_gives_repo_admins_and_maintainers_their_roles() {
        use bee_policy::{Action, Actor, PrincipalKind};
        let p =
            Policy::from_sources(Some(&builtin_org_default(&["01J0ADMIN".into()])), None).unwrap();
        let agent =
            |role: Option<&str>| Actor::new("01J0BOT", PrincipalKind::Agent).with_repo_role(role);
        let admin = p.actions_of(&agent(Some("admin")));
        assert!(admin.contains(&Action::ManagePolicy) && admin.contains(&Action::ForceUpdate));
        let maintain = p.actions_of(&agent(Some("maintain")));
        assert!(maintain.contains(&Action::ManageRefs));
        assert!(!maintain.contains(&Action::ManagePolicy));
        for role in [None, Some("read"), Some("triage"), Some("write")] {
            let a = p.actions_of(&agent(role));
            assert!(a.contains(&Action::Commit), "{role:?}");
            assert!(!a.contains(&Action::ManageRefs), "{role:?}");
        }
        let listed = p.actions_of(&Actor::new("01J0ADMIN", PrincipalKind::Human));
        assert!(listed.contains(&Action::ManagePolicy));
        // A repo `[members]` entry replaces the org default's entry for that role only.
        let repo = "version = 1\n[members]\nadmin = [\"01J0OTHER\"]\n";
        let p = Policy::from_sources(Some(&builtin_org_default(&[])), Some(repo)).unwrap();
        assert!(
            !p.actions_of(&agent(Some("admin")))
                .contains(&Action::ManagePolicy)
        );
        assert!(
            p.actions_of(&agent(Some("maintain")))
                .contains(&Action::ManageRefs)
        );
    }
}
