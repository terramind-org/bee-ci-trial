//! The pack cache (SPEC §17.4): fetch results keyed by the digest of everything that decides
//! them — repo, object format, wants, common haves, the client's shallow commits, the deepen
//! request, the filter and pack options. Objects are content-addressed, so a key always
//! describes the same pack; repeated clones of a busy tip are served from memory without
//! touching the stores.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bee_git::GitOid;

use crate::walk::{Deepen, FetchArgs, FetchPlan};

/// The digest a fetch is cached under.
pub fn key(repo: &str, args: &FetchArgs, common: &[GitOid], tags: &[GitOid]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    let mut put = |label: &str, bytes: &[u8]| {
        h.update(label.as_bytes());
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    put("repo", repo.as_bytes());
    let sorted = |v: &[GitOid]| {
        let mut v: Vec<Vec<u8>> = v.iter().map(|o| o.as_bytes().to_vec()).collect();
        v.sort();
        v.dedup();
        v.concat()
    };
    put("wants", &sorted(&args.wants));
    put("common", &sorted(common));
    put("shallow", &sorted(&args.shallow));
    put("tags", &sorted(tags));
    let deepen = match &args.deepen {
        None => String::new(),
        Some(Deepen::Depth(n)) => format!("depth:{n}"),
        Some(Deepen::Relative(n)) => format!("relative:{n}"),
        Some(Deepen::Since(t)) => format!("since:{t}"),
        Some(Deepen::Not(ids)) => {
            let mut v: Vec<String> = ids.iter().map(|i| i.to_text()).collect();
            v.sort();
            format!("not:{}", v.join(","))
        }
    };
    put("deepen", deepen.as_bytes());
    put(
        "filter",
        args.filter.map(|f| f.spec()).unwrap_or_default().as_bytes(),
    );
    put(
        "opts",
        &[
            u8::from(args.ofs_delta),
            u8::from(args.include_tag),
            u8::from(args.thin_pack),
        ],
    );
    *h.finalize().as_bytes()
}

struct Entry {
    plan: Arc<FetchPlan>,
    used: u64,
}

#[derive(Default)]
struct Inner {
    entries: BTreeMap<[u8; 32], Entry>,
    bytes: usize,
    tick: u64,
    hits: u64,
    misses: u64,
}

/// A bounded, least-recently-used cache of planned fetches.
pub struct PackCache {
    capacity: usize,
    inner: Mutex<Inner>,
}

impl PackCache {
    /// A cache holding packs up to `capacity` bytes in total (0 disables it).
    pub fn new(capacity: usize) -> PackCache {
        PackCache {
            capacity,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The cached plan for a key.
    pub fn get(&self, key: &[u8; 32]) -> Option<Arc<FetchPlan>> {
        let mut g = self.lock();
        g.tick += 1;
        let tick = g.tick;
        match g.entries.get_mut(key) {
            Some(e) => {
                e.used = tick;
                let plan = e.plan.clone();
                g.hits += 1;
                Some(plan)
            }
            None => {
                g.misses += 1;
                None
            }
        }
    }

    /// Stores a plan, evicting the least recently used ones beyond the capacity. A pack larger
    /// than half the capacity is not cached.
    pub fn put(&self, key: [u8; 32], plan: Arc<FetchPlan>) {
        let size = plan.pack.len();
        if self.capacity == 0 || size > self.capacity / 2 {
            return;
        }
        let mut g = self.lock();
        g.tick += 1;
        let used = g.tick;
        if let Some(old) = g.entries.insert(key, Entry { plan, used }) {
            g.bytes -= old.plan.pack.len();
        }
        g.bytes += size;
        while g.bytes > self.capacity {
            let Some(victim) = g
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| *k)
            else {
                break;
            };
            if let Some(e) = g.entries.remove(&victim) {
                g.bytes -= e.plan.pack.len();
            }
        }
    }

    /// Whether a plan is cached under `key`, without counting a hit or a miss or refreshing
    /// its place in the eviction order (prewarming asks this before building a pack).
    pub fn contains(&self, key: &[u8; 32]) -> bool {
        self.lock().entries.contains_key(key)
    }

    /// The capacity in bytes (0: the cache is disabled).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// `(hits, misses, cached bytes)`.
    pub fn stats(&self) -> (u64, u64, usize) {
        let g = self.lock();
        (g.hits, g.misses, g.bytes)
    }
}

/// A request's use of the cache: wraps the cache and remembers whether the request's pack was
/// found (reported to clients in the `X-Bee-Pack-Cache` response header).
pub struct CacheUse<'a> {
    cache: &'a PackCache,
    outcome: std::sync::atomic::AtomicU8,
}

impl<'a> CacheUse<'a> {
    /// A fresh use of `cache`.
    pub fn new(cache: &'a PackCache) -> CacheUse<'a> {
        CacheUse {
            cache,
            outcome: std::sync::atomic::AtomicU8::new(0),
        }
    }

    /// The cached plan for a key; records a hit or a miss.
    pub fn get(&self, key: &[u8; 32]) -> Option<Arc<FetchPlan>> {
        let found = self.cache.get(key);
        self.outcome.store(
            if found.is_some() { 2 } else { 1 },
            std::sync::atomic::Ordering::Relaxed,
        );
        found
    }

    /// Stores a plan.
    pub fn put(&self, key: [u8; 32], plan: Arc<FetchPlan>) {
        self.cache.put(key, plan);
    }

    /// `hit`, `miss`, or `none` when the request planned no pack.
    pub fn outcome(&self) -> &'static str {
        match self.outcome.load(std::sync::atomic::Ordering::Relaxed) {
            2 => "hit",
            1 => "miss",
            _ => "none",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(n: usize) -> Arc<FetchPlan> {
        Arc::new(FetchPlan {
            pack: vec![0; n],
            ..FetchPlan::default()
        })
    }

    #[test]
    fn keys_ignore_order_and_duplicates() {
        let a = GitOid::Sha1([1; 20]);
        let b = GitOid::Sha1([2; 20]);
        let mut x = FetchArgs {
            wants: vec![a, b],
            ..FetchArgs::default()
        };
        let k1 = key("r", &x, &[a], &[]);
        x.wants = vec![b, a, a];
        assert_eq!(k1, key("r", &x, &[a], &[]));
        assert_ne!(k1, key("r", &x, &[], &[]), "haves matter");
        assert_ne!(k1, key("s", &x, &[a], &[]), "repo matters");
        x.filter = Some(crate::walk::Filter::BlobNone);
        assert_ne!(k1, key("r", &x, &[a], &[]), "filter matters");
    }

    #[test]
    fn evicts_least_recently_used() {
        let c = PackCache::new(100);
        c.put([1; 32], plan(40));
        c.put([2; 32], plan(40));
        assert!(c.get(&[1; 32]).is_some());
        c.put([3; 32], plan(40));
        assert!(c.get(&[2; 32]).is_none(), "2 was least recently used");
        assert!(c.get(&[1; 32]).is_some());
        assert!(c.get(&[3; 32]).is_some());
        c.put([4; 32], plan(60));
        assert!(c.get(&[4; 32]).is_none(), "larger than half the capacity");
        let (hits, misses, bytes) = c.stats();
        assert_eq!((hits, misses, bytes), (3, 2, 80));
        assert!(c.contains(&[3; 32]) && !c.contains(&[4; 32]));
        assert_eq!(
            c.stats(),
            (3, 2, 80),
            "contains counts neither a hit nor a miss"
        );
    }
}
