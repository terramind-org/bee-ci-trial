//! Rename and copy detection between a base tree and one side.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::diff::Interner;
use crate::lines::split_lines;
use crate::path::{Content, MergeOptions, Mode, Side, is_binary};
use crate::tree::Tree;

/// Rename detection settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RenameOptions {
    /// Detect renames at all. When false, every path merges in place.
    pub enabled: bool,
    /// Minimum line-set similarity, as a fraction `numerator / denominator` (default 1/2).
    pub threshold: (u64, u64),
    /// Similarity is skipped when `deleted × added` candidate pairs exceed this (default
    /// 1,000,000). Exact renames are always detected.
    pub max_pairs: usize,
}

impl Default for RenameOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: (1, 2),
            max_pairs: 1_000_000,
        }
    }
}

/// How a rename was matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Similarity {
    /// Identical content.
    Exact,
    /// Line-set similarity `shared / union` at or above the threshold.
    Lines {
        /// Distinct lines present in both files.
        shared: u64,
        /// Distinct lines present in either file.
        union: u64,
    },
}

/// A detected rename from a base path to a side path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rename {
    /// The path in the base.
    pub from: Vec<u8>,
    /// The path on the side.
    pub to: Vec<u8>,
    /// How it was matched.
    pub similarity: Similarity,
}

fn basename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Modes that may rename into each other: files and executables freely, symlinks and
/// gitlinks only to the same mode.
fn modes_compatible(a: Mode, b: Mode) -> bool {
    let class = |m| match m {
        Mode::File | Mode::Executable => 0,
        Mode::Symlink => 1,
        Mode::Gitlink => 2,
    };
    class(a) == class(b)
}

/// Content that may take part in detection: empty files never do.
fn detectable(side: &Side<'_>) -> bool {
    match side.content {
        Content::Bytes(b) => !b.is_empty(),
        Content::Redacted(_) => true,
    }
}

/// Content that may take part in similarity: readable, non-empty text files.
fn similarity_text<'a>(side: &Side<'a>, opts: &MergeOptions) -> Option<&'a [u8]> {
    if !matches!(side.mode, Mode::File | Mode::Executable) {
        return None;
    }
    side.content
        .bytes()
        .filter(|b| !b.is_empty() && !is_binary(b, opts.max_text_bytes))
}

/// The sorted distinct line tokens of a text.
fn line_set<'a>(interner: &mut Interner<'a>, text: &'a [u8]) -> Vec<u32> {
    let mut ids = interner.intern_all(&split_lines(text));
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn shared_count(a: &[u32], b: &[u32]) -> u64 {
    let (mut i, mut j, mut n) = (0, 0, 0u64);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                n += 1;
                i += 1;
                j += 1;
            }
        }
    }
    n
}

/// Line-set (Jaccard) similarity of two texts: `(shared, union)` over their distinct lines.
pub fn line_similarity(a: &[u8], b: &[u8]) -> (u64, u64) {
    let mut interner = Interner::new();
    let sa = line_set(&mut interner, a);
    let sb = line_set(&mut interner, b);
    let shared = shared_count(&sa, &sb);
    (shared, sa.len() as u64 + sb.len() as u64 - shared)
}

/// `shared / union >= num / den`, in exact integer arithmetic.
fn meets(shared: u64, union: u64, threshold: (u64, u64)) -> bool {
    union > 0 && (shared as u128) * (threshold.1 as u128) >= (union as u128) * (threshold.0 as u128)
}

struct Candidate<'p> {
    shared: u64,
    union: u64,
    same_basename: bool,
    from: &'p [u8],
    to: &'p [u8],
}

