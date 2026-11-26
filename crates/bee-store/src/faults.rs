//! Named fault points (SPEC §14.4).
//!
//! Every store call has a point named `meta.<method>` or `blob.<method>` (checked by the
//! [`FaultyMeta`] and [`FaultyBlob`] wrappers, which work around any implementation), and
//! every write-ordering step of [`crate::write::Writer`] has a point in [`points`]. A
//! [`FaultInjector`] holds rules saying which fault fires at which hit of which point:
//!
//! - [`Fault::Error`]: the call fails before doing anything.
//! - [`Fault::Crash`]: the task panics before doing anything, as if the process died there.
//! - [`Fault::Delay`]: the call sleeps, then proceeds.
//! - [`Fault::FailAfter`]: the call completes (commits), then reports an error: "committed,
//!   response lost".
//!
//! Hits are counted per point, so a test can first run a scenario with no rules, read
//! [`FaultInjector::hits`], and then replay it once per `(point, hit)` with a crash there.

use std::collections::BTreeMap;
use std::future::Future;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bee_core::{BeeError, ChangeId, ObjectId, ObjectIdPrefix, Reason, RefName, RepoPath, Result};

use crate::blob::{BlobInfo, BlobStore};
use crate::counters::AccessCounters;
use crate::meta::MetaStore;
use crate::types::{
    ChangesetRow, DerivedRow, EventHead, EventRow, GcEpoch, LandedRow, NewChangeset, NewEvent,
    NewObject, ObjectRow, ObjectState, OrgId, OrgRow, PutOutcome, ReadFrom, Redacted, RedactionRow,
    RefLogEntry, RefRow, RefUpdate, RefUpdated, RepoId, RepoRow, StorageClass, SweepOutcome,
    SweepPlan, UploadRecord, UploadRow,
};

/// Names of the write-ordering steps (SPEC §6.2). Store-call points are `meta.<method>` and
/// `blob.<method>`.
pub mod points {
    /// Before verifying uploaded bytes against their ids.
    pub const UPLOAD_VERIFY: &str = "write.upload.verify";
    /// Before writing one object's bytes to the blob store (once per non-inline object).
    pub const UPLOAD_BLOB: &str = "write.upload.blob";
    /// Before inserting the `objects` rows of an upload.
    pub const UPLOAD_ROW: &str = "write.upload.row";
    /// Before the changeset transaction.
    pub const CHANGESET_ROW: &str = "write.changeset.row";
    /// Before the ref compare-and-swap.
    pub const REF_CAS: &str = "write.ref.cas";
    /// Before the redaction transaction.
    pub const REDACT_ROW: &str = "write.redact.row";
    /// Before deleting redacted bytes.
    pub const REDACT_BYTES: &str = "write.redact.bytes";
    /// Every write-ordering step.
    pub const WRITE_STEPS: [&str; 7] = [
        UPLOAD_VERIFY,
        UPLOAD_BLOB,
        UPLOAD_ROW,
        CHANGESET_ROW,
        REF_CAS,
        REDACT_ROW,
        REDACT_BYTES,
    ];
}

/// The marker every injected crash panics with; see [`is_injected_crash`].
pub const CRASH_MARKER: &str = "bee-fault: injected crash at ";

/// True when a panic payload message comes from an injected [`Fault::Crash`].
pub fn is_injected_crash(message: &str) -> bool {
    message.starts_with(CRASH_MARKER)
}

/// A fault to inject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Fail before doing anything.
    Error(BeeError),
    /// Panic before doing anything (abort the operation as a process crash would).
    Crash,
    /// Sleep, then proceed.
    Delay(Duration),
    /// Do the operation, then fail with this error ("committed, response lost").
    FailAfter(BeeError),
}

impl Fault {
    /// A retryable `UNAVAILABLE` error fault.
    pub fn unavailable() -> Fault {
        Fault::Error(injected_error())
    }

    /// "Committed, response lost" with a retryable `UNAVAILABLE` error.
    pub fn response_lost() -> Fault {
        Fault::FailAfter(injected_error())
    }
}

