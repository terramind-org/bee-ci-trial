//! The CLI through the node daemon: the real `bee` and `bee-daemon` binaries against an
//! in-process `bee-server` in dev mode. Every `--json` document is validated against its
//! published schema.

use crate::support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use bee_server::config::Config;
use bee_server::server::{RunningServer, ServerOptions, start};
use serde_json::Value;
use support::{Bee, Run, validate, validate_error};

/// The `bee-daemon` executable, built on demand when only `bee-cli` was built.
fn daemon_binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let bee = PathBuf::from(env!("CARGO_BIN_EXE_bee"));
        let bin = bee.with_file_name("bee-daemon");
        if !bin.is_file() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = Command::new(cargo)
                .args(["build", "-p", "bee-daemon", "--bin", "bee-daemon"])
                .current_dir(support::repo_root())
                .status()
                .expect("run cargo build");
            assert!(status.success(), "cargo build -p bee-daemon failed");
        }
        assert!(bin.is_file(), "{} was not built", bin.display());
        bin
    })
}

struct Env {
    srv: Option<RunningServer>,
    runtime: tokio::runtime::Runtime,
    token: String,
    url: String,
    tmp: tempfile::TempDir,
    /// Short directory for the daemon socket and cache (unix socket paths are length-limited).
    sock_dir: tempfile::TempDir,
    started_daemon: std::cell::Cell<bool>,
}

