//! Rename- and copy-aware merge of flat path maps.

use std::collections::{BTreeMap, BTreeSet};

use crate::merge3::{ConflictHunk, LineMerge};
use crate::path::{
    ConflictKind, Content, ContentSource, MergeOptions, Mode, PathMergeInput, PathOutcome,
    Resolution, Side, merge_path,
};
use crate::rename::{RenameOptions, Similarity, copy_source, detect_renames};
use crate::structural::file_directory_conflicts;

/// A tree flattened to its non-directory entries: `/`-separated path → side.
pub type Tree<'a> = BTreeMap<Vec<u8>, Side<'a>>;

/// Owned content, for trees built by this crate (virtual bases) or held by callers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OwnedContent {
    /// Readable bytes.
    Bytes(Vec<u8>),
    /// Redacted; the payload is an opaque identity used only for equality.
    Redacted(Vec<u8>),
}

/// An owned side.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnedSide {
    /// The entry mode.
    pub mode: Mode,
    /// The content.
    pub content: OwnedContent,
}

impl OwnedSide {
    /// Borrows this side.
    pub fn as_side(&self) -> Side<'_> {
        Side {
            mode: self.mode,
            content: match &self.content {
                OwnedContent::Bytes(b) => Content::Bytes(b),
                OwnedContent::Redacted(id) => Content::Redacted(id),
            },
        }
    }

    /// Copies a borrowed side.
    pub fn from_side(side: &Side<'_>) -> Self {
        Self {
            mode: side.mode,
            content: match side.content {
                Content::Bytes(b) => OwnedContent::Bytes(b.to_vec()),
                Content::Redacted(id) => OwnedContent::Redacted(id.to_vec()),
            },
        }
    }
}

/// An owned flat tree.
pub type OwnedTree = BTreeMap<Vec<u8>, OwnedSide>;

/// Borrows every side of an owned tree.
pub fn borrow_tree(tree: &OwnedTree) -> Tree<'_> {
    tree.iter().map(|(p, s)| (p.clone(), s.as_side())).collect()
}

/// One of the three inputs of a merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Which {
    /// The merge base.
    Base,
    /// Ours.
    Ours,
    /// Theirs.
    Theirs,
}

/// Where a merged entry's content comes from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TreeContent {
    /// The content of an input entry, unchanged (its object can be reused). The path is the
    /// entry's path in that input, which differs from the result path after a rename.
    Side {
        /// The input.
        side: Which,
        /// The path in that input.
        path: Vec<u8>,
    },
    /// New content from a clean line merge.
    Merged(Vec<u8>),
}

/// A clean entry of the merged tree.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TreeEntry {
    /// The mode.
    pub mode: Mode,
    /// The content.
    pub content: TreeContent,
}

/// A conflict at one result path. Each side carries its own path, which differs from the
/// result path when a rename or copy was followed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeConflict<'a> {
    /// The kind.
    pub kind: ConflictKind,
    /// The base side used for this path (for a copy-aware add, the copy source).
    pub base: Option<(Vec<u8>, Side<'a>)>,
    /// Ours.
    pub ours: Option<(Vec<u8>, Side<'a>)>,
    /// Theirs.
    pub theirs: Option<(Vec<u8>, Side<'a>)>,
    /// Conflicting line regions, when a line merge ran.
    pub hunks: Vec<ConflictHunk>,
    /// The line merge, for rendering markers.
    pub lines: Option<LineMerge<'a>>,
    /// For a `mode` conflict: the content, which merged cleanly.
    pub content: Option<TreeContent>,
}

/// A rename followed by the merge.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RenameRecord {
    /// The side that renamed (`Ours` or `Theirs`).
    pub side: Which,
    /// The base path.
    pub from: Vec<u8>,
    /// The path on that side.
    pub to: Vec<u8>,
    /// How the rename was matched.
    pub similarity: Similarity,
}

/// The result of a tree merge: clean entries and conflicts, keyed by result path. A path is
/// in at most one of the two maps; paths in neither are absent from the result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeMerge<'a> {
    /// Clean entries.
    pub entries: BTreeMap<Vec<u8>, TreeEntry>,
    /// Conflicts.
    pub conflicts: BTreeMap<Vec<u8>, TreeConflict<'a>>,
    /// Renames that were followed, sorted by side then base path.
    pub renames: Vec<RenameRecord>,
}

