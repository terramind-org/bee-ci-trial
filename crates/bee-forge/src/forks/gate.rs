//! [`ModerationGate`]: whether workflow runs of a fork change request wait for approval,
//! decided by enterprise moderation's first-time contributor rules.
//!
//! Moderation's `run_approval` setting (per repo, else per org) says whose runs need
//! approval: first-time contributors (the default), first-time contributors with new
//! accounts, everyone without collaborator standing, or nobody. A first-time contributor has
//! no collaborator standing and no landed change request in the repo, so after their first
//! change request lands, their next fork change requests run at once.

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{Principal, PrincipalKind, Repo};

use crate::ci::engine::ForkRunGate;
use crate::cx::Cx;
use crate::enterprise::moderation::{Moderation, WriteCheck, WriteKind};
use crate::error::Result;

/// [`ForkRunGate`] over [`Moderation`].
pub struct ModerationGate(pub Arc<Moderation>);

#[async_trait]
impl ForkRunGate for ModerationGate {
    async fn needs_approval(&self, repo: &Repo, author: &Principal) -> Result<bool> {
        let cx = Cx::new(
            author.id.clone(),
            PrincipalKind::try_from(author.kind).unwrap_or(PrincipalKind::Human),
        );
        let mut check = WriteCheck::new(&cx, &repo.id, &repo.org_id, WriteKind::ChangeRequest);
        check.dry_run = true;
        match self.0.check_write(&check).await {
            Ok(d) => Ok(d.require_run_approval || !d.allow),
            // A refused author (blocked, limited) never runs unattended.
            Err(e) if !e.retryable() => Ok(true),
            Err(e) => Err(e),
        }
    }
}
