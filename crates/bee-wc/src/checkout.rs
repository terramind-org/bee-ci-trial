//! Making the working tree hold a set of targets: deletes first, then writes through a temp
//! file and rename, never through a symlinked parent; case collisions on case-insensitive file
//! systems are reported and not written (SPEC §10.3). Every step is idempotent, so a crashed
//! apply is finished by running it again. Files are not synced one by one: an apply makes all
//! it wrote durable together before returning (one device-cache flush, not one per file), so
//! the journal still only records an apply after its files are on stable storage.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::Path;

use bee_core::{Mode, ObjectId};

use crate::eol::{Attributes, to_worktree};
use crate::error::{Result, WcError};
use crate::fsutil::{self, fs_path, path_text};
use crate::journal::{Faults, Layout};
use crate::manifest::{Entry, Manifest};
use crate::scan::{Stat, read_entry_with};
use crate::state::{HiddenPath, PathBytes};

/// Fetches blob bytes by id: `Ok(bytes)` or `Err(reason)` per id.
pub type Fetch<'a> =
    dyn FnMut(&[ObjectId]) -> Result<BTreeMap<ObjectId, std::result::Result<Vec<u8>, String>>> + 'a;

/// What an apply did.
#[derive(Clone, Debug, Default)]
pub struct Applied {
    /// Files written.
    pub written: usize,
    /// Files removed.
    pub removed: usize,
    /// Tracked paths not written, with the reason.
    pub hidden: Vec<HiddenPath>,
    /// Stat and entry of every file written or found already correct.
    pub stats: Vec<(Vec<u8>, Stat, Entry)>,
}

/// True if the file system under `bee_dir` folds ASCII case.
pub fn case_insensitive(bee_dir: &Path) -> bool {
    let probe = bee_dir.join("CaseProbe");
    if fs::write(&probe, b"").is_err() {
        return false;
    }
    let folded = bee_dir.join("caseprobe").exists();
    let _ = fs::remove_file(&probe);
    folded
}

/// Paths of `tree` that collide with another path (or with a directory of another path)
/// under ASCII case folding.
pub fn case_collisions(tree: &Manifest) -> BTreeSet<Vec<u8>> {
    let mut by_lower: BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>> = BTreeMap::new();
    for path in tree.keys() {
        let mut end = 0;
        loop {
            let next = path[end..].iter().position(|&b| b == b'/').map(|i| end + i);
            let prefix = &path[..next.unwrap_or(path.len())];
            by_lower
                .entry(prefix.to_ascii_lowercase())
                .or_default()
                .insert(prefix.to_vec());
            match next {
                Some(i) => end = i + 1,
                None => break,
            }
        }
    }
    let clashing: BTreeSet<Vec<u8>> = by_lower
        .into_values()
        .filter(|s| s.len() > 1)
        .flatten()
        .collect();
    if clashing.is_empty() {
        return BTreeSet::new();
    }
    tree.keys()
        .filter(|p| {
            clashing.iter().any(|c| {
                p.as_slice() == c.as_slice()
                    || (p.len() > c.len() && p.starts_with(c) && p[c.len()] == b'/')
            })
        })
        .cloned()
        .collect()
}

/// The exact bytes a working copy writes in place of a redacted file (SPEC §13). Status
/// treats a redacted path holding exactly these bytes as unchanged.
pub const REDACTED_PLACEHOLDER: &[u8] =
    b"[bee] This file was redacted. Its content is no longer available.\n";

/// The blob id of [`REDACTED_PLACEHOLDER`].
pub fn placeholder_id() -> ObjectId {
    bee_object::blob_id(REDACTED_PLACEHOLDER)
}

/// Writes the placeholder for a redacted entry at `rel` (a regular file whatever the entry's
/// mode), returning its stat tuple for the cache.
pub fn write_placeholder(root: &Path, rel: &[u8]) -> Result<Option<Stat>> {
    let path = fs_path(root, rel)?;
    let file = Entry {
        mode: Mode::File,
        id: placeholder_id(),
        size: REDACTED_PLACEHOLDER.len() as u64,
    };
    write_file(root, rel, &file, REDACTED_PLACEHOLDER)?;
    Ok(fs::symlink_metadata(&path).ok().map(|m| Stat::of(&m)))
}

