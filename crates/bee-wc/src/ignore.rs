//! `.gitignore` semantics for untracked files: nested files, negation, anchoring, directory-only
//! patterns, `*`, `?`, `[...]` and `**`. Tracked paths are never ignored.
//!
//! Rules are kept in walk order (a directory's `.gitignore` after its parent's); the last rule
//! that matches a path decides. A directory that is ignored is not descended into, so a file
//! under it cannot be re-included, as in git.

/// One pattern line.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    /// Directory holding the `.gitignore`, as a repo path prefix (`""` for the root).
    base: Vec<u8>,
    /// The glob, without leading `/`, trailing `/` or `!`.
    glob: Vec<u8>,
    /// `!pattern`: re-include.
    negate: bool,
    /// `pattern/`: matches directories only.
    dir_only: bool,
    /// The pattern has a `/` before its end: match the whole path relative to `base`.
    anchored: bool,
}

/// The ignore rules collected so far.
#[derive(Clone, Debug, Default)]
pub struct Ignore {
    rules: Vec<Rule>,
}

impl Ignore {
    /// No rules.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the lines of the `.gitignore` found in directory `base` (`""` for the root).
    pub fn add_file(&mut self, base: &[u8], text: &[u8]) {
        for raw in text.split(|&b| b == b'\n') {
            let mut line = raw.strip_suffix(b"\r").unwrap_or(raw).to_vec();
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            // Trailing spaces are ignored unless escaped with a backslash.
            while line.last() == Some(&b' ') && !line.ends_with(b"\\ ") {
                line.pop();
            }
            if line.ends_with(b"\\ ") {
                let n = line.len();
                line.remove(n - 2);
            }
            if line.is_empty() {
                continue;
            }
            let mut negate = false;
            if line[0] == b'!' {
                negate = true;
                line.remove(0);
            } else if line.starts_with(b"\\!") || line.starts_with(b"\\#") {
                line.remove(0);
            }
            let mut dir_only = false;
            if line.last() == Some(&b'/') {
                dir_only = true;
                line.pop();
            }
            let anchored = line.contains(&b'/');
            if line.first() == Some(&b'/') {
                line.remove(0);
            }
            if line.is_empty() {
                continue;
            }
            self.rules.push(Rule {
                base: base.to_vec(),
                glob: line,
                negate,
                dir_only,
                anchored,
            });
        }
    }

    /// True if an untracked `path` (a directory when `is_dir`) is ignored.
    pub fn is_ignored(&self, path: &[u8], is_dir: bool) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            let rel = if rule.base.is_empty() {
                path
            } else if path.len() > rule.base.len()
                && path.starts_with(&rule.base)
                && path[rule.base.len()] == b'/'
            {
                &path[rule.base.len() + 1..]
            } else {
                continue;
            };
            let subject = if rule.anchored {
                rel
            } else {
                rel.rsplit(|&b| b == b'/').next().unwrap_or(rel)
            };
            if glob_match(&rule.glob, subject) {
                ignored = !rule.negate;
            }
        }
        ignored
    }
}

/// Matches a gitignore glob against a `/`-separated subject.
pub fn glob_match(p: &[u8], s: &[u8]) -> bool {
    if p.starts_with(b"**/") {
        let rest = &p[3..];
        if glob_match(rest, s) {
            return true;
        }
        return s
            .iter()
            .enumerate()
            .any(|(i, &b)| b == b'/' && glob_match(rest, &s[i + 1..]));
    }
    if p == b"**" {
        return true;
    }
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => {
            let rest = if p.starts_with(b"**") {
                &p[2..]
            } else {
                &p[1..]
            };
            let mut i = 0;
            loop {
                if glob_match(rest, &s[i..]) {
                    return true;
                }
                if i >= s.len() || s[i] == b'/' {
                    return false;
                }
                i += 1;
            }
        }
        Some(b'?') => !s.is_empty() && s[0] != b'/' && glob_match(&p[1..], &s[1..]),
        Some(b'[') => match class_match(p, s.first().copied()) {
            Some((true, used)) => glob_match(&p[used..], &s[1..]),
            Some((false, _)) => false,
            // An unterminated `[` is a literal.
            None => s.first() == Some(&b'[') && glob_match(&p[1..], &s[1..]),
        },
        Some(b'\\') if p.len() > 1 => s.first() == Some(&p[1]) && glob_match(&p[2..], &s[1..]),
        Some(&c) => s.first() == Some(&c) && glob_match(&p[1..], &s[1..]),
    }
}

