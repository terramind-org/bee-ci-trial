//! The node daemon's on-disk object cache.
//!
//! Layout under the cache root:
//!
//! | Path | Content |
//! |---|---|
//! | `objects/<tag>/<hh>/<64 hex>` | one object's bytes, read-only (`0444`); `<tag>` is the id's text tag (`b1`, `t1`, …) and `<hh>` the first two hex digits |
//! | `tmp/` | writes in progress; emptied at startup |
//! | `index` | the recency index (`SWDI` v1, BLAKE3 trailer), rewritten atomically |
//! | `pins.json` | named pin sets, rewritten atomically |
//!
//! Writes follow SPEC §10.2: temp file → verify → fsync (when enabled) → rename. Objects are
//! keyed only by id, so identical content from different repos (or orgs) is stored once.
//!
//! **Crash safety.** The object files are the truth; the index only remembers recency. At
//! startup the cache deletes `tmp/`, loads the index if its trailer verifies (otherwise starts
//! from an empty one), then reconciles it with the files: a file the index does not know is
//! adopted as least recently used, an index entry without a file is dropped. A file whose bytes
//! are torn or corrupt is found on read (every read is verified) and deleted. So a crash at any
//! point leaves either the old state, an orphaned temp file, or an object the next start adopts.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use bee_core::ObjectId;
use bee_proto::v1::ErrorReason;
use bytes::Bytes;

use crate::error::{Result, err, io};
use crate::lru::Lru;
use crate::store::{Faults, ObjectStore, StoreStats, verify};

const INDEX_MAGIC: &[u8; 4] = b"SWDI";
const INDEX_VERSION: u32 = 1;
/// Mutations between automatic index writes.
const INDEX_EVERY: u64 = 4096;

/// Settings of a disk cache.
#[derive(Clone, Debug)]
pub struct DiskConfig {
    /// Cache root directory.
    pub root: PathBuf,
    /// High watermark in bytes.
    pub max_bytes: u64,
    /// Low watermark as a percentage of `max_bytes` (eviction stops there).
    pub low_watermark_pct: u8,
    /// fsync each object before renaming it into place (SPEC §10.2).
    pub fsync: bool,
}

impl DiskConfig {
    /// Defaults for `root`: 10 GiB, evict down to 90%, fsync on.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_bytes: 10 << 30,
            low_watermark_pct: 90,
            fsync: true,
        }
    }
}

/// An on-disk, content-addressed, size-bounded object cache.
pub struct DiskCache {
    cfg: DiskConfig,
    lru: Mutex<Lru>,
    mutations: AtomicU64,
    seq: AtomicU64,
    /// Fault points: `put.write` (torn write, then crash), `put.rename` (crash before the
    /// rename), `put.index` (crash after the rename, before the index knows), `index.write`.
    pub faults: Faults,
}

fn tag_of(id: &ObjectId) -> (String, String) {
    let text = id.to_text();
    match text.split_once('_') {
        Some((tag, hex)) => (tag.to_owned(), hex.to_owned()),
        None => (String::from("x"), text),
    }
}

fn parse_entry_name(tag: &str, hex: &str) -> Option<ObjectId> {
    ObjectId::parse(&format!("{tag}_{hex}")).ok()
}

