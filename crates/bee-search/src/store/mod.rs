//! Index storage: documents (one per content-addressed blob per org), trigram posting lists
//! and per-(repo, ref) path manifests, behind the [`IndexStore`] trait.
//!
//! - [`MemoryIndexStore`]: maps in memory, for tests and small deployments.
//! - [`DiskIndexStore`]: immutable segment files plus atomically replaced manifest files,
//!   read with plain `std::fs` (no mmap).
//!
//! Documents are keyed by blob id, so a blob shared by many repos, refs or paths of one org
//! is indexed once (SPEC §6.1 dedup). A document says nothing about where it appears;
//! authorization goes through manifests and the caller's path check (SPEC §9.3).

pub mod codec;
pub mod disk;
pub mod memory;

use std::collections::BTreeMap;
use std::sync::Arc;

use bee_core::{Mode, ObjectId, RepoPath, Result};
use bee_store::{OrgId, RepoId};

pub use disk::DiskIndexStore;
pub use memory::MemoryIndexStore;

use crate::lang::Language;
use crate::trigram::Trigram;

/// A document's number within its org: assigned once, never reused while referenced.
pub type DocNum = u32;

/// Files larger than this are recorded but not indexed (their content is not searchable).
pub const MAX_INDEXED_BYTES: u64 = 1 << 20;

/// Bytes inspected for a NUL to classify a file as binary.
pub const BINARY_SNIFF_BYTES: usize = 8 << 10;

/// Why a document's content is not indexed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SkipReason {
    /// Larger than [`MAX_INDEXED_BYTES`].
    TooLarge,
    /// Contains a NUL byte in the first [`BINARY_SNIFF_BYTES`].
    Binary,
}

impl SkipReason {
    /// Stable on-disk code (0 is "indexed").
    pub fn code(self) -> u8 {
        match self {
            SkipReason::TooLarge => 1,
            SkipReason::Binary => 2,
        }
    }

    /// Parses [`SkipReason::code`]; `Ok(None)` for 0.
    pub fn from_code(code: u8) -> std::result::Result<Option<SkipReason>, u8> {
        match code {
            0 => Ok(None),
            1 => Ok(Some(SkipReason::TooLarge)),
            2 => Ok(Some(SkipReason::Binary)),
            other => Err(other),
        }
    }
}

/// One blob's searchable form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    /// The blob.
    pub blob: ObjectId,
    /// Its bytes when indexed; empty when skipped.
    pub content: Vec<u8>,
    /// Set when the content is not indexed.
    pub skip: Option<SkipReason>,
}

impl Document {
    /// Builds a document from a blob's bytes, classifying it (pure function of the bytes).
    pub fn from_bytes(blob: ObjectId, bytes: Vec<u8>) -> Document {
        if bytes.len() as u64 > MAX_INDEXED_BYTES {
            return Document::skipped(blob, SkipReason::TooLarge);
        }
        let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
        if sniff.contains(&0) {
            return Document::skipped(blob, SkipReason::Binary);
        }
        Document {
            blob,
            content: bytes,
            skip: None,
        }
    }

    /// A document whose content is not indexed.
    pub fn skipped(blob: ObjectId, reason: SkipReason) -> Document {
        Document {
            blob,
            content: Vec::new(),
            skip: Some(reason),
        }
    }

    /// Trigrams of the indexed content (none when skipped).
    pub fn trigrams(&self) -> Vec<Trigram> {
        if self.skip.is_some() {
            Vec::new()
        } else {
            crate::trigram::trigrams_of(&self.content)
        }
    }
}

/// One path of a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// The blob (or foreign id for a gitlink).
    pub blob: ObjectId,
    /// The tree entry mode.
    pub mode: Mode,
    /// Blob size from the tree.
    pub size: u64,
    /// The indexed document; `None` for symlinks and gitlinks, which are listed for
    /// `path:` queries but have no searchable content.
    pub doc: Option<DocNum>,
    /// Detected language.
    pub language: Option<Language>,
}

