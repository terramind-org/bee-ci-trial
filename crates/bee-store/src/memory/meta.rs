//! The in-memory [`MetaStore`].

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use bee_core::{
    BeeError, ChangeId, ObjectId, ObjectIdPrefix, ObjectKind, Reason, RefName, RepoPath, Result,
    Subcode,
};
use bee_object::{DeltaOp, hash_id};

use crate::counters::{AccessCounters, Table};
use crate::meta::MetaStore;
use crate::types::{
    ChangesetRow, DerivedRow, EventHead, EventRow, GcEpoch, LandedRow, MinVersion, NewChangeset,
    NewEvent, NewObject, ObjectRow, ObjectState, OrgId, OrgRow, PutOutcome, ReadFrom, Redacted,
    RedactionRow, RefLogEntry, RefRow, RefUpdate, RefUpdated, RepoId, RepoRow, SweepOutcome,
    SweepPlan, UploadRecord, UploadRow, is_inline,
};

#[derive(Clone, Debug)]
struct ObjEntry {
    row: ObjectRow,
    /// Commit that inserted the row (replica visibility).
    seq: u64,
    /// Last commit that touched the row (GC conflict detection).
    touched: u64,
}

#[derive(Clone, Debug)]
struct CsEntry {
    row: ChangesetRow,
    seq: u64,
    touched: u64,
}

#[derive(Clone, Debug)]
struct OrgData {
    row: OrgRow,
    repo_names: BTreeMap<String, RepoId>,
    objects: BTreeMap<ObjectId, ObjEntry>,
    uploads: BTreeMap<(ObjectId, String), UploadRow>,
    redactions: BTreeMap<ObjectId, RedactionRow>,
    gc_marks: BTreeMap<u64, GcEpoch>,
}

#[derive(Clone, Debug)]
struct RepoData {
    row: RepoRow,
    changesets: BTreeMap<ObjectId, CsEntry>,
    by_change_id: BTreeMap<ChangeId, BTreeSet<ObjectId>>,
    edges: BTreeMap<ObjectId, Vec<ObjectId>>,
    children: BTreeMap<ObjectId, BTreeSet<ObjectId>>,
    object_paths: BTreeMap<ObjectId, BTreeMap<Option<RepoPath>, u64>>,
    derived: BTreeMap<(ObjectId, String, u32), (Vec<u8>, u64)>,
    refs: BTreeMap<RefName, RefRow>,
    ref_log: BTreeMap<RefName, BTreeMap<u64, (RefLogEntry, u64)>>,
    landed: BTreeMap<RefName, BTreeMap<ChangeId, LandedRow>>,
    event_counter: u64,
    events: BTreeMap<u64, (EventRow, u64)>,
    redaction_epoch: u64,
}

