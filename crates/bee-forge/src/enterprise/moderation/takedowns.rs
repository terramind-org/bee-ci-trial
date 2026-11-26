//! Takedowns: legal and abuse notices that disable a repo or a comment.
//!
//! A takedown is recorded with its notice, disables the subject at once and is reversible.
//! The disabled state is one marker row per subject, so the write path asks with one point
//! read ([`Moderation::is_taken_down`]); the server answers reads of a taken-down subject
//! with `NOT_FOUND` and writes with `PERMISSION_DENIED` / `taken_down`
//! ([`Moderation::check_write`](super::Moderation::check_write) does the latter). Blob content
//! is removed through `AdminService.Redact`, not here. Creating and reinstating both record an
//! audit entry; the takedown keeps the id of the creating entry.

use bee_proto::v1::{SubjectKind, SubjectRef, Takedown, TakedownKind, TakedownState};
use serde_json::json;

use super::tables::{MARKS, TAKEDOWNS};
use super::{Change, Moderation, SURFACE, filtered_page, record, subject_in};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Key, Row, Tx};

const SCOPE: &str = "all";

/// The notice of a takedown to create.
#[derive(Clone, Debug)]
pub struct NewTakedown {
    /// Basis of the takedown.
    pub kind: TakedownKind,
    /// The notice text.
    pub notice_text: String,
    /// Public copy of the notice, if published.
    pub notice_url: String,
    /// Who complained.
    pub complainant: String,
}

/// The marker key of a subject: `repo:<id>` or `comment:<id>`.
pub(crate) fn mark_key(kind: i32, repo_id: &str, id: &str) -> Option<String> {
    match SubjectKind::try_from(kind) {
        Ok(SubjectKind::Repo) => Some(format!("repo:{repo_id}")),
        Ok(SubjectKind::Comment) => Some(format!("comment:{id}")),
        _ => None,
    }
}

/// Whether a marker exists (one point read).
pub(crate) async fn marked(tx: &mut Tx, key: &str) -> Result<bool> {
    Ok(tx.get(&MARKS, "mark", key).await?.is_some())
}

impl Moderation {
    /// Records a takedown of a repo or comment. `repo_id` and `org_id` are those of the
    /// subject's repo. `ALREADY_EXISTS` when the subject is already taken down.
    pub async fn create_takedown(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        subject: &SubjectRef,
        notice: NewTakedown,
    ) -> Result<Takedown> {
        if notice.kind == TakedownKind::Unspecified {
            return Err(ForgeError::missing("kind"));
        }
        let id = if subject.kind == SubjectKind::Repo as i32 && subject.id.is_empty() {
            repo_id.to_string()
        } else {
            subject.id.clone()
        };
        let Some(key) = mark_key(subject.kind, repo_id, &id) else {
            return Err(ForgeError::invalid(
                "bad_subject",
                "only repos and comments can be taken down",
            ));
        };
        if id.is_empty() {
            return Err(ForgeError::missing("subject.id"));
        }
        let subject = subject_in(
            repo_id,
            SubjectKind::try_from(subject.kind).unwrap_or(SubjectKind::Unspecified),
            &id,
        );
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        if marked(&mut tx, &key).await? {
            return Err(ForgeError::already_exists(format!("takedown of {key}")));
        }
        let takedown_id = tx.new_id()?;
        let mut takedown = Takedown {
            id: takedown_id.clone(),
            kind: notice.kind as i32,
            state: TakedownState::Active as i32,
            subject: Some(subject.clone()),
            notice_url: notice.notice_url,
            notice_text: notice.notice_text,
            complainant: notice.complainant,
            created_by: Some(cx.principal()),
            reinstate_reason: String::new(),
            reinstated_by: None,
            audit_entry_id: String::new(),
            created_at: Some(ts(now)),
            reinstated_at: None,
        };
        let audit_id = record(
            &mut tx,
            Change {
                kind: "moderation.takedown_created",
                action: "moderation.takedown_create",
                org: org_id.to_string(),
                repo: repo_id.to_string(),
                subject: Some(subject),
                before: None,
                after: json!({
                    "takedown": takedown_id,
                    "subject": key,
                    "kind": notice.kind.as_str_name(),
                    "complainant": takedown.complainant,
                }),
            },
        )
        .await?;
        takedown.audit_entry_id = audit_id;
        tx.insert(
            &TAKEDOWNS,
            &Row::new(SCOPE, takedown_id.clone())
                .k1(key.clone())
                .k2(format!("repo:{repo_id}"))
                .msg(&takedown),
        )
        .await?;
        tx.insert(&MARKS, &Row::new("mark", key).k1(takedown_id))
            .await?;
        tx.commit().await?;
        Ok(takedown)
    }

