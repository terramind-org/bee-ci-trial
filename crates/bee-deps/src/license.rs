//! SPDX license expressions: parsing, canonical rendering and allow/deny evaluation.
//!
//! The grammar is the SPDX one: `AND` binds tighter than `OR`, `WITH` attaches an exception to a
//! license, `+` marks "or later", parentheses group. Identifiers are checked for shape only
//! (letters, digits, `.`, `-`, `+`, and the `LicenseRef-` / `DocumentRef-` prefixes), not against
//! the SPDX license list, so a new identifier never makes a manifest unparseable.

use std::collections::BTreeSet;
use std::fmt;

use crate::error::{DepsError, Result};

/// A parsed license expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LicenseExpr {
    /// One license, possibly with a `+` suffix and a `WITH` exception, as a canonical string such
    /// as `Apache-2.0`, `GPL-2.0+` or `GPL-2.0-only WITH Classpath-exception-2.0`.
    License(String),
    /// All of the operands apply.
    And(Vec<LicenseExpr>),
    /// The licensee picks one of the operands.
    Or(Vec<LicenseExpr>),
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b':'))
        && !id.starts_with(['-', '.', '+'])
}

struct Parser {
    tokens: Vec<String>,
    pos: usize,
}

fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        match c {
            '(' | ')' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(c.to_string());
            }
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl Parser {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.pos).map(String::as_str)
    }

    fn is_kw(&self, kw: &str) -> bool {
        self.peek().is_some_and(|t| t.eq_ignore_ascii_case(kw))
    }

    fn or_expr(&mut self) -> Option<LicenseExpr> {
        let mut parts = vec![self.and_expr()?];
        while self.is_kw("OR") {
            self.pos += 1;
            parts.push(self.and_expr()?);
        }
        Some(if parts.len() == 1 {
            parts.remove(0)
        } else {
            LicenseExpr::Or(parts)
        })
    }

    fn and_expr(&mut self) -> Option<LicenseExpr> {
        let mut parts = vec![self.with_expr()?];
        while self.is_kw("AND") {
            self.pos += 1;
            parts.push(self.with_expr()?);
        }
        Some(if parts.len() == 1 {
            parts.remove(0)
        } else {
            LicenseExpr::And(parts)
        })
    }

    fn with_expr(&mut self) -> Option<LicenseExpr> {
        let base = self.primary()?;
        if self.is_kw("WITH") {
            self.pos += 1;
            let exc = self.peek()?.to_owned();
            if !valid_id(&exc) {
                return None;
            }
            self.pos += 1;
            return match base {
                LicenseExpr::License(l) => Some(LicenseExpr::License(format!("{l} WITH {exc}"))),
                _ => None,
            };
        }
        Some(base)
    }

    fn primary(&mut self) -> Option<LicenseExpr> {
        let t = self.peek()?.to_owned();
        if t == "(" {
            self.pos += 1;
            let e = self.or_expr()?;
            if self.peek() != Some(")") {
                return None;
            }
            self.pos += 1;
            return Some(e);
        }
        if t == ")"
            || ["AND", "OR", "WITH"]
                .iter()
                .any(|k| t.eq_ignore_ascii_case(k))
            || !valid_id(&t)
        {
            return None;
        }
        self.pos += 1;
        Some(LicenseExpr::License(t))
    }
}

impl LicenseExpr {
    /// Parses an SPDX license expression. Fails with `INVALID_ARGUMENT` on a syntax error.
    pub fn parse(text: &str) -> Result<LicenseExpr> {
        let mut p = Parser {
            tokens: tokenize(text),
            pos: 0,
        };
        let bad = || DepsError::invalid("", format!("invalid license expression {text:?}"));
        let e = p.or_expr().ok_or_else(bad)?;
        if p.pos != p.tokens.len() {
            return Err(bad());
        }
        Ok(e)
    }

    /// The set of license identifiers mentioned, uppercased for case-insensitive comparison and
    /// without `WITH` exceptions or `+` suffixes folded away (`GPL-2.0+` stays `GPL-2.0+`).
    pub fn ids(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut BTreeSet<String>) {
        match self {
            LicenseExpr::License(l) => {
                out.insert(l.to_ascii_uppercase());
            }
            LicenseExpr::And(v) | LicenseExpr::Or(v) => v.iter().for_each(|e| e.collect(out)),
        }
    }

