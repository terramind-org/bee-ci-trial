//! The regional cache's in-memory object store: objects are served straight from memory
//! (PRODUCT §4.5), bounded by bytes, evicted in LRU order, and lost on restart (a regional
//! cache keeps no long-lived state).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use bee_core::ObjectId;
use bee_proto::v1::ErrorReason;
use bytes::Bytes;

use crate::error::{Result, err};
use crate::lru::Lru;
use crate::store::{ObjectStore, StoreStats, verify};

struct Inner {
    lru: Lru,
    bytes: HashMap<ObjectId, Bytes>,
}

/// An in-memory, content-addressed, size-bounded object store.
pub struct MemoryCache {
    inner: Mutex<Inner>,
}

impl MemoryCache {
    /// A store holding at most `max_bytes`, evicting down to `low_watermark_pct` percent.
    pub fn new(max_bytes: u64, low_watermark_pct: u8) -> Self {
        Self {
            inner: Mutex::new(Inner {
                lru: Lru::new(max_bytes, low_watermark_pct),
                bytes: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl ObjectStore for MemoryCache {
    fn get(&self, id: &ObjectId) -> Option<Bytes> {
        let mut g = self.lock();
        let b = g.bytes.get(id).cloned()?;
        g.lru.touch(id);
        Some(b)
    }

    fn contains(&self, id: &ObjectId) -> bool {
        self.lock().bytes.contains_key(id)
    }

    fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<()> {
        if !verify(id, bytes) {
            return Err(err(
                ErrorReason::Integrity,
                format!("bytes do not match object {id}"),
            ));
        }
        let mut g = self.lock();
        if g.lru.touch(id) {
            return Ok(());
        }
        g.bytes.insert(*id, Bytes::copy_from_slice(bytes));
        g.lru.insert(*id, bytes.len() as u64, None);
        for victim in g.lru.evict() {
            g.bytes.remove(&victim);
        }
        Ok(())
    }

    fn remove(&self, id: &ObjectId) {
        let mut g = self.lock();
        g.lru.remove(id);
        g.bytes.remove(id);
    }

    fn pin(&self, ids: &[ObjectId]) {
        self.lock().lru.pin(ids);
    }

    fn unpin(&self, ids: &[ObjectId]) {
        let mut g = self.lock();
        g.lru.unpin(ids);
        for victim in g.lru.evict() {
            g.bytes.remove(&victim);
        }
    }

    fn pin_set(&self, name: &str, ids: &[ObjectId]) -> Result<()> {
        self.lock()
            .lru
            .pinsets
            .insert(name.to_owned(), ids.iter().copied().collect());
        Ok(())
    }

    fn unpin_set(&self, name: &str) -> Result<bool> {
        let mut g = self.lock();
        let existed = g.lru.pinsets.remove(name).is_some();
        for victim in g.lru.evict() {
            g.bytes.remove(&victim);
        }
        Ok(existed)
    }

    fn pin_sets(&self) -> Vec<(String, usize)> {
        self.lock()
            .lru
            .pinsets
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect()
    }

    fn file_path(&self, _id: &ObjectId) -> Option<PathBuf> {
        None
    }

    fn stats(&self) -> StoreStats {
        let g = self.lock();
        StoreStats {
            objects: g.bytes.len() as u64,
            bytes: g.lru.bytes,
            capacity: g.lru.capacity,
            evictions: g.lru.evictions,
            corrupt_dropped: g.lru.corrupt,
            pinned: g.lru.pinned_count(),
        }
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_lru_in_memory() {
        let m = MemoryCache::new(30, 70);
        let ids: Vec<(ObjectId, Vec<u8>)> = (0u8..4)
            .map(|n| {
                let b = vec![n; 10];
                (bee_object::blob_id(&b), b)
            })
            .collect();
        for (id, b) in &ids[..3] {
            m.put(id, b).expect("put");
        }
        m.pin(&[ids[0].0]);
        m.put(&ids[3].0, &ids[3].1).expect("put");
        assert!(m.contains(&ids[0].0));
        assert!(!m.contains(&ids[1].0));
        assert!(!m.contains(&ids[2].0));
        assert!(m.contains(&ids[3].0));
        assert!(m.put(&ids[1].0, b"wrong").is_err());
        assert_eq!(m.stats().objects, 2);
    }
}
