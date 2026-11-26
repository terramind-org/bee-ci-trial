//! Comment moderation and conversation locks.
//!
//! Comments belong to the work module. Moderation keeps its own record per comment: whether it
//! is minimized (reason, who, when) and whether it was edited, plus the comment's edit
//! history. Minimizing never touches the body or the history, so a hidden comment keeps every
//! earlier version for moderators and auditors.
//!
//! Locks name a conversation (issue, CR or discussion) of a repo. While a conversation is
//! locked, writes on it by principals without collaborator standing fail with `LOCKED`.

use bee_proto::v1::{
    Comment, CommentEdit, MinimizedReason, ModerationLock, ModerationLockReason, Principal,
    SubjectKind, SubjectRef,
};
use prost::Message;
use serde_json::json;

use super::tables::{COMMENTS, CommentState, EDITS, LOCKS};
use super::{
    Change, Moderation, SURFACE, is_conversation, record, subject_in, subject_key, subject_repo_id,
};
use crate::cx::Cx;
use crate::enterprise::common::scan_all;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Row, Tx};

/// Scope of the per-comment state rows.
const STATE_SCOPE: &str = "c";
/// Most edits kept per comment.
pub const MAX_EDITS: usize = 10_000;

/// A comment as moderation needs to know it.
#[derive(Clone, Debug)]
pub struct CommentRef {
    /// Comment id.
    pub comment_id: String,
    /// Org of the comment's repo.
    pub org_id: String,
    /// The comment's repo.
    pub repo_id: String,
    /// The subject of the comment (what the comment itself is, by repo id).
    pub subject: SubjectRef,
}

async fn state_of(tx: &mut Tx, id: &str) -> Result<Option<(CommentState, Row)>> {
    tx.get_msg::<CommentState>(&COMMENTS, STATE_SCOPE, id).await
}

