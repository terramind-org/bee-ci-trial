//! Watcher status: status computed from the daemon's watcher journal equals a full rehash of
//! the working tree under randomized edits, and rescans only what changed.

use crate::common;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bee_daemon::{TierConfig, TierKind, Upstream};
use bee_wc::{OpenOptions, Sparse, WorkingCopy};
use common::*;
use serde_json::json;

/// Deterministic xorshift, so a failing run can be replayed from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if dir == root && e.file_name() == ".bee" {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(root, &p, out),
                Ok(_) => out.push(p),
                Err(_) => {}
            }
        }
    }
    walk(root, root, &mut out);
    out.sort();
    out
}

fn dirs_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if dir == root && e.file_name() == ".bee" {
                continue;
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                out.push(p.clone());
                walk(root, &p, out);
            }
        }
    }
    walk(root, root, &mut out);
    out.sort();
    out
}

fn random_edit(root: &Path, rng: &mut Rng, n: usize) -> String {
    let files = files_under(root);
    let pick = |rng: &mut Rng| files.get(rng.below(files.len())).cloned();
    let name = format!("n{n}");
    match rng.below(13) {
        0 => {
            let Some(f) = pick(rng) else {
                return "noop".into();
            };
            let _ = fs::write(&f, format!("rewritten {n}\n"));
            format!("rewrite {}", f.display())
        }
        1 => {
            // Same size, same instant: only the content differs.
            let Some(f) = pick(rng) else {
                return "noop".into();
            };
            if let Ok(mut b) = fs::read(&f)
                && !b.is_empty()
            {
                b[0] = if b[0] == b'#' { b'%' } else { b'#' };
                let _ = fs::write(&f, &b);
            }
            format!("flip {}", f.display())
        }
        2 => {
            let dir = root
                .join(format!("d{}", rng.below(4)))
                .join(format!("e{}", rng.below(3)));
            let _ = fs::create_dir_all(&dir);
            let _ = fs::write(dir.join(format!("{name}.txt")), format!("new {n}\n"));
            format!("create {}", dir.display())
        }
        3 => {
            let Some(f) = pick(rng) else {
                return "noop".into();
            };
            let _ = fs::remove_file(&f);
            format!("delete {}", f.display())
        }
        4 => {
            let Some(f) = pick(rng) else {
                return "noop".into();
            };
            let to = f.with_file_name(format!("{name}-moved.txt"));
            let _ = fs::rename(&f, &to);
            format!("rename {}", f.display())
        }
        5 => {
            let dirs = dirs_under(root);
            let Some(d) = dirs.get(rng.below(dirs.len())).cloned() else {
                return "noop".into();
            };
            let to = d.with_file_name(format!("{name}-dir"));
            let _ = fs::rename(&d, &to);
            format!("rename dir {}", d.display())
        }
        6 => {
            let _ = fs::write(root.join(format!("{name}.log")), b"ignored by *.log");
            let _ = fs::create_dir_all(root.join("build"));
            let _ = fs::write(root.join("build").join(format!("{name}.o")), b"ignored dir");
            "ignored files".into()
        }
        7 => {
            let gi = root.join(".gitignore");
            let text = fs::read_to_string(&gi).unwrap_or_default();
            let next = if text.contains("tmp*") {
                text.replace("tmp*\n", "")
            } else {
                format!("{text}tmp*\n")
            };
            let _ = fs::write(&gi, next);
            let _ = fs::write(root.join(format!("tmp{n}.txt")), b"maybe ignored");
            "toggle .gitignore".into()
        }
        8 => {
            let Some(f) = pick(rng) else {
                return "noop".into();
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if let Ok(m) = fs::symlink_metadata(&f)
                    && m.file_type().is_file()
                {
                    let mode = m.permissions().mode() ^ 0o111;
                    let _ = fs::set_permissions(&f, fs::Permissions::from_mode(mode));
                }
            }
            format!("chmod {}", f.display())
        }
        9 => {
            #[cfg(unix)]
            {
                let _ = std::os::unix::fs::symlink("README.md", root.join(format!("{name}.lnk")));
            }
            "symlink".into()
        }
        10 => {
            // Edit, then restore: no net change.
            let Some(f) = pick(rng) else {
                return "noop".into();
            };
            if let Ok(orig) = fs::read(&f) {
                let _ = fs::write(&f, b"temporary");
                let _ = fs::write(&f, &orig);
            }
            format!("edit+restore {}", f.display())
        }
        11 => {
            let _ = fs::create_dir_all(root.join(format!("{name}-empty/inner")));
            "empty dirs".into()
        }
        _ => {
            let Some(f) = pick(rng) else {
                return "noop".into();
            };
            let _ = fs::write(&f, format!("{}{n}", "y".repeat(rng.below(200))));
            format!("resize {}", f.display())
        }
    }
}

