//! Row types of the metadata store and the requests its methods take.
//!
//! The logical schema (SPEC §6, amended in `docs/contracts/amendments.md`):
//!
//! | Table | Key | Contents |
//! |---|---|---|
//! | `orgs` | `org` (unique `name`) | [`OrgRow`] |
//! | `repos` | `repo` (unique `(org, name)`) | [`RepoRow`] |
//! | `objects` | `(org, object)` | [`ObjectRow`]: size, [`ObjectState`], inline bytes for trees and metas ≤ [`INLINE_MAX_BYTES`] |
//! | `object_uploads` | `(org, object, principal)` | [`UploadRow`] |
//! | `changesets` | `(repo, changeset)`; index `(repo, change_id)` | [`ChangesetRow`]: the canonical bytes. Changesets live only here, never in the blob store or `objects` |
//! | `parent_edges` | `(repo, child, index)`; index `(repo, parent, child)` | first parent at index 0 |
//! | `object_paths` | `(repo, object, path)` | [`ObjectPathRow`], the §9.3 authorization index |
//! | `derived` | `(repo, key, type, version)` | [`DerivedRow`] |
//! | `refs` | `(repo, name)` | [`RefRow`]; a deleted ref keeps its row with no target so numbering continues |
//! | `ref_log` | `(repo, name, version)` | [`RefLogEntry`] |
//! | `landed` | unique `(repo, ref, change_id)` | [`LandedRow`] |
//! | `event_counters` | `repo` | last allocated event sequence number |
//! | `events` | `(repo, seq)` | [`EventRow`] |
//! | `redactions` | `(org, object)` | [`RedactionRow`]; the permanent blocklist |
//! | `redaction_epochs` | `repo` | counter bumped by each redaction that touches the repo |
//! | `gc_marks` | `(org, seq)` | [`GcEpoch`] |

use std::fmt;

use bee_core::{ChangeId, ObjectId, ObjectKind, RefName, RepoPath, Ulid};
use bee_object::Changeset;

/// Trees and metas whose encoding is at most this many bytes are stored inline in their
/// `objects` row instead of the blob store (SPEC §6.2).
pub const INLINE_MAX_BYTES: usize = 64 * 1024;

/// Default draft retention, in days (SPEC §6.4).
pub const DEFAULT_DRAFT_RETENTION_DAYS: u32 = 30;

/// Milliseconds in one day.
pub const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// True when an object of `kind` and encoded length `len` is stored inline in its row.
pub fn is_inline(kind: ObjectKind, len: usize) -> bool {
    matches!(kind, ObjectKind::Tree | ObjectKind::Meta) && len <= INLINE_MAX_BYTES
}

/// An organization's surrogate id (SPEC §3.2).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrgId(pub Ulid);

/// A repository's surrogate id (SPEC §3.2).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoId(pub Ulid);

impl fmt::Display for OrgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "org:{}", self.0)
    }
}

impl fmt::Debug for OrgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "repo:{}", self.0)
    }
}

impl fmt::Debug for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// An organization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrgRow {
    /// Surrogate id.
    pub id: OrgId,
    /// Mutable, unique label.
    pub name: String,
    /// Creation time, from the injected clock.
    pub created_at_ms: u64,
}

/// A repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoRow {
    /// Surrogate id.
    pub id: RepoId,
    /// Owning org; objects are deduplicated per org.
    pub org: OrgId,
    /// Mutable label, unique within the org.
    pub name: String,
    /// Creation time.
    pub created_at_ms: u64,
    /// How long unreferenced changesets are kept after their last use (SPEC §6.4).
    pub draft_retention_days: u32,
}

/// Lifecycle state of a stored object (PRODUCT §4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectState {
    /// Bytes are durable and served.
    Live,
    /// Bytes were moved aside (failed verification or held for inspection) and are not
    /// served; re-uploading verified bytes makes the object live again.
    Quarantined,
    /// Bytes were deleted by redaction (SPEC §13); the row stays as a tombstone so ids keep
    /// resolving to `REDACTED`.
    Purged,
}

