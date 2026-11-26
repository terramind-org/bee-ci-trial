//! Line diff: hunks, unified rendering and statistics.

use std::collections::HashMap;

use crate::lines::split_lines;
use crate::myers::matched_pairs;

/// One maximal run of changed lines between two texts.
///
/// Ranges are 0-based line indexes. A hunk deletes `old_len` lines starting at `old_start` and
/// inserts `new_len` lines starting at `new_start`; within a hunk deletions come before
/// insertions. Either length may be zero, never both. Consecutive hunks are separated by at
/// least one unchanged line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiffHunk {
    /// First deleted line in the old text (or the insertion point when `old_len == 0`).
    pub old_start: usize,
    /// Number of deleted lines.
    pub old_len: usize,
    /// First inserted line in the new text (or the deletion point when `new_len == 0`).
    pub new_start: usize,
    /// Number of inserted lines.
    pub new_len: usize,
}

impl DiffHunk {
    /// One past the last deleted line.
    pub fn old_end(&self) -> usize {
        self.old_start + self.old_len
    }

    /// One past the last inserted line.
    pub fn new_end(&self) -> usize {
        self.new_start + self.new_len
    }
}

/// Assigns each distinct line a small integer, in order of first appearance.
pub(crate) struct Interner<'a> {
    ids: HashMap<&'a [u8], u32>,
}

impl<'a> Interner<'a> {
    pub(crate) fn new() -> Self {
        Self {
            ids: HashMap::new(),
        }
    }

    pub(crate) fn intern(&mut self, line: &'a [u8]) -> u32 {
        let next = self.ids.len() as u32;
        *self.ids.entry(line).or_insert(next)
    }

    pub(crate) fn intern_all(&mut self, lines: &[&'a [u8]]) -> Vec<u32> {
        lines.iter().map(|l| self.intern(l)).collect()
    }
}

/// Diffs two line sequences with the forward greedy Myers algorithm of `diff3-v1`.
///
/// The result is the shortest edit script, normalized into hunks; ties between equally short
/// scripts are broken as the contract specifies, so the output is fully determined.
pub fn diff_lines(old: &[&[u8]], new: &[&[u8]]) -> Vec<DiffHunk> {
    let mut interner = Interner::new();
    let a = interner.intern_all(old);
    let b = interner.intern_all(new);
    hunks_from_pairs(&matched_pairs(&a, &b), old.len(), new.len())
}

/// Splits both texts into lines ([`split_lines`]) and diffs them ([`diff_lines`]).
pub fn diff(old: &[u8], new: &[u8]) -> Vec<DiffHunk> {
    diff_lines(&split_lines(old), &split_lines(new))
}

/// Turns increasing matched pairs into the hunks between them.
pub(crate) fn hunks_from_pairs(pairs: &[(usize, usize)], n: usize, m: usize) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();
    let (mut x, mut y) = (0usize, 0usize);
    for &(i, j) in pairs.iter().chain(std::iter::once(&(n, m))) {
        if i > x || j > y {
            hunks.push(DiffHunk {
                old_start: x,
                old_len: i - x,
                new_start: y,
                new_len: j - y,
            });
        }
        x = i + 1;
        y = j + 1;
    }
    hunks
}

/// Summary counts of a diff.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DiffStats {
    /// Number of hunks (maximal changed runs, before context grouping).
    pub hunks: usize,
    /// Lines inserted.
    pub added: usize,
    /// Lines deleted.
    pub removed: usize,
}

impl DiffStats {
    /// Counts the lines in `hunks`.
    pub fn from_hunks(hunks: &[DiffHunk]) -> Self {
        Self {
            hunks: hunks.len(),
            added: hunks.iter().map(|h| h.new_len).sum(),
            removed: hunks.iter().map(|h| h.old_len).sum(),
        }
    }

    /// Diffs two texts and counts the result.
    pub fn between(old: &[u8], new: &[u8]) -> Self {
        Self::from_hunks(&diff(old, new))
    }
}

/// The origin of one line in a unified hunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineOrigin {
    /// Present in both texts (` `).
    Context,
    /// Only in the old text (`-`).
    Removed,
    /// Only in the new text (`+`).
    Added,
}

impl LineOrigin {
    /// The prefix byte used in unified diffs.
    pub fn prefix(self) -> u8 {
        match self {
            LineOrigin::Context => b' ',
            LineOrigin::Removed => b'-',
            LineOrigin::Added => b'+',
        }
    }
}

/// One line of a unified hunk with its origin and 1-based line numbers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiffLine<'a> {
    /// Where the line comes from.
    pub origin: LineOrigin,
    /// 1-based line number in the old text (absent for added lines).
    pub old_lineno: Option<usize>,
    /// 1-based line number in the new text (absent for removed lines).
    pub new_lineno: Option<usize>,
    /// The line, including its terminator if it has one.
    pub content: &'a [u8],
}

/// A unified-diff hunk: changed lines plus surrounding context.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UnifiedHunk<'a> {
    /// 0-based first old line covered by the hunk.
    pub old_start: usize,
    /// Old lines covered (context plus removed).
    pub old_len: usize,
    /// 0-based first new line covered by the hunk.
    pub new_start: usize,
    /// New lines covered (context plus added).
    pub new_len: usize,
    /// The lines, in display order: within each change, removed lines before added lines.
    pub lines: Vec<DiffLine<'a>>,
}

