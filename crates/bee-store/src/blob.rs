//! The blob store trait (SPEC §6.1, §6.5): immutable, content-addressed bytes, keyed by org.

use std::sync::Arc;

use async_trait::async_trait;
use bee_core::{ObjectId, ObjectKind, Result, hex};

use crate::counters::AccessCounters;
use crate::types::{OrgId, StorageClass};

/// What the blob store knows about stored bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobInfo {
    /// Byte length.
    pub len: u64,
    /// Storage class.
    pub class: StorageClass,
    /// True when the bytes sit under the quarantine prefix and are not served.
    pub quarantined: bool,
    /// Last time the bytes were written (a re-put refreshes it), from the store's clock.
    pub written_at_ms: u64,
}

/// The object-storage key for bytes (amended SPEC §6.1):
/// `o/<ORG ULID>/<kind tag>/<first two hex digits>/<64 hex digits>`, and the same under
/// `q/` for quarantined bytes. The two-digit fan-out spreads keys across storage
/// partitions; the kind tag keeps listings by kind cheap.
pub fn blob_key(org: OrgId, id: &ObjectId, quarantined: bool) -> String {
    let digest = hex::encode(id.digest());
    format!(
        "{}/{}/{}/{}/{}",
        if quarantined { "q" } else { "o" },
        org.0,
        id.kind().text_tag(),
        &digest[..2],
        digest
    )
}

/// True for kinds whose bytes may live in the blob store: blobs, and trees and metas too
/// large to inline. Changesets live only in their metadata row; foreign ids are never stored.
pub fn blob_storable(kind: ObjectKind) -> bool {
    matches!(kind, ObjectKind::Blob | ObjectKind::Tree | ObjectKind::Meta)
}

/// The blob store.
///
/// # Contract
///
/// - `put` returns only once the bytes are durable; a repeated `put` of the same id is
///   harmless and refreshes `written_at_ms` (GC relies on this, see [`crate::gc`]).
/// - Bytes are immutable: implementations never change what an id maps to. Callers verify
///   bytes against the id before `put`; readers verify after `get`.
/// - `get` never serves quarantined bytes. Moving between storage classes never loses the
///   bytes: the hot copy is removed only after the cold copy is durable and verified, and
///   reads are served transparently from either class.
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Stores bytes under `id` in `class`.
    async fn put(
        &self,
        org: OrgId,
        id: &ObjectId,
        bytes: Vec<u8>,
        class: StorageClass,
    ) -> Result<()>;

    /// Reads all bytes; `None` when absent or quarantined.
    async fn get(&self, org: OrgId, id: &ObjectId) -> Result<Option<Vec<u8>>>;

    /// Reads `len` bytes from `offset` (clamped to the end); `None` when absent or
    /// quarantined. Callers verify ranges against chunk ids where the format provides them.
    async fn get_range(
        &self,
        org: OrgId,
        id: &ObjectId,
        offset: u64,
        len: u64,
    ) -> Result<Option<Vec<u8>>>;

    /// True when servable (not quarantined) bytes exist, per id.
    async fn has(&self, org: OrgId, ids: &[ObjectId]) -> Result<Vec<bool>>;

    /// Metadata of stored bytes, quarantined included.
    async fn stat(&self, org: OrgId, id: &ObjectId) -> Result<Option<BlobInfo>>;

    /// Deletes bytes (servable or quarantined). With `written_before_ms`, deletes only if the
    /// bytes were last written strictly before that time, so a concurrent re-upload survives
    /// GC. Returns whether anything was deleted.
    async fn delete(
        &self,
        org: OrgId,
        id: &ObjectId,
        written_before_ms: Option<u64>,
    ) -> Result<bool>;

    /// Moves bytes to another storage class (SPEC §6.5). Returns false when absent.
    async fn set_class(&self, org: OrgId, id: &ObjectId, class: StorageClass) -> Result<bool>;

    /// Moves bytes under the quarantine prefix, where `get` no longer serves them. Returns
    /// false when absent.
    async fn quarantine(&self, org: OrgId, id: &ObjectId) -> Result<bool>;

    /// Moves quarantined bytes back. Returns false when not quarantined.
    async fn release(&self, org: OrgId, id: &ObjectId) -> Result<bool>;

    /// Stored ids of an org (quarantined included) ordered by id, after `after`, at most
    /// `limit`.
    async fn list(
        &self,
        org: OrgId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, BlobInfo)>>;

    /// Storage access counters, when the implementation keeps them.
    fn access_counters(&self) -> Option<Arc<AccessCounters>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::Ulid;

    #[test]
    fn key_layout() {
        let org = OrgId(Ulid::from_u128(1));
        let id = bee_object::blob_id(b"hello");
        let key = blob_key(org, &id, false);
        let hex = hex::encode(id.digest());
        assert_eq!(
            key,
            format!("o/00000000000000000000000001/b1/{}/{}", &hex[..2], hex)
        );
        assert_eq!(blob_key(org, &id, true), format!("q{}", &key[1..]));
        let tree = bee_object::Tree::empty().id();
        assert!(blob_key(org, &tree, false).contains("/t1/"));
        assert!(blob_storable(ObjectKind::Blob));
        assert!(blob_storable(ObjectKind::Tree));
        assert!(!blob_storable(ObjectKind::Changeset));
        assert!(!blob_storable(ObjectKind::Foreign));
    }
}
