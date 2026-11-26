//! The operation journal: write-ahead intent records that make every mutating command
//! crash-safe, the operation log behind `bee undo`, the advisory lock, and fault points.
//!
//! A mutating command first does its server work (which is idempotent and touches nothing
//! local), then commits locally in this order:
//!
//! 1. write `journal/manifest.next` (the new base manifest) and fsync it;
//! 2. write `journal/pending.json` atomically: the [`OpRecord`] with the state after the
//!    operation, and the working-tree targets (path -> entry or absent). From here on the
//!    operation rolls forward;
//! 3. make the working tree match the targets;
//! 4. install the manifest, write `state.toml`, append the record to `oplog.jsonl`;
//! 5. delete `pending.json`.
//!
//! The next command that finds `pending.json` repeats steps 3 to 5, each of which is
//! idempotent. A crash before step 2 completes leaves no intent, so nothing local changed.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Result, WcError};
use crate::fsutil;
use crate::manifest::Entry;
use crate::state::{PathBytes, State};

/// A completed operation, as kept in `oplog.jsonl`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpRecord {
    /// Sequence number, from 1.
    pub seq: u64,
    /// Operation name: `clone`, `commit`, `amend`, `sync`, ...
    pub op: String,
    /// One-line description.
    pub summary: String,
    /// State before.
    pub before: State,
    /// State after.
    pub after: State,
    /// The base manifest's previous entries for every path it changed (`None` = absent).
    pub manifest_undo: Vec<(PathBytes, Option<Entry>)>,
    /// Undo keeps the working files (the operation absorbed working changes).
    pub soft: bool,
    /// False for operations with server effects that undo cannot reverse (land, clone).
    pub undoable: bool,
    /// For an undo: the sequence number it undid.
    pub undoes: Option<u64>,
}

/// The write-ahead intent of an operation in progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    /// The record to append when done.
    pub record: OpRecord,
    /// Working-tree paths and what each must hold (`None` = absent).
    pub targets: Vec<(PathBytes, Option<Entry>)>,
}

/// Paths inside `.bee/`.
#[derive(Clone, Debug)]
pub struct Layout {
    /// `.bee/`.
    pub dir: PathBuf,
}

impl Layout {
    /// The layout for `.bee/` at `dir`.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `config.json`.
    pub fn config(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    /// `state.toml` (canonical TOML).
    pub fn state(&self) -> PathBuf {
        self.dir.join("state.toml")
    }

    /// `state.json`, the state file of older clients, replaced on the next write.
    pub fn legacy_state(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// `manifest`.
    pub fn manifest(&self) -> PathBuf {
        self.dir.join("manifest")
    }

    /// `statcache`.
    pub fn statcache(&self) -> PathBuf {
        self.dir.join("statcache")
    }

    /// `oplog.jsonl`.
    pub fn oplog(&self) -> PathBuf {
        self.dir.join("oplog.jsonl")
    }

    /// `conflicts/`: one TOML file per conflicted path of the rebase in progress.
    pub fn conflicts(&self) -> PathBuf {
        self.dir.join("conflicts")
    }

    /// `lock`.
    pub fn lock(&self) -> PathBuf {
        self.dir.join("lock")
    }

    /// `journal/`.
    pub fn journal(&self) -> PathBuf {
        self.dir.join("journal")
    }

    /// `journal/pending.json`.
    pub fn pending(&self) -> PathBuf {
        self.journal().join("pending.json")
    }

    /// `journal/manifest.next`.
    pub fn manifest_next(&self) -> PathBuf {
        self.journal().join("manifest.next")
    }

    /// `objects/`: local blobs (conflict renders) by id.
    pub fn objects(&self) -> PathBuf {
        self.dir.join("objects")
    }

    /// `objects/<id>`.
    pub fn object(&self, id: &bee_core::ObjectId) -> PathBuf {
        self.objects().join(id.to_text())
    }
}

/// Fault injection: stop at a named point, as if the process had been killed there.
///
/// Points: `journal` (intent written, nothing applied), `files:<n>` (after `n` working-tree
/// writes), `manifest` (manifest installed), `state` (state written, oplog not appended).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    point: Option<String>,
}

impl Faults {
    /// No fault armed.
    pub fn none() -> Self {
        Self::default()
    }

    /// Arms `point`.
    pub fn at(point: impl Into<String>) -> Self {
        Self {
            point: Some(point.into()),
        }
    }

    /// From `BEE_WC_FAULT`, if set.
    pub fn from_env() -> Self {
        match std::env::var("BEE_WC_FAULT") {
            Ok(p) if !p.trim().is_empty() => Self::at(p.trim()),
            _ => Self::none(),
        }
    }

    /// True when a fault point is armed.
    pub fn armed(&self) -> bool {
        self.point.is_some()
    }

