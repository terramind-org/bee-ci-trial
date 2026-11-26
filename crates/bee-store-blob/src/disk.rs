//! [`DiskBlobStore`]: the blob store on a local directory (SPEC §6.1, §6.5).
//!
//! # Layout
//!
//! ```text
//! <root>/hot/<blob_key>     servable bytes in the hot class   (o/<org>/<kind>/<xx>/<digest>)
//! <root>/cold/<blob_key>    servable bytes in the cold class
//! <root>/hot/q/...          quarantined bytes, same key under the `q/` prefix
//! <root>/cold/q/...
//! <root>/tmp/               in-flight writes; emptied when the store is opened
//! ```
//!
//! The hot and cold classes are two subtrees with the same key layout. Moving between them
//! copies the file, verifies the copy against the id, and only then deletes the original, so
//! the bytes are never absent; reads look in both subtrees. Quarantine is a rename to the
//! `q/` prefix, which `get`, `get_range` and `has` do not serve.
//!
//! # File format
//!
//! Each file is a 24-byte header followed by one zstd frame (with content checksum):
//!
//! | Bytes | Meaning |
//! |---|---|
//! | 0..4 | magic `SWBL` |
//! | 4 | format version (1) |
//! | 5..8 | zero |
//! | 8..16 | `written_at_ms`, little endian |
//! | 16..24 | uncompressed length, little endian |
//!
//! The compression level is not part of identity: ids hash the uncompressed bytes, and any
//! level (or a store configured with a different level) reads any file. The written time
//! lives in the header so renames and class copies preserve it; a re-`put` rewrites the
//! file and refreshes it, which is what GC's `written_before_ms` rule relies on.
//!
//! # Durability
//!
//! A write goes to a temp file under `tmp/`, is flushed with [`File::sync_all`], and is
//! renamed into place; the parent directory is then synced so the rename survives power
//! loss. On macOS plain `fsync(2)` does not reach stable storage, but Rust's standard
//! library knows this: in `library/std/src/sys/fs/unix.rs`, `File::fsync` (behind
//! `File::sync_all`) is implemented on `target_vendor = "apple"` as
//! `fcntl(fd, F_FULLFSYNC)` and on every other Unix as `fsync(fd)`. So `sync_all` is already
//! the strongest flush on each platform, with no `unsafe` needed here.
//!
//! # Verify on read
//!
//! [`BlobStore::get`] hashes the decoded bytes against the id and reports a mismatch, a bad
//! header, a truncated file or a zstd checksum failure as `INTEGRITY`. Range reads decode
//! only as far as the range needs; with [`DiskBlobConfig::verify_range_reads`] (off by
//! default, as the trait leaves range verification to callers) they verify the whole object
//! first.
//!
//! # Concurrency
//!
//! One process owns a root. Mutations of one id are serialised by a striped lock; reads
//! take no lock and retry across the two class subtrees, so a concurrent class move never
//! makes bytes look absent. Everything blocking runs on tokio's blocking pool, so the store
//! must be used inside a tokio runtime.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bee_core::{ObjectId, Result};
use bee_store::faults::Proceed;
use bee_store::{
    AccessCounters, BlobInfo, BlobStore, Clock, FaultInjector, OrgId, StorageClass, SystemClock,
    blob_key,
};
use tokio::sync::Mutex;

use crate::common::{
    clamp_range, corrupt, id_from_digest_hex, internal, io_error, listing_kinds, verify_bytes,
};

const MAGIC: &[u8; 4] = b"SWBL";
const FORMAT_VERSION: u8 = 1;
const HEADER_LEN: usize = 24;
const LOCK_STRIPES: usize = 64;
/// Largest buffer reserved up front from an untrusted length field.
const MAX_PREALLOC: u64 = 64 * 1024 * 1024;

