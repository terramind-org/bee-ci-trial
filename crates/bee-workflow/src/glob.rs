//! GitHub's filter patterns for branches, tags, paths and workflow names.
//!
//! From the "Filter pattern cheat sheet":
//!
//! * `*` matches zero or more characters, but not `/`.
//! * `**` matches zero or more of any character; `**/` also matches no directory at all, so
//!   `**/README.md` matches `README.md`.
//! * `?` matches zero or one of the preceding character; `+` one or more of it.
//! * `[...]` matches one character listed or in a range (`[0-9a-z]`).
//! * `!` at the start negates. Patterns are checked in order: a later matching negative
//!   pattern excludes, a later matching positive pattern re-includes.
//! * `\` escapes a special character.
//!
//! Matching is case-sensitive and must cover the whole name or path.

/// What a pattern element matches.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Atom {
    Lit(char),
    Class(Vec<(char, char)>),
    Star,
    DoubleStar,
    /// `**/`: any prefix that is empty or ends with `/`.
    DoubleStarSlash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quant {
    One,
    Optional,
    OneOrMore,
}

/// A compiled filter pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    source: String,
    negated: bool,
    atoms: Vec<(Atom, Quant)>,
}

impl Pattern {
    /// Compiles a pattern. Every string is a valid pattern; an unclosed `[` is literal.
    pub fn new(source: &str) -> Pattern {
        let (negated, body) = match source.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, source),
        };
        let chars: Vec<char> = body.chars().collect();
        let mut atoms: Vec<(Atom, Quant)> = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '\\' if i + 1 < chars.len() => {
                    atoms.push((Atom::Lit(chars[i + 1]), Quant::One));
                    i += 2;
                }
                '*' if chars.get(i + 1) == Some(&'*') => {
                    if chars.get(i + 2) == Some(&'/') {
                        atoms.push((Atom::DoubleStarSlash, Quant::One));
                        i += 3;
                    } else {
                        atoms.push((Atom::DoubleStar, Quant::One));
                        i += 2;
                    }
                }
                '*' => {
                    atoms.push((Atom::Star, Quant::One));
                    i += 1;
                }
                '?' | '+' => {
                    let q = if c == '?' {
                        Quant::Optional
                    } else {
                        Quant::OneOrMore
                    };
                    match atoms.last_mut() {
                        Some((Atom::Lit(_) | Atom::Class(_), quant @ Quant::One)) => {
                            *quant = q;
                        }
                        // After a wildcard the quantifier changes nothing; with nothing
                        // before it, it is a literal character.
                        Some((Atom::Star | Atom::DoubleStar | Atom::DoubleStarSlash, _)) => {}
                        _ => atoms.push((Atom::Lit(c), Quant::One)),
                    }
                    i += 1;
                }
                '[' => match parse_class(&chars, i) {
                    Some((ranges, next)) => {
                        atoms.push((Atom::Class(ranges), Quant::One));
                        i = next;
                    }
                    None => {
                        atoms.push((Atom::Lit('['), Quant::One));
                        i += 1;
                    }
                },
                _ => {
                    atoms.push((Atom::Lit(c), Quant::One));
                    i += 1;
                }
            }
        }
        Pattern {
            source: source.to_owned(),
            negated,
            atoms,
        }
    }

    /// The pattern as written.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether the pattern starts with `!`.
    pub fn is_negated(&self) -> bool {
        self.negated
    }

    /// Whether the pattern (ignoring its `!`) matches the whole of `text`.
    pub fn matches(&self, text: &str) -> bool {
        let t: Vec<char> = text.chars().collect();
        let n = self.atoms.len();
        let mut memo = vec![None; (n + 1) * (t.len() + 1)];
        self.m(0, 0, &t, &mut memo)
    }

    fn m(&self, pi: usize, ti: usize, t: &[char], memo: &mut Vec<Option<bool>>) -> bool {
        let key = pi * (t.len() + 1) + ti;
        if let Some(r) = memo[key] {
            return r;
        }
        let r = self.m_inner(pi, ti, t, memo);
        memo[key] = Some(r);
        r
    }

    fn m_inner(&self, pi: usize, ti: usize, t: &[char], memo: &mut Vec<Option<bool>>) -> bool {
        let Some((atom, quant)) = self.atoms.get(pi) else {
            return ti == t.len();
        };
        match atom {
            Atom::Star => {
                let mut k = ti;
                loop {
                    if self.m(pi + 1, k, t, memo) {
                        return true;
                    }
                    if k == t.len() || t[k] == '/' {
                        return false;
                    }
                    k += 1;
                }
            }
            Atom::DoubleStar => (ti..=t.len()).any(|k| self.m(pi + 1, k, t, memo)),
            Atom::DoubleStarSlash => {
                self.m(pi + 1, ti, t, memo)
                    || (ti + 1..=t.len()).any(|k| t[k - 1] == '/' && self.m(pi + 1, k, t, memo))
            }
            Atom::Lit(_) | Atom::Class(_) => {
                let one = |k: usize| k < t.len() && atom_matches(atom, t[k]);
                match quant {
                    Quant::One => one(ti) && self.m(pi + 1, ti + 1, t, memo),
                    Quant::Optional => {
                        self.m(pi + 1, ti, t, memo) || (one(ti) && self.m(pi + 1, ti + 1, t, memo))
                    }
                    Quant::OneOrMore => {
                        let mut k = ti;
                        while one(k) {
                            k += 1;
                            if self.m(pi + 1, k, t, memo) {
                                return true;
                            }
                        }
                        false
                    }
                }
            }
        }
    }
}

