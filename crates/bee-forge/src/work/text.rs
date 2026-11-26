//! Pure text analysis of issue and comment bodies: task lists, closing keywords, issue
//! references and mentions. Code blocks and code spans never count.

/// A `#12` or `owner/repo#12` reference.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IssueReference {
    /// `owner/repo` for a cross-repo reference.
    pub repo: Option<String>,
    /// The number.
    pub number: u64,
}

/// Replaces fenced code blocks and inline code spans with spaces (newlines are kept).
pub fn strip_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let marker = trimmed
            .chars()
            .next()
            .filter(|c| (*c == '`' || *c == '~') && indent < 4);
        if let Some(c) = marker {
            let run = trimmed.chars().take_while(|x| *x == c).count();
            if run >= 3 {
                match fence {
                    None => {
                        fence = Some((c, run));
                        blank(&mut out, line);
                        continue;
                    }
                    Some((fc, fl)) if fc == c && run >= fl && trimmed[run..].trim().is_empty() => {
                        fence = None;
                        blank(&mut out, line);
                        continue;
                    }
                    _ => {}
                }
            }
        }
        if fence.is_some() {
            blank(&mut out, line);
        } else {
            out.push_str(&strip_spans(line));
        }
    }
    out
}

fn blank(out: &mut String, line: &str) {
    for ch in line.chars() {
        out.push(if ch == '\n' { '\n' } else { ' ' });
    }
}

fn strip_spans(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let run = chars[i..].iter().take_while(|c| **c == '`').count();
        let mut j = i + run;
        let mut close = None;
        while j < chars.len() {
            if chars[j] == '`' {
                let r = chars[j..].iter().take_while(|c| **c == '`').count();
                if r == run {
                    close = Some(j + r);
                    break;
                }
                j += r;
            } else {
                j += 1;
            }
        }
        match close {
            Some(end) => {
                for _ in i..end {
                    out.push(' ');
                }
                i = end;
            }
            None => {
                for _ in 0..run {
                    out.push('`');
                }
                i += run;
            }
        }
    }
    out
}

/// Checkbox counts of the Markdown task lists in `body`: `(total, completed)`.
pub fn task_list_progress(body: &str) -> (u32, u32) {
    let (mut total, mut done) = (0u32, 0u32);
    for line in strip_code(body).lines() {
        let t = line.trim_start();
        let rest = if let Some(r) = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("+ "))
        {
            r
        } else {
            let digits = t.chars().take_while(char::is_ascii_digit).count();
            if digits == 0 || digits > 9 {
                continue;
            }
            match t[digits..]
                .strip_prefix(". ")
                .or_else(|| t[digits..].strip_prefix(") "))
            {
                Some(r) => r,
                None => continue,
            }
        };
        let rest = rest.trim_start();
        let (checked, text) = if let Some(r) = rest.strip_prefix("[ ]") {
            (false, r)
        } else if let Some(r) = rest
            .strip_prefix("[x]")
            .or_else(|| rest.strip_prefix("[X]"))
        {
            (true, r)
        } else {
            continue;
        };
        if text.is_empty() || !text.starts_with(char::is_whitespace) || text.trim().is_empty() {
            continue;
        }
        total += 1;
        if checked {
            done += 1;
        }
    }
    (total, done)
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn repo_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'
}

/// Parses the reference starting at `s[i]` (`#` or the first byte of `owner/repo#`); returns
/// the reference and the index after it.
fn reference_at(s: &[u8], i: usize) -> Option<(IssueReference, usize)> {
    let (repo, hash) = if s[i] == b'#' {
        (None, i)
    } else {
        let mut j = i;
        while j < s.len() && repo_char(s[j]) {
            j += 1;
        }
        if j >= s.len() || s[j] != b'/' || j == i {
            return None;
        }
        let owner = std::str::from_utf8(&s[i..j]).ok()?;
        let mut k = j + 1;
        while k < s.len() && repo_char(s[k]) {
            k += 1;
        }
        if k >= s.len() || s[k] != b'#' || k == j + 1 {
            return None;
        }
        let name = std::str::from_utf8(&s[j + 1..k]).ok()?;
        (Some(format!("{owner}/{name}")), k)
    };
    let mut e = hash + 1;
    while e < s.len() && s[e].is_ascii_digit() {
        e += 1;
    }
    if e == hash + 1 || e - hash > 10 {
        return None;
    }
    if e < s.len() && is_word_byte(s[e]) {
        return None;
    }
    let number: u64 = std::str::from_utf8(&s[hash + 1..e]).ok()?.parse().ok()?;
    if number == 0 {
        return None;
    }
    Some((IssueReference { repo, number }, e))
}