/// Fault points checked by [`DiskBlobStore`] when built with
/// [`DiskBlobStore::with_faults`], in addition to the `blob.<method>` points
/// [`bee_store::FaultyBlob`] provides. Every point sits between two durable steps, so
/// crashing at it leaves a state a real crash could leave.
pub mod points {
    /// `put`: the temp file is durable but not yet renamed into place.
    pub const PUT_TEMP_WRITTEN: &str = "disk.put.temp_written";
    /// `put`: the bytes are in place but copies in other classes or quarantine remain.
    pub const PUT_INSTALLED: &str = "disk.put.installed";
    /// `set_class`: the copy in the new class is durable and verified; the old copy remains.
    pub const SET_CLASS_COPIED: &str = "disk.set_class.copied";
}

/// Settings of a [`DiskBlobStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskBlobConfig {
    /// zstd compression level for new files (SPEC §6.1's `zstd` setting); default 3. Not part
    /// of identity: any level reads any file.
    pub zstd_level: i32,
    /// Verify the whole object against its id before serving a range. Costs a full read per
    /// range; default off because callers verify ranges against chunk ids.
    pub verify_range_reads: bool,
}

impl Default for DiskBlobConfig {
    fn default() -> DiskBlobConfig {
        DiskBlobConfig {
            zstd_level: 3,
            verify_range_reads: false,
        }
    }
}

/// Where one copy of an object can live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
    class: StorageClass,
    quarantined: bool,
}

/// Every slot, in the order lookups prefer them: servable before quarantined, hot before cold.
const SLOTS: [Slot; 4] = [
    Slot {
        class: StorageClass::Hot,
        quarantined: false,
    },
    Slot {
        class: StorageClass::Cold,
        quarantined: false,
    },
    Slot {
        class: StorageClass::Hot,
        quarantined: true,
    },
    Slot {
        class: StorageClass::Cold,
        quarantined: true,
    },
];

fn class_dir(class: StorageClass) -> &'static str {
    match class {
        StorageClass::Hot => "hot",
        StorageClass::Cold => "cold",
    }
}

#[derive(Clone, Copy, Debug)]
struct Header {
    written_at_ms: u64,
    raw_len: u64,
}

impl Header {
    fn encode(self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..4].copy_from_slice(MAGIC);
        out[4] = FORMAT_VERSION;
        out[8..16].copy_from_slice(&self.written_at_ms.to_le_bytes());
        out[16..24].copy_from_slice(&self.raw_len.to_le_bytes());
        out
    }

    fn read(id: &ObjectId, file: &mut File) -> Result<Header> {
        let mut raw = [0u8; HEADER_LEN];
        file.read_exact(&mut raw)
            .map_err(|_| corrupt(id, "file shorter than its header"))?;
        if &raw[..4] != MAGIC || raw[4] != FORMAT_VERSION {
            return Err(corrupt(id, "unrecognised file header"));
        }
        Ok(Header {
            written_at_ms: u64::from_le_bytes(raw[8..16].try_into().unwrap_or_default()),
            raw_len: u64::from_le_bytes(raw[16..24].try_into().unwrap_or_default()),
        })
    }
}

/// The state shared with blocking tasks.
#[derive(Debug)]
struct Inner {
    root: PathBuf,
    tmp: PathBuf,
    config: DiskBlobConfig,
    tmp_seq: AtomicU64,
}

/// A blob store on a local directory. See the [module docs](self).
#[derive(Debug)]
pub struct DiskBlobStore {
    inner: Arc<Inner>,
    clock: Arc<dyn Clock>,
    counters: Arc<AccessCounters>,
    faults: Option<Arc<FaultInjector>>,
    locks: Vec<Mutex<()>>,
}

impl DiskBlobStore {
    /// Opens (creating if needed) the store rooted at `root`, with the system clock and the
    /// default configuration. Temp files left by a crash are removed.
    pub fn open(root: impl Into<PathBuf>) -> Result<DiskBlobStore> {
        DiskBlobStore::open_with(root, DiskBlobConfig::default(), Arc::new(SystemClock))
    }

