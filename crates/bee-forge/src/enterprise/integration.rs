//! Adapters that plug the enterprise domains into the work and changes modules:
//!
//! - [`Moderation`] implements [`WriteGuard`], so issue creation, issue and change request
//!   comments and change request opening honor blocks, interaction limits, locks and flood
//!   limits (`BLOCKED`, `INTERACTION_LIMITED`, `LOCKED`, `RESOURCE_EXHAUSTED`). Install it
//!   with `forge.set_write_guard(moderation.clone())`.
//! - [`Teams`] implements the changes module's team directory, so review requests to a team
//!   resolve to its members and honor its review assignment settings: pass
//!   `ChangeHooks { teams: ent.teams.clone(), .. }`.

use std::collections::BTreeSet;

use async_trait::async_trait;
use bee_proto::v1::{Principal, Repo, SubjectRef, TeamReviewAssignment};

use super::moderation::{Moderation, WriteCheck, WriteKind};
use super::teams::Teams;
use crate::changes::TeamDirectory as ChangesTeams;
use crate::cx::Cx;
use crate::error::Result;
use crate::hooks::{GuardedWrite, WriteGuard};

#[async_trait]
impl WriteGuard for Moderation {
    async fn check(
        &self,
        cx: &Cx,
        repo: &Repo,
        kind: GuardedWrite,
        subject: Option<SubjectRef>,
    ) -> Result<()> {
        let k = match kind {
            GuardedWrite::Issue => WriteKind::Issue,
            GuardedWrite::Comment => WriteKind::Comment,
            GuardedWrite::ChangeRequest => WriteKind::ChangeRequest,
        };
        let mut c = WriteCheck::new(cx, &repo.id, &repo.org_id, k);
        if let Some(s) = subject {
            c = c.subject(SubjectRef {
                repo: Some(bee_proto::v1::RepoRef {
                    repo: Some(bee_proto::v1::repo_ref::Repo::Id(repo.id.clone())),
                }),
                ..s
            });
        }
        self.check_write(&c).await.map(|_| ())
    }
}

#[async_trait]
impl ChangesTeams for Teams {
    async fn members(&self, org_id: &str, team_id: &str) -> Result<Vec<Principal>> {
        let cx = Cx::system();
        let ids = self.member_ids(&cx, org_id, team_id, true).await?;
        let mut out = Vec::new();
        let seen: BTreeSet<String> = ids.into_iter().collect();
        for id in seen {
            out.push(self.principal(&id).await);
        }
        Ok(out)
    }

    async fn assignment(
        &self,
        org_id: &str,
        team_id: &str,
    ) -> Result<Option<TeamReviewAssignment>> {
        self.configured_assignment(&Cx::system(), org_id, team_id)
            .await
    }
}