impl ObjectState {
    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectState::Live => "live",
            ObjectState::Quarantined => "quarantined",
            ObjectState::Purged => "purged",
        }
    }
}

/// Storage class of blob-store bytes (SPEC §6.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StorageClass {
    /// The default, fast class.
    Hot,
    /// The cheaper class for objects not read for `cold_after_days`.
    Cold,
}

/// An object to record with [`crate::MetaStore::put_objects`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewObject {
    /// The object id. Changeset and foreign ids are rejected: changesets live in their own
    /// table and foreign ids name git objects that bee never stores.
    pub id: ObjectId,
    /// Length of the bytes the id hashes.
    pub size: u64,
    /// The bytes, for trees and metas that qualify per [`is_inline`]; `None` means the bytes
    /// are in the blob store (and MUST already be durable there).
    pub inline: Option<Vec<u8>>,
}

impl NewObject {
    /// The row for `bytes` under `id`: inline when [`is_inline`] says so, otherwise a
    /// reference to blob-store bytes.
    pub fn for_bytes(id: ObjectId, bytes: &[u8]) -> NewObject {
        NewObject {
            id,
            size: bytes.len() as u64,
            inline: is_inline(id.kind(), bytes.len()).then(|| bytes.to_vec()),
        }
    }

    /// True when the bytes must be written to the blob store before the row.
    pub fn needs_blob_store(&self) -> bool {
        self.inline.is_none()
    }
}

/// A row of the `objects` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRow {
    /// Owning org.
    pub org: OrgId,
    /// The object id.
    pub id: ObjectId,
    /// Length of the hashed bytes.
    pub size: u64,
    /// Lifecycle state.
    pub state: ObjectState,
    /// Inline bytes for small trees and metas.
    pub inline: Option<Vec<u8>>,
    /// When the row was first inserted.
    pub created_at_ms: u64,
}

/// Who uploaded an object, recorded with the row (SPEC §9.3: a never-referenced upload is
/// readable only by its uploader).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadRecord {
    /// The repo the upload went through, if any.
    pub repo: Option<RepoId>,
    /// The uploading principal.
    pub principal: String,
}

/// A row of `object_uploads`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadRow {
    /// Owning org.
    pub org: OrgId,
    /// The uploaded object.
    pub object: ObjectId,
    /// The repo the upload went through, if any.
    pub repo: Option<RepoId>,
    /// The uploading principal.
    pub principal: String,
    /// Time of the most recent upload by this principal.
    pub at_ms: u64,
}

/// Whether an insert-or-ignore wrote a new row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutOutcome {
    /// The row was new.
    Inserted,
    /// An identical key existed; nothing but touch times changed.
    AlreadyPresent,
}

/// An `object_paths` row (SPEC §9.3): `object` appears at `path` in some changeset of the
/// repo. `path = None` is the repository root (root trees).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectPathRow {
    /// The object.
    pub object: ObjectId,
    /// Where it appears; `None` for the root.
    pub path: Option<RepoPath>,
}

/// A derived-data row (SPEC §8).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DerivedRow {
    /// The key: a changeset or object id.
    pub key: ObjectId,
    /// Derived type name, e.g. `graph`.
    pub type_name: String,
    /// Derived type version.
    pub version: u32,
    /// Encoded value; opaque to the store.
    pub value: Vec<u8>,
}

/// A derived type that must be present for every changeset (fsck checks it).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EagerType {
    /// Type name.
    pub name: String,
    /// Type version.
    pub version: u32,
}

impl EagerType {
    /// An eager type.
    pub fn new(name: impl Into<String>, version: u32) -> EagerType {
        EagerType {
            name: name.into(),
            version,
        }
    }
}

