//! The redaction lifecycle (SPEC §13, PRODUCT §4.10): live → quarantined → purged.
//!
//! - **Redact** moves an object's bytes under the quarantine prefix and marks its row
//!   quarantined; reads answer `REDACTED`; the repo's redaction epoch increases. The object
//!   stays recoverable until its purge time (`redaction.quarantine_days`).
//! - **Unredact** (quarantine only) releases the bytes and marks the row live; the epoch
//!   increases again so caches re-fetch.
//! - **Purge** (scheduled or `PurgeNow`) runs the store's redaction: permanent blocklist,
//!   purged row, bytes deleted, epoch bumped by the store.
//!
//! The lifecycle rows live in the server store (`redaction/<org>/<object>`); the purged state
//! and the blocklist are the metadata store's (`MetaStore::redact`).

use bee_core::{ObjectId, ObjectKind};
use bee_store::{ObjectState, OrgId, RedactionRow};
use serde::{Deserialize, Serialize};

use crate::errors::{ApiError, ApiResult};
use crate::kv::Kv;
use crate::state::{AppState, RepoCtx};
use bee_proto::v1::ErrorReason;

/// Namespace of lifecycle rows.
pub const NS_REDACTION: &str = "redaction";

/// Lifecycle states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Bytes moved aside; unredact possible.
    Quarantined,
    /// Bytes deleted for good.
    Purged,
    /// Restored from quarantine.
    Unredacted,
}

/// One lifecycle row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    /// The object (text id).
    pub object: String,
    /// The repo it was redacted through.
    pub repo: String,
    /// State.
    pub state: State,
    /// Why.
    pub reason: String,
    /// Who.
    pub principal: String,
    /// When.
    pub at_ms: u64,
    /// When the purge is due (quarantined only).
    pub purge_at_ms: u64,
    /// The repo's redaction epoch after this change.
    pub epoch: u64,
}

fn key(org: OrgId, id: &ObjectId) -> Vec<u8> {
    format!("{}/{}", org.0.to_text(), id.to_text()).into_bytes()
}

/// The lifecycle row of an object.
pub async fn record(kv: &dyn Kv, org: OrgId, id: &ObjectId) -> ApiResult<Option<Record>> {
    Ok(kv
        .get(NS_REDACTION, &key(org, id))
        .await?
        .and_then(|b| serde_json::from_slice(&b).ok()))
}

/// True when reads of the object must answer `REDACTED`.
pub async fn is_redacted(state: &AppState, org: OrgId, id: &ObjectId) -> ApiResult<bool> {
    if let Some(r) = record(state.kv.as_ref(), org, id).await? {
        return Ok(matches!(r.state, State::Quarantined | State::Purged));
    }
    Ok(!state
        .meta
        .get_redactions(org, std::slice::from_ref(id))
        .await?
        .is_empty())
}

/// The objects of `ids` whose reads must answer `REDACTED` ([`is_redacted`] for many ids, in
/// a few queries).
pub async fn redacted_among(
    state: &AppState,
    org: OrgId,
    ids: &[ObjectId],
) -> ApiResult<std::collections::BTreeSet<ObjectId>> {
    let wanted: std::collections::HashSet<ObjectId> = ids.iter().copied().collect();
    let records: std::collections::HashMap<String, State> = list(state.kv.as_ref(), org)
        .await?
        .into_iter()
        .map(|r| (r.object, r.state))
        .collect();
    let mut out = std::collections::BTreeSet::new();
    for chunk in ids.chunks(1000) {
        for row in state.meta.get_redactions(org, chunk).await? {
            out.insert(row.object);
        }
    }
    out.retain(|id| !matches!(records.get(&id.to_text()), Some(State::Unredacted)));
    for (object, st) in &records {
        if matches!(st, State::Quarantined | State::Purged)
            && let Ok(id) = ObjectId::parse(object)
            && wanted.contains(&id)
        {
            out.insert(id);
        }
    }
    Ok(out)
}