/// What is on disk at `rel`: `Some(true)` the placeholder, `Some(false)` exactly `entry`,
/// `None` anything else (changed, missing or not a file).
pub fn placeholder_or_entry(
    root: &Path,
    rel: &[u8],
    entry: &Entry,
    attrs: &Attributes,
) -> Result<Option<bool>> {
    let path = fs_path(root, rel)?;
    Ok(match on_disk_with(&path, rel, attrs)? {
        Some(e) if e.id == placeholder_id() => Some(true),
        Some(e) if e == *entry => Some(false),
        _ => None,
    })
}

/// Writes `bytes` (the stored content of `entry`) at `rel` with its line-ending conversion,
/// returning its stat tuple.
pub fn write_entry(
    root: &Path,
    rel: &[u8],
    entry: &Entry,
    bytes: &[u8],
    attrs: &Attributes,
) -> Result<Option<Stat>> {
    let path = fs_path(root, rel)?;
    let disk = worktree_bytes(rel, entry, bytes, attrs);
    write_file(root, rel, entry, &disk)?;
    Ok(fs::symlink_metadata(&path).ok().map(|m| Stat::of(&m)))
}

fn hidden(path: &[u8], reason: &str) -> HiddenPath {
    HiddenPath {
        path: PathBytes(path.to_vec()),
        reason: reason.to_owned(),
    }
}

/// True if any ancestor directory of `rel` under `root` is a symlink.
fn symlinked_parent(root: &Path, rel: &[u8]) -> Result<bool> {
    let mut end = 0;
    while let Some(i) = rel[end..].iter().position(|&b| b == b'/') {
        let dir = fs_path(root, &rel[..end + i])?;
        if let Ok(meta) = fs::symlink_metadata(&dir)
            && meta.file_type().is_symlink()
        {
            return Ok(true);
        }
        end += i + 1;
    }
    Ok(false)
}

/// The working-tree bytes of stored `bytes` for `entry` at `rel` (`crate::eol`).
fn worktree_bytes<'a>(
    rel: &[u8],
    entry: &Entry,
    bytes: &'a [u8],
    attrs: &Attributes,
) -> std::borrow::Cow<'a, [u8]> {
    if entry.mode == Mode::Symlink || attrs.is_empty() {
        return std::borrow::Cow::Borrowed(bytes);
    }
    to_worktree(attrs.conversion(rel, bytes), bytes)
}

/// The entry on disk at `path` as the repository would store it (line endings converted).
fn on_disk_with(path: &Path, rel: &[u8], attrs: &Attributes) -> Result<Option<Entry>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() || meta.file_type().is_symlink() => {
            Ok(Some(read_entry_with(path, &meta, rel, attrs)?.0))
        }
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(WcError::io(format!("stat {}", path.display()), &e)),
    }
}

fn remove_entry(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(false),
        Ok(_) => {
            fs::remove_file(path)
                .map_err(|e| WcError::io(format!("remove {}", path.display()), &e))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(WcError::io(format!("stat {}", path.display()), &e)),
    }
}