    /// Opens the store with an explicit configuration and clock (`written_at_ms` comes from
    /// `clock`).
    pub fn open_with(
        root: impl Into<PathBuf>,
        config: DiskBlobConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<DiskBlobStore> {
        let root = root.into();
        let tmp = root.join("tmp");
        for dir in [&root, &root.join("hot"), &root.join("cold"), &tmp] {
            fs::create_dir_all(dir).map_err(|e| io_error("creating store directories", e))?;
        }
        // A crash can leave temp files behind; none is referenced by anything.
        for entry in fs::read_dir(&tmp).map_err(|e| io_error("reading the temp directory", e))? {
            let entry = entry.map_err(|e| io_error("reading the temp directory", e))?;
            // Best effort: a file that cannot be removed is retried at the next open.
            let _ = fs::remove_file(entry.path());
        }
        Ok(DiskBlobStore {
            inner: Arc::new(Inner {
                root,
                tmp,
                config,
                tmp_seq: AtomicU64::new(0),
            }),
            clock,
            counters: Arc::new(AccessCounters::new()),
            faults: None,
            locks: (0..LOCK_STRIPES).map(|_| Mutex::new(())).collect(),
        })
    }

    /// Counts into `counters` instead of the store's own.
    pub fn with_counters(mut self, counters: Arc<AccessCounters>) -> DiskBlobStore {
        self.counters = counters;
        self
    }

    /// Checks the named [`points`] in `faults` between durable steps, for crash tests.
    pub fn with_faults(mut self, faults: Arc<FaultInjector>) -> DiskBlobStore {
        self.faults = Some(faults);
        self
    }

    /// The counters.
    pub fn counters(&self) -> &Arc<AccessCounters> {
        &self.counters
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// The file currently holding the bytes of `id`, quarantined copies included; `None` when
    /// absent. For operators and tests (corrupting a file, measuring size on disk).
    pub fn path_of(&self, org: OrgId, id: &ObjectId) -> Option<PathBuf> {
        self.inner
            .existing(org, id)
            .into_iter()
            .next()
            .map(|(_, p)| p)
    }

    async fn lock(&self, id: &ObjectId) -> tokio::sync::MutexGuard<'_, ()> {
        let d = id.digest();
        let stripe = (usize::from(d[0]) << 8 | usize::from(d[1])) % LOCK_STRIPES;
        self.locks[stripe].lock().await
    }

    async fn hit(&self, point: &str) -> Result<()> {
        if let Some(faults) = &self.faults {
            match faults.check(point).await? {
                Proceed::Normally => {}
                Proceed::ThenFail(e) => return Err(e),
            }
        }
        Ok(())
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| internal(format!("blocking storage task failed: {e}")))?
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    match File::open(dir).and_then(|f| f.sync_all()) {
        // Some filesystems cannot flush a directory; there is nothing stronger to do.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(())
        }
        other => other,
    }
}

impl Inner {
    fn path(&self, slot: Slot, org: OrgId, id: &ObjectId) -> PathBuf {
        self.root
            .join(class_dir(slot.class))
            .join(blob_key(org, id, slot.quarantined))
    }

    /// The copies of `id` that exist, in lookup-preference order.
    fn existing(&self, org: OrgId, id: &ObjectId) -> Vec<(Slot, PathBuf)> {
        SLOTS
            .iter()
            .map(|s| (*s, self.path(*s, org, id)))
            .filter(|(_, p)| fs::metadata(p).is_ok())
            .collect()
    }

    fn ensure_dir(&self, dir: &Path) -> io::Result<()> {
        if dir.is_dir() {
            return Ok(());
        }
        fs::create_dir_all(dir)?;
        // New directory entries must reach disk too, all the way up to the root.
        for ancestor in dir.ancestors() {
            sync_dir(ancestor)?;
            if ancestor == self.root {
                break;
            }
        }
        Ok(())
    }

    fn temp_path(&self) -> PathBuf {
        let n = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        self.tmp.join(format!("{}-{n}.tmp", std::process::id()))
    }

