//! Reads of refs that survive a metadata-primary outage (SPEC §15.1).
//!
//! Every ref this process reads (or resolves) from the primary is remembered with the time it
//! was read. When the primary then answers `UNAVAILABLE`, a read of that ref returns the
//! remembered version marked stale, provided it was read at most
//! `metadata.stale_read_max_ms` ago; otherwise the read fails as before. Writes are never
//! served this way, and a stale answer older than a client's `min_version` is refused. The
//! repo's redaction epoch, which every ref read returns as a trailer, is kept the same way.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use bee_core::{ObjectId, RefName};
use bee_store::{RefRow, RepoId};

/// Most refs remembered; past it the oldest half is forgotten.
pub const MAX_REFS: usize = 100_000;

#[derive(Clone, Debug)]
struct Known {
    row: RefRow,
    root: Option<ObjectId>,
    read_ms: u64,
}

/// Last known versions of refs, and when a stale read was last served.
#[derive(Debug, Default)]
pub struct LastKnownRefs {
    refs: Mutex<HashMap<(RepoId, RefName), Known>>,
    /// Redaction epoch per repo (the epoch trailers of a ref read) and when it was read.
    epochs: Mutex<HashMap<RepoId, (u64, u64)>>,
    served_ms: AtomicU64,
    served_age_ms: AtomicU64,
}

impl LastKnownRefs {
    /// Remembers a repo's store redaction epoch, read from the primary at `now_ms`.
    pub fn record_epoch(&self, repo: RepoId, epoch: u64, now_ms: u64) {
        let mut epochs = self.epochs.lock().unwrap_or_else(|p| p.into_inner());
        if epochs.len() >= MAX_REFS && !epochs.contains_key(&repo) {
            epochs.clear();
        }
        epochs.insert(repo, (epoch, now_ms));
    }

    /// The last known redaction epoch of `repo`, if read at most `max_age_ms` ago.
    pub fn stale_epoch(&self, repo: RepoId, now_ms: u64, max_age_ms: u64) -> Option<u64> {
        let epochs = self.epochs.lock().unwrap_or_else(|p| p.into_inner());
        let (epoch, read_ms) = *epochs.get(&repo)?;
        (now_ms.saturating_sub(read_ms) <= max_age_ms).then_some(epoch)
    }

    /// Remembers `row` (read from the primary at `now_ms`) and, when known, the root tree of
    /// its target. A root already known for the same target is kept.
    pub fn record(&self, repo: RepoId, row: &RefRow, root: Option<ObjectId>, now_ms: u64) {
        let mut refs = self.refs.lock().unwrap_or_else(|p| p.into_inner());
        if refs.len() >= MAX_REFS {
            let mut ages: Vec<u64> = refs.values().map(|k| k.read_ms).collect();
            ages.sort_unstable();
            let cut = ages[ages.len() / 2];
            refs.retain(|_, k| k.read_ms > cut);
        }
        let key = (repo, row.name.clone());
        let root = root.or_else(|| {
            refs.get(&key)
                .filter(|k| k.row.target == row.target)
                .and_then(|k| k.root)
        });
        refs.insert(
            key,
            Known {
                row: row.clone(),
                root,
                read_ms: now_ms,
            },
        );
    }

    /// The last known row of `name` and its target's root tree, if it was read at most
    /// `max_age_ms` before `now_ms`. Records that a stale read was served.
    pub fn stale(
        &self,
        repo: RepoId,
        name: &RefName,
        now_ms: u64,
        max_age_ms: u64,
    ) -> Option<(RefRow, Option<ObjectId>)> {
        let refs = self.refs.lock().unwrap_or_else(|p| p.into_inner());
        let k = refs.get(&(repo, name.clone()))?;
        let age = now_ms.saturating_sub(k.read_ms);
        if age > max_age_ms {
            return None;
        }
        self.served_ms.store(now_ms, Ordering::Relaxed);
        self.served_age_ms.store(age, Ordering::Relaxed);
        Some((k.row.clone(), k.root))
    }

    /// When a stale read was last served (0 = never), and the age of what it served.
    pub fn last_served(&self) -> (u64, u64) {
        (
            self.served_ms.load(Ordering::Relaxed),
            self.served_age_ms.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(repo: RepoId, name: &str, version: u64, target: Option<ObjectId>) -> RefRow {
        RefRow {
            repo,
            name: RefName::new(name).unwrap(),
            target,
            version,
        }
    }

    #[test]
    fn serves_only_within_the_bound_and_keeps_roots() {
        let k = LastKnownRefs::default();
        let repo = RepoId(bee_core::Ulid::from_parts(1, 1).unwrap());
        let t = bee_object::Tree::empty().id();
        let main = RefName::new("main").unwrap();
        assert!(k.stale(repo, &main, 10, 1000).is_none());
        k.record(repo, &row(repo, "main", 3, Some(t)), Some(t), 100);
        // A later read without a root keeps the root of the same target.
        k.record(repo, &row(repo, "main", 3, Some(t)), None, 200);
        let (r, root) = k
            .stale(repo, &main, 1_200, 1_000)
            .expect("within the bound");
        assert_eq!((r.version, root), (3, Some(t)));
        assert_eq!(k.last_served(), (1_200, 1_000));
        assert!(k.stale(repo, &main, 1_201, 1_000).is_none(), "too old");
    }
}