fn prune_empty_parents(root: &Path, rel: &[u8]) -> Result<()> {
    let mut cur = rel.to_vec();
    while let Some(i) = cur.iter().rposition(|&b| b == b'/') {
        cur.truncate(i);
        let dir = fs_path(root, &cur)?;
        match fs::remove_dir(&dir) {
            Ok(()) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// Writes `bytes` (the content of `entry`) at `rel` under `root`, never following a symlink in
/// any component below `root` (SPEC §10.3). On unix every step is relative to a directory
/// descriptor opened with `O_NOFOLLOW`: parents are walked with `openat`, created with
/// `mkdirat`, the file is written to a fresh temporary with `openat(O_CREAT | O_EXCL |
/// O_NOFOLLOW)` and moved into place with `renameat`, so a symlink swapped in by another
/// process between a check and the write cannot redirect it.
fn write_file(root: &Path, rel: &[u8], entry: &Entry, bytes: &[u8]) -> Result<()> {
    write_file_with(&mut Dirs::default(), root, rel, entry, bytes).map(|_| ())
}

/// Directories a run of writes keeps open: the root and the parent of the last file
/// written, so consecutive files in one directory open it once. Every directory was opened
/// without following symlinks, so a cached one is still the real directory.
#[derive(Default)]
struct Dirs {
    #[cfg(unix)]
    root: Option<rustix::fd::OwnedFd>,
    #[cfg(unix)]
    parent: Option<(Vec<u8>, rustix::fd::OwnedFd)>,
}

/// Writes `bytes` at `rel`; returns the new file's metadata when it was read from the
/// written file itself (`None`: the caller stats the path).
fn write_file_with(
    dirs: &mut Dirs,
    root: &Path,
    rel: &[u8],
    entry: &Entry,
    bytes: &[u8],
) -> Result<Option<fs::Metadata>> {
    #[cfg(unix)]
    {
        write_file_at(dirs, root, rel, entry, bytes)
    }
    #[cfg(not(unix))]
    {
        let _ = dirs;
        write_file_by_path(&fs_path(root, rel)?, entry, bytes).map(|_| None)
    }
}

#[cfg(unix)]
fn write_file_at(
    dirs: &mut Dirs,
    root: &Path,
    rel: &[u8],
    entry: &Entry,
    bytes: &[u8],
) -> Result<Option<fs::Metadata>> {
    use rustix::fs::{AtFlags, Mode as Perm, OFlags};
    use rustix::io::Errno;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    let shown = || root.join(OsStr::from_bytes(rel)).display().to_string();
    let io = |what: &str, e: Errno| {
        WcError::io(
            format!("{what} {}", shown()),
            &std::io::Error::from_raw_os_error(e.raw_os_error()),
        )
    };
    let dir_flags = OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let (parent_rel, name) = match rel.iter().rposition(|b| *b == b'/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => (&rel[..0], rel),
    };
    if name.is_empty() {
        return Err(WcError::internal("bad_path", "empty path".to_string()));
    }
    let cached = matches!(&dirs.parent, Some((p, _)) if p.as_slice() == parent_rel);
    if !cached {
        dirs.parent = None;
        if dirs.root.is_none() {
            dirs.root = Some(
                rustix::fs::open(
                    root,
                    OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
                    Perm::empty(),
                )
                .map_err(|e| io("open root of", e))?,
            );
        }
        let mut dir: Option<rustix::fd::OwnedFd> = None;
        if !parent_rel.is_empty() {
            for part in parent_rel.split(|b| *b == b'/') {
                let at = match &dir {
                    Some(d) => rustix::fd::AsFd::as_fd(d),
                    None => rustix::fd::AsFd::as_fd(dirs.root.as_ref().expect("opened above")),
                };
                let c = OsStr::from_bytes(part);
                let next = match open_parent_component(at, c, dir_flags) {
                    Ok(fd) => fd,
                    Err(Errno::NOENT) => {
                        match rustix::fs::mkdirat(at, c, Perm::from_raw_mode(0o755)) {
                            Ok(()) | Err(Errno::EXIST) => {}
                            Err(e) => return Err(io("mkdir for", e)),
                        }
                        open_parent_component(at, c, dir_flags)
                            .map_err(|e| io("open parent of", e))?
                    }
                    // A symlink (ELOOP) or a file (ENOTDIR) where a directory goes: never
                    // followed.
                    Err(e) => return Err(io("refusing a non-directory parent of", e)),
                };
                dir = Some(next);
            }
        }
        if let Some(d) = dir {
            dirs.parent = Some((parent_rel.to_vec(), d));
        }
    }
    let dir = match &dirs.parent {
        Some((_, d)) => rustix::fd::AsFd::as_fd(d),
        None => rustix::fd::AsFd::as_fd(dirs.root.as_ref().expect("opened above")),
    };
    let name = OsStr::from_bytes(name);
    let perm = if entry.mode == Mode::Executable {
        0o755
    } else {
        0o644
    };
    let write = |fd: rustix::fd::OwnedFd| -> Result<fs::File> {
        // The mode given at create time holds unless a umask cleared bits: chmod only then (a
        // chmod is one more metadata write per file).
        let st = rustix::fs::fstat(&fd).map_err(|e| io("stat", e))?;
        if Perm::from_raw_mode(st.st_mode) != Perm::from_raw_mode(perm) {
            rustix::fs::fchmod(&fd, Perm::from_raw_mode(perm)).map_err(|e| io("chmod", e))?;
        }
        let mut f = fs::File::from(fd);
        // Not synced here: `apply` makes every file it wrote durable at once at the end.
        f.write_all(bytes)
            .map_err(|e| WcError::io(format!("write {}", shown()), &e))?;
        Ok(f)
    };
    let create =
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    if entry.mode != Mode::Symlink {
        // Nothing at the name (a fresh checkout): written in place. A crash mid-write leaves
        // a file that differs from the entry, which the journal's re-run rewrites.
        match rustix::fs::openat(dir, name, create, Perm::from_raw_mode(perm)) {
            Ok(fd) => {
                let f = write(fd)?;
                return Ok(f.metadata().ok());
            }
            Err(Errno::EXIST) => {}
            Err(e) => return Err(io("create", e)),
        }
    }
    if let Ok(st) = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        && rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::Directory
    {
        // A directory where a file goes: only an empty one is replaced.
        rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR)
            .map_err(|e| io("non-empty directory where a file goes:", e))?;
    }
    if entry.mode == Mode::Symlink {
        match rustix::fs::unlinkat(dir, name, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(e) => return Err(io("remove", e)),
        }
        rustix::fs::symlinkat(OsStr::from_bytes(bytes), dir, name).map_err(|e| io("symlink", e))?;
        return Ok(None);
    }
    let mut tmp_name = b".".to_vec();
    tmp_name.extend_from_slice(name.as_bytes());
    tmp_name.extend_from_slice(b".bee-tmp");
    let tmp = OsStr::from_bytes(&tmp_name);
    match rustix::fs::unlinkat(dir, tmp, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(e) => return Err(io("remove a stale temporary of", e)),
    }
    let fd = rustix::fs::openat(dir, tmp, create, Perm::from_raw_mode(perm))
        .map_err(|e| io("create a temporary for", e))?;
    drop(write(fd)?);
    // `renameat` replaces a symlink at `name` instead of writing through it.
    rustix::fs::renameat(dir, tmp, dir, name).map_err(|e| io("rename into", e))?;
    Ok(None)
}

#[cfg(target_os = "linux")]
fn open_parent_component(
    at: rustix::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    _dir_flags: rustix::fs::OFlags,
) -> std::result::Result<rustix::fd::OwnedFd, rustix::io::Errno> {
    use rustix::fs::{FileType, OFlags};

    // O_PATH | O_NOFOLLOW gives us a handle to the directory entry itself when it is a
    // symlink. Check that handle before using it as a dirfd; O_DIRECTORY | O_NOFOLLOW alone
    // is not sufficient on every Linux-compatible kernel.
    let fd = rustix::fs::openat(
        at,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let st = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::Directory {
        return Err(rustix::io::Errno::NOTDIR);
    }
    Ok(fd)
}

#[cfg(not(target_os = "linux"))]
fn open_parent_component(
    at: rustix::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    dir_flags: rustix::fs::OFlags,
) -> std::result::Result<rustix::fd::OwnedFd, rustix::io::Errno> {
    rustix::fs::openat(at, name, dir_flags, rustix::fs::Mode::empty())
}

#[cfg(not(unix))]
fn write_file_by_path(path: &Path, entry: &Entry, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| WcError::io(format!("create {}", parent.display()), &e))?;
    }
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.is_dir()
    {
        // A directory where a file goes: only an empty one is replaced.
        fs::remove_dir(path).map_err(|e| {
            WcError::io(
                format!(
                    "{} is a non-empty directory where a file goes",
                    path.display()
                ),
                &e,
            )
        })?;
    }
    if entry.mode == Mode::Symlink {
        remove_entry(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(bytes), path)
                .map_err(|e| WcError::io(format!("symlink {}", path.display()), &e))?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            // No symlinks here: the target text is written as a file.
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.bee-tmp"));
    {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| WcError::io(format!("create {}", tmp.display()), &e))?;
        // Not synced here: `apply` makes every file it wrote durable at once at the end.
        f.write_all(bytes)
            .map_err(|e| WcError::io(format!("write {}", tmp.display()), &e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = if entry.mode == Mode::Executable {
                0o755
            } else {
                0o644
            };
            f.set_permissions(fs::Permissions::from_mode(mode))
                .map_err(|e| WcError::io(format!("chmod {}", tmp.display()), &e))?;
        }
    }
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        remove_entry(path)?;
    }
    fs::rename(&tmp, path).map_err(|e| WcError::io(format!("rename {}", path.display()), &e))?;
    Ok(())
}

/// Threads an apply writes files on.
const WRITE_THREADS: usize = 8;
/// Below this many files an apply writes on the calling thread.
const PARALLEL_WRITES_MIN: usize = 64;

/// Files an apply fetches and writes as one group (see [`apply`]).
pub(crate) const PIPELINE_FILES: usize = 2048;

/// One file an apply writes: from `bytes`, or the redaction placeholder when `None`.
struct Job {
    rel: Vec<u8>,
    entry: Entry,
    bytes: Option<Vec<u8>>,
}

/// What one run of [`Job`]s wrote.
#[derive(Default)]
struct Written {
    written: usize,
    hidden: Vec<HiddenPath>,
    stats: Vec<(Vec<u8>, Stat, Entry)>,
    synced: Vec<std::path::PathBuf>,
    dirs: BTreeSet<std::path::PathBuf>,
}

impl Written {
    fn extend(&mut self, w: Written) {
        self.written += w.written;
        self.hidden.extend(w.hidden);
        self.stats.extend(w.stats);
        self.synced.extend(w.synced);
        self.dirs.extend(w.dirs);
    }
}

/// Writes `jobs` on a few threads, each taking a contiguous run of paths (one thread for a
/// short run, or with a fault point armed); results in path order.
fn write_parallel(
    root: &Path,
    jobs: &[Job],
    attrs: &Attributes,
    faults: &Faults,
) -> Result<Written> {
    let threads = if faults.armed() || jobs.len() < PARALLEL_WRITES_MIN {
        1
    } else {
        WRITE_THREADS
    };
    if threads == 1 {
        return write_jobs(root, jobs, attrs, faults);
    }
    let per = jobs.len().div_ceil(threads).max(1);
    let parts: Vec<Result<Written>> = std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .chunks(per)
            .map(|part| s.spawn(move || write_jobs(root, part, attrs, faults)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    Err(WcError::internal("write_failed", "a write thread panicked"))
                })
            })
            .collect()
    });
    let mut all = Written::default();
    for part in parts {
        all.extend(part?);
    }
    Ok(all)
}