impl RepoData {
    fn new(row: RepoRow) -> RepoData {
        RepoData {
            row,
            changesets: BTreeMap::new(),
            by_change_id: BTreeMap::new(),
            edges: BTreeMap::new(),
            children: BTreeMap::new(),
            object_paths: BTreeMap::new(),
            derived: BTreeMap::new(),
            refs: BTreeMap::new(),
            ref_log: BTreeMap::new(),
            landed: BTreeMap::new(),
            event_counter: 0,
            events: BTreeMap::new(),
            redaction_epoch: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct State {
    /// Number of committed write transactions.
    seq: u64,
    orgs: BTreeMap<OrgId, OrgData>,
    org_names: BTreeMap<String, OrgId>,
    repos: BTreeMap<RepoId, RepoData>,
}

fn not_found(what: impl std::fmt::Display) -> BeeError {
    BeeError::new(Reason::NotFound, format!("{what} not found"))
}

fn missing(id: &ObjectId) -> BeeError {
    BeeError::new(Reason::MissingObject, format!("object {id} has no row"))
        .with_payload("object", id.to_text())
}

fn invalid(subcode: Subcode, message: impl Into<String>) -> BeeError {
    BeeError::invalid(subcode, message)
}

impl State {
    fn org(&self, id: OrgId) -> Result<&OrgData> {
        self.orgs.get(&id).ok_or_else(|| not_found(id))
    }

    fn org_mut(&mut self, id: OrgId) -> Result<&mut OrgData> {
        self.orgs.get_mut(&id).ok_or_else(|| not_found(id))
    }

    fn repo(&self, id: RepoId) -> Result<&RepoData> {
        self.repos.get(&id).ok_or_else(|| not_found(id))
    }

    fn repo_mut(&mut self, id: RepoId) -> Result<&mut RepoData> {
        self.repos.get_mut(&id).ok_or_else(|| not_found(id))
    }

    fn reached(&self, min: &MinVersion, head: u64) -> bool {
        match min {
            MinVersion::Ref {
                repo,
                name,
                version,
            } => self
                .repos
                .get(repo)
                .and_then(|r| r.ref_log.get(name))
                .and_then(|log| log.get(version))
                .is_some_and(|(_, s)| *s <= head),
            MinVersion::Event { repo, seq } => self
                .repos
                .get(repo)
                .and_then(|r| r.events.get(seq))
                .is_some_and(|(_, s)| *s <= head),
        }
    }
}

/// A faithful in-memory metadata store (SPEC §14.3).
///
/// - **Strictly serializable:** every method runs under one mutex and validates everything
///   before changing anything, so a failed call has no effect.
/// - **Stale replicas:** with [`MemoryMetaStore::set_replica_lag`], reads with
///   [`ReadFrom::Replica`] see immutable rows only as of `lag` commits ago, and forward to
///   the primary when the caller's `min_version` is newer than that.
/// - **Transaction failures:** wrap it in [`crate::faults::FaultyMeta`].
/// - **Counters:** every row read and written is counted in [`AccessCounters`].
#[derive(Debug)]
pub struct MemoryMetaStore {
    state: Mutex<State>,
    counters: Arc<AccessCounters>,
    replica_lag: AtomicU64,
}

impl Default for MemoryMetaStore {
    fn default() -> Self {
        MemoryMetaStore::new()
    }
}

impl MemoryMetaStore {
    /// An empty store with its own counters.
    pub fn new() -> MemoryMetaStore {
        MemoryMetaStore::with_counters(Arc::new(AccessCounters::new()))
    }

    /// An empty store counting into `counters` (share them with a blob store).
    pub fn with_counters(counters: Arc<AccessCounters>) -> MemoryMetaStore {
        MemoryMetaStore {
            state: Mutex::new(State::default()),
            counters,
            replica_lag: AtomicU64::new(0),
        }
    }

    /// The counters.
    pub fn counters(&self) -> &Arc<AccessCounters> {
        &self.counters
    }

    /// Makes replica reads lag `commits` write transactions behind the primary.
    pub fn set_replica_lag(&self, commits: u64) {
        self.replica_lag.store(commits, Ordering::SeqCst);
    }

    /// Number of committed write transactions so far.
    pub fn commit_seq(&self) -> u64 {
        self.lock().seq
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a transaction: counts it and takes the lock.
    fn txn(&self) -> MutexGuard<'_, State> {
        self.counters.transaction();
        self.lock()
    }

    /// The commit horizon a read sees: `None` for the primary.
    fn view(&self, st: &State, read: &ReadFrom) -> Option<u64> {
        match read {
            ReadFrom::Primary => None,
            ReadFrom::Replica(min) => {
                self.counters.replica_read();
                let head = st
                    .seq
                    .saturating_sub(self.replica_lag.load(Ordering::SeqCst));
                if let Some(m) = min
                    && !st.reached(m, head)
                {
                    self.counters.replica_forward();
                    return None;
                }
                Some(head)
            }
        }
    }

    fn mutate<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let mut st = self.lock();
        f(&mut st)
    }

    // ----- test hooks: deliberately break invariants so fsck can be tested ---------------

    /// Test hook: inserts or replaces an object row without any validation.
    pub fn debug_put_object_row(&self, row: ObjectRow) {
        self.mutate(|st| {
            st.seq += 1;
            let seq = st.seq;
            if let Some(org) = st.orgs.get_mut(&row.org) {
                org.objects.insert(
                    row.id,
                    ObjEntry {
                        row,
                        seq,
                        touched: seq,
                    },
                );
            }
        });
    }

    /// Test hook: removes an object row.
    pub fn debug_remove_object_row(&self, org: OrgId, id: &ObjectId) {
        self.mutate(|st| {
            if let Some(o) = st.orgs.get_mut(&org) {
                o.objects.remove(id);
            }
        });
    }

    /// Test hook: replaces a changeset row's stored bytes.
    pub fn debug_set_changeset_bytes(&self, repo: RepoId, id: &ObjectId, bytes: Vec<u8>) {
        self.mutate(|st| {
            if let Some(e) = st
                .repos
                .get_mut(&repo)
                .and_then(|r| r.changesets.get_mut(id))
            {
                e.row.bytes = bytes;
            }
        });
    }

    /// Test hook: removes a changeset's parent edges.
    pub fn debug_remove_parent_edges(&self, repo: RepoId, id: &ObjectId) {
        self.mutate(|st| {
            if let Some(r) = st.repos.get_mut(&repo) {
                r.edges.remove(id);
            }
        });
    }

    /// Test hook: removes a changeset row only (edges and indexes stay).
    pub fn debug_remove_changeset_row(&self, repo: RepoId, id: &ObjectId) {
        self.mutate(|st| {
            if let Some(r) = st.repos.get_mut(&repo) {
                r.changesets.remove(id);
            }
        });
    }

    /// Test hook: removes every `object_paths` row of an object in a repo.
    pub fn debug_remove_object_paths(&self, repo: RepoId, object: &ObjectId) {
        self.mutate(|st| {
            if let Some(r) = st.repos.get_mut(&repo) {
                r.object_paths.remove(object);
            }
        });
    }

    /// Test hook: removes every derived row keyed by `key`.
    pub fn debug_remove_derived(&self, repo: RepoId, key: &ObjectId) {
        self.mutate(|st| {
            if let Some(r) = st.repos.get_mut(&repo) {
                r.derived.retain(|(k, _, _), _| k != key);
            }
        });
    }

    /// Test hook: removes one ref-log entry.
    pub fn debug_remove_ref_log_entry(&self, repo: RepoId, name: &RefName, version: u64) {
        self.mutate(|st| {
            if let Some(log) = st
                .repos
                .get_mut(&repo)
                .and_then(|r| r.ref_log.get_mut(name))
            {
                log.remove(&version);
            }
        });
    }

    /// Test hook: rewrites the `old` target of one ref-log entry.
    pub fn debug_set_ref_log_old(
        &self,
        repo: RepoId,
        name: &RefName,
        version: u64,
        old: Option<ObjectId>,
    ) {
        self.mutate(|st| {
            if let Some((e, _)) = st
                .repos
                .get_mut(&repo)
                .and_then(|r| r.ref_log.get_mut(name))
                .and_then(|log| log.get_mut(&version))
            {
                e.old = old;
            }
        });
    }

    /// Test hook: inserts or replaces a ref-log entry without touching the ref.
    pub fn debug_put_ref_log_entry(&self, entry: RefLogEntry) {
        self.mutate(|st| {
            st.seq += 1;
            let seq = st.seq;
            if let Some(r) = st.repos.get_mut(&entry.repo) {
                r.ref_log
                    .entry(entry.name.clone())
                    .or_default()
                    .insert(entry.version, (entry, seq));
            }
        });
    }

    /// Test hook: overwrites a ref row.
    pub fn debug_set_ref(&self, row: RefRow) {
        self.mutate(|st| {
            if let Some(r) = st.repos.get_mut(&row.repo) {
                r.refs.insert(row.name.clone(), row);
            }
        });
    }

    /// Test hook: inserts or replaces a landed row.
    pub fn debug_put_landed(&self, row: LandedRow) {
        self.mutate(|st| {
            if let Some(r) = st.repos.get_mut(&row.repo) {
                r.landed
                    .entry(row.ref_name.clone())
                    .or_default()
                    .insert(row.change_id, row);
            }
        });
    }

    /// Test hook: removes a landed row.
    pub fn debug_remove_landed(&self, repo: RepoId, name: &RefName, change_id: &ChangeId) {
        self.mutate(|st| {
            if let Some(m) = st.repos.get_mut(&repo).and_then(|r| r.landed.get_mut(name)) {
                m.remove(change_id);
            }
        });
    }

    /// Test hook: writes an event row under any sequence number.
    pub fn debug_put_event(&self, row: EventRow) {
        self.mutate(|st| {
            st.seq += 1;
            let seq = st.seq;
            if let Some(r) = st.repos.get_mut(&row.repo) {
                r.events.insert(row.seq, (row, seq));
            }
        });
    }
}

fn check_new_object(o: &NewObject) -> Result<()> {
    let kind = o.id.kind();
    match kind {
        ObjectKind::Blob | ObjectKind::Tree | ObjectKind::Meta | ObjectKind::Derived(_) => {}
        ObjectKind::Changeset => {
            return Err(invalid(
                Subcode::WrongKind,
                "changesets are stored only in their changeset row",
            ));
        }
        ObjectKind::Foreign => {
            return Err(invalid(
                Subcode::WrongKind,
                "foreign objects are not stored",
            ));
        }
    }
    match &o.inline {
        Some(bytes) => {
            if !is_inline(kind, bytes.len()) {
                return Err(invalid(
                    Subcode::InvalidField,
                    format!("{} cannot be stored inline", o.id),
                ));
            }
            if bytes.len() as u64 != o.size {
                return Err(invalid(
                    Subcode::InvalidField,
                    format!("size of {} disagrees with its inline bytes", o.id),
                ));
            }
            if hash_id(kind, bytes) != o.id {
                return Err(BeeError::new(
                    Reason::Integrity,
                    format!("inline bytes do not hash to {}", o.id),
                ));
            }
        }
        None => {
            if usize::try_from(o.size).is_ok_and(|s| is_inline(kind, s)) {
                return Err(invalid(
                    Subcode::InvalidField,
                    format!("{} must be stored inline", o.id),
                ));
            }
        }
    }
    Ok(())
}

fn redacted_error(id: &ObjectId) -> BeeError {
    BeeError::new(Reason::Redacted, format!("{id} is redacted"))
        .with_payload("object", id.to_text())
}

#[async_trait]
impl MetaStore for MemoryMetaStore {
    async fn create_org(&self, org: OrgRow) -> Result<PutOutcome> {
        let mut st = self.txn();
        self.counters.read(Table::Orgs, 1);
        if let Some(existing) = st.orgs.get(&org.id) {
            return if existing.row == org {
                Ok(PutOutcome::AlreadyPresent)
            } else {
                Err(BeeError::new(
                    Reason::AlreadyExists,
                    format!("{} exists with different fields", org.id),
                ))
            };
        }
        if st.org_names.contains_key(&org.name) {
            return Err(BeeError::new(
                Reason::AlreadyExists,
                format!("org name {:?} is taken", org.name),
            ));
        }
        st.seq += 1;
        self.counters.write(Table::Orgs, 1);
        st.org_names.insert(org.name.clone(), org.id);
        st.orgs.insert(
            org.id,
            OrgData {
                row: org,
                repo_names: BTreeMap::new(),
                objects: BTreeMap::new(),
                uploads: BTreeMap::new(),
                redactions: BTreeMap::new(),
                gc_marks: BTreeMap::new(),
            },
        );
        Ok(PutOutcome::Inserted)
    }

    async fn get_org(&self, id: OrgId) -> Result<Option<OrgRow>> {
        let st = self.txn();
        self.counters.read(Table::Orgs, 1);
        Ok(st.orgs.get(&id).map(|o| o.row.clone()))
    }

    async fn org_by_name(&self, name: &str) -> Result<Option<OrgRow>> {
        let st = self.txn();
        self.counters.read(Table::Orgs, 1);
        Ok(st
            .org_names
            .get(name)
            .and_then(|id| st.orgs.get(id))
            .map(|o| o.row.clone()))
    }

    async fn create_repo(&self, repo: RepoRow) -> Result<PutOutcome> {
        let mut st = self.txn();
        self.counters.read(Table::Repos, 1);
        if let Some(existing) = st.repos.get(&repo.id) {
            return if existing.row == repo {
                Ok(PutOutcome::AlreadyPresent)
            } else {
                Err(BeeError::new(
                    Reason::AlreadyExists,
                    format!("{} exists with different fields", repo.id),
                ))
            };
        }
        let org = st.org(repo.org)?;
        if org.repo_names.contains_key(&repo.name) {
            return Err(BeeError::new(
                Reason::AlreadyExists,
                format!("repo name {:?} is taken", repo.name),
            ));
        }
        st.seq += 1;
        self.counters.write(Table::Repos, 1);
        st.org_mut(repo.org)?
            .repo_names
            .insert(repo.name.clone(), repo.id);
        st.repos.insert(repo.id, RepoData::new(repo));
        Ok(PutOutcome::Inserted)
    }

    async fn get_repo(&self, id: RepoId) -> Result<Option<RepoRow>> {
        let st = self.txn();
        self.counters.read(Table::Repos, 1);
        Ok(st.repos.get(&id).map(|r| r.row.clone()))
    }

    async fn repo_by_name(&self, org: OrgId, name: &str) -> Result<Option<RepoRow>> {
        let st = self.txn();
        self.counters.read(Table::Repos, 1);
        Ok(st
            .orgs
            .get(&org)
            .and_then(|o| o.repo_names.get(name))
            .and_then(|id| st.repos.get(id))
            .map(|r| r.row.clone()))
    }

    async fn list_repos(&self, org: OrgId) -> Result<Vec<RepoRow>> {
        let st = self.txn();
        let out: Vec<RepoRow> = st
            .repos
            .values()
            .filter(|r| r.row.org == org)
            .map(|r| r.row.clone())
            .collect();
        self.counters.read(Table::Repos, out.len() as u64);
        Ok(out)
    }

    async fn put_objects(
        &self,
        org: OrgId,
        objects: Vec<NewObject>,
        upload: Option<UploadRecord>,
        at_ms: u64,
    ) -> Result<Vec<PutOutcome>> {
        let mut st = self.txn();
        let o = st.org(org)?;
        for obj in &objects {
            check_new_object(obj)?;
            self.counters.read(Table::Redactions, 1);
            if o.redactions.contains_key(&obj.id) {
                return Err(redacted_error(&obj.id));
            }
        }
        if let Some(u) = &upload
            && let Some(repo) = u.repo
        {
            let r = st.repo(repo)?;
            if r.row.org != org {
                return Err(not_found(repo));
            }
        }
        st.seq += 1;
        let seq = st.seq;
        let o = st.org_mut(org)?;
        let mut out = Vec::with_capacity(objects.len());
        for obj in objects {
            self.counters.read(Table::Objects, 1);
            let id = obj.id;
            match o.objects.get_mut(&id) {
                Some(e) => {
                    e.touched = seq;
                    if e.row.state == ObjectState::Quarantined {
                        e.row.state = ObjectState::Live;
                        e.row.inline = obj.inline;
                        e.row.size = obj.size;
                        self.counters.write(Table::Objects, 1);
                    }
                    out.push(PutOutcome::AlreadyPresent);
                }
                None => {
                    self.counters.write(Table::Objects, 1);
                    o.objects.insert(
                        id,
                        ObjEntry {
                            row: ObjectRow {
                                org,
                                id,
                                size: obj.size,
                                state: ObjectState::Live,
                                inline: obj.inline,
                                created_at_ms: at_ms,
                            },
                            seq,
                            touched: seq,
                        },
                    );
                    out.push(PutOutcome::Inserted);
                }
            }
            if let Some(u) = &upload {
                self.counters.write(Table::Uploads, 1);
                let row = o
                    .uploads
                    .entry((id, u.principal.clone()))
                    .or_insert_with(|| UploadRow {
                        org,
                        object: id,
                        repo: u.repo,
                        principal: u.principal.clone(),
                        at_ms,
                    });
                row.at_ms = row.at_ms.max(at_ms);
                if u.repo.is_some() {
                    row.repo = u.repo;
                }
            }
        }
        Ok(out)
    }

    async fn get_objects(
        &self,
        org: OrgId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ObjectRow>>> {
        let st = self.txn();
        let view = self.view(&st, &read);
        let o = st.org(org)?;
        Ok(ids
            .iter()
            .map(|id| {
                self.counters.read(Table::Objects, 1);
                let e = o
                    .objects
                    .get(id)
                    .filter(|e| view.is_none_or(|v| e.seq <= v))?;
                if e.row.inline.is_some() {
                    self.counters.fetch(id.kind());
                }
                Some(e.row.clone())
            })
            .collect())
    }

    async fn set_object_state(&self, org: OrgId, id: &ObjectId, state: ObjectState) -> Result<()> {
        let mut st = self.txn();
        let o = st.org(org)?;
        let e = o.objects.get(id).ok_or_else(|| not_found(id))?;
        if state == ObjectState::Purged || e.row.state == ObjectState::Purged {
            return Err(invalid(
                Subcode::InvalidField,
                "only redaction moves an object to or from purged",
            ));
        }
        st.seq += 1;
        let seq = st.seq;
        let e = st
            .org_mut(org)?
            .objects
            .get_mut(id)
            .ok_or_else(|| not_found(id))?;
        e.row.state = state;
        e.touched = seq;
        self.counters.write(Table::Objects, 1);
        Ok(())
    }

    async fn list_objects(
        &self,
        org: OrgId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectRow>> {
        let st = self.txn();
        let o = st.org(org)?;
        let iter: Box<dyn Iterator<Item = (&ObjectId, &ObjEntry)>> = match after {
            Some(a) => Box::new(o.objects.range((Excluded(a), Unbounded))),
            None => Box::new(o.objects.iter()),
        };
        let out: Vec<ObjectRow> = iter.take(limit).map(|(_, e)| e.row.clone()).collect();
        self.counters.read(Table::Objects, out.len() as u64);
        Ok(out)
    }

    async fn get_uploads(&self, org: OrgId, id: &ObjectId) -> Result<Vec<UploadRow>> {
        let st = self.txn();
        let o = st.org(org)?;
        let out: Vec<UploadRow> = o
            .uploads
            .iter()
            .filter(|((obj, _), _)| obj == id)
            .map(|(_, r)| r.clone())
            .collect();
        self.counters.read(Table::Uploads, out.len() as u64);
        Ok(out)
    }

    async fn list_uploads(&self, org: OrgId, since_ms: u64) -> Result<Vec<UploadRow>> {
        let st = self.txn();
        let o = st.org(org)?;
        let out: Vec<UploadRow> = o
            .uploads
            .values()
            .filter(|r| r.at_ms >= since_ms)
            .cloned()
            .collect();
        self.counters.read(Table::Uploads, out.len() as u64);
        Ok(out)
    }

    async fn insert_changeset(&self, new: NewChangeset) -> Result<PutOutcome> {
        let mut st = self.txn();
        let cs = &new.changeset;
        let bytes = cs.encode();
        let id = hash_id(ObjectKind::Changeset, &bytes);
        let repo = st.repo(new.repo)?;
        let org_id = repo.row.org;
        for p in cs.parents() {
            self.counters.read(Table::Changesets, 1);
            if !repo.changesets.contains_key(p) {
                return Err(missing(p));
            }
        }
        let mut referenced: BTreeSet<ObjectId> = BTreeSet::new();
        referenced.insert(*cs.root());
        referenced.insert(*cs.meta());
        for e in cs.delta() {
            if let DeltaOp::Put { id, .. } = e.op {
                referenced.insert(id);
            }
        }
        for row in &new.object_paths {
            referenced.insert(row.object);
        }
        referenced.retain(|id| id.kind() != ObjectKind::Foreign);
        let org = st.org(org_id)?;
        for oid in &referenced {
            self.counters.read(Table::Objects, 1);
            match org.objects.get(oid) {
                Some(e) if e.row.state != ObjectState::Quarantined => {}
                _ => return Err(missing(oid)),
            }
        }

        st.seq += 1;
        let seq = st.seq;
        let at = new.at_ms;
        let org = st.org_mut(org_id)?;
        for oid in &referenced {
            if let Some(e) = org.objects.get_mut(oid) {
                e.touched = seq;
            }
        }
        let repo = st.repo_mut(new.repo)?;
        let outcome = match repo.changesets.get_mut(&id) {
            Some(e) => {
                e.row.last_used_ms = e.row.last_used_ms.max(at);
                e.touched = seq;
                PutOutcome::AlreadyPresent
            }
            None => {
                self.counters.write(Table::Changesets, 1);
                repo.changesets.insert(
                    id,
                    CsEntry {
                        row: ChangesetRow {
                            repo: new.repo,
                            id,
                            bytes,
                            changeset: cs.clone(),
                            created_at_ms: at,
                            last_used_ms: at,
                        },
                        seq,
                        touched: seq,
                    },
                );
                repo.by_change_id
                    .entry(*cs.change_id())
                    .or_default()
                    .insert(id);
                PutOutcome::Inserted
            }
        };
        if let Entry::Vacant(v) = repo.edges.entry(id) {
            self.counters
                .write(Table::ParentEdges, cs.parents().len() as u64);
            v.insert(cs.parents().to_vec());
        }
        for p in cs.parents() {
            repo.children.entry(*p).or_default().insert(id);
            if let Some(e) = repo.changesets.get_mut(p) {
                e.row.last_used_ms = e.row.last_used_ms.max(at);
                e.touched = seq;
            }
        }
        for row in new.object_paths {
            let paths = repo.object_paths.entry(row.object).or_default();
            if let Entry::Vacant(v) = paths.entry(row.path) {
                self.counters.write(Table::ObjectPaths, 1);
                v.insert(seq);
            }
        }
        for d in new.derived {
            let key = (d.key, d.type_name, d.version);
            if let Entry::Vacant(v) = repo.derived.entry(key) {
                self.counters.write(Table::Derived, 1);
                v.insert((d.value, seq));
            }
        }
        Ok(outcome)
    }

    async fn get_changesets(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        let st = self.txn();
        let view = self.view(&st, &read);
        let r = st.repo(repo)?;
        Ok(ids
            .iter()
            .map(|id| {
                self.counters.read(Table::Changesets, 1);
                r.changesets
                    .get(id)
                    .filter(|e| view.is_none_or(|v| e.seq <= v))
                    .map(|e| e.row.clone())
            })
            .collect())
    }

    async fn list_changesets(
        &self,
        repo: RepoId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ChangesetRow>> {
        let st = self.txn();
        let r = st.repo(repo)?;
        let iter: Box<dyn Iterator<Item = (&ObjectId, &CsEntry)>> = match after {
            Some(a) => Box::new(r.changesets.range((Excluded(a), Unbounded))),
            None => Box::new(r.changesets.iter()),
        };
        let out: Vec<ChangesetRow> = iter.take(limit).map(|(_, e)| e.row.clone()).collect();
        self.counters.read(Table::Changesets, out.len() as u64);
        Ok(out)
    }

    async fn changesets_by_change_id(
        &self,
        repo: RepoId,
        change_id: &ChangeId,
    ) -> Result<Vec<ObjectId>> {
        let st = self.txn();
        let r = st.repo(repo)?;
        let out: Vec<ObjectId> = r
            .by_change_id
            .get(change_id)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        self.counters.read(Table::Changesets, out.len() as u64);
        Ok(out)
    }

    async fn parents(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>> {
        let st = self.txn();
        let r = st.repo(repo)?;
        let out = r.edges.get(id).cloned().unwrap_or_default();
        self.counters.read(Table::ParentEdges, out.len() as u64);
        Ok(out)
    }

    async fn children(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>> {
        let st = self.txn();
        let r = st.repo(repo)?;
        let out: Vec<ObjectId> = r
            .children
            .get(id)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        self.counters.read(Table::ParentEdges, out.len() as u64);
        Ok(out)
    }

    async fn changesets_with_prefix(
        &self,
        repo: RepoId,
        prefix: &ObjectIdPrefix,
        limit: usize,
    ) -> Result<Vec<ObjectId>> {
        let st = self.txn();
        let r = st.repo(repo)?;
        let out: Vec<ObjectId> = r
            .changesets
            .keys()
            .filter(|id| prefix.matches(id))
            .take(limit)
            .copied()
            .collect();
        self.counters.read(Table::Changesets, out.len() as u64);
        Ok(out)
    }

    async fn touch_changesets(&self, repo: RepoId, ids: &[ObjectId], at_ms: u64) -> Result<()> {
        let mut st = self.txn();
        st.repo(repo)?;
        st.seq += 1;
        let seq = st.seq;
        let r = st.repo_mut(repo)?;
        for id in ids {
            if let Some(e) = r.changesets.get_mut(id) {
                e.row.last_used_ms = e.row.last_used_ms.max(at_ms);
                e.touched = seq;
                self.counters.write(Table::Changesets, 1);
            }
        }
        Ok(())
    }

    async fn object_paths(
        &self,
        repo: RepoId,
        object: &ObjectId,
        read: ReadFrom,
    ) -> Result<Vec<Option<RepoPath>>> {
        let st = self.txn();
        let view = self.view(&st, &read);
        let r = st.repo(repo)?;
        let out: Vec<Option<RepoPath>> = r
            .object_paths
            .get(object)
            .map(|m| {
                m.iter()
                    .filter(|(_, s)| view.is_none_or(|v| **s <= v))
                    .map(|(p, _)| p.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.counters.read(Table::ObjectPaths, out.len() as u64);
        Ok(out)
    }

    async fn put_derived(&self, repo: RepoId, rows: Vec<DerivedRow>) -> Result<()> {
        let mut st = self.txn();
        st.repo(repo)?;
        st.seq += 1;
        let seq = st.seq;
        let r = st.repo_mut(repo)?;
        for d in rows {
            let key = (d.key, d.type_name, d.version);
            if let Entry::Vacant(v) = r.derived.entry(key) {
                self.counters.write(Table::Derived, 1);
                v.insert((d.value, seq));
            }
        }
        Ok(())
    }

    async fn get_derived(
        &self,
        repo: RepoId,
        key: &ObjectId,
        type_name: &str,
        version: u32,
        read: ReadFrom,
    ) -> Result<Option<Vec<u8>>> {
        let st = self.txn();
        let view = self.view(&st, &read);
        let r = st.repo(repo)?;
        self.counters.read(Table::Derived, 1);
        Ok(r.derived
            .get(&(*key, type_name.to_string(), version))
            .filter(|(_, s)| view.is_none_or(|v| *s <= v))
            .map(|(v, _)| v.clone()))
    }

    async fn list_derived_by_type(
        &self,
        repo: RepoId,
        type_name: &str,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, u32, Vec<u8>)>> {
        let st = self.txn();
        let r = st.repo(repo)?;
        let out: Vec<(ObjectId, u32, Vec<u8>)> = r
            .derived
            .iter()
            .filter(|((k, t, _), _)| t == type_name && after.is_none_or(|a| *k > a))
            .take(limit)
            .map(|((k, _, v), (value, _))| (*k, *v, value.clone()))
            .collect();
        self.counters.read(Table::Derived, out.len() as u64);
        Ok(out)
    }

    async fn delete_derived_type(&self, repo: RepoId, type_name: &str) -> Result<u64> {
        let mut st = self.txn();
        st.repo(repo)?;
        let r = st.repo_mut(repo)?;
        let before = r.derived.len();
        r.derived.retain(|(_, t, _), _| t != type_name);
        let n = (before - r.derived.len()) as u64;
        self.counters.write(Table::Derived, n);
        Ok(n)
    }

    async fn get_ref(&self, repo: RepoId, name: &RefName) -> Result<Option<RefRow>> {
        let st = self.txn();
        let r = st.repo(repo)?;
        self.counters.read(Table::Refs, 1);
        Ok(r.refs.get(name).cloned())
    }

    async fn list_refs(&self, repo: RepoId, prefix: &str) -> Result<Vec<RefRow>> {
        let st = self.txn();
        let r = st.repo(repo)?;
        let out: Vec<RefRow> = r
            .refs
            .values()
            .filter(|row| row.name.as_str().starts_with(prefix))
            .cloned()
            .collect();
        self.counters.read(Table::Refs, out.len() as u64);
        Ok(out)
    }

    async fn update_ref(&self, update: RefUpdate) -> Result<RefUpdated> {
        let mut st = self.txn();
        let r = st.repo(update.repo)?;
        if update.steps.is_empty() {
            return Err(invalid(Subcode::InvalidField, "a ref update needs a step"));
        }
        self.counters.read(Table::Refs, 1);
        let cur = r.refs.get(&update.name);
        let cur_version = cur.map(|c| c.version).unwrap_or(0);
        let cur_target = cur.and_then(|c| c.target);
        let ok = match update.expected {
            crate::types::Expected::Absent => cur_target.is_none(),
            crate::types::Expected::Version(v) => cur.is_some() && cur_version == v,
        };
        if !ok {
            return Err(BeeError::new(
                Reason::VersionMismatch,
                format!(
                    "{} is at version {cur_version}, expected {:?}",
                    update.name, update.expected
                ),
            )
            .with_payload("current_version", cur_version as i64));
        }
        let landed_now = r.landed.get(&update.name);
        let mut seen: BTreeMap<ChangeId, ObjectId> = BTreeMap::new();
        for step in &update.steps {
            if let Some(t) = &step.target {
                self.counters.read(Table::Changesets, 1);
                if !r.changesets.contains_key(t) {
                    return Err(missing(t));
                }
            } else if !step.landed.is_empty() {
                return Err(invalid(
                    Subcode::InvalidField,
                    "a ref deletion cannot land changes",
                ));
            }
            for l in &step.landed {
                for id in [&l.source, &l.result] {
                    self.counters.read(Table::Changesets, 1);
                    if !r.changesets.contains_key(id) {
                        return Err(missing(id));
                    }
                }
                self.counters.read(Table::Landed, 1);
                let existing = landed_now
                    .and_then(|m| m.get(&l.change_id))
                    .map(|row| row.source)
                    .or_else(|| seen.get(&l.change_id).copied());
                if let Some(src) = existing {
                    return Err(BeeError::new(
                        Reason::AlreadyLanded,
                        format!("{} already landed on {}", l.change_id, update.name),
                    )
                    .with_payload("change_id", l.change_id.to_text())
                    .with_payload("existing_source", src.to_text()));
                }
                seen.insert(l.change_id, l.source);
            }
        }

        st.seq += 1;
        let seq = st.seq;
        let r = st.repo_mut(update.repo)?;
        let mut version = cur_version;
        let mut old = cur_target;
        for step in &update.steps {
            version += 1;
            let entry = RefLogEntry {
                repo: update.repo,
                name: update.name.clone(),
                version,
                old,
                new: step.target,
                principal: update.principal.clone(),
                at_ms: update.at_ms,
                landed: step.landed.clone(),
            };
            self.counters.write(Table::RefLog, 1);
            r.ref_log
                .entry(update.name.clone())
                .or_default()
                .insert(version, (entry, seq));
            for l in &step.landed {
                self.counters.write(Table::Landed, 1);
                r.landed.entry(update.name.clone()).or_default().insert(
                    l.change_id,
                    LandedRow {
                        repo: update.repo,
                        ref_name: update.name.clone(),
                        change_id: l.change_id,
                        source: l.source,
                        result: l.result,
                        version,
                    },
                );
            }
            if let Some(t) = &step.target
                && let Some(e) = r.changesets.get_mut(t)
            {
                e.touched = seq;
                e.row.last_used_ms = e.row.last_used_ms.max(update.at_ms);
            }
            old = step.target;
        }
        self.counters.write(Table::Refs, 1);
        r.refs.insert(
            update.name.clone(),
            RefRow {
                repo: update.repo,
                name: update.name.clone(),
                target: old,
                version,
            },
        );
        let mut event_seqs = Vec::new();
        if !update.events.is_empty() {
            self.counters.write(Table::EventCounters, 1);
            for ev in update.events {
                r.event_counter += 1;
                let s = r.event_counter;
                self.counters.write(Table::Events, 1);
                r.events.insert(
                    s,
                    (
                        EventRow {
                            repo: update.repo,
                            seq: s,
                            kind: ev.kind,
                            payload: ev.payload,
                            at_ms: ev.at_ms,
                        },
                        seq,
                    ),
                );
                event_seqs.push(s);
            }
        }
        Ok(RefUpdated {
            old_version: cur_version,
            new_version: version,
            event_seqs,
        })
    }

    async fn ref_log(
        &self,
        repo: RepoId,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogEntry>> {
        let st = self.txn();
        let r = st.repo(repo)?;
        let out: Vec<RefLogEntry> = r
            .ref_log
            .get(name)
            .map(|log| {
                log.range(from_version..)
                    .take(limit)
                    .map(|(_, (e, _))| e.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.counters.read(Table::RefLog, out.len() as u64);
        Ok(out)
    }

    async fn get_landed(
        &self,
        repo: RepoId,
        name: &RefName,
        change_ids: &[ChangeId],
    ) -> Result<Vec<Option<LandedRow>>> {
        let st = self.txn();
        let r = st.repo(repo)?;
        self.counters.read(Table::Landed, change_ids.len() as u64);
        let m = r.landed.get(name);
        Ok(change_ids
            .iter()
            .map(|c| m.and_then(|m| m.get(c)).cloned())
            .collect())
    }

    async fn list_landed(&self, repo: RepoId, name: Option<&RefName>) -> Result<Vec<LandedRow>> {
        let st = self.txn();
        let r = st.repo(repo)?;
        let out: Vec<LandedRow> = r
            .landed
            .iter()
            .filter(|(n, _)| name.is_none_or(|want| want == *n))
            .flat_map(|(_, m)| m.values().cloned())
            .collect();
        self.counters.read(Table::Landed, out.len() as u64);
        Ok(out)
    }

    async fn allocate_event_seqs(&self, repo: RepoId, n: u64) -> Result<Range<u64>> {
        let mut st = self.txn();
        st.repo(repo)?;
        st.seq += 1;
        let r = st.repo_mut(repo)?;
        let start = r.event_counter + 1;
        r.event_counter += n;
        self.counters.write(Table::EventCounters, 1);
        Ok(start..start + n)
    }

    async fn insert_events(&self, repo: RepoId, events: Vec<(u64, NewEvent)>) -> Result<()> {
        let mut st = self.txn();
        let r = st.repo(repo)?;
        for (s, _) in &events {
            if *s == 0 || *s > r.event_counter {
                return Err(invalid(
                    Subcode::InvalidField,
                    format!("event sequence number {s} was never allocated"),
                ));
            }
        }
        st.seq += 1;
        let seq = st.seq;
        let r = st.repo_mut(repo)?;
        for (s, ev) in events {
            if let Entry::Vacant(v) = r.events.entry(s) {
                self.counters.write(Table::Events, 1);
                v.insert((
                    EventRow {
                        repo,
                        seq: s,
                        kind: ev.kind,
                        payload: ev.payload,
                        at_ms: ev.at_ms,
                    },
                    seq,
                ));
            }
        }
        Ok(())
    }

    async fn append_events(&self, repo: RepoId, events: Vec<NewEvent>) -> Result<Vec<u64>> {
        let mut st = self.txn();
        st.repo(repo)?;
        st.seq += 1;
        let seq = st.seq;
        let r = st.repo_mut(repo)?;
        self.counters.write(Table::EventCounters, 1);
        let mut out = Vec::with_capacity(events.len());
        for ev in events {
            r.event_counter += 1;
            let s = r.event_counter;
            self.counters.write(Table::Events, 1);
            r.events.insert(
                s,
                (
                    EventRow {
                        repo,
                        seq: s,
                        kind: ev.kind,
                        payload: ev.payload,
                        at_ms: ev.at_ms,
                    },
                    seq,
                ),
            );
            out.push(s);
        }
        Ok(out)
    }

    async fn read_events(
        &self,
        repo: RepoId,
        from_seq: u64,
        limit: usize,
        read: ReadFrom,
    ) -> Result<Vec<EventRow>> {
        let st = self.txn();
        let view = self.view(&st, &read);
        let r = st.repo(repo)?;
        let out: Vec<EventRow> = r
            .events
            .range(from_seq..)
            .filter(|(_, (_, s))| view.is_none_or(|v| *s <= v))
            .take(limit)
            .map(|(_, (e, _))| e.clone())
            .collect();
        self.counters.read(Table::Events, out.len() as u64);
        Ok(out)
    }

    async fn event_head(&self, repo: RepoId) -> Result<EventHead> {
        let st = self.txn();
        let r = st.repo(repo)?;
        self.counters.read(Table::EventCounters, 1);
        Ok(EventHead {
            allocated: r.event_counter,
            written: r.events.keys().next_back().copied().unwrap_or(0),
        })
    }

    async fn redact(&self, row: RedactionRow) -> Result<Redacted> {
        let mut st = self.txn();
        if !matches!(row.object.kind(), ObjectKind::Blob | ObjectKind::Meta) {
            return Err(invalid(
                Subcode::WrongKind,
                "only blobs and metas can be redacted",
            ));
        }
        let org_id = row.org;
        let org = st.org(org_id)?;
        self.counters.read(Table::Redactions, 1);
        let already = org.redactions.contains_key(&row.object);
        let repos: Vec<RepoId> = st
            .repos
            .iter()
            .filter(|(_, r)| r.row.org == org_id && r.object_paths.contains_key(&row.object))
            .map(|(id, _)| *id)
            .collect();
        if already {
            let epochs = repos
                .iter()
                .filter_map(|id| st.repos.get(id).map(|r| (*id, r.redaction_epoch)))
                .collect();
            return Ok(Redacted {
                newly_redacted: false,
                epochs,
            });
        }
        st.seq += 1;
        let seq = st.seq;
        let object = row.object;
        let org = st.org_mut(org_id)?;
        self.counters.write(Table::Redactions, 1);
        org.redactions.insert(object, row);
        if let Some(e) = org.objects.get_mut(&object) {
            e.row.state = ObjectState::Purged;
            e.row.inline = None;
            e.touched = seq;
            self.counters.write(Table::Objects, 1);
        }
        let mut epochs = Vec::new();
        for id in repos {
            let r = st.repo_mut(id)?;
            r.redaction_epoch += 1;
            self.counters.write(Table::RedactionEpochs, 1);
            epochs.push((id, r.redaction_epoch));
        }
        Ok(Redacted {
            newly_redacted: true,
            epochs,
        })
    }

    async fn get_redactions(&self, org: OrgId, ids: &[ObjectId]) -> Result<Vec<RedactionRow>> {
        let st = self.txn();
        let o = st.org(org)?;
        self.counters.read(Table::Redactions, ids.len() as u64);
        Ok(ids
            .iter()
            .filter_map(|id| o.redactions.get(id).cloned())
            .collect())
    }

    async fn list_redactions(&self, org: OrgId) -> Result<Vec<RedactionRow>> {
        let st = self.txn();
        let o = st.org(org)?;
        self.counters
            .read(Table::Redactions, o.redactions.len() as u64);
        Ok(o.redactions.values().cloned().collect())
    }

    async fn redaction_epoch(&self, repo: RepoId) -> Result<u64> {
        let st = self.txn();
        self.counters.read(Table::RedactionEpochs, 1);
        Ok(st.repo(repo)?.redaction_epoch)
    }

    async fn gc_begin(&self, org: OrgId, at_ms: u64) -> Result<GcEpoch> {
        let mut st = self.txn();
        st.org(org)?;
        st.seq += 1;
        let epoch = GcEpoch {
            org,
            seq: st.seq,
            at_ms,
        };
        self.counters.write(Table::GcMarks, 1);
        st.org_mut(org)?.gc_marks.insert(epoch.seq, epoch);
        Ok(epoch)
    }

    async fn gc_sweep(&self, epoch: GcEpoch, plan: SweepPlan) -> Result<SweepOutcome> {
        let mut st = self.txn();
        let org = st.org(epoch.org)?;
        self.counters.read(Table::GcMarks, 1);
        if org.gc_marks.get(&epoch.seq) != Some(&epoch) {
            return Err(invalid(Subcode::InvalidField, "unknown GC epoch"));
        }
        let planned: BTreeSet<(RepoId, ObjectId)> = plan.changesets.iter().copied().collect();
        let mut touched = Vec::new();
        for (repo, id) in &plan.changesets {
            let r = st.repo(*repo)?;
            if r.row.org != epoch.org {
                return Err(not_found(repo));
            }
            self.counters.read(Table::Changesets, 1);
            if let Some(e) = r.changesets.get(id) {
                if e.touched > epoch.seq {
                    touched.push(format!("changeset {id} in {repo} was used after the mark"));
                }
                if r.refs.values().any(|row| row.target == Some(*id)) {
                    touched.push(format!("changeset {id} in {repo} is a ref target"));
                }
                if let Some(kids) = r.children.get(id) {
                    for k in kids {
                        if r.changesets.contains_key(k) && !planned.contains(&(*repo, *k)) {
                            touched.push(format!("changeset {id} in {repo} has a kept child {k}"));
                        }
                    }
                }
            }
        }
        let mut objects = Vec::new();
        for id in &plan.objects {
            self.counters.read(Table::Objects, 1);
            if let Some(e) = org.objects.get(id) {
                if e.row.state == ObjectState::Purged {
                    continue;
                }
                if e.touched > epoch.seq {
                    touched.push(format!("object {id} was referenced after the mark"));
                }
                objects.push(*id);
            }
        }
        if !touched.is_empty() {
            return Ok(SweepOutcome::Deferred { touched });
        }

        st.seq += 1;
        let mut changesets = Vec::new();
        for (repo, id) in &plan.changesets {
            let r = st.repo_mut(*repo)?;
            let Some(e) = r.changesets.remove(id) else {
                continue;
            };
            self.counters.write(Table::Changesets, 1);
            if let Some(set) = r.by_change_id.get_mut(e.row.changeset.change_id()) {
                set.remove(id);
                if set.is_empty() {
                    r.by_change_id.remove(e.row.changeset.change_id());
                }
            }
            if let Some(parents) = r.edges.remove(id) {
                self.counters
                    .write(Table::ParentEdges, parents.len() as u64);
                for p in parents {
                    if let Some(kids) = r.children.get_mut(&p) {
                        kids.remove(id);
                    }
                }
            }
            r.children.remove(id);
            r.derived.retain(|(k, _, _), _| k != id);
            changesets.push((*repo, *id));
        }
        let repo_ids: Vec<RepoId> = st
            .repos
            .iter()
            .filter(|(_, r)| r.row.org == epoch.org)
            .map(|(id, _)| *id)
            .collect();
        for id in &objects {
            for rid in &repo_ids {
                let r = st.repo_mut(*rid)?;
                if r.object_paths.remove(id).is_some() {
                    self.counters.write(Table::ObjectPaths, 1);
                }
            }
            let org = st.org_mut(epoch.org)?;
            org.objects.remove(id);
            org.uploads.retain(|(obj, _), _| obj != id);
            self.counters.write(Table::Objects, 1);
        }
        Ok(SweepOutcome::Swept {
            changesets,
            objects,
        })
    }

    fn access_counters(&self) -> Option<Arc<AccessCounters>> {
        Some(self.counters.clone())
    }
}
