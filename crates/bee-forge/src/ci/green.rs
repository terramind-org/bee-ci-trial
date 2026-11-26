//! `green/<ref>`: the newest tested changeset of a ref (SPEC §5, §11.6).
//!
//! A changeset is **green** when it has `post_land` check runs and the latest run of every
//! name passed ([`Ci::green_of`]). [`GreenListener`] moves `green/<default ref>` forward on
//! every passing `post_land` check: to the changeset the check ran on, when that changeset is
//! on the ref and now green. The move is a plain compare-and-swap of the ref's current target
//! (`green/*` is non-protected), so concurrent listeners never lose a write, and it only ever
//! goes forward:
//!
//! - never to a changeset that is not green (a failing or pending `post_land` run holds it);
//! - never to a changeset that is not reachable from the ref's tip;
//! - never backwards: not to an ancestor of the current target, and not sideways while the
//!   current target is still on the ref and is not an ancestor of the new one.
//!
//! The decision reads only the ref tips, the ancestry and the latest check runs, so the same
//! inputs always give the same move; repeating it is a no-op. Ports that cannot write refs
//! ([`super::CiVcs::set_ref_if`] answers UNIMPLEMENTED) leave `green/*` computed on demand.

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{CheckStatus, Event, ObjectId, Repo, event};

use super::convert::bee_ref;
use super::engine::Ci;
use crate::checks::policy_status;
use crate::cx::Cx;
use crate::error::Result;
use crate::events::EventListener;

/// Compare-and-swap attempts before a move gives way to the next event.
const GREEN_ATTEMPTS: usize = 3;

/// The name of the green ref that tracks `ref_name` (`main` → `green/main`).
pub fn green_ref_name(ref_name: &str) -> String {
    format!("green/{}", bee_ref(ref_name))
}

impl Ci {
    /// The repo's default ref (bee name).
    pub(crate) fn default_ref_of(repo: &Repo) -> String {
        bee_ref(if repo.default_ref.is_empty() {
            "main"
        } else {
            &repo.default_ref
        })
    }

    /// Moves `green/<default ref>` to `cs` when `cs` is green and on the default ref and the
    /// move goes forward (module docs). Returns the new target when the ref moved.
    pub async fn advance_green(
        &self,
        cx: &Cx,
        repo: &Repo,
        cs: &ObjectId,
    ) -> Result<Option<ObjectId>> {
        if cs.raw.is_empty() {
            return Ok(None);
        }
        let name = Self::default_ref_of(repo);
        let Some(tip) = self.vcs.ref_tip(repo, &name).await? else {
            return Ok(None);
        };
        if *cs != tip && !self.vcs.is_ancestor(repo, cs, &tip).await? {
            return Ok(None);
        }
        if !self.green_of(cx, &repo.id, cs).await?.0 {
            return Ok(None);
        }
        let green = green_ref_name(&name);
        for _ in 0..GREEN_ATTEMPTS {
            let cur = self.vcs.ref_tip(repo, &green).await?;
            if let Some(c) = &cur {
                if c == cs || self.vcs.is_ancestor(repo, cs, c).await? {
                    return Ok(None);
                }
                if !self.vcs.is_ancestor(repo, c, cs).await?
                    && self.vcs.is_ancestor(repo, c, &tip).await?
                {
                    return Ok(None);
                }
            }
            if self.vcs.set_ref_if(repo, &green, cur.as_ref(), cs).await? {
                return Ok(Some(cs.clone()));
            }
        }
        Ok(None)
    }
}

/// Moves `green/<ref>` forward when `post_land` checks pass.
pub struct GreenListener {
    ci: Arc<Ci>,
}

impl GreenListener {
    /// A listener for `ci`.
    pub fn new(ci: Arc<Ci>) -> GreenListener {
        GreenListener { ci }
    }
}

#[async_trait]
impl EventListener for GreenListener {
    async fn on_event(&self, ev: &Event) {
        if ev.repo_id.is_empty() || ev.kind != "check.updated" {
            return;
        }
        let Some(event::Body::Check(run)) = &ev.body else {
            return;
        };
        let passed = matches!(
            policy_status(CheckStatus::try_from(run.status).unwrap_or_default()),
            bee_policy::CheckStatus::Success
                | bee_policy::CheckStatus::Neutral
                | bee_policy::CheckStatus::Skipped
        );
        if !run.post_land || !passed {
            return;
        }
        let Some(cs) = run.changeset.clone() else {
            return;
        };
        let cx = Cx::system();
        let Ok(repo) = self.ci.orgs.repo_any(&cx, &ev.repo_id).await else {
            return;
        };
        let _ = Box::pin(self.ci.advance_green(&cx, &repo, &cs)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn green_ref_names_follow_the_tracked_ref() {
        assert_eq!(green_ref_name("main"), "green/main");
        assert_eq!(green_ref_name("refs/heads/release/1"), "green/release/1");
    }
}