fn write_jobs(root: &Path, jobs: &[Job], attrs: &Attributes, faults: &Faults) -> Result<Written> {
    let mut out = Written::default();
    let mut dirs = Dirs::default();
    for job in jobs {
        let (rel, e) = (&job.rel, &job.entry);
        let Some(bytes) = job.bytes.as_deref() else {
            // Status treats the placeholder as unchanged (SPEC §13).
            if let Some(st) = write_placeholder(root, rel)? {
                out.stats.push((rel.clone(), st, *e));
            }
            out.written += 1;
            out.hidden.push(hidden(rel, "redacted"));
            continue;
        };
        let path = fs_path(root, rel)?;
        let written = write_file_with(
            &mut dirs,
            root,
            rel,
            e,
            &worktree_bytes(rel, e, bytes, attrs),
        )?;
        out.written += 1;
        if let Some(meta) = written.or_else(|| fs::symlink_metadata(&path).ok()) {
            out.stats.push((rel.clone(), Stat::of(&meta), *e));
        }
        if let Some(dir) = path.parent() {
            out.dirs.insert(dir.to_path_buf());
        }
        if e.mode != Mode::Symlink {
            out.synced.push(path);
        }
        faults.hit(&format!("files:{}", out.written))?;
    }
    Ok(out)
}

