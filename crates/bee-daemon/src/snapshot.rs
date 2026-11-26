//! Snapshots: materialize the tree of a revision into a directory straight from the cache.
//!
//! Every object is fetched once into the shared cache (coalesced, authorized for the caller),
//! then each file is created from its cache file by one of:
//!
//! - **clone** (default): `std::fs::copy`, which is `fclonefileat` (copy-on-write) on APFS and
//!   `copy_file_range` (reflink on Btrfs/XFS) on Linux, so a checkout costs metadata, not
//!   bytes, and its files are ordinary writable files;
//! - **hardlink**: a hard link to the cache file, the cheapest form, for read-only consumers
//!   (CI jobs, agents that only read). Files are read-only (`0444`) and share the cache's
//!   inode: editing one in place is refused by its mode, and a write-and-rename editor
//!   replaces the link without touching the cache. Executables are cloned instead, because a
//!   link would share the cache file's mode;
//! - **copy**: plain read and write, for stores without files.
//!
//! The objects stay pinned while they are linked, so eviction never races a snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use bee_core::{Mode, ObjectId, RepoPath};
use bee_proto::v1::{self, ErrorReason};
use serde::{Deserialize, Serialize};

use crate::error::{Result, err, invalid, io};
use crate::store::{ObjectStore, PinGuard, verify};
use crate::tier::Tier;
use crate::upstream::Auth;

/// How snapshot files are created from cache files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    /// Copy-on-write clone (falls back to a copy where the file system cannot clone).
    #[default]
    Clone,
    /// Hard link to the cache file (read-only files).
    Hardlink,
    /// Plain copy.
    Copy,
}

/// What to materialize.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRequest {
    /// Repo, `org/name` or an id.
    pub repo: String,
    /// Revision expression (ref name, changeset id, …).
    pub revision: String,
    /// Destination directory: created, or empty.
    #[serde(default)]
    pub dest: PathBuf,
    /// Sparse prefixes (empty = whole repo).
    #[serde(default)]
    pub sparse: Vec<String>,
    /// Exact paths only (virtual checkouts materializing on access); empty = no filter.
    #[serde(default)]
    pub paths: Vec<String>,
    /// How files are created.
    #[serde(default)]
    pub mode: LinkMode,
    /// Re-read and verify every written file against its id.
    #[serde(default)]
    pub verify: bool,
    /// Allow writing into a non-empty destination (only missing files are created).
    #[serde(default)]
    pub into_existing: bool,
}

/// A path that was not written, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skipped {
    /// Repo path.
    pub path: String,
    /// `REDACTED`, `NOT_FOUND`, `restricted`, `case_collision`, `exists`, …
    pub reason: String,
}

/// Phase timings in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timings {
    /// Resolving the revision and listing the tree.
    pub list_ms: f64,
    /// Making sure every object is cached.
    pub fetch_ms: f64,
    /// Creating directories, files and links.
    pub materialize_ms: f64,
    /// Everything.
    pub total_ms: f64,
}

/// What a snapshot (or prefetch) did.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReport {
    /// The changeset materialized.
    pub changeset: String,
    /// Regular and executable files written.
    pub files: u64,
    /// Symlinks written.
    pub symlinks: u64,
    /// Bytes of the files written.
    pub bytes: u64,
    /// Files hard-linked to the cache.
    pub linked: u64,
    /// Files cloned (or copied by the clone path).
    pub cloned: u64,
    /// Files copied byte by byte.
    pub copied: u64,
    /// Objects that had to come from the tier above.
    pub fetched: u64,
    /// Paths not written.
    pub skipped: Vec<Skipped>,
    /// Timings.
    pub timings: Timings,
}

/// One leaf to materialize.
#[derive(Clone, Debug)]
pub struct Leaf {
    /// Repo path.
    pub path: Vec<u8>,
    /// File, executable or symlink.
    pub mode: Mode,
    /// Blob id.
    pub id: ObjectId,
    /// Size.
    pub size: u64,
}

