//! Regex → trigram query planning.
//!
//! [`plan_regex`] turns a pattern into a [`TrigramQuery`]: a boolean formula over trigrams
//! that every file containing a match must satisfy. The planner is *sound*, never complete:
//! it may let through files that do not match (the verifier runs the real regex on every
//! candidate), but it never rejects a file that does. Whenever it would have to reason about
//! something it cannot bound (large classes, unbounded repetition, sets that grow past
//! [`MAX_SET`]) it gives up information, which only widens the candidate set.
//!
//! The analysis follows the classic approach for trigram regex search: for each
//! sub-expression it computes either the exact set of strings it can match, or a query plus
//! sets of possible prefixes and suffixes, and combines these bottom-up. Strings are folded
//! to ASCII lower case to agree with the index ([`crate::trigram`]).

use std::collections::BTreeSet;

use regex_syntax::hir::{Class, Hir, HirKind};

use crate::trigram::{Trigram, fold};

/// Largest string set the planner tracks before generalizing.
pub const MAX_SET: usize = 64;
/// Longest prefix or suffix the planner keeps per string.
const MAX_AFFIX: usize = 16;
/// Largest character class expanded into its members.
const MAX_CLASS: u32 = 16;

/// A boolean formula over trigrams.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrigramQuery {
    /// Every file.
    All,
    /// No file.
    None,
    /// Files containing the trigram.
    Tri(Trigram),
    /// All sub-queries hold.
    And(Vec<TrigramQuery>),
    /// Some sub-query holds.
    Or(Vec<TrigramQuery>),
}

impl TrigramQuery {
    /// Conjunction, simplified.
    pub fn and(parts: Vec<TrigramQuery>) -> TrigramQuery {
        let mut out = BTreeSet::new();
        for p in parts {
            match p {
                TrigramQuery::All => {}
                TrigramQuery::None => return TrigramQuery::None,
                TrigramQuery::And(inner) => out.extend(inner),
                other => {
                    out.insert(other);
                }
            }
        }
        match out.len() {
            0 => TrigramQuery::All,
            1 => out.into_iter().next().unwrap_or(TrigramQuery::All),
            _ => TrigramQuery::And(out.into_iter().collect()),
        }
    }

    /// Disjunction, simplified.
    pub fn or(parts: Vec<TrigramQuery>) -> TrigramQuery {
        let mut out = BTreeSet::new();
        for p in parts {
            match p {
                TrigramQuery::All => return TrigramQuery::All,
                TrigramQuery::None => {}
                TrigramQuery::Or(inner) => out.extend(inner),
                other => {
                    out.insert(other);
                }
            }
        }
        match out.len() {
            0 => TrigramQuery::None,
            1 => out.into_iter().next().unwrap_or(TrigramQuery::None),
            _ => TrigramQuery::Or(out.into_iter().collect()),
        }
    }

    /// The conjunction of every trigram of `s` (after folding); `All` below three bytes.
    pub fn of_literal(s: &[u8]) -> TrigramQuery {
        TrigramQuery::and(
            crate::trigram::trigrams_of(s)
                .into_iter()
                .map(TrigramQuery::Tri)
                .collect(),
        )
    }

    /// Some string of the set occurs: OR over the strings of AND over their trigrams.
    fn of_set(set: &BTreeSet<Vec<u8>>) -> TrigramQuery {
        if set.is_empty() {
            return TrigramQuery::None;
        }
        TrigramQuery::or(set.iter().map(|s| TrigramQuery::of_literal(s)).collect())
    }

    /// Evaluates the formula against a trigram membership test (for files in memory).
    pub fn matches(&self, has: &dyn Fn(Trigram) -> bool) -> bool {
        match self {
            TrigramQuery::All => true,
            TrigramQuery::None => false,
            TrigramQuery::Tri(t) => has(*t),
            TrigramQuery::And(v) => v.iter().all(|q| q.matches(has)),
            TrigramQuery::Or(v) => v.iter().any(|q| q.matches(has)),
        }
    }
}

/// Plans a regex. `case_insensitive` must match how the verifier compiles the pattern.
pub fn plan_regex(
    pattern: &str,
    case_insensitive: bool,
) -> std::result::Result<TrigramQuery, Box<regex_syntax::Error>> {
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .utf8(false)
        .build()
        .parse(pattern)
        .map_err(Box::new)?;
    Ok(plan_hir(&hir))
}

/// Plans an already-parsed HIR.
pub fn plan_hir(hir: &Hir) -> TrigramQuery {
    analyze(hir).into_query()
}

type Set = BTreeSet<Vec<u8>>;

/// What the planner knows about a sub-expression.
#[derive(Clone, Debug)]
enum Info {
    /// It matches exactly one of these strings (folded).
    Exact(Set),
    /// Every match satisfies `query`, starts with one of `prefix` and ends with one of
    /// `suffix` (an empty string in a set means "no information").
    Inexact {
        query: TrigramQuery,
        prefix: Set,
        suffix: Set,
    },
}