    /// Reverses an active takedown. `INVALID_STATE` unless it is active.
    pub async fn reinstate_takedown(
        &self,
        cx: &Cx,
        org_id: &str,
        id: &str,
        reason: &str,
    ) -> Result<Takedown> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some((mut t, row)) = tx.get_msg::<Takedown>(&TAKEDOWNS, SCOPE, id).await? else {
            return Err(ForgeError::not_found(format!("takedown {id}")));
        };
        if t.state != TakedownState::Active as i32 {
            return Err(ForgeError::invalid_state("takedown is not active"));
        }
        t.state = TakedownState::Reinstated as i32;
        t.reinstate_reason = reason.to_string();
        t.reinstated_by = Some(cx.principal());
        t.reinstated_at = Some(ts(now));
        let key = row.k1.clone().unwrap_or_default();
        let repo_id = row
            .k2
            .as_deref()
            .and_then(|k| k.strip_prefix("repo:"))
            .unwrap_or("")
            .to_string();
        let next = Row {
            body: prost::Message::encode_to_vec(&t),
            ..row
        };
        tx.update(&TAKEDOWNS, &next).await?;
        tx.delete(&MARKS, "mark", &key).await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.takedown_reinstated",
                action: "moderation.takedown_reinstate",
                org: org_id.to_string(),
                repo: repo_id,
                subject: t.subject.clone(),
                before: Some(json!({ "state": "active" })),
                after: json!({ "takedown": id, "subject": key, "reason": reason }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(t)
    }

    /// One takedown by id.
    pub async fn takedown(&self, cx: &Cx, id: &str) -> Result<Takedown> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<Takedown>(&TAKEDOWNS, SCOPE, id)
            .await?
            .map(|(t, _)| t)
            .ok_or_else(|| ForgeError::not_found(format!("takedown {id}")))
    }

    /// Takedowns, optionally of one repo (its own and its comments'), filtered by state and
    /// kind (unspecified = any), paged by id.
    pub async fn list_takedowns(
        &self,
        cx: &Cx,
        repo_id: Option<&str>,
        state: TakedownState,
        kind: TakedownKind,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Takedown>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        filtered_page::<Takedown>(
            &mut tx,
            &TAKEDOWNS,
            SCOPE,
            repo_id.map(|r| Key::K2(format!("repo:{r}"))),
            after,
            limit,
            |t, _| {
                (state == TakedownState::Unspecified || t.state == state as i32)
                    && (kind == TakedownKind::Unspecified || t.kind == kind as i32)
            },
        )
        .await
    }

    /// Whether the subject (a repo or a comment) is taken down: one or two point reads. A
    /// repo named by full name is resolved through the orgs module.
    pub async fn is_taken_down(&self, subject: &SubjectRef) -> Result<bool> {
        let cx = Cx::system();
        let repo_id = if subject.repo.is_none() && subject.kind == SubjectKind::Repo as i32 {
            Ok(Some(subject.id.clone()))
        } else {
            self.repo_id_of(&cx, subject).await
        }
        .or_else(|e| {
            // A repo that no longer resolves has nothing to take down.
            if e.reason == bee_proto::v1::ErrorReason::NotFound {
                Ok(None)
            } else {
                Err(e)
            }
        })?;
        let mut tx = self.db.begin(SURFACE, &cx).await?;
        if let Some(r) = &repo_id
            && marked(&mut tx, &format!("repo:{r}")).await?
        {
            return Ok(true);
        }
        if subject.kind == SubjectKind::Comment as i32
            && marked(&mut tx, &format!("comment:{}", subject.id)).await?
        {
            return Ok(true);
        }
        Ok(false)
    }

    /// Whether a repo is taken down (one point read).
    pub async fn is_repo_taken_down(&self, repo_id: &str) -> Result<bool> {
        let mut tx = self.db.begin(SURFACE, &Cx::system()).await?;
        marked(&mut tx, &format!("repo:{repo_id}")).await
    }
}
