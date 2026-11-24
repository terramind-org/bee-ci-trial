//! The working-tree scan behind status: a stat-cache walk that re-hashes only files whose stat
//! tuple changed or that are "racily clean", honoring the sparse scope and `.gitignore`.
//!
//! The stat cache (`.bee/statcache`, sealed and versioned) maps each path to the stat tuple
//! `(size, mtime_ns, ctime_ns, inode, mode)` it had when hashed, and the entry it hashed to. A
//! cached entry is trusted only if its mtime is strictly older than the cache's clock, which
//! is the mtime of `.bee/clock` touched when the scan that wrote the cache began; the file
//! system stamps both, so a file edited in the same clock tick as the scan is re-hashed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use bee_core::Mode;

use crate::error::{Result, WcError};
use crate::fsutil::{self, Reader, name_bytes, path_text, put_bytes, seal, unseal};
use crate::ignore::Ignore;
use crate::manifest::{Entry, Manifest};
use crate::sparse::Sparse;

const MAGIC: &[u8; 4] = b"SWSC";
const VERSION: u32 = 1;

/// The stat tuple of SPEC §10.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    /// File size in bytes.
    pub size: u64,
    /// Modification time, nanoseconds since the epoch.
    pub mtime_ns: i64,
    /// Status-change time, nanoseconds since the epoch (0 where unavailable).
    pub ctime_ns: i64,
    /// Inode number (0 where unavailable).
    pub ino: u64,
    /// Permission and type bits (0 where unavailable).
    pub mode: u32,
}

impl Stat {
    /// The stat tuple of `meta`.
    pub fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Self {
                size: meta.size(),
                mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
                ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
                ino: meta.ino(),
                mode: meta.mode(),
            }
        }
        #[cfg(not(unix))]
        {
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as i64);
            Self {
                size: meta.len(),
                mtime_ns,
                ctime_ns: 0,
                ino: 0,
                mode: 0,
            }
        }
    }
}

/// Cached hashes by path, and the clock they were taken against.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatCache {
    /// Mtime (ns) of `.bee/clock` when the scan that wrote this cache began.
    pub clock_ns: i64,
    /// Path to the stat it had and the entry it hashed to.
    pub entries: BTreeMap<Vec<u8>, (Stat, Entry)>,
}

impl StatCache {
    /// Encodes the cache as a sealed file.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.entries.len() * 96);
        out.extend_from_slice(&self.clock_ns.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for (path, (st, e)) in &self.entries {
            put_bytes(&mut out, path);
            out.extend_from_slice(&st.size.to_le_bytes());
            out.extend_from_slice(&st.mtime_ns.to_le_bytes());
            out.extend_from_slice(&st.ctime_ns.to_le_bytes());
            out.extend_from_slice(&st.ino.to_le_bytes());
            out.extend_from_slice(&st.mode.to_le_bytes());
            out.push(e.mode.as_u64() as u8);
            out.extend_from_slice(e.id.as_bytes());
            out.extend_from_slice(&e.size.to_le_bytes());
        }
        seal(MAGIC, VERSION, &out)
    }

    /// Decodes a sealed cache file.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (version, payload) = unseal(MAGIC, "stat cache", bytes)?;
        if version != VERSION {
            return Err(WcError::integrity(
                "unknown_version",
                format!("stat cache version {version} is not supported"),
            ));
        }
        let mut r = Reader::new(payload, "stat cache");
        let clock_ns = r.i64()?;
        let n = r.u64()?;
        let mut entries = BTreeMap::new();
        for _ in 0..n {
            let path = r.bytes()?;
            let st = Stat {
                size: r.u64()?,
                mtime_ns: r.i64()?,
                ctime_ns: r.i64()?,
                ino: r.u64()?,
                mode: r.u32()?,
            };
            let mode = Mode::from_u64(u64::from(r.u8()?))
                .map_err(|e| WcError::integrity("corrupt_file", e.to_string()))?;
            let id = bee_core::ObjectId::from_bytes(r.raw(34)?)
                .map_err(|e| WcError::integrity("corrupt_file", e.to_string()))?;
            let size = r.u64()?;
            entries.insert(path, (st, Entry { mode, id, size }));
        }
        r.finish()?;
        Ok(Self { clock_ns, entries })
    }
}