impl TreeMerge<'_> {
    /// True when nothing conflicts.
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Options for [`merge_trees`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TreeMergeOptions {
    /// Per-path limits.
    pub merge: MergeOptions,
    /// Rename and copy detection.
    pub renames: RenameOptions,
}

type Located<'a> = Option<(Vec<u8>, Side<'a>)>;

/// The three located sides merged into one result path.
#[derive(Clone)]
struct Job<'a> {
    base: Located<'a>,
    ours: Located<'a>,
    theirs: Located<'a>,
    /// A rename was followed into this path.
    renamed: bool,
}

fn located<'a>(tree: &Tree<'a>, path: &[u8]) -> Located<'a> {
    tree.get(path).map(|s| (path.to_vec(), *s))
}

/// Merges three flat trees, following renames and copies (contract §6).
///
/// 1. Renames are detected base→ours and base→theirs ([`detect_renames`]).
/// 2. A rename to a path the other side also occupies, other than by renaming the same base
///    path there, is not followed (both paths then merge in place).
/// 3. For each base path renamed by one side, the base entry, the renamed entry and the other
///    side's entry at the base path merge at the new path; if the other side deleted it, that
///    is `modify_delete`. Renamed by both to one path: merged there. Renamed to two different
///    paths: `rename_rename` at both destinations.
/// 4. Every other path merges in place. When both sides added a path with different content
///    and both copies come from the same base file (contract §6.3), that file is the base.
/// 5. Rule 9: [`file_directory_conflicts`] over the candidate result turns every involved path
///    into a `file_directory` conflict.
pub fn merge_trees<'a>(
    base: &Tree<'a>,
    ours: &Tree<'a>,
    theirs: &Tree<'a>,
    opts: &TreeMergeOptions,
) -> TreeMerge<'a> {
    let ro: BTreeMap<Vec<u8>, (Vec<u8>, Similarity)> =
        detect_renames(base, ours, &opts.renames, &opts.merge)
            .into_iter()
            .map(|r| (r.from, (r.to, r.similarity)))
            .collect();
    let rt: BTreeMap<Vec<u8>, (Vec<u8>, Similarity)> =
        detect_renames(base, theirs, &opts.renames, &opts.merge)
            .into_iter()
            .map(|r| (r.from, (r.to, r.similarity)))
            .collect();
    // Step 2: drop renames whose destination the other side occupies independently.
    let keep = |own: &BTreeMap<Vec<u8>, (Vec<u8>, Similarity)>,
                other_renames: &BTreeMap<Vec<u8>, (Vec<u8>, Similarity)>,
                other: &Tree<'a>| {
        own.iter()
            .filter(|(from, (to, _))| {
                !other.contains_key(to)
                    || other_renames
                        .get(*from)
                        .is_some_and(|(other_to, _)| other_to == to)
            })
            .map(|(f, v)| (f.clone(), v.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    let ro_kept = keep(&ro, &rt, theirs);
    let rt_kept = keep(&rt, &ro, ours);

    let mut jobs: BTreeMap<Vec<u8>, Job<'a>> = BTreeMap::new();
    let mut conflicts: BTreeMap<Vec<u8>, TreeConflict<'a>> = BTreeMap::new();
    let mut renames = Vec::new();
    let mut used_base: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut used_ours: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut used_theirs: BTreeSet<Vec<u8>> = BTreeSet::new();

    let renamed_bases: BTreeSet<&Vec<u8>> = ro_kept.keys().chain(rt_kept.keys()).collect();
    for b in renamed_bases {
        let base_side = located(base, b);
        used_base.insert(b.clone());
        match (ro_kept.get(b), rt_kept.get(b)) {
            (Some((p, so)), Some((q, st))) => {
                renames.push(RenameRecord {
                    side: Which::Ours,
                    from: b.clone(),
                    to: p.clone(),
                    similarity: *so,
                });
                renames.push(RenameRecord {
                    side: Which::Theirs,
                    from: b.clone(),
                    to: q.clone(),
                    similarity: *st,
                });
                used_ours.insert(p.clone());
                used_theirs.insert(q.clone());
                if p == q {
                    jobs.insert(
                        p.clone(),
                        Job {
                            base: base_side,
                            ours: located(ours, p),
                            theirs: located(theirs, q),
                            renamed: true,
                        },
                    );
                } else {
                    for dest in [p, q] {
                        conflicts.insert(
                            dest.clone(),
                            TreeConflict {
                                kind: ConflictKind::RenameRename,
                                base: base_side.clone(),
                                ours: located(ours, p),
                                theirs: located(theirs, q),
                                hunks: Vec::new(),
                                lines: None,
                                content: None,
                            },
                        );
                    }
                }
            }
            (Some((p, so)), None) => {
                renames.push(RenameRecord {
                    side: Which::Ours,
                    from: b.clone(),
                    to: p.clone(),
                    similarity: *so,
                });
                used_ours.insert(p.clone());
                used_theirs.insert(b.clone());
                jobs.insert(
                    p.clone(),
                    Job {
                        base: base_side,
                        ours: located(ours, p),
                        theirs: located(theirs, b),
                        renamed: true,
                    },
                );
            }
            (None, Some((q, st))) => {
                renames.push(RenameRecord {
                    side: Which::Theirs,
                    from: b.clone(),
                    to: q.clone(),
                    similarity: *st,
                });
                used_theirs.insert(q.clone());
                used_ours.insert(b.clone());
                jobs.insert(
                    q.clone(),
                    Job {
                        base: base_side,
                        ours: located(ours, b),
                        theirs: located(theirs, q),
                        renamed: true,
                    },
                );
            }
            (None, None) => unreachable!("b comes from one of the rename maps"),
        }
    }

    // Step 4: in-place merges for every remaining path.
    let all: BTreeSet<&Vec<u8>> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    for p in all {
        let job = Job {
            base: if used_base.contains(p) {
                None
            } else {
                located(base, p)
            },
            ours: if used_ours.contains(p) {
                None
            } else {
                located(ours, p)
            },
            theirs: if used_theirs.contains(p) {
                None
            } else {
                located(theirs, p)
            },
            renamed: false,
        };
        if job.base.is_none() && job.ours.is_none() && job.theirs.is_none() {
            continue;
        }
        debug_assert!(!jobs.contains_key(p) && !conflicts.contains_key(p));
        jobs.insert(p.clone(), job);
    }

    let mut entries = BTreeMap::new();
    for (path, job) in &jobs {
        match run_job(base, job, opts) {
            Ok(Some(entry)) => {
                entries.insert(path.clone(), entry);
            }
            Ok(None) => {}
            Err(conflict) => {
                conflicts.insert(path.clone(), *conflict);
            }
        }
    }

    // Step 5: rule 9.
    let candidate: Vec<&[u8]> = entries
        .keys()
        .chain(conflicts.keys())
        .map(|p| p.as_slice())
        .collect();
    let mut involved: BTreeSet<Vec<u8>> = BTreeSet::new();
    for fd in file_directory_conflicts(candidate) {
        involved.insert(fd.file);
        involved.extend(fd.under);
    }
    for path in involved {
        if let Some(c) = conflicts.get_mut(&path) {
            c.kind = ConflictKind::FileDirectory;
            continue;
        }
        entries.remove(&path);
        let job = &jobs[&path];
        conflicts.insert(
            path,
            TreeConflict {
                kind: ConflictKind::FileDirectory,
                base: job.base.clone(),
                ours: job.ours.clone(),
                theirs: job.theirs.clone(),
                hunks: Vec::new(),
                lines: None,
                content: None,
            },
        );
    }

    renames.sort_by(|a, b| (a.side, &a.from).cmp(&(b.side, &b.from)));
    TreeMerge {
        entries,
        conflicts,
        renames,
    }
}

fn content_of(job: &Job<'_>, source: ContentSource) -> TreeContent {
    let path = |l: &Located<'_>| {
        l.as_ref()
            .map(|(p, _)| p.clone())
            .expect("taken side exists")
    };
    match source {
        ContentSource::Ours => TreeContent::Side {
            side: Which::Ours,
            path: path(&job.ours),
        },
        ContentSource::Theirs => TreeContent::Side {
            side: Which::Theirs,
            path: path(&job.theirs),
        },
        ContentSource::Merged(bytes) => TreeContent::Merged(bytes),
    }
}