/// The indexed view of one ref: every file path with its document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The org (documents live in its namespace).
    pub org: OrgId,
    /// The repo.
    pub repo: RepoId,
    /// The ref, e.g. `heads/main`.
    pub ref_name: String,
    /// The changeset this manifest reflects.
    pub changeset: ObjectId,
    /// Files by path.
    pub files: BTreeMap<RepoPath, FileEntry>,
}

/// Storage for the code index.
///
/// # Contract
///
/// - **Insert-or-ignore documents.** [`IndexStore::add_documents`] returns the existing
///   number for a blob already present and assigns fresh numbers, in input order, to new
///   ones. Numbers are never reassigned while a manifest references them.
/// - **Posting lists** are sorted ascending without duplicates.
/// - **Write ordering.** Callers add documents before the manifest that references them, so
///   a visible manifest never names a missing document.
/// - **Determinism.** Given the same sequence of calls, every read returns the same values.
/// - Errors are SPEC §12.2 reasons: `INTEGRITY` for corrupt files, `UNAVAILABLE` for I/O
///   failures.
pub trait IndexStore: Send + Sync {
    /// Document numbers for blobs, `None` where absent.
    fn lookup(&self, org: OrgId, blobs: &[ObjectId]) -> Result<Vec<Option<DocNum>>>;

    /// Adds documents (insert-or-ignore) and indexes their trigrams as one unit.
    fn add_documents(&self, org: OrgId, docs: Vec<Document>) -> Result<Vec<DocNum>>;

    /// Reads a document.
    fn document(&self, org: OrgId, doc: DocNum) -> Result<Option<Arc<Document>>>;

    /// The sorted documents containing a trigram.
    fn postings(&self, org: OrgId, trigram: Trigram) -> Result<Vec<DocNum>>;

    /// Number of live documents in the org.
    fn doc_count(&self, org: OrgId) -> Result<u64>;

    /// Reads a manifest.
    fn manifest(&self, org: OrgId, repo: RepoId, ref_name: &str) -> Result<Option<Arc<Manifest>>>;

    /// Replaces a manifest atomically.
    fn put_manifest(&self, manifest: Manifest) -> Result<()>;

    /// Deletes a manifest; true if it existed.
    fn delete_manifest(&self, org: OrgId, repo: RepoId, ref_name: &str) -> Result<bool>;

    /// Every manifest of the org as `(repo, ref)`, ordered.
    fn list_manifests(&self, org: OrgId) -> Result<Vec<(RepoId, String)>>;

    /// Drops documents whose blob `keep` rejects, rewriting storage; surviving documents
    /// keep their numbers. Returns how many were dropped.
    fn compact(&self, org: OrgId, keep: &dyn Fn(&ObjectId) -> bool) -> Result<usize>;
}

#[cfg(test)]
pub(crate) mod contract {
    //! Behaviour every [`IndexStore`] must share; run against both implementations.

    use super::*;
    use bee_core::Ulid;
    use bee_object::blob_id;

    pub fn org() -> OrgId {
        OrgId(Ulid::from_u128(7))
    }

    fn doc(text: &str) -> Document {
        Document::from_bytes(blob_id(text.as_bytes()), text.as_bytes().to_vec())
    }

    fn tri(s: &str) -> Trigram {
        let b = s.as_bytes();
        Trigram::new(b[0], b[1], b[2])
    }

    pub fn manifest(org: OrgId, repo: u128, doc: DocNum, blob: ObjectId) -> Manifest {
        let mut files = BTreeMap::new();
        files.insert(
            RepoPath::new("src/a.rs").unwrap(),
            FileEntry {
                blob,
                mode: Mode::File,
                size: 3,
                doc: Some(doc),
                language: Some(Language::Rust),
            },
        );
        files.insert(
            RepoPath::new("link").unwrap(),
            FileEntry {
                blob,
                mode: Mode::Symlink,
                size: 3,
                doc: None,
                language: None,
            },
        );
        Manifest {
            org,
            repo: RepoId(Ulid::from_u128(repo)),
            ref_name: "heads/main".into(),
            changeset: bee_object::Changeset::empty_root().id(),
            files,
        }
    }