    /// Writes header and compressed bytes to a fresh synced temp file.
    fn write_temp(&self, bytes: &[u8], written_at_ms: u64) -> io::Result<PathBuf> {
        let tmp = self.temp_path();
        let result = (|| {
            let file = File::create_new(&tmp)?;
            let mut out = BufWriter::new(file);
            out.write_all(
                &Header {
                    written_at_ms,
                    raw_len: bytes.len() as u64,
                }
                .encode(),
            )?;
            let mut enc = zstd::stream::write::Encoder::new(out, self.config.zstd_level)?;
            enc.include_checksum(true)?;
            enc.write_all(bytes)?;
            let out = enc.finish()?;
            let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
            file.sync_all()
        })();
        match result {
            Ok(()) => Ok(tmp),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Renames a synced temp file to `dst` and makes the rename durable.
    fn install(&self, tmp: &Path, dst: &Path) -> io::Result<()> {
        let parent = dst
            .parent()
            .ok_or_else(|| io::Error::other("destination has no parent"))?;
        self.ensure_dir(parent)?;
        fs::rename(tmp, dst)?;
        sync_dir(parent)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        match path.parent() {
            Some(parent) => sync_dir(parent),
            None => Ok(()),
        }
    }

    /// Decodes a whole file and verifies it against `id`.
    fn decode_verified(id: &ObjectId, mut file: File) -> Result<(Header, Vec<u8>)> {
        let header = Header::read(id, &mut file)?;
        let mut decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|e| corrupt(id, &format!("zstd frame: {e}")))?;
        let mut out = Vec::with_capacity(header.raw_len.min(MAX_PREALLOC) as usize);
        (&mut decoder)
            .take(header.raw_len.saturating_add(1))
            .read_to_end(&mut out)
            .map_err(|e| corrupt(id, &format!("zstd decode: {e}")))?;
        if out.len() as u64 != header.raw_len {
            return Err(corrupt(id, "length differs from the header"));
        }
        verify_bytes(id, &out)?;
        Ok((header, out))
    }

    /// Opens a servable copy, preferring hot; `None` when there is none. Two rounds, so a
    /// copy moving between classes is never missed.
    fn open_servable(&self, org: OrgId, id: &ObjectId) -> Result<Option<(StorageClass, File)>> {
        for _ in 0..2 {
            for slot in SLOTS.iter().filter(|s| !s.quarantined) {
                match File::open(self.path(*slot, org, id)) {
                    Ok(f) => return Ok(Some((slot.class, f))),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_error("opening stored bytes", e)),
                }
            }
        }
        Ok(None)
    }

    fn read_servable(&self, org: OrgId, id: &ObjectId) -> Result<Option<(StorageClass, Vec<u8>)>> {
        match self.open_servable(org, id)? {
            Some((class, file)) => {
                let (_, bytes) = Inner::decode_verified(id, file)?;
                Ok(Some((class, bytes)))
            }
            None => Ok(None),
        }
    }

