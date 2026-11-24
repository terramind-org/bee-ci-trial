//! Agent-flood defenses: limits, throttles, counters and reputation.
//!
//! # Model
//!
//! Every counter and window is **per repo**, so a flood against repo A cannot slow repo B.
//! A repo's [`ModerationFloodLimits`] (all zero = off) hold:
//!
//! - open-CR caps per principal and per owner (the delegation root of an agent, so all agents
//!   of one owner share the owner's cap). Counters move with [`Moderation::note_open`] and
//!   [`Moderation::note_closed`];
//! - hourly creation limits per owner for CRs, comments and issues, counted over a sliding
//!   one-hour window of admitted writes (the `activity` table, one row per admitted write);
//! - `min_reputation`: principals whose reputation is lower are throttled;
//! - `throttle_duration_seconds`: how long a principal stays throttled once a rate or
//!   reputation limit trips (0 = no throttle record; the write is refused until the window
//!   frees a slot);
//! - `exempt_collaborators`: collaborators are never limited.
//!
//! A throttle is a row with an expiry, visible through `ListThrottledPrincipals` and lifted
//! early by `ReleaseThrottledPrincipal` (which also resets the owner's rate window, otherwise
//! the next write would trip the limit again at once).
//!
//! # Reputation
//!
//! Each principal has a tally of landed, rejected and spam contributions fed by
//! [`Moderation::record_outcome`]. The score is
//!
//! ```text
//! score = (landed + 1) / (landed + rejected + 3 * spam + 2)
//! ```
//!
//! a number in (0, 1]: 0.5 for a newcomer, rising with landed work and falling faster with
//! spam. It is a pure function of the tally, so it is deterministic. Where a chain acts (an
//! agent for an owner) the lowest score of the chain is used.

use bee_proto::v1::{ModerationFloodLimits, ModerationThrottledPrincipal, Principal};
use serde_json::json;

use super::tables::{ACTIVITY, BLOCKS, COUNTERS, FLOOD, LIMITS, REPUTATION, Reputation, THROTTLES};
use super::{Change, Moderation, SURFACE, WINDOW_MS, record};
use crate::cx::Cx;
use crate::enterprise::common::scan_page;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Row, Scan, Tx};

/// Throttle length when a repo's limits name none.
pub const DEFAULT_THROTTLE_SECS: u64 = 3600;
/// Retry hint for an open-CR cap, which frees up when a CR closes rather than at a time.
pub const CAP_RETRY_MS: u64 = 60_000;
/// Rows removed per table by one sweep.
pub const SWEEP_BATCH: usize = 500;

/// What happened to a contribution of a principal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It landed or was accepted.
    Landed,
    /// It was rejected or closed unmerged.
    Rejected,
    /// It was confirmed as spam.
    Spam,
}

/// A partial update of flood limits; `None` fields keep their value.
#[derive(Clone, Debug, Default)]
pub struct FloodPatch {
    /// Open-CR cap per principal.
    pub max_open_change_requests_per_principal: Option<u32>,
    /// Open-CR cap per owner.
    pub max_open_change_requests_per_owner: Option<u32>,
    /// New CRs per hour per owner.
    pub max_new_change_requests_per_hour_per_owner: Option<u32>,
    /// New comments per hour per owner.
    pub max_new_comments_per_hour_per_owner: Option<u32>,
    /// New issues per hour per owner.
    pub max_new_issues_per_hour_per_owner: Option<u32>,
    /// Reputation below which principals are throttled.
    pub min_reputation: Option<f64>,
    /// Whether collaborators are exempt.
    pub exempt_collaborators: Option<bool>,
    /// Throttle length in seconds.
    pub throttle_duration_seconds: Option<u64>,
}

/// What one [`Moderation::sweep_expired`] pass removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Expired blocks.
    pub blocks: usize,
    /// Expired interaction limits.
    pub limits: usize,
    /// Expired throttles.
    pub throttles: usize,
    /// Window rows older than the window.
    pub activity: usize,
}