async fn bump_epoch(state: &AppState, repo: &RepoCtx) -> ApiResult<u64> {
    crate::auth::bump_counter(
        state.kv.as_ref(),
        &format!("redaction/{}", repo.id_text()),
        1,
    )
    .await?;
    state.redaction_epoch(repo).await
}

fn check_kind(id: &ObjectId) -> ApiResult<()> {
    match id.kind() {
        ObjectKind::Blob | ObjectKind::Meta => Ok(()),
        k => Err(ApiError::new(
            ErrorReason::NotRedactable,
            format!("{} objects cannot be redacted", k.text_tag()),
        )),
    }
}

/// Redacts into quarantine. Returns the new epoch.
pub async fn quarantine(
    state: &AppState,
    repo: &RepoCtx,
    id: &ObjectId,
    principal: &str,
    reason: &str,
    purge_after_ms: u64,
) -> ApiResult<u64> {
    check_kind(id)?;
    let org = repo.org.id;
    if state
        .meta
        .get_object(org, id, bee_store::ReadFrom::Primary)
        .await?
        .is_none()
    {
        return Err(ApiError::not_found(format!("object {id} not found")));
    }
    if let Some(r) = record(state.kv.as_ref(), org, id).await?
        && matches!(r.state, State::Quarantined | State::Purged)
    {
        return state.redaction_epoch(repo).await;
    }
    state.blobs.quarantine(org, id).await?;
    state
        .meta
        .set_object_state(org, id, ObjectState::Quarantined)
        .await?;
    let epoch = bump_epoch(state, repo).await?;
    let now = state.now_ms();
    let rec = Record {
        object: id.to_text(),
        repo: repo.id_text(),
        state: State::Quarantined,
        reason: reason.to_string(),
        principal: principal.to_string(),
        at_ms: now,
        purge_at_ms: now + purge_after_ms,
        epoch,
    };
    state
        .kv
        .put(
            NS_REDACTION,
            &key(org, id),
            serde_json::to_vec(&rec).unwrap_or_default(),
        )
        .await?;
    Ok(epoch)
}

/// Restores an object from quarantine (`NOT_QUARANTINED` otherwise).
pub async fn unredact(state: &AppState, repo: &RepoCtx, id: &ObjectId) -> ApiResult<u64> {
    let org = repo.org.id;
    let Some(mut rec) = record(state.kv.as_ref(), org, id).await? else {
        return Err(ApiError::new(
            ErrorReason::NotQuarantined,
            format!("{id} is not quarantined"),
        ));
    };
    if rec.state != State::Quarantined {
        return Err(ApiError::new(
            ErrorReason::NotQuarantined,
            format!("{id} is not quarantined"),
        ));
    }
    state.blobs.release(org, id).await?;
    state
        .meta
        .set_object_state(org, id, ObjectState::Live)
        .await?;
    let epoch = bump_epoch(state, repo).await?;
    rec.state = State::Unredacted;
    rec.epoch = epoch;
    rec.at_ms = state.now_ms();
    state
        .kv
        .put(
            NS_REDACTION,
            &key(org, id),
            serde_json::to_vec(&rec).unwrap_or_default(),
        )
        .await?;
    Ok(epoch)
}

/// Deletes the rows of every content-bearing derived type in `repos`.
async fn purge_content_bearing(state: &AppState, repos: &[bee_store::RepoId]) -> ApiResult<()> {
    let types: Vec<&'static str> = state
        .registry
        .types()
        .filter(|t| t.content_bearing())
        .map(|t| t.name())
        .collect();
    for repo in repos {
        for t in &types {
            state.meta.delete_derived_type(*repo, t).await?;
        }
    }
    Ok(())
}