impl Moderation {
    /// Minimizes a comment with a reason (hiding it behind the reason; body and history stay).
    /// Minimizing an already minimized comment changes the reason. `source` names what
    /// prompted it (`rpc`, `block`, `report`) for the audit entry.
    pub async fn minimize(
        &self,
        cx: &Cx,
        c: &CommentRef,
        reason: MinimizedReason,
        source: &str,
    ) -> Result<CommentState> {
        if reason == MinimizedReason::Unspecified {
            return Err(ForgeError::missing("reason"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let existing = state_of(&mut tx, &c.comment_id).await?;
        let edited = existing.as_ref().is_some_and(|(s, _)| s.edited);
        let before = existing
            .as_ref()
            .map(|(s, _)| json!({ "reason": s.reason }));
        let state = CommentState {
            comment_id: c.comment_id.clone(),
            subject: Some(c.subject.clone()),
            reason: reason as i32,
            by: Some(cx.principal()),
            at_ms: now,
            edited,
        };
        let row = Row::new(STATE_SCOPE, c.comment_id.clone()).msg(&state);
        tx.put(&COMMENTS, &row).await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.comment_minimized",
                action: "moderation.minimize",
                org: c.org_id.clone(),
                repo: c.repo_id.clone(),
                subject: Some(c.subject.clone()),
                before,
                after: json!({
                    "comment": c.comment_id,
                    "reason": reason.as_str_name(),
                    "source": source,
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(state)
    }

    /// Shows a minimized comment again. `INVALID_STATE` when it is not minimized.
    pub async fn unminimize(&self, cx: &Cx, c: &CommentRef) -> Result<CommentState> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut state, row)) = state_of(&mut tx, &c.comment_id).await? else {
            return Err(ForgeError::invalid_state("comment is not minimized"));
        };
        if state.reason == 0 {
            return Err(ForgeError::invalid_state("comment is not minimized"));
        }
        let before = json!({ "reason": state.reason });
        state.reason = 0;
        state.by = None;
        state.at_ms = 0;
        let next = Row {
            body: state.encode_to_vec(),
            ..row
        };
        tx.update(&COMMENTS, &next).await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.comment_unminimized",
                action: "moderation.unminimize",
                org: c.org_id.clone(),
                repo: c.repo_id.clone(),
                subject: Some(c.subject.clone()),
                before: Some(before),
                after: json!({ "comment": c.comment_id, "reason": null }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(state)
    }

    /// Moderation state of a comment, if moderation knows it (minimized or edited).
    pub async fn comment_state(&self, cx: &Cx, comment_id: &str) -> Result<Option<CommentState>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(state_of(&mut tx, comment_id).await?.map(|(s, _)| s))
    }

    /// The `Comment` message for a moderation action: the comment from the
    /// [`CommentSource`](super::CommentSource) when there is one, with `minimized_reason` and
    /// `edited` taken from the moderation record.
    pub async fn comment_view(&self, cx: &Cx, c: &CommentRef) -> Result<Comment> {
        let mut comment = match &self.cfg.comments {
            Some(src) => src.comment(cx, &c.comment_id).await?,
            None => None,
        }
        .unwrap_or_else(|| Comment {
            id: c.comment_id.clone(),
            subject: Some(c.subject.clone()),
            ..Comment::default()
        });
        self.decorate_comment(cx, &mut comment).await?;
        Ok(comment)
    }

    /// Fills `minimized_reason` and `edited` of a comment from the moderation record. The
    /// work module calls it for every comment it returns.
    pub async fn decorate_comment(&self, cx: &Cx, comment: &mut Comment) -> Result<()> {
        if let Some(s) = self.comment_state(cx, &comment.id).await? {
            comment.minimized_reason = s.reason;
            comment.edited |= s.edited;
        }
        Ok(())
    }

    /// Records that `editor` replaced `previous_body` of a comment. The work module calls it
    /// when a comment is edited; the history survives minimizing.
    pub async fn record_comment_edit(
        &self,
        cx: &Cx,
        comment_id: &str,
        editor: &Principal,
        previous_body: &str,
    ) -> Result<CommentEdit> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let edit = CommentEdit {
            comment_id: comment_id.to_string(),
            editor: Some(editor.clone()),
            previous_body: previous_body.to_string(),
            edited_at: Some(ts(now)),
        };
        if tx.count(&EDITS, comment_id, None).await? as usize >= MAX_EDITS {
            return Err(ForgeError::too_large(
                "comment_edits",
                MAX_EDITS as u64,
                MAX_EDITS as u64 + 1,
            ));
        }
        let id = tx.new_id()?;
        tx.insert(&EDITS, &Row::new(comment_id, id).msg(&edit))
            .await?;
        match state_of(&mut tx, comment_id).await? {
            Some((mut s, row)) if !s.edited => {
                s.edited = true;
                tx.update(
                    &COMMENTS,
                    &Row {
                        body: s.encode_to_vec(),
                        ..row
                    },
                )
                .await?;
            }
            Some(_) => {}
            None => {
                let s = CommentState {
                    comment_id: comment_id.to_string(),
                    subject: None,
                    reason: 0,
                    by: None,
                    at_ms: 0,
                    edited: true,
                };
                tx.insert(&COMMENTS, &Row::new(STATE_SCOPE, comment_id).msg(&s))
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(edit)
    }

    /// Every earlier version of a comment, oldest first.
    pub async fn edit_history(&self, cx: &Cx, comment_id: &str) -> Result<Vec<CommentEdit>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(scan_all::<CommentEdit>(&mut tx, &EDITS, comment_id, None)
            .await?
            .into_iter()
            .map(|(e, _)| e)
            .collect())
    }

    /// Locks a conversation. `INVALID_STATE` when it is already locked, `INVALID_ARGUMENT`
    /// when the subject is not an issue, CR or discussion.
    pub async fn lock_conversation(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        subject: &SubjectRef,
        reason: ModerationLockReason,
    ) -> Result<ModerationLock> {
        if !is_conversation(subject.kind) || subject.id.is_empty() {
            return Err(ForgeError::invalid(
                "not_a_conversation",
                "only issues, change requests and discussions can be locked",
            ));
        }
        let subject = subject_in(
            repo_id,
            SubjectKind::try_from(subject.kind).unwrap_or(SubjectKind::Unspecified),
            &subject.id,
        );
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let key = subject_key(&subject);
        if tx.get(&LOCKS, repo_id, &key).await?.is_some() {
            return Err(ForgeError::invalid_state("conversation is already locked"));
        }
        let lock = ModerationLock {
            subject: Some(subject.clone()),
            reason: reason as i32,
            locked_by: Some(cx.principal()),
            locked_at: Some(ts(now)),
        };
        tx.insert(&LOCKS, &Row::new(repo_id, key).msg(&lock))
            .await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.conversation_locked",
                action: "moderation.lock",
                org: org_id.to_string(),
                repo: repo_id.to_string(),
                subject: Some(subject),
                before: None,
                after: json!({ "reason": reason.as_str_name() }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(lock)
    }

    /// Unlocks a conversation. `INVALID_STATE` when it is not locked.
    pub async fn unlock_conversation(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        subject: &SubjectRef,
    ) -> Result<()> {
        let subject = subject_in(
            repo_id,
            SubjectKind::try_from(subject.kind).unwrap_or(SubjectKind::Unspecified),
            &subject.id,
        );
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let key = subject_key(&subject);
        let Some((lock, _)) = tx.get_msg::<ModerationLock>(&LOCKS, repo_id, &key).await? else {
            return Err(ForgeError::invalid_state("conversation is not locked"));
        };
        tx.delete(&LOCKS, repo_id, &key).await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.conversation_unlocked",
                action: "moderation.unlock",
                org: org_id.to_string(),
                repo: repo_id.to_string(),
                subject: Some(subject),
                before: Some(json!({ "reason": lock.reason })),
                after: json!({ "locked": false }),
            },
        )
        .await?;
        tx.commit().await
    }

    /// The lock on a conversation, if it is locked.
    pub async fn lock_of(
        &self,
        cx: &Cx,
        repo_id: &str,
        subject: &SubjectRef,
    ) -> Result<Option<ModerationLock>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .get_msg::<ModerationLock>(&LOCKS, repo_id, &subject_key(subject))
            .await?
            .map(|(l, _)| l))
    }

    /// The repo id a subject names, resolving a full name through the orgs module.
    pub(crate) async fn repo_id_of(&self, cx: &Cx, s: &SubjectRef) -> Result<Option<String>> {
        if let Some(id) = subject_repo_id(s) {
            return Ok(Some(id.to_string()));
        }
        match &s.repo {
            Some(r) => Ok(Some(self.forge.orgs.resolve_repo(cx, r).await?.id)),
            None => Ok(None),
        }
    }
}