/// A repo reference from `org/name` or an id.
pub fn repo_ref(repo: &str) -> v1::RepoRef {
    let r = if repo.contains('/') {
        v1::repo_ref::Repo::FullName(repo.to_owned())
    } else {
        v1::repo_ref::Repo::Id(repo.to_owned())
    };
    v1::RepoRef { repo: Some(r) }
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

fn mode_of(m: i32) -> Option<Mode> {
    match v1::Mode::try_from(m).ok()? {
        v1::Mode::File => Some(Mode::File),
        v1::Mode::Executable => Some(Mode::Executable),
        v1::Mode::Symlink => Some(Mode::Symlink),
        v1::Mode::Tree | v1::Mode::Unspecified => None,
    }
}

fn path_text(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

/// The leaves of changeset `at` inside `sparse` (and `paths`, when given), with the paths
/// listed but not readable by this credential.
pub async fn leaves(
    tier: &Tier,
    auth: &Auth,
    repo: &v1::RepoRef,
    at: &ObjectId,
    sparse: &[String],
    paths: &[String],
) -> Result<(Vec<Leaf>, Vec<Skipped>)> {
    let mut prefixes: Vec<Vec<u8>> = sparse
        .iter()
        .map(|p| p.trim_matches('/').as_bytes().to_vec())
        .collect();
    if prefixes.iter().any(Vec::is_empty) || prefixes.is_empty() {
        prefixes = vec![Vec::new()];
    }
    let wanted: BTreeSet<Vec<u8>> = paths
        .iter()
        .map(|p| p.trim_matches('/').as_bytes().to_vec())
        .collect();
    let mut out: BTreeMap<Vec<u8>, Leaf> = BTreeMap::new();
    let mut skipped = Vec::new();
    for prefix in prefixes {
        let entries = match tier.list_tree(auth, repo, at, &prefix).await {
            Ok(e) => e,
            Err(e)
                if !prefix.is_empty()
                    && matches!(e.reason.as_str(), "NOT_FOUND" | "INVALID_ARGUMENT") =>
            {
                // The prefix is a file (or absent): find it in its parent's listing.
                let parent = match prefix.iter().rposition(|&b| b == b'/') {
                    Some(i) => prefix[..i].to_vec(),
                    None => Vec::new(),
                };
                match tier.list_tree(auth, repo, at, &parent).await {
                    Ok(list) => {
                        Arc::new(list.iter().filter(|t| t.path == prefix).cloned().collect())
                    }
                    Err(e2) if matches!(e2.reason.as_str(), "NOT_FOUND" | "INVALID_ARGUMENT") => {
                        Arc::new(Vec::new())
                    }
                    Err(e2) => return Err(e2),
                }
            }
            Err(e) => return Err(e),
        };
        for t in entries.iter() {
            if !wanted.is_empty() && !wanted.contains(&t.path) {
                continue;
            }
            let Some(mode) = mode_of(t.mode) else {
                continue;
            };
            if t.restricted {
                skipped.push(Skipped {
                    path: path_text(&t.path),
                    reason: "restricted".into(),
                });
                continue;
            }
            let id =
                t.id.as_ref()
                    .and_then(|i| ObjectId::from_bytes(&i.raw).ok())
                    .ok_or_else(|| err(ErrorReason::Internal, "tree entry without a valid id"))?;
            out.insert(
                t.path.clone(),
                Leaf {
                    path: t.path.clone(),
                    mode,
                    id,
                    size: t.size,
                },
            );
        }
    }
    Ok((out.into_values().collect(), skipped))
}

/// Resolves, lists and caches the objects of a revision; returns the leaves, failures and the
/// number of objects fetched upstream. Shared by snapshots, prefetch and pinning.
pub async fn prepare(
    tier: &Tier,
    auth: &Auth,
    req: &SnapshotRequest,
    report: &mut SnapshotReport,
) -> Result<Vec<Leaf>> {
    let t0 = Instant::now();
    let repo = repo_ref(&req.repo);
    let at = tier.resolve_revision(auth, &repo, &req.revision).await?;
    report.changeset = at.to_text();
    let (mut leaves, skipped) = leaves(tier, auth, &repo, &at, &req.sparse, &req.paths).await?;
    report.skipped.extend(skipped);
    report.timings.list_ms = ms(t0);
    let t1 = Instant::now();
    let mut ids: Vec<ObjectId> = leaves.iter().map(|l| l.id).collect();
    ids.sort();
    ids.dedup();
    let before = tier.stats_view().upstream_objects;
    let failures = tier.ensure(auth, &repo, &ids).await?;
    report.fetched = tier.stats_view().upstream_objects.saturating_sub(before);
    if !failures.is_empty() {
        let failed: BTreeMap<ObjectId, String> = failures.into_iter().collect();
        leaves.retain(|l| match failed.get(&l.id) {
            Some(reason) => {
                report.skipped.push(Skipped {
                    path: path_text(&l.path),
                    reason: reason.clone(),
                });
                false
            }
            None => true,
        });
    }
    report.timings.fetch_ms = ms(t1);
    Ok(leaves)
}

/// Materializes a revision into `req.dest`.
pub async fn snapshot(
    tier: Arc<Tier>,
    auth: &Auth,
    req: SnapshotRequest,
) -> Result<SnapshotReport> {
    let t0 = Instant::now();
    if req.dest.as_os_str().is_empty() {
        return Err(invalid("missing_field", "dest is required"));
    }
    if !req.into_existing && req.dest.exists() {
        let mut entries =
            fs::read_dir(&req.dest).map_err(|e| io(format!("read {}", req.dest.display()), &e))?;
        if entries.next().is_some() {
            return Err(err(
                ErrorReason::InvalidState,
                format!("{} is not empty", req.dest.display()),
            ));
        }
    }
    let mut report = SnapshotReport::default();
    let leaves = prepare(&tier, auth, &req, &mut report).await?;
    let store = tier.store().clone();
    let ids: Vec<ObjectId> = leaves.iter().map(|l| l.id).collect();
    let pins = PinGuard::new(store.clone(), ids);
    let t2 = Instant::now();
    let dest = req.dest.clone();
    let (mode, verify_files, existing) = (req.mode, req.verify, req.into_existing);
    let mut report = tokio::task::spawn_blocking(move || {
        materialize(
            store.as_ref(),
            &dest,
            &leaves,
            mode,
            verify_files,
            existing,
            &mut report,
        )
        .map(|()| report)
    })
    .await
    .map_err(|e| err(ErrorReason::Internal, format!("snapshot task failed: {e}")))??;
    drop(pins);
    report.timings.materialize_ms = ms(t2);
    report.timings.total_ms = ms(t0);
    Ok(report)
}

#[derive(Default)]
struct Tally {
    files: u64,
    symlinks: u64,
    bytes: u64,
    linked: u64,
    cloned: u64,
    copied: u64,
    skipped: Vec<Skipped>,
}

fn case_insensitive(dir: &Path) -> bool {
    let probe = dir.join(".bee-case-probe");
    if fs::write(&probe, b"").is_err() {
        return false;
    }
    let folded = dir.join(".BEE-CASE-PROBE").exists();
    let _ = fs::remove_file(&probe);
    folded
}

/// Creates every leaf under `dest` from the store's files.
pub fn materialize(
    store: &dyn ObjectStore,
    dest: &Path,
    leaves: &[Leaf],
    mode: LinkMode,
    verify_files: bool,
    into_existing: bool,
    report: &mut SnapshotReport,
) -> Result<()> {
    fs::create_dir_all(dest).map_err(|e| io(format!("create {}", dest.display()), &e))?;
    let mut todo: Vec<&Leaf> = Vec::with_capacity(leaves.len());
    let mut skipped = Vec::new();
    let mut folded: BTreeSet<Vec<u8>> = BTreeSet::new();
    let fold = case_insensitive(dest);
    for l in leaves {
        if RepoPath::new(l.path.clone()).is_err() {
            skipped.push(Skipped {
                path: path_text(&l.path),
                reason: "invalid_path".into(),
            });
            continue;
        }
        if fold && !folded.insert(l.path.to_ascii_lowercase()) {
            skipped.push(Skipped {
                path: path_text(&l.path),
                reason: "case_collision".into(),
            });
            continue;
        }
        todo.push(l);
    }
    // Every directory to create, parents before children (component order sorts them so).
    let mut dirs: BTreeSet<Vec<u8>> = BTreeSet::new();
    for l in &todo {
        let mut end = 0;
        while let Some(i) = l.path[end..].iter().position(|&b| b == b'/') {
            end += i;
            dirs.insert(l.path[..end].to_vec());
            end += 1;
        }
    }
    let mut ordered: Vec<(PathBuf, Vec<u8>)> =
        dirs.into_iter().map(|d| (fs_path(dest, &d), d)).collect();
    ordered.sort();
    for (d, _) in &ordered {
        match fs::create_dir(d) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && into_existing => {
                // Never write through a pre-existing symlinked parent (SPEC §10.3).
                if fs::symlink_metadata(d).is_ok_and(|m| !m.file_type().is_dir()) {
                    return Err(err(
                        ErrorReason::InvalidState,
                        format!(
                            "{} is not a directory; refusing to write through it",
                            d.display()
                        ),
                    ));
                }
            }
            Err(e) => return Err(io(format!("create {}", d.display()), &e)),
        }
    }
    // A thread per 512 files, at most 8: small trees stay on few threads when many
    // snapshots run at once.
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8)
        .min(todo.len().div_ceil(512).max(1));
    let per = todo.len().div_ceil(workers).max(1);
    let results: Vec<Result<Tally>> = std::thread::scope(|s| {
        let handles: Vec<_> = todo
            .chunks(per)
            .map(|part| {
                s.spawn(move || {
                    let mut t = Tally::default();
                    for l in part {
                        write_leaf(store, dest, l, mode, verify_files, into_existing, &mut t)?;
                    }
                    Ok(t)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(err(ErrorReason::Internal, "snapshot worker panicked")))
            })
            .collect()
    });
    for r in results {
        let t = r?;
        report.files += t.files;
        report.symlinks += t.symlinks;
        report.bytes += t.bytes;
        report.linked += t.linked;
        report.cloned += t.cloned;
        report.copied += t.copied;
        report.skipped.extend(t.skipped);
    }
    report.skipped.extend(skipped);
    Ok(())
}

fn fs_path(root: &Path, rel: &[u8]) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split(|&b| b == b'/') {
        p.push(os_name(part));
    }
    p
}