/// Matches `[...]` at the start of `p` against `c`: `(matched, pattern bytes used)`, or `None`
/// when the class is not terminated.
fn class_match(p: &[u8], c: Option<u8>) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some(b'!') | Some(b'^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let b = *p.get(i)?;
        if b == b']' && !first {
            break;
        }
        first = false;
        let lo = b;
        if p.get(i + 1) == Some(&b'-') && p.get(i + 2).is_some_and(|&x| x != b']') {
            let hi = p[i + 2];
            if let Some(c) = c
                && lo <= c
                && c <= hi
            {
                matched = true;
            }
            i += 3;
        } else {
            if c == Some(lo) {
                matched = true;
            }
            i += 1;
        }
    }
    let c = c?;
    if c == b'/' {
        return Some((false, i + 1));
    }
    Some((matched != negate, i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match(b"*.log", b"a.log"));
        assert!(!glob_match(b"*.log", b"dir/a.log"));
        assert!(glob_match(b"a/**/b", b"a/b"));
        assert!(glob_match(b"a/**/b", b"a/x/y/b"));
        assert!(glob_match(b"**/b", b"x/b"));
        assert!(glob_match(b"a/**", b"a/x/y"));
        assert!(glob_match(b"fo?", b"foo"));
        assert!(!glob_match(b"fo?", b"fo/"));
        assert!(glob_match(b"[a-c]x", b"bx"));
        assert!(!glob_match(b"[!a-c]x", b"bx"));
        assert!(glob_match(b"[!a-c]x", b"dx"));
        assert!(glob_match(b"\\*", b"*"));
        assert!(glob_match(b"[ab", b"[ab"));
    }

    #[test]
    fn nested_files_negation_and_anchoring() {
        let mut ig = Ignore::new();
        ig.add_file(b"", b"# comment\n*.log\n!keep.log\n/build\ncache/\n");
        ig.add_file(b"sub", b"*.tmp\n!important.log\nlocal/only\n");
        assert!(ig.is_ignored(b"a.log", false));
        assert!(ig.is_ignored(b"deep/x/a.log", false));
        assert!(!ig.is_ignored(b"keep.log", false));
        assert!(ig.is_ignored(b"build", true));
        assert!(!ig.is_ignored(b"src/build", true), "anchored to the root");
        assert!(ig.is_ignored(b"x/cache", true));
        assert!(!ig.is_ignored(b"x/cache", false), "directory-only");
        assert!(ig.is_ignored(b"sub/a.tmp", false));
        assert!(!ig.is_ignored(b"other/a.tmp", false), "scoped to sub/");
        assert!(
            !ig.is_ignored(b"sub/important.log", false),
            "deeper file wins"
        );
        assert!(ig.is_ignored(b"sub/local/only", false));
        assert!(!ig.is_ignored(b"local/only", false));
    }

    #[test]
    fn whitespace_and_escapes() {
        let mut ig = Ignore::new();
        ig.add_file(b"", b"trail   \n\\#hash\n\\!bang\nsp\\ \r\n");
        assert!(ig.is_ignored(b"trail", false));
        assert!(ig.is_ignored(b"#hash", false));
        assert!(ig.is_ignored(b"!bang", false));
        assert!(ig.is_ignored(b"sp ", false));
    }
}
