//! Replica reads with primary fallback (SPEC §6.3).
//!
//! Immutable rows may be read from a replica, but a miss there may only mean the replica is
//! behind, so it MUST be retried on the primary before the caller answers `NOT_FOUND`. These
//! helpers are the one place that rule is written down; every read path uses them.

use bee_core::{BeeError, ObjectId, Reason, Result};

use crate::meta::MetaStore;
use crate::types::{ChangesetRow, MinVersion, ObjectRow, OrgId, ReadFrom, RepoId};

/// Reads a changeset from a replica (honouring `min`), retrying a miss on the primary.
/// `NOT_FOUND` only when the primary has no row either.
pub async fn changeset(
    meta: &dyn MetaStore,
    repo: RepoId,
    id: &ObjectId,
    min: Option<MinVersion>,
) -> Result<ChangesetRow> {
    if let Some(row) = meta.get_changeset(repo, id, ReadFrom::Replica(min)).await? {
        return Ok(row);
    }
    meta.get_changeset(repo, id, ReadFrom::Primary)
        .await?
        .ok_or_else(|| {
            BeeError::new(Reason::NotFound, format!("changeset {id} not found"))
                .with_payload("object", id.to_text())
        })
}

/// Reads an object row from a replica (honouring `min`), retrying a miss on the primary.
/// `MISSING_OBJECT` only when the primary has no row either.
pub async fn object(
    meta: &dyn MetaStore,
    org: OrgId,
    id: &ObjectId,
    min: Option<MinVersion>,
) -> Result<ObjectRow> {
    if let Some(row) = meta.get_object(org, id, ReadFrom::Replica(min)).await? {
        return Ok(row);
    }
    meta.get_object(org, id, ReadFrom::Primary)
        .await?
        .ok_or_else(|| {
            BeeError::new(Reason::MissingObject, format!("object {id} has no row"))
                .with_payload("object", id.to_text())
        })
}

/// Reads a derived value from a replica, retrying a miss on the primary; `None` only when
/// the primary has none either.
pub async fn derived(
    meta: &dyn MetaStore,
    repo: RepoId,
    key: &ObjectId,
    type_name: &str,
    version: u32,
    min: Option<MinVersion>,
) -> Result<Option<Vec<u8>>> {
    if let Some(v) = meta
        .get_derived(repo, key, type_name, version, ReadFrom::Replica(min))
        .await?
    {
        return Ok(Some(v));
    }
    meta.get_derived(repo, key, type_name, version, ReadFrom::Primary)
        .await
}
