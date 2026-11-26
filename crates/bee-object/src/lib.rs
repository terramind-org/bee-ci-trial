//! Canonical encoding and content-addressed objects: blob, tree, changeset, meta.
//!
//! - [`cbor`]: sCBOR v1, the strict deterministic CBOR profile (SPEC §4).
//! - [`Tree`], [`Meta`], [`Changeset`]: the structured objects, each with `encode`,
//!   `decode` (rejecting every non-canonical form) and `id`.
//! - Blobs are raw bytes; [`blob_id`] hashes them.
//! - [`apply_delta`] builds new trees from a parent tree and a delta, reading O(depth) trees
//!   per changed path; [`diff_trees`] computes file-level differences; and
//!   [`check_invariant_d`] verifies a changeset's delta against its trees.
//!
//! Everything here is a pure function of its inputs.

pub mod build;
pub mod cbor;
pub mod changeset;
pub mod fields;
pub mod meta;
pub mod tree;

pub use build::{
    BuiltTree, MemoryTrees, TreeSource, apply_delta, build_changeset, check_invariant_d,
    diff_trees, list_leaves,
};
pub use cbor::Value;
pub use changeset::{Changeset, DeltaEntry, DeltaOp, validate_delta};
pub use fields::{Extensions, FORMAT_VERSION};
pub use meta::{Cost, GIT_EXTENSION_KEY, LocalCheck, Meta, Provenance, Timestamp};
pub use tree::{Tree, TreeEntry};

use bee_core::{BeeError, ChangeId, Limits, ObjectId, ObjectKind, Reason, Result, Subcode};

/// BLAKE3-256 of `bytes`.
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// The id of an object of `kind` whose hashed bytes are `bytes`.
pub fn hash_id(kind: ObjectKind, bytes: &[u8]) -> ObjectId {
    ObjectId::new(kind, digest(bytes))
}

/// A blob's id: BLAKE3 over its raw bytes (SPEC §4.1).
pub fn blob_id(data: &[u8]) -> ObjectId {
    hash_id(ObjectKind::Blob, data)
}

/// The ChangeId the importer gives a git commit (SPEC §3.3): the first 16 bytes of
/// BLAKE3(`"swarm-import\0"` ‖ the commit's raw object id bytes).
pub fn import_change_id(git_oid: &[u8]) -> ChangeId {
    let mut input = b"swarm-import\0".to_vec();
    input.extend_from_slice(git_oid);
    let d = digest(&input);
    let mut out = [0u8; 16];
    out.copy_from_slice(&d[..16]);
    ChangeId::new(out)
}

/// A decoded object of any kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Object {
    /// Raw blob bytes.
    Blob(Vec<u8>),
    /// A tree.
    Tree(Tree),
    /// A meta.
    Meta(Box<Meta>),
    /// A changeset.
    Changeset(Changeset),
}

impl Object {
    /// The object's kind.
    pub fn kind(&self) -> ObjectKind {
        match self {
            Object::Blob(_) => ObjectKind::Blob,
            Object::Tree(_) => ObjectKind::Tree,
            Object::Meta(_) => ObjectKind::Meta,
            Object::Changeset(_) => ObjectKind::Changeset,
        }
    }