impl DiskCache {
    /// Opens (creating if needed) the cache at `cfg.root` and recovers its index.
    pub fn open(cfg: DiskConfig) -> Result<Self> {
        for dir in ["objects", "tmp"] {
            let p = cfg.root.join(dir);
            fs::create_dir_all(&p).map_err(|e| io(format!("create {}", p.display()), &e))?;
        }
        let tmp = cfg.root.join("tmp");
        for entry in fs::read_dir(&tmp).map_err(|e| io(format!("read {}", tmp.display()), &e))? {
            let entry = entry.map_err(|e| io(format!("read {}", tmp.display()), &e))?;
            let _ = fs::remove_file(entry.path());
        }
        let mut lru = Lru::new(cfg.max_bytes, cfg.low_watermark_pct);
        let known = read_index(&cfg.root.join("index")).unwrap_or_default();
        let mut found: Vec<(ObjectId, u64)> = Vec::new();
        let objects = cfg.root.join("objects");
        for tag in read_dirs(&objects)? {
            let tag_name = tag.file_name().to_string_lossy().into_owned();
            for fan in read_dirs(&tag.path())? {
                for file in read_dirs(&fan.path())? {
                    let name = file.file_name().to_string_lossy().into_owned();
                    let path = file.path();
                    match (parse_entry_name(&tag_name, &name), fs::metadata(&path)) {
                        (Some(id), Ok(meta)) if meta.is_file() => found.push((id, meta.len())),
                        _ => {
                            let _ = fs::remove_file(&path);
                        }
                    }
                }
            }
        }
        for (id, size) in found {
            let tick = known
                .get(&id)
                .filter(|(s, _)| *s == size)
                .map_or(0, |(_, t)| *t);
            lru.insert(id, size, Some(tick));
        }
        let pins_path = cfg.root.join("pins.json");
        if let Ok(bytes) = fs::read(&pins_path)
            && let Ok(sets) = serde_json::from_slice::<BTreeMap<String, Vec<String>>>(&bytes)
        {
            for (name, ids) in sets {
                let set: BTreeSet<ObjectId> =
                    ids.iter().filter_map(|t| ObjectId::parse(t).ok()).collect();
                lru.pinsets.insert(name, set);
            }
        }
        let cache = Self {
            cfg,
            lru: Mutex::new(lru),
            mutations: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            faults: Faults::none(),
        };
        let victims = cache.lock().evict();
        cache.delete_files(&victims);
        Ok(cache)
    }

    /// The configuration.
    pub fn config(&self) -> &DiskConfig {
        &self.cfg
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Lru> {
        self.lru.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Where `id`'s bytes live (whether or not they are there).
    pub fn object_path(&self, id: &ObjectId) -> PathBuf {
        let (tag, hex) = tag_of(id);
        let fan = hex.get(..2).unwrap_or("00").to_owned();
        self.cfg.root.join("objects").join(tag).join(fan).join(hex)
    }

    fn delete_files(&self, ids: &[ObjectId]) {
        for id in ids {
            let path = self.object_path(id);
            let _ = fs::remove_file(&path);
        }
    }

    fn mutated(&self) {
        if self.mutations.fetch_add(1, Ordering::Relaxed) % INDEX_EVERY == INDEX_EVERY - 1 {
            let _ = self.flush();
        }
    }

    fn write_object(&self, id: &ObjectId, bytes: &[u8]) -> Result<()> {
        let tmp = self.cfg.root.join("tmp").join(format!(
            "{}-{}.tmp",
            std::process::id(),
            self.seq.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| io(format!("create {}", tmp.display()), &e))?;
        if self.faults.fires("put.write") {
            // A torn write: half the bytes land, then the process "dies" after the rename.
            let half = bytes.len() / 2;
            f.write_all(&bytes[..half])
                .map_err(|e| io(format!("write {}", tmp.display()), &e))?;
            drop(f);
            let dest = self.object_path(id);
            if let Some(parent) = dest.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::rename(&tmp, &dest);
            return Err(err(ErrorReason::Internal, "fault injected at put.write"));
        }
        f.write_all(bytes)
            .map_err(|e| io(format!("write {}", tmp.display()), &e))?;
        if self.cfg.fsync {
            f.sync_all()
                .map_err(|e| io(format!("fsync {}", tmp.display()), &e))?;
        }
        drop(f);
        let mut perms = fs::metadata(&tmp)
            .map_err(|e| io(format!("stat {}", tmp.display()), &e))?
            .permissions();
        perms.set_readonly(true);
        fs::set_permissions(&tmp, perms).map_err(|e| io(format!("chmod {}", tmp.display()), &e))?;
        self.faults.hit("put.rename")?;
        let dest = self.object_path(id);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| io(format!("create {}", parent.display()), &e))?;
        }
        fs::rename(&tmp, &dest).map_err(|e| io(format!("rename {}", dest.display()), &e))?;
        self.faults.hit("put.index")?;
        Ok(())
    }

    fn save_pins(&self, lru: &Lru) -> Result<()> {
        let sets: BTreeMap<&String, Vec<String>> = lru
            .pinsets
            .iter()
            .map(|(k, v)| (k, v.iter().map(ObjectId::to_text).collect()))
            .collect();
        let json = serde_json::to_vec_pretty(&sets)
            .map_err(|e| err(ErrorReason::Internal, e.to_string()))?;
        atomic_write(&self.cfg.root.join("pins.json"), &json)
    }
}

fn read_dirs(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut out = Vec::new();
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(io(format!("read {}", dir.display()), &e)),
    };
    for entry in read {
        out.push(entry.map_err(|e| io(format!("read {}", dir.display()), &e))?);
    }
    Ok(out)
}

