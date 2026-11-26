use bee_proto::v1::*;
use prost::Message as _;
use serde_json::Value;

use super::{ATTENTION, RECORDS};
use crate::agents::{Agents, SURFACE};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::store::{Cursor, Key, Row, Scan, Tx};

const MAX_RECORD_BYTES: usize = 384 * 1024;
pub(super) const MAX_PAGE_BYTES: usize = 3 * 1024 * 1024;

pub(super) fn key(work: &SafetyRef) -> String {
    format!("{}/{}", work.kind, work.id)
}

pub(super) async fn lock(
    tx: &mut Tx,
    repo: &Repo,
    work: &SafetyRef,
) -> Result<(SafetyRecord, Row)> {
    let row = tx
        .get_for_update(&RECORDS, &repo.id, &key(work))
        .await?
        .ok_or_else(|| ForgeError::not_found("Safety record").with_subcode("safety_record"))?;
    Ok((row.decode()?, row))
}

pub(super) async fn ensure(agents: &Agents, cx: &Cx, repo: &Repo, work: &SafetyRef) -> Result<()> {
    let id = key(work);
    let mut tx = agents.db.begin(SURFACE, cx).await?;
    if tx.get(&RECORDS, &repo.id, &id).await?.is_some() {
        return Ok(());
    }
    // Competing creators insert the same key. A duplicate ends this transaction on
    // Postgres, so the caller always opens a fresh transaction to lock the row.
    match tx.insert(&RECORDS, &Row::new(&repo.id, &id)).await {
        Ok(()) => tx.commit().await,
        Err(e) if e.reason == ErrorReason::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

pub(super) async fn save(tx: &mut Tx, row: Row, record: &SafetyRecord) -> Result<()> {
    let bytes = record.encoded_len();
    if bytes > MAX_RECORD_BYTES {
        return Err(ForgeError::too_large(
            "safety record",
            MAX_RECORD_BYTES as u64,
            bytes as u64,
        ));
    }
    let time = record.updated_at.as_ref().map_or(0, ts_ms);
    tx.update(&RECORDS, &row.n1(time as i64).msg(record))
        .await?;
    Ok(())
}

pub(super) async fn add_attention(tx: &mut Tx, repo: &Repo, item: &SafetyAttention) -> Result<()> {
    let answerable = item
        .answerable
        .as_ref()
        .filter(|p| p.kind() == PrincipalKind::Human && !p.id.is_empty())
        .ok_or_else(|| {
            ForgeError::invalid("safety_recipient", "Work needs an answerable person")
        })?;
    let created = item.created_at.as_ref().map_or(0, ts_ms);
    tx.insert(
        &ATTENTION,
        &Row::new(&repo.id, &item.id)
            .k1(&answerable.id)
            .n1(created as i64)
            .msg(item),
    )
    .await
}

// Resume uses this read while holding the swarm lock that also guards a tripwire pause.
pub(crate) async fn unacknowledged_swarm_pause(
    tx: &mut Tx,
    repo: &str,
    swarm: &str,
    owner: &str,
) -> Result<Option<String>> {
    const PAGE_SIZE: usize = 128;
    let mut cursor = None;
    loop {
        let rows = tx
            .scan(
                &ATTENTION,
                &Scan::scope(repo, PAGE_SIZE)
                    .key(Key::K1(owner.into()))
                    .after(cursor),
            )
            .await?;
        for row in &rows {
            let item: SafetyAttention = row.decode()?;
            if item.acknowledged_at.is_none()
                && item
                    .tripwire
                    .as_ref()
                    .is_some_and(|tripwire| tripwire.paused_swarm)
                && item
                    .work
                    .as_ref()
                    .is_some_and(|work| work.kind() == SafetyWorkKind::Swarm && work.id == swarm)
            {
                return Ok(Some(item.id));
            }
        }
        if rows.len() < PAGE_SIZE {
            return Ok(None);
        }
        cursor = rows.last().map(Cursor::after);
    }
}

pub(super) async fn emit(
    tx: &mut Tx,
    repo: &Repo,
    work: &SafetyRef,
    action: &str,
    data: Value,
) -> Result<()> {
    let subject = SubjectRef {
        kind: SubjectKind::SafetyRecord as i32,
        repo: work.repo.clone(),
        id: work.id.clone(),
    };
    tx.emit_event(Event {
        kind: action.into(),
        repo_id: repo.id.clone(),
        org_id: repo.org_id.clone(),
        subject: Some(subject),
        payload_json: data.to_string(),
        ..Default::default()
    })
    .await?;
    tx.audit(
        AuditDraft::new(action, &repo.org_id)
            .repo_id(&repo.id)
            .target(SubjectKind::SafetyRecord, &work.id)
            .after(&data.to_string()),
    )
    .await
}
