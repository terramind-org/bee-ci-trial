//! Conversation comments of change requests by comment id, for the cross-domain
//! `CommentService`: where a comment lives, edits with history, deletion, and reactions on a
//! comment or on the CR itself.
//!
//! Conversation comments are rows of [`super::COMMENTS`] scoped by CR id; the lookup table
//! [`super::COMMENT_IDS`] maps a comment id to its CR and repo (written by
//! [`Changes::comment`] since migration 2; comments written before it are found only through
//! their CR).

use bee_proto::v1::{Comment, CommentEdit, Principal, Reaction, ReactionContent, SubjectKind};

use super::model::{CommentRecord, reaction_groups, toggle_reaction};
use super::store::Changes;
use super::{COMMENT_IDS, COMMENTS, SURFACE};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Row, Tx};

/// Where a CR conversation comment lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentHome {
    /// Repo id.
    pub repo_id: String,
    /// Change request id.
    pub cr_id: String,
}

/// Records where a new conversation comment lives (inside the creating transaction).
pub(crate) async fn index_comment(
    tx: &mut Tx,
    comment_id: &str,
    repo_id: &str,
    cr_id: &str,
) -> Result<()> {
    tx.insert(
        &COMMENT_IDS,
        &Row::new("", comment_id).k1(cr_id).k2(repo_id),
    )
    .await
}

fn not_found(id: &str) -> ForgeError {
    ForgeError::not_found(format!("comment {id}"))
}

impl Changes {
    /// The repo and CR of a conversation comment, if `comment_id` is one.
    pub async fn comment_home(&self, cx: &Cx, comment_id: &str) -> Result<Option<CommentHome>> {
        if comment_id.is_empty() {
            return Ok(None);
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .get(&COMMENT_IDS, "", comment_id)
            .await?
            .map(|r| CommentHome {
                repo_id: r.k2.unwrap_or_default(),
                cr_id: r.k1.unwrap_or_default(),
            }))
    }

    async fn conversation(
        tx: &mut Tx,
        home: &CommentHome,
        comment_id: &str,
    ) -> Result<(CommentRecord, Row, Comment)> {
        let row = tx
            .get_for_update(&COMMENTS, &home.cr_id, comment_id)
            .await?
            .ok_or_else(|| not_found(comment_id))?;
        let rec: CommentRecord = row.decode()?;
        let c = rec
            .conversation
            .clone()
            .ok_or_else(|| not_found(comment_id))?;
        Ok((rec, row, c))
    }

