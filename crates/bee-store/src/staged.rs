//! A write buffer over a metadata store: object rows and changesets are held in memory,
//! served to every read through this handle as if written, and written together by
//! [`StagedMeta::flush`] in one [`MetaStore::put_batch`].
//!
//! The land coalescer rebases many stacks in a batch, each onto the previous stack's result,
//! before one ref compare-and-swap. Written one by one, every rebased stack costs two durable
//! commits (its trees, then its changeset row) that the next stack reads back. Over a
//! [`StagedMeta`] (see [`crate::Writer::buffered`]) the batch costs one commit for all its
//! rows, then the CAS. Write ordering (SPEC §6.2) holds: blob-store bytes are still written
//! through immediately and durably, the rows are flushed before the ref update that makes
//! them reachable, and every write is insert-or-ignore, so a failed flush or CAS leaves only
//! unreferenced rows. What gets computed does not depend on the buffer, so results stay
//! deterministic.
//!
//! Only rows without an upload record are buffered; every other method passes through.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bee_core::{ChangeId, ObjectId, ObjectIdPrefix, RefName, RepoPath, Result};

use crate::counters::AccessCounters;
use crate::meta::MetaStore;
use crate::types::{
    ChangesetRow, DerivedRow, EventHead, EventRow, GcEpoch, LandedRow, NewChangeset, NewEvent,
    NewObject, ObjectRow, ObjectState, OrgId, OrgRow, PutOutcome, ReadFrom, Redacted, RedactionRow,
    RefLogEntry, RefRow, RefUpdate, RefUpdated, RepoId, RepoRow, SweepOutcome, SweepPlan,
    UploadRecord, UploadRow,
};

#[derive(Default)]
struct Buffer {
    /// Object puts in call order, with their time.
    puts: Vec<(OrgId, Vec<NewObject>, u64)>,
    /// Buffered object rows by id.
    objects: HashMap<(OrgId, ObjectId), ObjectRow>,
    /// Changesets in insertion order.
    changesets: Vec<NewChangeset>,
    /// Index into `changesets` by (repo, id).
    by_id: HashMap<(RepoId, ObjectId), usize>,
    /// Eager derived rows of buffered changesets.
    derived: BTreeMap<(RepoId, ObjectId, String, u32), Vec<u8>>,
}

/// A metadata store whose object and changeset writes are buffered until
/// [`StagedMeta::flush`].
pub struct StagedMeta {
    inner: Arc<dyn MetaStore>,
    buf: Mutex<Buffer>,
}

impl StagedMeta {
    /// An empty buffer over `inner`.
    pub fn new(inner: Arc<dyn MetaStore>) -> StagedMeta {
        StagedMeta {
            inner,
            buf: Mutex::new(Buffer::default()),
        }
    }

    fn buf(&self) -> std::sync::MutexGuard<'_, Buffer> {
        self.buf.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changesets buffered and not yet flushed.
    pub fn pending_changesets(&self) -> usize {
        self.buf().changesets.len()
    }

    /// Writes every buffered row with one [`MetaStore::put_batch`] and empties the buffer.
    /// Nothing to write is a no-op. On failure the buffer is kept, so a retry writes the
    /// same rows again (insert-or-ignore).
    pub async fn flush(&self) -> Result<()> {
        let (puts, changesets) = {
            let b = self.buf();
            (b.puts.clone(), b.changesets.clone())
        };
        if puts.is_empty() && changesets.is_empty() {
            return Ok(());
        }
        self.inner.put_batch(puts, changesets).await?;
        *self.buf() = Buffer::default();
        Ok(())
    }

    fn changeset_row(new: &NewChangeset) -> ChangesetRow {
        let bytes = new.changeset.encode();
        ChangesetRow {
            repo: new.repo,
            id: new.changeset.id(),
            bytes,
            changeset: new.changeset.clone(),
            created_at_ms: new.at_ms,
            last_used_ms: new.at_ms,
        }
    }
}