fn run_job<'a>(
    base_tree: &Tree<'a>,
    job: &Job<'a>,
    opts: &TreeMergeOptions,
) -> Result<Option<TreeEntry>, Box<TreeConflict<'a>>> {
    let conflict = |job: &Job<'a>, kind| TreeConflict {
        kind,
        base: job.base.clone(),
        ours: job.ours.clone(),
        theirs: job.theirs.clone(),
        hunks: Vec::new(),
        lines: None,
        content: None,
    };
    if job.renamed && (job.ours.is_none() || job.theirs.is_none()) {
        // One side renamed the file, the other deleted it.
        return Err(Box::new(conflict(job, ConflictKind::ModifyDelete)));
    }
    let side = |l: &Located<'a>| l.as_ref().map(|(_, s)| *s);
    let mut job = job.clone();
    let mut outcome = merge_path(
        &PathMergeInput {
            base: side(&job.base),
            ours: side(&job.ours),
            theirs: side(&job.theirs),
        },
        &opts.merge,
    );
    if let (PathOutcome::Conflict(c), None, Some((op, os)), Some((tp, ts))) =
        (&outcome, &job.base, &job.ours, &job.theirs)
        && c.kind == ConflictKind::AddAdd
    {
        // Copy-aware add/add: both added files derive from the same base file.
        let so = copy_source(base_tree, op, os, &opts.renames, &opts.merge);
        let st = copy_source(base_tree, tp, ts, &opts.renames, &opts.merge);
        if let (Some(so), Some(st)) = (so, st)
            && so == st
        {
            job.base = located(base_tree, so);
            outcome = merge_path(
                &PathMergeInput {
                    base: side(&job.base),
                    ours: Some(*os),
                    theirs: Some(*ts),
                },
                &opts.merge,
            );
        }
    }
    match outcome {
        PathOutcome::Clean(Resolution::Absent) => Ok(None),
        PathOutcome::Clean(Resolution::Entry { mode, content }) => Ok(Some(TreeEntry {
            mode,
            content: content_of(&job, content),
        })),
        PathOutcome::Conflict(c) => {
            let mut out = conflict(&job, c.kind);
            out.hunks = c.hunks;
            out.lines = c.lines;
            out.content = c.content.map(|s| content_of(&job, s));
            Err(Box::new(out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree<'a>(entries: &[(&str, &'a [u8])]) -> Tree<'a> {
        entries
            .iter()
            .map(|(p, c)| (p.as_bytes().to_vec(), Side::file(c)))
            .collect()
    }

    #[test]
    fn change_follows_rename() {
        let base = tree(&[("a.txt", b"1\n2\n3\n4\n5\n")]);
        let ours = tree(&[("b.txt", b"1\n2\n3\n4\n5\n")]);
        let theirs = tree(&[("a.txt", b"1\n2\n3\n4\nFIVE\n")]);
        let m = merge_trees(&base, &ours, &theirs, &TreeMergeOptions::default());
        assert!(m.is_clean());
        assert_eq!(m.entries.len(), 1);
        assert_eq!(
            m.entries[&b"b.txt".to_vec()].content,
            TreeContent::Side {
                side: Which::Theirs,
                path: b"a.txt".to_vec()
            }
        );
    }

    #[test]
    fn file_directory_collision() {
        let base = tree(&[("d/x", b"x\n")]);
        let ours = tree(&[("d", b"file\n")]);
        let theirs = tree(&[("d/x", b"x\n"), ("d/y", b"y\n")]);
        let m = merge_trees(&base, &ours, &theirs, &TreeMergeOptions::default());
        let kinds: Vec<_> = m
            .conflicts
            .iter()
            .map(|(p, c)| (p.clone(), c.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (b"d".to_vec(), ConflictKind::FileDirectory),
                (b"d/y".to_vec(), ConflictKind::FileDirectory)
            ]
        );
    }
}
