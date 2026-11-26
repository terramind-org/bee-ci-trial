//! The git gateway's test suite proper (PRODUCT §4.11, §4.25): a client matrix over every git
//! installed on the machine and every protocol version, object-set completeness (a clone holds
//! exactly the source's objects, byte for byte), and golden vectors for the projection of new
//! bee history to git ids.

#![allow(missing_docs, clippy::result_large_err)]

use crate::common;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::*;

/// Every distinct git on this machine (by version), the one on PATH first.
fn gits() -> Vec<(PathBuf, String)> {
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("git")];
    for p in [
        "/usr/bin/git",
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
        "/Applications/Xcode.app/Contents/Developer/usr/bin/git",
        "/Library/Developer/CommandLineTools/usr/bin/git",
    ] {
        if Path::new(p).exists() {
            candidates.push(PathBuf::from(p));
        }
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for c in candidates {
        let Ok(o) = Command::new(&c).arg("--version").output() else {
            continue;
        };
        let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if o.status.success() && seen.insert(v.clone()) {
            out.push((c, v));
        }
    }
    out
}

fn run(env: &Env, git: &Path, dir: &Path, args: &[&str]) -> String {
    let out = Command::new(git)
        .args(args)
        .current_dir(dir)
        .env("HOME", env.home())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", env.home().join(".gitconfig"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_DATE", "2026-10-18T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-10-18T12:00:00Z")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "{} {args:?}:\n{}\n{}",
        git.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Every object id reachable from every ref.
fn objects(env: &Env, git: &Path, dir: &Path) -> BTreeSet<String> {
    run(env, git, dir, &["rev-list", "--objects", "--all"])
        .lines()
        .map(|l| l.split(' ').next().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_installed_git_clones_fetches_and_pushes_over_every_protocol_version() {
    let env = start_env().await;
    let src = imported(&env, "r").await;
    // Pushes go to a second import of the same history, so "r" keeps exactly its objects.
    env.import(&src, "w").await;
    let gits = gits();
    assert!(!gits.is_empty());
    eprintln!(
        "git client matrix: {:?}",
        gits.iter().map(|g| &g.1).collect::<Vec<_>>()
    );
    let want = objects(&env, Path::new("git"), &src);
    let mut n = 0u8;
    for (git, version) in &gits {
        for proto in ["0", "1", "2"] {
            n += 1;
            let name = format!("c{n}");
            let dest = env.path(&name);
            let p = format!("protocol.version={proto}");
            run(
                &env,
                git,
                env.dir.path(),
                &[
                    "-c",
                    &p,
                    "clone",
                    "-q",
                    &env.url("r"),
                    dest.to_str().unwrap(),
                ],
            );
            // Object-set completeness: exactly the source's objects, verified by fsck.
            assert_eq!(
                objects(&env, git, &dest),
                want,
                "{version} v{proto}: the clone holds exactly the source's objects"
            );
            run(&env, git, &dest, &["fsck", "--strict", "--no-dangling"]);
            // A push of a new branch, then a fetch of it from another clone.
            run(
                &env,
                git,
                &dest,
                &["checkout", "-q", "-b", &format!("m/{name}")],
            );
            write(&dest.join(format!("{name}.txt")), name.as_bytes());
            run(&env, git, &dest, &["add", "-A"]);
            run(
                &env,
                git,
                &dest,
                &["commit", "-q", "-m", &format!("matrix {name}")],
            );
            let head = run(&env, git, &dest, &["rev-parse", "HEAD"]);
            run(
                &env,
                git,
                &dest,
                &[
                    "-c",
                    &p,
                    "push",
                    "-q",
                    &env.url("w"),
                    &format!("HEAD:refs/heads/m/{name}"),
                ],
            );
            run(
                &env,
                git,
                &src,
                &[
                    "-c",
                    &p,
                    "fetch",
                    "-q",
                    &env.url("w"),
                    &format!("refs/heads/m/{name}"),
                ],
            );
            assert_eq!(
                run(&env, git, &src, &["rev-parse", "FETCH_HEAD"]),
                head,
                "{version} v{proto}: the pushed commit keeps its id"
            );
        }
    }
}

/// Golden vectors: the git ids the projection gives new bee history (SPEC §17.4 new-history
/// form). Frozen: if one changes, every clone of every repo changes its ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_history_projects_to_frozen_git_ids() {
    let env = start_env().await;
    env.api_commit_and_land(
        "playground",
        &[("golden.txt", b"golden\n"), ("dir/inner.md", b"# inner\n")],
        "golden vector one",
        1,
    )
    .await;
    env.api_commit_and_land(
        "playground",
        &[("golden.txt", b"golden two\n")],
        "golden vector two",
        2,
    )
    .await;
    let dest = env.path("golden");
    let git = Path::new("git");
    run(
        &env,
        git,
        env.dir.path(),
        &[
            "clone",
            "-q",
            &env.url("playground"),
            dest.to_str().unwrap(),
        ],
    );
    let ids: Vec<String> = [
        "HEAD",
        "HEAD^{tree}",
        "HEAD:golden.txt",
        "HEAD~1",
        "HEAD~1^{tree}",
        "HEAD~1:dir",
        "HEAD~1:dir/inner.md",
    ]
    .iter()
    .map(|r| run(&env, git, &dest, &["rev-parse", r]))
    .collect();
    assert_eq!(ids, GOLDEN, "the projection of new history changed");
    // The commit bytes themselves: the new-history form with its Change-Id trailer.
    let raw = run(&env, git, &dest, &["cat-file", "commit", "HEAD"]);
    assert_eq!(raw, GOLDEN_HEAD_COMMIT);
}

const GOLDEN: [&str; 7] = [
    "a35a8ea6c829d8db372dec7048bd4be173d917c7",
    "255cd8581841984576a8ff7b81d030fdb5d8e7d1",
    "a8c221376e4eb420cdd8697ad40194b1f49eb77d",
    "722260bd6ac6dce16a276f9bc55a94f3df71f8b4",
    "bcbd75f97e9db63adf650b1dd745bbbbe6ef830e",
    "27d56914feae47945ef327869122eb3f039e0c5b",
    "8e848fa46e89ca4753034dfb870e985cfc7ee3e8",
];

const GOLDEN_HEAD_COMMIT: &str = "tree 255cd8581841984576a8ff7b81d030fdb5d8e7d1\n\
parent 722260bd6ac6dce16a276f9bc55a94f3df71f8b4\n\
author API Author <api@example.com> 1792328400 +0000\n\
committer API Author <api@example.com> 1792328400 +0000\n\
\n\
golden vector two\n\
\n\
Change-Id: k_42020202020202020202020202020202";
