//! The `diff3-v1` three-way line merge.

use crate::diff::{DiffHunk, Interner, hunks_from_pairs};
use crate::lines::split_lines;
use crate::markers::{MarkerStyle, render_conflict};
use crate::myers::matched_pairs;

/// Which input a clean region's lines are taken from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// Lines unchanged on both sides.
    Base,
    /// Lines changed only by ours, or changed identically by both sides.
    Ours,
    /// Lines changed only by theirs.
    Theirs,
}

/// A conflicting region, as 0-based line ranges into each input.
///
/// This is the `ConflictHunk` of the wire contract. A zero length is an empty range at that
/// position (for example an insertion point in the base).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConflictHunk {
    /// First base line of the region.
    pub base_start: usize,
    /// Base lines in the region.
    pub base_len: usize,
    /// First ours line of the region.
    pub ours_start: usize,
    /// Ours lines in the region.
    pub ours_len: usize,
    /// First theirs line of the region.
    pub theirs_start: usize,
    /// Theirs lines in the region.
    pub theirs_len: usize,
}

/// One region of a merge result, in output order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Region {
    /// Lines `start..end` of `source`, taken as they are. Never empty.
    Clean {
        /// The input the lines come from.
        source: Source,
        /// First line (0-based) in that input.
        start: usize,
        /// One past the last line.
        end: usize,
    },
    /// Both sides changed the region differently.
    Conflict(ConflictHunk),
}

/// The result of a line merge. Conflicts are data: the merge never writes markers unless
/// asked to render them for a working copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMerge<'a> {
    base: Vec<&'a [u8]>,
    ours: Vec<&'a [u8]>,
    theirs: Vec<&'a [u8]>,
    regions: Vec<Region>,
}

impl<'a> LineMerge<'a> {
    /// The regions of the result, in order.
    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// True when no region conflicts.
    pub fn is_clean(&self) -> bool {
        self.conflicts().next().is_none()
    }

    /// The conflicting regions, in order.
    pub fn conflicts(&self) -> impl Iterator<Item = ConflictHunk> + '_ {
        self.regions.iter().filter_map(|r| match r {
            Region::Conflict(h) => Some(*h),
            Region::Clean { .. } => None,
        })
    }

    /// The base lines.
    pub fn base_lines(&self) -> &[&'a [u8]] {
        &self.base
    }

    /// The ours lines.
    pub fn ours_lines(&self) -> &[&'a [u8]] {
        &self.ours
    }

    /// The theirs lines.
    pub fn theirs_lines(&self) -> &[&'a [u8]] {
        &self.theirs
    }

    fn lines_of(&self, source: Source) -> &[&'a [u8]] {
        match source {
            Source::Base => &self.base,
            Source::Ours => &self.ours,
            Source::Theirs => &self.theirs,
        }
    }

    /// The merged text, or `None` if any region conflicts.
    pub fn merged(&self) -> Option<Vec<u8>> {
        if !self.is_clean() {
            return None;
        }
        Some(self.render(MarkerStyle::Diff3))
    }

    /// Renders the result for a working copy: clean regions as they are and each conflict as
    /// a marker block in the given style (see [`render_conflict`]). A clean merge renders as
    /// the merged text.
    pub fn render(&self, style: MarkerStyle) -> Vec<u8> {
        let mut out = Vec::new();
        for region in &self.regions {
            match *region {
                Region::Clean { source, start, end } => {
                    for line in &self.lines_of(source)[start..end] {
                        out.extend_from_slice(line);
                    }
                }
                Region::Conflict(h) => render_conflict(
                    &mut out,
                    &self.ours[h.ours_start..h.ours_start + h.ours_len],
                    &self.base[h.base_start..h.base_start + h.base_len],
                    &self.theirs[h.theirs_start..h.theirs_start + h.theirs_len],
                    style,
                ),
            }
        }
        out
    }
}

/// One side's hunk, tagged with its side, while forming regions.
#[derive(Clone, Copy)]
struct Tagged {
    hunk: DiffHunk,
    theirs: bool,
}

/// Merges `ours` and `theirs` against `base` with `diff3-v1`.
///
/// Each side is diffed against the base. Hunks whose base ranges overlap or touch form one
/// region: closed intervals `[old_start, old_end]` that intersect, so adjacent changes and an
/// insertion at either end of (or inside) another change's range are in the same region. A
/// region changed by one side takes that side; changed by both, it takes the shared lines when
/// both sides produced identical lines and conflicts otherwise.
pub fn merge_lines<'a>(base: &'a [u8], ours: &'a [u8], theirs: &'a [u8]) -> LineMerge<'a> {
    let base = split_lines(base);
    let ours = split_lines(ours);
    let theirs = split_lines(theirs);
    let mut interner = Interner::new();
    let b = interner.intern_all(&base);
    let o = interner.intern_all(&ours);
    let t = interner.intern_all(&theirs);
    let ours_hunks = hunks_from_pairs(&matched_pairs(&b, &o), b.len(), o.len());
    let theirs_hunks = hunks_from_pairs(&matched_pairs(&b, &t), b.len(), t.len());

    let mut tagged: Vec<Tagged> = ours_hunks
        .iter()
        .map(|&hunk| Tagged {
            hunk,
            theirs: false,
        })
        .chain(
            theirs_hunks
                .iter()
                .map(|&hunk| Tagged { hunk, theirs: true }),
        )
        .collect();
    // Stable sort by base start: ours before theirs at equal starts (only affects grouping
    // order, never the result).
    tagged.sort_by_key(|t| (t.hunk.old_start, t.hunk.old_end(), t.theirs));

    let mut regions = Vec::new();
    let mut base_pos = 0usize;
    let mut idx = 0usize;
    while idx < tagged.len() {
        let region_start = tagged[idx].hunk.old_start;
        let mut region_end = tagged[idx].hunk.old_end();
        let mut last = idx;
        while last + 1 < tagged.len() && tagged[last + 1].hunk.old_start <= region_end {
            last += 1;
            region_end = region_end.max(tagged[last].hunk.old_end());
        }
        let group = &tagged[idx..=last];
        push_clean(&mut regions, Source::Base, base_pos, region_start);

        let ours_range = side_range(group, false, region_start, region_end);
        let theirs_range = side_range(group, true, region_start, region_end);
        match (ours_range, theirs_range) {
            (Some((s, e)), None) => push_clean(&mut regions, Source::Ours, s, e),
            (None, Some((s, e))) => push_clean(&mut regions, Source::Theirs, s, e),
            (Some((os, oe)), Some((ts, te))) => {
                if o[os..oe] == t[ts..te] {
                    push_clean(&mut regions, Source::Ours, os, oe);
                } else {
                    regions.push(Region::Conflict(ConflictHunk {
                        base_start: region_start,
                        base_len: region_end - region_start,
                        ours_start: os,
                        ours_len: oe - os,
                        theirs_start: ts,
                        theirs_len: te - ts,
                    }));
                }
            }
            (None, None) => unreachable!("a region has at least one hunk"),
        }
        base_pos = region_end;
        idx = last + 1;
    }
    push_clean(&mut regions, Source::Base, base_pos, base.len());
    LineMerge {
        base,
        ours,
        theirs,
        regions,
    }
}