/// What a scan found.
#[derive(Clone, Debug, Default)]
pub struct Scan {
    /// Every file in scope that is tracked or not ignored, with its current entry.
    pub actual: Manifest,
    /// The refreshed cache.
    pub cache: StatCache,
    /// True when files were re-hashed, so writing `cache` saves work next time.
    pub cache_changed: bool,
    /// Names that are not valid repo paths and were skipped.
    pub skipped: Vec<String>,
    /// Number of files re-hashed.
    pub hashed: usize,
}

/// Reads a working-tree file (or symlink) as an entry plus its bytes.
pub fn read_entry(path: &Path, meta: &fs::Metadata) -> Result<(Entry, Vec<u8>)> {
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path)
            .map_err(|e| WcError::io(format!("readlink {}", path.display()), &e))?;
        let bytes = name_bytes(target.as_os_str()).ok_or_else(|| {
            WcError::usage(
                "unrepresentable_path",
                format!("symlink target of {} is not UTF-8", path.display()),
            )
        })?;
        return Ok((Entry::for_bytes(Mode::Symlink, &bytes), bytes));
    }
    let bytes = fs::read(path).map_err(|e| WcError::io(format!("read {}", path.display()), &e))?;
    Ok((Entry::for_bytes(file_mode(meta), &bytes), bytes))
}

/// Like [`read_entry`], with the line-ending conversion `attrs` give repo path `rel` applied
/// to a regular file's bytes before they are hashed (`crate::eol`).
pub fn read_entry_with(
    path: &Path,
    meta: &fs::Metadata,
    rel: &[u8],
    attrs: &crate::eol::Attributes,
) -> Result<(Entry, Vec<u8>)> {
    if attrs.is_empty() || meta.file_type().is_symlink() {
        return read_entry(path, meta);
    }
    let bytes = fs::read(path).map_err(|e| WcError::io(format!("read {}", path.display()), &e))?;
    let conv = attrs.conversion(rel, &bytes);
    let bytes = crate::eol::to_repo(conv, bytes);
    Ok((Entry::for_bytes(file_mode(meta), &bytes), bytes))
}

/// File or executable, from permission bits.
pub fn file_mode(meta: &fs::Metadata) -> Mode {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if meta.permissions().mode() & 0o111 != 0 {
            return Mode::Executable;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
    }
    Mode::File
}

/// Walks the working tree under `root`.
///
/// `base` is the checked-out manifest (its paths are tracked and never ignored); `hidden` are
/// tracked paths deliberately not on disk (case collisions, gitlinks), reported as unchanged.
pub fn scan(
    root: &Path,
    clock_ns: i64,
    sparse: &Sparse,
    base: &Manifest,
    hidden: &BTreeSet<Vec<u8>>,
    cache: &StatCache,
) -> Result<Scan> {
    let mut out = Scan {
        cache: StatCache {
            clock_ns,
            entries: BTreeMap::new(),
        },
        ..Scan::default()
    };
    let mut ignore = Ignore::new();
    let mut attrs = crate::eol::Attributes::new();
    walk(
        root,
        &[],
        sparse,
        base,
        cache,
        &mut ignore,
        &mut attrs,
        &mut out,
    )?;
    let placeholder = crate::checkout::placeholder_id();
    for (path, entry) in base {
        let is_hidden = hidden.contains(path);
        if (is_hidden || entry.mode == Mode::Gitlink) && !out.actual.contains_key(path) {
            out.actual.insert(path.clone(), *entry);
        }
        // A redacted file is on disk as the placeholder (SPEC §13): unchanged.
        if is_hidden && out.actual.get(path).is_some_and(|e| e.id == placeholder) {
            out.actual.insert(path.clone(), *entry);
        }
    }
    if out.cache.entries.len() != cache.entries.len() {
        out.cache_changed = true;
    }
    Ok(out)
}

fn has_tracked_under(base: &Manifest, dir: &[u8]) -> bool {
    let mut prefix = dir.to_vec();
    prefix.push(b'/');
    base.range(prefix.clone()..)
        .next()
        .is_some_and(|(p, _)| p.starts_with(&prefix))
}

