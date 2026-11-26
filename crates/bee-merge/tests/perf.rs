//! Performance bound (contract §2.6): diffing two 10,000-line files that differ in 1% of lines
//! takes under 50 ms in a release build. Run `cargo test -p bee-merge --release --test perf`;
//! debug builds run the same work without the time bound.

use std::time::{Duration, Instant};

use bee_merge::{DiffStats, diff, merge_lines};

const LINES: usize = 10_000;

fn files() -> (Vec<u8>, Vec<u8>) {
    let mut old = String::new();
    let mut new = String::new();
    // Every 100th line changes in a rotating way (replace, delete, insert): 1% of lines.
    for i in 0..LINES {
        let line = format!("    let value_{i} = compute({i}, {});\n", i % 17);
        old.push_str(&line);
        match (i % 100, i / 100 % 3) {
            (50, 0) => new.push_str(&format!("    let value_{i} = changed({i});\n")),
            (50, 1) => {}
            (50, _) => {
                new.push_str(&line);
                new.push_str("    // inserted\n");
            }
            _ => new.push_str(&line),
        }
    }
    (old.into_bytes(), new.into_bytes())
}

fn bound(limit: Duration, elapsed: Duration, what: &str) {
    if cfg!(debug_assertions) {
        eprintln!("{what}: {elapsed:?} (debug build: bound not enforced)");
    } else {
        assert!(elapsed < limit, "{what} took {elapsed:?}, bound {limit:?}");
    }
}

#[test]
fn diff_of_two_10k_line_files_is_fast() {
    let (old, new) = files();
    // Best of three, to keep a loaded machine from failing the bound spuriously.
    let mut best = Duration::MAX;
    let mut hunks = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        hunks = diff(&old, &new);
        best = best.min(start.elapsed());
    }
    assert_eq!(
        DiffStats::from_hunks(&hunks),
        DiffStats {
            hunks: 100,
            added: 67,
            removed: 67
        }
    );
    bound(Duration::from_millis(50), best, "10k-line diff");
}

#[test]
fn merge_of_10k_line_files_is_fast() {
    let (base, ours) = files();
    let theirs = {
        let mut t = base.clone();
        t.extend_from_slice(b"// appended by theirs\n");
        t
    };
    let start = Instant::now();
    let m = merge_lines(&base, &ours, &theirs);
    let elapsed = start.elapsed();
    assert!(m.is_clean());
    bound(Duration::from_millis(100), elapsed, "10k-line merge");
}
