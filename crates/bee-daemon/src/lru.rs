//! The recency index shared by both stores: sizes, an LRU order, pins and the watermarks.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bee_core::ObjectId;

/// Size and last-use tick of one held object.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Slot {
    pub size: u64,
    pub tick: u64,
}

/// LRU bookkeeping. Ticks are a logical clock, so the order never depends on wall time.
#[derive(Debug)]
pub(crate) struct Lru {
    pub entries: HashMap<ObjectId, Slot>,
    order: BTreeSet<(u64, ObjectId)>,
    pins: HashMap<ObjectId, u32>,
    pub pinsets: BTreeMap<String, BTreeSet<ObjectId>>,
    pub tick: u64,
    pub bytes: u64,
    pub capacity: u64,
    pub low: u64,
    pub evictions: u64,
    pub corrupt: u64,
}

impl Lru {
    /// An empty index evicting above `capacity` down to `low_pct` percent of it.
    pub fn new(capacity: u64, low_pct: u8) -> Self {
        let pct = u64::from(low_pct.clamp(1, 100));
        Self {
            entries: HashMap::new(),
            order: BTreeSet::new(),
            pins: HashMap::new(),
            pinsets: BTreeMap::new(),
            tick: 1,
            bytes: 0,
            capacity,
            low: capacity / 100 * pct + capacity % 100 * pct / 100,
            evictions: 0,
            corrupt: 0,
        }
    }

    /// Marks `id` as just used; false if it is not held.
    pub fn touch(&mut self, id: &ObjectId) -> bool {
        let tick = self.tick;
        let Some(slot) = self.entries.get_mut(id) else {
            return false;
        };
        self.order.remove(&(slot.tick, *id));
        slot.tick = tick;
        self.order.insert((tick, *id));
        self.tick += 1;
        true
    }

    /// Records a held object with an explicit tick (recovery) or as just used.
    pub fn insert(&mut self, id: ObjectId, size: u64, tick: Option<u64>) {
        if self.entries.contains_key(&id) {
            self.touch(&id);
            return;
        }
        let tick = tick.unwrap_or_else(|| {
            let t = self.tick;
            self.tick += 1;
            t
        });
        self.tick = self.tick.max(tick + 1);
        self.entries.insert(id, Slot { size, tick });
        self.order.insert((tick, id));
        self.bytes += size;
    }

    /// Forgets `id`; returns its size if it was held.
    pub fn remove(&mut self, id: &ObjectId) -> Option<u64> {
        let slot = self.entries.remove(id)?;
        self.order.remove(&(slot.tick, *id));
        self.bytes -= slot.size;
        Some(slot.size)
    }

    /// True if an in-memory pin or a named pin set holds `id`.
    pub fn is_pinned(&self, id: &ObjectId) -> bool {
        self.pins.get(id).is_some_and(|n| *n > 0) || self.pinsets.values().any(|s| s.contains(id))
    }

    /// Adds one in-memory pin per id.
    pub fn pin(&mut self, ids: &[ObjectId]) {
        for id in ids {
            *self.pins.entry(*id).or_insert(0) += 1;
        }
    }

    /// Releases one in-memory pin per id.
    pub fn unpin(&mut self, ids: &[ObjectId]) {
        for id in ids {
            if let Some(n) = self.pins.get_mut(id) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    self.pins.remove(id);
                }
            }
        }
    }

    /// Number of held objects that are pinned.
    pub fn pinned_count(&self) -> u64 {
        self.entries.keys().filter(|id| self.is_pinned(id)).count() as u64
    }

    /// When above capacity, removes unpinned objects oldest first until at or below the low
    /// watermark, and returns them (the caller deletes their bytes). The most recently used
    /// object is never chosen, so a put is always followed by a hit.
    pub fn evict(&mut self) -> Vec<ObjectId> {
        if self.bytes <= self.capacity {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut candidates: Vec<(u64, ObjectId)> = self.order.iter().copied().collect();
        candidates.pop();
        for (_, id) in candidates {
            if self.bytes <= self.low {
                break;
            }
            if self.is_pinned(&id) {
                continue;
            }
            self.remove(&id);
            self.evictions += 1;
            out.push(id);
        }
        out
    }

    /// Held ids with their sizes and ticks, oldest first.
    pub fn snapshot(&self) -> Vec<(ObjectId, Slot)> {
        self.order
            .iter()
            .filter_map(|(_, id)| self.entries.get(id).map(|s| (*id, *s)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ObjectId {
        bee_object::blob_id(&[n])
    }

    #[test]
    fn evicts_oldest_unpinned_down_to_the_low_watermark() {
        let mut l = Lru::new(100, 50);
        for n in 0..10 {
            l.insert(id(n), 10, None);
        }
        assert!(l.evict().is_empty(), "at capacity, nothing to do");
        l.touch(&id(0));
        l.pin(&[id(1)]);
        l.insert(id(10), 10, None);
        let gone = l.evict();
        assert_eq!(gone, vec![id(2), id(3), id(4), id(5), id(6), id(7)]);
        assert_eq!(l.bytes, 50);
        assert!(l.entries.contains_key(&id(0)) && l.entries.contains_key(&id(1)));
        l.unpin(&[id(1)]);
        assert!(!l.is_pinned(&id(1)));
    }

    #[test]
    fn named_pin_sets_hold_objects() {
        let mut l = Lru::new(10, 50);
        l.pinsets
            .insert("rev".into(), [id(1)].into_iter().collect());
        l.insert(id(1), 8, None);
        l.insert(id(2), 8, None);
        l.insert(id(3), 8, None);
        assert_eq!(l.evict(), vec![id(2)], "pinned and newest are kept");
        assert_eq!(l.pinned_count(), 1);
    }
}