/// The reputation score of a tally (see the module docs).
pub fn reputation_score(r: &Reputation) -> f64 {
    let good = r.landed.saturating_add(1) as f64;
    let all = r
        .landed
        .saturating_add(r.rejected)
        .saturating_add(r.spam.saturating_mul(3))
        .saturating_add(2) as f64;
    good / all
}

/// A counter's value.
pub(crate) async fn counter(tx: &mut Tx, repo_id: &str, id: &str) -> Result<i64> {
    Ok(tx
        .get(&COUNTERS, repo_id, id)
        .await?
        .and_then(|r| r.n1)
        .unwrap_or(0))
}

/// The reputation tally of a principal.
pub(crate) async fn tally(tx: &mut Tx, principal_id: &str) -> Result<Reputation> {
    Ok(tx
        .get_msg::<Reputation>(&REPUTATION, "rep", principal_id)
        .await?
        .map(|(r, _)| r)
        .unwrap_or_default())
}

/// The scope of the admission window rows of an owner's writes of one kind in a repo.
pub(crate) fn window_scope(repo_id: &str, owner_id: &str, kind: &str) -> String {
    format!("{repo_id}:{owner_id}:{kind}")
}

/// The kinds that have an hourly limit.
pub(crate) const RATE_KINDS: [&str; 3] = ["cr", "comment", "issue"];

/// Everything needed to record a throttle.
pub(crate) struct NewThrottle<'a> {
    pub repo_id: &'a str,
    pub org_id: &'a str,
    pub principal: Principal,
    pub owner_id: &'a str,
    pub limit: &'a str,
    pub reputation: f64,
    pub recent_count: u32,
    pub expires_ms: u64,
}

/// Records a throttle (replacing an earlier one) with its event and audit entry.
pub(crate) async fn put_throttle(tx: &mut Tx, t: NewThrottle<'_>) -> Result<()> {
    let now = tx.now_ms();
    let rec = ModerationThrottledPrincipal {
        principal: Some(t.principal.clone()),
        owner_id: t.owner_id.to_string(),
        limit: t.limit.to_string(),
        reputation: t.reputation,
        recent_count: t.recent_count,
        throttled_at: Some(ts(now)),
        expires_at: Some(ts(t.expires_ms)),
    };
    let mut row = Row::new(t.repo_id, t.principal.id.clone()).msg(&rec);
    row.n1 = Some(t.expires_ms as i64);
    tx.put(&THROTTLES, &row).await?;
    record(
        tx,
        Change {
            kind: "moderation.principal_throttled",
            action: "moderation.throttle",
            org: t.org_id.to_string(),
            repo: t.repo_id.to_string(),
            subject: Some(super::subject_in(
                "",
                bee_proto::v1::SubjectKind::User,
                &t.principal.id,
            )),
            before: None,
            after: json!({
                "principal": t.principal.id,
                "owner": t.owner_id,
                "limit": t.limit,
                "recent_count": t.recent_count,
                "reputation": t.reputation,
                "expires_at_ms": t.expires_ms,
            }),
        },
    )
    .await
    .map(|_| ())
}

impl Moderation {
    fn base_flood(&self, repo_id: &str) -> ModerationFloodLimits {
        let mut l = self
            .cfg
            .default_flood
            .clone()
            .unwrap_or_else(|| ModerationFloodLimits {
                throttle_duration_seconds: DEFAULT_THROTTLE_SECS,
                ..ModerationFloodLimits::default()
            });
        l.repo = Some(bee_proto::v1::RepoRef {
            repo: Some(bee_proto::v1::repo_ref::Repo::Id(repo_id.to_string())),
        });
        l.updated_at = None;
        l
    }

