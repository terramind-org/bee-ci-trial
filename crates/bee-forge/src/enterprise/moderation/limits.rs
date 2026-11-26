//! Interaction limits: temporary restrictions on who may interact with a repo, an org's repos
//! or a user's repos.
//!
//! A limit has a level and an expiry:
//!
//! | level | who may still interact |
//! |---|---|
//! | `EXISTING_USERS` | accounts older than 24 hours |
//! | `CONTRIBUTORS_ONLY` | principals with a prior contribution in the repo |
//! | `COLLABORATORS_ONLY` | collaborators, maintainers and moderators |
//!
//! Collaborators, maintainers and moderators are never limited. Agents are judged by their
//! owner's standing when the limit `applies_to_agents`, and are not limited at all when it does
//! not. Expiry is applied on read; [`Moderation::sweep_expired`](super::Moderation) deletes
//! expired rows. When several limits are in force (repo, org, user) a write must satisfy all
//! of them.

use bee_proto::v1::{
    InteractionLimit, InteractionLimitLevel, InteractionLimitScope, RepoRef,
    interaction_limit_scope, repo_ref,
};
use serde_json::json;

use super::tables::LIMITS;
use super::{Change, MAX_DURATION_SECS, Moderation, SURFACE, record};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Row, Tx};

/// Where a limit applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LimitTarget {
    /// One repo (the repo id).
    Repo(String),
    /// Every repo of an org (the org id).
    Org(String),
    /// Every repo a user owns (the user id).
    User(String),
}

impl LimitTarget {
    /// The partition key of the scope.
    pub fn key(&self) -> String {
        match self {
            LimitTarget::Repo(i) => format!("repo:{i}"),
            LimitTarget::Org(i) => format!("org:{i}"),
            LimitTarget::User(i) => format!("user:{i}"),
        }
    }

    /// The scope as the proto message (repo and org by id).
    pub fn to_proto(&self) -> InteractionLimitScope {
        InteractionLimitScope {
            scope: Some(match self {
                LimitTarget::Repo(i) => interaction_limit_scope::Scope::Repo(RepoRef {
                    repo: Some(repo_ref::Repo::Id(i.clone())),
                }),
                LimitTarget::Org(i) => interaction_limit_scope::Scope::Org(i.clone()),
                LimitTarget::User(i) => interaction_limit_scope::Scope::UserId(i.clone()),
            }),
        }
    }

    fn inherited_from(&self) -> &'static str {
        match self {
            LimitTarget::Repo(_) => "",
            LimitTarget::Org(_) => "org",
            LimitTarget::User(_) => "user",
        }
    }
}

/// A limit to set.
#[derive(Clone, Copy, Debug)]
pub struct NewLimit {
    /// Who may still interact.
    pub level: InteractionLimitLevel,
    /// Seconds until the limit expires (1 ..= [`MAX_DURATION_SECS`]).
    pub duration_secs: u64,
    /// Whether agents are limited too (judged by their owner's standing).
    pub applies_to_agents: bool,
}

/// The limit stored at `key`, if one is in force at `now`.
pub(crate) async fn live_limit(
    tx: &mut Tx,
    key: &str,
    now: u64,
) -> Result<Option<InteractionLimit>> {
    Ok(tx
        .get_msg::<InteractionLimit>(&LIMITS, key, "limit")
        .await?
        .filter(|(_, row)| row.n1.is_none_or(|e| e as u64 > now))
        .map(|(l, _)| l))
}

impl Moderation {
    /// Sets (or replaces) the limit at `target`. `audit_org` is the org whose log records
    /// the change.
    pub async fn set_limit(
        &self,
        cx: &Cx,
        target: &LimitTarget,
        audit_org: &str,
        new: NewLimit,
    ) -> Result<InteractionLimit> {
        if new.level == InteractionLimitLevel::Unspecified {
            return Err(ForgeError::invalid(
                "missing_level",
                "a limit needs a level; use RemoveInteractionLimit to lift one",
            ));
        }
        if new.duration_secs == 0 || new.duration_secs > MAX_DURATION_SECS {
            return Err(ForgeError::invalid(
                "bad_duration",
                format!("duration_seconds must be 1..={MAX_DURATION_SECS}"),
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let expires = now + new.duration_secs * 1000;
        let limit = InteractionLimit {
            scope: Some(target.to_proto()),
            level: new.level as i32,
            applies_to_agents: new.applies_to_agents,
            inherited_from: String::new(),
            set_by: Some(cx.principal()),
            expires_at: Some(ts(expires)),
        };
        let before = live_limit(&mut tx, &target.key(), now).await?;
        let mut row = Row::new(target.key(), "limit").msg(&limit);
        row.n1 = Some(expires as i64);
        tx.put(&LIMITS, &row).await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.limit_set",
                action: "moderation.limit_set",
                org: audit_org.to_string(),
                repo: match target {
                    LimitTarget::Repo(i) => i.clone(),
                    _ => String::new(),
                },
                subject: None,
                before: before.map(|b| json!({ "level": b.level })),
                after: json!({
                    "scope": target.key(),
                    "level": new.level.as_str_name(),
                    "applies_to_agents": new.applies_to_agents,
                    "expires_at_ms": expires,
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(limit)
    }

    /// Removes the limit at `target`. `NOT_FOUND` when none is in force.
    pub async fn remove_limit(&self, cx: &Cx, target: &LimitTarget, audit_org: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some(before) = live_limit(&mut tx, &target.key(), now).await? else {
            return Err(ForgeError::not_found(format!(
                "interaction limit at {}",
                target.key()
            )));
        };
        tx.delete(&LIMITS, &target.key(), "limit").await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.limit_removed",
                action: "moderation.limit_remove",
                org: audit_org.to_string(),
                repo: match target {
                    LimitTarget::Repo(i) => i.clone(),
                    _ => String::new(),
                },
                subject: None,
                before: Some(json!({ "level": before.level })),
                after: json!({ "scope": target.key(), "removed": true }),
            },
        )
        .await?;
        tx.commit().await
    }

    /// The limit stored at `target` itself, if one is in force (no inheritance).
    pub async fn limit(&self, cx: &Cx, target: &LimitTarget) -> Result<Option<InteractionLimit>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        live_limit(&mut tx, &target.key(), now).await
    }

    /// The limit in force for a repo: the strictest of the repo's own, its org's and (when
    /// `owner_id` is given) its owner's, with `inherited_from` naming where it came from.
    /// `None` when nothing limits the repo.
    pub async fn effective_limit(
        &self,
        cx: &Cx,
        repo_id: &str,
        org_id: &str,
        owner_id: Option<&str>,
    ) -> Result<Option<InteractionLimit>> {
        let mut targets = vec![
            LimitTarget::Repo(repo_id.to_string()),
            LimitTarget::Org(org_id.to_string()),
        ];
        if let Some(o) = owner_id {
            targets.push(LimitTarget::User(o.to_string()));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let mut best: Option<InteractionLimit> = None;
        for t in targets {
            if let Some(mut l) = live_limit(&mut tx, &t.key(), now).await? {
                l.inherited_from = t.inherited_from().to_string();
                // Strictest level wins; on a tie the more specific scope (earlier) stays.
                if best.as_ref().is_none_or(|b| l.level > b.level) {
                    best = Some(l);
                }
            }
        }
        Ok(best)
    }
}
