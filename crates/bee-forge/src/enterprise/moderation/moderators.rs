//! Moderators.
//!
//! A moderator is an org member with `Role::Moderator`, a principal listed here, or a member
//! of a team listed here. Listing someone here grants nothing in the org's roles: it only
//! lets them hide content, lock conversations, block users, resolve reports and set limits in
//! the org's repos without repo write access. [`Moderation::is_moderator`] considers all
//! three sources.

use bee_proto::v1::{ModerationModerator, Role, SubjectKind, moderation_moderator};
use serde_json::json;

use super::tables::MODERATORS;
use super::{Change, Moderation, SURFACE, record};
use crate::cx::Cx;
use crate::enterprise::common::{page_args, scan_all, scan_page};
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Key, Row};

/// A moderator entry: one principal or one team.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModeratorRef {
    /// A principal id.
    Principal(String),
    /// A team id.
    Team(String),
}

impl ModeratorRef {
    fn row_id(&self) -> String {
        match self {
            ModeratorRef::Principal(p) => format!("p:{p}"),
            ModeratorRef::Team(t) => format!("t:{t}"),
        }
    }

    fn name(&self) -> &str {
        match self {
            ModeratorRef::Principal(p) | ModeratorRef::Team(p) => p,
        }
    }
}

impl Moderation {
    /// Adds a moderator to an org. `ALREADY_EXISTS` when listed already; a team must belong
    /// to the org (`INVALID_ARGUMENT` otherwise, `INVALID_ARGUMENT`/`no_teams` without a
    /// team directory).
    pub async fn add_moderator(
        &self,
        cx: &Cx,
        org_id: &str,
        who: &ModeratorRef,
    ) -> Result<ModerationModerator> {
        if who.name().is_empty() {
            return Err(ForgeError::missing("moderator"));
        }
        let moderator = match who {
            ModeratorRef::Principal(id) => {
                moderation_moderator::Moderator::Principal(self.principal(id).await)
            }
            ModeratorRef::Team(id) => {
                let Some(teams) = &self.cfg.teams else {
                    return Err(ForgeError::invalid(
                        "no_teams",
                        "this deployment has no teams to name as moderators",
                    ));
                };
                if teams.team_org(cx, id).await? != org_id {
                    return Err(ForgeError::invalid(
                        "team_not_in_org",
                        format!("team {id} does not belong to this org"),
                    ));
                }
                moderation_moderator::Moderator::TeamId(id.clone())
            }
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let entry = ModerationModerator {
            org: org_id.to_string(),
            moderator: Some(moderator),
            added_by: Some(cx.principal()),
            added_at: Some(ts(now)),
        };
        let kind = match who {
            ModeratorRef::Principal(_) => "principal",
            ModeratorRef::Team(_) => "team",
        };
        tx.insert(
            &MODERATORS,
            &Row::new(org_id, who.row_id()).k1(kind).msg(&entry),
        )
        .await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.moderator_added",
                action: "moderation.moderator_add",
                org: org_id.to_string(),
                repo: String::new(),
                subject: Some(super::subject_in(
                    "",
                    match who {
                        ModeratorRef::Principal(_) => SubjectKind::User,
                        ModeratorRef::Team(_) => SubjectKind::Team,
                    },
                    who.name(),
                )),
                before: None,
                after: json!({ kind: who.name() }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(entry)
    }

    /// Removes a moderator entry. `NOT_FOUND` when it is not listed.
    pub async fn remove_moderator(&self, cx: &Cx, org_id: &str, who: &ModeratorRef) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx.delete(&MODERATORS, org_id, &who.row_id()).await? {
            return Err(ForgeError::not_found(format!("moderator {}", who.name())));
        }
        record(
            &mut tx,
            Change {
                kind: "moderation.moderator_removed",
                action: "moderation.moderator_remove",
                org: org_id.to_string(),
                repo: String::new(),
                subject: Some(super::subject_in(
                    "",
                    match who {
                        ModeratorRef::Principal(_) => SubjectKind::User,
                        ModeratorRef::Team(_) => SubjectKind::Team,
                    },
                    who.name(),
                )),
                before: Some(json!({ "moderator": who.name() })),
                after: json!({ "removed": who.name() }),
            },
        )
        .await?;
        tx.commit().await
    }

    /// The moderator entries of an org, paged by entry id.
    pub async fn list_moderators(
        &self,
        cx: &Cx,
        org_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<ModerationModerator>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) =
            scan_page::<ModerationModerator>(&mut tx, &MODERATORS, org_id, None, after, limit)
                .await?;
        Ok((rows.into_iter().map(|(m, _)| m).collect(), next))
    }

    /// The org members whose role is `Moderator` (managed through the org's roles).
    pub async fn role_moderators(
        &self,
        cx: &Cx,
        org_id: &str,
    ) -> Result<Vec<bee_proto::v1::Principal>> {
        let (_, size) = page_args(&None);
        let mut out = Vec::new();
        let mut after = String::new();
        loop {
            let (members, next) = self.forge.orgs.members(cx, org_id, &after, size).await?;
            out.extend(
                members
                    .into_iter()
                    .filter(|m| m.role == Role::Moderator as i32)
                    .filter_map(|m| m.principal),
            );
            if next.is_empty() {
                return Ok(out);
            }
            after = next;
        }
    }

    /// Whether `principal_id` moderates the org: an org member with `Role::Moderator`, a
    /// listed principal, or a member of a listed team.
    pub async fn is_moderator(&self, cx: &Cx, org_id: &str, principal_id: &str) -> Result<bool> {
        if let Some(m) = self.forge.orgs.member(cx, org_id, principal_id).await?
            && m.role == Role::Moderator as i32
        {
            return Ok(true);
        }
        let teams = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            if tx
                .get(
                    &MODERATORS,
                    org_id,
                    &ModeratorRef::Principal(principal_id.into()).row_id(),
                )
                .await?
                .is_some()
            {
                return Ok(true);
            }
            if self.cfg.teams.is_none() {
                return Ok(false);
            }
            scan_all::<ModerationModerator>(
                &mut tx,
                &MODERATORS,
                org_id,
                Some(Key::K1("team".into())),
            )
            .await?
        };
        let Some(directory) = &self.cfg.teams else {
            return Ok(false);
        };
        for (m, _) in teams {
            if let Some(moderation_moderator::Moderator::TeamId(t)) = m.moderator
                && directory.is_team_member(cx, &t, principal_id).await?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
