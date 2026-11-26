//! The in-memory [`BlobStore`].

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use bee_core::{ObjectId, Result};

use crate::blob::{BlobInfo, BlobStore};
use crate::clock::Clock;
use crate::counters::AccessCounters;
use crate::types::{OrgId, StorageClass};

#[derive(Clone, Debug)]
struct Stored {
    bytes: Vec<u8>,
    info: BlobInfo,
}

/// An in-memory blob store. `put` is durable on return; quarantine and storage classes are
/// tracked per object; every call is counted.
#[derive(Debug)]
pub struct MemoryBlobStore {
    data: Mutex<BTreeMap<OrgId, BTreeMap<ObjectId, Stored>>>,
    clock: Arc<dyn Clock>,
    counters: Arc<AccessCounters>,
}

impl MemoryBlobStore {
    /// An empty store using `clock` for `written_at_ms`, with its own counters.
    pub fn new(clock: Arc<dyn Clock>) -> MemoryBlobStore {
        MemoryBlobStore::with_counters(clock, Arc::new(AccessCounters::new()))
    }

    /// An empty store counting into `counters`.
    pub fn with_counters(clock: Arc<dyn Clock>, counters: Arc<AccessCounters>) -> MemoryBlobStore {
        MemoryBlobStore {
            data: Mutex::new(BTreeMap::new()),
            clock,
            counters,
        }
    }

    /// The counters.
    pub fn counters(&self) -> &Arc<AccessCounters> {
        &self.counters
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<OrgId, BTreeMap<ObjectId, Stored>>> {
        self.data.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Test hook: replaces stored bytes without changing anything else (simulates bit rot).
    pub fn debug_overwrite(&self, org: OrgId, id: &ObjectId, bytes: Vec<u8>) {
        if let Some(s) = self.lock().get_mut(&org).and_then(|m| m.get_mut(id)) {
            s.info.len = bytes.len() as u64;
            s.bytes = bytes;
        }
    }

    /// Number of stored objects across orgs, quarantined included.
    pub fn len(&self) -> usize {
        self.lock().values().map(|m| m.len()).sum()
    }

    /// True when nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.lock().values().all(|m| m.is_empty())
    }
}

#[async_trait]
impl BlobStore for MemoryBlobStore {
    async fn put(
        &self,
        org: OrgId,
        id: &ObjectId,
        bytes: Vec<u8>,
        class: StorageClass,
    ) -> Result<()> {
        self.counters.blob_put();
        let now = self.clock.now_ms();
        let mut data = self.lock();
        let info = BlobInfo {
            len: bytes.len() as u64,
            class,
            quarantined: false,
            written_at_ms: now,
        };
        data.entry(org)
            .or_default()
            .insert(*id, Stored { bytes, info });
        Ok(())
    }

    async fn get(&self, org: OrgId, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        let data = self.lock();
        Ok(data
            .get(&org)
            .and_then(|m| m.get(id))
            .filter(|s| !s.info.quarantined)
            .map(|s| {
                self.counters.blob_get(
                    id.kind(),
                    s.bytes.len() as u64,
                    false,
                    s.info.class == StorageClass::Cold,
                );
                s.bytes.clone()
            }))
    }

    async fn get_range(
        &self,
        org: OrgId,
        id: &ObjectId,
        offset: u64,
        len: u64,
    ) -> Result<Option<Vec<u8>>> {
        let data = self.lock();
        Ok(data
            .get(&org)
            .and_then(|m| m.get(id))
            .filter(|s| !s.info.quarantined)
            .map(|s| {
                let total = s.bytes.len() as u64;
                let start = offset.min(total) as usize;
                let end = offset.saturating_add(len).min(total) as usize;
                let out = s.bytes[start..end].to_vec();
                self.counters.blob_get(
                    id.kind(),
                    out.len() as u64,
                    true,
                    s.info.class == StorageClass::Cold,
                );
                out
            }))
    }

    async fn has(&self, org: OrgId, ids: &[ObjectId]) -> Result<Vec<bool>> {
        self.counters.blob_has(ids.len() as u64);
        let data = self.lock();
        Ok(ids
            .iter()
            .map(|id| {
                data.get(&org)
                    .and_then(|m| m.get(id))
                    .is_some_and(|s| !s.info.quarantined)
            })
            .collect())
    }

    async fn stat(&self, org: OrgId, id: &ObjectId) -> Result<Option<BlobInfo>> {
        Ok(self
            .lock()
            .get(&org)
            .and_then(|m| m.get(id))
            .map(|s| s.info))
    }

    async fn delete(
        &self,
        org: OrgId,
        id: &ObjectId,
        written_before_ms: Option<u64>,
    ) -> Result<bool> {
        let mut data = self.lock();
        let eligible = data
            .get(&org)
            .and_then(|m| m.get(id))
            .is_some_and(|s| written_before_ms.is_none_or(|t| s.info.written_at_ms < t));
        if eligible {
            if let Some(m) = data.get_mut(&org) {
                m.remove(id);
            }
            self.counters.blob_delete();
        }
        Ok(eligible)
    }

    async fn set_class(&self, org: OrgId, id: &ObjectId, class: StorageClass) -> Result<bool> {
        // In memory the "copy to the new class, verify, then drop the old copy" sequence is
        // a single step; the bytes are never absent.
        Ok(
            match self.lock().get_mut(&org).and_then(|m| m.get_mut(id)) {
                Some(s) => {
                    s.info.class = class;
                    true
                }
                None => false,
            },
        )
    }

    async fn quarantine(&self, org: OrgId, id: &ObjectId) -> Result<bool> {
        Ok(
            match self.lock().get_mut(&org).and_then(|m| m.get_mut(id)) {
                Some(s) => {
                    s.info.quarantined = true;
                    true
                }
                None => false,
            },
        )
    }

    async fn release(&self, org: OrgId, id: &ObjectId) -> Result<bool> {
        Ok(
            match self.lock().get_mut(&org).and_then(|m| m.get_mut(id)) {
                Some(s) if s.info.quarantined => {
                    s.info.quarantined = false;
                    true
                }
                _ => false,
            },
        )
    }

    async fn list(
        &self,
        org: OrgId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, BlobInfo)>> {
        let data = self.lock();
        let Some(m) = data.get(&org) else {
            return Ok(Vec::new());
        };
        let iter: Box<dyn Iterator<Item = (&ObjectId, &Stored)>> = match after {
            Some(a) => Box::new(m.range((Excluded(a), Unbounded))),
            None => Box::new(m.iter()),
        };
        Ok(iter.take(limit).map(|(id, s)| (*id, s.info)).collect())
    }

    fn access_counters(&self) -> Option<Arc<AccessCounters>> {
        Some(self.counters.clone())
    }
}