    /// Fails with [`WcError::fault`] if `point` is armed.
    pub fn hit(&self, point: &str) -> Result<()> {
        if self.point.as_deref() == Some(point) {
            return Err(WcError::fault(point));
        }
        Ok(())
    }
}

/// The advisory lock: one command at a time per working copy. Held until dropped; the
/// operating system releases it if the process dies.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

impl Lock {
    /// Takes the lock, waiting up to `wait` for another command to finish.
    pub fn acquire(layout: &Layout, wait: Duration) -> Result<Lock> {
        let path = layout.lock();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| WcError::io(format!("open {}", path.display()), &e))?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Lock { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {
                    if start.elapsed() >= wait {
                        return Err(WcError::state(
                            "working_copy_locked",
                            format!(
                                "another bee command holds {}; retry when it finishes",
                                path.display()
                            ),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(fs::TryLockError::Error(e)) => {
                    return Err(WcError::io(format!("lock {}", path.display()), &e));
                }
            }
        }
    }
}

/// Reads the pending intent, if any. A torn or unreadable intent cannot have been committed
/// (it is written atomically), so it is treated as absent.
pub fn read_pending(layout: &Layout) -> Result<Option<Pending>> {
    let Some(bytes) = fsutil::read_optional(&layout.pending())? else {
        return Ok(None);
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

/// Writes the intent (step 2).
pub fn write_pending(layout: &Layout, pending: &Pending) -> Result<()> {
    let dir = layout.journal();
    fs::create_dir_all(&dir).map_err(|e| WcError::io(format!("create {}", dir.display()), &e))?;
    let bytes =
        serde_json::to_vec(pending).map_err(|e| WcError::internal("encode", e.to_string()))?;
    fsutil::atomic_write(&layout.pending(), &bytes)
}

/// Deletes the intent and any leftover journal files (step 5).
pub fn clear_pending(layout: &Layout) -> Result<()> {
    fsutil::remove_optional(&layout.pending())?;
    fsutil::remove_optional(&layout.manifest_next())?;
    fsutil::fsync_dir(&layout.journal())
}

/// Every record of the operation log, oldest first. A torn final line (a crash during an
/// append) is ignored.
pub fn read_oplog(layout: &Layout) -> Result<Vec<OpRecord>> {
    let Some(bytes) = fsutil::read_optional(&layout.oplog())? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let lines: Vec<&[u8]> = bytes
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_slice::<OpRecord>(line) {
            Ok(r) => out.push(r),
            Err(_) if i + 1 == lines.len() => break,
            Err(e) => {
                return Err(WcError::integrity(
                    "corrupt_file",
                    format!("oplog.jsonl line {}: {e}", i + 1),
                ));
            }
        }
    }
    Ok(out)
}

/// Appends a record unless the log already ends with its sequence number.
pub fn append_oplog(layout: &Layout, record: &OpRecord) -> Result<()> {
    let existing = read_oplog(layout)?;
    if existing.last().is_some_and(|r| r.seq >= record.seq) {
        return Ok(());
    }
    let path = layout.oplog();
    // Drop a torn final line before appending, so the log stays one record per line.
    if let Some(bytes) = fsutil::read_optional(&path)?
        && !bytes.is_empty()
        && bytes.last() != Some(&b'\n')
    {
        let keep = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        fsutil::atomic_write(&path, &bytes[..keep])?;
    }
    let mut line =
        serde_json::to_vec(record).map_err(|e| WcError::internal("encode", e.to_string()))?;
    line.push(b'\n');
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| WcError::io(format!("open {}", path.display()), &e))?;
    f.write_all(&line)
        .map_err(|e| WcError::io(format!("append {}", path.display()), &e))?;
    fsutil::full_sync(&f, &path)?;
    if let Some(dir) = path.parent() {
        fsutil::fsync_dir(dir)?;
    }
    Ok(())
}

/// The next sequence number.
pub fn next_seq(layout: &Layout) -> Result<u64> {
    Ok(read_oplog(layout)?.last().map_or(1, |r| r.seq + 1))
}

/// True if `path` exists.
pub fn exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_command_at_a_time() {
        let dir = tempfile::tempdir().expect("tmp");
        let layout = Layout::new(dir.path().to_path_buf());
        let held = Lock::acquire(&layout, Duration::from_millis(10)).expect("first");
        let e = Lock::acquire(&layout, Duration::from_millis(50)).expect_err("second");
        assert_eq!(e.subcode, "working_copy_locked");
        drop(held);
        Lock::acquire(&layout, Duration::from_millis(10)).expect("after release");
    }

    #[test]
    fn torn_oplog_lines_are_dropped() {
        let dir = tempfile::tempdir().expect("tmp");
        let layout = Layout::new(dir.path().to_path_buf());
        fs::write(layout.oplog(), b"{\"seq\":").expect("torn");
        assert!(read_oplog(&layout).expect("read").is_empty());
        assert_eq!(next_seq(&layout).expect("seq"), 1);
    }
}
