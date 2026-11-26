//! Ref-name patterns and their precedence.

use std::fmt;

/// A ref pattern: an exact ref name, or a prefix followed by a single trailing `*`.
///
/// `release/*` matches `release/1.0` and `release/1.0/hotfix`; `*` alone matches every ref.
/// Precedence when several patterns match one ref: an exact name beats every pattern, and among
/// patterns the longest prefix wins. Two different patterns of the same length cannot both
/// match one ref, so the choice is always unique.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RefPattern {
    /// Matches exactly this ref name.
    Exact(String),
    /// Matches every ref name starting with this prefix (the `*` is not stored).
    Prefix(String),
}

impl RefPattern {
    /// Parses a pattern. `*` may appear only once, as the last character.
    pub fn parse(raw: &str) -> Result<RefPattern, String> {
        if raw.is_empty() {
            return Err("ref pattern is empty".to_string());
        }
        if raw.chars().any(char::is_whitespace) {
            return Err(format!("ref pattern `{raw}` contains whitespace"));
        }
        let stars = raw.matches('*').count();
        match stars {
            0 => Ok(RefPattern::Exact(raw.to_string())),
            1 if raw.ends_with('*') => Ok(RefPattern::Prefix(raw[..raw.len() - 1].to_string())),
            _ => Err(format!(
                "ref pattern `{raw}`: `*` is allowed only once, at the end"
            )),
        }
    }

    /// Whether the pattern matches `name`.
    pub fn matches(&self, name: &str) -> bool {
        match self {
            RefPattern::Exact(e) => e == name,
            RefPattern::Prefix(p) => name.starts_with(p.as_str()),
        }
    }

    /// Whether every ref matched by `other` is also matched by `self`.
    pub fn covers(&self, other: &RefPattern) -> bool {
        match (self, other) {
            (RefPattern::Exact(a), RefPattern::Exact(b)) => a == b,
            (RefPattern::Exact(_), RefPattern::Prefix(_)) => false,
            (RefPattern::Prefix(p), RefPattern::Exact(b)) => b.starts_with(p.as_str()),
            (RefPattern::Prefix(p), RefPattern::Prefix(q)) => q.starts_with(p.as_str()),
        }
    }

    /// The narrower of two patterns when one covers the other, `None` when they are disjoint.
    /// This is exact set intersection, because prefix sets are either nested or disjoint.
    pub fn meet(&self, other: &RefPattern) -> Option<RefPattern> {
        if self.covers(other) {
            Some(other.clone())
        } else if other.covers(self) {
            Some(self.clone())
        } else {
            None
        }
    }

    /// Sort key for precedence: exact names first, then longer prefixes.
    fn rank(&self) -> (u8, usize) {
        match self {
            RefPattern::Exact(e) => (1, e.len()),
            RefPattern::Prefix(p) => (0, p.len()),
        }
    }
}

impl fmt::Display for RefPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefPattern::Exact(e) => f.write_str(e),
            RefPattern::Prefix(p) => write!(f, "{p}*"),
        }
    }
}

/// Picks the most specific item whose pattern matches `name`: exact beats pattern, longest
/// pattern wins.
pub fn most_specific<T>(
    items: impl IntoIterator<Item = T>,
    pattern: impl Fn(&T) -> &RefPattern,
    name: &str,
) -> Option<T> {
    items
        .into_iter()
        .filter(|item| pattern(item).matches(name))
        .max_by_key(|item| pattern(item).rank())
}

/// Intersects two pattern sets: a ref matches the result exactly when it matches some pattern
/// in `a` and some pattern in `b`. The result is sorted, deduplicated and has no pattern
/// covered by another.
pub fn intersect_patterns(a: &[RefPattern], b: &[RefPattern]) -> Vec<RefPattern> {
    let mut out: Vec<RefPattern> = a
        .iter()
        .flat_map(|x| b.iter().filter_map(move |y| x.meet(y)))
        .collect();
    reduce_patterns(&mut out);
    out
}

/// Sorts, deduplicates and drops patterns covered by another pattern in the set.
pub fn reduce_patterns(set: &mut Vec<RefPattern>) {
    set.sort();
    set.dedup();
    let snapshot = set.clone();
    set.retain(|p| !snapshot.iter().any(|q| q != p && q.covers(p)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RefPattern {
        RefPattern::parse(s).unwrap()
    }

    #[test]
    fn parse_rules() {
        assert_eq!(p("main"), RefPattern::Exact("main".into()));
        assert_eq!(p("release/*"), RefPattern::Prefix("release/".into()));
        assert_eq!(p("*"), RefPattern::Prefix(String::new()));
        assert!(RefPattern::parse("a*b").is_err());
        assert!(RefPattern::parse("a**").is_err());
        assert!(RefPattern::parse("").is_err());
    }

    #[test]
    fn precedence_exact_then_longest() {
        let rules = [p("*"), p("release/*"), p("release/1.*"), p("release/1.0")];
        let pick = |n: &str| most_specific(rules.iter(), |r| *r, n).map(ToString::to_string);
        assert_eq!(pick("release/1.0").as_deref(), Some("release/1.0"));
        assert_eq!(pick("release/1.1").as_deref(), Some("release/1.*"));
        assert_eq!(pick("release/2").as_deref(), Some("release/*"));
        assert_eq!(pick("main").as_deref(), Some("*"));
        let none: [RefPattern; 0] = [];
        assert!(most_specific(none.iter(), |r| *r, "main").is_none());
    }

    #[test]
    fn intersection() {
        let r = intersect_patterns(&[p("ws/*")], &[p("ws/a/*"), p("main")]);
        assert_eq!(r, vec![p("ws/a/*")]);
        let r = intersect_patterns(&[p("*")], &[p("main"), p("release/*")]);
        assert_eq!(r, vec![p("main"), p("release/*")]);
        assert!(intersect_patterns(&[p("main")], &[p("release/*")]).is_empty());
    }
}
