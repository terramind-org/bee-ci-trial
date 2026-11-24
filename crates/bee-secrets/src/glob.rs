//! A small path glob matcher for allow-lists.

/// True when `path` matches `pattern`.
///
/// * `*` matches any run of bytes except `/`; `?` matches one byte except `/`.
/// * `**` matches any run of bytes including `/`; `**/` also matches zero directories.
/// * A pattern without `/` matches the final path component at any depth (`*.md`).
/// * A leading `/` anchors the pattern at the repository root; a trailing `/` matches everything
///   below that directory.
pub(crate) fn glob_match(pattern: &str, path: &str) -> bool {
    let path = path.trim_start_matches('/');
    let mut pat = pattern.to_owned();
    if pat.ends_with('/') {
        pat.push_str("**");
    }
    if let Some(rest) = pat.strip_prefix('/') {
        return matches(rest.as_bytes(), path.as_bytes());
    }
    if !pat.contains('/') {
        let base = path.rsplit('/').next().unwrap_or(path);
        return matches(pat.as_bytes(), base.as_bytes());
    }
    matches(pat.as_bytes(), path.as_bytes())
}

fn matches(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') if p.starts_with(b"**/") => {
            let rest = &p[3..];
            matches(rest, s)
                || s.iter()
                    .enumerate()
                    .any(|(i, &b)| b == b'/' && matches(rest, &s[i + 1..]))
        }
        Some(b'*') if p.starts_with(b"**") => {
            let rest = &p[2..];
            (0..=s.len()).any(|i| matches(rest, &s[i..]))
        }
        Some(b'*') => {
            let rest = &p[1..];
            for i in 0..=s.len() {
                if matches(rest, &s[i..]) {
                    return true;
                }
                if s.get(i) == Some(&b'/') || i == s.len() {
                    break;
                }
            }
            false
        }
        Some(b'?') => matches!(s.first(), Some(&b) if b != b'/') && matches(&p[1..], &s[1..]),
        Some(&c) => s.first() == Some(&c) && matches(&p[1..], &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn basename_patterns_match_at_any_depth() {
        assert!(glob_match("*.md", "docs/a/readme.md"));
        assert!(glob_match("*.md", "readme.md"));
        assert!(!glob_match("*.md", "readme.mdx"));
    }

    #[test]
    fn slash_patterns_match_the_whole_path() {
        assert!(glob_match("tests/fixtures/*", "tests/fixtures/a.txt"));
        assert!(!glob_match("tests/fixtures/*", "tests/fixtures/deep/a.txt"));
        assert!(glob_match("tests/**", "tests/fixtures/deep/a.txt"));
        assert!(glob_match("**/fixtures/*.pem", "a/b/fixtures/k.pem"));
        assert!(glob_match("**/fixtures/*.pem", "fixtures/k.pem"));
    }

    #[test]
    fn anchors_and_directories() {
        assert!(glob_match("/vendor/", "vendor/x/y.rs"));
        assert!(!glob_match("/vendor/", "src/vendor/y.rs"));
        assert!(glob_match("a?c.txt", "abc.txt"));
        assert!(!glob_match("a?c.txt", "a/c.txt"));
    }
}