/// The error injected faults use by default.
pub fn injected_error() -> BeeError {
    BeeError::new(Reason::Unavailable, "injected fault").with_payload("injected", true)
}

/// When and where a fault fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultRule {
    /// Point name; a trailing `*` matches any point with that prefix.
    pub point: String,
    /// The fault.
    pub fault: Fault,
    /// Matching hits to let pass before firing.
    pub skip: u64,
    /// How many times to fire; `None` for every later hit.
    pub times: Option<u64>,
}

impl FaultRule {
    /// Fires on every hit of `point`.
    pub fn always(point: impl Into<String>, fault: Fault) -> FaultRule {
        FaultRule {
            point: point.into(),
            fault,
            skip: 0,
            times: None,
        }
    }

    /// Fires once, on hit number `n` (0-based) of `point`.
    pub fn nth(point: impl Into<String>, n: u64, fault: Fault) -> FaultRule {
        FaultRule {
            point: point.into(),
            fault,
            skip: n,
            times: Some(1),
        }
    }

    fn matches(&self, point: &str) -> bool {
        match self.point.strip_suffix('*') {
            Some(prefix) => point.starts_with(prefix),
            None => self.point == point,
        }
    }
}

/// What a passed check tells the caller to do.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub enum Proceed {
    /// Run the operation and return its result.
    Normally,
    /// Run the operation, then return this error instead of its result.
    ThenFail(BeeError),
}

#[derive(Debug, Default)]
struct Inner {
    rules: Vec<FaultRule>,
    hits: BTreeMap<String, u64>,
    fired: Vec<(String, u64)>,
}

/// Holds fault rules and per-point hit counts. Cheap to check when empty.
#[derive(Debug, Default)]
pub struct FaultInjector {
    inner: Mutex<Inner>,
}

