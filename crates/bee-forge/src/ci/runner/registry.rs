//! The action registry: where the local runner gets `uses: owner/repo[/path]@ref` actions.
//!
//! [`ActionSource`] is the port; [`ForgeActionSource`] is the in-process implementation over
//! the VCS port of CI ([`crate::ci::CiVcs`]): `owner/repo` is a Beehive repo, or, through a
//! configured mirror, a Beehive org holding repos imported from github.com (the importer
//! path: `actions/setup-node` → `<mirror org>/setup-node`). The ref is a changeset id, a branch
//! or a tag. Public repos and repos of the job's own org may be used; anything else is
//! `PERMISSION_DENIED`. A runner host outside the server implements [`ActionSource`] over its
//! own transport.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{ObjectId, RepoRef, RunnerJobAssignment, Visibility, repo_ref};

use crate::ci::Ci;
use crate::ci::convert::bee_ref;
use crate::ci::vcs::TreeFile;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::hex;

/// A `uses:` reference to an action in another repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRef {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Directory of the action inside the repository (empty = the root).
    pub path: String,
    /// Ref: changeset id, branch or tag.
    pub git_ref: String,
}

impl ActionRef {
    /// Parses `owner/repo[/path]@ref`; `None` for local (`./`), Docker and malformed
    /// references.
    pub fn parse(uses: &str) -> Option<ActionRef> {
        if uses.starts_with("./") || uses.starts_with("docker://") {
            return None;
        }
        let (name, git_ref) = uses.split_once('@')?;
        let mut parts = name.split('/');
        let owner = parts.next()?.trim();
        let repo = parts.next()?.trim();
        let path: Vec<&str> = parts.collect();
        if owner.is_empty()
            || repo.is_empty()
            || git_ref.is_empty()
            || path.iter().any(|p| p.is_empty() || *p == "." || *p == "..")
        {
            return None;
        }
        Some(ActionRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            path: path.join("/"),
            git_ref: git_ref.to_string(),
        })
    }

    /// `owner/repo`.
    pub fn repository(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// A fetched action repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedAction {
    /// The changeset the ref resolved to (hex).
    pub changeset: String,
    /// Every file of the repository at that changeset.
    pub files: Vec<TreeFile>,
}

/// Where remote actions come from.
#[async_trait]
pub trait ActionSource: Send + Sync {
    /// Fetches the repository of `action` at its ref, for the job `a`.
    async fn fetch(&self, a: &RunnerJobAssignment, action: &ActionRef) -> Result<FetchedAction>;
}

/// An [`ActionSource`] shared by the runner's jobs ([`super::RunnerConfig::actions`]).
#[derive(Clone)]
pub struct ActionRegistry(pub Arc<dyn ActionSource>);

impl std::fmt::Debug for ActionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ActionRegistry(..)")
    }
}

impl ActionRegistry {
    /// A registry over `source`.
    pub fn new(source: Arc<dyn ActionSource>) -> ActionRegistry {
        ActionRegistry(source)
    }
}

/// [`ActionSource`] over Beehive repos and mirrors of github.com actions.
pub struct ForgeActionSource {
    ci: Arc<Ci>,
    mirrors: BTreeMap<String, String>,
}

impl ForgeActionSource {
    /// Actions from Beehive repos named like the reference.
    pub fn new(ci: Arc<Ci>) -> ForgeActionSource {
        ForgeActionSource {
            ci,
            mirrors: BTreeMap::new(),
        }
    }

    /// Serves `github_owner/*` actions from the Beehive org `org` (repos imported with the
    /// same names). `*` as the owner mirrors every owner not mapped otherwise.
    pub fn with_mirror(mut self, github_owner: &str, org: &str) -> ForgeActionSource {
        self.mirrors
            .insert(github_owner.to_lowercase(), org.to_string());
        self
    }

    fn candidates(&self, a: &ActionRef) -> Vec<String> {
        let mut out = vec![a.repository()];
        let mirror = self
            .mirrors
            .get(&a.owner.to_lowercase())
            .or_else(|| self.mirrors.get("*"));
        if let Some(org) = mirror {
            out.push(format!("{org}/{}", a.repo));
        }
        out
    }
}

#[async_trait]
impl ActionSource for ForgeActionSource {
    async fn fetch(&self, a: &RunnerJobAssignment, action: &ActionRef) -> Result<FetchedAction> {
        let cx = Cx::system();
        let job_org = match a.repo.as_ref().and_then(|r| r.repo.as_ref()) {
            Some(repo_ref::Repo::Id(id)) => self.ci.orgs.repo_any(&cx, id).await?.org_id,
            _ => String::new(),
        };
        let mut found = None;
        for name in self.candidates(action) {
            if let Ok(r) = self
                .ci
                .orgs
                .resolve_repo(
                    &cx,
                    &RepoRef {
                        repo: Some(repo_ref::Repo::FullName(name)),
                    },
                )
                .await
                && r.deleted_at.is_none()
            {
                found = Some(r);
                break;
            }
        }
        let repo = found.ok_or_else(|| {
            ForgeError::not_found(format!(
                "action repository {} (no Bee repo or mirror has it)",
                action.repository()
            ))
        })?;
        if repo.visibility != Visibility::Public as i32 && repo.org_id != job_org {
            return Err(ForgeError::permission_denied(format!(
                "{} is private to another org; its actions cannot be used here",
                action.repository()
            )));
        }
        let by_id = crate::ci::engine::object_id(&action.git_ref).filter(|o| o.raw.len() >= 16);
        let at: Option<ObjectId> = match by_id {
            Some(id) => Some(id),
            None => {
                let branch = self
                    .ci
                    .vcs
                    .ref_tip(&repo, &bee_ref(&action.git_ref))
                    .await?;
                match branch {
                    Some(t) => Some(t),
                    None => {
                        self.ci
                            .vcs
                            .ref_tip(&repo, &format!("tags/{}", action.git_ref))
                            .await?
                    }
                }
            }
        };
        let at = at.ok_or_else(|| {
            ForgeError::not_found(format!(
                "ref {} of action repository {}",
                action.git_ref,
                action.repository()
            ))
        })?;
        let files = self.ci.vcs.checkout(&repo, &at).await?;
        Ok(FetchedAction {
            changeset: hex(&at.raw),
            files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_refs_parse() {
        assert_eq!(
            ActionRef::parse("acme/tools/lint@v2"),
            Some(ActionRef {
                owner: "acme".into(),
                repo: "tools".into(),
                path: "lint".into(),
                git_ref: "v2".into(),
            })
        );
        assert_eq!(ActionRef::parse("acme/tools@main").unwrap().path, "");
        assert!(ActionRef::parse("./local").is_none());
        assert!(ActionRef::parse("docker://alpine").is_none());
        assert!(ActionRef::parse("acme/tools").is_none());
        assert!(ActionRef::parse("acme/tools/../x@v1").is_none());
        assert!(ActionRef::parse("acme@v1").is_none());
    }
}
