//! Property tests for the diff and the line merge.

use bee_merge::{ConflictHunk, MarkerStyle, Region, Source, diff, merge_lines, split_lines};
use proptest::prelude::*;

/// Texts over a small line alphabet so that repeated lines, collisions and ties are common.
fn text() -> impl Strategy<Value = Vec<u8>> {
    let line = prop_oneof![
        Just(&b"a\n"[..]),
        Just(&b"b\n"[..]),
        Just(&b"c\n"[..]),
        Just(&b"d\n"[..]),
        Just(&b"a\r\n"[..]),
    ];
    (prop::collection::vec(line, 0..10), prop::bool::ANY).prop_map(|(lines, cut)| {
        let mut t = lines.concat();
        if cut && t.ends_with(b"\n") {
            // Sometimes drop the final terminator.
            t.pop();
        }
        t
    })
}

/// A fixed seed keeps runs reproducible; `PROPTEST_CASES` raises the case count for a soak.
fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    ProptestConfig {
        cases,
        failure_persistence: None,
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x00d1_ff3e),
        ..ProptestConfig::default()
    }
}

/// Applies a diff to `old` and checks it reproduces `new`.
fn apply(old: &[u8], new: &[u8]) -> Vec<u8> {
    let a = split_lines(old);
    let b = split_lines(new);
    let mut out = Vec::new();
    let mut pos = 0;
    for h in diff(old, new) {
        a[pos..h.old_start]
            .iter()
            .for_each(|l| out.extend_from_slice(l));
        b[h.new_start..h.new_end()]
            .iter()
            .for_each(|l| out.extend_from_slice(l));
        pos = h.old_end();
    }
    a[pos..].iter().for_each(|l| out.extend_from_slice(l));
    out
}

