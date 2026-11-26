//! Line diff, three-way merge and structured conflicts (SPEC §7.3, `docs/contracts/diff3-v1.md`).
//!
//! This crate is a pure library: every function is a deterministic function of its inputs. It
//! uses no clock, no randomness and never iterates a hash map in an order that reaches an
//! output. It depends on no other bee crate; callers adapt their object types to the small
//! input types defined here ([`Side`], [`Tree`], [`OwnedTree`]).
//!
//! Layers, from the bottom up:
//!
//! - [`split_lines`]: the line model (split on `\n`, terminators kept, no CRLF normalization).
//! - [`diff_lines`] / [`diff`]: exact Myers diff, rendered as [`DiffHunk`]s, unified hunks
//!   ([`unified_hunks`], [`unified_diff`]) and statistics ([`DiffStats`]).
//! - [`merge_lines`]: the `diff3-v1` three-way line merge, producing a [`LineMerge`] whose
//!   conflicts are data ([`ConflictHunk`]) and can be rendered as markers for a working copy
//!   ([`LineMerge::render`]).
//! - [`merge_path`]: the per-path rules 1–9 of SPEC §7.3.
//! - [`merge_trees`]: rename- and copy-aware merge of flat path maps, including the structural
//!   `file_directory` check ([`file_directory_conflicts`]).
//! - [`virtual_base`]: a virtual merge base for criss-cross merges.
//!
//! The algorithm identifier is [`ALGORITHM_ID`]. Any change to observable behaviour ships as a
//! new identifier; rebase and merge results are pure functions of their inputs and this id.

mod diff;
mod lines;
mod markers;
mod merge3;
mod myers;
mod path;
mod rename;
mod structural;
mod tree;
mod virtual_base;

pub use diff::{
    DiffHunk, DiffLine, DiffStats, LineOrigin, UnifiedHunk, diff, diff_lines, render_unified,
    unified_diff, unified_hunks,
};
pub use lines::split_lines;
pub use markers::{MarkerStyle, render_conflict};
pub use merge3::{ConflictHunk, LineMerge, Region, Source, merge_lines, merge_two_way};
pub use path::{
    BINARY_SNIFF_BYTES, ConflictKind, Content, ContentSource, DEFAULT_MAX_TEXT_BYTES, MergeOptions,
    Mode, PathConflict, PathMergeInput, PathOutcome, Resolution, Side, is_binary, merge_path,
};
pub use rename::{Rename, RenameOptions, Similarity, detect_renames, line_similarity};
pub use structural::{FileDirectoryConflict, file_directory_conflicts};
pub use tree::{
    OwnedContent, OwnedSide, OwnedTree, RenameRecord, Tree, TreeConflict, TreeContent, TreeEntry,
    TreeMerge, TreeMergeOptions, Which, borrow_tree, merge_trees,
};
pub use virtual_base::{MergeGraph, VirtualBase, virtual_base};

/// The versioned identifier of the diff and merge algorithm implemented by this crate.
///
/// A rebase or merge result is a pure function of its inputs and this identifier. Behaviour
/// changes ship under a new identifier; `diff3-v1` is frozen.
pub const ALGORITHM_ID: &str = "diff3-v1";