/// Makes the working tree under `root` hold `targets` (`None` = absent).
///
/// Paths in `skip` (case collisions) are not written and are reported as hidden. Blobs come
/// from `.bee/objects/` when present there, else from `fetch`. Text files get the line
/// endings `attrs` give them (`crate::eol`).
pub fn apply(
    root: &Path,
    layout: &Layout,
    targets: &[(PathBytes, Option<Entry>)],
    skip: &BTreeSet<Vec<u8>>,
    fetch: &mut Fetch<'_>,
    faults: &Faults,
    attrs: &Attributes,
) -> Result<Applied> {
    let mut out = Applied::default();
    // Deletes, deepest first.
    let mut deletes: Vec<&Vec<u8>> = targets
        .iter()
        .filter(|(_, e)| e.is_none())
        .map(|(p, _)| &p.0)
        .collect();
    deletes.sort_by(|a, b| b.cmp(a));
    for rel in deletes {
        if symlinked_parent(root, rel)? {
            continue;
        }
        let path = fs_path(root, rel)?;
        if remove_entry(&path)? {
            out.removed += 1;
        }
        prune_empty_parents(root, rel)?;
    }
    // Writes: decide which are needed, fetch what is missing, then write in path order.
    let mut synced = Vec::new();
    let mut needed: Vec<(&Vec<u8>, &Entry)> = Vec::new();
    for (p, e) in targets {
        let Some(e) = e else { continue };
        let rel = &p.0;
        if skip.contains(rel) {
            out.hidden.push(hidden(rel, "case_collision"));
            continue;
        }
        if e.mode == Mode::Gitlink {
            out.hidden.push(hidden(rel, "gitlink"));
            continue;
        }
        if symlinked_parent(root, rel)? {
            out.hidden.push(hidden(rel, "symlinked_parent"));
            continue;
        }
        let path = fs_path(root, rel)?;
        if on_disk_with(&path, rel, attrs)?.as_ref() == Some(e) {
            if let Ok(meta) = fs::symlink_metadata(&path) {
                out.stats.push((rel.clone(), Stat::of(&meta), *e));
            }
            // Possibly written by an apply that crashed before its sync: synced again.
            if e.mode != Mode::Symlink {
                synced.push(path);
            }
            continue;
        }
        needed.push((rel, e));
    }
    // Blobs already in `.bee/objects/`; the rest are fetched group by group below.
    let mut local: BTreeMap<ObjectId, Vec<u8>> = BTreeMap::new();
    for (_, e) in &needed {
        if local.contains_key(&e.id) {
            continue;
        }
        match fsutil::read_optional(&layout.object(&e.id))? {
            Some(bytes) if bee_object::blob_id(&bytes) == e.id => {
                local.insert(e.id, bytes);
            }
            _ => {}
        }
    }
    // Fetched and written in groups of paths, in path order: while one group is written,
    // the next downloads, and only two groups' bytes are held at once. With a fault point
    // armed, one group on one thread, so `files:<n>` stops after exactly n files.
    let group = if faults.armed() {
        needed.len().max(1)
    } else {
        PIPELINE_FILES
    };
    let mut dirs = BTreeSet::new();
    let written = std::thread::scope(|s| -> Result<Written> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<Job>>(1);
        let writer = s.spawn(move || -> Result<Written> {
            let mut all = Written::default();
            for jobs in rx {
                all.extend(write_parallel(root, &jobs, attrs, faults)?);
            }
            Ok(all)
        });
        for paths in needed.chunks(group) {
            let mut ids: Vec<ObjectId> = paths
                .iter()
                .map(|(_, e)| e.id)
                .filter(|id| !local.contains_key(id))
                .collect();
            ids.sort();
            ids.dedup();
            let fetched = if ids.is_empty() {
                BTreeMap::new()
            } else {
                fetch(&ids)?
            };
            // What each path is written from.
            let mut jobs = Vec::with_capacity(paths.len());
            for (rel, e) in paths {
                let bytes = match local.get(&e.id) {
                    Some(b) => Some(b.clone()),
                    None => match fetched.get(&e.id) {
                        Some(Ok(b)) => Some(b.clone()),
                        // A redacted file is written as the fixed placeholder (SPEC §13).
                        Some(Err(reason)) if reason == "REDACTED" => None,
                        Some(Err(_)) => {
                            out.hidden.push(hidden(rel, "not_readable"));
                            continue;
                        }
                        None => {
                            return Err(WcError::internal(
                                "missing_blob",
                                format!("{} was not fetched", path_text(rel)),
                            ));
                        }
                    },
                };
                jobs.push(Job {
                    rel: (*rel).clone(),
                    entry: **e,
                    bytes,
                });
            }
            if tx.send(jobs).is_err() {
                // The writer stopped on an error; joining it below returns that error.
                break;
            }
        }
        drop(tx);
        writer
            .join()
            .unwrap_or_else(|_| Err(WcError::internal("write_failed", "a write thread panicked")))
    })?;
    out.written += written.written;
    out.hidden.extend(written.hidden);
    out.stats.extend(written.stats);
    synced.extend(written.synced);
    dirs.extend(written.dirs);
    // Durable before the caller installs the manifest and clears the intent: the written
    // files and the directories holding their new entries, flushed together.
    let dirs: Vec<_> = dirs.into_iter().collect();
    fsutil::sync_all_paths(&synced, &dirs)?;
    Ok(out)
}