    /// True when the expression can be satisfied using only licenses accepted by `ok`: `OR` needs
    /// one satisfiable operand, `AND` needs all of them.
    pub fn satisfiable(&self, ok: &impl Fn(&str) -> bool) -> bool {
        match self {
            LicenseExpr::License(l) => ok(l),
            LicenseExpr::And(v) => v.iter().all(|e| e.satisfiable(ok)),
            LicenseExpr::Or(v) => v.iter().any(|e| e.satisfiable(ok)),
        }
    }
}

impl fmt::Display for LicenseExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn go(e: &LicenseExpr, f: &mut fmt::Formatter<'_>, parent_and: bool) -> fmt::Result {
            match e {
                LicenseExpr::License(l) => f.write_str(l),
                LicenseExpr::And(v) => {
                    for (i, x) in v.iter().enumerate() {
                        if i > 0 {
                            f.write_str(" AND ")?;
                        }
                        go(x, f, true)?;
                    }
                    Ok(())
                }
                LicenseExpr::Or(v) => {
                    if parent_and {
                        f.write_str("(")?;
                    }
                    for (i, x) in v.iter().enumerate() {
                        if i > 0 {
                            f.write_str(" OR ")?;
                        }
                        go(x, f, false)?;
                    }
                    if parent_and {
                        f.write_str(")")?;
                    }
                    Ok(())
                }
            }
        }
        go(self, f, false)
    }
}

/// Combines the declared licenses of one package into one expression.
///
/// Lockfiles that list several licenses (Composer's `license` array) mean a choice, so the
/// result is the `OR` of the entries that parse, sorted and de-duplicated. Entries that are not
/// valid SPDX expressions are ignored; `None` when nothing parses.
pub fn combine_declared(list: &[String]) -> Option<LicenseExpr> {
    let mut parsed: Vec<LicenseExpr> = list
        .iter()
        .filter_map(|l| LicenseExpr::parse(l).ok())
        .collect();
    parsed.sort_by_key(ToString::to_string);
    parsed.dedup();
    match parsed.len() {
        0 => None,
        1 => parsed.pop(),
        _ => Some(LicenseExpr::Or(parsed)),
    }
}

/// Normalizes one license string from a lockfile: a valid SPDX expression is returned in
/// canonical form, anything else (`"SEE LICENSE IN LICENSE.md"`, free text) comes back `None`.
pub fn normalize(text: &str) -> Option<String> {
    LicenseExpr::parse(text.trim()).ok().map(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_and_rendering() {
        let e = LicenseExpr::parse("MIT OR Apache-2.0 AND BSD-3-Clause").unwrap();
        assert_eq!(e.to_string(), "MIT OR Apache-2.0 AND BSD-3-Clause");
        assert!(
            matches!(e, LicenseExpr::Or(ref v) if v.len() == 2 && matches!(v[1], LicenseExpr::And(_)))
        );
        let g = LicenseExpr::parse("(MIT or Apache-2.0) and ISC").unwrap();
        assert_eq!(g.to_string(), "(MIT OR Apache-2.0) AND ISC");
    }

    #[test]
    fn with_plus_and_refs() {
        let e = LicenseExpr::parse("GPL-2.0-only WITH Classpath-exception-2.0").unwrap();
        assert_eq!(e.to_string(), "GPL-2.0-only WITH Classpath-exception-2.0");
        assert!(LicenseExpr::parse("LGPL-2.1+").is_ok());
        assert!(LicenseExpr::parse("LicenseRef-Proprietary").is_ok());
    }

    #[test]
    fn rejects_syntax_errors() {
        for bad in [
            "",
            "MIT OR",
            "AND MIT",
            "(MIT",
            "MIT)",
            "MIT Apache-2.0",
            "MIT WITH",
            "SEE LICENSE IN x",
            "(A OR B) WITH C",
        ] {
            assert!(LicenseExpr::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(normalize("not a license"), None);
        assert_eq!(normalize(" mit "), Some("mit".to_owned()));
    }

    #[test]
    fn satisfiability() {
        let deny = |l: &str| !l.eq_ignore_ascii_case("GPL-3.0-only");
        assert!(
            LicenseExpr::parse("MIT OR GPL-3.0-only")
                .unwrap()
                .satisfiable(&deny)
        );
        assert!(
            !LicenseExpr::parse("MIT AND GPL-3.0-only")
                .unwrap()
                .satisfiable(&deny)
        );
        assert!(
            !LicenseExpr::parse("GPL-3.0-only")
                .unwrap()
                .satisfiable(&deny)
        );
        let ids = LicenseExpr::parse("MIT OR (Apache-2.0 AND ISC)")
            .unwrap()
            .ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains("APACHE-2.0"));
    }
}