/// What [`crate::MetaStore::insert_changeset`] writes in one transaction (SPEC §6.2 step 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewChangeset {
    /// The repo.
    pub repo: RepoId,
    /// The changeset; its canonical encoding is what gets stored.
    pub changeset: Changeset,
    /// Its `object_paths` rows (see [`crate::trees::object_paths_for`]).
    pub object_paths: Vec<ObjectPathRow>,
    /// Its eager derived rows.
    pub derived: Vec<DerivedRow>,
    /// Time of the write; becomes `created_at_ms` and `last_used_ms`.
    pub at_ms: u64,
}

/// A row of `changesets`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangesetRow {
    /// The repo.
    pub repo: RepoId,
    /// The changeset id (hash of `bytes`).
    pub id: ObjectId,
    /// The canonical encoding, the source of truth.
    pub bytes: Vec<u8>,
    /// The decoded changeset, as inserted.
    pub changeset: Changeset,
    /// When the row was inserted.
    pub created_at_ms: u64,
    /// Last time the changeset was created, used as a parent, targeted by a ref or touched;
    /// drafts are kept for the retention window after this (SPEC §6.4).
    pub last_used_ms: u64,
}

/// A ref (SPEC §5). A deleted ref keeps its row with `target = None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefRow {
    /// The repo.
    pub repo: RepoId,
    /// The ref name.
    pub name: RefName,
    /// Current target changeset, or `None` when deleted.
    pub target: Option<ObjectId>,
    /// Current version; starts at 1 and increases by exactly 1 per update.
    pub version: u64,
}

/// The precondition of a ref compare-and-swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expected {
    /// The ref must not exist or must be deleted.
    Absent,
    /// The ref's current version must be exactly this.
    Version(u64),
}

/// One `(change_id, source, result)` entry of a land (SPEC §5, §7.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LandedEntry {
    /// The change's stable identity.
    pub change_id: ChangeId,
    /// The changeset the client asked to land.
    pub source: ObjectId,
    /// The changeset that ended up on the ref (the rebased source).
    pub result: ObjectId,
}

/// One version step of a ref update: a new target and the changes it lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefStep {
    /// New target, or `None` to delete the ref.
    pub target: Option<ObjectId>,
    /// Changes landed by this step; inserted into the landed index.
    pub landed: Vec<LandedEntry>,
}

/// An event to append to a repo's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewEvent {
    /// Event kind, e.g. `ref.updated`.
    pub kind: String,
    /// Encoded payload; opaque to the store.
    pub payload: Vec<u8>,
    /// Event time.
    pub at_ms: u64,
}

/// A ref compare-and-swap (SPEC §6.2 step 3). Each step writes one version and one ref-log
/// entry; a land coalescer publishes K stacks as one update with K steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdate {
    /// The repo.
    pub repo: RepoId,
    /// The ref.
    pub name: RefName,
    /// The precondition.
    pub expected: Expected,
    /// One or more steps, applied in order.
    pub steps: Vec<RefStep>,
    /// Who is writing, for the ref log.
    pub principal: String,
    /// Time of the write.
    pub at_ms: u64,
    /// Events appended in the same transaction.
    pub events: Vec<NewEvent>,
}

impl RefUpdate {
    /// A single-step update with no landed entries and no events.
    pub fn set(
        repo: RepoId,
        name: RefName,
        expected: Expected,
        target: Option<ObjectId>,
        principal: impl Into<String>,
        at_ms: u64,
    ) -> RefUpdate {
        RefUpdate {
            repo,
            name,
            expected,
            steps: vec![RefStep {
                target,
                landed: Vec::new(),
            }],
            principal: principal.into(),
            at_ms,
            events: Vec::new(),
        }
    }
}

/// The result of a successful ref update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdated {
    /// Version before the update (0 when the ref never existed).
    pub old_version: u64,
    /// Version after the update.
    pub new_version: u64,
    /// Sequence numbers of the events appended with the update.
    pub event_seqs: Vec<u64>,
}