    /// The flood limits of a repo: its own, else the configured defaults, else all off.
    pub async fn flood_limits(&self, cx: &Cx, repo_id: &str) -> Result<ModerationFloodLimits> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .get_msg::<ModerationFloodLimits>(&FLOOD, repo_id, "limits")
            .await?
            .map(|(l, _)| l)
            .unwrap_or_else(|| self.base_flood(repo_id)))
    }

    /// Applies a patch to a repo's flood limits (unset fields unchanged) and returns them.
    pub async fn set_flood_limits(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        patch: FloodPatch,
    ) -> Result<ModerationFloodLimits> {
        if patch
            .min_reputation
            .is_some_and(|r| !(0.0..=1.0).contains(&r) || r.is_nan())
        {
            return Err(ForgeError::invalid(
                "bad_min_reputation",
                "min_reputation must be between 0 and 1",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let own = tx
            .get_msg::<ModerationFloodLimits>(&FLOOD, repo_id, "limits")
            .await?;
        let mut l = match &own {
            Some((l, _)) => l.clone(),
            None => self.base_flood(repo_id),
        };
        let before = json!({
            "owner_cr_cap": l.max_open_change_requests_per_owner,
            "cr_per_hour": l.max_new_change_requests_per_hour_per_owner,
            "min_reputation": l.min_reputation,
        });
        macro_rules! apply {
            ($($f:ident),*) => {
                $(if let Some(v) = patch.$f { l.$f = v; })*
            };
        }
        apply!(
            max_open_change_requests_per_principal,
            max_open_change_requests_per_owner,
            max_new_change_requests_per_hour_per_owner,
            max_new_comments_per_hour_per_owner,
            max_new_issues_per_hour_per_owner,
            min_reputation,
            exempt_collaborators,
            throttle_duration_seconds
        );
        l.updated_at = Some(ts(now));
        let mut row = Row::new(repo_id, "limits").msg(&l);
        if let Some((_, old)) = &own {
            row.version = old.version;
            tx.update(&FLOOD, &row).await?;
        } else {
            tx.insert(&FLOOD, &row).await?;
        }
        record(
            &mut tx,
            Change {
                kind: "moderation.flood_limits_set",
                action: "moderation.flood_limits_set",
                org: org_id.to_string(),
                repo: repo_id.to_string(),
                subject: None,
                before: Some(before),
                after: json!({
                    "max_open_change_requests_per_principal": l.max_open_change_requests_per_principal,
                    "max_open_change_requests_per_owner": l.max_open_change_requests_per_owner,
                    "max_new_change_requests_per_hour_per_owner": l.max_new_change_requests_per_hour_per_owner,
                    "max_new_comments_per_hour_per_owner": l.max_new_comments_per_hour_per_owner,
                    "max_new_issues_per_hour_per_owner": l.max_new_issues_per_hour_per_owner,
                    "min_reputation": l.min_reputation,
                    "exempt_collaborators": l.exempt_collaborators,
                    "throttle_duration_seconds": l.throttle_duration_seconds,
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(l)
    }

    /// The principals throttled in a repo now, by principal id, paged.
    pub async fn throttled_principals(
        &self,
        cx: &Cx,
        repo_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<ModerationThrottledPrincipal>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (rows, next) = scan_page::<ModerationThrottledPrincipal>(
            &mut tx, &THROTTLES, repo_id, None, after, limit,
        )
        .await?;
        Ok((
            rows.into_iter()
                .filter(|(_, r)| r.n1.is_some_and(|e| e as u64 > now))
                .map(|(t, _)| t)
                .collect(),
            next,
        ))
    }

    /// Lifts a throttle early and resets its owner's rate window in the repo. `NOT_FOUND`
    /// when the principal is not throttled.
    pub async fn release_throttle(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        principal_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some((t, row)) = tx
            .get_msg::<ModerationThrottledPrincipal>(&THROTTLES, repo_id, principal_id)
            .await?
        else {
            return Err(ForgeError::not_found(format!("throttle of {principal_id}")));
        };
        if row.n1.is_none_or(|e| e as u64 <= now) {
            return Err(ForgeError::not_found(format!("throttle of {principal_id}")));
        }
        tx.delete(&THROTTLES, repo_id, principal_id).await?;
        let owner = if t.owner_id.is_empty() {
            principal_id.to_string()
        } else {
            t.owner_id.clone()
        };
        for kind in RATE_KINDS {
            let scope = window_scope(repo_id, &owner, kind);
            let rows = tx
                .scan(
                    &ACTIVITY,
                    &Scan::scope(scope.clone(), super::flood::SWEEP_BATCH * 4),
                )
                .await?;
            for r in rows {
                tx.delete(&ACTIVITY, &scope, &r.id).await?;
            }
        }
        record(
            &mut tx,
            Change {
                kind: "moderation.throttle_released",
                action: "moderation.throttle_release",
                org: org_id.to_string(),
                repo: repo_id.to_string(),
                subject: Some(super::subject_in(
                    "",
                    bee_proto::v1::SubjectKind::User,
                    principal_id,
                )),
                before: Some(json!({ "limit": t.limit })),
                after: json!({ "released": principal_id, "owner": owner }),
            },
        )
        .await?;
        tx.commit().await
    }

    /// Counts a CR opened in a repo by `author_id` (acting for `owner_id`); returns the
    /// author's open count. The changes module calls it when a CR opens (or reopens).
    pub async fn note_open(
        &self,
        cx: &Cx,
        repo_id: &str,
        author_id: &str,
        owner_id: &str,
    ) -> Result<u64> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let n = tx
            .add_n1(&COUNTERS, repo_id, &format!("cr:p:{author_id}"), 1)
            .await?;
        tx.add_n1(&COUNTERS, repo_id, &format!("cr:o:{owner_id}"), 1)
            .await?;
        tx.commit().await?;
        Ok(n.max(0) as u64)
    }

    /// Counts a CR closed, landed or abandoned; the counters never go below zero. The
    /// changes module calls it with the same ids as [`Moderation::note_open`].
    pub async fn note_closed(
        &self,
        cx: &Cx,
        repo_id: &str,
        author_id: &str,
        owner_id: &str,
    ) -> Result<u64> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut left = 0;
        for (i, id) in [format!("cr:p:{author_id}"), format!("cr:o:{owner_id}")]
            .iter()
            .enumerate()
        {
            let mut n = tx.add_n1(&COUNTERS, repo_id, id, -1).await?;
            if n < 0 {
                n = tx.add_n1(&COUNTERS, repo_id, id, -n).await?;
            }
            if i == 0 {
                left = n;
            }
        }
        tx.commit().await?;
        Ok(left.max(0) as u64)
    }

    /// The open-CR count of a principal (`owner` = false) or owner (`owner` = true) in a repo.
    pub async fn open_count(
        &self,
        cx: &Cx,
        repo_id: &str,
        principal_id: &str,
        owner: bool,
    ) -> Result<u64> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let id = if owner {
            format!("cr:o:{principal_id}")
        } else {
            format!("cr:p:{principal_id}")
        };
        Ok(counter(&mut tx, repo_id, &id).await?.max(0) as u64)
    }

    /// Feeds the reputation of `principal_id` with the fate of one contribution and returns
    /// the new score. `org_id` names the log that records it.
    pub async fn record_outcome(
        &self,
        cx: &Cx,
        org_id: &str,
        principal_id: &str,
        outcome: Outcome,
    ) -> Result<f64> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let existing = tx
            .get_msg::<Reputation>(&REPUTATION, "rep", principal_id)
            .await?;
        let mut r = existing
            .as_ref()
            .map(|(r, _)| r.clone())
            .unwrap_or_default();
        let before = reputation_score(&r);
        match outcome {
            Outcome::Landed => r.landed = r.landed.saturating_add(1),
            Outcome::Rejected => r.rejected = r.rejected.saturating_add(1),
            Outcome::Spam => r.spam = r.spam.saturating_add(1),
        }
        let score = reputation_score(&r);
        let mut row = Row::new("rep", principal_id).msg(&r);
        if let Some((_, old)) = &existing {
            row.version = old.version;
            tx.update(&REPUTATION, &row).await?;
        } else {
            tx.insert(&REPUTATION, &row).await?;
        }
        record(
            &mut tx,
            Change {
                kind: "moderation.reputation_changed",
                action: "moderation.reputation",
                org: org_id.to_string(),
                repo: String::new(),
                subject: Some(super::subject_in(
                    "",
                    bee_proto::v1::SubjectKind::User,
                    principal_id,
                )),
                before: Some(json!({ "score": before })),
                after: json!({
                    "principal": principal_id,
                    "outcome": format!("{outcome:?}"),
                    "landed": r.landed,
                    "rejected": r.rejected,
                    "spam": r.spam,
                    "score": score,
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(score)
    }

    /// The reputation score of a principal (0.5 for one never recorded).
    pub async fn reputation(&self, cx: &Cx, principal_id: &str) -> Result<f64> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(reputation_score(&tally(&mut tx, principal_id).await?))
    }

    /// Deletes what expired: blocks and interaction limits (each recorded as its removal),
    /// throttles and window rows older than the window. The maintenance loop calls it.
    pub async fn sweep_expired(&self, cx: &Cx) -> Result<SweepReport> {
        let mut report = SweepReport::default();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let expired = |limit: usize| Scan {
            n1_max: Some(now as i64),
            limit,
            ..Scan::default()
        };
        for row in tx.scan(&BLOCKS, &expired(SWEEP_BATCH)).await? {
            // Active while `n1 > now`; `n1 == now` has just expired.
            tx.delete(&BLOCKS, &row.scope, &row.id).await?;
            let org = row.scope.strip_prefix("org:").unwrap_or("").to_string();
            record(
                &mut tx,
                Change {
                    kind: "moderation.unblocked",
                    action: "moderation.block_expire",
                    org,
                    repo: String::new(),
                    subject: Some(super::subject_in(
                        "",
                        bee_proto::v1::SubjectKind::User,
                        &row.id,
                    )),
                    before: None,
                    after: json!({ "scope": row.scope, "unblocked": row.id, "expired": true }),
                },
            )
            .await?;
            report.blocks += 1;
        }
        for row in tx.scan(&LIMITS, &expired(SWEEP_BATCH)).await? {
            tx.delete(&LIMITS, &row.scope, &row.id).await?;
            let org = row.scope.strip_prefix("org:").unwrap_or("").to_string();
            let repo = row.scope.strip_prefix("repo:").unwrap_or("").to_string();
            record(
                &mut tx,
                Change {
                    kind: "moderation.limit_removed",
                    action: "moderation.limit_expire",
                    org,
                    repo,
                    subject: None,
                    before: None,
                    after: json!({ "scope": row.scope, "removed": true, "expired": true }),
                },
            )
            .await?;
            report.limits += 1;
        }
        for row in tx.scan(&THROTTLES, &expired(SWEEP_BATCH)).await? {
            tx.delete(&THROTTLES, &row.scope, &row.id).await?;
            report.throttles += 1;
        }
        let stale = Scan {
            n1_max: Some(now.saturating_sub(WINDOW_MS) as i64),
            limit: SWEEP_BATCH,
            ..Scan::default()
        };
        for row in tx.scan(&ACTIVITY, &stale).await? {
            tx.delete(&ACTIVITY, &row.scope, &row.id).await?;
            report.activity += 1;
        }
        tx.commit().await?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(landed: u64, rejected: u64, spam: u64) -> Reputation {
        Reputation {
            landed,
            rejected,
            spam,
        }
    }

    #[test]
    fn reputation_is_a_pure_function_of_the_tally() {
        assert_eq!(reputation_score(&rep(0, 0, 0)), 0.5);
        assert_eq!(reputation_score(&rep(2, 1, 0)), 0.6);
        assert_eq!(reputation_score(&rep(0, 0, 1)), 0.2);
        assert_eq!(reputation_score(&rep(0, 1, 0)), 1.0 / 3.0);
        // Spam costs more than a rejection; landed work earns it back.
        assert!(reputation_score(&rep(0, 0, 1)) < reputation_score(&rep(0, 1, 0)));
        assert!(reputation_score(&rep(9, 0, 1)) > reputation_score(&rep(0, 0, 1)));
        let r = rep(u64::MAX, u64::MAX, u64::MAX);
        assert!(reputation_score(&r).is_finite());
        assert!(reputation_score(&rep(1000, 0, 0)) < 1.0);
    }

    #[test]
    fn window_scopes_never_mix_repos_owners_or_kinds() {
        let a = window_scope("r1", "o1", "cr");
        assert_ne!(a, window_scope("r2", "o1", "cr"));
        assert_ne!(a, window_scope("r1", "o2", "cr"));
        assert_ne!(a, window_scope("r1", "o1", "comment"));
    }
}
