//! The bee working copy (SPEC §10.3, PRODUCT §4.6).
//!
//! A working copy is a directory with `.bee/` holding the endpoint and repo, the checked-out
//! changeset, the sparse profile, a stat cache and an operation journal. There is no staging
//! area: the working tree *is* the change. Every file that is tracked, or untracked and not
//! ignored by `.gitignore`, is part of it; adds, removes and moves are found by comparing the
//! tree with the base manifest (BLAKE3 content ids).
//!
//! - [`WorkingCopy::status`] walks the tree with a stat cache and never touches the network.
//! - `commit`, `amend`, `describe` and the stack operations (`stack_new`, `stack_edit`,
//!   `squash`, `split`) create changesets with `CreateChangeset` (the delta from the parent),
//!   uploading changed blobs first; changes above a rewritten one are rebased server-side.
//! - [`WorkingCopy::sync`] rebases the local stack onto the target ref server-side; conflicts
//!   come back as data and are written with `diff3-v1` markers until [`WorkingCopy::resolve`]
//!   or [`WorkingCopy::abort`]. [`WorkingCopy::land`] calls `Land` (the server coalesces
//!   concurrent lands on one ref).
//! - Every mutating operation journals its intent first ([`journal`]); the next command
//!   finishes an interrupted one. [`WorkingCopy::undo`] reverts local operations.
//!
//! The server is reached only through the public `bee.v1` API ([`remote::Transport`]).

pub mod checkout;
pub mod conflict;
pub mod eol;
pub mod error;
pub mod fsutil;
pub mod ignore;
pub mod journal;
pub mod manifest;
pub mod ops;
pub mod remote;
pub mod report;
pub mod scan;
pub mod sparse;
pub mod state;
pub mod wc;

pub use error::{ExitClass, Result, WcError};
pub use journal::Faults;
pub use ops::{CommitOptions, CreateOptions, FileDiff, Pick, create};
pub use remote::{Remote, Signature, Transport};
pub use report::{OpReport, StatusReport};
pub use sparse::Sparse;
pub use wc::{BEE_DIR, OpenOptions, WorkingCopy, find_root, read_config};

/// Serializes a report (or any of this crate's serializable values) as JSON.
pub fn to_json<T: serde::Serialize>(value: &T) -> std::result::Result<serde_json::Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;
