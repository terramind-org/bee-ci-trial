//! One-way git history importer with git-to-bee mappings (SPEC §17, PLAN task W9).
//!
//! Git history goes in; changesets with identical git commit ids come out (SPEC §17.4), with
//! a fidelity report (SPEC §17.6) saying what was carried and what was not, and why.
//!
//! - [`source`]: object sources. [`RepoSource`] reads a local repository (packs and loose
//!   objects parsed directly, `git cat-file --batch` as a fallback); [`PackSource`] serves a
//!   fetched packfile; [`parse_ref_list`] reads the ref list that comes with it.
//! - [`refs`]: git ref names → bee ref names (SPEC §5 is stricter than git), with every
//!   rename recorded.
//! - [`pipeline`]: the import itself: [`import_repository`], [`import_pack`] and the general
//!   [`import_refs`]. Writes go through [`bee_store::Writer`] in the SPEC §6.2 order, in
//!   bounded batches, parents first; re-imports are incremental.
//! - [`report`]: the machine-readable fidelity report
//!   (`schemas/v1/import-report.schema.json`).
//! - [`lfs`]: Git LFS pointer detection.
//!
//! # Derived rows
//!
//! The importer writes two derived types (SPEC §8), keyed so either direction is one lookup:
//!
//! | Type | Key | Value |
//! |---|---|---|
//! | `git_mapping` v1 | a changeset, tree or blob id | the raw git object id (20 or 32 bytes) |
//! | `git_mapping` v1 | the git id as a foreign id (`g2_…`, SPEC §3.1) | the 34-byte bee id |
//! | `git_tag` v1 | an annotated tag's git id as a foreign id | the tag object's exact bytes |
//! | `git_ref_tag` v1 | [`ref_tag_key`]: a tag ref at a changeset | the raw git id of the annotated tag that ref carries |
//!
//! `git_ref_tag` rows record which ref a tag object belongs to (the object itself does not say), so
//! a git client is shown the ref as the annotated tag with its exact id. The importer writes one for
//! every tag ref whose tag object points directly at the commit (a tag of a tag is kept as a record
//! and served as a lightweight tag).
//!
//! `git_mapping` rows are written in each changeset's CreateChangeset transaction; `git_tag`
//! rows just before the ref that needs them. [`lookup_bee_id`], [`lookup_git_id`] and
//! [`lookup_git_tag`] read them back.
//!
//! # Determinism
//!
//! Every id, row and report field is a pure function of the git input and the options: maps
//! are ordered, no clock or randomness is read (row times come from
//! [`ImportOptions::at_ms`]), and ChangeIds follow the SPEC §3.3 importer rule.

pub mod lfs;
pub mod pipeline;
pub mod refs;
pub mod report;
pub mod source;

pub use pipeline::{
    DerivedHook, ImportOptions, ImportOutcome, ImportTarget, RefPublisher, drop_reason,
    git_mapping_rows, import_pack, import_refs, import_repository, lookup_bee_id, lookup_git_id,
    lookup_git_tag,
};
pub use refs::{MappedRef, RefKind, RefMapping, bee_ref_name, map_ref_names};
pub use report::{ImportReport, REPORT_SCHEMA};
pub use source::{GitRef, PackSource, RepoSource, SharedSource, find_git_dir, parse_ref_list};

/// Derived type name of the git id ↔ bee id rows.
pub const GIT_MAPPING_TYPE: &str = "git_mapping";
/// Version of the `git_mapping` encoding.
pub const GIT_MAPPING_VERSION: u32 = 1;
/// Derived type name linking a tag ref to the annotated tag object it carries.
pub const GIT_REF_TAG_TYPE: &str = "git_ref_tag";
/// Version of the `git_ref_tag` encoding.
pub const GIT_REF_TAG_VERSION: u32 = 1;
/// Derived type name of annotated tag records.
pub const GIT_TAG_TYPE: &str = "git_tag";
/// Version of the `git_tag` encoding.
pub const GIT_TAG_VERSION: u32 = 1;

/// The key of the [`GIT_REF_TAG_TYPE`] row for a tag ref at a changeset: a foreign id of BLAKE3
/// over the ref name and the changeset id.
pub fn ref_tag_key(
    ref_name: &bee_core::RefName,
    changeset: &bee_core::ObjectId,
) -> bee_core::Result<bee_core::ObjectId> {
    let mut h = blake3::Hasher::new();
    h.update(b"git_ref_tag\0");
    h.update(ref_name.as_str().as_bytes());
    h.update(&[0]);
    h.update(changeset.as_bytes());
    bee_core::ObjectId::git_foreign(h.finalize().as_bytes())
}
