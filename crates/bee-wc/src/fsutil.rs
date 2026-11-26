//! Durable file primitives: atomic replace (temp file, fsync, rename, directory fsync), sealed
//! files with a trailing BLAKE3 checksum, and the bytes-to-filesystem path mapping.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::{Result, WcError};

thread_local! {
    static FULL_SYNCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Full syncs (a flush through the device cache: `sync_all`, or `F_FULLFSYNC` on Apple
/// platforms, where each costs milliseconds) this thread has issued so far. A diagnostic: tests
/// bound the durable-write cost of an operation with it.
pub fn full_syncs_on_this_thread() -> u64 {
    FULL_SYNCS.with(std::cell::Cell::get)
}

/// `sync_all` on `f` (opened from `path`), counted in [`full_syncs_on_this_thread`].
pub(crate) fn full_sync(f: &File, path: &Path) -> Result<()> {
    FULL_SYNCS.with(|c| c.set(c.get() + 1));
    f.sync_all()
        .map_err(|e| WcError::io(format!("fsync {}", path.display()), &e))
}

/// Flushes a directory entry change (create, rename, unlink) to disk. A no-op where the
/// platform cannot open directories.
pub fn fsync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let f = File::open(dir).map_err(|e| WcError::io(format!("open {}", dir.display()), &e))?;
        full_sync(&f, dir)?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Makes freshly written `files` and the `dirs` whose entries changed durable, all together,
/// before the caller records that they were written. Equivalent to a full sync of each, but
/// one device-cache flush instead of one per file: every path is pushed to the device with a
/// plain `fsync` (spread over a few threads), then on Apple platforms a single `F_FULLFSYNC`
/// flushes the device cache once for all of them (elsewhere `fsync` already reaches stable
/// storage). Directories are skipped where the platform cannot open them.
pub fn sync_all_paths(files: &[PathBuf], dirs: &[PathBuf]) -> Result<()> {
    let mut all: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    #[cfg(unix)]
    all.extend(dirs.iter().map(PathBuf::as_path));
    #[cfg(not(unix))]
    let _ = dirs;
    let Some(last) = all.last().copied() else {
        return Ok(());
    };
    let threads = all.len().clamp(1, SYNC_THREADS);
    let per = all.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = all
            .chunks(per)
            .map(|part| s.spawn(move || part.iter().try_for_each(|p| push_to_device(p))))
            .collect();
        handles.into_iter().try_for_each(|h| {
            h.join()
                .map_err(|_| WcError::internal("sync_failed", "a sync thread panicked"))?
        })
    })?;
    flush_device(last)
}

/// Threads [`sync_all_paths`] spreads its `fsync` calls over.
const SYNC_THREADS: usize = 8;

/// Hands a file's (or directory's) dirty data to the device.
fn push_to_device(path: &Path) -> Result<()> {
    let f = File::open(path).map_err(|e| WcError::io(format!("open {}", path.display()), &e))?;
    #[cfg(unix)]
    {
        rustix::fs::fsync(&f).map_err(|e| {
            WcError::io(
                format!("fsync {}", path.display()),
                &std::io::Error::from(e),
            )
        })
    }
    #[cfg(not(unix))]
    {
        f.sync_all()
            .map_err(|e| WcError::io(format!("fsync {}", path.display()), &e))
    }
}

/// Flushes the device's write cache (Apple platforms, where `fsync` stops at the device).
#[cfg(target_vendor = "apple")]
fn flush_device(any: &Path) -> Result<()> {
    let f = File::open(any).map_err(|e| WcError::io(format!("open {}", any.display()), &e))?;
    FULL_SYNCS.with(|c| c.set(c.get() + 1));
    rustix::fs::fcntl_fullfsync(&f).map_err(|e| {
        WcError::io(
            format!("F_FULLFSYNC {}", any.display()),
            &std::io::Error::from(e),
        )
    })
}

/// Nothing to do: `fsync` already reached stable storage.
#[cfg(not(target_vendor = "apple"))]
fn flush_device(_any: &Path) -> Result<()> {
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.bee-tmp"))
}

/// Replaces `path` with `bytes` atomically and durably: write a temp file in the same
/// directory, fsync it, rename it over `path`, fsync the directory.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_path(path);
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| WcError::io(format!("create {}", tmp.display()), &e))?;
        f.write_all(bytes)
            .map_err(|e| WcError::io(format!("write {}", tmp.display()), &e))?;
        full_sync(&f, &tmp)?;
    }
    fs::rename(&tmp, path).map_err(|e| WcError::io(format!("rename {}", path.display()), &e))?;
    if let Some(dir) = path.parent() {
        fsync_dir(dir)?;
    }
    Ok(())
}

