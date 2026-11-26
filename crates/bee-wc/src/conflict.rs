//! Conflicts in the working tree: the server returns them as data (SPEC §7.3.2) and the
//! working copy renders them with the `diff3-v1` marker format (`docs/contracts/diff3-v1.md`
//! §8). In a rebase, "ours" is the side being rebased onto (the target) and "theirs" is the
//! change being moved.

use bee_core::Mode;
use bee_merge::{MarkerStyle, merge_lines, merge_two_way};

/// The kind name of a proto `ConflictKind` value.
pub fn kind_name(kind: i32) -> &'static str {
    match kind {
        1 => "content",
        2 => "add_add",
        3 => "modify_delete",
        4 => "symlink",
        5 => "binary",
        6 => "redacted",
        7 => "mode",
        8 => "file_directory",
        _ => "content",
    }
}

/// One side of a conflict: its mode and bytes, or absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideBytes {
    /// File mode.
    pub mode: Mode,
    /// Content.
    pub bytes: Vec<u8>,
}

/// What to write for a conflicting path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// Bytes and mode to write, or `None` for nothing at the path.
    pub content: Option<SideBytes>,
    /// True if the content holds conflict markers.
    pub markers: bool,
}

fn merged_mode(base: Option<&SideBytes>, ours: &SideBytes, theirs: &SideBytes) -> Mode {
    match base {
        Some(b) if b.mode == ours.mode => theirs.mode,
        _ => ours.mode,
    }
}

/// Renders one conflict for the working tree.
///
/// Text conflicts (`content`, `add_add`) become the line merge with diff3 markers; the other
/// kinds write the moved change's side ("theirs") when it exists, else the target's, and
/// `file_directory` writes nothing. `bee resolve --ours/--theirs/--base` picks a side.
pub fn render(
    kind: &str,
    base: Option<&SideBytes>,
    ours: Option<&SideBytes>,
    theirs: Option<&SideBytes>,
) -> Rendered {
    match (kind, ours, theirs) {
        ("content", Some(o), Some(t)) | ("add_add", Some(o), Some(t)) => {
            let merge = match base {
                Some(b) if kind == "content" => merge_lines(&b.bytes, &o.bytes, &t.bytes),
                _ => merge_two_way(&o.bytes, &t.bytes),
            };
            Rendered {
                markers: !merge.is_clean(),
                content: Some(SideBytes {
                    mode: merged_mode(base, o, t),
                    bytes: merge.render(MarkerStyle::Diff3),
                }),
            }
        }
        ("file_directory", _, _) => Rendered {
            content: None,
            markers: false,
        },
        _ => Rendered {
            content: theirs.or(ours).cloned(),
            markers: false,
        },
    }
}

/// True if `bytes` still holds a conflict block (an `<<<<<<< ours` or `>>>>>>> theirs` line).
pub fn has_markers(bytes: &[u8]) -> bool {
    bytes.split(|&b| b == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        line == b"<<<<<<< ours" || line == b">>>>>>> theirs"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(text: &str) -> SideBytes {
        SideBytes {
            mode: Mode::File,
            bytes: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn content_conflict_renders_diff3_markers_and_roundtrips() {
        let base = side("one\ntwo\nthree\n");
        let ours = side("one\nTWO-upstream\nthree\n");
        let theirs = side("one\nTWO-mine\nthree\n");
        let r = render("content", Some(&base), Some(&ours), Some(&theirs));
        assert!(r.markers);
        let text = String::from_utf8(r.content.expect("content").bytes).expect("utf8");
        assert_eq!(
            text,
            "one\n<<<<<<< ours\nTWO-upstream\n||||||| base\ntwo\n=======\nTWO-mine\n>>>>>>> theirs\nthree\n"
        );
        assert!(has_markers(text.as_bytes()));
        let resolved = text.replace(
            "<<<<<<< ours\nTWO-upstream\n||||||| base\ntwo\n=======\nTWO-mine\n>>>>>>> theirs\n",
            "TWO-both\n",
        );
        assert!(!has_markers(resolved.as_bytes()));
    }

    #[test]
    fn add_add_merges_two_way_and_others_pick_a_side() {
        let r = render("add_add", None, Some(&side("a\n")), Some(&side("b\n")));
        assert!(r.markers);
        let r = render(
            "modify_delete",
            Some(&side("x\n")),
            None,
            Some(&side("y\n")),
        );
        assert_eq!(r.content, Some(side("y\n")));
        assert!(!r.markers);
        let r = render(
            "modify_delete",
            Some(&side("x\n")),
            Some(&side("z\n")),
            None,
        );
        assert_eq!(r.content, Some(side("z\n")));
        let r = render("file_directory", None, Some(&side("x")), None);
        assert_eq!(r.content, None);
        assert!(!has_markers(b"title\n=======\n"));
        assert_eq!(kind_name(3), "modify_delete");
    }
}
