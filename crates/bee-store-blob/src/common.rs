//! Helpers shared by the disk and S3 stores.

use bee_core::{BeeError, ObjectId, ObjectKind, Reason, Result, hex};
use bee_object::hash_id;

/// Checks that `bytes` hash to `id`; the verify-on-read rule of SPEC §6.1. A mismatch is
/// `INTEGRITY`, whatever the cause (bit rot, truncation, a wrong object under the key).
pub(crate) fn verify_bytes(id: &ObjectId, bytes: &[u8]) -> Result<()> {
    if hash_id(id.kind(), bytes) == *id {
        Ok(())
    } else {
        Err(BeeError::new(
            Reason::Integrity,
            format!("stored bytes do not hash to {id}"),
        ))
    }
}

/// The `INTEGRITY` error for storage that cannot be decoded at all.
pub(crate) fn corrupt(id: &ObjectId, what: &str) -> BeeError {
    BeeError::new(
        Reason::Integrity,
        format!("stored bytes of {id} are corrupt: {what}"),
    )
}

/// The kinds the blob store holds, in [`ObjectId`] order (the order `list` returns).
pub(crate) fn listing_kinds() -> [ObjectKind; 3] {
    [ObjectKind::Blob, ObjectKind::Tree, ObjectKind::Meta]
}

/// The id of `kind` whose digest is `digest_hex`, for a name found in a listing; `None` for
/// anything that is not 64 lowercase hex digits.
pub(crate) fn id_from_digest_hex(kind: ObjectKind, digest_hex: &str) -> Option<ObjectId> {
    if digest_hex.len() != 64 || !hex::is_lower_hex(digest_hex) {
        return None;
    }
    let bytes = hex::decode(digest_hex)?;
    let digest: [u8; 32] = bytes.try_into().ok()?;
    Some(ObjectId::new(kind, digest))
}

/// Clamps `offset..offset + len` to a total of `total` bytes.
pub(crate) fn clamp_range(total: u64, offset: u64, len: u64) -> (u64, u64) {
    let start = offset.min(total);
    let end = offset.saturating_add(len).min(total);
    (start, end)
}

/// A retryable `UNAVAILABLE` error.
pub(crate) fn unavailable(message: impl Into<String>) -> BeeError {
    BeeError::new(Reason::Unavailable, message)
}

/// An `INTERNAL` error for a storage failure that retrying cannot fix.
pub(crate) fn internal(message: impl Into<String>) -> BeeError {
    BeeError::new(Reason::Internal, message)
}

/// Maps an I/O error to a store error (`UNAVAILABLE`: disks fill up and recover).
pub(crate) fn io_error(context: &str, err: std::io::Error) -> BeeError {
    unavailable(format!("{context}: {err}"))
}