#[allow(clippy::too_many_arguments)]
fn walk(
    root: &Path,
    dir: &[u8],
    sparse: &Sparse,
    base: &Manifest,
    cache: &StatCache,
    ignore: &mut Ignore,
    attrs: &mut crate::eol::Attributes,
    out: &mut Scan,
) -> Result<()> {
    let fs_dir = fsutil::fs_path(root, dir)?;
    let gitignore = fs_dir.join(".gitignore");
    if let Some(text) = fsutil::read_optional(&gitignore)? {
        ignore.add_file(dir, &text);
    }
    if let Some(text) = fsutil::read_optional(&fs_dir.join(".gitattributes"))? {
        attrs.add_file(dir, &text);
    }
    let read = match fs::read_dir(&fs_dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(WcError::io(format!("read {}", fs_dir.display()), &e)),
    };
    let mut names: Vec<(Vec<u8>, std::path::PathBuf)> = Vec::new();
    for item in read {
        let item = item.map_err(|e| WcError::io(format!("read {}", fs_dir.display()), &e))?;
        let Some(name) = name_bytes(&item.file_name()) else {
            out.skipped.push(item.path().to_string_lossy().into_owned());
            continue;
        };
        names.push((name, item.path()));
    }
    names.sort();
    for (name, fs_path) in names {
        if dir.is_empty() && name == b".bee" {
            continue;
        }
        if name.ends_with(b".bee-tmp") && name.first() == Some(&b'.') {
            continue;
        }
        let path = if dir.is_empty() {
            name.clone()
        } else {
            let mut p = dir.to_vec();
            p.push(b'/');
            p.extend_from_slice(&name);
            p
        };
        if bee_core::validate_name(&name).is_err() || bee_core::RepoPath::new(path.clone()).is_err()
        {
            out.skipped.push(path_text(&path));
            continue;
        }
        let meta = match fs::symlink_metadata(&fs_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(WcError::io(format!("stat {}", fs_path.display()), &e)),
        };
        let ft = meta.file_type();
        if ft.is_dir() {
            if !sparse.may_contain(&path) {
                continue;
            }
            if ignore.is_ignored(&path, true) && !has_tracked_under(base, &path) {
                continue;
            }
            walk(root, &path, sparse, base, cache, ignore, attrs, out)?;
            continue;
        }
        if !(ft.is_file() || ft.is_symlink()) || !sparse.contains(&path) {
            continue;
        }
        let tracked = base.contains_key(&path);
        if !tracked && ignore.is_ignored(&path, false) {
            continue;
        }
        let st = Stat::of(&meta);
        let entry = match cache.entries.get(&path) {
            Some((cached, entry)) if *cached == st && st.mtime_ns < cache.clock_ns => *entry,
            _ => {
                let (entry, _) = read_entry_with(&fs_path, &meta, &path, attrs)?;
                out.hashed += 1;
                out.cache_changed = true;
                entry
            }
        };
        out.cache.entries.insert(path.clone(), (st, entry));
        out.actual.insert(path, entry);
    }
    Ok(())
}

/// Touches `.bee/clock` and returns its mtime in nanoseconds: the file system's notion of
/// "now", at the same granularity as the files it stamps.
pub fn touch_clock(bee_dir: &Path) -> Result<i64> {
    let path = bee_dir.join("clock");
    fs::write(&path, b"").map_err(|e| WcError::io(format!("write {}", path.display()), &e))?;
    let meta =
        fs::metadata(&path).map_err(|e| WcError::io(format!("stat {}", path.display()), &e))?;
    Ok(Stat::of(&meta).mtime_ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_roundtrip() {
        let mut c = StatCache {
            clock_ns: 42,
            entries: BTreeMap::new(),
        };
        c.entries.insert(
            b"a/b".to_vec(),
            (
                Stat {
                    size: 3,
                    mtime_ns: 7,
                    ctime_ns: 8,
                    ino: 9,
                    mode: 0o100644,
                },
                Entry::for_bytes(Mode::File, b"abc"),
            ),
        );
        assert_eq!(StatCache::decode(&c.encode()).expect("decode"), c);
        let mut bad = c.encode();
        bad[12] ^= 0xff;
        assert!(StatCache::decode(&bad).is_err());
    }
}