    fn read_range(
        &self,
        org: OrgId,
        id: &ObjectId,
        offset: u64,
        len: u64,
    ) -> Result<Option<(StorageClass, Vec<u8>)>> {
        let Some((class, mut file)) = self.open_servable(org, id)? else {
            return Ok(None);
        };
        if self.config.verify_range_reads {
            let (header, bytes) = Inner::decode_verified(id, file)?;
            let (start, end) = clamp_range(header.raw_len, offset, len);
            return Ok(Some((class, bytes[start as usize..end as usize].to_vec())));
        }
        let header = Header::read(id, &mut file)?;
        let (start, end) = clamp_range(header.raw_len, offset, len);
        let mut decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|e| corrupt(id, &format!("zstd frame: {e}")))?;
        let skipped = io::copy(&mut (&mut decoder).take(start), &mut io::sink())
            .map_err(|e| corrupt(id, &format!("zstd decode: {e}")))?;
        let mut out = Vec::with_capacity((end - start).min(MAX_PREALLOC) as usize);
        (&mut decoder)
            .take(end - start)
            .read_to_end(&mut out)
            .map_err(|e| corrupt(id, &format!("zstd decode: {e}")))?;
        if skipped != start || out.len() as u64 != end - start {
            return Err(corrupt(id, "file ends before its recorded length"));
        }
        Ok(Some((class, out)))
    }

    /// The written time of a file: its header, or the file's mtime when the header is
    /// unreadable (so GC can still collect corrupt files).
    fn written_at(&self, id: &ObjectId, path: &Path) -> io::Result<u64> {
        let mut file = File::open(path)?;
        if let Ok(h) = Header::read(id, &mut file) {
            return Ok(h.written_at_ms);
        }
        let modified = fs::metadata(path)?.modified()?;
        Ok(modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0))
    }

    fn info_of(&self, id: &ObjectId, slot: Slot, path: &Path) -> Result<Option<BlobInfo>> {
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error("opening stored bytes", e)),
        };
        let header = Header::read(id, &mut file)?;
        Ok(Some(BlobInfo {
            len: header.raw_len,
            class: slot.class,
            quarantined: slot.quarantined,
            written_at_ms: header.written_at_ms,
        }))
    }

    /// Copies a file to `dst` through a synced temp file, then verifies the copy.
    fn copy_verified(&self, id: &ObjectId, src: &Path, dst: &Path) -> Result<()> {
        let tmp = self.temp_path();
        let copied = (|| {
            let mut from = File::open(src)?;
            let mut to = File::create_new(&tmp)?;
            io::copy(&mut from, &mut to)?;
            to.sync_all()
        })();
        if let Err(e) = copied {
            let _ = fs::remove_file(&tmp);
            return Err(io_error("copying stored bytes", e));
        }
        if let Err(e) = self.install(&tmp, dst) {
            let _ = fs::remove_file(&tmp);
            return Err(io_error("installing the copy", e));
        }
        let verified = File::open(dst)
            .map_err(|e| io_error("reopening the copy", e))
            .and_then(|f| Inner::decode_verified(id, f));
        if let Err(e) = verified {
            // Keep the original; a bad copy must never become the only one.
            let _ = self.remove_file(dst);
            return Err(e);
        }
        Ok(())
    }

    /// Lists up to `limit` ids greater than `after` in one slot's tree, in id order.
    fn list_tree(
        &self,
        org: OrgId,
        slot: Slot,
        after: Option<&ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, Slot, PathBuf)>> {
        let base = self
            .root
            .join(class_dir(slot.class))
            .join(if slot.quarantined { "q" } else { "o" })
            .join(org.0.to_string());
        let mut out = Vec::new();
        for kind in listing_kinds() {
            if out.len() >= limit {
                break;
            }
            if after.is_some_and(|a| a.kind().as_byte() > kind.as_byte()) {
                continue;
            }
            let same_kind_after = after.filter(|a| a.kind() == kind);
            let after_hex = same_kind_after.map(|a| bee_core::hex::encode(a.digest()));
            let kind_dir = base.join(kind.text_tag());
            for fan in sorted_names(&kind_dir)? {
                if fan.len() != 2 || !bee_core::hex::is_lower_hex(&fan) {
                    continue;
                }
                if after_hex.as_ref().is_some_and(|h| fan.as_str() < &h[..2]) {
                    continue;
                }
                for name in sorted_names(&kind_dir.join(&fan))? {
                    let Some(id) = id_from_digest_hex(kind, &name) else {
                        continue;
                    };
                    if !name.starts_with(&fan) || after.is_some_and(|a| id <= *a) {
                        continue;
                    }
                    out.push((id, slot, kind_dir.join(&fan).join(&name)));
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }
}

/// File names of a directory, sorted; empty when the directory does not exist.
fn sorted_names(dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error("listing stored bytes", e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_error("listing stored bytes", e))?;
        if let Ok(name) = entry.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

#[async_trait]
impl BlobStore for DiskBlobStore {
    async fn put(
        &self,
        org: OrgId,
        id: &ObjectId,
        bytes: Vec<u8>,
        class: StorageClass,
    ) -> Result<()> {
        self.counters.blob_put();
        let _guard = self.lock(id).await;
        let now = self.clock.now_ms();
        let keep = Slot {
            class,
            quarantined: false,
        };
        let dst = self.inner.path(keep, org, id);
        let inner = self.inner.clone();
        let tmp = blocking(move || {
            inner
                .write_temp(&bytes, now)
                .map_err(|e| io_error("writing stored bytes", e))
        })
        .await?;
        if let Err(e) = self.hit(points::PUT_TEMP_WRITTEN).await {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        let inner = self.inner.clone();
        blocking(move || {
            inner
                .install(&tmp, &dst)
                .map_err(|e| io_error("installing stored bytes", e))
        })
        .await?;
        self.hit(points::PUT_INSTALLED).await?;
        // The new copy is durable; drop every other copy (the other class, quarantined ones).
        let inner = self.inner.clone();
        let id2 = *id;
        blocking(move || {
            for (slot, path) in inner.existing(org, &id2) {
                if slot != keep {
                    inner
                        .remove_file(&path)
                        .map_err(|e| io_error("removing a superseded copy", e))?;
                }
            }
            Ok(())
        })
        .await
    }

    async fn get(&self, org: OrgId, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        let inner = self.inner.clone();
        let id2 = *id;
        let found = blocking(move || inner.read_servable(org, &id2)).await?;
        Ok(found.map(|(class, bytes)| {
            self.counters.blob_get(
                id.kind(),
                bytes.len() as u64,
                false,
                class == StorageClass::Cold,
            );
            bytes
        }))
    }

    async fn get_range(
        &self,
        org: OrgId,
        id: &ObjectId,
        offset: u64,
        len: u64,
    ) -> Result<Option<Vec<u8>>> {
        let inner = self.inner.clone();
        let id2 = *id;
        let found = blocking(move || inner.read_range(org, &id2, offset, len)).await?;
        Ok(found.map(|(class, bytes)| {
            self.counters.blob_get(
                id.kind(),
                bytes.len() as u64,
                true,
                class == StorageClass::Cold,
            );
            bytes
        }))
    }

    async fn has(&self, org: OrgId, ids: &[ObjectId]) -> Result<Vec<bool>> {
        self.counters.blob_has(ids.len() as u64);
        let inner = self.inner.clone();
        let ids = ids.to_vec();
        blocking(move || {
            Ok(ids
                .iter()
                .map(|id| {
                    SLOTS
                        .iter()
                        .filter(|s| !s.quarantined)
                        .any(|s| fs::metadata(inner.path(*s, org, id)).is_ok())
                })
                .collect())
        })
        .await
    }

    async fn stat(&self, org: OrgId, id: &ObjectId) -> Result<Option<BlobInfo>> {
        let inner = self.inner.clone();
        let id2 = *id;
        blocking(move || {
            for (slot, path) in inner.existing(org, &id2) {
                if let Some(info) = inner.info_of(&id2, slot, &path)? {
                    return Ok(Some(info));
                }
            }
            Ok(None)
        })
        .await
    }

    async fn delete(
        &self,
        org: OrgId,
        id: &ObjectId,
        written_before_ms: Option<u64>,
    ) -> Result<bool> {
        let _guard = self.lock(id).await;
        let inner = self.inner.clone();
        let id2 = *id;
        let deleted = blocking(move || {
            let mut any = false;
            for (_, path) in inner.existing(org, &id2) {
                let eligible = match written_before_ms {
                    None => true,
                    Some(t) => match inner.written_at(&id2, &path) {
                        Ok(w) => w < t,
                        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                        Err(e) => return Err(io_error("reading the written time", e)),
                    },
                };
                if eligible {
                    inner
                        .remove_file(&path)
                        .map_err(|e| io_error("deleting stored bytes", e))?;
                    any = true;
                }
            }
            Ok(any)
        })
        .await?;
        if deleted {
            self.counters.blob_delete();
        }
        Ok(deleted)
    }

    async fn set_class(&self, org: OrgId, id: &ObjectId, class: StorageClass) -> Result<bool> {
        let _guard = self.lock(id).await;
        let inner = self.inner.clone();
        let id2 = *id;
        let plan = blocking(move || {
            let found = inner.existing(org, &id2);
            let Some((first, src)) = found.first().cloned() else {
                return Ok(None);
            };
            // Copies in this group are the same state as the preferred one (a crash can
            // leave one in each class); the others are leftovers of an interrupted move.
            let group: Vec<PathBuf> = found
                .iter()
                .filter(|(s, _)| s.quarantined == first.quarantined)
                .map(|(_, p)| p.clone())
                .collect();
            let target = Slot {
                class,
                quarantined: first.quarantined,
            };
            let dst = inner.path(target, org, &id2);
            if !group.contains(&dst) {
                inner.copy_verified(&id2, &src, &dst)?;
            }
            Ok(Some((group, dst)))
        })
        .await?;
        let Some((group, dst)) = plan else {
            return Ok(false);
        };
        self.hit(points::SET_CLASS_COPIED).await?;
        let inner = self.inner.clone();
        blocking(move || {
            for path in group.iter().filter(|p| **p != dst) {
                inner
                    .remove_file(path)
                    .map_err(|e| io_error("removing the old copy", e))?;
            }
            Ok(())
        })
        .await?;
        Ok(true)
    }

    async fn quarantine(&self, org: OrgId, id: &ObjectId) -> Result<bool> {
        let _guard = self.lock(id).await;
        let inner = self.inner.clone();
        let id2 = *id;
        blocking(move || {
            let mut found = false;
            for (slot, path) in inner.existing(org, &id2) {
                found = true;
                if slot.quarantined {
                    continue;
                }
                let dst = inner.path(
                    Slot {
                        class: slot.class,
                        quarantined: true,
                    },
                    org,
                    &id2,
                );
                let parent = dst.parent().unwrap_or(&inner.root);
                inner
                    .ensure_dir(parent)
                    .and_then(|()| fs::rename(&path, &dst))
                    .and_then(|()| sync_dir(parent))
                    .and_then(|()| path.parent().map_or(Ok(()), sync_dir))
                    .map_err(|e| io_error("quarantining stored bytes", e))?;
            }
            Ok(found)
        })
        .await
    }

    async fn release(&self, org: OrgId, id: &ObjectId) -> Result<bool> {
        let _guard = self.lock(id).await;
        let inner = self.inner.clone();
        let id2 = *id;
        blocking(move || {
            let mut released = false;
            for (slot, path) in inner.existing(org, &id2) {
                if !slot.quarantined {
                    continue;
                }
                let dst = inner.path(
                    Slot {
                        class: slot.class,
                        quarantined: false,
                    },
                    org,
                    &id2,
                );
                let parent = dst.parent().unwrap_or(&inner.root);
                inner
                    .ensure_dir(parent)
                    .and_then(|()| fs::rename(&path, &dst))
                    .and_then(|()| sync_dir(parent))
                    .and_then(|()| path.parent().map_or(Ok(()), sync_dir))
                    .map_err(|e| io_error("releasing stored bytes", e))?;
                released = true;
            }
            Ok(released)
        })
        .await
    }

    async fn list(
        &self,
        org: OrgId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, BlobInfo)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let inner = self.inner.clone();
        blocking(move || {
            let mut all = Vec::new();
            for slot in SLOTS {
                all.extend(inner.list_tree(org, slot, after.as_ref(), limit)?);
            }
            // Slot order is preference order, and the sort is stable: the first copy of an
            // id is the one `stat` would report.
            all.sort_by_key(|(id, _, _)| *id);
            all.dedup_by_key(|(id, _, _)| *id);
            let mut out = Vec::new();
            for (id, slot, path) in all.into_iter().take(limit) {
                // A copy deleted since the directory walk is simply gone.
                if let Some(info) = inner.info_of(&id, slot, &path)? {
                    out.push((id, info));
                }
            }
            Ok(out)
        })
        .await
    }

    fn access_counters(&self) -> Option<Arc<AccessCounters>> {
        Some(self.counters.clone())
    }
}