    /// The bytes the id is computed over: raw bytes for blobs, the canonical encoding
    /// otherwise.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Object::Blob(b) => b.clone(),
            Object::Tree(t) => t.encode(),
            Object::Meta(m) => m.encode(),
            Object::Changeset(c) => c.encode(),
        }
    }

    /// The object's id.
    pub fn id(&self) -> ObjectId {
        hash_id(self.kind(), &self.encode())
    }

    /// Decodes bytes of a known kind, enforcing `limits` (blob size included).
    pub fn decode(kind: ObjectKind, bytes: &[u8], limits: &Limits) -> Result<Object> {
        match kind {
            ObjectKind::Blob => {
                limits.check_blob_size(bytes.len() as u64)?;
                Ok(Object::Blob(bytes.to_vec()))
            }
            ObjectKind::Tree => Tree::decode_with_limits(bytes, limits).map(Object::Tree),
            ObjectKind::Meta => {
                Meta::decode_with_limits(bytes, limits).map(|m| Object::Meta(Box::new(m)))
            }
            ObjectKind::Changeset => {
                Changeset::decode_with_limits(bytes, limits).map(Object::Changeset)
            }
            other => Err(BeeError::invalid(
                Subcode::WrongKind,
                format!("{} objects are not stored as bytes here", other.text_tag()),
            )),
        }
    }

    /// Verifies bytes received under `id` (SPEC §7.1, §10.1): the digest must match
    /// (`INTEGRITY` otherwise) and structured objects must decode canonically.
    pub fn verify(id: &ObjectId, bytes: &[u8], limits: &Limits) -> Result<Object> {
        if hash_id(id.kind(), bytes) != *id {
            return Err(BeeError::new(
                Reason::Integrity,
                format!("bytes do not hash to {id}"),
            ));
        }
        Object::decode(id.kind(), bytes, limits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::hex;

    #[test]
    fn blob_ids_are_blake3_of_raw_bytes() {
        // BLAKE3("") is af1349b9…; the id adds the algorithm and kind bytes.
        let id = blob_id(b"");
        assert_eq!(
            id.to_text(),
            "b1_af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(hex::encode(&id.as_bytes()[..2]), "0101");
        assert_ne!(blob_id(b"a"), blob_id(b"b"));
    }

    #[test]
    fn same_bytes_different_kinds_differ() {
        let t = Tree::empty();
        assert_ne!(hash_id(ObjectKind::Blob, &t.encode()), t.id());
    }

    #[test]
    fn import_change_id_is_deterministic() {
        let a = import_change_id(&[0x11; 20]);
        assert_eq!(a, import_change_id(&[0x11; 20]));
        assert_ne!(a, import_change_id(&[0x12; 20]));
        let mut input = b"swarm-import\0".to_vec();
        input.extend_from_slice(&[0x11; 20]);
        assert_eq!(&a.as_bytes()[..], &digest(&input)[..16]);
    }

    #[test]
    fn object_verify() {
        let limits = Limits::DEFAULT;
        let t = Tree::empty();
        let obj = Object::verify(&t.id(), &t.encode(), &limits).unwrap();
        assert_eq!(obj, Object::Tree(t.clone()));
        assert_eq!(obj.kind(), ObjectKind::Tree);
        assert_eq!(obj.id(), t.id());
        let err = Object::verify(&t.id(), b"junk", &limits).unwrap_err();
        assert_eq!(err.reason, Reason::Integrity);
        let blob = Object::verify(&blob_id(b"hi"), b"hi", &limits).unwrap();
        assert_eq!(blob, Object::Blob(b"hi".to_vec()));
        assert_eq!(blob.encode(), b"hi");
        let m = Meta::empty_root();
        assert_eq!(
            Object::verify(&m.id(), &m.encode(), &limits).unwrap(),
            Object::Meta(Box::new(m))
        );
        let c = Changeset::empty_root();
        let obj = Object::verify(&c.id(), &c.encode(), &limits).unwrap();
        assert_eq!(obj.kind(), ObjectKind::Changeset);
        assert_eq!(obj.id(), c.id());
        // Non-canonical bytes whose digest matches are still rejected.
        let mut bad = t.encode();
        bad.push(0);
        let bad_id = hash_id(ObjectKind::Tree, &bad);
        assert!(
            Object::verify(&bad_id, &bad, &limits)
                .unwrap_err()
                .is_invalid(Subcode::NonCanonical)
        );
        let tiny = Limits::with_blob_size(1).unwrap();
        assert_eq!(
            Object::decode(ObjectKind::Blob, b"ab", &tiny)
                .unwrap_err()
                .reason,
            Reason::TooLarge
        );
        assert!(Object::decode(ObjectKind::Foreign, b"", &limits).is_err());
        assert!(Object::decode(ObjectKind::Derived(0x10), b"", &limits).is_err());
    }
}
