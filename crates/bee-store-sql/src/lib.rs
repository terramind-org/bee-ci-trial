//! The metadata store ([`bee_store::MetaStore`]) on SQL databases (SPEC §6):
//! [`PgMetaStore`] on Postgres 15+ for production and [`SqliteMetaStore`] on SQLite for dev
//! mode.
//!
//! Both share one implementation of the store logic and one versioned migration set
//! ([`migrate`]), applied when a store opens. The schema follows `bee_store::types`: every
//! key starts with the org's 64-bit surrogate key, then the repo's; ids are stored as bytes;
//! trees and metas of at most 64 KiB are inline in their `objects` row.
//!
//! - **One method, one transaction**, retried as a whole on serialization failures
//!   (Postgres SQLSTATE 40001/40P01, SQLite `BUSY`) under
//!   [`bee_store::retry::RetryPolicy::TRANSACTION`], then `UNAVAILABLE`.
//! - **Ref CAS** runs SERIALIZABLE on Postgres and locks the ref row `FOR UPDATE` before the
//!   version check; one update of K steps writes K versions, K ref-log rows, the landed rows
//!   and its events in one transaction (the land coalescer, SPEC §7.4).
//! - **Insert-or-ignore** uses `ON CONFLICT DO NOTHING`, batched with `unnest` arrays on
//!   Postgres.
//! - **Event sequence numbers** come in blocks from one counter update (SPEC §11.7).
//! - **Replica reads** (`ReadFrom::Replica`) go to an optional read replica; with a
//!   `min_version` the store waits up to `read_wait` for the replica and otherwise reads the
//!   primary (SPEC §6.3).
//! - **Deadlines** bound every statement (Postgres `statement_timeout` plus a client timer;
//!   SQLite progress-handler interrupts and a bounded busy timeout).
//! - **Access counters** count rows read and written per table (SPEC §14.6); writes count only
//!   when their transaction commits.

use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use bee_core::{ChangeId, ObjectId, ObjectIdPrefix, RefName, RepoPath, Result};
use bee_store::{
    AccessCounters, ChangesetRow, DerivedRow, EventHead, EventRow, GcEpoch, LandedRow, MetaStore,
    NewChangeset, NewEvent, NewObject, ObjectRow, ObjectState, OrgId, OrgRow, PutOutcome, ReadFrom,
    Redacted, RedactionRow, RefLogEntry, RefRow, RefUpdate, RefUpdated, RepoId, RepoRow,
    SweepOutcome, SweepPlan, UploadRecord, UploadRow,
};

mod backend;
mod codec;
pub mod migrate;
mod pg;
mod sqlite;
mod store;

pub use pg::PgOptions;
pub use sqlite::SqliteOptions;

use backend::Fail;
use store::Core;

fn opened(r: std::result::Result<(), Fail>) -> Result<()> {
    r.map_err(|f| match f {
        Fail::Retry(e) | Fail::Err(e) => e,
    })
}

/// The metadata store on Postgres 15+ (production).
pub struct PgMetaStore {
    core: Core<pg::Pg>,
}

impl PgMetaStore {
    /// Connects to the primary (and replica, if configured), applies pending migrations and
    /// returns the store, counting into fresh [`AccessCounters`].
    pub async fn connect(opts: PgOptions) -> Result<PgMetaStore> {
        PgMetaStore::connect_with_counters(opts, Arc::new(AccessCounters::new())).await
    }

    /// Like [`PgMetaStore::connect`], counting into `counters` (share them with a blob store).
    pub async fn connect_with_counters(
        opts: PgOptions,
        counters: Arc<AccessCounters>,
    ) -> Result<PgMetaStore> {
        let mut db = None;
        opened(async { pg::Pg::open(&opts).await.map(|p| db = Some(p)) }.await)?;
        let db = db.ok_or_else(|| {
            bee_core::BeeError::new(bee_core::Reason::Internal, "Postgres did not open")
        })?;
        Ok(PgMetaStore {
            core: Core::new(db, counters, opts.read_wait),
        })
    }

    /// The access counters.
    pub fn counters(&self) -> &Arc<AccessCounters> {
        self.core.counters()
    }
}

/// The metadata store on SQLite (dev mode).
pub struct SqliteMetaStore {
    core: Core<sqlite::Lite>,
}

impl SqliteMetaStore {
    /// Opens (creating if needed) the database, applies pending migrations and returns the
    /// store, counting into fresh [`AccessCounters`].
    pub async fn open(opts: SqliteOptions) -> Result<SqliteMetaStore> {
        SqliteMetaStore::open_with_counters(opts, Arc::new(AccessCounters::new())).await
    }

