//! Diff between two changesets (PRODUCT §4.1): the file-level difference of their trees,
//! optionally scoped to paths, with rename detection and line hunks from `diff3-v1`'s Myers
//! diff (whitespace-insensitive on request).
//!
//! The tree difference reads only differing subtrees. Blob contents are read only for what
//! was asked: hunks (both sides of each changed file) and rename detection (deleted and added
//! files).

use std::collections::{BTreeMap, BTreeSet};

use bee_core::{Mode, ObjectId, RepoPath, Result};
use bee_merge::{DEFAULT_MAX_TEXT_BYTES, DiffHunk, RenameOptions, Similarity};
use bee_object::DeltaOp;

use crate::merge::{merge_mode, side_of};
use crate::pathhist::LeafEntry;
use crate::repo::{BlobContent, History};

/// What to compute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffOptions {
    /// Only paths at or under these (all paths when empty).
    pub paths: Vec<RepoPath>,
    /// Compute line hunks and line statistics.
    pub hunks: bool,
    /// Context lines around each change.
    pub context: usize,
    /// Pair deleted and added files as renames (exact content, then line similarity ≥ 1/2).
    pub detect_renames: bool,
    /// Compare lines with all ASCII whitespace removed (`-w`).
    pub ignore_whitespace: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        DiffOptions {
            paths: Vec::new(),
            hunks: false,
            context: 3,
            detect_renames: false,
            ignore_whitespace: false,
        }
    }
}

/// How a file changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileChange {
    /// New path.
    Added,
    /// Removed path.
    Deleted,
    /// Changed in place (content or mode).
    Modified,
    /// Moved from another path (possibly with edits).
    Renamed {
        /// The old path.
        from: RepoPath,
        /// `None` for identical content, else `(shared, union)` distinct lines.
        similarity: Option<(u64, u64)>,
    },
}

/// The kind of a hunk line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// Unchanged.
    Context,
    /// Only in the old file.
    Removed,
    /// Only in the new file.
    Added,
}

/// One line of a hunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkLine {
    /// Its kind.
    pub kind: LineKind,
    /// 1-based line number in the old file.
    pub old_lineno: Option<usize>,
    /// 1-based line number in the new file.
    pub new_lineno: Option<usize>,
    /// The line, with its terminator.
    pub content: Vec<u8>,
}

/// A unified hunk (starts are 0-based, as in `bee_merge::UnifiedHunk`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// First old line covered.
    pub old_start: usize,
    /// Old lines covered.
    pub old_len: usize,
    /// First new line covered.
    pub new_start: usize,
    /// New lines covered.
    pub new_len: usize,
    /// The lines.
    pub lines: Vec<HunkLine>,
}

/// One changed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    /// The path (the new path for a rename).
    pub path: RepoPath,
    /// How it changed.
    pub change: FileChange,
    /// The old entry.
    pub old: Option<LeafEntry>,
    /// The new entry.
    pub new: Option<LeafEntry>,
    /// Either side is binary (or too large to diff as text); no hunks then.
    pub binary: bool,
    /// Either side is redacted; no hunks then.
    pub redacted: bool,
    /// Line hunks, when requested.
    pub hunks: Vec<Hunk>,
    /// Lines added (when hunks were computed).
    pub added: usize,
    /// Lines removed (when hunks were computed).
    pub removed: usize,
}

fn leaf(mode: Mode, id: ObjectId, size: u64) -> LeafEntry {
    LeafEntry { mode, id, size }
}

fn strip_ws(line: &[u8]) -> Vec<u8> {
    line.iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect()
}

/// Line hunks between two texts with `context` lines of context; lines are compared with
/// whitespace removed when `ignore_whitespace` is set (output lines are the originals).
pub fn line_hunks(old: &[u8], new: &[u8], context: usize, ignore_whitespace: bool) -> Vec<Hunk> {
    let ol = bee_merge::split_lines(old);
    let nl = bee_merge::split_lines(new);
    let changes = if ignore_whitespace {
        let a: Vec<Vec<u8>> = ol.iter().map(|l| strip_ws(l)).collect();
        let b: Vec<Vec<u8>> = nl.iter().map(|l| strip_ws(l)).collect();
        let ar: Vec<&[u8]> = a.iter().map(Vec::as_slice).collect();
        let br: Vec<&[u8]> = b.iter().map(Vec::as_slice).collect();
        bee_merge::diff_lines(&ar, &br)
    } else {
        bee_merge::diff_lines(&ol, &nl)
    };
    group(&ol, &nl, &changes, context)
}