impl UnifiedHunk<'_> {
    /// The `@@ -a,b +c,d @@` header line, with its newline.
    ///
    /// Starts are 1-based; a zero-length range shows the line before it (0 at the top). A
    /// length of exactly 1 is omitted.
    pub fn header(&self) -> String {
        fn range(start: usize, len: usize) -> String {
            match len {
                0 => format!("{start},0"),
                1 => format!("{}", start + 1),
                _ => format!("{},{}", start + 1, len),
            }
        }
        format!(
            "@@ -{} +{} @@\n",
            range(self.old_start, self.old_len),
            range(self.new_start, self.new_len)
        )
    }
}

/// Groups the diff of two texts into unified hunks with `context` lines around each change.
///
/// Two changes share a hunk when at most `2 * context` unchanged lines separate them.
pub fn unified_hunks<'a>(old: &'a [u8], new: &'a [u8], context: usize) -> Vec<UnifiedHunk<'a>> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let hunks = diff_lines(&old_lines, &new_lines);
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < hunks.len() {
        // Extend the group while the gap to the next change fits in 2 * context.
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
        let old_stop = (end.old_end() + context).min(old_lines.len());
        let trail = old_stop - end.old_end();
        let new_stop = end.new_end() + trail;

        let mut lines = Vec::new();
        let mut o = old_start;
        let mut n = new_start;
        let context_until = |lines: &mut Vec<DiffLine<'a>>, o: &mut usize, n: &mut usize, to| {
            while *o < to {
                lines.push(DiffLine {
                    origin: LineOrigin::Context,
                    old_lineno: Some(*o + 1),
                    new_lineno: Some(*n + 1),
                    content: old_lines[*o],
                });
                *o += 1;
                *n += 1;
            }
        };
        for h in &hunks[idx..=last] {
            context_until(&mut lines, &mut o, &mut n, h.old_start);
            for (i, &content) in old_lines
                .iter()
                .enumerate()
                .take(h.old_end())
                .skip(h.old_start)
            {
                lines.push(DiffLine {
                    origin: LineOrigin::Removed,
                    old_lineno: Some(i + 1),
                    new_lineno: None,
                    content,
                });
            }
            for (j, &content) in new_lines
                .iter()
                .enumerate()
                .take(h.new_end())
                .skip(h.new_start)
            {
                lines.push(DiffLine {
                    origin: LineOrigin::Added,
                    old_lineno: None,
                    new_lineno: Some(j + 1),
                    content,
                });
            }
            o = h.old_end();
            n = h.new_end();
        }
        context_until(&mut lines, &mut o, &mut n, old_stop);
        debug_assert_eq!(n, new_stop);
        out.push(UnifiedHunk {
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

/// Renders unified hunks: each header followed by its lines prefixed with ` `, `-` or `+`.
///
/// A line without a terminator is followed by `\n\ No newline at end of file\n`. No file
/// header (`---`/`+++`) is written; callers add one if they want it.
pub fn render_unified(hunks: &[UnifiedHunk<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for h in hunks {
        out.extend_from_slice(h.header().as_bytes());
        for line in &h.lines {
            out.push(line.origin.prefix());
            out.extend_from_slice(line.content);
            if !line.content.ends_with(b"\n") {
                out.extend_from_slice(b"\n\\ No newline at end of file\n");
            }
        }
    }
    out
}

/// Diffs two texts and renders the result as unified hunks with `context` lines of context.
/// Identical texts render as the empty string.
pub fn unified_diff(old: &[u8], new: &[u8], context: usize) -> Vec<u8> {
    render_unified(&unified_hunks(old, new, context))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_texts_have_no_hunks() {
        assert!(diff(b"a\nb\n", b"a\nb\n").is_empty());
        assert!(unified_diff(b"a\n", b"a\n", 3).is_empty());
    }

    #[test]
    fn hunks_cover_both_texts_exactly() {
        let old = b"a\nb\nc\nd\ne\n";
        let new = b"a\nx\nc\ne\nf\n";
        let hunks = diff(old, new);
        assert_eq!(
            hunks,
            vec![
                DiffHunk {
                    old_start: 1,
                    old_len: 1,
                    new_start: 1,
                    new_len: 1
                },
                DiffHunk {
                    old_start: 3,
                    old_len: 1,
                    new_start: 3,
                    new_len: 0
                },
                DiffHunk {
                    old_start: 5,
                    old_len: 0,
                    new_start: 4,
                    new_len: 1
                },
            ]
        );
        assert_eq!(
            DiffStats::from_hunks(&hunks),
            DiffStats {
                hunks: 3,
                added: 2,
                removed: 2
            }
        );
    }

    #[test]
    fn context_zero_and_grouping() {
        let old = b"1\n2\n3\n4\n5\n6\n7\n8\n";
        let new = b"1\nX\n3\n4\n5\n6\nY\n8\n";
        // Gap of four unchanged lines: separate hunks at context 1, one hunk at context 2.
        assert_eq!(unified_hunks(old, new, 1).len(), 2);
        assert_eq!(unified_hunks(old, new, 2).len(), 1);
        assert_eq!(
            String::from_utf8(unified_diff(old, new, 0)).unwrap(),
            "@@ -2 +2 @@\n-2\n+X\n@@ -7 +7 @@\n-7\n+Y\n"
        );
    }

    #[test]
    fn missing_newline_is_marked() {
        assert_eq!(
            String::from_utf8(unified_diff(b"a", b"b", 3)).unwrap(),
            "@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n"
        );
    }

    #[test]
    fn line_numbers_are_one_based() {
        let hunks = unified_hunks(b"a\nb\n", b"a\nc\n", 1);
        let numbers: Vec<_> = hunks[0]
            .lines
            .iter()
            .map(|l| (l.old_lineno, l.new_lineno))
            .collect();
        assert_eq!(
            numbers,
            vec![(Some(1), Some(1)), (Some(2), None), (None, Some(2))]
        );
    }
}
