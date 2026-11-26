//! Storage traits for metadata (refs, changesets, indexes) and blobs, plus write-ordering
//! rules (SPEC §6).
//!
//! - [`MetaStore`]: the serializable metadata database; one method is one transaction.
//! - [`BlobStore`]: immutable, content-addressed bytes, keyed by org ([`blob_key`]).
//! - [`memory`]: faithful in-memory implementations of both (SPEC §14.3).
//! - [`faults`]: named fault points at every store call and write-ordering step (§14.4).
//! - [`counters`]: storage access counters for complexity gates (§14.6).
//! - [`write::Writer`]: the write-ordering protocol; [`trees`]: tree prefetch and
//!   `object_paths` computation; [`consistency`]: replica reads with primary fallback;
//!   [`retry`]: transaction retry rules.
//! - [`mod@fsck`]: the invariant oracle; [`gc`]: mark-and-sweep GC with dry runs.
//!
//! # Write ordering (SPEC §6.2)
//!
//! The rule: **bytes are durable before the row that refers to them, and a visible row
//! implies durable bytes.** Concretely:
//!
//! 1. *Upload.* Verify bytes against the id; write them to the blob store (unless the object
//!    is a tree or meta of at most [`INLINE_MAX_BYTES`], which lives inline in its row);
//!    then insert the `objects` row, insert-or-ignore. Bytes without a row are orphans that
//!    GC removes after 24 hours.
//! 2. *CreateChangeset.* Every object the changeset references (parents, root tree, meta,
//!    delta puts, and the trees of its `object_paths` rows) already has a row. One
//!    transaction inserts the changeset row (its canonical bytes; changesets live only
//!    there), its parent edges, its `object_paths` rows and its eager derived rows, all
//!    insert-or-ignore. Re-running after a crash repairs missing rows. The objects a new
//!    changeset creates (its meta and trees) may instead be inserted in that same
//!    transaction, after step 1's blob-store bytes ([`write::Writer::store_new`]).
//! 3. *Ref update.* One transaction checks the expected version (or absence), writes
//!    `version + 1` per step with a ref-log entry each, inserts the landed rows and appends
//!    events. A landed-index violation aborts the whole transaction.
//!
//! Deletion runs in the opposite order: a row stops being live (purged by redaction, or
//! deleted by GC) before its bytes are deleted. A crash at any step therefore leaves either
//! orphans or drafts, never a dangling reference; `fsck` checks this, and the fault tests in
//! `bee-testkit` crash at every step to prove it.
//!
//! Determinism: nothing here reads the clock or a random source; times arrive as `at_ms`
//! arguments or through an injected [`clock::Clock`].

pub mod blob;
pub mod clock;
pub mod consistency;
pub mod counters;
pub mod faults;
pub mod fsck;
pub mod gc;
pub mod memory;
pub mod meta;
pub mod retry;
pub mod staged;
pub mod trees;
pub mod types;
pub mod write;

pub use blob::{BlobInfo, BlobStore, blob_key};
pub use clock::{Clock, ManualClock, SystemClock};
pub use counters::{AccessCounters, AccessSnapshot, FetchKind, Table};
pub use faults::{Fault, FaultInjector, FaultRule, FaultyBlob, FaultyMeta};
pub use fsck::{FsckOptions, FsckReport, Violation, ViolationKind, fsck};
pub use gc::{GcOptions, GcPlan, GcReport};
pub use memory::{MemoryBlobStore, MemoryMetaStore};
pub use meta::MetaStore;
pub use staged::StagedMeta;
pub use types::*;
pub use write::{BuiltChangeset, Writer};