impl Env {
    fn start() -> Env {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("runtime");
        let tmp = tempfile::tempdir().expect("tmp");
        let mut cfg = Config::dev(tmp.path().join("server"), Some("127.0.0.1:0".into()));
        cfg.redaction.purge_interval_secs = 3600;
        let mut opts = ServerOptions::new(cfg);
        opts.dev = true;
        let srv = runtime.block_on(start(opts)).expect("server starts");
        let dev = srv.dev.as_ref().expect("dev bootstrap");
        Env {
            token: dev.token.clone(),
            url: srv.url.clone(),
            srv: Some(srv),
            runtime,
            tmp,
            sock_dir: tempfile::Builder::new()
                .prefix("swc")
                .tempdir_in("/tmp")
                .expect("sock dir"),
            started_daemon: std::cell::Cell::new(false),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    fn socket(&self) -> PathBuf {
        self.sock_dir.path().join("d.sock")
    }

    /// `bee <args> --json` in `dir`, using the daemon socket of this environment.
    fn run(&self, dir: &Path, args: &[&str]) -> Run {
        self.run_with(dir, args, &self.socket(), &[])
    }

    fn run_with(&self, dir: &Path, args: &[&str], socket: &Path, extra: &[(&str, &str)]) -> Run {
        let mut all: Vec<&str> = args.to_vec();
        all.push("--json");
        let mut cmd = Bee::new(&all)
            .current_dir(dir)
            .env("BEE_ENDPOINT", &self.url)
            .env("BEE_TOKEN", &self.token)
            .env("BEE_AUTHOR", "tester")
            .env("BEE_DATE", "1792281600")
            .env("BEE_DAEMON_BIN", &daemon_binary().display().to_string())
            .env("BEE_DAEMON_SOCKET", &socket.display().to_string());
        for (k, v) in extra {
            cmd = cmd.env(k, v);
        }
        cmd.run().0
    }

    /// Runs, expects exit 0, validates the output against the command's schema, returns `data`.
    fn ok(&self, dir: &Path, args: &[&str]) -> Value {
        let run = self.run(dir, args);
        self.checked(&run, args)
    }

    fn checked(&self, run: &Run, args: &[&str]) -> Value {
        assert_eq!(run.code, 0, "bee {args:?}: {run:?}");
        let doc = run.json();
        validate(&schema_file(args), &doc);
        doc["data"].clone()
    }

    fn fails(&self, dir: &Path, args: &[&str], socket: &Path, code: i32) -> Value {
        let run = self.run_with(dir, args, socket, &[]);
        assert_eq!(run.code, code, "bee {args:?}: {run:?}");
        validate_error(&run.json())
    }

    /// Starts the daemon with `bee daemon start` (a real, detached `bee-daemon serve`).
    fn start_daemon(&self) -> Value {
        let cache = self.sock_dir.path().join("cache");
        let args = [
            "daemon",
            "start",
            "--cache",
            cache.to_str().expect("utf8"),
            "--no-fsync",
        ];
        let data = self.ok(self.tmp.path(), &args);
        self.started_daemon.set(true);
        data
    }

    fn stats(&self) -> Value {
        self.ok(self.tmp.path(), &["daemon", "stats"])
    }

    /// Lands `files` on `main` through a direct (daemon-free) clone.
    fn seed(&self, files: &[(&str, String)]) {
        let wc = self.path("seed");
        let run = self.run_with(
            self.tmp.path(),
            &["clone", "dev/playground", "seed"],
            &self.socket(),
            &[("BEE_NO_DAEMON", "1")],
        );
        assert_eq!(run.code, 0, "{run:?}");
        for (p, t) in files {
            write(&wc, p, t);
        }
        for args in [vec!["commit", "-m", "seed"], vec!["land", "--no-daemon"]] {
            let run = self.run_with(&wc, &args, &self.socket(), &[("BEE_NO_DAEMON", "1")]);
            assert_eq!(run.code, 0, "{args:?}: {run:?}");
        }
    }

    fn clone_to(&self, name: &str, extra: &[&str]) -> (PathBuf, Value) {
        let mut args = vec!["clone", "dev/playground", name];
        args.extend(extra);
        let data = self.ok(self.tmp.path(), &args);
        assert_eq!(data["op"], "clone");
        (self.path(name), data)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if self.started_daemon.get() {
            let _ = self.run(self.tmp.path(), &["daemon", "stop"]);
        }
        if let Some(srv) = self.srv.take() {
            self.runtime.block_on(srv.shutdown());
        }
    }
}

/// `schemas/v1/cli/<command words>.schema.json` for an argument list.
fn schema_file(args: &[&str]) -> String {
    let words: Vec<&str> = if matches!(args[0], "daemon" | "sparse") {
        args[..2].to_vec()
    } else {
        args[..1].to_vec()
    };
    format!("schemas/v1/cli/{}.schema.json", words.join("."))
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(p, text).expect("write");
}

fn files(n: usize) -> Vec<(&'static str, String)> {
    let names: Vec<(&'static str, String)> = (0..n)
        .map(|i| {
            let dir = ["alpha", "beta/inner", "gamma", "delta/a/b"][i % 4];
            let path: &'static str = Box::leak(format!("{dir}/f{i:03}.txt").into_boxed_str());
            (path, format!("file {i}\n{}\n", "x".repeat(i % 7)))
        })
        .collect();
    names
}

/// Files of a working tree (excluding `.bee`), path to bytes.
fn tree_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).expect("read").flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if dir == root && e.file_name() == ".bee" {
                continue;
            }
            let ft = e.file_type().expect("type");
            if ft.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p
                    .strip_prefix(root)
                    .expect("under")
                    .to_string_lossy()
                    .into_owned();
                let bytes = if ft.is_symlink() {
                    std::fs::read_link(&p)
                        .expect("link")
                        .to_string_lossy()
                        .as_bytes()
                        .to_vec()
                } else {
                    std::fs::read(&p).expect("read")
                };
                out.insert(rel, bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// BLAKE3 of the sorted (path, bytes) listing: the hash of a working tree's content.
fn tree_hash(root: &Path) -> String {
    let mut input = Vec::new();
    for (p, b) in tree_files(root) {
        input.extend_from_slice(&(p.len() as u64).to_le_bytes());
        input.extend_from_slice(p.as_bytes());
        input.extend_from_slice(&(b.len() as u64).to_le_bytes());
        input.extend_from_slice(&b);
    }
    bee_object::digest(&input)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A status document without the fields that name the working copy.
fn comparable(status: &Value) -> Value {
    let mut s = status.clone();
    if let Some(m) = s.as_object_mut() {
        m.remove("root");
        // ChangeIds come from a per-working-copy seed.
        m.remove("stack");
    }
    s
}

fn upstream_objects(stats: &Value) -> u64 {
    stats["upstreamObjects"].as_u64().expect("upstreamObjects")
}

#[test]
fn clone_via_daemon_equals_clone_direct() {
    let env = Env::start();
    env.seed(&files(40));
    env.start_daemon();
    assert_eq!(upstream_objects(&env.stats()), 0, "the daemon starts cold");

    env.ok(
        env.tmp.path(),
        &["clone", "dev/playground", "direct", "--no-daemon"],
    );
    assert_eq!(upstream_objects(&env.stats()), 0, "--no-daemon bypasses it");
    let (via, via_report) = env.clone_to("via", &[]);
    assert!(
        upstream_objects(&env.stats()) >= 40,
        "the clone went through the daemon: {}",
        env.stats()
    );

    assert_eq!(tree_files(&via).len(), 40);
    assert_eq!(tree_files(&via), tree_files(&env.path("direct")));
    assert_eq!(tree_hash(&via), tree_hash(&env.path("direct")));
    assert_eq!(via_report["written"], 40);
    let a = env.ok(&via, &["status"]);
    let b = env.ok(&env.path("direct"), &["status", "--no-daemon"]);
    assert_eq!(a["clean"], true);
    assert_eq!(comparable(&a), comparable(&b));
    assert_eq!(a["head"], b["head"]);
}

#[test]
fn a_second_clone_of_the_same_repo_fetches_zero_objects_upstream() {
    let env = Env::start();
    env.seed(&files(30));
    env.start_daemon();
    env.clone_to("one", &[]);
    let first = env.stats();
    assert!(upstream_objects(&first) >= 30, "{first}");
    env.clone_to("two", &[]);
    env.clone_to("three", &["--sparse", "alpha"]);
    let after = env.stats();
    assert_eq!(
        upstream_objects(&after),
        upstream_objects(&first),
        "no object came from the server again: {after}"
    );
    assert!(after["hits"].as_u64().unwrap_or(0) > first["hits"].as_u64().unwrap_or(0));
    assert_eq!(tree_hash(&env.path("one")), tree_hash(&env.path("two")));
}

/// A tiny deterministic generator for edit scripts.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % bound.max(1)
    }
}

fn random_edit(rng: &mut Lcg, root: &Path, round: usize) {
    let tracked: Vec<String> = tree_files(root).into_keys().collect();
    let pick = |rng: &mut Lcg| tracked[rng.next(tracked.len())].clone();
    match rng.next(9) {
        0 => {
            let p = pick(rng);
            write(root, &p, &format!("rewritten in round {round}\n"));
        }
        1 => {
            // Same size, same instant: only a rehash can tell.
            let p = pick(rng);
            let old = std::fs::read(root.join(&p)).expect("read");
            let new: Vec<u8> = old.iter().map(|b| b.wrapping_add(1) | 0x20).collect();
            std::fs::write(root.join(&p), new).expect("write");
        }
        2 => write(root, &format!("fresh/r{round}/new.txt"), "new file\n"),
        3 => {
            let p = pick(rng);
            let _ = std::fs::remove_file(root.join(p));
        }
        4 => {
            let _ = std::fs::rename(root.join("gamma"), root.join(format!("moved{round}")));
        }
        5 => {
            use std::os::unix::fs::PermissionsExt as _;
            let p = pick(rng);
            if let Ok(meta) = std::fs::metadata(root.join(&p)) {
                let mode = meta.permissions().mode() ^ 0o111;
                let _ =
                    std::fs::set_permissions(root.join(&p), std::fs::Permissions::from_mode(mode));
            }
        }
        6 => {
            let ignore = if round.is_multiple_of(2) {
                "*.txt\n"
            } else {
                ""
            };
            write(root, "alpha/.gitignore", ignore);
        }
        7 => {
            let _ =
                std::os::unix::fs::symlink(format!("f{round}"), root.join(format!("link{round}")));
        }
        _ => write(root, "beta/inner/extra.log", &format!("log {round}\n")),
    }
}

#[test]
fn status_via_the_watcher_equals_a_full_walk_under_random_edits() {
    let env = Env::start();
    env.seed(&files(24));
    env.start_daemon();
    let (wc, _) = env.clone_to("wc", &[]);
    let traced = [("BEE_DAEMON_TRACE", "1")];
    let status = |dir: &Path, extra: &[&str]| -> (Value, String) {
        let mut args = vec!["status"];
        args.extend(extra);
        let run = env.run_with(dir, &args, &env.socket(), &traced);
        let data = env.checked(&run, &["status"]);
        (data, run.stderr)
    };
    // The first status after a clone cannot be vouched for: the watcher started after it.
    let (first, why) = status(&wc, &[]);
    assert_eq!(first["clean"], true);
    assert!(why.contains("status walks the tree"), "{why}");
    // On a loaded machine the watcher can take a while to confirm its first event round
    // trip; count rounds only once it vouches, so the ratio measures steady state.
    let ready_by = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let (_, trace) = status(&wc, &[]);
        if trace.contains("status from the watcher") {
            break;
        }
        assert!(
            std::time::Instant::now() < ready_by,
            "the watcher never vouched within 60 s: {trace}"
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let mut rng = Lcg(0x5eed);
    let (mut from_watcher, rounds) = (0, 30);
    let mut walked_because: Vec<String> = Vec::new();
    for round in 0..rounds {
        for _ in 0..=rng.next(3) {
            random_edit(&mut rng, &wc, round);
        }
        let (watched, trace) = status(&wc, &[]);
        let (walked, _) = status(&wc, &["--no-daemon"]);
        assert_eq!(
            comparable(&watched),
            comparable(&walked),
            "round {round}: watcher status differs from the walk ({trace})"
        );
        if trace.contains("status from the watcher") {
            from_watcher += 1;
        } else {
            walked_because.push(format!("round {round}: {}", trace.trim()));
        }
        if round % 10 == 9 {
            let run = env.run_with(
                &wc,
                &["commit", "-m", &format!("round {round}")],
                &env.socket(),
                &traced,
            );
            assert_eq!(run.code, 0, "{run:?}");
        }
    }
    assert!(
        from_watcher >= rounds * 8 / 10,
        "only {from_watcher}/{rounds} rounds were answered from the watcher: {walked_because:#?}"
    );
}

#[test]
fn a_virtual_checkout_edits_commits_and_lands_one_path() {
    let env = Env::start();
    env.seed(&files(60));
    env.start_daemon();
    let touched = "beta/inner/f001.txt";
    let data = env.ok(
        env.tmp.path(),
        &[
            "checkout",
            "--virtual",
            "dev/playground",
            "virt",
            "--touch",
            touched,
        ],
    );
    assert_eq!(data["op"], "clone");
    let wc = env.path("virt");
    // Only the touched path is on disk; the rest is served by the daemon.
    assert_eq!(
        tree_files(&wc).keys().cloned().collect::<Vec<_>>(),
        vec![touched.to_owned()]
    );
    assert_eq!(env.ok(&wc, &["status"])["clean"], true);
    let other = "gamma/f002.txt";
    assert!(!wc.join(other).exists());
    let cat = env.ok(&wc, &["cat", other, "--rev", "main"]);
    assert_eq!(cat["content"], files(60)[2].1);
    // Touch a second path: the sparse profile grows and the file is written.
    env.ok(&wc, &["sparse", "add", other]);
    assert_eq!(tree_files(&wc).len(), 2);
    write(&wc, touched, "edited in a virtual checkout\n");
    let status = env.ok(&wc, &["status"]);
    assert_eq!(
        status["changes"].as_array().expect("changes").len(),
        1,
        "untouched paths are not deletions: {status}"
    );
    env.ok(&wc, &["commit", "-m", "edit one path"]);
    let land = env.ok(&wc, &["land"]);
    assert_eq!(land["land"]["landed"].as_array().expect("landed").len(), 1);
    // The commit was a one-path delta: a fresh full clone has all 60 files, one edited.
    let (full, _) = env.clone_to("full", &["--no-daemon"]);
    let tree = tree_files(&full);
    assert_eq!(tree.len(), 60);
    assert_eq!(tree[touched], b"edited in a virtual checkout\n");
    assert_eq!(tree[other], files(60)[2].1.as_bytes());
}

#[test]
fn snapshot_produces_a_working_copy() {
    let env = Env::start();
    env.seed(&files(50));
    env.start_daemon();
    let data = env.ok(
        env.tmp.path(),
        &["snapshot", "main", "snap", "--repo", "dev/playground"],
    );
    let wc = env.path("snap");
    assert_eq!(data["files"], 50);
    assert_eq!(data["matchesHead"], true);
    assert!(data["fetched"].as_u64().unwrap_or(0) >= 50);
    assert_eq!(tree_files(&wc).len(), 50);
    let status = env.ok(&wc, &["status"]);
    assert_eq!(status["clean"], true, "{status}");
    // A second snapshot of the same revision downloads nothing.
    let again = env.ok(
        env.tmp.path(),
        &[
            "snapshot",
            "main",
            "snap2",
            "--repo",
            "dev/playground",
            "--sparse",
            "alpha",
        ],
    );
    assert_eq!(again["fetched"], 0);
    assert!(again["files"].as_u64().unwrap_or(0) > 0);
    assert_eq!(env.ok(&env.path("snap2"), &["status"])["clean"], true);
    // It is a real working copy: edit, commit, land.
    write(&wc, "alpha/f000.txt", "edited after a snapshot\n");
    env.ok(&wc, &["commit", "-m", "edit after snapshot"]);
    env.ok(&wc, &["land"]);
    let (check, _) = env.clone_to("check", &["--no-daemon"]);
    assert_eq!(
        tree_files(&check)["alpha/f000.txt"],
        b"edited after a snapshot\n"
    );
    assert_eq!(tree_files(&check).len(), 50);
}

#[test]
fn no_daemon_a_missing_socket_and_a_dead_socket_fall_back_cleanly() {
    let env = Env::start();
    env.seed(&files(20));
    env.start_daemon();
    let before = upstream_objects(&env.stats());
    // --no-daemon and BEE_NO_DAEMON: direct, the daemon is not touched.
    env.ok(
        env.tmp.path(),
        &["clone", "dev/playground", "direct", "--no-daemon"],
    );
    let run = env.run_with(
        env.tmp.path(),
        &["clone", "dev/playground", "envoff"],
        &env.socket(),
        &[("BEE_NO_DAEMON", "1")],
    );
    env.checked(&run, &["clone"]);
    assert_eq!(upstream_objects(&env.stats()), before);
    let reference = tree_hash(&env.path("direct"));
    assert_eq!(tree_hash(&env.path("envoff")), reference);

    // A socket path where nothing exists.
    let missing = env.sock_dir.path().join("missing.sock");
    let run = env.run_with(
        env.tmp.path(),
        &["clone", "dev/playground", "missing"],
        &missing,
        &[],
    );
    env.checked(&run, &["clone"]);
    assert_eq!(tree_hash(&env.path("missing")), reference);

    // A socket file nobody listens on (a daemon that crashed).
    let dead = env.sock_dir.path().join("dead.sock");
    drop(std::os::unix::net::UnixListener::bind(&dead).expect("bind"));
    assert!(dead.exists());
    let run = env.run_with(
        env.tmp.path(),
        &["clone", "dev/playground", "dead"],
        &dead,
        &[("BEE_DAEMON_TRACE", "1")],
    );
    env.checked(&run, &["clone"]);
    assert!(run.stderr.contains("going direct"), "{}", run.stderr);
    assert_eq!(tree_hash(&env.path("dead")), reference);
    // status and commit work with the dead socket too.
    write(&env.path("dead"), "alpha/f000.txt", "changed\n");
    let run = env.run_with(&env.path("dead"), &["status"], &dead, &[]);
    let status = env.checked(&run, &["status"]);
    assert_eq!(status["changes"].as_array().expect("changes").len(), 1);

    // A file that is not a socket.
    let regular = env.sock_dir.path().join("regular.sock");
    std::fs::write(&regular, b"not a socket").expect("write");
    let run = env.run_with(
        env.tmp.path(),
        &["clone", "dev/playground", "regular"],
        &regular,
        &[],
    );
    env.checked(&run, &["clone"]);

    // Commands that need the daemon say so, with a documented reason.
    let err = env.fails(
        env.tmp.path(),
        &["snapshot", "main", "nosnap", "--repo", "dev/playground"],
        &dead,
        1,
    );
    assert_eq!(err["reason"], "UNAVAILABLE");
    assert!(!env.path("nosnap").exists());
    let err = env.fails(env.tmp.path(), &["daemon", "stats"], &dead, 1);
    assert_eq!(err["reason"], "UNAVAILABLE");

    // A daemon that fronts another server is not used.
    std::fs::write(
        env.sock_dir.path().join("d.sock.upstream"),
        "http://127.0.0.1:1",
    )
    .expect("write");
    env.ok(env.tmp.path(), &["clone", "dev/playground", "foreign"]);
    assert_eq!(upstream_objects(&env.stats()), before);
    // The original sidecar is restored for the cleanup stop.
    std::fs::write(env.sock_dir.path().join("d.sock.upstream"), &env.url).expect("write");
}

#[test]
fn daemon_commands_manage_the_cache() {
    let env = Env::start();
    env.seed(&files(40));
    let socket = env.socket();
    let idle = env.ok(env.tmp.path(), &["daemon", "status"]);
    assert_eq!(idle["running"], false);
    assert_eq!(idle["tier"], Value::Null);
    let stopped = env.ok(env.tmp.path(), &["daemon", "stop"]);
    assert_eq!(stopped["stopped"], false);

    let started = env.start_daemon();
    assert_eq!(started["started"], true);
    assert_eq!(started["upstream"], env.url.as_str());
    let pid = started["pid"].as_u64().expect("pid");
    let again = env.start_daemon();
    assert_eq!(again["started"], false);
    assert_eq!(again["pid"], pid);
    let status = env.ok(env.tmp.path(), &["daemon", "status"]);
    assert_eq!(status["running"], true);
    assert_eq!(status["tier"], "daemon");
    assert_eq!(status["pid"], pid);
    assert_eq!(status["socket"], socket.display().to_string());

    // Prefetch loads objects; the same prefetch again, and a clone, fetch nothing.
    let pre = env.ok(
        env.tmp.path(),
        &[
            "daemon",
            "prefetch",
            "--repo",
            "dev/playground",
            "alpha",
            "gamma",
        ],
    );
    assert!(pre["fetched"].as_u64().unwrap_or(0) >= 20, "{pre}");
    assert!(pre["files"].as_u64().unwrap_or(0) >= 20);
    let pre2 = env.ok(
        env.tmp.path(),
        &[
            "daemon",
            "prefetch",
            "--repo",
            "dev/playground",
            "alpha",
            "gamma",
        ],
    );
    assert_eq!(pre2["fetched"], 0);
    let objects = upstream_objects(&env.stats());
    env.clone_to("sparse", &["--sparse", "alpha", "--sparse", "gamma"]);
    assert_eq!(upstream_objects(&env.stats()), objects);

    // Pin and unpin.
    let pin = env.ok(
        env.tmp.path(),
        &["daemon", "pin", "hot", "--repo", "dev/playground", "alpha"],
    );
    assert_eq!(pin["name"], "hot");
    assert!(pin["objects"].as_u64().unwrap_or(0) >= 10);
    assert!(env.stats()["store"]["pinned"].as_u64().unwrap_or(0) >= 10);
    assert_eq!(
        env.ok(env.tmp.path(), &["daemon", "unpin", "hot"])["removed"],
        true
    );
    assert_eq!(
        env.ok(env.tmp.path(), &["daemon", "unpin", "hot"])["removed"],
        false
    );
    assert_eq!(env.stats()["store"]["pinned"], 0);

    // Stop.
    let stop = env.ok(env.tmp.path(), &["daemon", "stop"]);
    assert_eq!(stop["stopped"], true);
    assert_eq!(stop["pid"], pid);
    assert!(!socket.exists());
    let status = env.ok(env.tmp.path(), &["daemon", "status"]);
    assert_eq!(status["running"], false);
    // With the daemon gone, clones go direct and still work.
    env.clone_to("after", &[]);
    assert_eq!(tree_files(&env.path("after")).len(), 40);
}
