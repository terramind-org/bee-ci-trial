//! The metadata store trait (SPEC §6.1): a serializable database where every method is
//! exactly one transaction.

use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use bee_core::{ChangeId, ObjectId, ObjectIdPrefix, RefName, RepoPath, Result};

use crate::counters::AccessCounters;
use crate::types::{
    ChangesetRow, DerivedRow, EventHead, EventRow, GcEpoch, LandedRow, NewChangeset, NewEvent,
    NewObject, ObjectRow, ObjectState, OrgId, OrgRow, PutOutcome, ReadFrom, Redacted, RedactionRow,
    RefLogEntry, RefRow, RefUpdate, RefUpdated, RepoId, RepoRow, SweepOutcome, SweepPlan,
    UploadRecord, UploadRow,
};

/// The metadata store.
///
/// # Contract
///
/// - **One method, one transaction.** Every method either commits all of its effects or none,
///   and is strictly serializable with every other method. Implementations retry their own
///   serialization failures (Postgres SQLSTATE 40001, SQLite `BUSY`) up to 10 times with
///   full-jitter backoff (see [`crate::retry`]) and then return `UNAVAILABLE` (retryable).
/// - **Insert-or-ignore.** Content-addressed rows (objects, changesets, edges,
///   `object_paths`, derived rows, uploads, events) are never overwritten; re-running a write
///   is harmless and repairs nothing-but-missing rows.
/// - **Write ordering** (SPEC §6.2, see the crate docs): a row that refers to blob-store
///   bytes is inserted only after those bytes are durable, and a changeset row only after
///   every object it references has a row, or together with those rows in one transaction
///   ([`MetaStore::insert_changeset_with_objects`]).
/// - **Consistency** (SPEC §6.3): refs, redactions, epochs and event counters are read from
///   the primary. Methods taking a [`ReadFrom`] read immutable rows and may be served by a
///   replica; callers retry a replica miss on the primary ([`crate::consistency`]).
/// - **Errors** are SPEC §12.2 reasons: `NOT_FOUND` for an unknown repo or org,
///   `MISSING_OBJECT` for a referenced row that does not exist, `VERSION_MISMATCH` and
///   `ALREADY_LANDED` from ref updates, `REDACTED` for blocked ids, `ALREADY_EXISTS` for a
///   name taken by another id, `INVALID_ARGUMENT` for malformed requests.
#[async_trait]
pub trait MetaStore: Send + Sync {
    // ----- orgs and repos -------------------------------------------------------------

    /// Creates an org. Idempotent for an identical row; `ALREADY_EXISTS` if the id or name
    /// is taken by a different org.
    async fn create_org(&self, org: OrgRow) -> Result<PutOutcome>;

    /// Reads an org by id.
    async fn get_org(&self, id: OrgId) -> Result<Option<OrgRow>>;

    /// Reads an org by name.
    async fn org_by_name(&self, name: &str) -> Result<Option<OrgRow>>;

    /// Creates a repo in an existing org (`NOT_FOUND` otherwise). Idempotent for an
    /// identical row; `ALREADY_EXISTS` if the id or `(org, name)` is taken.
    async fn create_repo(&self, repo: RepoRow) -> Result<PutOutcome>;

    /// Reads a repo by id.
    async fn get_repo(&self, id: RepoId) -> Result<Option<RepoRow>>;

    /// Reads a repo by name within an org.
    async fn repo_by_name(&self, org: OrgId, name: &str) -> Result<Option<RepoRow>>;

    /// Every repo of an org, ordered by id.
    async fn list_repos(&self, org: OrgId) -> Result<Vec<RepoRow>>;

    // ----- objects ---------------------------------------------------------------------

    /// Inserts object rows, insert-or-ignore, plus an upload record per object when
    /// `upload` is given. Non-inline objects' bytes MUST already be durable in the blob
    /// store. Rejects the whole batch with `REDACTED` if any id is redacted, and with
    /// `INVALID_ARGUMENT` for changeset or foreign ids, inline bytes on a kind or size that
    /// is not inlined, inline bytes that do not hash to the id, or a size that disagrees.
    /// A quarantined row becomes live again (the caller verified fresh bytes). Touches every
    /// object for GC.
    async fn put_objects(
        &self,
        org: OrgId,
        objects: Vec<NewObject>,
        upload: Option<UploadRecord>,
        at_ms: u64,
    ) -> Result<Vec<PutOutcome>>;