fn group(ol: &[&[u8]], nl: &[&[u8]], hunks: &[DiffHunk], context: usize) -> Vec<Hunk> {
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < hunks.len() {
        let mut last = idx;
        while last + 1 < hunks.len()
            && hunks[last + 1].old_start - hunks[last].old_end() <= 2 * context
        {
            last += 1;
        }
        let first = hunks[idx];
        let end = hunks[last];
        let lead = context.min(first.old_start);
        let old_start = first.old_start - lead;
        let new_start = first.new_start - lead;
        let old_stop = (end.old_end() + context).min(ol.len());
        let new_stop = end.new_end() + (old_stop - end.old_end());
        let mut lines = Vec::new();
        let (mut o, mut n) = (old_start, new_start);
        let ctx = |lines: &mut Vec<HunkLine>, o: &mut usize, n: &mut usize, to: usize| {
            while *o < to {
                lines.push(HunkLine {
                    kind: LineKind::Context,
                    old_lineno: Some(*o + 1),
                    new_lineno: Some(*n + 1),
                    content: ol[*o].to_vec(),
                });
                *o += 1;
                *n += 1;
            }
        };
        for h in &hunks[idx..=last] {
            ctx(&mut lines, &mut o, &mut n, h.old_start);
            for (i, l) in ol.iter().enumerate().take(h.old_end()).skip(h.old_start) {
                lines.push(HunkLine {
                    kind: LineKind::Removed,
                    old_lineno: Some(i + 1),
                    new_lineno: None,
                    content: l.to_vec(),
                });
            }
            for (j, l) in nl.iter().enumerate().take(h.new_end()).skip(h.new_start) {
                lines.push(HunkLine {
                    kind: LineKind::Added,
                    old_lineno: None,
                    new_lineno: Some(j + 1),
                    content: l.to_vec(),
                });
            }
            o = h.old_end();
            n = h.new_end();
        }
        ctx(&mut lines, &mut o, &mut n, old_stop);
        out.push(Hunk {
            old_start,
            old_len: old_stop - old_start,
            new_start,
            new_len: new_stop - new_start,
            lines,
        });
        idx = last + 1;
    }
    out
}

