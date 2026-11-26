//! Blocks: org-wide and personal block lists.
//!
//! A block names a principal in a scope. Org blocks apply to every repo of the org; personal
//! blocks apply to content in the blocker's namespace (repos they own, conversations they
//! started). A blocked principal cannot comment, react, open issues or CRs, or be assigned:
//! [`Moderation::check_write`](super::Moderation::check_write) fails with `BLOCKED`. A block
//! covers the principal's whole delegation chain, so an agent is blocked when its owner is
//! and the owner is blocked when the agent is. Blocks may expire; an expired block is
//! invisible at once and deleted by [`Moderation::sweep_expired`](super::Moderation).

use bee_proto::v1::{BlockEntry, BlockScope, Principal, SubjectRef, block_scope};
use serde_json::json;

use super::tables::BLOCKS;
use super::{
    Change, CommentScope, MAX_DURATION_SECS, Moderation, SURFACE, record, subject_key,
    subject_repo_id,
};
use crate::cx::Cx;
use crate::enterprise::common::scan_page;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::store::{Row, Tx};

/// Where a block applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockTarget {
    /// Every repo of an org (the org id).
    Org(String),
    /// Content in a user's namespace (the user id).
    Personal(String),
}

impl BlockTarget {
    /// The partition key of the scope.
    pub fn key(&self) -> String {
        match self {
            BlockTarget::Org(o) => format!("org:{o}"),
            BlockTarget::Personal(u) => format!("user:{u}"),
        }
    }

    /// The org whose log records changes to the scope (empty for personal blocks).
    pub fn org_id(&self) -> &str {
        match self {
            BlockTarget::Org(o) => o,
            BlockTarget::Personal(_) => "",
        }
    }

    /// The scope as the proto message (org by id).
    pub fn to_proto(&self) -> BlockScope {
        BlockScope {
            scope: Some(match self {
                BlockTarget::Org(o) => block_scope::Scope::Org(o.clone()),
                BlockTarget::Personal(_) => block_scope::Scope::Personal(true),
            }),
        }
    }
}

/// The block of `principal` in scope `key`, if one is in force at `now`.
pub(crate) async fn live_block(
    tx: &mut Tx,
    key: &str,
    principal: &str,
    now: u64,
) -> Result<Option<BlockEntry>> {
    Ok(tx
        .get_msg::<BlockEntry>(&BLOCKS, key, principal)
        .await?
        .filter(|(_, row)| row.n1.is_none_or(|e| e as u64 > now))
        .map(|(b, _)| b))
}

/// What a block records.
#[derive(Clone, Debug, Default)]
pub struct NewBlock {
    /// The principal to block.
    pub blocked: Principal,
    /// Why (free text, shown to moderators).
    pub reason: String,
    /// A private note.
    pub note: String,
    /// Seconds until the block expires; 0 is permanent.
    pub duration_secs: u64,
    /// The content that prompted the block, recorded in the audit entry.
    pub source: Option<SubjectRef>,
}

