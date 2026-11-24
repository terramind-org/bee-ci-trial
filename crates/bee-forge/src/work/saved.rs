//! Saved replies: per-user reusable snippets.

use bee_proto::v1::SavedReply;

use super::*;
use crate::ids::ts;

fn audit_of(r: &SavedReply) -> String {
    serde_json::json!({"saved_reply": r.id, "title": r.title}).to_string()
}

impl Work {
    /// Creates a saved reply for the caller.
    pub async fn create_saved_reply(&self, cx: &Cx, title: &str, body: &str) -> Result<SavedReply> {
        let title = valid_name("title", title, 256)?;
        if body.trim().is_empty() {
            return Err(ForgeError::missing("body"));
        }
        if body.len() > limits::BODY {
            return Err(ForgeError::too_large(
                "body_bytes",
                limits::BODY as u64,
                body.len() as u64,
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let owner = cx.actor_id.clone();
        let count = tx.count(&SAVED, &owner, None).await? as usize;
        if count >= limits::SAVED_REPLIES {
            return Err(exhausted("saved_replies", limits::SAVED_REPLIES, count + 1));
        }
        if !tx
            .scan(
                &SAVED,
                &Scan::scope(owner.clone(), 1).key(Key::K1(lc(&title))),
            )
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("saved reply {title:?}")));
        }
        let now = ts(tx.now_ms());
        let id = tx.new_id()?;
        let r = SavedReply {
            id: id.clone(),
            title: title.clone(),
            body: body.to_string(),
            created_at: Some(now),
            updated_at: Some(now),
            version: 1,
        };
        tx.insert(&SAVED, &Row::new(owner, id).k1(lc(&title)).msg(&r))
            .await?;
        saved_event(&mut tx, cx, "saved_reply.create", &r).await?;
        tx.commit().await?;
        Ok(r)
    }

    /// One of the caller's saved replies.
    pub async fn get_saved_reply(&self, cx: &Cx, id: &str) -> Result<SavedReply> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut r, row) = tx
            .get_msg::<SavedReply>(&SAVED, &cx.actor_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("saved reply {id}")))?;
        r.version = row.version;
        Ok(r)
    }

    /// Edits one of the caller's saved replies.
    pub async fn update_saved_reply(
        &self,
        cx: &Cx,
        id: &str,
        title: Option<String>,
        body: Option<String>,
        expected_version: u64,
    ) -> Result<SavedReply> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let row = tx
            .get_for_update(&SAVED, &cx.actor_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("saved reply {id}")))?;
        check_version(expected_version, row.version)?;
        let mut r: SavedReply = row.decode()?;
        if let Some(t) = title {
            let t = valid_name("title", &t, 256)?;
            let clash = tx
                .scan(
                    &SAVED,
                    &Scan::scope(cx.actor_id.clone(), 1).key(Key::K1(lc(&t))),
                )
                .await?
                .into_iter()
                .any(|o| o.id != id);
            if clash {
                return Err(ForgeError::already_exists(format!("saved reply {t:?}")));
            }
            r.title = t;
        }
        if let Some(b) = body {
            if b.trim().is_empty() {
                return Err(ForgeError::missing("body"));
            }
            if b.len() > limits::BODY {
                return Err(ForgeError::too_large(
                    "body_bytes",
                    limits::BODY as u64,
                    b.len() as u64,
                ));
            }
            r.body = b;
        }
        r.updated_at = Some(ts(tx.now_ms()));
        let v = tx
            .update(
                &SAVED,
                &Row::new(cx.actor_id.clone(), id)
                    .k1(lc(&r.title))
                    .version(row.version)
                    .msg(&r),
            )
            .await?;
        r.version = v;
        saved_event(&mut tx, cx, "saved_reply.update", &r).await?;
        tx.commit().await?;
        Ok(r)
    }

    /// Deletes one of the caller's saved replies.
    pub async fn delete_saved_reply(&self, cx: &Cx, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (r, _) = tx
            .get_msg::<SavedReply>(&SAVED, &cx.actor_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("saved reply {id}")))?;
        tx.delete(&SAVED, &cx.actor_id, id).await?;
        saved_event(&mut tx, cx, "saved_reply.delete", &r).await?;
        tx.commit().await
    }

    /// The caller's saved replies by title.
    pub async fn list_saved_replies(
        &self,
        cx: &Cx,
        query: &str,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<SavedReply>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let q = lc(query);
        let mut all: Vec<SavedReply> =
            scan_all_msgs::<SavedReply>(&mut tx, &SAVED, Scan::scope(cx.actor_id.clone(), 1))
                .await?
                .into_iter()
                .map(|(mut r, row)| {
                    r.version = row.version;
                    r
                })
                .filter(|r| q.is_empty() || r.title.to_lowercase().contains(&q))
                .collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        let rest: Vec<SavedReply> = all
            .into_iter()
            .filter(|r| after.is_empty() || r.id.as_str() > after)
            .collect();
        let next = if rest.len() > limit {
            rest[limit - 1].id.clone()
        } else {
            String::new()
        };
        Ok((rest.into_iter().take(limit).collect(), next))
    }
}

/// Saved replies are personal: the audit entry goes to the system chain and no event is sent
/// beyond the audit record.
async fn saved_event(tx: &mut Tx, _cx: &Cx, action: &str, r: &SavedReply) -> Result<()> {
    tx.audit(
        crate::audit::AuditDraft::new(action, "")
            .target_id(&r.id)
            .after(&audit_of(r)),
    )
    .await
}
