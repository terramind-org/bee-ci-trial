//! Per-path merge rules (SPEC §7.3 rules 1–8; rule 9 is structural, see `structural`).

use crate::merge3::{ConflictHunk, LineMerge, merge_lines, merge_two_way};

/// Default `merge_max_text_bytes`: larger sides are treated as binary.
pub const DEFAULT_MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// Number of leading bytes inspected for a NUL byte by [`is_binary`].
pub const BINARY_SNIFF_BYTES: usize = 8000;

/// The mode of a non-directory tree entry (SPEC §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    /// A regular file.
    File,
    /// An executable file.
    Executable,
    /// A symbolic link; the content is the link target.
    Symlink,
    /// A submodule pointer; the content is the foreign object id.
    Gitlink,
}

/// The content of a side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Content<'a> {
    /// The bytes are available.
    Bytes(&'a [u8]),
    /// The bytes were redacted. The payload is an opaque identity (for example the object id)
    /// used only for equality; it is never read as content.
    Redacted(&'a [u8]),
}

impl<'a> Content<'a> {
    /// The bytes, if they are available.
    pub fn bytes(&self) -> Option<&'a [u8]> {
        match *self {
            Content::Bytes(b) => Some(b),
            Content::Redacted(_) => None,
        }
    }
}

/// One side of a path: a mode and its content. An absent side is `None` wherever a side is
/// optional.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Side<'a> {
    /// The entry mode.
    pub mode: Mode,
    /// The content.
    pub content: Content<'a>,
}

impl<'a> Side<'a> {
    /// A side with readable bytes.
    pub fn new(mode: Mode, bytes: &'a [u8]) -> Self {
        Self {
            mode,
            content: Content::Bytes(bytes),
        }
    }

    /// A regular file.
    pub fn file(bytes: &'a [u8]) -> Self {
        Self::new(Mode::File, bytes)
    }
}

/// The three sides of one path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PathMergeInput<'a> {
    /// The merge base; `None` when the path did not exist there.
    pub base: Option<Side<'a>>,
    /// Ours (the side being merged onto).
    pub ours: Option<Side<'a>>,
    /// Theirs (the side being merged in).
    pub theirs: Option<Side<'a>>,
}

/// Merge limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MergeOptions {
    /// Sides larger than this many bytes are treated as binary (`merge_max_text_bytes`).
    pub max_text_bytes: usize,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            max_text_bytes: DEFAULT_MAX_TEXT_BYTES,
        }
    }
}

/// The kind of a conflict (the wire `ConflictKind`, plus rename kinds).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConflictKind {
    /// Both sides changed the same lines differently.
    Content,
    /// Both sides added the path with different content.
    AddAdd,
    /// One side deleted the path, the other modified (or renamed) it.
    ModifyDelete,
    /// A symlink is involved and the sides differ.
    Symlink,
    /// A side is binary, too large to merge as text, or a gitlink.
    Binary,
    /// A side whose bytes are needed was redacted.
    Redacted,
    /// The contents merge but both sides changed the mode differently.
    Mode,
    /// A file on one side collides with a directory on the other.
    FileDirectory,
    /// Both sides renamed the same base path to different paths.
    RenameRename,
}

impl ConflictKind {
    /// The snake_case name used in payloads and the contract.
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictKind::Content => "content",
            ConflictKind::AddAdd => "add_add",
            ConflictKind::ModifyDelete => "modify_delete",
            ConflictKind::Symlink => "symlink",
            ConflictKind::Binary => "binary",
            ConflictKind::Redacted => "redacted",
            ConflictKind::Mode => "mode",
            ConflictKind::FileDirectory => "file_directory",
            ConflictKind::RenameRename => "rename_rename",
        }
    }
}

/// Where a resolved entry's content comes from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ContentSource {
    /// Ours' content, unchanged (its object can be reused).
    Ours,
    /// Theirs' content, unchanged (its object can be reused).
    Theirs,
    /// New content produced by a clean line merge.
    Merged(Vec<u8>),
}