    /// One conversation comment with the viewer's reactions.
    pub async fn get_comment(
        &self,
        cx: &Cx,
        home: &CommentHome,
        comment_id: &str,
    ) -> Result<Comment> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let row = tx
            .get(&COMMENTS, &home.cr_id, comment_id)
            .await?
            .ok_or_else(|| not_found(comment_id))?;
        let rec: CommentRecord = row.decode()?;
        let mut c = rec.conversation.ok_or_else(|| not_found(comment_id))?;
        c.reactions = reaction_groups(&rec.reactions, &cx.actor_id);
        Ok(c)
    }

    /// Edits a conversation comment, keeping the previous body. The author may edit; a
    /// `moderator` (triage) may edit any comment.
    pub async fn edit_comment(
        &self,
        cx: &Cx,
        home: &CommentHome,
        comment_id: &str,
        body: &str,
        expected_version: u64,
        moderator: bool,
    ) -> Result<Comment> {
        if body.trim().is_empty() {
            return Err(ForgeError::missing("body"));
        }
        if body.len() > super::store::MAX_BODY {
            return Err(ForgeError::too_large(
                "body",
                super::store::MAX_BODY as u64,
                body.len() as u64,
            ));
        }
        let editor = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row, mut c) = Self::conversation(&mut tx, home, comment_id).await?;
        if expected_version != 0 && expected_version != row.version {
            return Err(ForgeError::version_mismatch(expected_version, row.version));
        }
        if !moderator && c.author.as_ref().map(|a| a.id.as_str()) != Some(cx.actor_id.as_str()) {
            return Err(ForgeError::permission_denied(
                "only the author or a maintainer may edit a comment",
            ));
        }
        if c.body != body {
            let now = tx.now_ms();
            rec.edits.push(CommentEdit {
                comment_id: comment_id.to_string(),
                editor: Some(editor),
                previous_body: c.body.clone(),
                edited_at: Some(ts(now)),
            });
            c.body = body.to_string();
            c.edited = true;
            c.updated_at = Some(ts(now));
            rec.conversation = Some(c.clone());
            tx.update(&COMMENTS, &row.clone().msg(&rec)).await?;
            let (cr, _) = Self::load(&mut tx, &home.repo_id, &home.cr_id).await?;
            let json = serde_json::to_string(&c).unwrap_or_default();
            Self::emit_cr(&mut tx, "cr_comment.edited", &cr, json.clone()).await?;
            tx.audit(
                AuditDraft::new("cr.comment_edit", &cr.org_id)
                    .repo_id(&home.repo_id)
                    .target(SubjectKind::Comment, comment_id)
                    .after(&json),
            )
            .await?;
            tx.commit().await?;
        }
        c.reactions = reaction_groups(&rec.reactions, &cx.actor_id);
        Ok(c)
    }

    /// Deletes a conversation comment (the author or a `moderator`).
    pub async fn delete_comment(
        &self,
        cx: &Cx,
        home: &CommentHome,
        comment_id: &str,
        moderator: bool,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (_, _, c) = Self::conversation(&mut tx, home, comment_id).await?;
        if !moderator && c.author.as_ref().map(|a| a.id.as_str()) != Some(cx.actor_id.as_str()) {
            return Err(ForgeError::permission_denied(
                "only the author or a maintainer may delete a comment",
            ));
        }
        tx.delete(&COMMENTS, &home.cr_id, comment_id).await?;
        tx.delete(&COMMENT_IDS, "", comment_id).await?;
        let (mut cr, row) = Self::load(&mut tx, &home.repo_id, &home.cr_id).await?;
        cr.comment_count = cr.comment_count.saturating_sub(1);
        let (cr, _) = Self::save(&mut tx, cr, &row).await?;
        let json = serde_json::json!({ "id": comment_id, "change_request_id": home.cr_id });
        Self::emit_cr(&mut tx, "cr_comment.deleted", &cr, json.to_string()).await?;
        tx.audit(
            AuditDraft::new("cr.comment_delete", &cr.org_id)
                .repo_id(&home.repo_id)
                .target(SubjectKind::Comment, comment_id),
        )
        .await?;
        tx.commit().await
    }

    /// Every edit of a conversation comment, oldest first.
    pub async fn comment_edits(
        &self,
        cx: &Cx,
        home: &CommentHome,
        comment_id: &str,
    ) -> Result<Vec<CommentEdit>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let row = tx
            .get(&COMMENTS, &home.cr_id, comment_id)
            .await?
            .ok_or_else(|| not_found(comment_id))?;
        let rec: CommentRecord = row.decode()?;
        if rec.conversation.is_none() {
            return Err(not_found(comment_id));
        }
        Ok(rec.edits)
    }

    /// The reactions on a conversation comment, as one record per principal and content.
    pub async fn comment_reactions(
        &self,
        cx: &Cx,
        home: &CommentHome,
        comment_id: &str,
    ) -> Result<Vec<Reaction>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let row = tx
            .get(&COMMENTS, &home.cr_id, comment_id)
            .await?
            .ok_or_else(|| not_found(comment_id))?;
        let rec: CommentRecord = row.decode()?;
        let mut out = Vec::new();
        for r in &rec.reactions {
            out.push(reaction_of(
                comment_id,
                self.principal(&r.principal_id).await,
                r.content,
            ));
        }
        Ok(out)
    }

    /// The reactions on a CR itself, as one record per principal and content.
    pub async fn cr_reactions(&self, cx: &Cx, repo_id: &str, cr_id: &str) -> Result<Vec<Reaction>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, repo_id, cr_id).await?;
        let mut out = Vec::new();
        for r in &rec.reactions {
            out.push(reaction_of(
                cr_id,
                self.principal(&r.principal_id).await,
                r.content,
            ));
        }
        Ok(out)
    }

    /// Adds (or removes) the caller's reaction on a conversation comment, by comment id.
    pub async fn react_comment_by_id(
        &self,
        cx: &Cx,
        home: &CommentHome,
        comment_id: &str,
        content: ReactionContent,
        add: bool,
    ) -> Result<Reaction> {
        if content == ReactionContent::Unspecified {
            return Err(ForgeError::missing("content"));
        }
        let me = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row, _) = Self::conversation(&mut tx, home, comment_id).await?;
        if toggle_reaction(&mut rec.reactions, &cx.actor_id, content as i32, add) {
            tx.update(&COMMENTS, &row.clone().msg(&rec)).await?;
            let (cr, _) = Self::load(&mut tx, &home.repo_id, &home.cr_id).await?;
            Self::emit_cr(
                &mut tx,
                "cr.reacted",
                &cr,
                serde_json::json!({
                    "comment_id": comment_id,
                    "content": content.as_str_name(),
                    "added": add,
                })
                .to_string(),
            )
            .await?;
            tx.audit(
                AuditDraft::new("cr.react", &cr.org_id)
                    .repo_id(&home.repo_id)
                    .target(SubjectKind::Comment, comment_id),
            )
            .await?;
            tx.commit().await?;
        }
        Ok(reaction_of(comment_id, me, content as i32))
    }
}

/// A reaction record for a CR-module reaction (these rows keep no id or time of their own,
/// so the id is derived from the subject, principal and content).
fn reaction_of(subject_id: &str, user: Principal, content: i32) -> Reaction {
    Reaction {
        id: format!("{subject_id}:{}:{content}", user.id),
        content,
        user: Some(user),
        created_at: None,
    }
}