    pub fn run(store: &dyn IndexStore) {
        let o = org();
        let nums = store
            .add_documents(
                o,
                vec![doc("hello world"), doc("yellow"), doc("hello world")],
            )
            .unwrap();
        assert_eq!(nums[0], nums[2]);
        assert_ne!(nums[0], nums[1]);
        let again = store.add_documents(o, vec![doc("yellow")]).unwrap();
        assert_eq!(again, vec![nums[1]]);
        assert_eq!(store.doc_count(o).unwrap(), 2);
        assert_eq!(
            store
                .lookup(o, &[blob_id(b"yellow"), blob_id(b"nope")])
                .unwrap(),
            vec![Some(nums[1]), None]
        );
        let mut both = vec![nums[0], nums[1]];
        both.sort();
        assert_eq!(store.postings(o, tri("ell")).unwrap(), both);
        assert_eq!(store.postings(o, tri("ELL")).unwrap(), both);
        assert_eq!(store.postings(o, tri("wor")).unwrap(), vec![nums[0]]);
        assert!(store.postings(o, tri("zzz")).unwrap().is_empty());
        let d = store.document(o, nums[1]).unwrap().unwrap();
        assert_eq!(d.content, b"yellow");
        assert!(store.document(o, 999).unwrap().is_none());
        // Binary and large documents are kept but not indexed.
        let bin = Document::from_bytes(blob_id(b"a\0b"), b"a\0b".to_vec());
        assert_eq!(bin.skip, Some(SkipReason::Binary));
        let n = store.add_documents(o, vec![bin]).unwrap()[0];
        assert_eq!(
            store.document(o, n).unwrap().unwrap().skip,
            Some(SkipReason::Binary)
        );
        // Other orgs are separate namespaces.
        let other = OrgId(Ulid::from_u128(8));
        assert_eq!(
            store.lookup(other, &[blob_id(b"yellow")]).unwrap(),
            vec![None]
        );
        assert!(store.postings(other, tri("ell")).unwrap().is_empty());

        // Manifests.
        let m = manifest(o, 1, nums[0], blob_id(b"hello world"));
        assert!(store.manifest(o, m.repo, "heads/main").unwrap().is_none());
        store.put_manifest(m.clone()).unwrap();
        assert_eq!(
            *store.manifest(o, m.repo, "heads/main").unwrap().unwrap(),
            m
        );
        let m2 = manifest(o, 2, nums[1], blob_id(b"yellow"));
        store.put_manifest(m2.clone()).unwrap();
        assert_eq!(
            store.list_manifests(o).unwrap(),
            vec![
                (m.repo, "heads/main".to_string()),
                (m2.repo, "heads/main".to_string())
            ]
        );
        assert!(store.delete_manifest(o, m2.repo, "heads/main").unwrap());
        assert!(!store.delete_manifest(o, m2.repo, "heads/main").unwrap());
        assert_eq!(store.list_manifests(o).unwrap().len(), 1);

        // Compaction keeps numbers of survivors and drops the rest from postings.
        let keep_blob = blob_id(b"hello world");
        let dropped = store.compact(o, &|b| *b == keep_blob).unwrap();
        assert_eq!(dropped, 2);
        assert_eq!(store.doc_count(o).unwrap(), 1);
        assert_eq!(store.postings(o, tri("ell")).unwrap(), vec![nums[0]]);
        assert_eq!(
            store.document(o, nums[0]).unwrap().unwrap().content,
            b"hello world"
        );
        assert!(store.document(o, nums[1]).unwrap().is_none());
        // New documents never reuse a number.
        let fresh = store.add_documents(o, vec![doc("brand new")]).unwrap()[0];
        assert!(fresh > n);
    }
}
