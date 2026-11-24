//! The code search grammar: content terms (literal or `/regex/`), `path:`, `lang:`, `repo:`,
//! `org:`, `symbol:`, `content:` and the `case:` modifier.

use regex::bytes::{Regex, RegexBuilder};

use super::{Atom, Expr, LexOptions, QueryError, Span, ValueKind, quote_value};
use crate::lang::Language;

/// Compiled-program size limit for user regexes; larger ones are `TOO_LARGE`.
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// A content or symbol pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// A literal string (from a bare word or a quoted phrase).
    Literal(String),
    /// A regular expression (Rust `regex` syntax, multi-line mode).
    Regex(String),
}

impl Pattern {
    /// The pattern as a regex source.
    pub fn regex_source(&self) -> String {
        match self {
            Pattern::Literal(s) => regex::escape(s),
            Pattern::Regex(r) => r.clone(),
        }
    }

    /// Compiles for matching file contents.
    pub fn compile(&self, case_sensitive: bool) -> std::result::Result<Regex, regex::Error> {
        RegexBuilder::new(&self.regex_source())
            .case_insensitive(!case_sensitive)
            .multi_line(true)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
    }
}

/// How a `path:` value matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathPattern {
    /// No glob characters: the value occurs in the path starting at a component boundary
    /// (`path:billing/` matches `billing/x.rs` and `src/billing/x.rs`, not `rebilling/`).
    Segment(String),
    /// A glob (`*` within a component, `**` across components, `?`, `[...]`). Anchored at
    /// the repo root when it starts with `/`, otherwise it may start at any component.
    /// Always anchored at the end of the path.
    Glob(String),
    /// A regex over the whole path.
    Regex(String),
}

impl PathPattern {
    fn parse(atom: &Atom) -> std::result::Result<PathPattern, QueryError> {
        if atom.kind == ValueKind::Regex {
            return Ok(PathPattern::Regex(atom.value.clone()));
        }
        let v = &atom.value;
        if v.contains(['*', '?', '[']) {
            let mut depth = 0i32;
            for c in v.chars() {
                match c {
                    '[' => depth += 1,
                    ']' if depth > 0 => depth -= 1,
                    _ => {}
                }
            }
            if depth != 0 {
                return Err(QueryError::new(
                    "bad_value",
                    atom.value_span,
                    "unclosed [ in path glob",
                ));
            }
            Ok(PathPattern::Glob(v.clone()))
        } else {
            Ok(PathPattern::Segment(v.clone()))
        }
    }

    /// The equivalent regex source.
    pub fn regex_source(&self) -> String {
        match self {
            PathPattern::Regex(r) => r.clone(),
            PathPattern::Segment(s) => {
                let s = s.trim_start_matches('/');
                format!("(?:^|/){}", regex::escape(s))
            }
            PathPattern::Glob(g) => {
                let (anchor, body) = match g.strip_prefix('/') {
                    Some(rest) => ("^", rest),
                    None => ("(?:^|/)", g.as_str()),
                };
                let mut out = String::from(anchor);
                let chars: Vec<char> = body.chars().collect();
                let mut i = 0;
                while i < chars.len() {
                    match chars[i] {
                        '*' if chars.get(i + 1) == Some(&'*') => {
                            i += 2;
                            if chars.get(i) == Some(&'/') {
                                out.push_str("(?:.*/)?");
                                i += 1;
                            } else {
                                out.push_str(".*");
                            }
                            continue;
                        }
                        '*' => out.push_str("[^/]*"),
                        '?' => out.push_str("[^/]"),
                        '[' => {
                            out.push('[');
                            i += 1;
                            if matches!(chars.get(i), Some('!') | Some('^')) {
                                out.push('^');
                                i += 1;
                            }
                            while i < chars.len() && chars[i] != ']' {
                                let c = chars[i];
                                if c == '\\' || c == '[' || c == '&' || c == '~' {
                                    out.push('\\');
                                }
                                out.push(c);
                                i += 1;
                            }
                            out.push(']');
                        }
                        c => out.push_str(&regex::escape(&c.to_string())),
                    }
                    i += 1;
                }
                out.push('$');
                out
            }
        }
    }

    /// Compiles for matching paths.
    pub fn compile(&self, case_sensitive: bool) -> std::result::Result<Regex, regex::Error> {
        RegexBuilder::new(&self.regex_source())
            .case_insensitive(!case_sensitive)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
    }

    fn render(&self) -> String {
        match self {
            PathPattern::Segment(s) | PathPattern::Glob(s) => quote_value(s, ValueKind::Bare),
            PathPattern::Regex(r) => quote_value(r, ValueKind::Regex),
        }
    }
}