#[cfg(unix)]
fn os_name(b: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::OsStr::from_bytes(b).to_owned()
}

#[cfg(not(unix))]
fn os_name(b: &[u8]) -> std::ffi::OsString {
    String::from_utf8_lossy(b).into_owned().into()
}

fn set_mode(path: &Path, mode: Mode) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let bits = if mode == Mode::Executable {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(path, fs::Permissions::from_mode(bits))
            .map_err(|e| io(format!("chmod {}", path.display()), &e))
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        let mut perms = fs::metadata(path)
            .map_err(|e| io(format!("stat {}", path.display()), &e))?
            .permissions();
        perms.set_readonly(false);
        fs::set_permissions(path, perms).map_err(|e| io(format!("chmod {}", path.display()), &e))
    }
}

fn write_leaf(
    store: &dyn ObjectStore,
    dest: &Path,
    l: &Leaf,
    mode: LinkMode,
    verify_files: bool,
    into_existing: bool,
    t: &mut Tally,
) -> Result<()> {
    let target = fs_path(dest, &l.path);
    if fs::symlink_metadata(&target).is_ok() {
        if into_existing {
            return Ok(());
        }
        t.skipped.push(Skipped {
            path: path_text(&l.path),
            reason: "exists".into(),
        });
        return Ok(());
    }
    if l.mode == Mode::Symlink {
        let bytes = store.get(&l.id).ok_or_else(|| {
            err(
                ErrorReason::Internal,
                format!("object {} left the cache", l.id),
            )
        })?;
        make_symlink(&os_name(&bytes), &target)?;
        t.symlinks += 1;
        return Ok(());
    }
    let src = store.file_path(&l.id);
    match (mode, &src) {
        (LinkMode::Hardlink, Some(src)) if l.mode == Mode::File => {
            fs::hard_link(src, &target)
                .map_err(|e| io(format!("link {}", target.display()), &e))?;
            t.linked += 1;
        }
        (LinkMode::Clone | LinkMode::Hardlink, Some(src)) => {
            fs::copy(src, &target).map_err(|e| io(format!("clone {}", target.display()), &e))?;
            set_mode(&target, l.mode)?;
            t.cloned += 1;
        }
        _ => {
            let bytes = store.get(&l.id).ok_or_else(|| {
                err(
                    ErrorReason::Internal,
                    format!("object {} left the cache", l.id),
                )
            })?;
            fs::write(&target, &bytes)
                .map_err(|e| io(format!("write {}", target.display()), &e))?;
            set_mode(&target, l.mode)?;
            t.copied += 1;
        }
    }
    if verify_files {
        let bytes = fs::read(&target).map_err(|e| io(format!("read {}", target.display()), &e))?;
        if !verify(&l.id, &bytes) {
            return Err(err(
                ErrorReason::Integrity,
                format!("{} does not match {}", target.display(), l.id),
            ));
        }
    }
    t.files += 1;
    t.bytes += l.size;
    Ok(())
}

#[cfg(unix)]
fn make_symlink(target: &std::ffi::OsStr, at: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, at).map_err(|e| io(format!("symlink {}", at.display()), &e))
}

#[cfg(not(unix))]
fn make_symlink(target: &std::ffi::OsStr, at: &Path) -> Result<()> {
    fs::write(at, target.to_string_lossy().as_bytes())
        .map_err(|e| io(format!("write {}", at.display()), &e))
}
