//! Content-addressed object stores behind a cache tier: [`crate::disk::DiskCache`] for the node
//! daemon and [`crate::memory::MemoryCache`] for the regional cache.
//!
//! Both are size-bounded and evict in approximate LRU order between watermarks (SPEC §10.2),
//! never evicting a pinned object. Every write and every read is verified against the object
//! id, so a store never hands out bytes that do not match the id they were asked for.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bee_core::{HashAlgo, ObjectId};
use bee_proto::v1::ErrorReason;
use bytes::Bytes;
use serde::Serialize;

use crate::error::{Result, err};

/// True when `bytes` are the object `id` names: BLAKE3 over the bytes (a blob's raw bytes, any
/// other kind's canonical encoding) equals the id's digest. Foreign ids are never verifiable.
pub fn verify(id: &ObjectId, bytes: &[u8]) -> bool {
    id.algo() == HashAlgo::Blake3 && bee_object::digest(bytes) == *id.digest()
}

/// Counters of one store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreStats {
    /// Objects held.
    pub objects: u64,
    /// Bytes held.
    pub bytes: u64,
    /// High watermark: eviction starts above it.
    pub capacity: u64,
    /// Objects evicted since start.
    pub evictions: u64,
    /// Objects dropped because their bytes no longer matched their id.
    pub corrupt_dropped: u64,
    /// Objects currently pinned (in-memory pins or named pin sets).
    pub pinned: u64,
}

/// A content-addressed, size-bounded object store.
pub trait ObjectStore: Send + Sync {
    /// The verified bytes of `id`, refreshing its recency; `None` on a miss. Bytes that no
    /// longer match the id are dropped and reported as a miss.
    fn get(&self, id: &ObjectId) -> Option<Bytes>;

    /// True if `id` is held (without reading or verifying it).
    fn contains(&self, id: &ObjectId) -> bool;

    /// Stores `bytes` as `id` after verifying them (`INTEGRITY` if they do not match), then
    /// evicts down to the low watermark if the store is above its capacity.
    fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<()>;

    /// Drops `id` (redacted upstream, or corrupt).
    fn remove(&self, id: &ObjectId);

    /// Adds an in-memory pin to each id (held or not); pinned objects are never evicted.
    fn pin(&self, ids: &[ObjectId]);

    /// Releases one in-memory pin of each id.
    fn unpin(&self, ids: &[ObjectId]);

    /// Replaces the named, persistent pin set `name` with `ids`.
    fn pin_set(&self, name: &str, ids: &[ObjectId]) -> Result<()>;

    /// Deletes the named pin set; returns whether it existed.
    fn unpin_set(&self, name: &str) -> Result<bool>;

    /// Names of the persistent pin sets with their sizes.
    fn pin_sets(&self) -> Vec<(String, usize)>;

    /// Where the file holding `id` lives, for stores that keep objects as files (snapshots
    /// link or clone from it); `None` for stores without files. Callers make sure the object
    /// is held (and pinned) first. The file was verified when written.
    fn file_path(&self, id: &ObjectId) -> Option<PathBuf>;

    /// Counters.
    fn stats(&self) -> StoreStats;

    /// Persists the recency index (a no-op for memory stores).
    fn flush(&self) -> Result<()>;
}

/// Pins a set of objects for as long as it lives.
pub struct PinGuard {
    store: Arc<dyn ObjectStore>,
    ids: Vec<ObjectId>,
}

impl PinGuard {
    /// Pins `ids` in `store`.
    pub fn new(store: Arc<dyn ObjectStore>, ids: Vec<ObjectId>) -> Self {
        store.pin(&ids);
        Self { store, ids }
    }
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        self.store.unpin(&self.ids);
    }
}

/// Named fault points for crash tests: arming `name` makes the `n`-th hit of that point fail
/// as if the process had died there, leaving whatever the step had written so far.
#[derive(Debug, Default)]
pub struct Faults {
    armed: Mutex<Option<(String, u32)>>,
}

impl Faults {
    /// No fault armed.
    pub fn none() -> Self {
        Self::default()
    }

    /// Arms `name` to fail on its `nth` hit (1 = the next one).
    pub fn arm(&self, name: &str, nth: u32) {
        if let Ok(mut a) = self.armed.lock() {
            *a = Some((name.to_owned(), nth.max(1)));
        }
    }

    /// Disarms every fault point.
    pub fn clear(&self) {
        if let Ok(mut a) = self.armed.lock() {
            *a = None;
        }
    }

    /// True if hitting `name` now fails (consumes the hit).
    pub fn fires(&self, name: &str) -> bool {
        let Ok(mut a) = self.armed.lock() else {
            return false;
        };
        match a.as_mut() {
            Some((n, left)) if n == name => {
                *left -= 1;
                if *left == 0 {
                    *a = None;
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Fails with `INTERNAL` when `name` fires.
    pub fn hit(&self, name: &str) -> Result<()> {
        if self.fires(name) {
            return Err(err(
                ErrorReason::Internal,
                format!("fault injected at {name}"),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_checks_the_digest() {
        let id = bee_object::blob_id(b"hello");
        assert!(verify(&id, b"hello"));
        assert!(!verify(&id, b"hellO"));
        let foreign = ObjectId::git_foreign(&[7u8; 20]).expect("foreign id");
        assert!(!verify(&foreign, b"anything"));
    }

    #[test]
    fn faults_fire_on_the_nth_hit_once() {
        let f = Faults::none();
        f.arm("x", 2);
        assert!(f.hit("y").is_ok());
        assert!(f.hit("x").is_ok());
        assert!(f.hit("x").is_err());
        assert!(f.hit("x").is_ok());
    }
}