/// Every `#N` and `owner/repo#N` in `text`, in order of first appearance, without repeats.
pub fn references(text: &str) -> Vec<IssueReference> {
    let clean = strip_code(text);
    let s = clean.as_bytes();
    let mut out: Vec<IssueReference> = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let starts = if s[i] == b'#' {
            i == 0 || !(is_word_byte(s[i - 1]) || s[i - 1] == b'&')
        } else if repo_char(s[i]) {
            i == 0 || !(repo_char(s[i - 1]) || s[i - 1] == b'/')
        } else {
            false
        };
        if starts && let Some((r, end)) = reference_at(s, i) {
            if !out.contains(&r) {
                out.push(r);
            }
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

const CLOSING: [&str; 9] = [
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

/// References introduced by a closing keyword (`fixes #12`, `Closes: org/repo#3`).
pub fn closing_references(text: &str) -> Vec<IssueReference> {
    let clean = strip_code(text);
    let s = clean.as_bytes();
    let mut out: Vec<IssueReference> = Vec::new();
    let mut i = 0;
    while i < s.len() {
        if !s[i].is_ascii_alphabetic() || (i > 0 && is_word_byte(s[i - 1])) {
            i += 1;
            continue;
        }
        let mut e = i;
        while e < s.len() && s[e].is_ascii_alphabetic() {
            e += 1;
        }
        let word = clean[i..e].to_ascii_lowercase();
        if !CLOSING.contains(&word.as_str()) {
            i = e;
            continue;
        }
        let mut j = e;
        let mut sep = false;
        if j < s.len() && s[j] == b':' {
            j += 1;
            sep = true;
        }
        while j < s.len() && (s[j] == b' ' || s[j] == b'\t') {
            j += 1;
            sep = true;
        }
        if !sep {
            i = e;
            continue;
        }
        if j < s.len()
            && let Some((r, end)) = reference_at(s, j)
        {
            if !out.contains(&r) {
                out.push(r);
            }
            i = end;
            continue;
        }
        i = e;
    }
    out
}

/// Handles mentioned with `@handle`, lower-cased, in order of first appearance. Team mentions
/// (`@org/team`) and e-mail addresses are not user mentions.
pub fn mentions(text: &str) -> Vec<String> {
    let clean = strip_code(text);
    let s = clean.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < s.len() {
        if s[i] != b'@' || (i > 0 && (is_word_byte(s[i - 1]) || s[i - 1] == b'.')) {
            i += 1;
            continue;
        }
        let mut e = i + 1;
        while e < s.len() && (s[e].is_ascii_alphanumeric() || s[e] == b'-' || s[e] == b'_') {
            e += 1;
        }
        if e > i + 1 && !(e < s.len() && s[e] == b'/') {
            let h = clean[i + 1..e].to_ascii_lowercase();
            if !out.contains(&h) {
                out.push(h);
            }
        }
        i = e.max(i + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(repo: Option<&str>, number: u64) -> IssueReference {
        IssueReference {
            repo: repo.map(str::to_string),
            number,
        }
    }

    #[test]
    fn task_lists_count_checkboxes_outside_code() {
        let body = "- [ ] one\n- [x] two\n  * [X] nested\n1. [ ] numbered\n- [ ]\n- not a task\n\
                    ```\n- [ ] in code\n```\n- [ ]no space\n";
        assert_eq!(task_list_progress(body), (4, 2));
        assert_eq!(task_list_progress(""), (0, 0));
    }

    #[test]
    fn references_skip_code_and_words() {
        let t = "See #12, org/repo#7 and `#99` but not abc#5 or #5x.\n```\n#8\n```\nAlso #12 again";
        assert_eq!(references(t), vec![r(None, 12), r(Some("org/repo"), 7)]);
    }

    #[test]
    fn closing_keywords_need_a_reference() {
        let t = "Fixes #3, closes: org/repo#4\nresolved #5 and Fix #0\nunfixes #6\nfixes\n#7";
        assert_eq!(
            closing_references(t),
            vec![r(None, 3), r(Some("org/repo"), 4), r(None, 5)]
        );
        assert!(closing_references("```\nfixes #1\n```").is_empty());
    }

    #[test]
    fn mentions_ignore_emails_teams_and_code() {
        let t = "hi @Alice and @bob-2, mail a@b.com, team @org/core, `@code`\n@alice";
        assert_eq!(mentions(t), vec!["alice".to_string(), "bob-2".to_string()]);
    }
}