    /// Reads object rows; `None` for ids with no row.
    async fn get_objects(
        &self,
        org: OrgId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ObjectRow>>>;

    /// Reads one object row.
    async fn get_object(
        &self,
        org: OrgId,
        id: &ObjectId,
        read: ReadFrom,
    ) -> Result<Option<ObjectRow>> {
        Ok(self
            .get_objects(org, std::slice::from_ref(id), read)
            .await?
            .pop()
            .flatten())
    }

    /// Changes an object's state (`NOT_FOUND` without a row). Moving to or from
    /// [`ObjectState::Purged`] is reserved to [`MetaStore::redact`] (`INVALID_ARGUMENT`).
    async fn set_object_state(&self, org: OrgId, id: &ObjectId, state: ObjectState) -> Result<()>;

    /// Object rows of an org ordered by id, starting after `after`, at most `limit`.
    async fn list_objects(
        &self,
        org: OrgId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectRow>>;

    /// Upload records of one object.
    async fn get_uploads(&self, org: OrgId, id: &ObjectId) -> Result<Vec<UploadRow>>;

    /// Upload records at or after `since_ms`, ordered by object then principal.
    async fn list_uploads(&self, org: OrgId, since_ms: u64) -> Result<Vec<UploadRow>>;

    // ----- changesets ------------------------------------------------------------------

    /// Inserts a changeset with its parent edges, `object_paths` rows and eager derived rows
    /// in one transaction, all insert-or-ignore (SPEC §6.2 step 2). Every parent must have a
    /// changeset row in the repo, and the root tree, meta, every non-foreign delta put and
    /// every `object_paths` object must have an object row that is not quarantined
    /// (`MISSING_OBJECT` with the id in the payload otherwise). Sets `last_used_ms` and
    /// touches the parents and referenced objects for GC.
    async fn insert_changeset(&self, new: NewChangeset) -> Result<PutOutcome>;

    /// One new changeset with the objects it creates (SPEC §6.2 steps 1 and 2 for a
    /// CreateChangeset): inserts `objects` as [`MetaStore::put_objects`] does (with
    /// `upload` recorded for each), then `new` as [`MetaStore::insert_changeset`] does, so
    /// the changeset may refer to those objects. Non-inline objects' bytes MUST already be
    /// durable in the blob store. The SQL stores write both in one transaction (one durable
    /// commit instead of two): the changeset row and the object rows it needs become visible
    /// and durable together, so no reader sees the changeset without its objects, and a
    /// crash before the commit leaves nothing but orphan blob-store bytes. The default makes
    /// the two calls one after the other, which is the same as separate calls.
    async fn insert_changeset_with_objects(
        &self,
        org: OrgId,
        objects: Vec<NewObject>,
        upload: Option<UploadRecord>,
        at_ms: u64,
        new: NewChangeset,
    ) -> Result<PutOutcome> {
        if !objects.is_empty() {
            self.put_objects(org, objects, upload, at_ms).await?;
        }
        self.insert_changeset(new).await
    }

    /// Object rows (per org, without upload records) and then changesets, in order, with the
    /// checks and effects of [`MetaStore::put_objects`] and [`MetaStore::insert_changeset`];
    /// a changeset may refer to objects and parents written earlier in the same call. The
    /// SQL stores write everything in one transaction, so a land coalescer's batch of
    /// rebased stacks costs one durable commit instead of two per stack
    /// ([`crate::staged::StagedMeta`]). The default makes the calls one by one: each is
    /// insert-or-ignore, so a failure part-way leaves only unreferenced rows, exactly as
    /// separate calls would.
    async fn put_batch(
        &self,
        objects: Vec<(OrgId, Vec<NewObject>, u64)>,
        changesets: Vec<NewChangeset>,
    ) -> Result<()> {
        for (org, rows, at_ms) in objects {
            self.put_objects(org, rows, None, at_ms).await?;
        }
        for c in changesets {
            self.insert_changeset(c).await?;
        }
        Ok(())
    }

    /// Reads changeset rows; `None` for ids with no row in the repo.
    async fn get_changesets(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ChangesetRow>>>;

    /// The root tree id of a changeset; `None` with no row in the repo. The SQL stores
    /// read a short prefix of the row's bytes ([`bee_object::Changeset::root_from_prefix`]),
    /// so the cost does not grow with the changeset's delta (a parent with a 10,000-entry
    /// delta costs what a one-file parent costs). The default reads the whole row.
    async fn get_changeset_root(
        &self,
        repo: RepoId,
        id: &ObjectId,
        read: ReadFrom,
    ) -> Result<Option<ObjectId>> {
        Ok(self
            .get_changeset(repo, id, read)
            .await?
            .map(|row| *row.changeset.root()))
    }

    /// Reads one changeset row.
    async fn get_changeset(
        &self,
        repo: RepoId,
        id: &ObjectId,
        read: ReadFrom,
    ) -> Result<Option<ChangesetRow>> {
        Ok(self
            .get_changesets(repo, std::slice::from_ref(id), read)
            .await?
            .pop()
            .flatten())
    }

    /// Changeset rows of a repo ordered by id, after `after`, at most `limit`.
    async fn list_changesets(
        &self,
        repo: RepoId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ChangesetRow>>;

    /// Changesets of the repo carrying a ChangeId, ordered by id.
    async fn changesets_by_change_id(
        &self,
        repo: RepoId,
        change_id: &ChangeId,
    ) -> Result<Vec<ObjectId>>;

    /// Parents of a changeset from the `parent_edges` table, in order.
    async fn parents(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>>;

    /// Children of a changeset, ordered by id.
    async fn children(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>>;

    /// Changeset ids of the repo matching a prefix, ordered, at most `limit`.
    async fn changesets_with_prefix(
        &self,
        repo: RepoId,
        prefix: &ObjectIdPrefix,
        limit: usize,
    ) -> Result<Vec<ObjectId>>;

    /// Sets `last_used_ms` to at least `at_ms` for the listed changesets that exist.
    async fn touch_changesets(&self, repo: RepoId, ids: &[ObjectId], at_ms: u64) -> Result<()>;

    /// The paths at which an object appears in the repo, ordered (`None` = root).
    async fn object_paths(
        &self,
        repo: RepoId,
        object: &ObjectId,
        read: ReadFrom,
    ) -> Result<Vec<Option<RepoPath>>>;

    /// Inserts derived rows, insert-or-ignore (lazy types write back this way).
    async fn put_derived(&self, repo: RepoId, rows: Vec<DerivedRow>) -> Result<()>;

    /// Reads one derived value.
    async fn get_derived(
        &self,
        repo: RepoId,
        key: &ObjectId,
        type_name: &str,
        version: u32,
        read: ReadFrom,
    ) -> Result<Option<Vec<u8>>>;

    /// Derived rows of one type in key order, strictly after `after`, at most `limit`: the
    /// key, the type version and the value. Garbage collection reads the rows of types whose
    /// values name objects (`lfs_object`) so that those objects are roots.
    async fn list_derived_by_type(
        &self,
        repo: RepoId,
        type_name: &str,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, u32, Vec<u8>)>>;

    /// Deletes every row of one derived type (all versions) in a repo; returns how many.
    /// Purging content (SPEC §13) deletes the rows of content-bearing types this way; lazy
    /// types derive again on the next read.
    async fn delete_derived_type(&self, repo: RepoId, type_name: &str) -> Result<u64>;

    // ----- refs ------------------------------------------------------------------------

    /// Reads a ref (always strong).
    async fn get_ref(&self, repo: RepoId, name: &RefName) -> Result<Option<RefRow>>;

    /// Refs of a repo whose name starts with `prefix`, ordered by name. Deleted refs are
    /// included (with no target).
    async fn list_refs(&self, repo: RepoId, prefix: &str) -> Result<Vec<RefRow>>;

    /// Compare-and-swap (SPEC §6.2 step 3): checks the precondition (`VERSION_MISMATCH`
    /// with `current_version` in the payload), checks every target is a changeset of the
    /// repo (`MISSING_OBJECT`), writes one version and one ref-log entry per step, inserts
    /// the landed rows and appends the events. A landed key that already exists, or appears
    /// twice in the update, aborts the whole transaction with `ALREADY_LANDED` (payload:
    /// `change_id`, `existing_source`).
    async fn update_ref(&self, update: RefUpdate) -> Result<RefUpdated>;

    /// Ref-log entries from `from_version` upward, at most `limit`.
    async fn ref_log(
        &self,
        repo: RepoId,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogEntry>>;

    /// Landed rows for the listed ChangeIds on a ref, in input order.
    async fn get_landed(
        &self,
        repo: RepoId,
        name: &RefName,
        change_ids: &[ChangeId],
    ) -> Result<Vec<Option<LandedRow>>>;

    /// Every landed row of a repo (one ref, or all when `name` is `None`), ordered by ref
    /// then ChangeId.
    async fn list_landed(&self, repo: RepoId, name: Option<&RefName>) -> Result<Vec<LandedRow>>;

    // ----- events ----------------------------------------------------------------------

    /// Allocates a block of `n` sequence numbers with one counter update (SPEC §7.4 "hot
    /// counters"). Unused numbers leave gaps, which readers tolerate.
    async fn allocate_event_seqs(&self, repo: RepoId, n: u64) -> Result<Range<u64>>;

    /// Writes events under sequence numbers from allocated blocks, insert-or-ignore.
    /// `INVALID_ARGUMENT` for a number never allocated.
    async fn insert_events(&self, repo: RepoId, events: Vec<(u64, NewEvent)>) -> Result<()>;

    /// Allocates a block and writes the events in one transaction; returns their numbers.
    async fn append_events(&self, repo: RepoId, events: Vec<NewEvent>) -> Result<Vec<u64>>;

    /// Events with `seq >= from_seq`, ascending, at most `limit`.
    async fn read_events(
        &self,
        repo: RepoId,
        from_seq: u64,
        limit: usize,
        read: ReadFrom,
    ) -> Result<Vec<EventRow>>;

    /// The repo's event counters.
    async fn event_head(&self, repo: RepoId) -> Result<EventHead>;

    // ----- redaction -------------------------------------------------------------------

    /// Redacts a blob or meta (SPEC §13): records the blocklist row, sets the object's state
    /// to purged (if it has a row) and bumps the redaction epoch of every repo with an
    /// `object_paths` row for it. The caller deletes the bytes afterwards. Idempotent.
    async fn redact(&self, row: RedactionRow) -> Result<Redacted>;

    /// Redaction rows for the listed ids that are redacted.
    async fn get_redactions(&self, org: OrgId, ids: &[ObjectId]) -> Result<Vec<RedactionRow>>;

    /// Every redaction of an org, ordered by object.
    async fn list_redactions(&self, org: OrgId) -> Result<Vec<RedactionRow>>;

    /// A repo's redaction epoch (0 before any redaction).
    async fn redaction_epoch(&self, repo: RepoId) -> Result<u64>;

    // ----- GC --------------------------------------------------------------------------

    /// Records a GC mark. Any write after it that touches a sweep candidate (re-insert,
    /// use as a parent, ref target, upload, `object_paths` reference) defers that sweep.
    async fn gc_begin(&self, org: OrgId, at_ms: u64) -> Result<GcEpoch>;

    /// Deletes the planned rows in one transaction unless a candidate was touched after
    /// `epoch`, in which case nothing is deleted. Purged objects are never deleted.
    async fn gc_sweep(&self, epoch: GcEpoch, plan: SweepPlan) -> Result<SweepOutcome>;

    // ----- instrumentation ---------------------------------------------------------------

    /// Storage access counters, when the implementation keeps them (complexity gates).
    fn access_counters(&self) -> Option<Arc<AccessCounters>> {
        None
    }
}