/// How a `repo:` value matches a repo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoPattern {
    /// `org/name`, case-insensitive.
    FullName(String),
    /// `name` in any org, case-insensitive.
    Name(String),
    /// A regex over `org/name`.
    Regex(String),
}

impl RepoPattern {
    /// True when the pattern selects the repo.
    pub fn matches(&self, org: &str, name: &str) -> bool {
        match self {
            RepoPattern::FullName(f) => f.eq_ignore_ascii_case(&format!("{org}/{name}")),
            RepoPattern::Name(n) => n.eq_ignore_ascii_case(name),
            RepoPattern::Regex(r) => regex::RegexBuilder::new(r)
                .size_limit(REGEX_SIZE_LIMIT)
                .build()
                .is_ok_and(|re| re.is_match(&format!("{org}/{name}"))),
        }
    }
}

/// One code search condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeTerm {
    /// The file content matches.
    Content(Pattern),
    /// The file defines a symbol whose name matches (literal: equal name).
    Symbol(Pattern),
    /// The path matches.
    Path(PathPattern),
    /// The file's detected language.
    Lang(Language),
    /// The repo.
    Repo(RepoPattern),
    /// The org, by name, case-insensitive.
    Org(String),
}

impl CodeTerm {
    /// True for terms that read file contents.
    pub fn reads_content(&self) -> bool {
        matches!(self, CodeTerm::Content(_) | CodeTerm::Symbol(_))
    }

    fn key(&self) -> &'static str {
        match self {
            CodeTerm::Content(_) => "",
            CodeTerm::Symbol(_) => "symbol",
            CodeTerm::Path(_) => "path",
            CodeTerm::Lang(_) => "lang",
            CodeTerm::Repo(_) => "repo",
            CodeTerm::Org(_) => "org",
        }
    }

    /// Canonical text of the term.
    pub fn render(&self) -> String {
        let pat = |p: &Pattern| match p {
            Pattern::Literal(s) => quote_value(s, ValueKind::Bare),
            Pattern::Regex(r) => quote_value(r, ValueKind::Regex),
        };
        let value = match self {
            CodeTerm::Content(p) => {
                let v = pat(p);
                // A literal that would read as a qualifier keeps an explicit `content:`.
                if matches!(p, Pattern::Literal(s) if is_qualifier_like(s)) {
                    return format!("content:{v}");
                }
                return v;
            }
            CodeTerm::Symbol(p) => pat(p),
            CodeTerm::Path(p) => p.render(),
            CodeTerm::Lang(l) => l.name().to_string(),
            CodeTerm::Repo(RepoPattern::FullName(s) | RepoPattern::Name(s)) => s.clone(),
            CodeTerm::Repo(RepoPattern::Regex(r)) => quote_value(r, ValueKind::Regex),
            CodeTerm::Org(o) => o.clone(),
        };
        format!("{}:{value}", self.key())
    }
}

fn is_qualifier_like(s: &str) -> bool {
    s.split_once(':')
        .is_some_and(|(k, v)| !v.is_empty() && is_code_key(&k.to_ascii_lowercase()))
}

/// The keys of the code grammar (aliases included).
pub const CODE_KEYS: &[&str] = &[
    "content", "path", "file", "lang", "language", "repo", "org", "symbol", "case",
];

fn is_code_key(k: &str) -> bool {
    CODE_KEYS.contains(&k)
}

/// A parsed term with its query-string details (mirrors `bee.v1.SearchQualifier`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualifierInfo {
    /// Canonical key, empty for free text.
    pub key: String,
    /// Value as written (quoting removed).
    pub value: String,
    /// Under an odd number of negations.
    pub negated: bool,
    /// Written as `/regex/`.
    pub regex: bool,
    /// Position in the query.
    pub span: Span,
    /// The value after resolution (e.g. a canonical language name, a resolved `@me`).
    pub resolved: String,
}

/// A parsed, validated code query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeQuery {
    /// The condition; `None` only if the query had nothing but modifiers (rejected).
    pub expr: Expr<CodeTerm>,
    /// `case:yes` was given.
    pub case_sensitive: bool,
    /// Every term, for `ParseSearchQuery`.
    pub qualifiers: Vec<QualifierInfo>,
}

impl CodeQuery {
    /// Canonical text: equivalent queries normalize identically.
    pub fn normalized(&self) -> String {
        let body = self.expr.render(&CodeTerm::render);
        if self.case_sensitive {
            format!("{body} case:yes")
        } else {
            body
        }
    }
}

