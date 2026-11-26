//! Integrity alerting (PRODUCT §4.22): every time stored bytes fail verification the server
//! counts it in `bee_integrity_failures_total{source}`, writes an `error` log line from the
//! `integrity` component (object id, source, detail; never content), and reports the
//! `integrity` health component as down for an hour after the last failure.
//!
//! [`IntegrityWatch`] wraps the blob store the server reads through: a whole-object read whose
//! bytes do not hash to the requested id, or any read the store itself refuses with
//! `INTEGRITY` (bit rot, truncation, bad frames), raises an alert with source `blob_store`.
//! `RunFsck` raises one per digest, canonical-encoding, size or missing-bytes violation with
//! source `fsck`.

use std::sync::Arc;

use async_trait::async_trait;
use bee_core::{BeeError, ObjectId, Reason, Result};
use bee_store::{AccessCounters, BlobInfo, BlobStore, OrgId, StorageClass};

use crate::metrics::Metrics;

/// How long after the last failure the `integrity` health component stays down.
pub const ALERT_WINDOW_MS: u64 = 60 * 60 * 1000;

/// A blob store whose reads raise integrity alerts.
pub struct IntegrityWatch {
    inner: Arc<dyn BlobStore>,
    metrics: Arc<Metrics>,
    clock: Arc<dyn bee_store::Clock>,
}

impl IntegrityWatch {
    /// Wraps `inner`, recording alerts in `metrics`.
    pub fn new(
        inner: Arc<dyn BlobStore>,
        metrics: Arc<Metrics>,
        clock: Arc<dyn bee_store::Clock>,
    ) -> IntegrityWatch {
        IntegrityWatch {
            inner,
            metrics,
            clock,
        }
    }

    fn alert(&self, id: &ObjectId, detail: &str) {
        self.metrics
            .integrity_failure("blob_store", &id.to_text(), detail, self.clock.now_ms());
    }

    fn watch<T>(&self, id: &ObjectId, r: Result<T>) -> Result<T> {
        if let Err(e) = &r
            && e.reason == Reason::Integrity
        {
            self.alert(id, &e.message);
        }
        r
    }
}

#[async_trait]
impl BlobStore for IntegrityWatch {
    async fn put(
        &self,
        org: OrgId,
        id: &ObjectId,
        bytes: Vec<u8>,
        class: StorageClass,
    ) -> Result<()> {
        self.inner.put(org, id, bytes, class).await
    }

    async fn get(&self, org: OrgId, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        let got = self.watch(id, self.inner.get(org, id).await)?;
        if let Some(bytes) = &got
            && bee_object::hash_id(id.kind(), bytes) != *id
        {
            let detail = format!("stored bytes do not hash to {id}");
            self.alert(id, &detail);
            return Err(
                BeeError::new(Reason::Integrity, detail).with_payload("object", id.to_text())
            );
        }
        Ok(got)
    }

    async fn get_range(
        &self,
        org: OrgId,
        id: &ObjectId,
        offset: u64,
        len: u64,
    ) -> Result<Option<Vec<u8>>> {
        let r = self.inner.get_range(org, id, offset, len).await;
        self.watch(id, r)
    }

    async fn has(&self, org: OrgId, ids: &[ObjectId]) -> Result<Vec<bool>> {
        self.inner.has(org, ids).await
    }

    async fn stat(&self, org: OrgId, id: &ObjectId) -> Result<Option<BlobInfo>> {
        let r = self.inner.stat(org, id).await;
        self.watch(id, r)
    }

    async fn delete(
        &self,
        org: OrgId,
        id: &ObjectId,
        written_before_ms: Option<u64>,
    ) -> Result<bool> {
        self.inner.delete(org, id, written_before_ms).await
    }

    async fn set_class(&self, org: OrgId, id: &ObjectId, class: StorageClass) -> Result<bool> {
        let r = self.inner.set_class(org, id, class).await;
        self.watch(id, r)
    }

    async fn quarantine(&self, org: OrgId, id: &ObjectId) -> Result<bool> {
        self.inner.quarantine(org, id).await
    }

    async fn release(&self, org: OrgId, id: &ObjectId) -> Result<bool> {
        self.inner.release(org, id).await
    }

    async fn list(
        &self,
        org: OrgId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, BlobInfo)>> {
        self.inner.list(org, after, limit).await
    }

    fn access_counters(&self) -> Option<Arc<AccessCounters>> {
        self.inner.access_counters()
    }
}

/// True for fsck violation kinds that mean stored bytes failed verification.
pub fn is_integrity_violation(kind: &str) -> bool {
    matches!(
        kind,
        "digest_mismatch"
            | "changeset_id_mismatch"
            | "non_canonical"
            | "size_mismatch"
            | "missing_bytes"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_store::memory::MemoryBlobStore;

    #[tokio::test]
    async fn reads_that_fail_verification_raise_alerts() {
        let metrics = Arc::new(Metrics::new());
        let clock: Arc<dyn bee_store::Clock> = Arc::new(bee_store::ManualClock::new(5_000));
        let mem = Arc::new(MemoryBlobStore::new(clock.clone()));
        let org = OrgId(bee_core::Ulid::from_bytes([1; 16]));
        let good = b"good bytes".to_vec();
        let id = bee_object::blob_id(&good);
        let other = bee_object::blob_id(b"other");
        // Bytes stored under the wrong id: what bit rot that keeps a valid frame looks like.
        mem.put(org, &other, good.clone(), StorageClass::Hot)
            .await
            .unwrap();
        mem.put(org, &id, good.clone(), StorageClass::Hot)
            .await
            .unwrap();
        let watch = IntegrityWatch::new(mem, metrics.clone(), clock);
        assert_eq!(watch.get(org, &id).await.unwrap(), Some(good));
        assert_eq!(metrics.integrity_failures(), 0);
        let e = watch.get(org, &other).await.unwrap_err();
        assert_eq!(e.reason, Reason::Integrity);
        assert_eq!(metrics.integrity_failures(), 1);
        let last = metrics.last_integrity_failure().expect("recorded");
        assert_eq!(last.source, "blob_store");
        assert_eq!(last.object, other.to_text());
        assert_eq!(last.at_ms, 5_000);
        let text = metrics.render();
        assert!(
            text.contains("bee_integrity_failures_total{source=\"blob_store\"} 1"),
            "{text}"
        );
        assert!(is_integrity_violation("digest_mismatch"));
        assert!(!is_integrity_violation("ref_log_gap"));
    }
}