/// A clean result for one path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Resolution {
    /// The path is absent from the result.
    Absent,
    /// The path is present with this mode and content.
    Entry {
        /// The resolved mode.
        mode: Mode,
        /// The resolved content.
        content: ContentSource,
    },
}

/// A conflict on one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathConflict<'a> {
    /// The kind.
    pub kind: ConflictKind,
    /// The conflicting line regions, when a line merge (or two-way merge) was run.
    pub hunks: Vec<ConflictHunk>,
    /// The line merge itself, for rendering markers in a working copy.
    pub lines: Option<LineMerge<'a>>,
    /// For a `mode` conflict: the content, which merged cleanly.
    pub content: Option<ContentSource>,
}

impl<'a> PathConflict<'a> {
    fn bare(kind: ConflictKind) -> Self {
        Self {
            kind,
            hunks: Vec::new(),
            lines: None,
            content: None,
        }
    }

    fn with_lines(kind: ConflictKind, lines: LineMerge<'a>) -> Self {
        Self {
            kind,
            hunks: lines.conflicts().collect(),
            lines: Some(lines),
            content: None,
        }
    }
}

/// The outcome of merging one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathOutcome<'a> {
    /// Merged cleanly.
    Clean(Resolution),
    /// Conflicted.
    Conflict(PathConflict<'a>),
}

/// True when `bytes` must be merged as binary: longer than `max_text_bytes`, or containing a
/// NUL byte in its first [`BINARY_SNIFF_BYTES`] bytes (a NUL at index 7999 is binary, at index
/// 8000 is not).
pub fn is_binary(bytes: &[u8], max_text_bytes: usize) -> bool {
    bytes.len() > max_text_bytes || bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

fn take(side: Option<Side<'_>>, source: ContentSource) -> PathOutcome<'_> {
    PathOutcome::Clean(match side {
        None => Resolution::Absent,
        Some(s) => Resolution::Entry {
            mode: s.mode,
            content: source,
        },
    })
}

/// Merges one path by the rules of SPEC §7.3, in order:
///
/// 1. ours = base → take theirs. 2. theirs = base, or ours = theirs → take ours.
/// 3. no base → identical contents take ours' content (differing modes are a `mode`
///    conflict); otherwise `add_add`, with a two-way line merge as the payload when both
///    sides are readable text.
/// 4. one side absent → `modify_delete`.
/// 5. a symlink on any side → `symlink`. Gitlinks likewise give `binary`.
/// 6. contents and modes then merge separately by rules 1–2. When all three contents
///    differ: any readable side binary → `binary`; any side redacted → `redacted`; else the
///    `diff3-v1` line merge (`content` conflict if it conflicts).
/// 7. modes that cannot merge give `mode` when the content merged cleanly.
///
/// Equality is on mode and content together; redacted content compares by its identity.
pub fn merge_path<'a>(input: &PathMergeInput<'a>, opts: &MergeOptions) -> PathOutcome<'a> {
    let PathMergeInput { base, ours, theirs } = *input;
    // Rules 1 and 2.
    if ours == base {
        return take(theirs, ContentSource::Theirs);
    }
    if theirs == base || ours == theirs {
        return take(ours, ContentSource::Ours);
    }
    let (o, t) = match (base, ours, theirs) {
        // Rule 3: both added (both present, since ours != theirs and neither equals absent).
        (None, Some(o), Some(t)) => return add_add(o, t, opts),
        // Rule 4: one side deleted, the other modified.
        (Some(_), None, _) | (Some(_), _, None) => {
            return PathOutcome::Conflict(PathConflict::bare(ConflictKind::ModifyDelete));
        }
        (Some(_), Some(o), Some(t)) => (o, t),
        (None, _, _) => unreachable!("rules 1-2 cover a single added side"),
    };
    let b = base.expect("base present past rule 4");
    // Rule 5.
    let sides = [b, o, t];
    if sides.iter().any(|s| s.mode == Mode::Symlink) {
        return PathOutcome::Conflict(PathConflict::bare(ConflictKind::Symlink));
    }
    if sides.iter().any(|s| s.mode == Mode::Gitlink) {
        return PathOutcome::Conflict(PathConflict::bare(ConflictKind::Binary));
    }
    // Rules 6-8 on content.
    let content = if o.content == b.content {
        Ok(ContentSource::Theirs)
    } else if t.content == b.content || o.content == t.content {
        Ok(ContentSource::Ours)
    } else {
        merge_content(b.content, o.content, t.content, opts)
    };
    let content = match content {
        Ok(c) => c,
        Err(conflict) => return PathOutcome::Conflict(*conflict),
    };
    // Modes by rules 1-2.
    let mode = if o.mode == b.mode {
        t.mode
    } else if t.mode == b.mode || o.mode == t.mode {
        o.mode
    } else {
        let mut conflict = PathConflict::bare(ConflictKind::Mode);
        conflict.content = Some(content);
        return PathOutcome::Conflict(conflict);
    };
    PathOutcome::Clean(Resolution::Entry { mode, content })
}

