//! `.gitattributes` parsing and matching, as far as language statistics need it (the
//! `linguist-*` attributes).
//!
//! Each file contributes rules `pattern attr...` relative to its directory. An attribute is
//! `name` (set), `-name` (unset), `!name` (unspecified) or `name=value`. A pattern without a
//! `/` matches the basename at any depth below the file's directory; with a `/` (a leading
//! one is dropped) it matches the path relative to that directory. `*` and `?` stay within
//! one component, `**` crosses components. Later rules win over earlier ones, and files in
//! deeper directories win over shallower ones. Comments start with `#`.

use std::collections::BTreeMap;

use bee_core::RepoPath;

/// An attribute's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttrValue {
    /// `name`.
    Set,
    /// `-name`.
    Unset,
    /// `name=value`.
    Value(String),
}

impl AttrValue {
    /// True for `Set` and for the values `true`/`1`; false otherwise.
    pub fn is_true(&self) -> bool {
        match self {
            AttrValue::Set => true,
            AttrValue::Unset => false,
            AttrValue::Value(v) => v == "true" || v == "1",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    anchored: bool,
    pattern: Vec<u8>,
    attrs: Vec<(String, Option<AttrValue>)>,
}

/// The rules of one `.gitattributes` file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttrFile {
    rules: Vec<Rule>,
}

impl AttrFile {
    /// Parses a file's contents.
    pub fn parse(text: &[u8]) -> AttrFile {
        let mut rules = Vec::new();
        for line in text.split(|b| *b == b'\n') {
            let line = String::from_utf8_lossy(line);
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(pat) = parts.next() else { continue };
            let anchored = pat.contains('/');
            let pattern = pat.trim_start_matches('/').as_bytes().to_vec();
            let attrs = parts
                .map(|a| {
                    if let Some(n) = a.strip_prefix('-') {
                        (n.to_string(), Some(AttrValue::Unset))
                    } else if let Some(n) = a.strip_prefix('!') {
                        (n.to_string(), None)
                    } else if let Some((n, v)) = a.split_once('=') {
                        (n.to_string(), Some(AttrValue::Value(v.to_string())))
                    } else {
                        (a.to_string(), Some(AttrValue::Set))
                    }
                })
                .collect();
            rules.push(Rule {
                anchored,
                pattern,
                attrs,
            });
        }
        AttrFile { rules }
    }
}

/// Glob match over bytes: `**` matches anything (slashes included), `*` and `?` stay within
/// a component.
pub fn glob(pattern: &[u8], text: &[u8]) -> bool {
    // Iterative backtracking over the two kinds of star.
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize, bool)> = None;
    while t < text.len() {
        if p + 1 < pattern.len() && pattern[p] == b'*' && pattern[p + 1] == b'*' {
            let mut np = p + 2;
            if np < pattern.len() && pattern[np] == b'/' {
                np += 1;
            }
            star = Some((np, t, true));
            p = np;
            continue;
        }
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p + 1, t, false));
            p += 1;
            continue;
        }
        if p < pattern.len() && (pattern[p] == text[t] || (pattern[p] == b'?' && text[t] != b'/')) {
            p += 1;
            t += 1;
            continue;
        }
        match star {
            Some((sp, st, deep)) if deep || text[st] != b'/' => {
                star = Some((sp, st + 1, deep));
                p = sp;
                t = st + 1;
            }
            _ => return false,
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// The attribute files of a tree, by directory (`None` = root).
#[derive(Clone, Debug, Default)]
pub struct Attributes {
    files: BTreeMap<Option<RepoPath>, AttrFile>,
}

impl Attributes {
    /// Adds the file found in `dir`.
    pub fn insert(&mut self, dir: Option<RepoPath>, file: AttrFile) {
        self.files.insert(dir, file);
    }

    /// True when no attribute file was found.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The attributes in effect for `path`.
    pub fn lookup(&self, path: &RepoPath) -> BTreeMap<String, AttrValue> {
        let mut out = BTreeMap::new();
        let mut dirs: Vec<Option<RepoPath>> = vec![None];
        dirs.extend(path.ancestors().into_iter().map(Some));
        for dir in dirs {
            let Some(file) = self.files.get(&dir) else {
                continue;
            };
            let rel: &[u8] = match &dir {
                None => path.as_bytes(),
                Some(d) => &path.as_bytes()[d.as_bytes().len() + 1..],
            };
            for rule in &file.rules {
                let hit = if rule.anchored {
                    glob(&rule.pattern, rel)
                } else {
                    glob(&rule.pattern, path.file_name())
                };
                if !hit {
                    continue;
                }
                for (name, value) in &rule.attrs {
                    match value {
                        Some(v) => {
                            out.insert(name.clone(), v.clone());
                        }
                        None => {
                            out.remove(name);
                        }
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s.as_bytes()).unwrap()
    }

    #[test]
    fn globs() {
        assert!(glob(b"*.rs", b"main.rs"));
        assert!(!glob(b"*.rs", b"src/main.rs"));
        assert!(glob(b"src/**", b"src/a/b.rs"));
        assert!(glob(b"**/gen/*.c", b"a/b/gen/x.c"));
        assert!(glob(b"**/gen/*.c", b"gen/x.c"));
        assert!(glob(b"a?c", b"abc"));
        assert!(!glob(b"a?c", b"a/c"));
        assert!(glob(b"vendor/*", b"vendor/x"));
        assert!(!glob(b"vendor/*", b"vendor/x/y"));
    }

    #[test]
    fn lookup_layers_files_and_rules() {
        let mut a = Attributes::default();
        a.insert(
            None,
            AttrFile::parse(b"# top\n*.js linguist-vendored\nlib/** linguist-language=Rust\n"),
        );
        a.insert(
            Some(p("lib")),
            AttrFile::parse(b"keep.js -linguist-vendored\nx.js !linguist-vendored\n"),
        );
        assert_eq!(a.lookup(&p("a.js"))["linguist-vendored"], AttrValue::Set);
        assert_eq!(
            a.lookup(&p("lib/keep.js"))["linguist-vendored"],
            AttrValue::Unset
        );
        assert!(!a.lookup(&p("lib/x.js")).contains_key("linguist-vendored"));
        assert_eq!(
            a.lookup(&p("lib/q.txt"))["linguist-language"],
            AttrValue::Value("Rust".into())
        );
        assert!(AttrValue::Value("true".into()).is_true());
        assert!(!AttrValue::Unset.is_true());
        assert!(!a.is_empty());
    }
}
