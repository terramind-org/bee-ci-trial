//! Decoded-object caches that keep serving during a metadata outage (PRODUCT §4.3, SPEC
//! §15.1).
//!
//! [`OutageCache`] wraps the metadata store the server reads through. Every successful read of
//! a changeset (decoded row), an object row, a derived row, parent edges, `object_paths`, an
//! org or repo record, or a redaction epoch is remembered. When the store answers with a
//! retryable `UNAVAILABLE` (the primary is down), the read is answered from memory instead:
//!
//! - immutable rows (changesets, parent edges, derived rows) at any age;
//! - rows that can change (refs, object states, path rows, org and repo records, epochs) only when
//!   fetched within `metadata.stale_read_max_ms`, the same bound as stale ref reads; past it
//!   (or with the bound at 0) the read fails as before.
//!
//! So history, trees and file reads that were served before an outage keep being served
//! while it lasts; writes still fail. Redactions and state changes made through this process
//! drop the cached rows they affect at once, and a purge drops the derived rows it deletes.
//! Each map holds at most [`MAX_ENTRIES`] rows and is cleared when it fills.

use std::collections::HashMap;
use std::hash::Hash;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bee_core::{BeeError, ChangeId, ObjectId, ObjectIdPrefix, Reason, RefName, RepoPath, Result};
use bee_store::{
    AccessCounters, ChangesetRow, Clock, DerivedRow, EventHead, EventRow, GcEpoch, LandedRow,
    MetaStore, NewChangeset, NewEvent, NewObject, ObjectRow, ObjectState, OrgId, OrgRow,
    PutOutcome, ReadFrom, Redacted, RedactionRow, RefLogEntry, RefRow, RefUpdate, RefUpdated,
    RepoId, RepoRow, SweepOutcome, SweepPlan, UploadRecord, UploadRow,
};

/// Rows kept per map at most.
pub const MAX_ENTRIES: usize = 200_000;

type Map<K, V> = Mutex<HashMap<K, (V, u64)>>;

/// The read-through cache over a metadata store.
pub struct OutageCache {
    inner: Arc<dyn MetaStore>,
    clock: Arc<dyn Clock>,
    stale_max_ms: u64,
    changesets: Map<(RepoId, ObjectId), ChangesetRow>,
    objects: Map<(OrgId, ObjectId), ObjectRow>,
    derived: Map<(RepoId, ObjectId, String, u32), Vec<u8>>,
    parents: Map<(RepoId, ObjectId), Vec<ObjectId>>,
    paths: Map<(RepoId, ObjectId), Vec<Option<RepoPath>>>,
    orgs: Map<String, Option<OrgRow>>,
    orgs_by_name: Map<String, Option<OrgRow>>,
    repos: Map<String, Option<RepoRow>>,
    repos_by_name: Map<String, Option<RepoRow>>,
    epochs: Map<String, u64>,
    refs: Map<(RepoId, String), Option<RefRow>>,
    redactions: Map<(OrgId, ObjectId), Option<RedactionRow>>,
}

tokio::task_local! {
    /// Set when a ref read in this scope was answered from memory during an outage.
    pub static SERVED_STALE: std::cell::Cell<bool>;
}

/// True for the failures an outage cache answers: the store is unreachable.
fn serves(e: &BeeError) -> bool {
    e.reason == Reason::Unavailable
}

impl OutageCache {
    /// Wraps `inner`; mutable rows are served for at most `stale_max_ms` after they were read.
    pub fn new(inner: Arc<dyn MetaStore>, clock: Arc<dyn Clock>, stale_max_ms: u64) -> OutageCache {
        OutageCache {
            inner,
            clock,
            stale_max_ms,
            changesets: Mutex::default(),
            objects: Mutex::default(),
            derived: Mutex::default(),
            parents: Mutex::default(),
            paths: Mutex::default(),
            orgs: Mutex::default(),
            orgs_by_name: Mutex::default(),
            repos: Mutex::default(),
            repos_by_name: Mutex::default(),
            epochs: Mutex::default(),
            refs: Mutex::default(),
            redactions: Mutex::default(),
        }
    }