/// Stores bytes in `.bee/objects/` (for conflict renders the server never saw).
pub fn store_local(layout: &Layout, bytes: &[u8]) -> Result<ObjectId> {
    let id = bee_object::blob_id(bytes);
    let path = layout.object(&id);
    if !path.exists() {
        let dir = layout.objects();
        fs::create_dir_all(&dir)
            .map_err(|e| WcError::io(format!("create {}", dir.display()), &e))?;
        fsutil::atomic_write(&path, bytes)?;
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collisions_include_directories() {
        let mut t = Manifest::new();
        for p in ["README", "readme", "Dir/a", "dir/b", "ok/x"] {
            t.insert(
                p.as_bytes().to_vec(),
                Entry::for_bytes(Mode::File, p.as_bytes()),
            );
        }
        let c: Vec<String> = case_collisions(&t).iter().map(|p| path_text(p)).collect();
        assert_eq!(c, vec!["Dir/a", "README", "dir/b", "readme"]);
        let mut clean = Manifest::new();
        clean.insert(b"a".to_vec(), Entry::for_bytes(Mode::File, b"a"));
        assert!(case_collisions(&clean).is_empty());
    }

    /// The W14 sparse-checkout miss: one full sync per written file (an `F_FULLFSYNC` of a few
    /// milliseconds each on macOS) made a 1,000-file checkout take over 4 s. An apply now
    /// issues one device flush for all its files, however many it writes.
    #[test]
    fn apply_flushes_the_device_once_not_once_per_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path();
        let layout = Layout::new(root.join(".bee"));
        fs::create_dir_all(&layout.dir).expect("mkdir");
        let files: Vec<(Vec<u8>, Vec<u8>)> = (0..500)
            .map(|i| {
                (
                    format!("d{:02}/f{i:03}.txt", i % 20).into_bytes(),
                    format!("file {i}\n").into_bytes(),
                )
            })
            .collect();
        let blobs: BTreeMap<ObjectId, Vec<u8>> = files
            .iter()
            .map(|(_, b)| (bee_object::blob_id(b), b.clone()))
            .collect();
        let targets: Vec<(PathBytes, Option<Entry>)> = files
            .iter()
            .map(|(p, b)| (PathBytes(p.clone()), Some(Entry::for_bytes(Mode::File, b))))
            .collect();
        let mut fetch = |ids: &[ObjectId]| {
            Ok(ids
                .iter()
                .map(|i| {
                    (
                        *i,
                        blobs.get(i).cloned().ok_or_else(|| "NOT_FOUND".to_owned()),
                    )
                })
                .collect())
        };
        let before = fsutil::full_syncs_on_this_thread();
        let done = apply(
            root,
            &layout,
            &targets,
            &BTreeSet::new(),
            &mut fetch,
            &Faults::none(),
            &Attributes::new(),
        )
        .expect("apply");
        let flushes = fsutil::full_syncs_on_this_thread() - before;
        assert_eq!(done.written, 500);
        assert!(flushes <= 1, "{flushes} full syncs for 500 files");
        for (p, b) in &files {
            let path = root.join(String::from_utf8(p.clone()).expect("utf-8"));
            assert_eq!(&fs::read(path).expect("read"), b);
        }
    }

    #[test]
    fn apply_writes_deletes_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path();
        let layout = Layout::new(root.join(".bee"));
        fs::create_dir_all(&layout.dir).expect("mkdir");
        let a = Entry::for_bytes(Mode::File, b"alpha\n");
        let x = Entry::for_bytes(Mode::Executable, b"#!/bin/sh\n");
        store_local(&layout, b"alpha\n").expect("store");
        let blobs: BTreeMap<ObjectId, Vec<u8>> = [(x.id, b"#!/bin/sh\n".to_vec())].into();
        let mut calls = 0;
        let mut fetch = |ids: &[ObjectId]| {
            calls += 1;
            Ok(ids
                .iter()
                .map(|i| {
                    (
                        *i,
                        blobs.get(i).cloned().ok_or_else(|| "NOT_FOUND".to_owned()),
                    )
                })
                .collect())
        };
        fs::create_dir_all(root.join("old")).expect("mkdir");
        fs::write(root.join("old/gone"), b"x").expect("write");
        let targets = vec![
            (PathBytes(b"d/a.txt".to_vec()), Some(a)),
            (PathBytes(b"bin/x".to_vec()), Some(x)),
            (PathBytes(b"old/gone".to_vec()), None),
        ];
        let done = apply(
            root,
            &layout,
            &targets,
            &BTreeSet::new(),
            &mut fetch,
            &Faults::none(),
            &Attributes::new(),
        )
        .expect("apply");
        assert_eq!((done.written, done.removed), (2, 1));
        assert_eq!(fs::read(root.join("d/a.txt")).expect("read"), b"alpha\n");
        assert!(!root.join("old").exists(), "empty parent pruned");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(root.join("bin/x"))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        let again = apply(
            root,
            &layout,
            &targets,
            &BTreeSet::new(),
            &mut fetch,
            &Faults::none(),
            &Attributes::new(),
        )
        .expect("again");
        assert_eq!((again.written, again.removed), (0, 0));
        assert_eq!(calls, 1, "nothing fetched the second time");
    }

    #[cfg(unix)]
    #[test]
    fn never_writes_through_a_symlinked_parent() {
        let dir = tempfile::tempdir().expect("tmp");
        let outside = tempfile::tempdir().expect("outside");
        let root = dir.path();
        let layout = Layout::new(root.join(".bee"));
        fs::create_dir_all(&layout.dir).expect("mkdir");
        std::os::unix::fs::symlink(outside.path(), root.join("link")).expect("symlink");
        let e = Entry::for_bytes(Mode::File, b"x");
        store_local(&layout, b"x").expect("store");
        let mut fetch = |_: &[ObjectId]| Ok(BTreeMap::new());
        let done = apply(
            root,
            &layout,
            &[(PathBytes(b"link/f".to_vec()), Some(e))],
            &BTreeSet::new(),
            &mut fetch,
            &Faults::none(),
            &Attributes::new(),
        )
        .expect("apply");
        assert_eq!(done.written, 0);
        assert_eq!(done.hidden[0].reason, "symlinked_parent");
        assert!(!outside.path().join("f").exists());
    }

    /// F46-012: the write itself never follows a symlink, even one swapped in after every
    /// check (simulated by calling the writer directly): a symlinked parent is refused and a
    /// symlink at the file's own name is replaced, never written through.
    #[test]
    fn writes_use_openat_and_never_follow_symlinks() {
        let dir = tempfile::tempdir().expect("tmp");
        let outside = tempfile::tempdir().expect("outside");
        let root = dir.path();
        let e = Entry::for_bytes(Mode::File, b"new");
        // A parent that is a symlink: refused, nothing lands outside.
        std::os::unix::fs::symlink(outside.path(), root.join("link")).expect("symlink");
        let err = write_file(root, b"link/f", &e, b"new").expect_err("symlinked parent");
        assert!(err.message.contains("non-directory parent"), "{err:?}");
        assert!(!outside.path().join("f").exists());
        // A deeper parent swapped for a symlink.
        fs::create_dir_all(root.join("a")).expect("mkdir");
        std::os::unix::fs::symlink(outside.path(), root.join("a/b")).expect("symlink");
        assert!(write_file(root, b"a/b/c/f", &e, b"new").is_err());
        assert!(!outside.path().join("c").exists());
        // A symlink at the name itself: replaced by the file, the target untouched.
        fs::write(outside.path().join("target"), b"keep").expect("write");
        std::os::unix::fs::symlink(outside.path().join("target"), root.join("f")).expect("symlink");
        write_file(root, b"f", &e, b"new").expect("write over a symlink");
        assert_eq!(
            fs::read(outside.path().join("target")).expect("read"),
            b"keep"
        );
        let meta = fs::symlink_metadata(root.join("f")).expect("stat");
        assert!(meta.is_file());
        assert_eq!(fs::read(root.join("f")).expect("read"), b"new");
        // Missing parents are created; executables get their mode; symlinks are written as
        // symlinks.
        let x = Entry::for_bytes(Mode::Executable, b"#!/bin/sh\n");
        write_file(root, b"new/dir/run.sh", &x, b"#!/bin/sh\n").expect("nested");
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(root.join("new/dir/run.sh"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        let l = Entry::for_bytes(Mode::Symlink, b"run.sh");
        write_file(root, b"new/dir/link", &l, b"run.sh").expect("symlink entry");
        assert_eq!(
            fs::read_link(root.join("new/dir/link")).expect("readlink"),
            std::path::PathBuf::from("run.sh")
        );
    }
}