    /// Like [`SqliteMetaStore::open`], counting into `counters`.
    pub async fn open_with_counters(
        opts: SqliteOptions,
        counters: Arc<AccessCounters>,
    ) -> Result<SqliteMetaStore> {
        let mut db = None;
        opened(async { sqlite::Lite::open(&opts).await.map(|l| db = Some(l)) }.await)?;
        let db = db.ok_or_else(|| {
            bee_core::BeeError::new(bee_core::Reason::Internal, "SQLite did not open")
        })?;
        Ok(SqliteMetaStore {
            core: Core::new(db, counters, opts.read_wait),
        })
    }

    /// The access counters.
    pub fn counters(&self) -> &Arc<AccessCounters> {
        self.core.counters()
    }
}

/// Implements [`MetaStore`] for a store type by delegating to its shared core.
macro_rules! delegate_meta_store {
    ($t:ty) => {
        #[async_trait]
        impl MetaStore for $t {
            async fn create_org(&self, org: OrgRow) -> Result<PutOutcome> {
                self.core.create_org(org).await
            }
            async fn get_org(&self, id: OrgId) -> Result<Option<OrgRow>> {
                self.core.get_org(id).await
            }
            async fn org_by_name(&self, name: &str) -> Result<Option<OrgRow>> {
                self.core.org_by_name(name).await
            }
            async fn create_repo(&self, repo: RepoRow) -> Result<PutOutcome> {
                self.core.create_repo(repo).await
            }
            async fn get_repo(&self, id: RepoId) -> Result<Option<RepoRow>> {
                self.core.get_repo(id).await
            }
            async fn repo_by_name(&self, org: OrgId, name: &str) -> Result<Option<RepoRow>> {
                self.core.repo_by_name(org, name).await
            }
            async fn list_repos(&self, org: OrgId) -> Result<Vec<RepoRow>> {
                self.core.list_repos(org).await
            }
            async fn put_objects(
                &self,
                org: OrgId,
                objects: Vec<NewObject>,
                upload: Option<UploadRecord>,
                at_ms: u64,
            ) -> Result<Vec<PutOutcome>> {
                self.core.put_objects(org, objects, upload, at_ms).await
            }
            async fn get_objects(
                &self,
                org: OrgId,
                ids: &[ObjectId],
                read: ReadFrom,
            ) -> Result<Vec<Option<ObjectRow>>> {
                self.core.get_objects(org, ids, read).await
            }
            async fn set_object_state(
                &self,
                org: OrgId,
                id: &ObjectId,
                state: ObjectState,
            ) -> Result<()> {
                self.core.set_object_state(org, id, state).await
            }
            async fn list_objects(
                &self,
                org: OrgId,
                after: Option<ObjectId>,
                limit: usize,
            ) -> Result<Vec<ObjectRow>> {
                self.core.list_objects(org, after, limit).await
            }
            async fn get_uploads(&self, org: OrgId, id: &ObjectId) -> Result<Vec<UploadRow>> {
                self.core.get_uploads(org, id).await
            }
            async fn list_uploads(&self, org: OrgId, since_ms: u64) -> Result<Vec<UploadRow>> {
                self.core.list_uploads(org, since_ms).await
            }
            async fn insert_changeset(&self, new: NewChangeset) -> Result<PutOutcome> {
                self.core.insert_changeset(new).await
            }
            async fn put_batch(
                &self,
                objects: Vec<(OrgId, Vec<NewObject>, u64)>,
                changesets: Vec<NewChangeset>,
            ) -> Result<()> {
                self.core.put_batch(objects, changesets).await
            }
            async fn insert_changeset_with_objects(
                &self,
                org: OrgId,
                objects: Vec<NewObject>,
                upload: Option<UploadRecord>,
                at_ms: u64,
                new: NewChangeset,
            ) -> Result<PutOutcome> {
                self.core
                    .insert_changeset_with_objects(org, objects, upload, at_ms, new)
                    .await
            }
            async fn get_changeset_root(
                &self,
                repo: RepoId,
                id: &ObjectId,
                read: ReadFrom,
            ) -> Result<Option<ObjectId>> {
                self.core.get_changeset_root(repo, id, read).await
            }
            async fn get_changesets(
                &self,
                repo: RepoId,
                ids: &[ObjectId],
                read: ReadFrom,
            ) -> Result<Vec<Option<ChangesetRow>>> {
                self.core.get_changesets(repo, ids, read).await
            }
            async fn list_changesets(
                &self,
                repo: RepoId,
                after: Option<ObjectId>,
                limit: usize,
            ) -> Result<Vec<ChangesetRow>> {
                self.core.list_changesets(repo, after, limit).await
            }
            async fn changesets_by_change_id(
                &self,
                repo: RepoId,
                change_id: &ChangeId,
            ) -> Result<Vec<ObjectId>> {
                self.core.changesets_by_change_id(repo, change_id).await
            }
            async fn parents(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>> {
                self.core.parents(repo, id).await
            }
            async fn children(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>> {
                self.core.children(repo, id).await
            }
            async fn changesets_with_prefix(
                &self,
                repo: RepoId,
                prefix: &ObjectIdPrefix,
                limit: usize,
            ) -> Result<Vec<ObjectId>> {
                self.core.changesets_with_prefix(repo, prefix, limit).await
            }
            async fn touch_changesets(
                &self,
                repo: RepoId,
                ids: &[ObjectId],
                at_ms: u64,
            ) -> Result<()> {
                self.core.touch_changesets(repo, ids, at_ms).await
            }
            async fn object_paths(
                &self,
                repo: RepoId,
                object: &ObjectId,
                read: ReadFrom,
            ) -> Result<Vec<Option<RepoPath>>> {
                self.core.object_paths(repo, object, read).await
            }
            async fn put_derived(&self, repo: RepoId, rows: Vec<DerivedRow>) -> Result<()> {
                self.core.put_derived(repo, rows).await
            }
            async fn get_derived(
                &self,
                repo: RepoId,
                key: &ObjectId,
                type_name: &str,
                version: u32,
                read: ReadFrom,
            ) -> Result<Option<Vec<u8>>> {
                self.core
                    .get_derived(repo, key, type_name, version, read)
                    .await
            }
            async fn list_derived_by_type(
                &self,
                repo: RepoId,
                type_name: &str,
                after: Option<ObjectId>,
                limit: usize,
            ) -> Result<Vec<(ObjectId, u32, Vec<u8>)>> {
                self.core
                    .list_derived_by_type(repo, type_name, after, limit)
                    .await
            }
            async fn delete_derived_type(&self, repo: RepoId, type_name: &str) -> Result<u64> {
                self.core.delete_derived_type(repo, type_name).await
            }
            async fn get_ref(&self, repo: RepoId, name: &RefName) -> Result<Option<RefRow>> {
                self.core.get_ref(repo, name).await
            }
            async fn list_refs(&self, repo: RepoId, prefix: &str) -> Result<Vec<RefRow>> {
                self.core.list_refs(repo, prefix).await
            }
            async fn update_ref(&self, update: RefUpdate) -> Result<RefUpdated> {
                self.core.update_ref(update).await
            }
            async fn ref_log(
                &self,
                repo: RepoId,
                name: &RefName,
                from_version: u64,
                limit: usize,
            ) -> Result<Vec<RefLogEntry>> {
                self.core.ref_log(repo, name, from_version, limit).await
            }
            async fn get_landed(
                &self,
                repo: RepoId,
                name: &RefName,
                change_ids: &[ChangeId],
            ) -> Result<Vec<Option<LandedRow>>> {
                self.core.get_landed(repo, name, change_ids).await
            }
            async fn list_landed(
                &self,
                repo: RepoId,
                name: Option<&RefName>,
            ) -> Result<Vec<LandedRow>> {
                self.core.list_landed(repo, name).await
            }
            async fn allocate_event_seqs(&self, repo: RepoId, n: u64) -> Result<Range<u64>> {
                self.core.allocate_event_seqs(repo, n).await
            }
            async fn insert_events(
                &self,
                repo: RepoId,
                events: Vec<(u64, NewEvent)>,
            ) -> Result<()> {
                self.core.insert_events(repo, events).await
            }
            async fn append_events(&self, repo: RepoId, events: Vec<NewEvent>) -> Result<Vec<u64>> {
                self.core.append_events(repo, events).await
            }
            async fn read_events(
                &self,
                repo: RepoId,
                from_seq: u64,
                limit: usize,
                read: ReadFrom,
            ) -> Result<Vec<EventRow>> {
                self.core.read_events(repo, from_seq, limit, read).await
            }
            async fn event_head(&self, repo: RepoId) -> Result<EventHead> {
                self.core.event_head(repo).await
            }
            async fn redact(&self, row: RedactionRow) -> Result<Redacted> {
                self.core.redact(row).await
            }
            async fn get_redactions(
                &self,
                org: OrgId,
                ids: &[ObjectId],
            ) -> Result<Vec<RedactionRow>> {
                self.core.get_redactions(org, ids).await
            }
            async fn list_redactions(&self, org: OrgId) -> Result<Vec<RedactionRow>> {
                self.core.list_redactions(org).await
            }
            async fn redaction_epoch(&self, repo: RepoId) -> Result<u64> {
                self.core.redaction_epoch(repo).await
            }
            async fn gc_begin(&self, org: OrgId, at_ms: u64) -> Result<GcEpoch> {
                self.core.gc_begin(org, at_ms).await
            }
            async fn gc_sweep(&self, epoch: GcEpoch, plan: SweepPlan) -> Result<SweepOutcome> {
                self.core.gc_sweep(epoch, plan).await
            }
            fn access_counters(&self) -> Option<Arc<AccessCounters>> {
                Some(self.core.counters().clone())
            }
        }
    };
}

delegate_meta_store!(PgMetaStore);
delegate_meta_store!(SqliteMetaStore);