    fn remember<K: Eq + Hash, V>(&self, map: &Map<K, V>, k: K, v: V) {
        let mut m = map.lock().unwrap_or_else(PoisonError::into_inner);
        if m.len() >= MAX_ENTRIES {
            m.clear();
        }
        m.insert(k, (v, self.clock.now_ms()));
    }

    /// The remembered value; for `mutable` rows only within the staleness bound.
    fn recall<K: Eq + Hash, V: Clone>(&self, map: &Map<K, V>, k: &K, mutable: bool) -> Option<V> {
        let m = map.lock().unwrap_or_else(PoisonError::into_inner);
        let (v, at) = m.get(k)?;
        if mutable
            && (self.stale_max_ms == 0
                || self.clock.now_ms().saturating_sub(*at) > self.stale_max_ms)
        {
            return None;
        }
        Some(v.clone())
    }

    fn forget<K: Eq + Hash, V>(&self, map: &Map<K, V>, k: &K) {
        map.lock().unwrap_or_else(PoisonError::into_inner).remove(k);
    }

    fn through<K: Eq + Hash, V: Clone>(
        &self,
        map: &Map<K, V>,
        k: K,
        r: Result<V>,
        mutable: bool,
    ) -> Result<V> {
        match r {
            Ok(v) => {
                self.remember(map, k, v.clone());
                Ok(v)
            }
            Err(e) if serves(&e) => self.recall(map, &k, mutable).ok_or(e),
            Err(e) => Err(e),
        }
    }

    fn through_vec<K: Eq + Hash, V: Clone>(
        &self,
        map: &Map<K, Vec<V>>,
        k: K,
        r: Result<Vec<V>>,
        mutable: bool,
    ) -> Result<Vec<V>> {
        self.through(map, k, r, mutable)
    }
}