/// Purges an object for good (from quarantine, or directly).
pub async fn purge(
    state: &AppState,
    repo: &RepoCtx,
    id: &ObjectId,
    principal: &str,
    reason: &str,
) -> ApiResult<u64> {
    check_kind(id)?;
    let org = repo.org.id;
    let prior = record(state.kv.as_ref(), org, id).await?;
    if prior.is_none()
        && state
            .meta
            .get_object(org, id, bee_store::ReadFrom::Primary)
            .await?
            .is_none()
    {
        return Err(ApiError::not_found(format!("object {id} not found")));
    }
    // Bytes may sit under the quarantine prefix: release first so the store's redaction
    // deletes them (it deletes servable bytes).
    if prior
        .as_ref()
        .is_some_and(|r| r.state == State::Quarantined)
    {
        state.blobs.release(org, id).await?;
        state
            .meta
            .set_object_state(org, id, ObjectState::Live)
            .await?;
    }
    let now = state.now_ms();
    let out = state
        .writer
        .redact(RedactionRow {
            org,
            object: *id,
            principal: principal.to_string(),
            reason: reason.to_string(),
            at_ms: now,
        })
        .await?;
    // Delete any quarantined copy left behind.
    let _ = state.blobs.delete(org, id, None).await;
    // Content-bearing derived records (SPEC §13) of every repo that referenced the object go
    // with it; they derive again, without the purged bytes, when next read.
    let mut repos: Vec<bee_store::RepoId> = out.epochs.iter().map(|(r, _)| *r).collect();
    repos.push(repo.repo.id);
    repos.sort();
    repos.dedup();
    purge_content_bearing(state, &repos).await?;
    let epoch = state.redaction_epoch(repo).await?;
    let rec = Record {
        object: id.to_text(),
        repo: repo.id_text(),
        state: State::Purged,
        reason: reason.to_string(),
        principal: principal.to_string(),
        at_ms: now,
        purge_at_ms: now,
        epoch,
    };
    state
        .kv
        .put(
            NS_REDACTION,
            &key(org, id),
            serde_json::to_vec(&rec).unwrap_or_default(),
        )
        .await?;
    Ok(epoch)
}

/// Every lifecycle row of an org.
pub async fn list(kv: &dyn Kv, org: OrgId) -> ApiResult<Vec<Record>> {
    let prefix = format!("{}/", org.0.to_text()).into_bytes();
    let mut out = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = kv
            .scan(NS_REDACTION, &prefix, after.as_deref(), 500)
            .await?;
        let Some(last) = page.last() else { break };
        after = Some(last.0.clone());
        out.extend(
            page.into_iter()
                .filter_map(|(_, v)| serde_json::from_slice(&v).ok()),
        );
    }
    Ok(out)
}

/// Purges every quarantined object whose purge time has passed (the scheduled purge).
pub async fn purge_due(state: &AppState) -> ApiResult<usize> {
    let mut n = 0;
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = state
            .kv
            .scan(NS_REDACTION, b"", after.as_deref(), 500)
            .await?;
        let Some(last) = page.last() else { break };
        after = Some(last.0.clone());
        for (_, v) in page {
            let Ok(rec) = serde_json::from_slice::<Record>(&v) else {
                continue;
            };
            if rec.state != State::Quarantined || rec.purge_at_ms > state.now_ms() {
                continue;
            }
            let Ok(id) = ObjectId::parse(&rec.object) else {
                continue;
            };
            let Ok(ulid) = bee_core::Ulid::parse(&rec.repo) else {
                continue;
            };
            let Some(repo) = state.meta.get_repo(bee_store::RepoId(ulid)).await? else {
                continue;
            };
            let Some(org) = state.meta.get_org(repo.org).await? else {
                continue;
            };
            let ctx = RepoCtx {
                full_name: format!("{}/{}", org.name, repo.name),
                org,
                repo,
            };
            purge(state, &ctx, &id, &rec.principal, &rec.reason).await?;
            n += 1;
        }
    }
    Ok(n)
}