/// Reads a file, `None` if it does not exist.
pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(WcError::io(format!("read {}", path.display()), &e)),
    }
}

/// Removes a file if it exists.
pub fn remove_optional(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(WcError::io(format!("remove {}", path.display()), &e)),
    }
}

/// Wraps `payload` as `magic ‖ version (u32 LE) ‖ payload ‖ BLAKE3(everything before)`.
pub fn seal(magic: &[u8; 4], version: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 40);
    out.extend_from_slice(magic);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(payload);
    let sum = bee_object::digest(&out);
    out.extend_from_slice(&sum);
    out
}

/// Checks a sealed file and returns its version and payload.
pub fn unseal<'a>(magic: &[u8; 4], what: &str, bytes: &'a [u8]) -> Result<(u32, &'a [u8])> {
    if bytes.len() < 40 || &bytes[..4] != magic {
        return Err(WcError::integrity(
            "corrupt_file",
            format!("{what}: not a bee {what} file"),
        ));
    }
    let (body, sum) = bytes.split_at(bytes.len() - 32);
    if bee_object::digest(body) != sum {
        return Err(WcError::integrity(
            "corrupt_file",
            format!("{what}: checksum mismatch (torn or edited file)"),
        ));
    }
    let version = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    Ok((version, &body[8..]))
}

/// A little-endian reader over a sealed payload.
pub struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    what: &'static str,
}

impl<'a> Reader<'a> {
    /// A reader over `bytes`, naming `what` in errors.
    pub fn new(bytes: &'a [u8], what: &'static str) -> Self {
        Self {
            bytes,
            pos: 0,
            what,
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.bytes.len() - self.pos < n {
            return Err(WcError::integrity(
                "corrupt_file",
                format!("{}: truncated", self.what),
            ));
        }
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// One byte.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// A u32.
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A u64.
    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    /// An i64.
    pub fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    /// A length-prefixed byte string.
    pub fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    /// Exactly `n` raw bytes.
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    /// Fails unless every byte was read.
    pub fn finish(&self) -> Result<()> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(WcError::integrity(
                "corrupt_file",
                format!("{}: trailing bytes", self.what),
            ))
        }
    }
}

/// Appends a length-prefixed byte string.
pub fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// The filesystem path of repo path `rel` under `root`.
pub fn fs_path(root: &Path, rel: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(root.join(std::ffi::OsStr::from_bytes(rel)))
    }
    #[cfg(not(unix))]
    {
        let text = std::str::from_utf8(rel).map_err(|_| {
            WcError::usage(
                "unrepresentable_path",
                format!(
                    "{} is not UTF-8 and cannot be written here",
                    String::from_utf8_lossy(rel)
                ),
            )
        })?;
        Ok(root.join(text.replace('/', "\\")))
    }
}

/// The bytes of a file name.
pub fn name_bytes(name: &std::ffi::OsStr) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Some(name.as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        name.to_str().map(|s| s.as_bytes().to_vec())
    }
}

/// Text form of a repo path for output (lossy for non-UTF-8 bytes).
pub fn path_text(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_files_detect_damage() {
        let sealed = seal(b"TEST", 3, b"payload");
        let (v, p) = unseal(b"TEST", "test", &sealed).expect("unseal");
        assert_eq!((v, p), (3, &b"payload"[..]));
        let mut torn = sealed.clone();
        torn.truncate(torn.len() - 1);
        assert!(unseal(b"TEST", "test", &torn).is_err());
        let mut flipped = sealed;
        flipped[9] ^= 1;
        assert_eq!(
            unseal(b"TEST", "test", &flipped).expect_err("flip").reason,
            "INTEGRITY"
        );
    }

    #[test]
    fn atomic_write_replaces() {
        let dir = tempfile::tempdir().expect("tmp");
        let p = dir.path().join("f");
        atomic_write(&p, b"one").expect("w1");
        atomic_write(&p, b"two").expect("w2");
        assert_eq!(read_optional(&p).expect("r"), Some(b"two".to_vec()));
        assert!(!dir.path().join(".f.bee-tmp").exists());
        remove_optional(&p).expect("rm");
        remove_optional(&p).expect("rm twice");
        assert_eq!(read_optional(&p).expect("r"), None);
    }
}
