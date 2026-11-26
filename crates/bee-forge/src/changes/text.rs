//! Pure text helpers: closing keywords, carrying review anchors forward across revisions, and
//! applying suggestions. Every function here is a pure function of its inputs.

use crate::error::{ForgeError, Result};

/// GitHub's closing keywords (case-insensitive).
const CLOSING: &[&str] = &[
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

/// Issue numbers that `text` closes with a keyword: `Fixes #12`, `closes: #3`,
/// `Resolves acme/app#7` (only when `acme/app` is `repo_full_name`, case-insensitive).
/// Sorted and unique.
pub fn closing_issues(text: &str, repo_full_name: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    for w in words.windows(2) {
        let kw = w[0]
            .trim_end_matches(':')
            .trim_start_matches(|c: char| !c.is_ascii_alphabetic());
        if !CLOSING.iter().any(|k| k.eq_ignore_ascii_case(kw)) {
            continue;
        }
        let target = w[1].trim_end_matches(|c: char| !c.is_ascii_digit());
        let (repo, num) = match target.split_once('#') {
            Some(x) => x,
            None => continue,
        };
        if !repo.is_empty() && !repo.eq_ignore_ascii_case(repo_full_name) {
            continue;
        }
        if let Ok(n) = num.parse::<u64>()
            && n > 0
        {
            out.push(n);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Where the 1-based inclusive line range `start..=end` of `old` lies in `new`, if none of
/// its lines changed: `Some((start', end'))` shifted by the lines added or removed above
/// it, or `None` when any anchored line was edited or removed, or a line was inserted
/// inside the range (the anchor is then outdated). A zero `start` (file-level anchor)
/// always carries forward unchanged.
pub fn carry_range(old: &[u8], new: &[u8], start: u32, end: u32) -> Option<(u32, u32)> {
    if start == 0 {
        return Some((0, 0));
    }
    let end = end.max(start);
    let old_lines = bee_merge::split_lines(old).len();
    if end as usize > old_lines {
        return None;
    }
    let (s0, e0) = (start as usize - 1, end as usize);
    let mut shift: i64 = 0;
    for h in bee_history::diff::line_hunks(old, new, 0, false) {
        let (os, oe) = (h.old_start, h.old_start + h.old_len);
        if h.old_len == 0 {
            // Pure insertion before old line `os`.
            if os <= s0 {
                shift += h.new_len as i64;
            } else if os < e0 {
                return None;
            }
            continue;
        }
        if oe <= s0 {
            shift += h.new_len as i64 - h.old_len as i64;
        } else if os < e0 {
            return None;
        }
    }
    let ns = start as i64 + shift;
    let ne = end as i64 + shift;
    (ns >= 1).then_some((ns as u32, ne as u32))
}

/// One suggestion to apply: replace lines `start..=end` (1-based) with `replacement`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuggestionEdit {
    /// First line.
    pub start: u32,
    /// Last line.
    pub end: u32,
    /// New content without a trailing newline; empty deletes the lines.
    pub replacement: String,
}

/// Applies non-overlapping suggestions to `content`. `INVALID_ARGUMENT` /
/// `suggestion_overlap` when two ranges overlap, `suggestion_out_of_range` when a range is
/// outside the file.
pub fn apply_suggestions(content: &[u8], edits: &[SuggestionEdit]) -> Result<Vec<u8>> {
    let lines = bee_merge::split_lines(content);
    let mut sorted: Vec<&SuggestionEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| (e.start, e.end));
    for w in sorted.windows(2) {
        if w[1].start <= w[0].end.max(w[0].start) {
            return Err(ForgeError::invalid(
                "suggestion_overlap",
                format!(
                    "suggestions on lines {}-{} and {}-{} overlap",
                    w[0].start, w[0].end, w[1].start, w[1].end
                ),
            ));
        }
    }
    let mut out = Vec::with_capacity(content.len());
    let mut next = 0usize;
    for e in sorted {
        let end = e.end.max(e.start) as usize;
        if e.start == 0 || end > lines.len() {
            return Err(ForgeError::invalid(
                "suggestion_out_of_range",
                format!(
                    "lines {}-{} are outside a file of {} lines",
                    e.start,
                    e.end,
                    lines.len()
                ),
            ));
        }
        for l in &lines[next..e.start as usize - 1] {
            out.extend_from_slice(l);
        }
        if !e.replacement.is_empty() {
            out.extend_from_slice(e.replacement.as_bytes());
            let last_had_newline = lines[end - 1].ends_with(b"\n");
            if last_had_newline {
                out.push(b'\n');
            }
        }
        next = end;
    }
    for l in &lines[next..] {
        out.extend_from_slice(l);
    }
    Ok(out)
}

/// Template files read from the target ref, in lookup order: single templates first, then
/// the files of the template directories.
pub const TEMPLATE_FILES: &[&str] = &[
    ".github/pull_request_template.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
    "pull_request_template.md",
    "PULL_REQUEST_TEMPLATE.md",
    "docs/pull_request_template.md",
    "docs/PULL_REQUEST_TEMPLATE.md",
    ".beehive/change_request_template.md",
];

/// Directories whose `*.md` files are named templates.
pub const TEMPLATE_DIRS: &[&str] = &[
    ".github/PULL_REQUEST_TEMPLATE",
    ".github/pull_request_template",
    "docs/PULL_REQUEST_TEMPLATE",
    ".beehive/change_request_templates",
];

/// Agent instruction files consulted by the repo context API, relative to each directory on
/// the way from the root to a requested path.
pub const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// The template name of a template path: the file stem for directory templates, `default`
/// for single-file templates.
pub fn template_name(path: &str) -> String {
    for d in TEMPLATE_DIRS {
        if let Some(rest) = path.strip_prefix(d).and_then(|r| r.strip_prefix('/')) {
            return rest.strip_suffix(".md").unwrap_or(rest).to_string();
        }
    }
    "default".to_string()
}

/// "Revert \"<title>\"" with GitHub's phrasing.
pub fn revert_title(title: &str) -> String {
    format!("Revert \"{title}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_keywords_are_found() {
        let body = "This fixes #12 and Closes: #3.\nresolves acme/app#7, see #9; fix other/repo#5\nFIXED #12";
        assert_eq!(closing_issues(body, "acme/app"), vec![3, 7, 12]);
        assert!(closing_issues("refs #4", "acme/app").is_empty());
        assert!(closing_issues("fixes #0 fixes #x", "a/b").is_empty());
    }

    #[test]
    fn ranges_carry_forward_or_go_outdated() {
        let old = b"a\nb\nc\nd\ne\n";
        // Unchanged file.
        assert_eq!(carry_range(old, old, 2, 3), Some((2, 3)));
        // Two lines inserted above: shifted.
        assert_eq!(
            carry_range(old, b"x\ny\na\nb\nc\nd\ne\n", 2, 3),
            Some((4, 5))
        );
        // A line removed above: shifted up.
        assert_eq!(carry_range(old, b"b\nc\nd\ne\n", 2, 3), Some((1, 2)));
        // Edit below: unchanged.
        assert_eq!(carry_range(old, b"a\nb\nc\nd\nE\n", 2, 3), Some((2, 3)));
        // An anchored line edited: outdated.
        assert_eq!(carry_range(old, b"a\nB\nc\nd\ne\n", 2, 3), None);
        // A line inserted inside the range: outdated.
        assert_eq!(carry_range(old, b"a\nb\nx\nc\nd\ne\n", 2, 3), None);
        // Inserted right after the range: unchanged.
        assert_eq!(carry_range(old, b"a\nb\nc\nx\nd\ne\n", 2, 3), Some((2, 3)));
        // Anchored line deleted: outdated.
        assert_eq!(carry_range(old, b"a\nc\nd\ne\n", 2, 2), None);
        // Range past the end: outdated.
        assert_eq!(carry_range(old, old, 5, 9), None);
        // File-level anchors always carry.
        assert_eq!(carry_range(old, b"", 0, 0), Some((0, 0)));
    }

    #[test]
    fn suggestions_apply_in_batches() {
        let f = b"one\ntwo\nthree\nfour\n";
        let out = apply_suggestions(
            f,
            &[
                SuggestionEdit {
                    start: 4,
                    end: 4,
                    replacement: "FOUR".into(),
                },
                SuggestionEdit {
                    start: 1,
                    end: 2,
                    replacement: "ONE+TWO".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(out, b"ONE+TWO\nthree\nFOUR\n");
        let del = apply_suggestions(
            f,
            &[SuggestionEdit {
                start: 2,
                end: 3,
                replacement: String::new(),
            }],
        )
        .unwrap();
        assert_eq!(del, b"one\nfour\n");
        let no_nl = apply_suggestions(
            b"a\nb",
            &[SuggestionEdit {
                start: 2,
                end: 2,
                replacement: "B".into(),
            }],
        )
        .unwrap();
        assert_eq!(no_nl, b"a\nB");
        let overlap = apply_suggestions(
            f,
            &[
                SuggestionEdit {
                    start: 1,
                    end: 2,
                    replacement: "x".into(),
                },
                SuggestionEdit {
                    start: 2,
                    end: 3,
                    replacement: "y".into(),
                },
            ],
        );
        assert_eq!(overlap.unwrap_err().subcode, "suggestion_overlap");
        let out_of_range = apply_suggestions(
            f,
            &[SuggestionEdit {
                start: 4,
                end: 6,
                replacement: "x".into(),
            }],
        );
        assert_eq!(out_of_range.unwrap_err().subcode, "suggestion_out_of_range");
    }

    #[test]
    fn template_names() {
        assert_eq!(template_name(".github/pull_request_template.md"), "default");
        assert_eq!(template_name(".github/PULL_REQUEST_TEMPLATE/bug.md"), "bug");
        assert_eq!(revert_title("Add x"), "Revert \"Add x\"");
    }
}