fn anything() -> Info {
    Info::Inexact {
        query: TrigramQuery::All,
        prefix: unknown(),
        suffix: unknown(),
    }
}

fn unknown() -> Set {
    let mut s = Set::new();
    s.insert(Vec::new());
    s
}

fn single(bytes: &[u8]) -> Set {
    let mut s = Set::new();
    s.insert(fold(bytes));
    s
}

/// Bounds an affix set: truncates strings and forgets the set when it is too large.
fn bound_prefix(set: Set) -> Set {
    if set.len() > MAX_SET {
        return unknown();
    }
    set.into_iter()
        .map(|mut s| {
            s.truncate(MAX_AFFIX);
            s
        })
        .collect()
}

fn bound_suffix(set: Set) -> Set {
    if set.len() > MAX_SET {
        return unknown();
    }
    set.into_iter()
        .map(|s| {
            if s.len() > MAX_AFFIX {
                s[s.len() - MAX_AFFIX..].to_vec()
            } else {
                s
            }
        })
        .collect()
}

/// The cross product `a × b`, or `None` when it would exceed [`MAX_SET`].
fn cross(a: &Set, b: &Set) -> Option<Set> {
    if a.len().saturating_mul(b.len()) > MAX_SET {
        return None;
    }
    let mut out = Set::new();
    for x in a {
        for y in b {
            let mut s = x.clone();
            s.extend_from_slice(y);
            out.insert(s);
        }
    }
    Some(out)
}

impl Info {
    /// Converts to the inexact form, folding the exact set into the query.
    fn generalize(self) -> (TrigramQuery, Set, Set) {
        match self {
            Info::Exact(set) => {
                let q = TrigramQuery::of_set(&set);
                (q, bound_prefix(set.clone()), bound_suffix(set))
            }
            Info::Inexact {
                query,
                prefix,
                suffix,
            } => (query, prefix, suffix),
        }
    }

    fn into_query(self) -> TrigramQuery {
        self.generalize().0
    }

    fn exact(set: Set) -> Info {
        if set.len() > MAX_SET {
            let q = TrigramQuery::of_set(&set);
            Info::Inexact {
                query: q,
                prefix: bound_prefix(set.clone()),
                suffix: bound_suffix(set),
            }
        } else {
            Info::Exact(set)
        }
    }
}

fn concat(a: Info, b: Info) -> Info {
    if let (Info::Exact(x), Info::Exact(y)) = (&a, &b)
        && let Some(set) = cross(x, y)
    {
        return Info::exact(set);
    }
    let a_exact = match &a {
        Info::Exact(s) => Some(s.clone()),
        Info::Inexact { .. } => None,
    };
    let b_exact = match &b {
        Info::Exact(s) => Some(s.clone()),
        Info::Inexact { .. } => None,
    };
    let (qa, pa, sa) = a.generalize();
    let (qb, pb, sb) = b.generalize();
    // Trigrams spanning the boundary: some suffix of `a` followed by some prefix of `b`.
    let straddle = match cross(&sa, &pb) {
        Some(set) => TrigramQuery::of_set(&set),
        None => TrigramQuery::All,
    };
    let prefix = match a_exact.as_ref().and_then(|x| cross(x, &pb)) {
        Some(set) => bound_prefix(set),
        None => pa,
    };
    let suffix = match b_exact.as_ref().and_then(|y| cross(&sa, y)) {
        Some(set) => bound_suffix(set),
        None => sb,
    };
    Info::Inexact {
        query: TrigramQuery::and(vec![qa, qb, straddle]),
        prefix,
        suffix,
    }
}

fn alternate(a: Info, b: Info) -> Info {
    if let (Info::Exact(x), Info::Exact(y)) = (&a, &b) {
        let mut set = x.clone();
        set.extend(y.iter().cloned());
        return Info::exact(set);
    }
    let (qa, mut pa, mut sa) = a.generalize();
    let (qb, pb, sb) = b.generalize();
    pa.extend(pb);
    sa.extend(sb);
    Info::Inexact {
        query: TrigramQuery::or(vec![qa, qb]),
        prefix: bound_prefix(pa),
        suffix: bound_suffix(sa),
    }
}

