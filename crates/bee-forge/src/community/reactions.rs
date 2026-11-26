//! Reactions on any community subject (discussions, comments, releases, gists).
//!
//! A reaction is one row per `(subject, content, principal)`, so adding twice is a no-op and
//! counts are exact. Subjects are named by a key such as `discussion:<id>` or
//! `comment:<id>`; the service layer authorizes against the repo the subject lives in.

use bee_proto::v1::{Principal, Reaction, ReactionContent, ReactionGroup, SubjectKind};

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::module::EventKind;
use crate::store::{Db, Index, Key, Row, Scan, Tx};

use super::SURFACE;
use super::common::{draft, emit, paged, subject};

/// Reactions, scoped by subject key; `k1` principal, `k2` content number.
pub static REACTIONS: crate::store::TableDef = crate::store::TableDef {
    name: "forge_reactions",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeK2, Index::ScopeN1],
};

/// Reaction event kinds.
pub const EVENTS: &[EventKind] = &[
    EventKind::new("reaction.added", "A reaction was added"),
    EventKind::new("reaction.removed", "A reaction was removed"),
];

/// Subject key of a discussion.
pub fn discussion_key(id: &str) -> String {
    format!("discussion:{id}")
}

/// Subject key of a comment.
pub fn comment_key(id: &str) -> String {
    format!("comment:{id}")
}

fn content_of(c: i32) -> Result<ReactionContent> {
    match ReactionContent::try_from(c) {
        Ok(ReactionContent::Unspecified) | Err(_) => Err(ForgeError::invalid(
            "invalid_reaction",
            "reaction content must be one of the eight fixed reactions",
        )),
        Ok(c) => Ok(c),
    }
}

/// Counts of each reaction on `key` and whether `viewer` is among them. Runs inside the
/// caller's transaction.
pub async fn groups(tx: &mut Tx, key: &str, viewer: &str) -> Result<Vec<ReactionGroup>> {
    let rows = tx.scan(&REACTIONS, &Scan::scope(key, 10_000)).await?;
    let mut out: Vec<ReactionGroup> = Vec::new();
    for r in rows {
        let c: i32 = r.k2.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0);
        let mine = r.k1.as_deref() == Some(viewer);
        match out.iter_mut().find(|g| g.content == c) {
            Some(g) => {
                g.count += 1;
                g.viewer_reacted |= mine;
            }
            None => out.push(ReactionGroup {
                content: c,
                count: 1,
                viewer_reacted: mine,
            }),
        }
    }
    out.sort_by_key(|g| g.content);
    Ok(out)
}

/// Where a subject lives, for events and audit.
#[derive(Clone, Debug, Default)]
pub struct Where {
    /// Org id.
    pub org_id: String,
    /// Repo id (empty for org or user subjects).
    pub repo_id: String,
    /// Subject kind.
    pub kind: i32,
    /// Subject id.
    pub id: String,
}

/// Reaction operations.
pub struct Reactions {
    db: Db,
}

impl Reactions {
    /// Reactions over `db`.
    pub fn new(db: Db) -> Reactions {
        Reactions { db }
    }

    /// Adds the caller's reaction; adding it again returns the existing one.
    pub async fn add(&self, cx: &Cx, key: &str, at: &Where, content: i32) -> Result<Reaction> {
        let content = content_of(content)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let id = format!("{:02}:{}", content as i32, cx.actor_id);
        if let Some((r, _)) = tx.get_msg::<Reaction>(&REACTIONS, key, &id).await? {
            return Ok(r);
        }
        let r = Reaction {
            id: tx.new_id()?,
            content: content as i32,
            user: Some(cx.principal()),
            created_at: Some(ts(tx.now_ms())),
        };
        let row = Row::new(key, id)
            .k1(cx.actor_id.clone())
            .k2((content as i32).to_string())
            .n1(tx.now_ms() as i64)
            .msg(&r);
        tx.insert(&REACTIONS, &row).await?;
        let kind = SubjectKind::try_from(at.kind).unwrap_or(SubjectKind::Unspecified);
        let payload = serde_json::json!({
            "subject_key": key,
            "content": content.as_str_name(),
            "reaction_id": r.id,
        });
        emit(
            &mut tx,
            "reaction.added",
            &at.org_id,
            &at.repo_id,
            subject(kind, &at.repo_id, &at.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "reaction.add",
            &at.org_id,
            &at.repo_id,
            kind,
            &at.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(r)
    }

    /// Removes the caller's reaction (absent is a no-op).
    pub async fn remove(&self, cx: &Cx, key: &str, at: &Where, content: i32) -> Result<()> {
        let content = content_of(content)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let id = format!("{:02}:{}", content as i32, cx.actor_id);
        if !tx.delete(&REACTIONS, key, &id).await? {
            return Ok(());
        }
        let kind = SubjectKind::try_from(at.kind).unwrap_or(SubjectKind::Unspecified);
        let payload = serde_json::json!({
            "subject_key": key,
            "content": content.as_str_name(),
        });
        emit(
            &mut tx,
            "reaction.removed",
            &at.org_id,
            &at.repo_id,
            subject(kind, &at.repo_id, &at.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "reaction.remove",
            &at.org_id,
            &at.repo_id,
            kind,
            &at.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Reactions on a subject (optionally one content), oldest first, with the groups.
    pub async fn list(
        &self,
        cx: &Cx,
        key: &str,
        content: i32,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<Reaction>, Vec<ReactionGroup>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut scan = Scan::scope(key, limit).by_n1();
        if content != 0 {
            scan = scan.key(Key::K2(content.to_string()));
        }
        let (rows, next) = paged(&mut tx, &REACTIONS, scan, token, limit).await?;
        let groups = groups(&mut tx, key, &cx.actor_id).await?;
        let reactions = rows
            .iter()
            .map(|r| r.decode::<Reaction>())
            .collect::<Result<Vec<_>>>()?;
        Ok((reactions, groups, next))
    }

    /// Principals who reacted (for tests and moderation tools).
    pub async fn reactors(&self, cx: &Cx, key: &str) -> Result<Vec<Principal>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.scan_msgs::<Reaction>(&REACTIONS, &Scan::scope(key, 10_000))
            .await
            .map(|v| v.into_iter().filter_map(|(r, _)| r.user).collect())
    }
}
