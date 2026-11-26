//! Differential test against `git merge-file` (contract §10). Skipped when `git` is not on PATH.

use crate::common;
use std::path::{Path, PathBuf};
use std::process::Command;

use bee_merge::{MarkerStyle, merge_lines};

/// Contract §12 examples known to differ from git. Must match the list in contract §10.
const KNOWN_DIFFERENCES: &[&str] = &[];

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bee-merge-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `git merge-file -p --diff3` and returns (output, exit code).
fn git_merge(dir: &Scratch, base: &[u8], ours: &[u8], theirs: &[u8]) -> (Vec<u8>, i32) {
    let write = |name: &str, bytes: &[u8]| {
        let p = dir.path(name);
        std::fs::write(&p, bytes).expect("write input");
        p
    };
    let (b, o, t) = (
        write("base", base),
        write("ours", ours),
        write("theirs", theirs),
    );
    let out = Command::new("git")
        .args([
            "merge-file",
            "-p",
            "--diff3",
            "-L",
            "ours",
            "-L",
            "base",
            "-L",
            "theirs",
        ])
        .args([&o, &b, &t].map(|p: &PathBuf| p.as_path() as &Path))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    (out.stdout, out.status.code().expect("exit code"))
}

/// Our result in git's terms: the diff3 rendering and the conflict count.
fn ours_merge(base: &[u8], ours: &[u8], theirs: &[u8]) -> (Vec<u8>, i32) {
    let m = merge_lines(base, ours, theirs);
    (m.render(MarkerStyle::Diff3), m.conflicts().count() as i32)
}

#[test]
fn contract_examples_agree_with_git() {
    if !git_available() {
        eprintln!("git not on PATH: skipping differential test");
        return;
    }
    let dir = Scratch::new("examples");
    let mut differing = Vec::new();
    for e in common::examples() {
        if !e.id().starts_with('M') || !e.has("base") {
            continue;
        }
        let (b, o, t) = (e.text("base"), e.text("ours"), e.text("theirs"));
        if git_merge(&dir, &b, &o, &t) != ours_merge(&b, &o, &t) {
            differing.push(e.id().to_string());
        }
    }
    assert_eq!(differing, KNOWN_DIFFERENCES, "examples differing from git");
}

/// A small deterministic generator (xorshift64*), so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Edits a base of unique lines. Replacement lines are unique per side, or shared by
/// position (`n{i}`) so both sides sometimes make the identical change.
fn edit(rng: &mut Rng, base: &[String], side: &str) -> Vec<u8> {
    let mut out = String::new();
    let mut fresh = 0;
    for (i, line) in base.iter().enumerate() {
        match rng.below(10) {
            0 => {}                                // delete
            1 => out.push_str(&format!("n{i}\n")), // shared replacement
            2 => {
                fresh += 1;
                out.push_str(&format!("{side}{i}x{fresh}\n")); // own replacement
            }
            3 => {
                fresh += 1;
                out.push_str(&format!("{side}{i}i{fresh}\n{line}")); // insert before
            }
            _ => out.push_str(line),
        }
    }
    if rng.below(4) == 0 {
        out.push_str(&format!("{side}end\n"));
    }
    out.into_bytes()
}

#[test]
fn generated_unique_line_merges_agree_with_git() {
    if !git_available() {
        eprintln!("git not on PATH: skipping differential test");
        return;
    }
    let dir = Scratch::new("generated");
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut clean, mut conflicted) = (0, 0);
    for case in 0..300 {
        let n = 1 + rng.below(25) as usize;
        let base: Vec<String> = (0..n).map(|i| format!("line{i}\n")).collect();
        let ours = edit(&mut rng, &base, "o");
        let theirs = edit(&mut rng, &base, "t");
        let base = base.concat().into_bytes();
        let expected = git_merge(&dir, &base, &ours, &theirs);
        let actual = ours_merge(&base, &ours, &theirs);
        assert_eq!(
            String::from_utf8_lossy(&actual.0),
            String::from_utf8_lossy(&expected.0),
            "case {case}: output differs\nbase={:?}\nours={:?}\ntheirs={:?}",
            String::from_utf8_lossy(&base),
            String::from_utf8_lossy(&ours),
            String::from_utf8_lossy(&theirs)
        );
        assert_eq!(actual.1, expected.1, "case {case}: conflict count differs");
        if actual.1 == 0 {
            clean += 1
        } else {
            conflicted += 1
        }
    }
    // The generator must exercise both outcomes.
    assert!(
        clean > 20 && conflicted > 20,
        "clean {clean}, conflicted {conflicted}"
    );
}