fn atom_matches(atom: &Atom, c: char) -> bool {
    match atom {
        Atom::Lit(l) => *l == c,
        Atom::Class(ranges) => ranges.iter().any(|(a, b)| (*a..=*b).contains(&c)),
        _ => false,
    }
}

fn parse_class(chars: &[char], start: usize) -> Option<(Vec<(char, char)>, usize)> {
    let mut i = start + 1;
    let mut ranges = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c == ']' {
            if ranges.is_empty() {
                return None;
            }
            return Some((ranges, i + 1));
        }
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|e| *e != ']') {
            let e = chars[i + 2];
            ranges.push((c.min(e), c.max(e)));
            i += 3;
        } else {
            ranges.push((c, c));
            i += 1;
        }
    }
    None
}

/// A compiled list of patterns evaluated in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PatternList {
    patterns: Vec<Pattern>,
}

impl PatternList {
    /// Compiles patterns in order.
    pub fn new<S: AsRef<str>>(sources: &[S]) -> PatternList {
        PatternList {
            patterns: sources.iter().map(|s| Pattern::new(s.as_ref())).collect(),
        }
    }

    /// The patterns.
    pub fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether `text` is included: the last pattern that changes the decision wins.
    pub fn matches(&self, text: &str) -> bool {
        let mut included = false;
        for p in &self.patterns {
            if p.negated {
                if included && p.matches(text) {
                    included = false;
                }
            } else if !included && p.matches(text) {
                included = true;
            }
        }
        included
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, t: &str) -> bool {
        Pattern::new(p).matches(t)
    }

    #[test]
    fn branch_and_tag_cheat_sheet_examples() {
        // "Patterns to match branches and tags" in GitHub's filter pattern cheat sheet.
        assert!(m("feature/*", "feature/my-branch"));
        assert!(!m("feature/*", "feature/beta-a/my-branch"));
        assert!(m("feature/**", "feature/beta-a/my-branch"));
        assert!(m("feature/**", "feature/mona/the/octocat"));
        assert!(m("main", "main"));
        assert!(m("*", "releases"));
        assert!(!m("*", "all/the/branches"));
        assert!(m("**", "all/the/branches"));
        assert!(m("*feature", "mona-feature"));
        assert!(m("*feature", "feature"));
        assert!(m("*feature", "ver-10-feature"));
        assert!(m("v2*", "v2"));
        assert!(m("v2*", "v2.9"));
        assert!(m("v[12].[0-9]+.[0-9]+", "v1.10.1"));
        assert!(m("v[12].[0-9]+.[0-9]+", "v2.0.0"));
        assert!(!m("v[12].[0-9]+.[0-9]+", "v3.0.0"));
        assert!(m("[CB]at", "Cat"));
        assert!(m("[1-2]00", "200"));
        assert!(!m("[1-2]00", "300"));
        assert!(m("releases/**-alpha", "releases/beta/3-alpha"));
        assert!(m("v1.*", "v1.9.1"));
    }