#[async_trait]
impl MetaStore for OutageCache {
    async fn create_org(&self, org: OrgRow) -> Result<PutOutcome> {
        self.inner.create_org(org).await
    }
    async fn get_org(&self, id: OrgId) -> Result<Option<OrgRow>> {
        let r = self.inner.get_org(id).await;
        self.through(&self.orgs, id.0.to_text(), r, true)
    }
    async fn org_by_name(&self, name: &str) -> Result<Option<OrgRow>> {
        let r = self.inner.org_by_name(name).await;
        self.through(&self.orgs_by_name, name.to_string(), r, true)
    }
    async fn create_repo(&self, repo: RepoRow) -> Result<PutOutcome> {
        self.inner.create_repo(repo).await
    }
    async fn get_repo(&self, id: RepoId) -> Result<Option<RepoRow>> {
        let r = self.inner.get_repo(id).await;
        self.through(&self.repos, id.0.to_text(), r, true)
    }
    async fn repo_by_name(&self, org: OrgId, name: &str) -> Result<Option<RepoRow>> {
        let r = self.inner.repo_by_name(org, name).await;
        self.through(
            &self.repos_by_name,
            format!("{}/{name}", org.0.to_text()),
            r,
            true,
        )
    }
    async fn list_repos(&self, org: OrgId) -> Result<Vec<RepoRow>> {
        self.inner.list_repos(org).await
    }
    async fn put_objects(
        &self,
        org: OrgId,
        objects: Vec<NewObject>,
        upload: Option<UploadRecord>,
        at_ms: u64,
    ) -> Result<Vec<PutOutcome>> {
        self.inner.put_objects(org, objects, upload, at_ms).await
    }
    async fn get_objects(
        &self,
        org: OrgId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ObjectRow>>> {
        match self.inner.get_objects(org, ids, read).await {
            Ok(rows) => {
                for (id, row) in ids.iter().zip(&rows) {
                    if let Some(row) = row {
                        self.remember(&self.objects, (org, *id), row.clone());
                    }
                }
                Ok(rows)
            }
            Err(e) if serves(&e) => ids
                .iter()
                .map(|id| self.recall(&self.objects, &(org, *id), true).map(Some))
                .collect::<Option<Vec<_>>>()
                .ok_or(e),
            Err(e) => Err(e),
        }
    }
    async fn set_object_state(&self, org: OrgId, id: &ObjectId, state: ObjectState) -> Result<()> {
        self.forget(&self.objects, &(org, *id));
        self.inner.set_object_state(org, id, state).await
    }
    async fn list_objects(
        &self,
        org: OrgId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectRow>> {
        self.inner.list_objects(org, after, limit).await
    }
    async fn get_uploads(&self, org: OrgId, id: &ObjectId) -> Result<Vec<UploadRow>> {
        self.inner.get_uploads(org, id).await
    }
    async fn list_uploads(&self, org: OrgId, since_ms: u64) -> Result<Vec<UploadRow>> {
        self.inner.list_uploads(org, since_ms).await
    }
    async fn insert_changeset(&self, new: NewChangeset) -> Result<PutOutcome> {
        self.inner.insert_changeset(new).await
    }
    async fn put_batch(
        &self,
        objects: Vec<(OrgId, Vec<NewObject>, u64)>,
        changesets: Vec<NewChangeset>,
    ) -> Result<()> {
        self.inner.put_batch(objects, changesets).await
    }
    async fn insert_changeset_with_objects(
        &self,
        org: OrgId,
        objects: Vec<NewObject>,
        upload: Option<UploadRecord>,
        at_ms: u64,
        new: NewChangeset,
    ) -> Result<PutOutcome> {
        self.inner
            .insert_changeset_with_objects(org, objects, upload, at_ms, new)
            .await
    }
    // Forwarded so the store's prefix read is used (the default would decode the whole row
    // through `get_changesets`); during an outage a remembered row answers.
    async fn get_changeset_root(
        &self,
        repo: RepoId,
        id: &ObjectId,
        read: ReadFrom,
    ) -> Result<Option<ObjectId>> {
        match self.inner.get_changeset_root(repo, id, read).await {
            Err(e) if serves(&e) => self
                .recall(&self.changesets, &(repo, *id), false)
                .map(|row| Some(*row.changeset.root()))
                .ok_or(e),
            r => r,
        }
    }
    async fn get_changesets(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        match self.inner.get_changesets(repo, ids, read).await {
            Ok(rows) => {
                for (id, row) in ids.iter().zip(&rows) {
                    if let Some(row) = row {
                        self.remember(&self.changesets, (repo, *id), row.clone());
                    }
                }
                Ok(rows)
            }
            // Changeset rows are immutable: served at any age.
            Err(e) if serves(&e) => ids
                .iter()
                .map(|id| self.recall(&self.changesets, &(repo, *id), false).map(Some))
                .collect::<Option<Vec<_>>>()
                .ok_or(e),
            Err(e) => Err(e),
        }
    }
    async fn list_changesets(
        &self,
        repo: RepoId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ChangesetRow>> {
        self.inner.list_changesets(repo, after, limit).await
    }
    async fn changesets_by_change_id(
        &self,
        repo: RepoId,
        change_id: &ChangeId,
    ) -> Result<Vec<ObjectId>> {
        self.inner.changesets_by_change_id(repo, change_id).await
    }
    async fn parents(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>> {
        let r = self.inner.parents(repo, id).await;
        self.through_vec(&self.parents, (repo, *id), r, false)
    }
    async fn children(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>> {
        self.inner.children(repo, id).await
    }
    async fn changesets_with_prefix(
        &self,
        repo: RepoId,
        prefix: &ObjectIdPrefix,
        limit: usize,
    ) -> Result<Vec<ObjectId>> {
        self.inner.changesets_with_prefix(repo, prefix, limit).await
    }
    async fn touch_changesets(&self, repo: RepoId, ids: &[ObjectId], at_ms: u64) -> Result<()> {
        self.inner.touch_changesets(repo, ids, at_ms).await
    }
    async fn object_paths(
        &self,
        repo: RepoId,
        object: &ObjectId,
        read: ReadFrom,
    ) -> Result<Vec<Option<RepoPath>>> {
        let r = self.inner.object_paths(repo, object, read).await;
        self.through_vec(&self.paths, (repo, *object), r, true)
    }
    async fn put_derived(&self, repo: RepoId, rows: Vec<DerivedRow>) -> Result<()> {
        self.inner.put_derived(repo, rows).await
    }
    async fn get_derived(
        &self,
        repo: RepoId,
        key: &ObjectId,
        type_name: &str,
        version: u32,
        read: ReadFrom,
    ) -> Result<Option<Vec<u8>>> {
        let k = (repo, *key, type_name.to_string(), version);
        match self
            .inner
            .get_derived(repo, key, type_name, version, read)
            .await
        {
            Ok(Some(v)) => {
                self.remember(&self.derived, k, v.clone());
                Ok(Some(v))
            }
            Ok(None) => Ok(None),
            // Derived rows are insert-only (purges delete through this cache).
            Err(e) if serves(&e) => self.recall(&self.derived, &k, false).map(Some).ok_or(e),
            Err(e) => Err(e),
        }
    }
    async fn list_derived_by_type(
        &self,
        repo: RepoId,
        type_name: &str,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, u32, Vec<u8>)>> {
        self.inner
            .list_derived_by_type(repo, type_name, after, limit)
            .await
    }
    async fn delete_derived_type(&self, repo: RepoId, type_name: &str) -> Result<u64> {
        self.derived
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(r, _, t, _), _| !(*r == repo && t == type_name));
        self.inner.delete_derived_type(repo, type_name).await
    }
    async fn get_ref(&self, repo: RepoId, name: &RefName) -> Result<Option<RefRow>> {
        let k = (repo, name.as_str().to_string());
        match self.inner.get_ref(repo, name).await {
            Ok(row) => {
                self.remember(&self.refs, k, row.clone());
                Ok(row)
            }
            Err(e) if serves(&e) => {
                let row = self.recall(&self.refs, &k, true).ok_or(e)?;
                // Callers that report staleness (`AppState::read_ref`) learn it was served
                // from memory.
                let _ = SERVED_STALE.try_with(|c| c.set(true));
                Ok(row)
            }
            Err(e) => Err(e),
        }
    }
    async fn list_refs(&self, repo: RepoId, prefix: &str) -> Result<Vec<RefRow>> {
        self.inner.list_refs(repo, prefix).await
    }
    async fn update_ref(&self, update: RefUpdate) -> Result<RefUpdated> {
        self.inner.update_ref(update).await
    }
    async fn ref_log(
        &self,
        repo: RepoId,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogEntry>> {
        self.inner.ref_log(repo, name, from_version, limit).await
    }
    async fn get_landed(
        &self,
        repo: RepoId,
        name: &RefName,
        change_ids: &[ChangeId],
    ) -> Result<Vec<Option<LandedRow>>> {
        self.inner.get_landed(repo, name, change_ids).await
    }
    async fn list_landed(&self, repo: RepoId, name: Option<&RefName>) -> Result<Vec<LandedRow>> {
        self.inner.list_landed(repo, name).await
    }
    async fn allocate_event_seqs(&self, repo: RepoId, n: u64) -> Result<Range<u64>> {
        self.inner.allocate_event_seqs(repo, n).await
    }
    async fn insert_events(&self, repo: RepoId, events: Vec<(u64, NewEvent)>) -> Result<()> {
        self.inner.insert_events(repo, events).await
    }
    async fn append_events(&self, repo: RepoId, events: Vec<NewEvent>) -> Result<Vec<u64>> {
        self.inner.append_events(repo, events).await
    }
    async fn read_events(
        &self,
        repo: RepoId,
        from_seq: u64,
        limit: usize,
        read: ReadFrom,
    ) -> Result<Vec<EventRow>> {
        self.inner.read_events(repo, from_seq, limit, read).await
    }
    async fn event_head(&self, repo: RepoId) -> Result<EventHead> {
        self.inner.event_head(repo).await
    }
    async fn redact(&self, row: RedactionRow) -> Result<Redacted> {
        self.forget(&self.objects, &(row.org, row.object));
        self.forget(&self.redactions, &(row.org, row.object));
        self.inner.redact(row).await
    }
    async fn get_redactions(&self, org: OrgId, ids: &[ObjectId]) -> Result<Vec<RedactionRow>> {
        match self.inner.get_redactions(org, ids).await {
            Ok(rows) => {
                for id in ids {
                    let row = rows.iter().find(|r| r.object == *id).cloned();
                    self.remember(&self.redactions, (org, *id), row);
                }
                Ok(rows)
            }
            // Redactions can be added at any time: within the staleness bound only.
            Err(e) if serves(&e) => {
                let mut out = Vec::new();
                for id in ids {
                    match self.recall(&self.redactions, &(org, *id), true) {
                        Some(Some(row)) => out.push(row),
                        Some(None) => {}
                        None => return Err(e),
                    }
                }
                Ok(out)
            }
            Err(e) => Err(e),
        }
    }
    async fn list_redactions(&self, org: OrgId) -> Result<Vec<RedactionRow>> {
        self.inner.list_redactions(org).await
    }
    async fn redaction_epoch(&self, repo: RepoId) -> Result<u64> {
        let r = self.inner.redaction_epoch(repo).await;
        self.through(&self.epochs, repo.0.to_text(), r, true)
    }
    async fn gc_begin(&self, org: OrgId, at_ms: u64) -> Result<GcEpoch> {
        self.inner.gc_begin(org, at_ms).await
    }
    async fn gc_sweep(&self, epoch: GcEpoch, plan: SweepPlan) -> Result<SweepOutcome> {
        self.inner.gc_sweep(epoch, plan).await
    }
    fn access_counters(&self) -> Option<Arc<AccessCounters>> {
        self.inner.access_counters()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_store::{Fault, FaultInjector, FaultRule, FaultyMeta, ManualClock, MemoryMetaStore};

    #[tokio::test]
    async fn serves_remembered_rows_while_the_store_is_down() {
        let faults = FaultInjector::new();
        let clock = Arc::new(ManualClock::new(1_000));
        let mem = MemoryMetaStore::new();
        let org = OrgRow {
            id: OrgId(bee_core::Ulid::from_bytes([1; 16])),
            name: "acme".into(),
            created_at_ms: 0,
        };
        mem.create_org(org.clone()).await.unwrap();
        let cache = OutageCache::new(
            Arc::new(FaultyMeta::new(mem, faults.clone())),
            clock.clone(),
            500,
        );
        assert_eq!(cache.org_by_name("acme").await.unwrap(), Some(org.clone()));
        faults.add(FaultRule::always("meta.*", Fault::unavailable()));
        // Within the bound: the remembered row.
        assert_eq!(cache.org_by_name("acme").await.unwrap(), Some(org.clone()));
        // Never read: the outage shows.
        let e = cache.org_by_name("other").await.unwrap_err();
        assert_eq!(e.reason, Reason::Unavailable);
        // Past the bound a mutable row is no longer served.
        clock.advance(501);
        assert!(cache.org_by_name("acme").await.is_err());
        // Other errors pass through untouched.
        faults.clear_rules();
        faults.add(FaultRule::always(
            "meta.*",
            Fault::Error(BeeError::new(Reason::Internal, "boom")),
        ));
        assert_eq!(
            cache.org_by_name("acme").await.unwrap_err().reason,
            Reason::Internal
        );
    }
}