/// Writes `path` atomically: a temp file next to it, then a rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp-write");
    fs::write(&tmp, bytes).map_err(|e| io(format!("write {}", tmp.display()), &e))?;
    fs::rename(&tmp, path).map_err(|e| io(format!("rename {}", path.display()), &e))
}

fn encode_index(entries: &[(ObjectId, u64, u64)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + entries.len() * 50 + 32);
    out.extend_from_slice(INDEX_MAGIC);
    out.extend_from_slice(&INDEX_VERSION.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for (id, size, tick) in entries {
        out.extend_from_slice(id.as_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&tick.to_le_bytes());
    }
    let trailer = bee_object::digest(&out);
    out.extend_from_slice(&trailer);
    out
}

fn read_index(path: &Path) -> Option<HashMap<ObjectId, (u64, u64)>> {
    let bytes = fs::read(path).ok()?;
    decode_index(&bytes)
}

fn decode_index(bytes: &[u8]) -> Option<HashMap<ObjectId, (u64, u64)>> {
    if bytes.len() < 16 + 32 {
        return None;
    }
    let (body, trailer) = bytes.split_at(bytes.len() - 32);
    if bee_object::digest(body) != trailer {
        return None;
    }
    if &body[..4] != INDEX_MAGIC || body[4..8] != INDEX_VERSION.to_le_bytes() {
        return None;
    }
    let n = u64::from_le_bytes(body[8..16].try_into().ok()?) as usize;
    let rec = 34 + 8 + 8;
    if body.len() != 16 + n.checked_mul(rec)? {
        return None;
    }
    let mut out = HashMap::with_capacity(n);
    for chunk in body[16..].chunks_exact(rec) {
        let id = ObjectId::from_bytes(&chunk[..34]).ok()?;
        let size = u64::from_le_bytes(chunk[34..42].try_into().ok()?);
        let tick = u64::from_le_bytes(chunk[42..50].try_into().ok()?);
        out.insert(id, (size, tick));
    }
    Some(out)
}

impl ObjectStore for DiskCache {
    fn get(&self, id: &ObjectId) -> Option<Bytes> {
        if !self.lock().entries.contains_key(id) {
            return None;
        }
        let path = self.object_path(id);
        match fs::read(&path) {
            Ok(bytes) if verify(id, &bytes) => {
                self.lock().touch(id);
                Some(Bytes::from(bytes))
            }
            Ok(_) => {
                let _ = fs::remove_file(&path);
                let mut l = self.lock();
                l.remove(id);
                l.corrupt += 1;
                None
            }
            Err(_) => {
                self.lock().remove(id);
                None
            }
        }
    }

    fn contains(&self, id: &ObjectId) -> bool {
        self.lock().entries.contains_key(id)
    }

    fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<()> {
        if !verify(id, bytes) {
            return Err(err(
                ErrorReason::Integrity,
                format!("bytes do not match object {id}"),
            ));
        }
        if self.lock().touch(id) {
            return Ok(());
        }
        self.write_object(id, bytes)?;
        let victims = {
            let mut l = self.lock();
            l.insert(*id, bytes.len() as u64, None);
            l.evict()
        };
        self.delete_files(&victims);
        self.mutated();
        Ok(())
    }

    fn remove(&self, id: &ObjectId) {
        if self.lock().remove(id).is_some() {
            self.delete_files(&[*id]);
            self.mutated();
        }
    }

    fn pin(&self, ids: &[ObjectId]) {
        self.lock().pin(ids);
    }

    fn unpin(&self, ids: &[ObjectId]) {
        let victims = {
            let mut l = self.lock();
            l.unpin(ids);
            l.evict()
        };
        self.delete_files(&victims);
    }

    fn pin_set(&self, name: &str, ids: &[ObjectId]) -> Result<()> {
        let mut l = self.lock();
        l.pinsets
            .insert(name.to_owned(), ids.iter().copied().collect());
        self.save_pins(&l)
    }

    fn unpin_set(&self, name: &str) -> Result<bool> {
        let (existed, victims) = {
            let mut l = self.lock();
            let existed = l.pinsets.remove(name).is_some();
            self.save_pins(&l)?;
            (existed, l.evict())
        };
        self.delete_files(&victims);
        Ok(existed)
    }

    fn pin_sets(&self) -> Vec<(String, usize)> {
        self.lock()
            .pinsets
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect()
    }

    fn file_path(&self, id: &ObjectId) -> Option<PathBuf> {
        // No index lock: snapshots call this per file from many threads, with the objects
        // pinned and just ensured. A missing file surfaces as an error on use.
        Some(self.object_path(id))
    }

    fn stats(&self) -> StoreStats {
        let l = self.lock();
        StoreStats {
            objects: l.entries.len() as u64,
            bytes: l.bytes,
            capacity: l.capacity,
            evictions: l.evictions,
            corrupt_dropped: l.corrupt,
            pinned: l.pinned_count(),
        }
    }

    fn flush(&self) -> Result<()> {
        let entries: Vec<(ObjectId, u64, u64)> = self
            .lock()
            .snapshot()
            .into_iter()
            .map(|(id, s)| (id, s.size, s.tick))
            .collect();
        let bytes = encode_index(&entries);
        let path = self.cfg.root.join("index");
        if self.faults.fires("index.write") {
            let tmp = path.with_extension("tmp-write");
            let _ = fs::write(&tmp, &bytes[..bytes.len() / 2]);
            return Err(err(ErrorReason::Internal, "fault injected at index.write"));
        }
        atomic_write(&path, &bytes)
    }
}

impl Drop for DiskCache {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(dir: &Path, max: u64) -> DiskCache {
        let mut cfg = DiskConfig::new(dir);
        cfg.max_bytes = max;
        cfg.low_watermark_pct = 50;
        cfg.fsync = false;
        DiskCache::open(cfg).expect("open")
    }

    fn blob(n: u32, len: usize) -> (ObjectId, Vec<u8>) {
        let mut b = n.to_le_bytes().to_vec();
        b.resize(len, b'x');
        (bee_object::blob_id(&b), b)
    }

    #[test]
    fn put_get_verify_and_dedup() {
        let dir = tempfile::tempdir().expect("tmp");
        let c = cache(dir.path(), 1 << 20);
        let (id, b) = blob(1, 100);
        assert!(
            c.put(&id, b"not it").is_err(),
            "unverified bytes are refused"
        );
        c.put(&id, &b).expect("put");
        c.put(&id, &b).expect("second put is a touch");
        assert_eq!(c.get(&id).as_deref(), Some(b.as_slice()));
        assert_eq!(c.stats().objects, 1);
        assert_eq!(c.stats().bytes, 100);
        let path = c.object_path(&id);
        assert!(fs::metadata(&path).expect("file").permissions().readonly());
    }

    #[test]
    fn lru_eviction_respects_pins_and_survives_restart() {
        let dir = tempfile::tempdir().expect("tmp");
        let blobs: Vec<_> = (0..10).map(|n| blob(n, 100)).collect();
        {
            let c = cache(dir.path(), 1000);
            for (id, b) in &blobs {
                c.put(id, b).expect("put");
            }
            c.pin(&[blobs[0].0]);
            c.pin_set("keep", &[blobs[1].0]).expect("pin set");
            assert!(c.get(&blobs[2].0).is_some(), "touch 2");
            let (id, b) = blob(99, 100);
            c.put(&id, &b).expect("put over capacity");
            // 1100 bytes > 1000: evict unpinned oldest (3, 4, 5, 6, 7, 8) down to 500.
            for (n, (id, _)) in blobs.iter().enumerate().take(9).skip(3) {
                assert!(!c.contains(id), "{n} evicted");
                assert!(!c.object_path(id).exists());
            }
            for n in [0, 1, 2, 9] {
                assert!(c.contains(&blobs[n].0), "{n} kept");
            }
            assert_eq!(c.stats().evictions, 6);
        }
        // Restart: the in-memory pin is gone, the named pin set and the recency survive.
        let c = cache(dir.path(), 1000);
        assert_eq!(c.stats().objects, 5);
        assert_eq!(c.pin_sets(), vec![("keep".to_owned(), 1)]);
        let snapshot: Vec<ObjectId> = c.lock().snapshot().into_iter().map(|(i, _)| i).collect();
        assert_eq!(
            snapshot.last(),
            Some(&blob(99, 100).0),
            "newest stays newest"
        );
        assert!(c.unpin_set("keep").expect("unpin"));
        assert!(!c.unpin_set("keep").expect("unpin twice"));
    }

    #[test]
    fn crash_at_every_write_step_recovers() {
        let dir = tempfile::tempdir().expect("tmp");
        let (a, ab) = blob(1, 64);
        let (b, bb) = blob(2, 64);
        let (c_id, cb) = blob(3, 64);
        let (d, db) = blob(4, 64);
        {
            let c = cache(dir.path(), 1 << 20);
            c.put(&a, &ab).expect("put a");
            c.flush().expect("flush");
            // Crash before the rename: an orphaned temp file.
            c.faults.arm("put.rename", 1);
            assert!(c.put(&b, &bb).is_err());
            // Crash after the rename, before the index knew.
            c.faults.arm("put.index", 1);
            assert!(c.put(&c_id, &cb).is_err());
            // A torn write that still got renamed into place.
            c.faults.arm("put.write", 1);
            assert!(c.put(&d, &db).is_err());
            // Crash while rewriting the index: the old index stays valid.
            c.faults.arm("index.write", 1);
            assert!(c.flush().is_err());
            std::mem::forget(c); // no orderly shutdown
        }
        let c = cache(dir.path(), 1 << 20);
        assert_eq!(
            fs::read_dir(dir.path().join("tmp")).expect("tmp").count(),
            0
        );
        assert_eq!(c.get(&a).as_deref(), Some(ab.as_slice()));
        assert!(!c.contains(&b), "never renamed: gone");
        assert_eq!(c.get(&c_id).as_deref(), Some(cb.as_slice()), "adopted");
        assert!(c.contains(&d), "the torn file is adopted by name");
        assert!(c.get(&d).is_none(), "and dropped on its first read");
        assert!(!c.object_path(&d).exists());
        assert_eq!(c.stats().corrupt_dropped, 1);
        c.put(&d, &db).expect("refetch");
        assert_eq!(c.get(&d).as_deref(), Some(db.as_slice()));
    }

    #[test]
    fn corrupt_index_is_rebuilt_from_files() {
        let dir = tempfile::tempdir().expect("tmp");
        let (a, ab) = blob(1, 10);
        {
            let c = cache(dir.path(), 1 << 20);
            c.put(&a, &ab).expect("put");
        }
        fs::write(dir.path().join("index"), b"garbage").expect("clobber");
        let c = cache(dir.path(), 1 << 20);
        assert_eq!(c.get(&a).as_deref(), Some(ab.as_slice()));
    }

    #[test]
    fn index_roundtrip_rejects_damage() {
        let id = blob(5, 3).0;
        let enc = encode_index(&[(id, 3, 7)]);
        assert_eq!(decode_index(&enc).expect("ok").get(&id), Some(&(3, 7)));
        let mut bad = enc.clone();
        bad[20] ^= 1;
        assert!(decode_index(&bad).is_none());
    }
}
