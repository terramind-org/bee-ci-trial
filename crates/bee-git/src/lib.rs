//! Git object format, packfiles, and the exact mapping between git objects and bee objects
//! (SPEC §17.4).
//!
//! - [`oid`]: the two git object formats (SHA-1 with collision detection, SHA-256) and
//!   [`GitOid`].
//! - [`object`], [`tree`], [`commit`], [`tag`]: byte-exact parsing and serialization of
//!   loose git objects. Parsing keeps every byte, so `serialize(parse(x)) == x`.
//! - [`delta`] and [`pack`]: git delta encoding, packfile v2 reading (ofs and ref deltas,
//!   thin packs) and writing, and pack index v2.
//! - [`note`]: the per-tree git note (tree extension key 64) that keeps non-canonical git
//!   modes, original entry order and empty directories, so a git tree id reproduces.
//! - [`import`]: git → bee. Commits become changesets whose Meta keeps the original header
//!   bytes (Meta key 64), so [`export`] rebuilds the identical commit id.
//! - [`export`]: bee → git, both for imported history (exact) and for new history (the
//!   deterministic representation with a `Change-Id` trailer).
//! - [`mapping`]: the bidirectional git id ↔ bee id index behind the derived `git_mapping`.
//!
//! Everything is a pure function of its inputs: no clock, no randomness, no hash-map
//! iteration order.

pub mod commit;
pub mod delta;
mod error;
pub mod export;
pub mod import;
pub mod mapping;
pub mod note;
pub mod object;
pub mod oid;
pub mod pack;
pub mod tag;
pub mod tree;

pub use commit::{CommitSignature, GitCommit, Ident};
pub use export::{
    BeeObjectSource, ExportedCommit, Exporter, change_id_trailer, new_history_commit,
    sanitize_ident,
};
pub use import::{GitObjectSource, ImportedCommit, Importer};
pub use mapping::GitMapping;
pub use note::{GIT_TREE_NOTE_KEY, HiddenEntry, TreeNote};
pub use object::{GitObject, ObjectType, RawObject};
pub use oid::{GitHasher, GitOid, ObjectFormat};
pub use pack::{
    DeltaBase, Pack, PackEntry, PackEntryKind, PackIndex, PackIndexEntry, PackObject,
    ResolvedEntry, WrittenPack, write_pack,
};
pub use tag::GitTag;
pub use tree::{EntryKind, GitTree, GitTreeEntry};
