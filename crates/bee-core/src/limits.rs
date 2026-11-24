//! Limits (SPEC §4.5, §4.6, §5). Decoders and validators take a [`Limits`] so repos can
//! raise the blob size; everything else is fixed at the defaults in v1.

use crate::error::{BeeError, Result};

/// One mebibyte.
pub const MIB: u64 = 1 << 20;
/// One gibibyte.
pub const GIB: u64 = 1 << 30;

/// Size and count limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Limits {
    /// Max blob size in bytes; repo-configurable up to [`Limits::BLOB_SIZE_HARD_CAP`].
    pub blob_size: u64,
    /// Max entries in one tree.
    pub tree_entries: u64,
    /// Max delta entries in one changeset.
    pub delta_entries: u64,
    /// Max parents of one changeset.
    pub parents: u64,
    /// Max message size in bytes.
    pub message_bytes: u64,
    /// Max encoded size of a tree, meta or changeset.
    pub encoded_object_bytes: u64,
    /// Max path length in bytes.
    pub path_bytes: u64,
    /// Max path depth in components.
    pub path_depth: u64,
    /// Max bytes in one path component (tree entry name).
    pub component_bytes: u64,
    /// Max ref name length in bytes.
    pub ref_name_bytes: u64,
}

impl Limits {
    /// Absolute ceiling for `blob_size`.
    pub const BLOB_SIZE_HARD_CAP: u64 = 4 * GIB;

    /// The SPEC §4.6 defaults.
    pub const DEFAULT: Limits = Limits {
        blob_size: 100 * MIB,
        tree_entries: 1_000_000,
        delta_entries: 1_000_000,
        parents: 16,
        message_bytes: MIB,
        encoded_object_bytes: 256 * MIB,
        path_bytes: 4096,
        path_depth: 256,
        component_bytes: 255,
        ref_name_bytes: 255,
    };

    /// The defaults with a repo-specific blob size, which must not exceed the hard cap.
    pub fn with_blob_size(blob_size: u64) -> Result<Limits> {
        if blob_size > Self::BLOB_SIZE_HARD_CAP {
            return Err(BeeError::too_large(
                "blob_size",
                Self::BLOB_SIZE_HARD_CAP,
                blob_size,
            ));
        }
        Ok(Limits {
            blob_size,
            ..Self::DEFAULT
        })
    }

    /// Fails with `TOO_LARGE` when `actual` exceeds `max`.
    pub fn check(limit: &str, max: u64, actual: u64) -> Result<()> {
        if actual > max {
            Err(BeeError::too_large(limit, max, actual))
        } else {
            Ok(())
        }
    }

    /// Fails with `TOO_LARGE` when a blob of `len` bytes exceeds the blob size limit.
    pub fn check_blob_size(&self, len: u64) -> Result<()> {
        Self::check("blob_size", self.blob_size, len)
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Reason;

    #[test]
    fn defaults_match_spec() {
        let l = Limits::default();
        assert_eq!(l, Limits::DEFAULT);
        assert_eq!(l.blob_size, 100 * 1024 * 1024);
        assert_eq!(l.tree_entries, 1_000_000);
        assert_eq!(l.delta_entries, 1_000_000);
        assert_eq!(l.parents, 16);
        assert_eq!(l.message_bytes, 1024 * 1024);
        assert_eq!(l.encoded_object_bytes, 256 * 1024 * 1024);
        assert_eq!(l.path_bytes, 4096);
        assert_eq!(l.path_depth, 256);
        assert_eq!(l.component_bytes, 255);
        assert_eq!(Limits::BLOB_SIZE_HARD_CAP, 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn blob_size_is_configurable_up_to_cap() {
        assert_eq!(Limits::with_blob_size(GIB).unwrap().blob_size, GIB);
        assert_eq!(
            Limits::with_blob_size(4 * GIB + 1).unwrap_err().reason,
            Reason::TooLarge
        );
        let l = Limits::DEFAULT;
        assert!(l.check_blob_size(100 * MIB).is_ok());
        assert_eq!(
            l.check_blob_size(100 * MIB + 1).unwrap_err().reason,
            Reason::TooLarge
        );
        assert!(Limits::check("x", 1, 1).is_ok());
        assert!(Limits::check("x", 1, 2).is_err());
    }
}