/// The range a side occupies for base region `[rs, re)`, if that side changed the region.
fn side_range(group: &[Tagged], theirs: bool, rs: usize, re: usize) -> Option<(usize, usize)> {
    let mut hunks = group.iter().filter(|t| t.theirs == theirs).map(|t| t.hunk);
    let first = hunks.next()?;
    let last = hunks.next_back().unwrap_or(first);
    // Outside its hunks a side maps base lines one to one.
    Some((
        first.new_start - (first.old_start - rs),
        last.new_end() + (re - last.old_end()),
    ))
}

fn push_clean(regions: &mut Vec<Region>, source: Source, start: usize, end: usize) {
    if start < end {
        regions.push(Region::Clean { source, start, end });
    }
}

/// Two-way merge of independent additions (no base): the lines both sides share are clean
/// and every difference is a conflict.
///
/// Identical inputs merge cleanly. Otherwise the regions follow the diff from `ours` to
/// `theirs`: unchanged runs are clean (taken from ours) and each diff hunk is a conflict whose
/// base range is empty at line 0.
pub fn merge_two_way<'a>(ours: &'a [u8], theirs: &'a [u8]) -> LineMerge<'a> {
    let ours = split_lines(ours);
    let theirs = split_lines(theirs);
    let mut interner = Interner::new();
    let o = interner.intern_all(&ours);
    let t = interner.intern_all(&theirs);
    let hunks = hunks_from_pairs(&matched_pairs(&o, &t), o.len(), t.len());
    let mut regions = Vec::new();
    let mut pos = 0usize;
    for h in hunks {
        push_clean(&mut regions, Source::Ours, pos, h.old_start);
        regions.push(Region::Conflict(ConflictHunk {
            base_start: 0,
            base_len: 0,
            ours_start: h.old_start,
            ours_len: h.old_len,
            theirs_start: h.new_start,
            theirs_len: h.new_len,
        }));
        pos = h.old_end();
    }
    push_clean(&mut regions, Source::Ours, pos, ours.len());
    LineMerge {
        base: Vec::new(),
        ours,
        theirs,
        regions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(b: &str, o: &str, t: &str) -> Option<String> {
        merge_lines(b.as_bytes(), o.as_bytes(), t.as_bytes())
            .merged()
            .map(|v| String::from_utf8(v).unwrap())
    }

    #[test]
    fn separated_changes_merge() {
        assert_eq!(
            merged("a\nb\nc\n", "A\nb\nc\n", "a\nb\nC\n").as_deref(),
            Some("A\nb\nC\n")
        );
    }

    #[test]
    fn adjacent_changes_touch_and_conflict() {
        let m = merge_lines(b"a\nb\nc\n", b"A\nb\nc\n", b"a\nB\nc\n");
        assert_eq!(
            m.conflicts().collect::<Vec<_>>(),
            vec![ConflictHunk {
                base_start: 0,
                base_len: 2,
                ours_start: 0,
                ours_len: 2,
                theirs_start: 0,
                theirs_len: 2
            }]
        );
    }

    #[test]
    fn insertion_at_either_end_of_a_change_touches() {
        // Theirs inserts before line b (index 1) and ours changes b.
        assert!(merged("a\nb\nc\n", "a\nB\nc\n", "a\nX\nb\nc\n").is_none());
        // Theirs inserts after line b.
        assert!(merged("a\nb\nc\n", "a\nB\nc\n", "a\nb\nX\nc\n").is_none());
        // Theirs inserts after c: not touching b.
        assert_eq!(
            merged("a\nb\nc\n", "a\nB\nc\n", "a\nb\nc\nX\n").as_deref(),
            Some("a\nB\nc\nX\n")
        );
    }

    #[test]
    fn identical_changes_are_taken_once() {
        assert_eq!(
            merged("a\nb\n", "a\nX\n", "a\nX\n").as_deref(),
            Some("a\nX\n")
        );
    }

    #[test]
    fn two_way_conflicts_on_differences_only() {
        let m = merge_two_way(b"a\nb\nc\n", b"a\nX\nc\n");
        assert_eq!(m.conflicts().count(), 1);
        assert!(merge_two_way(b"same\n", b"same\n").is_clean());
    }
}
