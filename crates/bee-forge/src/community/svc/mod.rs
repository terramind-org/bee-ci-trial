//! The community tonic services over a [`ForgeServices`].
//!
//! Each file implements one service on the same [`ForgeServices`] value the core services use,
//! following the pattern in `docs/spec/forge-core.md` §7: authenticate, validate, resolve and
//! authorize, call the store with the authorized [`Cx`], map errors.
//!
//! # Permission levels
//!
//! | level | repo home | org home / user-level |
//! |---|---|---|
//! | read | `read` | org member / token allows `read` |
//! | participate | `read` (readers may discuss), repo not archived | org member / token allows `commit` |
//! | moderate | `triage` | org owner or admin |
//! | admin | `manage_settings`, maintain | org owner or admin |
//!
//! State-changing calls that fail authorization are audited by [`ForgeServices::guarded`].
//! User-level services (gists, users, sponsors) check that the token allows `read` for reads and
//! `commit` for changes; notifications need `read` throughout. All act on the caller's own data.

mod activity;
mod discussions;
mod gists;
mod insights;
mod notifications;
mod releases;
mod sponsors;
mod users;
mod wiki;

use std::sync::Arc;

use bee_policy::Action;
use bee_proto::v1::{Repo, RepoRole};

use crate::auth::Caller;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::service::ForgeServices;

use super::Community;

impl ForgeServices {
    /// The forge.
    pub(crate) fn fg(&self) -> &Arc<Forge> {
        self.forge()
    }

    /// The community stores.
    pub(crate) fn community(&self) -> &Community {
        &self.forge().community
    }

    /// Authorizes a user-level call: the token must allow `action`. The returned context is
    /// the caller's.
    pub(crate) fn user_cx(&self, caller: &Caller, action: Action) -> Result<Cx> {
        if !caller.token.allows_action(action) {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::PolicyDenied,
                format!("token does not allow `{action}`"),
            ));
        }
        let mut cx = caller.cx.clone();
        cx.decision = format!("token:ok user:{}", cx.actor_id);
        Ok(cx)
    }

    /// Authorizes a repo action, auditing a denial as `audit` when given.
    pub(crate) async fn repo_cx(
        &self,
        caller: &Caller,
        repo: &Repo,
        action: Action,
        min: Option<RepoRole>,
        audit: Option<&str>,
    ) -> Result<Cx> {
        let auth = self
            .fg()
            .authz
            .repo(caller, repo, action, &[], None, min)
            .await;
        match audit {
            Some(a) => self.guarded(caller, &repo.org_id, &repo.id, a, auth).await,
            None => auth,
        }
    }

    /// Whether the caller holds `action` on the repo (no audit, no error).
    pub(crate) async fn can(&self, caller: &Caller, repo: &Repo, action: Action) -> bool {
        self.fg()
            .authz
            .repo(caller, repo, action, &[], None, None)
            .await
            .is_ok()
    }
}

/// Fails with `ARCHIVED` for a write to an archived repo.
pub(crate) fn writable(repo: &Repo) -> Result<()> {
    if repo.archived {
        Err(ForgeError::archived(&repo.full_name))
    } else {
        Ok(())
    }
}