impl Moderation {
    /// Blocks `blocked` in `target`. Blocking again replaces the earlier block (reason,
    /// note and expiry). `duration_secs` of 0 is permanent. `source` names the content that
    /// prompted the block and is recorded in the audit entry.
    pub async fn block(&self, cx: &Cx, target: &BlockTarget, new: NewBlock) -> Result<BlockEntry> {
        let NewBlock {
            blocked,
            reason,
            note,
            duration_secs,
            source,
        } = new;
        let (reason, note, source) = (reason.as_str(), note.as_str(), source.as_ref());
        if blocked.id.is_empty() {
            return Err(ForgeError::missing("principal_id"));
        }
        if cx.chain.contains(&blocked.id) {
            return Err(ForgeError::invalid(
                "cannot_block_self",
                "a principal cannot block itself or its delegator",
            ));
        }
        if duration_secs > MAX_DURATION_SECS {
            return Err(ForgeError::invalid(
                "duration_too_long",
                format!("duration_seconds is at most {MAX_DURATION_SECS}"),
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let expires = (duration_secs > 0).then(|| now + duration_secs * 1000);
        let entry = BlockEntry {
            scope: Some(target.to_proto()),
            blocked: Some(blocked.clone()),
            blocked_by: Some(cx.principal()),
            reason: reason.to_string(),
            note: note.to_string(),
            created_at: Some(ts(now)),
            expires_at: expires.map(ts),
        };
        let mut row = Row::new(target.key(), blocked.id.clone()).msg(&entry);
        row.n1 = expires.map(|e| e as i64);
        tx.put(&BLOCKS, &row).await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.blocked",
                action: "moderation.block",
                org: target.org_id().to_string(),
                repo: String::new(),
                subject: Some(super::subject_in(
                    "",
                    bee_proto::v1::SubjectKind::User,
                    &blocked.id,
                )),
                before: None,
                after: json!({
                    "scope": target.key(),
                    "blocked": blocked.id,
                    "reason": reason,
                    "expires_at_ms": expires,
                    "source": source.map(|s| json!({
                        "kind": s.kind,
                        "id": s.id,
                        "repo": subject_repo_id(s),
                        "key": subject_key(s),
                    })),
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(entry)
    }

    /// Removes a block. `NOT_FOUND` when the principal is not blocked in the scope.
    pub async fn unblock(&self, cx: &Cx, target: &BlockTarget, principal_id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some(entry) = live_block(&mut tx, &target.key(), principal_id, now).await? else {
            return Err(ForgeError::not_found(format!("block of {principal_id}")));
        };
        tx.delete(&BLOCKS, &target.key(), principal_id).await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.unblocked",
                action: "moderation.unblock",
                org: target.org_id().to_string(),
                repo: String::new(),
                subject: Some(super::subject_in(
                    "",
                    bee_proto::v1::SubjectKind::User,
                    principal_id,
                )),
                before: Some(json!({ "reason": entry.reason })),
                after: json!({ "scope": target.key(), "unblocked": principal_id }),
            },
        )
        .await?;
        tx.commit().await
    }

    /// The block of `principal_id` in the scope, if one is in force.
    pub async fn blocked(
        &self,
        cx: &Cx,
        target: &BlockTarget,
        principal_id: &str,
    ) -> Result<Option<BlockEntry>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        live_block(&mut tx, &target.key(), principal_id, now).await
    }

    /// The blocks in force in a scope, by blocked principal id, paged. Expired blocks are
    /// left out of the page (so a page may be shorter than `limit`).
    pub async fn list_blocks(
        &self,
        cx: &Cx,
        target: &BlockTarget,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<BlockEntry>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (rows, next) =
            scan_page::<BlockEntry>(&mut tx, &BLOCKS, &target.key(), None, after, limit).await?;
        Ok((
            rows.into_iter()
                .filter(|(_, r)| r.n1.is_none_or(|e| e as u64 > now))
                .map(|(b, _)| b)
                .collect(),
            next,
        ))
    }

    /// Minimizes the comments `user_id` wrote in the scope, through the comment-moderation
    /// records (reason `SPAM`). Returns how many comments were hidden; needs a
    /// [`CommentSource`](super::CommentSource).
    pub async fn hide_user_content(
        &self,
        cx: &Cx,
        target: &BlockTarget,
        user_id: &str,
    ) -> Result<usize> {
        let Some(source) = &self.cfg.comments else {
            return Ok(0);
        };
        let scope = match target {
            BlockTarget::Org(o) => CommentScope::Org(o.clone()),
            BlockTarget::Personal(u) => CommentScope::Personal(u.clone()),
        };
        let comments = source.comments_by_author(cx, &scope, user_id).await?;
        let mut hidden = 0;
        for c in comments {
            let Some(subject) = c.subject.clone() else {
                continue;
            };
            let Some(repo_ref) = subject.repo.clone() else {
                continue;
            };
            let repo = self.forge.orgs.resolve_repo(cx, &repo_ref).await?;
            if let BlockTarget::Org(o) = target
                && repo.org_id != *o
            {
                continue;
            }
            self.minimize(
                cx,
                &super::comments::CommentRef {
                    comment_id: c.id.clone(),
                    org_id: repo.org_id.clone(),
                    repo_id: repo.id.clone(),
                    subject: super::subject_in(
                        &repo.id,
                        bee_proto::v1::SubjectKind::Comment,
                        &c.id,
                    ),
                },
                bee_proto::v1::MinimizedReason::Spam,
                "block",
            )
            .await?;
            hidden += 1;
        }
        Ok(hidden)
    }

    /// Expiry (ms) of a block entry, if any.
    pub fn block_expiry(entry: &BlockEntry) -> Option<u64> {
        entry.expires_at.as_ref().map(ts_ms)
    }
}