fn convert(atom: &Atom) -> std::result::Result<CodeTerm, QueryError> {
    let pattern = |a: &Atom| -> std::result::Result<Pattern, QueryError> {
        if a.kind == ValueKind::Regex {
            check_regex(&a.value, a.value_span)?;
            Ok(Pattern::Regex(a.value.clone()))
        } else {
            Ok(Pattern::Literal(a.value.clone()))
        }
    };
    let empty = |a: &Atom| {
        QueryError::new(
            "bad_value",
            a.span,
            format!("{}: needs a value", a.key.as_deref().unwrap_or("")),
        )
    };
    match atom.key.as_deref() {
        None | Some("content") => {
            if atom.value.is_empty() {
                return Err(empty(atom));
            }
            pattern(atom).map(CodeTerm::Content)
        }
        Some("symbol") => {
            if atom.value.is_empty() {
                return Err(empty(atom));
            }
            pattern(atom).map(CodeTerm::Symbol)
        }
        Some("path" | "file") => {
            let p = PathPattern::parse(atom)?;
            if let PathPattern::Regex(r) = &p {
                check_regex(r, atom.value_span)?;
            }
            Ok(CodeTerm::Path(p))
        }
        Some("lang" | "language") => {
            Language::parse(&atom.value)
                .map(CodeTerm::Lang)
                .ok_or_else(|| {
                    QueryError::new(
                        "unknown_language",
                        atom.value_span,
                        format!("unknown language {:?}", atom.value),
                    )
                })
        }
        Some("repo") => {
            if atom.kind == ValueKind::Regex {
                check_regex(&atom.value, atom.value_span)?;
                Ok(CodeTerm::Repo(RepoPattern::Regex(atom.value.clone())))
            } else if atom.value.contains('/') {
                Ok(CodeTerm::Repo(RepoPattern::FullName(atom.value.clone())))
            } else {
                Ok(CodeTerm::Repo(RepoPattern::Name(atom.value.clone())))
            }
        }
        Some("org") => Ok(CodeTerm::Org(atom.value.clone())),
        Some("case") => Err(QueryError::new(
            "misplaced_qualifier",
            atom.span,
            "case: applies to the whole query; use it at the top level",
        )),
        Some(other) => Err(QueryError::new(
            "unknown_qualifier",
            atom.span,
            format!("unknown qualifier {other:?}"),
        )),
    }
}

fn check_regex(pattern: &str, span: Span) -> std::result::Result<(), QueryError> {
    match RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
    {
        Ok(_) => Ok(()),
        Err(regex::Error::CompiledTooBig(limit)) => Err(QueryError::new(
            "regex_too_large",
            span,
            format!("regex compiles to more than {limit} bytes"),
        )),
        Err(e) => Err(QueryError::new("bad_regex", span, e.to_string())),
    }
}