fn class_members(class: &Class) -> Option<Set> {
    let mut out = Set::new();
    match class {
        Class::Unicode(c) => {
            let mut count = 0u32;
            for r in c.ranges() {
                count = count.saturating_add(u32::from(r.end()) - u32::from(r.start()) + 1);
                if count > MAX_CLASS {
                    return None;
                }
            }
            for r in c.ranges() {
                for cp in u32::from(r.start())..=u32::from(r.end()) {
                    if let Some(ch) = char::from_u32(cp) {
                        let mut buf = [0u8; 4];
                        out.insert(fold(ch.encode_utf8(&mut buf).as_bytes()));
                    }
                }
            }
        }
        Class::Bytes(c) => {
            let mut count = 0u32;
            for r in c.ranges() {
                count = count.saturating_add(u32::from(r.end()) - u32::from(r.start()) + 1);
                if count > MAX_CLASS {
                    return None;
                }
            }
            for r in c.ranges() {
                for b in r.start()..=r.end() {
                    out.insert(vec![b.to_ascii_lowercase()]);
                }
            }
        }
    }
    Some(out)
}

fn analyze(hir: &Hir) -> Info {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::Exact(unknown()),
        HirKind::Literal(lit) => Info::Exact(single(&lit.0)),
        HirKind::Class(class) => match class_members(class) {
            Some(set) => Info::exact(set),
            None => anything(),
        },
        HirKind::Repetition(rep) => {
            if rep.max == Some(0) {
                return Info::Exact(unknown());
            }
            if rep.min == 0 {
                return anything();
            }
            let sub = analyze(&rep.sub);
            if rep.min == 1 && rep.max == Some(1) {
                return sub;
            }
            let (query, prefix, suffix) = sub.generalize();
            Info::Inexact {
                query,
                prefix,
                suffix,
            }
        }
        HirKind::Capture(cap) => analyze(&cap.sub),
        HirKind::Concat(parts) => {
            let mut acc = Info::Exact(unknown());
            for p in parts {
                acc = concat(acc, analyze(p));
            }
            acc
        }
        HirKind::Alternation(parts) => {
            let mut iter = parts.iter();
            let Some(first) = iter.next() else {
                return Info::Exact(Set::new());
            };
            let mut acc = analyze(first);
            for p in iter {
                acc = alternate(acc, analyze(p));
            }
            acc
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tri(s: &str) -> TrigramQuery {
        let b = s.as_bytes();
        TrigramQuery::Tri(Trigram::new(b[0], b[1], b[2]))
    }

    fn plan(p: &str) -> TrigramQuery {
        plan_regex(p, false).unwrap()
    }

    #[test]
    fn literal_is_conjunction() {
        assert_eq!(
            plan("abcd"),
            TrigramQuery::and(vec![tri("abc"), tri("bcd")])
        );
        assert_eq!(plan("ab"), TrigramQuery::All);
        assert_eq!(plan(""), TrigramQuery::All);
    }

    #[test]
    fn case_insensitive_folds() {
        assert_eq!(plan_regex("ABCD", true).unwrap(), plan("abcd"));
    }

    #[test]
    fn alternation_and_classes() {
        assert_eq!(
            plan("abc|xyz"),
            TrigramQuery::or(vec![tri("abc"), tri("xyz")])
        );
        assert_eq!(
            plan("ab[cd]"),
            TrigramQuery::or(vec![tri("abc"), tri("abd")])
        );
        // A wide class generalizes but keeps the literal runs around it.
        assert_eq!(
            plan(r"abc\wxyz"),
            TrigramQuery::and(vec![tri("abc"), tri("xyz")])
        );
    }

    #[test]
    fn repetition() {
        assert_eq!(plan("(abc)*"), TrigramQuery::All);
        assert_eq!(plan("(abc)+"), tri("abc"));
        assert_eq!(
            plan("abc.*def"),
            TrigramQuery::and(vec![tri("abc"), tri("def")])
        );
        assert_eq!(plan("x{0}"), TrigramQuery::All);
    }

    #[test]
    fn anchors_and_boundaries_are_transparent() {
        assert_eq!(plan(r"^\bfoo_bar\b"), plan("foo_bar"));
    }

    #[test]
    fn straddling_trigrams() {
        // `(a|b)cd` is exact: {acd, bcd}.
        assert_eq!(
            plan("(a|b)cd"),
            TrigramQuery::or(vec![tri("acd"), tri("bcd")])
        );
    }

    #[test]
    fn simplification() {
        assert_eq!(
            TrigramQuery::and(vec![TrigramQuery::All, TrigramQuery::None]),
            TrigramQuery::None
        );
        assert_eq!(
            TrigramQuery::or(vec![TrigramQuery::None, TrigramQuery::All]),
            TrigramQuery::All
        );
        assert_eq!(TrigramQuery::or(vec![]), TrigramQuery::None);
        assert!(plan("abc").matches(&|t| t == Trigram::new(b'a', b'b', b'c')));
        assert!(!plan("abd").matches(&|t| t == Trigram::new(b'a', b'b', b'c')));
    }

    #[test]
    fn huge_alternation_stays_bounded() {
        let pat: Vec<String> = (0..500).map(|i| format!("word{i:04}")).collect();
        let q = plan(&pat.join("|"));
        assert_ne!(q, TrigramQuery::None);
    }
}
