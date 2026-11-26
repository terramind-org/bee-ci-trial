//! Changeset graph and history without a clone: ancestry, merge base, log, path history,
//! blame, diff, revisions, read path and list tree, plus the tree-level rebase, merge and
//! land helpers the server's `WriteService` calls (SPEC §5, §7.3, §7.4, §8).
//!
//! Everything hangs off [`History`], a per-repo handle over a [`bee_store::Writer`]:
//!
//! - **Derived rows** this crate owns because history needs them below the derived-data
//!   framework's layer: [`graph`] (generation, skip pointers, first-parent merge counts),
//!   [`pathhist`] (per-path history nodes), [`blame`] (lazy line attribution) and
//!   [`gitmap`] (rows written by the importer). [`derive`] is the iterative forward
//!   derivation they share with `bee-derived`; [`eager`] writes the eager rows with every
//!   changeset this crate creates.
//! - **Queries**: [`History::resolve`], [`History::log`], [`History::path_log`],
//!   [`History::blame`], [`History::diff`], [`History::is_ancestor`],
//!   [`History::merge_bases`], [`History::read_path`], [`History::list_tree`].
//! - **Writes**: [`History::create_changeset`], [`History::rebase`], [`History::merge`],
//!   [`History::land_stack`] and [`History::rebase_stack`]. They examine only the paths
//!   that changed, are pure functions of their inputs (no clock, no randomness), and report
//!   conflicts as data ([`ConflictSet`]).

pub mod blame;
pub mod codec;
pub mod derive;
pub mod diff;
pub mod eager;
pub mod gitmap;
pub mod graph;
pub mod log;
pub mod merge;
pub mod pathhist;
pub mod repo;
pub mod revision;
pub mod tree_read;

pub use blame::{BlameLine, BlameOptions, BlameRange, BlameResult};
pub use derive::{EnsureStats, Forward, ensure, ensure_with_stats};
pub use diff::{DiffOptions, FileChange, FileDiff, Hunk, HunkLine, LineKind};
pub use eager::{CoreEager, EagerRows, core_eager_types};
pub use graph::{GRAPH_TYPE, GRAPH_VERSION, GraphNode};
pub use log::{ChangeKind, ChangedPath, LogEntry, LogOptions, LogPage, PathLogEntry, PathLogPage};
pub use merge::{
    ConflictSet, ConflictSide, MergeBase, MergeOptions, MergeOutcome, PathConflictData,
    RebaseOptions, RebaseOutcome, StackOutcome, StackStep,
};
pub use pathhist::{NodeKind, PATH_HISTORY_TYPE, PATH_HISTORY_VERSION, PathNode};
pub use repo::{BlobContent, History, SharedRows};
pub use tree_read::{ListTreePage, PathContent, TreeItem};