impl FaultInjector {
    /// An injector with no rules.
    pub fn new() -> Arc<FaultInjector> {
        Arc::new(FaultInjector::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a rule; rules are tried in insertion order.
    pub fn add(&self, rule: FaultRule) {
        self.lock().rules.push(rule);
    }

    /// Removes every rule (hit counts stay).
    pub fn clear_rules(&self) {
        self.lock().rules.clear();
    }

    /// Removes every rule and resets hit counts.
    pub fn reset(&self) {
        *self.lock() = Inner::default();
    }

    /// Hit counts per point since the last reset.
    pub fn hits(&self) -> BTreeMap<String, u64> {
        self.lock().hits.clone()
    }

    /// Hits of one point.
    pub fn hits_of(&self, point: &str) -> u64 {
        self.lock().hits.get(point).copied().unwrap_or(0)
    }

    /// Faults that fired, as `(point, hit index)`, in order.
    pub fn fired(&self) -> Vec<(String, u64)> {
        self.lock().fired.clone()
    }

    /// Records a hit of `point` and applies the first matching rule: returns `Err` for
    /// [`Fault::Error`], panics for [`Fault::Crash`], sleeps for [`Fault::Delay`] and returns
    /// [`Proceed::ThenFail`] for [`Fault::FailAfter`].
    pub async fn check(&self, point: &str) -> Result<Proceed> {
        let fault = {
            let mut inner = self.lock();
            let hit = {
                let h = inner.hits.entry(point.to_string()).or_insert(0);
                *h += 1;
                *h - 1
            };
            let mut chosen = None;
            for rule in inner.rules.iter_mut() {
                if !rule.matches(point) {
                    continue;
                }
                if rule.skip > 0 {
                    rule.skip -= 1;
                    continue;
                }
                if let Some(t) = rule.times.as_mut() {
                    if *t == 0 {
                        continue;
                    }
                    *t -= 1;
                }
                chosen = Some(rule.fault.clone());
                break;
            }
            if chosen.is_some() {
                inner.fired.push((point.to_string(), hit));
            }
            chosen
        };
        match fault {
            None => Ok(Proceed::Normally),
            Some(Fault::Error(e)) => Err(e.with_payload("fault_point", point)),
            Some(Fault::Crash) => panic!("{CRASH_MARKER}{point}"),
            Some(Fault::Delay(d)) => {
                tokio::time::sleep(d).await;
                Ok(Proceed::Normally)
            }
            Some(Fault::FailAfter(e)) => {
                Ok(Proceed::ThenFail(e.with_payload("fault_point", point)))
            }
        }
    }

    /// Checks `point`, then runs `op` unless the check failed; applies "then fail" after.
    pub async fn guard<T, F>(&self, point: &str, op: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        match self.check(point).await? {
            Proceed::Normally => op.await,
            Proceed::ThenFail(e) => {
                op.await?;
                Err(e)
            }
        }
    }
}

/// A [`MetaStore`] wrapper that checks the point `meta.<method>` before every call.
pub struct FaultyMeta<S> {
    inner: S,
    faults: Arc<FaultInjector>,
}

impl<S> FaultyMeta<S> {
    /// Wraps `inner`.
    pub fn new(inner: S, faults: Arc<FaultInjector>) -> FaultyMeta<S> {
        FaultyMeta { inner, faults }
    }

    /// The wrapped store.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// The injector.
    pub fn faults(&self) -> &Arc<FaultInjector> {
        &self.faults
    }
}

macro_rules! guarded {
    ($self:ident, $prefix:literal, $name:ident ( $($arg:expr),* )) => {
        $self
            .faults
            .guard(concat!($prefix, stringify!($name)), $self.inner.$name($($arg),*))
            .await
    };
}

#[async_trait]
impl<S: MetaStore> MetaStore for FaultyMeta<S> {
    async fn create_org(&self, org: OrgRow) -> Result<PutOutcome> {
        guarded!(self, "meta.", create_org(org))
    }
    async fn get_org(&self, id: OrgId) -> Result<Option<OrgRow>> {
        guarded!(self, "meta.", get_org(id))
    }
    async fn org_by_name(&self, name: &str) -> Result<Option<OrgRow>> {
        guarded!(self, "meta.", org_by_name(name))
    }
    async fn create_repo(&self, repo: RepoRow) -> Result<PutOutcome> {
        guarded!(self, "meta.", create_repo(repo))
    }
    async fn get_repo(&self, id: RepoId) -> Result<Option<RepoRow>> {
        guarded!(self, "meta.", get_repo(id))
    }
    async fn repo_by_name(&self, org: OrgId, name: &str) -> Result<Option<RepoRow>> {
        guarded!(self, "meta.", repo_by_name(org, name))
    }
    async fn list_repos(&self, org: OrgId) -> Result<Vec<RepoRow>> {
        guarded!(self, "meta.", list_repos(org))
    }
    async fn put_objects(
        &self,
        org: OrgId,
        objects: Vec<NewObject>,
        upload: Option<UploadRecord>,
        at_ms: u64,
    ) -> Result<Vec<PutOutcome>> {
        guarded!(self, "meta.", put_objects(org, objects, upload, at_ms))
    }
    async fn get_objects(
        &self,
        org: OrgId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ObjectRow>>> {
        guarded!(self, "meta.", get_objects(org, ids, read))
    }
    async fn set_object_state(&self, org: OrgId, id: &ObjectId, state: ObjectState) -> Result<()> {
        guarded!(self, "meta.", set_object_state(org, id, state))
    }
    async fn list_objects(
        &self,
        org: OrgId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectRow>> {
        guarded!(self, "meta.", list_objects(org, after, limit))
    }
    async fn get_uploads(&self, org: OrgId, id: &ObjectId) -> Result<Vec<UploadRow>> {
        guarded!(self, "meta.", get_uploads(org, id))
    }
    async fn list_uploads(&self, org: OrgId, since_ms: u64) -> Result<Vec<UploadRow>> {
        guarded!(self, "meta.", list_uploads(org, since_ms))
    }
    async fn insert_changeset(&self, new: NewChangeset) -> Result<PutOutcome> {
        guarded!(self, "meta.", insert_changeset(new))
    }
    async fn put_batch(
        &self,
        objects: Vec<(OrgId, Vec<NewObject>, u64)>,
        changesets: Vec<NewChangeset>,
    ) -> Result<()> {
        guarded!(self, "meta.", put_batch(objects, changesets))
    }
    async fn insert_changeset_with_objects(
        &self,
        org: OrgId,
        objects: Vec<NewObject>,
        upload: Option<UploadRecord>,
        at_ms: u64,
        new: NewChangeset,
    ) -> Result<PutOutcome> {
        guarded!(
            self,
            "meta.",
            insert_changeset_with_objects(org, objects, upload, at_ms, new)
        )
    }
    async fn get_changeset_root(
        &self,
        repo: RepoId,
        id: &ObjectId,
        read: ReadFrom,
    ) -> Result<Option<ObjectId>> {
        guarded!(self, "meta.", get_changeset_root(repo, id, read))
    }
    async fn get_changesets(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        guarded!(self, "meta.", get_changesets(repo, ids, read))
    }
    async fn list_changesets(
        &self,
        repo: RepoId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ChangesetRow>> {
        guarded!(self, "meta.", list_changesets(repo, after, limit))
    }
    async fn changesets_by_change_id(
        &self,
        repo: RepoId,
        change_id: &ChangeId,
    ) -> Result<Vec<ObjectId>> {
        guarded!(self, "meta.", changesets_by_change_id(repo, change_id))
    }
    async fn parents(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>> {
        guarded!(self, "meta.", parents(repo, id))
    }
    async fn children(&self, repo: RepoId, id: &ObjectId) -> Result<Vec<ObjectId>> {
        guarded!(self, "meta.", children(repo, id))
    }
    async fn changesets_with_prefix(
        &self,
        repo: RepoId,
        prefix: &ObjectIdPrefix,
        limit: usize,
    ) -> Result<Vec<ObjectId>> {
        guarded!(self, "meta.", changesets_with_prefix(repo, prefix, limit))
    }
    async fn touch_changesets(&self, repo: RepoId, ids: &[ObjectId], at_ms: u64) -> Result<()> {
        guarded!(self, "meta.", touch_changesets(repo, ids, at_ms))
    }
    async fn object_paths(
        &self,
        repo: RepoId,
        object: &ObjectId,
        read: ReadFrom,
    ) -> Result<Vec<Option<RepoPath>>> {
        guarded!(self, "meta.", object_paths(repo, object, read))
    }
    async fn put_derived(&self, repo: RepoId, rows: Vec<DerivedRow>) -> Result<()> {
        guarded!(self, "meta.", put_derived(repo, rows))
    }
    async fn get_derived(
        &self,
        repo: RepoId,
        key: &ObjectId,
        type_name: &str,
        version: u32,
        read: ReadFrom,
    ) -> Result<Option<Vec<u8>>> {
        guarded!(
            self,
            "meta.",
            get_derived(repo, key, type_name, version, read)
        )
    }
    async fn list_derived_by_type(
        &self,
        repo: RepoId,
        type_name: &str,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, u32, Vec<u8>)>> {
        guarded!(
            self,
            "meta.",
            list_derived_by_type(repo, type_name, after, limit)
        )
    }
    async fn delete_derived_type(&self, repo: RepoId, type_name: &str) -> Result<u64> {
        guarded!(self, "meta.", delete_derived_type(repo, type_name))
    }
    async fn get_ref(&self, repo: RepoId, name: &RefName) -> Result<Option<RefRow>> {
        guarded!(self, "meta.", get_ref(repo, name))
    }
    async fn list_refs(&self, repo: RepoId, prefix: &str) -> Result<Vec<RefRow>> {
        guarded!(self, "meta.", list_refs(repo, prefix))
    }
    async fn update_ref(&self, update: RefUpdate) -> Result<RefUpdated> {
        guarded!(self, "meta.", update_ref(update))
    }
    async fn ref_log(
        &self,
        repo: RepoId,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogEntry>> {
        guarded!(self, "meta.", ref_log(repo, name, from_version, limit))
    }
    async fn get_landed(
        &self,
        repo: RepoId,
        name: &RefName,
        change_ids: &[ChangeId],
    ) -> Result<Vec<Option<LandedRow>>> {
        guarded!(self, "meta.", get_landed(repo, name, change_ids))
    }
    async fn list_landed(&self, repo: RepoId, name: Option<&RefName>) -> Result<Vec<LandedRow>> {
        guarded!(self, "meta.", list_landed(repo, name))
    }
    async fn allocate_event_seqs(&self, repo: RepoId, n: u64) -> Result<Range<u64>> {
        guarded!(self, "meta.", allocate_event_seqs(repo, n))
    }
    async fn insert_events(&self, repo: RepoId, events: Vec<(u64, NewEvent)>) -> Result<()> {
        guarded!(self, "meta.", insert_events(repo, events))
    }
    async fn append_events(&self, repo: RepoId, events: Vec<NewEvent>) -> Result<Vec<u64>> {
        guarded!(self, "meta.", append_events(repo, events))
    }
    async fn read_events(
        &self,
        repo: RepoId,
        from_seq: u64,
        limit: usize,
        read: ReadFrom,
    ) -> Result<Vec<EventRow>> {
        guarded!(self, "meta.", read_events(repo, from_seq, limit, read))
    }
    async fn event_head(&self, repo: RepoId) -> Result<EventHead> {
        guarded!(self, "meta.", event_head(repo))
    }
    async fn redact(&self, row: RedactionRow) -> Result<Redacted> {
        guarded!(self, "meta.", redact(row))
    }
    async fn get_redactions(&self, org: OrgId, ids: &[ObjectId]) -> Result<Vec<RedactionRow>> {
        guarded!(self, "meta.", get_redactions(org, ids))
    }
    async fn list_redactions(&self, org: OrgId) -> Result<Vec<RedactionRow>> {
        guarded!(self, "meta.", list_redactions(org))
    }
    async fn redaction_epoch(&self, repo: RepoId) -> Result<u64> {
        guarded!(self, "meta.", redaction_epoch(repo))
    }
    async fn gc_begin(&self, org: OrgId, at_ms: u64) -> Result<GcEpoch> {
        guarded!(self, "meta.", gc_begin(org, at_ms))
    }
    async fn gc_sweep(&self, epoch: GcEpoch, plan: SweepPlan) -> Result<SweepOutcome> {
        guarded!(self, "meta.", gc_sweep(epoch, plan))
    }
    fn access_counters(&self) -> Option<Arc<AccessCounters>> {
        self.inner.access_counters()
    }
}

/// A [`BlobStore`] wrapper that checks the point `blob.<method>` before every call.
pub struct FaultyBlob<S> {
    inner: S,
    faults: Arc<FaultInjector>,
}

impl<S> FaultyBlob<S> {
    /// Wraps `inner`.
    pub fn new(inner: S, faults: Arc<FaultInjector>) -> FaultyBlob<S> {
        FaultyBlob { inner, faults }
    }

    /// The wrapped store.
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

#[async_trait]
impl<S: BlobStore> BlobStore for FaultyBlob<S> {
    async fn put(
        &self,
        org: OrgId,
        id: &ObjectId,
        bytes: Vec<u8>,
        class: StorageClass,
    ) -> Result<()> {
        guarded!(self, "blob.", put(org, id, bytes, class))
    }
    async fn get(&self, org: OrgId, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        guarded!(self, "blob.", get(org, id))
    }
    async fn get_range(
        &self,
        org: OrgId,
        id: &ObjectId,
        offset: u64,
        len: u64,
    ) -> Result<Option<Vec<u8>>> {
        guarded!(self, "blob.", get_range(org, id, offset, len))
    }
    async fn has(&self, org: OrgId, ids: &[ObjectId]) -> Result<Vec<bool>> {
        guarded!(self, "blob.", has(org, ids))
    }
    async fn stat(&self, org: OrgId, id: &ObjectId) -> Result<Option<BlobInfo>> {
        guarded!(self, "blob.", stat(org, id))
    }
    async fn delete(
        &self,
        org: OrgId,
        id: &ObjectId,
        written_before_ms: Option<u64>,
    ) -> Result<bool> {
        guarded!(self, "blob.", delete(org, id, written_before_ms))
    }
    async fn set_class(&self, org: OrgId, id: &ObjectId, class: StorageClass) -> Result<bool> {
        guarded!(self, "blob.", set_class(org, id, class))
    }
    async fn quarantine(&self, org: OrgId, id: &ObjectId) -> Result<bool> {
        guarded!(self, "blob.", quarantine(org, id))
    }
    async fn release(&self, org: OrgId, id: &ObjectId) -> Result<bool> {
        guarded!(self, "blob.", release(org, id))
    }
    async fn list(
        &self,
        org: OrgId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, BlobInfo)>> {
        guarded!(self, "blob.", list(org, after, limit))
    }
    fn access_counters(&self) -> Option<Arc<AccessCounters>> {
        self.inner.access_counters()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn rules_fire_by_point_and_hit() {
        let f = FaultInjector::new();
        f.add(FaultRule::nth("meta.get_ref", 1, Fault::unavailable()));
        assert_eq!(f.check("meta.get_ref").await.unwrap(), Proceed::Normally);
        let err = f.check("meta.get_ref").await.unwrap_err();
        assert_eq!(err.reason, Reason::Unavailable);
        assert!(err.retryable);
        assert_eq!(f.check("meta.get_ref").await.unwrap(), Proceed::Normally);
        assert_eq!(f.hits_of("meta.get_ref"), 3);
        assert_eq!(f.fired(), vec![("meta.get_ref".to_string(), 1)]);

        f.add(FaultRule::always("blob.*", Fault::response_lost()));
        assert!(matches!(
            f.check("blob.put").await.unwrap(),
            Proceed::ThenFail(_)
        ));
        assert!(matches!(
            f.check("blob.get").await.unwrap(),
            Proceed::ThenFail(_)
        ));
        assert_eq!(
            f.check("meta.put_objects").await.unwrap(),
            Proceed::Normally
        );
        f.clear_rules();
        assert_eq!(f.check("blob.put").await.unwrap(), Proceed::Normally);
        f.reset();
        assert!(f.hits().is_empty());
    }

    #[tokio::test]
    async fn guard_runs_op_for_fail_after_only() {
        let f = FaultInjector::new();
        let ran = std::sync::atomic::AtomicU32::new(0);
        let op = || async {
            ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, BeeError>(7)
        };
        f.add(FaultRule::nth("p", 0, Fault::unavailable()));
        f.add(FaultRule::nth("p", 0, Fault::response_lost()));
        assert!(f.guard("p", op()).await.is_err());
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(f.guard("p", op()).await.is_err());
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(f.guard("p", op()).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn delay_then_proceed() {
        let f = FaultInjector::new();
        f.add(FaultRule::nth(
            "p",
            0,
            Fault::Delay(Duration::from_millis(20)),
        ));
        let t = Instant::now();
        assert_eq!(f.check("p").await.unwrap(), Proceed::Normally);
        assert!(t.elapsed() >= Duration::from_millis(20));
    }

    #[tokio::test]
    async fn crash_panics_inside_the_task() {
        let f = FaultInjector::new();
        f.add(FaultRule::always("p", Fault::Crash));
        let f2 = f.clone();
        let res = tokio::spawn(async move { f2.check("p").await }).await;
        let err = res.unwrap_err();
        assert!(err.is_panic());
        let payload = err.into_panic();
        let msg = payload
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(is_injected_crash(&msg), "{msg}");
        assert_eq!(points::WRITE_STEPS.len(), 7);
    }
}
