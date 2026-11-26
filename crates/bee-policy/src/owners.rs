//! Owners: references to owners and the `CODEOWNERS` file format.

use std::collections::BTreeSet;
use std::fmt;

use crate::error::{Location, Origin, PolicyError, PolicyErrorKind};
use crate::path::Glob;
use crate::subject::{PrincipalId, Subject};

/// One owner.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OwnerRef {
    /// A principal by id (policy files: a bare id).
    Principal(PrincipalId),
    /// A group (policy files: `@name`; `CODEOWNERS`: `@org/team`).
    Group(String),
    /// A login from `CODEOWNERS` (`@login`), matched against [`crate::Actor::aliases`].
    Login(String),
    /// An email from `CODEOWNERS`, matched against [`crate::Actor::aliases`].
    Email(String),
}

impl OwnerRef {
    /// Converts a policy-file subject.
    pub fn from_subject(s: Subject) -> OwnerRef {
        match s {
            Subject::Principal(p) => OwnerRef::Principal(p),
            Subject::Group(g) => OwnerRef::Group(g),
        }
    }
}

impl fmt::Display for OwnerRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OwnerRef::Principal(p) => f.write_str(p.as_str()),
            OwnerRef::Group(g) => write!(f, "@{g}"),
            OwnerRef::Login(l) => write!(f, "login:{l}"),
            OwnerRef::Email(e) => write!(f, "email:{e}"),
        }
    }
}

/// One `CODEOWNERS` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeownersEntry {
    /// The pattern as written.
    pub pattern: String,
    glob: Glob,
    /// Owners; empty means the matched paths have no owner.
    pub owners: BTreeSet<OwnerRef>,
    /// 1-based line number.
    pub line: u32,
}

/// A parsed `CODEOWNERS` file. The last matching line wins, as on GitHub.
///
/// Supported syntax: `#` comments, blank lines, `\#` for a literal leading `#`, a pattern
/// followed by owners (`@login`, `@org/team`, `email@domain`). Patterns: a leading `/` or a `/`
/// in the middle anchors at the root, otherwise the pattern matches at any depth; a trailing
/// `/` matches only directories; `*`, `?` and `**` as in policy globs. A pattern matches a path
/// when it matches the path or one of its ancestor directories. Negation (`!`) and character
/// classes (`[ ]`) are not supported on GitHub either and are rejected with the line number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Codeowners {
    entries: Vec<CodeownersEntry>,
}

impl Codeowners {
    /// Parses a `CODEOWNERS` file.
    pub fn parse(text: &str) -> Result<Codeowners, PolicyError> {
        let mut entries = Vec::new();
        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
            let loc = || Location {
                origin: Origin::Codeowners,
                key: String::new(),
                line: Some(line_no),
                column: Some(1),
            };
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Strip a trailing comment (a `#` preceded by whitespace).
            let line = match line.find(" #") {
                Some(pos) => line[..pos].trim_end(),
                None => line,
            };
            let mut fields = line.split_whitespace();
            let Some(first) = fields.next() else { continue };
            let pattern = first.strip_prefix('\\').unwrap_or(first).to_string();
            if pattern.starts_with('!') || pattern.contains('[') || pattern.contains(']') {
                return Err(PolicyError::new(
                    PolicyErrorKind::InvalidValue,
                    format!("unsupported CODEOWNERS pattern `{first}`"),
                    loc(),
                ));
            }
            let glob = codeowners_glob(&pattern)
                .map_err(|e| PolicyError::new(PolicyErrorKind::InvalidPattern, e, loc()))?;
            let mut owners = BTreeSet::new();
            for owner in fields {
                let parsed = if let Some(name) = owner.strip_prefix('@') {
                    if name.is_empty() {
                        None
                    } else if name.contains('/') {
                        Some(OwnerRef::Group(name.to_string()))
                    } else {
                        Some(OwnerRef::Login(name.to_string()))
                    }
                } else if owner.contains('@') && !owner.starts_with('@') && !owner.ends_with('@') {
                    Some(OwnerRef::Email(owner.to_string()))
                } else {
                    None
                };
                match parsed {
                    Some(o) => {
                        owners.insert(o);
                    }
                    None => {
                        return Err(PolicyError::new(
                            PolicyErrorKind::InvalidSubject,
                            format!(
                                "CODEOWNERS owner `{owner}` is not @login, @org/team or an email"
                            ),
                            loc(),
                        ));
                    }
                }
            }
            entries.push(CodeownersEntry {
                pattern,
                glob,
                owners,
                line: line_no,
            });
        }
        Ok(Codeowners { entries })
    }

    /// The entries in file order.
    pub fn entries(&self) -> &[CodeownersEntry] {
        &self.entries
    }

    /// The last entry matching `path`, if any.
    pub fn lookup(&self, path: &str) -> Option<&CodeownersEntry> {
        self.entries.iter().rev().find(|e| e.glob.matches(path))
    }
}

fn codeowners_glob(pattern: &str) -> Result<Glob, String> {
    let dir_only = pattern.ends_with('/') && pattern.len() > 1;
    let body = pattern.trim_end_matches('/');
    if body.is_empty() {
        // `/` or `*`-like root pattern: everything.
        return Glob::parse("**");
    }
    let anchored = body.starts_with('/') || body.trim_start_matches('/').contains('/');
    let text = if anchored || body.starts_with("**") {
        body.to_string()
    } else {
        format!("**/{body}")
    };
    if dir_only {
        Glob::parse_dir_only(&text)
    } else {
        Glob::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "\
# comment
*            @org/everyone
/docs/       @docs-team-lead docs@example.com
*.rs         @org/rust
/src/auth/   @org/security   # trailing comment
/src/auth/vendor/
build/logs   @ops
";

    #[test]
    fn last_match_wins() {
        let co = Codeowners::parse(FILE).unwrap();
        let owners = |p: &str| co.lookup(p).map(|e| e.owners.clone()).unwrap_or_default();
        assert_eq!(
            owners("README"),
            [OwnerRef::Group("org/everyone".into())].into()
        );
        assert_eq!(
            owners("src/lib.rs"),
            [OwnerRef::Group("org/rust".into())].into()
        );
        assert_eq!(
            owners("src/auth/token.rs"),
            [OwnerRef::Group("org/security".into())].into()
        );
        assert_eq!(
            owners("docs/a.md"),
            [
                OwnerRef::Login("docs-team-lead".into()),
                OwnerRef::Email("docs@example.com".into())
            ]
            .into()
        );
        // Unowned override.
        assert!(owners("src/auth/vendor/x.c").is_empty());
        // A `/` in the middle anchors: `build/logs` does not match `x/build/logs`.
        assert_eq!(
            owners("build/logs/1"),
            [OwnerRef::Login("ops".into())].into()
        );
        assert_eq!(
            owners("x/build/logs/1"),
            [OwnerRef::Group("org/everyone".into())].into()
        );
        // Directory-only: a file named `docs` is not under `/docs/`.
        assert_eq!(co.lookup("docs").unwrap().line, 2);
    }

    #[test]
    fn rejects_unsupported_syntax_with_line() {
        let err = Codeowners::parse("* @a\n!foo @b\n").unwrap_err();
        assert_eq!(err.location.line, Some(2));
        let err = Codeowners::parse("* not-an-owner\n").unwrap_err();
        assert_eq!(err.kind, PolicyErrorKind::InvalidSubject);
    }
}
