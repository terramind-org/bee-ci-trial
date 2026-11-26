//! Watcher status: a file-system watcher per working copy, so `status` re-examines only what
//! changed instead of walking the tree.
//!
//! The daemon keeps, per watched root, a journal of paths and the wall time their last event
//! arrived. `changes(root, since_ns)` answers "which paths may have changed since `since_ns`",
//! where `since_ns` is the clock of the working copy's last scan (its stat cache clock). The
//! answer is a superset (paths whose events arrived up to [`SLACK_NS`] before `since_ns` are
//! included), or `fresh: true` when the journal cannot vouch for the whole interval: the
//! watcher started after `since_ns`, the OS dropped events (a rescan flag), the watcher failed,
//! or the sync cookie did not come back in time. A fresh answer means "walk the tree".
//!
//! Before answering, the daemon writes a cookie file under `.bee/` and waits for its event:
//! the OS delivers events in order, so every change made before the call has been journaled.
//! Events under `.bee/` are not working-tree changes and are not reported.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bee_proto::v1::ErrorReason;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use serde::{Deserialize, Serialize};

use crate::error::{Result, err, io};

/// Events that arrived this long before `since_ns` are still reported.
pub const SLACK_NS: i64 = 2_000_000_000;
/// Journal size past which it is reset (and the next answer is fresh).
const MAX_PATHS: usize = 200_000;
/// How long to wait for a sync cookie.
const COOKIE_WAIT: Duration = Duration::from_secs(3);

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

/// The answer to a changes query.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Changes {
    /// True when the caller must walk the whole tree.
    pub fresh: bool,
    /// Why a fresh answer was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Repo-relative paths that may have changed (files or directories), sorted.
    pub paths: Vec<String>,
    /// The daemon's clock when the answer was computed.
    pub now_ns: i64,
    /// When the watcher for this root started.
    pub watching_since_ns: i64,
}

#[derive(Debug, Default)]
struct Journal {
    paths: HashMap<String, i64>,
    overflow_ns: i64,
    cookies: HashSet<String>,
    pending_dirs: HashSet<PathBuf>,
    error: Option<String>,
}

struct Watched {
    root: PathBuf,
    started_ns: i64,
    shared: Arc<(Mutex<Journal>, Condvar)>,
    _watcher: RecommendedWatcher,
}

/// Watchers by working-copy root.
#[derive(Default)]
pub struct WatchRegistry {
    roots: Mutex<HashMap<PathBuf, Arc<Watched>>>,
    cookie_seq: AtomicU64,
}

fn rel_of(root: &Path, canon: &Path, path: &Path) -> Option<String> {
    let rel = path
        .strip_prefix(canon)
        .or_else(|_| path.strip_prefix(root))
        .ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    Some(parts.join("/"))
}