    #[test]
    fn path_cheat_sheet_examples() {
        // "Patterns to match file paths" in GitHub's filter pattern cheat sheet.
        assert!(m("*", "README.md"));
        assert!(!m("*", "docs/README.md"));
        assert!(m("*.jsx?", "page.js"));
        assert!(m("*.jsx?", "page.jsx"));
        assert!(!m("*.jsx?", "page.jsxx"));
        assert!(m("**", "all/the/files.md"));
        assert!(m("*.js", "app.js"));
        assert!(!m("*.js", "js/index.js"));
        assert!(m("**.js", "index.js"));
        assert!(m("**.js", "src/js/app.js"));
        assert!(m("docs/*", "docs/README.md"));
        assert!(!m("docs/*", "docs/mona/octocat.txt"));
        assert!(m("docs/**", "docs/mona/octocat.txt"));
        assert!(m("docs/**/*.md", "docs/README.md"));
        assert!(m("docs/**/*.md", "docs/a/markdown/file.md"));
        assert!(m("**/docs/**", "docs/hello.md"));
        assert!(m("**/docs/**", "space/docs/plan/space.doc"));
        assert!(m("**/README.md", "README.md"));
        assert!(m("**/README.md", "js/README.md"));
        assert!(m("**/*src/**", "a/src/app.js"));
        assert!(m("**/*src/**", "my-src/code/js/app.js"));
        assert!(m("**/*-post.md", "my-post.md"));
        assert!(m("**/*-post.md", "path/their-post.md"));
        assert!(m("**/migrate-*.sql", "db/sept/migrate-v1.sql"));
        assert!(!m("**/migrate-*.sql", "db/migrate.sql"));
    }

    #[test]
    fn ordered_negation() {
        // Cheat sheet: '*.md', '!README.md' then adding 'README*'.
        let l = PatternList::new(&["*.md", "!README.md"]);
        assert!(l.matches("hello.md"));
        assert!(!l.matches("README.md"));
        assert!(!l.matches("docs/hello.md"));
        let l = PatternList::new(&["*.md", "!README.md", "README*"]);
        assert!(l.matches("README.md"));
        assert!(l.matches("README.doc"));
        // "releases/**" then "!releases/**-alpha".
        let l = PatternList::new(&["releases/**", "!releases/**-alpha"]);
        assert!(l.matches("releases/10"));
        assert!(l.matches("releases/beta/mona"));
        assert!(!l.matches("releases/10-alpha"));
        assert!(!l.matches("releases/beta/3-alpha"));
        // Only negative patterns include nothing.
        assert!(!PatternList::new(&["!main"]).matches("dev"));
        assert!(!PatternList::new::<&str>(&[]).matches("x"));
    }

    #[test]
    fn escapes_and_literals() {
        assert!(m("a\\*b", "a*b"));
        assert!(!m("a\\*b", "axb"));
        assert!(m("[abc", "[abc"));
        assert!(m("c\\+\\+", "c++"));
        assert!(m("?x", "?x"));
        assert!(!m("main", "Main"));
    }

    #[test]
    fn pathological_patterns_stay_fast() {
        let p = Pattern::new(&"*a".repeat(30));
        assert!(!p.matches(&"a".repeat(29)));
        let p = Pattern::new(&"**/".repeat(20));
        assert!(!p.matches(&("a/".repeat(40) + "b")));
    }
}