/// Parses and validates a code query. Errors carry positions; all are reported.
pub fn parse_code_query(query: &str) -> std::result::Result<CodeQuery, Vec<QueryError>> {
    let opts = LexOptions {
        regex: true,
        is_qualifier: &is_code_key,
        strict_keys: false,
    };
    let (expr, mut errors) = super::parse(query, &opts);
    // A `case:` that is not a top-level positive term stays in the expression, where
    // `convert` reports it as misplaced.
    let (expr, modifiers) = super::extract_modifiers(
        expr,
        &|a: &Atom| a.key.as_deref() == Some("case"),
        &mut |_| {},
    );
    let mut case_sensitive = false;
    let mut qualifiers = Vec::new();
    for (i, m) in modifiers.iter().enumerate() {
        match m.value.to_ascii_lowercase().as_str() {
            "yes" | "true" | "sensitive" => case_sensitive = true,
            "no" | "false" | "insensitive" | "auto" => case_sensitive = false,
            _ => errors.push(QueryError::new(
                "bad_value",
                m.value_span,
                "case: takes yes or no",
            )),
        }
        if i > 0 {
            errors.push(QueryError::new(
                "duplicate_qualifier",
                m.span,
                "case: given more than once",
            ));
        }
    }
    let mut infos = Vec::new();
    let expr = expr.and_then(|e| {
        e.try_map(
            &mut |a: Atom, negated: bool| {
                let term = convert(&a)?;
                let resolved = match &term {
                    CodeTerm::Lang(l) => l.name().to_string(),
                    _ => a.value.clone(),
                };
                infos.push(QualifierInfo {
                    key: match term.key() {
                        "" => String::new(),
                        k => k.to_string(),
                    },
                    value: a.value.clone(),
                    negated,
                    regex: a.kind == ValueKind::Regex,
                    span: a.span,
                    resolved,
                });
                Ok(term)
            },
            &mut errors,
        )
    });
    qualifiers.extend(infos);
    for m in &modifiers {
        qualifiers.push(QualifierInfo {
            key: "case".into(),
            value: m.value.clone(),
            negated: false,
            regex: false,
            span: m.span,
            resolved: if case_sensitive { "yes" } else { "no" }.into(),
        });
    }
    qualifiers.sort_by_key(|q| q.span);
    if errors.is_empty() && expr.is_none() {
        errors.push(QueryError::new(
            "empty_query",
            Span::new(0, query.len()),
            "the query has no search terms",
        ));
    }
    if !errors.is_empty() {
        errors.sort_by_key(|e| e.span);
        return Err(errors);
    }
    match expr {
        Some(expr) => Ok(CodeQuery {
            expr,
            case_sensitive,
            qualifiers,
        }),
        None => Err(vec![QueryError::new(
            "empty_query",
            Span::new(0, query.len()),
            "the query has no search terms",
        )]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(q: &str) -> String {
        parse_code_query(q)
            .unwrap_or_else(|e| panic!("{q}: {e:?}"))
            .normalized()
    }

    fn err(q: &str) -> Vec<&'static str> {
        parse_code_query(q)
            .unwrap_err()
            .into_iter()
            .map(|e| e.subcode)
            .collect()
    }

    #[test]
    fn normalizes() {
        assert_eq!(norm("foo"), "foo");
        assert_eq!(norm("language:RS file:src/"), "lang:rust path:src/");
        assert_eq!(norm("CASE:yes Foo"), "Foo case:yes");
        assert_eq!(norm("\"a b\" OR /x+y/"), "\"a b\" OR /x+y/");
        assert_eq!(norm("content:path:x"), "content:path:x");
        assert_eq!(
            norm("repo:acme/api org:acme -symbol:Foo"),
            "repo:acme/api org:acme -symbol:Foo"
        );
    }

    #[test]
    fn errors() {
        assert_eq!(err("lang:cobol"), vec!["unknown_language"]);
        assert_eq!(err("/a(/"), vec!["bad_regex"]);
        assert_eq!(err("case:yes"), vec!["empty_query"]);
        assert_eq!(err("(a OR case:yes)"), vec!["misplaced_qualifier"]);
        assert_eq!(err("a case:maybe"), vec!["bad_value"]);
        assert_eq!(err("path:[ab"), vec!["bad_value"]);
        assert_eq!(
            err(&format!("/{}/", "a{1000}{1000}")),
            vec!["regex_too_large"]
        );
    }

    #[test]
    fn qualifier_info() {
        let q = parse_code_query("-lang:py foo case:yes").unwrap();
        assert_eq!(q.qualifiers.len(), 3);
        assert_eq!(q.qualifiers[0].key, "lang");
        assert!(q.qualifiers[0].negated);
        assert_eq!(q.qualifiers[0].resolved, "python");
        assert_eq!(q.qualifiers[1].key, "");
        assert_eq!(q.qualifiers[2].key, "case");
    }

    #[test]
    fn path_patterns() {
        let m = |pat: &str, path: &str| {
            let a = Atom {
                key: Some("path".into()),
                value: pat.into(),
                kind: ValueKind::Bare,
                span: Span::default(),
                value_span: Span::default(),
            };
            PathPattern::parse(&a)
                .unwrap()
                .compile(false)
                .unwrap()
                .is_match(path.as_bytes())
        };
        assert!(m("billing/", "billing/x.rs"));
        assert!(m("billing/", "src/billing/x.rs"));
        assert!(!m("billing/", "src/rebilling/x.rs"));
        assert!(m("*.rs", "a/b/c.rs"));
        assert!(!m("*.rs", "a/b/c.rsx"));
        assert!(m("/src/**", "src/a/b.rs"));
        assert!(!m("/src/**", "lib/src/b.rs"));
        assert!(m("src/**/test_*.py", "x/src/a/b/test_x.py"));
        assert!(m("src/**/test_*.py", "src/test_x.py"));
        assert!(m("[ab].txt", "dir/a.txt"));
        assert!(!m("[!ab].txt", "dir/a.txt"));
        assert!(m("?.md", "x.md"));
        assert!(m("README", "docs/readme.md"));
    }

    #[test]
    fn repo_patterns() {
        assert!(RepoPattern::FullName("Acme/API".into()).matches("acme", "api"));
        assert!(RepoPattern::Name("api".into()).matches("other", "api"));
        assert!(!RepoPattern::Name("api".into()).matches("acme", "web"));
        assert!(RepoPattern::Regex("^acme/".into()).matches("acme", "web"));
    }
}