#[async_trait]
impl MetaStore for StagedMeta {
    async fn create_org(&self, org: OrgRow) -> Result<PutOutcome> {
        self.inner.create_org(org).await
    }
    async fn get_org(&self, id: OrgId) -> Result<Option<OrgRow>> {
        self.inner.get_org(id).await
    }
    async fn org_by_name(&self, name: &str) -> Result<Option<OrgRow>> {
        self.inner.org_by_name(name).await
    }
    async fn create_repo(&self, repo: RepoRow) -> Result<PutOutcome> {
        self.inner.create_repo(repo).await
    }
    async fn get_repo(&self, id: RepoId) -> Result<Option<RepoRow>> {
        self.inner.get_repo(id).await
    }
    async fn repo_by_name(&self, org: OrgId, name: &str) -> Result<Option<RepoRow>> {
        self.inner.repo_by_name(org, name).await
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
        if upload.is_some() {
            return self.inner.put_objects(org, objects, upload, at_ms).await;
        }
        let mut b = self.buf();
        let mut out = Vec::with_capacity(objects.len());
        for o in &objects {
            let fresh = !b.objects.contains_key(&(org, o.id));
            if fresh {
                b.objects.insert(
                    (org, o.id),
                    ObjectRow {
                        org,
                        id: o.id,
                        size: o.size,
                        state: ObjectState::Live,
                        inline: o.inline.clone(),
                        created_at_ms: at_ms,
                    },
                );
            }
            out.push(if fresh {
                PutOutcome::Inserted
            } else {
                PutOutcome::AlreadyPresent
            });
        }
        b.puts.push((org, objects, at_ms));
        Ok(out)
    }
    async fn get_objects(
        &self,
        org: OrgId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ObjectRow>>> {
        let staged: Vec<Option<ObjectRow>> = {
            let b = self.buf();
            ids.iter()
                .map(|id| b.objects.get(&(org, *id)).cloned())
                .collect()
        };
        let missing: Vec<ObjectId> = ids
            .iter()
            .zip(&staged)
            .filter(|(_, s)| s.is_none())
            .map(|(id, _)| *id)
            .collect();
        if missing.is_empty() {
            return Ok(staged);
        }
        let mut stored = self
            .inner
            .get_objects(org, &missing, read)
            .await?
            .into_iter();
        Ok(staged
            .into_iter()
            .map(|s| s.or_else(|| stored.next().flatten()))
            .collect())
    }
    async fn set_object_state(&self, org: OrgId, id: &ObjectId, state: ObjectState) -> Result<()> {
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
        let id = new.changeset.id();
        let mut b = self.buf();
        if b.by_id.contains_key(&(new.repo, id)) {
            return Ok(PutOutcome::AlreadyPresent);
        }
        for d in &new.derived {
            b.derived.insert(
                (new.repo, d.key, d.type_name.clone(), d.version),
                d.value.clone(),
            );
        }
        let at = b.changesets.len();
        b.by_id.insert((new.repo, id), at);
        b.changesets.push(new);
        Ok(PutOutcome::Inserted)
    }
    async fn put_batch(
        &self,
        objects: Vec<(OrgId, Vec<NewObject>, u64)>,
        changesets: Vec<NewChangeset>,
    ) -> Result<()> {
        self.inner.put_batch(objects, changesets).await
    }
    async fn get_changesets(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        let staged: Vec<Option<ChangesetRow>> = {
            let b = self.buf();
            ids.iter()
                .map(|id| {
                    b.by_id
                        .get(&(repo, *id))
                        .map(|i| Self::changeset_row(&b.changesets[*i]))
                })
                .collect()
        };
        let missing: Vec<ObjectId> = ids
            .iter()
            .zip(&staged)
            .filter(|(_, s)| s.is_none())
            .map(|(id, _)| *id)
            .collect();
        if missing.is_empty() {
            return Ok(staged);
        }
        let mut stored = self
            .inner
            .get_changesets(repo, &missing, read)
            .await?
            .into_iter();
        Ok(staged
            .into_iter()
            .map(|s| s.or_else(|| stored.next().flatten()))
            .collect())
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
        let mut out = self.inner.changesets_by_change_id(repo, change_id).await?;
        {
            let b = self.buf();
            out.extend(
                b.changesets
                    .iter()
                    .filter(|c| c.repo == repo && c.changeset.change_id() == change_id)
                    .map(|c| c.changeset.id()),
            );
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
    async fn parents(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>> {
        {
            let b = self.buf();
            if let Some(i) = b.by_id.get(&(repo, *id)) {
                return Ok(b.changesets[*i].changeset.parents().to_vec());
            }
        }
        self.inner.parents(repo, id).await
    }
    async fn children(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>> {
        let mut out = self.inner.children(repo, id).await?;
        {
            let b = self.buf();
            out.extend(
                b.changesets
                    .iter()
                    .filter(|c| c.repo == repo && c.changeset.parents().contains(id))
                    .map(|c| c.changeset.id()),
            );
        }
        out.sort();
        out.dedup();
        Ok(out)
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
        let mut out = self.inner.object_paths(repo, object, read).await?;
        {
            let b = self.buf();
            out.extend(
                b.changesets
                    .iter()
                    .filter(|c| c.repo == repo)
                    .flat_map(|c| c.object_paths.iter())
                    .filter(|r| r.object == *object)
                    .map(|r| r.path.clone()),
            );
        }
        out.sort();
        out.dedup();
        Ok(out)
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
        let staged = self
            .buf()
            .derived
            .get(&(repo, *key, type_name.to_string(), version))
            .cloned();
        match staged {
            Some(v) => Ok(Some(v)),
            None => {
                self.inner
                    .get_derived(repo, key, type_name, version, read)
                    .await
            }
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
        self.buf()
            .derived
            .retain(|(r, _, t, _), _| !(*r == repo && t == type_name));
        self.inner.delete_derived_type(repo, type_name).await
    }

    async fn get_ref(&self, repo: RepoId, name: &RefName) -> Result<Option<RefRow>> {
        self.inner.get_ref(repo, name).await
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
        self.inner.redact(row).await
    }
    async fn get_redactions(&self, org: OrgId, ids: &[ObjectId]) -> Result<Vec<RedactionRow>> {
        self.inner.get_redactions(org, ids).await
    }
    async fn list_redactions(&self, org: OrgId) -> Result<Vec<RedactionRow>> {
        self.inner.list_redactions(org).await
    }
    async fn redaction_epoch(&self, repo: RepoId) -> Result<u64> {
        self.inner.redaction_epoch(repo).await
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