/// Collect the entries already present below a newly created directory. Recursive watcher
/// backends can install watches after delivering the directory-create event, leaving a window
/// in which files created inside it are otherwise missed.
fn entries_below(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut pending = vec![dir.to_path_buf()];
    let mut entries = Vec::new();
    while let Some(current) = pending.pop() {
        let mut children = std::fs::read_dir(&current)?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        children.sort();
        for child in children {
            let metadata = std::fs::symlink_metadata(&child)?;
            if metadata.file_type().is_dir() {
                pending.push(child.clone());
            }
            entries.push(child);
        }
    }
    entries.sort();
    Ok(entries)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl WatchRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Roots being watched.
    pub fn roots(&self) -> Vec<PathBuf> {
        lock(&self.roots).keys().cloned().collect()
    }

    /// Starts watching `root` (a no-op if already watched); returns when the watcher started.
    pub fn watch(&self, root: &Path) -> Result<i64> {
        Ok(self.get_or_start(root)?.started_ns)
    }

    /// Stops watching `root`; returns whether it was watched.
    pub fn unwatch(&self, root: &Path) -> bool {
        let key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        lock(&self.roots).remove(&key).is_some()
    }

    fn get_or_start(&self, root: &Path) -> Result<Arc<Watched>> {
        let canon = root
            .canonicalize()
            .map_err(|e| io(format!("open {}", root.display()), &e))?;
        if let Some(w) = lock(&self.roots).get(&canon) {
            let failed = lock(&w.shared.0).error.is_some();
            if !failed {
                return Ok(w.clone());
            }
        }
        let shared: Arc<(Mutex<Journal>, Condvar)> = Arc::default();
        let handler_shared = shared.clone();
        let (root_c, canon_c) = (root.to_path_buf(), canon.clone());
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let (m, cv) = &*handler_shared;
            let mut j = lock(m);
            let at = now_ns();
            match res {
                Err(e) => j.error = Some(e.to_string()),
                Ok(ev) => {
                    if ev.need_rescan() {
                        j.overflow_ns = at;
                    }
                    let mut event_paths = Vec::new();
                    for p in &ev.paths {
                        event_paths.push(p.clone());
                        if matches!(ev.kind, EventKind::Create(_))
                            && std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir())
                        {
                            j.pending_dirs.insert(p.clone());
                            match entries_below(p) {
                                Ok(mut entries) => event_paths.append(&mut entries),
                                Err(_) => j.overflow_ns = at,
                            }
                        }
                    }
                    // The sync cookie is delivered after changes made before `changes()`.
                    // Rescan directories created since the previous cookie so files created
                    // before recursive watches were installed are included even if the first
                    // directory walk ran before those files were written.
                    let has_cookie = ev.paths.iter().any(|p| {
                        rel_of(&root_c, &canon_c, p)
                            .and_then(|rel| rel.strip_prefix(".bee/").map(str::to_owned))
                            .is_some_and(|cookie| cookie.starts_with("daemon-cookie-"))
                    });
                    if has_cookie {
                        let mut dirs: Vec<PathBuf> = j.pending_dirs.drain().collect();
                        dirs.sort();
                        for dir in dirs {
                            match entries_below(&dir) {
                                Ok(mut entries) => event_paths.append(&mut entries),
                                Err(_) => j.overflow_ns = at,
                            }
                        }
                    }
                    event_paths.sort();
                    event_paths.dedup();
                    for p in &event_paths {
                        let Some(rel) = rel_of(&root_c, &canon_c, p) else {
                            // Not representable as a repo path: be conservative.
                            j.overflow_ns = at;
                            continue;
                        };
                        if rel.is_empty() {
                            if matches!(ev.kind, EventKind::Remove(_)) {
                                j.error = Some("the watched root was removed".into());
                            }
                            continue;
                        }
                        if let Some(cookie) = rel.strip_prefix(".bee/") {
                            if cookie.starts_with("daemon-cookie-") {
                                j.cookies.insert(cookie.to_owned());
                            }
                            continue;
                        }
                        if rel == ".bee" {
                            continue;
                        }
                        j.paths.insert(rel, at);
                    }
                    if j.paths.len() > MAX_PATHS {
                        j.paths.clear();
                        j.overflow_ns = at;
                    }
                }
            }
            cv.notify_all();
        })
        .map_err(|e| {
            err(
                ErrorReason::Unavailable,
                format!("cannot start a watcher: {e}"),
            )
        })?;
        let started_ns = now_ns();
        watcher
            .watch(&canon, RecursiveMode::Recursive)
            .map_err(|e| {
                err(
                    ErrorReason::Unavailable,
                    format!("cannot watch {}: {e}", root.display()),
                )
            })?;
        let w = Arc::new(Watched {
            root: canon.clone(),
            started_ns,
            shared,
            _watcher: watcher,
        });
        lock(&self.roots).insert(canon, w.clone());
        Ok(w)
    }

    /// Paths under `root` that may have changed since `since_ns` (see the module docs).
    pub fn changes(&self, root: &Path, since_ns: i64) -> Result<Changes> {
        let w = self.get_or_start(root)?;
        let mut out = Changes {
            now_ns: now_ns(),
            watching_since_ns: w.started_ns,
            ..Changes::default()
        };
        let fresh = |out: &mut Changes, why: &str| {
            out.fresh = true;
            out.reason = Some(why.to_owned());
            out.paths.clear();
        };
        // Sync: every event before this call is journaled once the cookie's event arrives.
        let name = format!(
            "daemon-cookie-{}-{}",
            std::process::id(),
            self.cookie_seq.fetch_add(1, Ordering::Relaxed)
        );
        let cookie = w.root.join(".bee").join(&name);
        if std::fs::write(&cookie, b"").is_err() {
            fresh(&mut out, "cannot write a sync cookie");
            return Ok(out);
        }
        let (m, cv) = &*w.shared;
        let deadline = std::time::Instant::now() + COOKIE_WAIT;
        let mut j = lock(m);
        while !j.cookies.contains(&name) && j.error.is_none() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            j = cv
                .wait_timeout(j, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        let synced = j.cookies.remove(&name);
        let _ = std::fs::remove_file(&cookie);
        if let Some(e) = &j.error {
            let e = e.clone();
            drop(j);
            self.unwatch(&w.root);
            fresh(&mut out, &format!("watcher failed: {e}"));
            return Ok(out);
        }
        let floor = since_ns.saturating_sub(SLACK_NS);
        if !synced {
            fresh(&mut out, "watcher did not sync in time");
        } else if floor < w.started_ns {
            fresh(&mut out, "watcher started after the last scan");
        } else if j.overflow_ns >= floor {
            fresh(&mut out, "events were dropped; rescan needed");
        } else {
            let mut paths: Vec<String> = j
                .paths
                .iter()
                .filter(|(_, t)| **t >= floor)
                .map(|(p, _)| p.clone())
                .collect();
            paths.sort();
            out.paths = paths;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        let root = Path::new("/a/wc");
        let canon = Path::new("/private/a/wc");
        assert_eq!(
            rel_of(root, canon, Path::new("/private/a/wc/x/y")).as_deref(),
            Some("x/y")
        );
        assert_eq!(
            rel_of(root, canon, Path::new("/a/wc/z")).as_deref(),
            Some("z")
        );
        assert_eq!(rel_of(root, canon, Path::new("/elsewhere")), None);
        assert_eq!(rel_of(root, canon, Path::new("/a/wc")).as_deref(), Some(""));
    }

    #[test]
    fn reports_changes_after_the_watch_started() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join(".bee")).expect("bee dir");
        let reg = WatchRegistry::new();
        let started = reg.watch(dir.path()).expect("watch");
        let first = reg
            .changes(dir.path(), started - 10 * SLACK_NS)
            .expect("changes");
        assert!(
            first.fresh,
            "the watcher cannot vouch for time before it started"
        );
        std::thread::sleep(Duration::from_millis(50));
        let since = now_ns() + SLACK_NS;
        std::fs::write(dir.path().join("a.txt"), b"x").expect("write");
        std::fs::create_dir_all(dir.path().join("d/e")).expect("mkdir");
        std::fs::write(dir.path().join("d/e/f.txt"), b"y").expect("write");
        let got = reg.changes(dir.path(), since).expect("changes");
        assert!(!got.fresh, "{got:?}");
        assert!(got.paths.contains(&"a.txt".to_owned()), "{got:?}");
        assert!(got.paths.contains(&"d/e/f.txt".to_owned()), "{got:?}");
        assert!(!got.paths.iter().any(|p| p.starts_with(".bee")), "{got:?}");
        assert!(reg.unwatch(dir.path()));
    }

    #[test]
    fn reports_files_created_inside_new_directories() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join(".bee")).expect("bee dir");
        let reg = WatchRegistry::new();
        let started = reg.watch(dir.path()).expect("watch");
        let since = now_ns() + SLACK_NS;

        std::fs::create_dir_all(dir.path().join("x/y/z")).expect("nested dirs");
        std::fs::write(dir.path().join("x/a.txt"), b"a").expect("write");
        std::fs::write(dir.path().join("x/y/b.txt"), b"b").expect("write");
        std::fs::write(dir.path().join("x/y/z/c.txt"), b"c").expect("write");

        let got = reg.changes(dir.path(), since).expect("changes");
        assert!(!got.fresh, "{got:?}");
        for path in ["x/a.txt", "x/y/b.txt", "x/y/z/c.txt"] {
            assert!(
                got.paths.contains(&path.to_owned()),
                "missing {path}: {got:?}"
            );
        }
        assert!(started <= got.watching_since_ns);
        assert!(reg.unwatch(dir.path()));
    }
}