/// Rules 6–8 for contents that all differ.
fn merge_content<'a>(
    b: Content<'a>,
    o: Content<'a>,
    t: Content<'a>,
    opts: &MergeOptions,
) -> Result<ContentSource, Box<PathConflict<'a>>> {
    let readable = [b, o, t];
    if readable
        .iter()
        .filter_map(|c| c.bytes())
        .any(|x| is_binary(x, opts.max_text_bytes))
    {
        return Err(Box::new(PathConflict::bare(ConflictKind::Binary)));
    }
    let (Some(bb), Some(ob), Some(tb)) = (b.bytes(), o.bytes(), t.bytes()) else {
        return Err(Box::new(PathConflict::bare(ConflictKind::Redacted)));
    };
    let lines = merge_lines(bb, ob, tb);
    match lines.merged() {
        Some(bytes) => Ok(ContentSource::Merged(bytes)),
        None => Err(Box::new(PathConflict::with_lines(
            ConflictKind::Content,
            lines,
        ))),
    }
}

/// Rule 3: independent additions.
fn add_add<'a>(o: Side<'a>, t: Side<'a>, opts: &MergeOptions) -> PathOutcome<'a> {
    if o.content == t.content {
        // Same content, so the modes differ.
        let mut conflict = PathConflict::bare(ConflictKind::Mode);
        conflict.content = Some(ContentSource::Ours);
        return PathOutcome::Conflict(conflict);
    }
    let text = |s: &Side<'a>| {
        matches!(s.mode, Mode::File | Mode::Executable)
            && s.content
                .bytes()
                .is_some_and(|b| !is_binary(b, opts.max_text_bytes))
    };
    if text(&o) && text(&t) {
        let lines = merge_two_way(
            o.content.bytes().expect("text side is readable"),
            t.content.bytes().expect("text side is readable"),
        );
        return PathOutcome::Conflict(PathConflict::with_lines(ConflictKind::AddAdd, lines));
    }
    PathOutcome::Conflict(PathConflict::bare(ConflictKind::AddAdd))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> MergeOptions {
        MergeOptions::default()
    }

    fn kind(input: PathMergeInput<'_>) -> Option<ConflictKind> {
        match merge_path(&input, &opts()) {
            PathOutcome::Clean(_) => None,
            PathOutcome::Conflict(c) => Some(c.kind),
        }
    }

    #[test]
    fn binary_boundary() {
        let mut v = vec![b'a'; 9000];
        v[7999] = 0;
        assert!(is_binary(&v, DEFAULT_MAX_TEXT_BYTES));
        v[7999] = b'a';
        v[8000] = 0;
        assert!(!is_binary(&v, DEFAULT_MAX_TEXT_BYTES));
        assert!(is_binary(&[b'a'; 11], 10));
        assert!(!is_binary(&[b'a'; 10], 10));
    }

    #[test]
    fn rule_order() {
        let b = Side::file(b"b\n");
        let o = Side::file(b"o\n");
        let t = Side::file(b"t\n");
        let input = |base, ours, theirs| PathMergeInput { base, ours, theirs };
        assert_eq!(
            kind(input(None, Some(o), Some(t))),
            Some(ConflictKind::AddAdd)
        );
        assert_eq!(
            kind(input(Some(b), None, Some(t))),
            Some(ConflictKind::ModifyDelete)
        );
        assert_eq!(kind(input(Some(b), None, None)), None);
        let link = Side::new(Mode::Symlink, b"target");
        assert_eq!(
            kind(input(Some(b), Some(link), Some(t))),
            Some(ConflictKind::Symlink)
        );
        let bin = Side::file(b"\0x");
        assert_eq!(
            kind(input(Some(b), Some(bin), Some(t))),
            Some(ConflictKind::Binary)
        );
        let red = Side {
            mode: Mode::File,
            content: Content::Redacted(b"id"),
        };
        assert_eq!(
            kind(input(Some(b), Some(red), Some(t))),
            Some(ConflictKind::Redacted)
        );
        assert_eq!(
            kind(input(Some(b), Some(o), Some(t))),
            Some(ConflictKind::Content)
        );
    }

    #[test]
    fn redacted_sides_merge_when_not_needed() {
        let red = Side {
            mode: Mode::File,
            content: Content::Redacted(b"id"),
        };
        let exec_red = Side {
            mode: Mode::Executable,
            content: Content::Redacted(b"id"),
        };
        let t = Side::file(b"t\n");
        // Ours only changed the mode; theirs changed the content.
        let out = merge_path(
            &PathMergeInput {
                base: Some(red),
                ours: Some(exec_red),
                theirs: Some(t),
            },
            &opts(),
        );
        assert_eq!(
            out,
            PathOutcome::Clean(Resolution::Entry {
                mode: Mode::Executable,
                content: ContentSource::Theirs
            })
        );
    }

    #[test]
    fn mode_conflict_keeps_merged_content() {
        let b = Side::new(Mode::File, b"a\n");
        let o = Side::new(Mode::Executable, b"a\n");
        let t = Side::new(Mode::Symlink, b"a\n");
        assert_eq!(
            kind(PathMergeInput {
                base: Some(b),
                ours: Some(o),
                theirs: Some(t)
            }),
            Some(ConflictKind::Symlink)
        );
        let b = Side::new(Mode::Executable, b"a\nb\nc\n");
        let o = Side::new(Mode::File, b"A\nb\nc\n");
        let t = Side::new(Mode::File, b"a\nb\nC\n");
        let out = merge_path(
            &PathMergeInput {
                base: Some(b),
                ours: Some(o),
                theirs: Some(t),
            },
            &opts(),
        );
        assert_eq!(
            out,
            PathOutcome::Clean(Resolution::Entry {
                mode: Mode::File,
                content: ContentSource::Merged(b"A\nb\nC\n".to_vec())
            })
        );
    }

    #[test]
    fn add_add_two_way() {
        let o = Side::file(b"a\nb\nc\n");
        let t = Side::file(b"a\nX\nc\n");
        match merge_path(
            &PathMergeInput {
                base: None,
                ours: Some(o),
                theirs: Some(t),
            },
            &opts(),
        ) {
            PathOutcome::Conflict(c) => {
                assert_eq!(c.kind, ConflictKind::AddAdd);
                assert_eq!(c.hunks.len(), 1);
            }
            other => panic!("unexpected {other:?}"),
        }
        // Identical adds merge.
        assert_eq!(
            kind(PathMergeInput {
                base: None,
                ours: Some(o),
                theirs: Some(o)
            }),
            None
        );
        // Identical content, different modes.
        let x = Side::new(Mode::Executable, b"a\nb\nc\n");
        assert_eq!(
            kind(PathMergeInput {
                base: None,
                ours: Some(o),
                theirs: Some(x)
            }),
            Some(ConflictKind::Mode)
        );
    }
}