/// Detects renames from `base` to `side`.
///
/// Only paths deleted from the base (`from`) and added on the side (`to`) are candidates.
/// Empty files never match, and modes must be compatible (files and executables together;
/// symlinks and gitlinks only with themselves).
///
/// 1. Exact: for each deleted path in bytewise order, the added paths with identical content
///    (redacted content compares by identity) are candidates; the first one with the same
///    basename wins, else the first in bytewise order.
/// 2. Similarity, for readable non-binary files left over: every pair whose line-set
///    similarity meets the threshold is a candidate. Candidates are ranked by similarity
///    (higher first), then same basename first, then `from`, then `to` bytewise, and accepted
///    greedily when neither path is taken. Skipped when the pair count exceeds `max_pairs`.
///
/// The result is sorted by `from`.
pub fn detect_renames(
    base: &Tree<'_>,
    side: &Tree<'_>,
    opts: &RenameOptions,
    merge: &MergeOptions,
) -> Vec<Rename> {
    if !opts.enabled {
        return Vec::new();
    }
    let deleted: Vec<(&[u8], &Side<'_>)> = base
        .iter()
        .filter(|(p, s)| !side.contains_key(*p) && detectable(s))
        .map(|(p, s)| (p.as_slice(), s))
        .collect();
    let added: Vec<(&[u8], &Side<'_>)> = side
        .iter()
        .filter(|(p, s)| !base.contains_key(*p) && detectable(s))
        .map(|(p, s)| (p.as_slice(), s))
        .collect();

    let mut by_content: BTreeMap<Content<'_>, Vec<usize>> = BTreeMap::new();
    for (idx, (_, s)) in added.iter().enumerate() {
        by_content.entry(s.content).or_default().push(idx);
    }
    let mut taken_to: BTreeSet<usize> = BTreeSet::new();
    let mut out = Vec::new();
    let mut left_deleted = Vec::new();
    for &(from, fs) in &deleted {
        let candidates: Vec<usize> = by_content
            .get(&fs.content)
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|i| {
                        !taken_to.contains(i) && modes_compatible(fs.mode, added[*i].1.mode)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let pick = candidates
            .iter()
            .copied()
            .find(|&i| basename(added[i].0) == basename(from))
            .or_else(|| candidates.first().copied());
        match pick {
            Some(i) => {
                taken_to.insert(i);
                out.push(Rename {
                    from: from.to_vec(),
                    to: added[i].0.to_vec(),
                    similarity: Similarity::Exact,
                });
            }
            None => left_deleted.push((from, fs)),
        }
    }

    let froms: Vec<(&[u8], &[u8])> = left_deleted
        .iter()
        .filter_map(|(p, s)| similarity_text(s, merge).map(|t| (*p, t)))
        .collect();
    let tos: Vec<(&[u8], &[u8])> = added
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken_to.contains(i))
        .filter_map(|(_, (p, s))| similarity_text(s, merge).map(|t| (*p, t)))
        .collect();
    if !froms.is_empty()
        && !tos.is_empty()
        && froms.len().saturating_mul(tos.len()) <= opts.max_pairs
    {
        let mut interner = Interner::new();
        let from_sets: Vec<Vec<u32>> = froms
            .iter()
            .map(|(_, t)| line_set(&mut interner, t))
            .collect();
        let to_sets: Vec<Vec<u32>> = tos
            .iter()
            .map(|(_, t)| line_set(&mut interner, t))
            .collect();
        let mut candidates = Vec::new();
        for (fi, (from, _)) in froms.iter().enumerate() {
            for (ti, (to, _)) in tos.iter().enumerate() {
                let (a, b) = (&from_sets[fi], &to_sets[ti]);
                // Jaccard >= t needs min/max >= t; skip pairs that cannot qualify.
                let (lo, hi) = (a.len().min(b.len()) as u64, a.len().max(b.len()) as u64);
                if !meets(lo, hi, opts.threshold) {
                    continue;
                }
                let shared = shared_count(a, b);
                let union = a.len() as u64 + b.len() as u64 - shared;
                if meets(shared, union, opts.threshold) {
                    candidates.push(Candidate {
                        shared,
                        union,
                        same_basename: basename(from) == basename(to),
                        from,
                        to,
                    });
                }
            }
        }
        candidates.sort_by(|x, y| {
            let score = ((y.shared as u128) * (x.union as u128))
                .cmp(&((x.shared as u128) * (y.union as u128)));
            score
                .then_with(|| y.same_basename.cmp(&x.same_basename))
                .then_with(|| x.from.cmp(y.from))
                .then_with(|| x.to.cmp(y.to))
        });
        let mut used_from: BTreeSet<&[u8]> = BTreeSet::new();
        let mut used_to: BTreeSet<&[u8]> = BTreeSet::new();
        for c in candidates {
            if used_from.contains(c.from) || used_to.contains(c.to) {
                continue;
            }
            used_from.insert(c.from);
            used_to.insert(c.to);
            out.push(Rename {
                from: c.from.to_vec(),
                to: c.to.to_vec(),
                similarity: Similarity::Lines {
                    shared: c.shared,
                    union: c.union,
                },
            });
        }
    }
    out.sort_by(|a, b| a.from.cmp(&b.from));
    out
}

/// Finds the base path a side's added file was copied (or renamed) from: an identical base
/// file, else the most similar base text file meeting the threshold. Ties break by same
/// basename first, then bytewise path order. `None` when nothing qualifies.
pub(crate) fn copy_source<'t>(
    base: &'t Tree<'_>,
    path: &[u8],
    added: &Side<'_>,
    opts: &RenameOptions,
    merge: &MergeOptions,
) -> Option<&'t [u8]> {
    if !opts.enabled || !detectable(added) {
        return None;
    }
    let exact: Vec<&[u8]> = base
        .iter()
        .filter(|(_, s)| s.content == added.content && modes_compatible(s.mode, added.mode))
        .map(|(p, _)| p.as_slice())
        .collect();
    if !exact.is_empty() {
        return exact
            .iter()
            .copied()
            .find(|p| basename(p) == basename(path))
            .or_else(|| exact.first().copied());
    }
    let text = similarity_text(added, merge)?;
    if base.len() > opts.max_pairs {
        return None;
    }
    let mut interner = Interner::new();
    let target = line_set(&mut interner, text);
    let mut best: Option<(u64, u64, bool, &[u8])> = None;
    for (p, s) in base {
        let Some(t) = similarity_text(s, merge) else {
            continue;
        };
        let set = line_set(&mut interner, t);
        let shared = shared_count(&target, &set);
        let union = target.len() as u64 + set.len() as u64 - shared;
        if !meets(shared, union, opts.threshold) {
            continue;
        }
        let same = basename(p) == basename(path);
        let better = match best {
            None => true,
            Some((bs, bu, bsame, _)) => {
                let lhs = (shared as u128) * (bu as u128);
                let rhs = (bs as u128) * (union as u128);
                lhs > rhs || (lhs == rhs && same && !bsame)
            }
        };
        if better {
            best = Some((shared, union, same, p.as_slice()));
        }
    }
    best.map(|(_, _, _, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Tree;

    fn tree<'a>(entries: &[(&str, &'a [u8])]) -> Tree<'a> {
        entries
            .iter()
            .map(|(p, c)| (p.as_bytes().to_vec(), Side::file(c)))
            .collect()
    }

    #[test]
    fn exact_rename_prefers_same_basename() {
        let base = tree(&[("a/x.rs", b"same\n"), ("b.rs", b"other\n")]);
        let side = tree(&[
            ("b.rs", b"other\n"),
            ("c/y.rs", b"same\n"),
            ("d/x.rs", b"same\n"),
        ]);
        let r = detect_renames(
            &base,
            &side,
            &RenameOptions::default(),
            &MergeOptions::default(),
        );
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].to, b"d/x.rs".to_vec());
        assert_eq!(r[0].similarity, Similarity::Exact);
    }

    #[test]
    fn similar_rename_threshold() {
        let base = tree(&[("old", b"1\n2\n3\n4\n")]);
        // Shares 3 of 5 distinct lines: 0.6 >= 0.5.
        let side = tree(&[("new", b"1\n2\n3\n5\n")]);
        let r = detect_renames(
            &base,
            &side,
            &RenameOptions::default(),
            &MergeOptions::default(),
        );
        assert_eq!(
            r[0].similarity,
            Similarity::Lines {
                shared: 3,
                union: 5
            }
        );
        // Shares 1 of 5: 0.2 < 0.5.
        let side = tree(&[("new", b"1\n5\n6\n7\n")]);
        let r = detect_renames(
            &base,
            &side,
            &RenameOptions::default(),
            &MergeOptions::default(),
        );
        assert!(r.is_empty());
    }

    #[test]
    fn empty_files_never_rename() {
        let base = tree(&[("a", b"")]);
        let side = tree(&[("b", b"")]);
        let r = detect_renames(
            &base,
            &side,
            &RenameOptions::default(),
            &MergeOptions::default(),
        );
        assert!(r.is_empty());
    }

    #[test]
    fn similarity_is_exact_rational() {
        assert_eq!(line_similarity(b"a\nb\n", b"a\nc\n"), (1, 3));
        assert!(meets(1, 2, (1, 2)));
        assert!(!meets(1, 3, (1, 2)));
    }
}