/// A ref-log row (SPEC §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefLogEntry {
    /// The repo.
    pub repo: RepoId,
    /// The ref.
    pub name: RefName,
    /// The version this entry produced.
    pub version: u64,
    /// Target before.
    pub old: Option<ObjectId>,
    /// Target after.
    pub new: Option<ObjectId>,
    /// Who wrote it.
    pub principal: String,
    /// When.
    pub at_ms: u64,
    /// Changes landed by this entry.
    pub landed: Vec<LandedEntry>,
}

/// A row of the landed index: unique per `(repo, ref, change_id)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedRow {
    /// The repo.
    pub repo: RepoId,
    /// The ref.
    pub ref_name: RefName,
    /// The change.
    pub change_id: ChangeId,
    /// The changeset that was asked to land.
    pub source: ObjectId,
    /// The changeset on the ref.
    pub result: ObjectId,
    /// The ref version whose log entry recorded it.
    pub version: u64,
}

/// A row of a repo's event log (SPEC §11.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventRow {
    /// The repo.
    pub repo: RepoId,
    /// Sequence number, from a block allocated by [`crate::MetaStore::allocate_event_seqs`].
    pub seq: u64,
    /// Event kind.
    pub kind: String,
    /// Payload.
    pub payload: Vec<u8>,
    /// Event time.
    pub at_ms: u64,
}

/// The state of a repo's event sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventHead {
    /// Highest sequence number ever allocated (0 when none).
    pub allocated: u64,
    /// Highest sequence number written (0 when none).
    pub written: u64,
}

/// A redaction (SPEC §13); its existence blocks the id forever.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedactionRow {
    /// The org.
    pub org: OrgId,
    /// The redacted blob or meta.
    pub object: ObjectId,
    /// The admin who redacted it.
    pub principal: String,
    /// Why, for audit.
    pub reason: String,
    /// When.
    pub at_ms: u64,
}

/// The result of [`crate::MetaStore::redact`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redacted {
    /// False when the object was already redacted (the call changed nothing).
    pub newly_redacted: bool,
    /// The new redaction epoch of every repo that references the object.
    pub epochs: Vec<(RepoId, u64)>,
}

/// A GC mark: the point after which any write that touches a sweep candidate defers the
/// sweep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcEpoch {
    /// The org.
    pub org: OrgId,
    /// Store-assigned, monotonically increasing mark number.
    pub seq: u64,
    /// When the mark began.
    pub at_ms: u64,
}

/// What a GC sweep proposes to delete.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepPlan {
    /// Changesets to delete, with their parent edges, `object_paths` rows of swept objects
    /// and derived rows keyed by them.
    pub changesets: Vec<(RepoId, ObjectId)>,
    /// Object rows to delete (purged tombstones are never deleted).
    pub objects: Vec<ObjectId>,
}

/// What a GC sweep did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SweepOutcome {
    /// Everything in the plan was deleted.
    Swept {
        /// Changesets deleted.
        changesets: Vec<(RepoId, ObjectId)>,
        /// Object rows deleted; their bytes may now be removed from the blob store.
        objects: Vec<ObjectId>,
    },
    /// Some candidate was touched after the mark, so nothing was deleted; run GC again.
    Deferred {
        /// Human-readable descriptions of the touched candidates.
        touched: Vec<String>,
    },
}

/// Where an immutable-row read may be served from (SPEC §6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadFrom {
    /// The primary: always current.
    Primary,
    /// A replica, which may lag. A replica that has not reached the given version forwards
    /// the read to the primary. A miss on a replica MUST be retried against the primary
    /// before answering `NOT_FOUND` (see [`crate::consistency`]).
    Replica(Option<MinVersion>),
}

/// The highest version a client has seen, for read-your-writes (SPEC §6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MinVersion {
    /// A ref version.
    Ref {
        /// The repo.
        repo: RepoId,
        /// The ref.
        name: RefName,
        /// The version.
        version: u64,
    },
    /// A repo event sequence number.
    Event {
        /// The repo.
        repo: RepoId,
        /// The sequence number.
        seq: u64,
    },
}