impl History {
    /// The diff from `from` (`None` = the empty tree) to `to`.
    pub async fn diff(
        &self,
        from: Option<&ObjectId>,
        to: &ObjectId,
        opts: &DiffOptions,
    ) -> Result<Vec<FileDiff>> {
        let old_root = match from {
            Some(f) => Some(*self.changeset(f).await?.root()),
            None => None,
        };
        let new_root = *self.changeset(to).await?.root();
        let mut scopes: Vec<Option<&RepoPath>> = opts.paths.iter().map(Some).collect();
        if scopes.is_empty() {
            scopes.push(None);
        }
        let mut delta = BTreeMap::new();
        for s in scopes {
            for e in self
                .diff_scoped(old_root.as_ref(), Some(&new_root), s)
                .await?
            {
                delta.insert(e.path.clone(), e.op);
            }
        }
        let mut olds: BTreeMap<RepoPath, LeafEntry> = BTreeMap::new();
        if let Some(root) = &old_root {
            for p in delta.keys() {
                if let Some(e) = self.entry_at(root, p).await?
                    && e.is_leaf()
                {
                    olds.insert(p.clone(), leaf(e.mode, e.id, e.size));
                }
            }
        }
        let mut files: Vec<FileDiff> = Vec::new();
        for (path, op) in &delta {
            let old = olds.get(path).copied();
            let (new, change) = match *op {
                DeltaOp::Put { mode, id, size } => (
                    Some(leaf(mode, id, size)),
                    if old.is_some() {
                        FileChange::Modified
                    } else {
                        FileChange::Added
                    },
                ),
                DeltaOp::Delete => (None, FileChange::Deleted),
            };
            files.push(FileDiff {
                path: path.clone(),
                change,
                old,
                new,
                binary: false,
                redacted: false,
                hunks: Vec::new(),
                added: 0,
                removed: 0,
            });
        }
        let mut contents: BTreeMap<ObjectId, BlobContent> = BTreeMap::new();
        if opts.detect_renames {
            self.pair_renames(&mut files, &mut contents).await?;
        }
        if opts.hunks {
            for f in &mut files {
                let old = match f.old {
                    Some(e) => Some(self.cached_content(&e, &mut contents).await?),
                    None => None,
                };
                let new = match f.new {
                    Some(e) => Some(self.cached_content(&e, &mut contents).await?),
                    None => None,
                };
                if old.as_ref().is_some_and(BlobContent::is_redacted)
                    || new.as_ref().is_some_and(BlobContent::is_redacted)
                {
                    f.redacted = true;
                    continue;
                }
                let ob = old.as_ref().map_or(&[][..], BlobContent::bytes_or_empty);
                let nb = new.as_ref().map_or(&[][..], BlobContent::bytes_or_empty);
                if bee_merge::is_binary(ob, DEFAULT_MAX_TEXT_BYTES)
                    || bee_merge::is_binary(nb, DEFAULT_MAX_TEXT_BYTES)
                {
                    f.binary = true;
                    continue;
                }
                f.hunks = line_hunks(ob, nb, opts.context, opts.ignore_whitespace);
                for h in &f.hunks {
                    for l in &h.lines {
                        match l.kind {
                            LineKind::Added => f.added += 1,
                            LineKind::Removed => f.removed += 1,
                            LineKind::Context => {}
                        }
                    }
                }
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    async fn cached_content(
        &self,
        e: &LeafEntry,
        cache: &mut BTreeMap<ObjectId, BlobContent>,
    ) -> Result<BlobContent> {
        if let Some(c) = cache.get(&e.id) {
            return Ok(c.clone());
        }
        let c = self.leaf_content(e.mode, &e.id).await?;
        cache.insert(e.id, c.clone());
        Ok(c)
    }

    /// Replaces matched deleted/added pairs with one rename each (bee-merge detection).
    async fn pair_renames(
        &self,
        files: &mut Vec<FileDiff>,
        contents: &mut BTreeMap<ObjectId, BlobContent>,
    ) -> Result<()> {
        let mut deleted: BTreeMap<Vec<u8>, (LeafEntry, BlobContent)> = BTreeMap::new();
        let mut added: BTreeMap<Vec<u8>, (LeafEntry, BlobContent)> = BTreeMap::new();
        for f in files.iter() {
            match (&f.change, f.old, f.new) {
                (FileChange::Deleted, Some(o), _) => {
                    let c = self.cached_content(&o, contents).await?;
                    deleted.insert(f.path.as_bytes().to_vec(), (o, c));
                }
                (FileChange::Added, _, Some(n)) => {
                    let c = self.cached_content(&n, contents).await?;
                    added.insert(f.path.as_bytes().to_vec(), (n, c));
                }
                _ => {}
            }
        }
        if deleted.is_empty() || added.is_empty() {
            return Ok(());
        }
        let base: bee_merge::Tree<'_> = deleted
            .iter()
            .map(|(p, (e, c))| (p.clone(), side_of(merge_mode(e.mode), c, &e.id)))
            .collect();
        let side: bee_merge::Tree<'_> = added
            .iter()
            .map(|(p, (e, c))| (p.clone(), side_of(merge_mode(e.mode), c, &e.id)))
            .collect();
        let renames = bee_merge::detect_renames(
            &base,
            &side,
            &RenameOptions::default(),
            &bee_merge::MergeOptions::default(),
        );
        let mut gone: BTreeSet<Vec<u8>> = BTreeSet::new();
        type Moved = (RepoPath, Option<(u64, u64)>, LeafEntry);
        let mut moved: BTreeMap<Vec<u8>, Moved> = BTreeMap::new();
        for r in renames {
            let from = RepoPath::new(r.from.clone())?;
            let similarity = match r.similarity {
                Similarity::Exact => None,
                Similarity::Lines { shared, union } => Some((shared, union)),
            };
            moved.insert(r.to.clone(), (from, similarity, deleted[&r.from].0));
            gone.insert(r.from);
        }
        files.retain(|f| !(f.change == FileChange::Deleted && gone.contains(f.path.as_bytes())));
        for f in files.iter_mut() {
            if let Some((from, similarity, old)) = moved.remove(f.path.as_bytes()) {
                f.change = FileChange::Renamed { from, similarity };
                f.old = Some(old);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouping_matches_bee_merge_unified_hunks() {
        let old = b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let new = b"a\nB\nc\nd\ne\nf\ng\nh\nI\nj\nk\nl\n";
        for context in [0, 1, 3] {
            let ours = line_hunks(old, new, context, false);
            let theirs = bee_merge::unified_hunks(old, new, context);
            assert_eq!(ours.len(), theirs.len());
            for (a, b) in ours.iter().zip(&theirs) {
                assert_eq!(
                    (a.old_start, a.old_len, a.new_start, a.new_len),
                    (b.old_start, b.old_len, b.new_start, b.new_len)
                );
                assert_eq!(a.lines.len(), b.lines.len());
                for (x, y) in a.lines.iter().zip(&b.lines) {
                    assert_eq!(x.content, y.content);
                    assert_eq!((x.old_lineno, x.new_lineno), (y.old_lineno, y.new_lineno));
                }
            }
        }
    }

    #[test]
    fn whitespace_only_changes_vanish_with_ignore_whitespace() {
        let old = b"fn main() {\n    run();\n}\n";
        let new = b"fn main() {\n\trun( );\n}\n";
        assert_eq!(line_hunks(old, new, 3, false).len(), 1);
        assert!(line_hunks(old, new, 3, true).is_empty());
        let changed = b"fn main() {\n\tstop();\n}\n";
        assert_eq!(line_hunks(old, changed, 3, true).len(), 1);
    }
}