fn seed_files(n: usize) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = (0..n)
        .map(|i| {
            (
                format!("src/m{}/f{i:04}.rs", i % 7),
                format!("// file {i}\nfn f{i}() {{}}\n").into_bytes(),
            )
        })
        .collect();
    files.push(("README.md".into(), b"# readme\n".to_vec()));
    files.push((".gitignore".into(), b"*.log\nbuild/\n".to_vec()));
    files.push(("docs/guide.md".into(), b"guide\n".to_vec()));
    files
}

#[test]
fn watcher_status_equals_a_full_walk_under_randomized_edits() {
    let srv = start_server();
    srv.land_files(&seed_files(80), "seed");
    let up: Arc<dyn Upstream> = Arc::new(srv.http_upstream());
    let mut d = start_daemon(&srv.rt, up, TierConfig::new(TierKind::Daemon));
    let t = srv.transport();
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("wc");
    drop(clone_wc(&root, &t, Sparse::full()));
    let daemon = d.client(None);
    daemon
        .local("Watch", &json!({ "root": root }))
        .expect("watch");
    // The first scan must come after the watcher's start plus the journal's slack.
    std::thread::sleep(Duration::from_millis(2_100));
    let mut wc = WorkingCopy::open(&root, &t, &OpenOptions::default()).expect("open");
    assert!(wc.status().expect("status").clean);
    drop(wc);
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    let mut used = 0usize;
    let rounds = 40;
    let mut log = Vec::new();
    for round in 0..rounds {
        for k in 0..=rng.below(5) {
            log.push(random_edit(&root, &mut rng, round * 10 + k));
        }
        let (watched, via_watcher, _) = watched_status(&root, &daemon, &NoNetwork);
        let full = full_rehash(&root, &NoNetwork);
        assert_eq!(
            watched, full,
            "round {round}: watcher status differs from a full rehash; edits: {log:?}"
        );
        if via_watcher {
            used += 1;
        }
        if round % 8 == 7 {
            // The product status (stat-cache walk) agrees too, and its rewrite of the stat
            // cache does not disturb the next watcher answer.
            let mut wc =
                WorkingCopy::open(&root, &NoNetwork, &OpenOptions::default()).expect("open");
            assert_eq!(wc.status().expect("status").changes, full, "round {round}");
        }
    }
    eprintln!("watcher status: {used}/{rounds} rounds answered from the watcher journal");
    assert!(
        used * 10 >= rounds * 8,
        "the watcher answered only {used} of {rounds} rounds"
    );
    d.stop(&srv.rt);
}

#[test]
fn watcher_status_rescans_only_changed_paths_on_a_large_tree() {
    let srv = start_server();
    srv.land_files(&seed_files(3_000), "seed");
    let up: Arc<dyn Upstream> = Arc::new(srv.http_upstream());
    let mut d = start_daemon(&srv.rt, up, TierConfig::new(TierKind::Daemon));
    let t = srv.transport();
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("wc");
    drop(clone_wc(&root, &t, Sparse::full()));
    let daemon = d.client(None);
    daemon
        .local("Watch", &json!({ "root": root }))
        .expect("watch");
    std::thread::sleep(Duration::from_millis(2_100));
    let mut wc = WorkingCopy::open(&root, &t, &OpenOptions::default()).expect("open");
    wc.status().expect("warm the stat cache");
    drop(wc);
    fs::write(root.join("src/m3/f0003.rs"), b"// edited\n").expect("edit");
    let t0 = Instant::now();
    let (watched, via, rescanned) = watched_status(&root, &daemon, &NoNetwork);
    let watched_ms = t0.elapsed().as_secs_f64() * 1000.0;
    assert!(via, "answered from the watcher");
    assert_eq!(rescanned, 1, "one path rescanned");
    assert_eq!(watched.len(), 1);
    let mut wc = WorkingCopy::open(&root, &NoNetwork, &OpenOptions::default()).expect("open");
    let t1 = Instant::now();
    let walked = wc.status().expect("status");
    let walk_ms = t1.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(walked.changes, watched);
    eprintln!(
        "status on a 3,003-file tree after one edit: watcher {watched_ms:.1} ms (incl. sync cookie), stat-cache walk {walk_ms:.1} ms"
    );
    drop(wc);
    // Unwatched roots fall back to a walk.
    assert_eq!(
        daemon
            .local("Unwatch", &json!({ "root": root }))
            .expect("unwatch")["removed"],
        true
    );
    let (_, via, _) = watched_status(&root, &daemon, &NoNetwork);
    assert!(!via, "a new watcher cannot vouch for the past: walk");
    d.stop(&srv.rt);
}