fn lcs(a: &[&[u8]], b: &[&[u8]]) -> usize {
    let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    dp[0][0]
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn diff_applies_and_is_minimal(old in text(), new in text()) {
        prop_assert_eq!(apply(&old, &new), new.clone());
        let hunks = diff(&old, &new);
        let a = split_lines(&old);
        let b = split_lines(&new);
        let removed: usize = hunks.iter().map(|h| h.old_len).sum();
        prop_assert_eq!(a.len() - removed, lcs(&a, &b));
        for w in hunks.windows(2) {
            // At least one unchanged line between hunks.
            prop_assert!(w[0].old_end() < w[1].old_start);
            prop_assert!(w[0].new_end() < w[1].new_start);
        }
    }

    #[test]
    fn same_change_on_both_sides(base in text(), x in text()) {
        prop_assert_eq!(merge_lines(&base, &x, &x).merged(), Some(x.clone()));
    }

    #[test]
    fn one_side_unchanged_takes_the_other(base in text(), t in text()) {
        prop_assert_eq!(merge_lines(&base, &base, &t).merged(), Some(t.clone()));
        prop_assert_eq!(merge_lines(&base, &t, &base).merged(), Some(t.clone()));
    }

    #[test]
    fn clean_merges_are_idempotent(base in text(), o in text(), t in text()) {
        let first = merge_lines(&base, &o, &t);
        // Merging again gives the same result, and a clean result is a fixed point: merging it
        // with itself, or over either input as the base, returns it unchanged. (Re-merging it
        // against one input over the original base is not stable for diff3 in general; see
        // `strong_idempotence_does_not_hold` and contract §3.)
        prop_assert_eq!(&merge_lines(&base, &o, &t), &first);
        if let Some(m) = first.merged() {
            prop_assert_eq!(merge_lines(&base, &m, &m).merged(), Some(m.clone()));
            prop_assert_eq!(merge_lines(&o, &m, &o).merged(), Some(m.clone()));
            prop_assert_eq!(merge_lines(&t, &t, &m).merged(), Some(m.clone()));
            prop_assert_eq!(merge_lines(&m, &m, &m).merged(), Some(m.clone()));
        }
    }

    #[test]
    fn merge_is_symmetric_and_deterministic(base in text(), o in text(), t in text()) {
        let ot = merge_lines(&base, &o, &t);
        let to = merge_lines(&base, &t, &o);
        prop_assert_eq!(&ot, &merge_lines(&base, &o, &t));
        prop_assert_eq!(ot.merged(), to.merged());
        let swapped: Vec<ConflictHunk> = to
            .conflicts()
            .map(|h| ConflictHunk {
                ours_start: h.theirs_start,
                ours_len: h.theirs_len,
                theirs_start: h.ours_start,
                theirs_len: h.ours_len,
                ..h
            })
            .collect();
        prop_assert_eq!(ot.conflicts().collect::<Vec<_>>(), swapped);
    }

    #[test]
    fn conflicts_never_lose_lines(base in text(), o in text(), t in text()) {
        let m = merge_lines(&base, &o, &t);
        let (bl, ol, tl) = (m.base_lines(), m.ours_lines(), m.theirs_lines());
        // Regions walk every input in order: each side's covered ranges are increasing and
        // in bounds, and every conflict carries its full ranges.
        let mut last = (0usize, 0usize, 0usize);
        let mut out = Vec::new();
        for r in m.regions() {
            match *r {
                Region::Clean { source, start, end } => {
                    prop_assert!(start < end);
                    let lines = match source { Source::Base => bl, Source::Ours => ol, Source::Theirs => tl };
                    prop_assert!(end <= lines.len());
                    lines[start..end].iter().for_each(|l| out.extend_from_slice(l));
                }
                Region::Conflict(h) => {
                    prop_assert!(h.base_start >= last.0 && h.ours_start >= last.1 && h.theirs_start >= last.2);
                    prop_assert!(h.base_start + h.base_len <= bl.len());
                    prop_assert!(h.ours_start + h.ours_len <= ol.len());
                    prop_assert!(h.theirs_start + h.theirs_len <= tl.len());
                    last = (h.base_start + h.base_len, h.ours_start + h.ours_len, h.theirs_start + h.theirs_len);
                    let mut block = Vec::new();
                    block.extend_from_slice(b"<<<<<<< ours\n");
                    ol[h.ours_start..last.1].iter().for_each(|l| block.extend_from_slice(l));
                    if !block.ends_with(b"\n") { block.push(b'\n'); }
                    block.extend_from_slice(b"||||||| base\n");
                    bl[h.base_start..last.0].iter().for_each(|l| block.extend_from_slice(l));
                    if !block.ends_with(b"\n") { block.push(b'\n'); }
                    block.extend_from_slice(b"=======\n");
                    tl[h.theirs_start..last.2].iter().for_each(|l| block.extend_from_slice(l));
                    if !block.ends_with(b"\n") { block.push(b'\n'); }
                    block.extend_from_slice(b">>>>>>> theirs\n");
                    if out.last().is_some_and(|&c| c != b'\n') { out.push(b'\n'); }
                    out.extend_from_slice(&block);
                }
            }
        }
        // The rendering is exactly the regions with each conflict's full sections.
        prop_assert_eq!(m.render(MarkerStyle::Diff3), out);
        // A conflict needs both sides to have changed it differently.
        for h in m.conflicts() {
            prop_assert!(ol[h.ours_start..h.ours_start + h.ours_len] != tl[h.theirs_start..h.theirs_start + h.theirs_len]);
        }
    }
}

/// The counterexample in contract §3: a clean result re-merged with ours over the original base
/// conflicts. Pinned so the contract's statement stays true.
#[test]
fn strong_idempotence_does_not_hold() {
    let base = b"a\nc\nd\na\na\n";
    let ours = b"a\nd\n";
    let theirs = b"c\na\nc\nd\n";
    let m = merge_lines(base, ours, theirs)
        .merged()
        .expect("first merge is clean");
    assert_eq!(m, b"c\na\nd\n");
    assert!(!merge_lines(base, ours, &m).is_clean());
}
