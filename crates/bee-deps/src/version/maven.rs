//! Maven version ordering and the interval range syntax shared by Maven and NuGet.
//!
//! Ordering follows Maven's `ComparableVersion`: versions split into numeric and qualifier
//! items at `.`, `-` and digit/letter transitions; known qualifiers order as
//! `alpha < beta < milestone < rc < snapshot < (release) < sp`, unknown qualifiers sort after
//! `sp` alphabetically, and trailing "null" items (`0`, `ga`, `final`, `release`) are ignored.

use std::cmp::Ordering;

#[derive(Clone, Debug)]
enum Item {
    /// A number, kept as digits without leading zeros so any length compares correctly.
    Int(String),
    /// A qualifier; the flag records that a digit follows it (`a1`, `b2`, `m3`).
    Str(String, bool),
    List(Vec<Item>),
}

const QUALIFIERS: [&str; 7] = ["alpha", "beta", "milestone", "rc", "snapshot", "", "sp"];

fn strip_zeros(s: &str) -> String {
    let t = s.trim_start_matches('0');
    t.to_owned()
}

fn comparable_qualifier(value: &str, followed_by_digit: bool) -> String {
    let mut q = value;
    if followed_by_digit && q.len() == 1 {
        q = match q {
            "a" => "alpha",
            "b" => "beta",
            "m" => "milestone",
            other => other,
        };
    }
    let q = match q {
        "ga" | "final" | "release" => "",
        "cr" => "rc",
        other => other,
    };
    match QUALIFIERS.iter().position(|k| *k == q) {
        Some(i) => i.to_string(),
        None => format!("{}-{}", QUALIFIERS.len(), q),
    }
}

impl Item {
    fn is_null(&self) -> bool {
        match self {
            Item::Int(s) => s.is_empty(),
            Item::Str(v, _) => comparable_qualifier(v, false) == comparable_qualifier("", false),
            Item::List(l) => l.is_empty(),
        }
    }

    fn cmp_item(&self, other: Option<&Item>) -> Ordering {
        match (self, other) {
            (Item::Int(a), None) => {
                if a.is_empty() {
                    Ordering::Equal
                } else {
                    Ordering::Greater
                }
            }
            (Item::Int(a), Some(Item::Int(b))) => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
            (Item::Int(_), Some(Item::Str(..))) => Ordering::Greater,
            (Item::Int(_), Some(Item::List(_))) => Ordering::Greater,
            (Item::Str(v, f), None) => {
                comparable_qualifier(v, *f).cmp(&comparable_qualifier("", false))
            }
            (Item::Str(..), Some(Item::Int(_))) => Ordering::Less,
            (Item::Str(a, fa), Some(Item::Str(b, fb))) => {
                comparable_qualifier(a, *fa).cmp(&comparable_qualifier(b, *fb))
            }
            (Item::Str(..), Some(Item::List(_))) => Ordering::Less,
            (Item::List(l), None) => match l.first() {
                None => Ordering::Equal,
                Some(first) => first.cmp_item(None),
            },
            (Item::List(_), Some(Item::Int(_))) => Ordering::Less,
            (Item::List(_), Some(Item::Str(..))) => Ordering::Greater,
            (Item::List(a), Some(Item::List(b))) => {
                let n = a.len().max(b.len());
                for i in 0..n {
                    let o = match (a.get(i), b.get(i)) {
                        (Some(l), r) => l.cmp_item(r),
                        (None, Some(r)) => r.cmp_item(None).reverse(),
                        (None, None) => Ordering::Equal,
                    };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            }
        }
    }
}

fn normalize(list: &mut Vec<Item>) {
    // Trailing null items go, looking past nested lists (which stay).
    let mut i = list.len();
    while i > 0 {
        i -= 1;
        if list[i].is_null() {
            list.remove(i);
        } else if !matches!(list[i], Item::List(_)) {
            break;
        }
    }
}

fn parse_item(is_digit: bool, text: &str) -> Item {
    if is_digit {
        Item::Int(strip_zeros(text))
    } else {
        Item::Str(text.to_owned(), false)
    }
}

/// A parsed Maven version.
#[derive(Clone, Debug)]
pub struct MavenVersion {
    root: Vec<Item>,
}

impl MavenVersion {
    /// Parses a version. Maven accepts any non-empty string.
    pub fn parse(text: &str) -> Option<MavenVersion> {
        let v = text.trim().to_ascii_lowercase();
        if v.is_empty() {
            return None;
        }
        // Stack of open lists; the last element is the one being filled.
        let mut stack: Vec<Vec<Item>> = vec![Vec::new()];
        let mut is_digit = false;
        let mut start = 0;
        let bytes = v.as_bytes();
        let push = |stack: &mut Vec<Vec<Item>>, item: Item| {
            if let Some(top) = stack.last_mut() {
                top.push(item);
            }
        };
        for (i, &c) in bytes.iter().enumerate() {
            if c == b'.' || c == b'-' {
                if i == start {
                    push(&mut stack, Item::Int(String::new()));
                } else {
                    push(&mut stack, parse_item(is_digit, &v[start..i]));
                }
                start = i + 1;
                if c == b'-' {
                    stack.push(Vec::new());
                }
            } else if c.is_ascii_digit() {
                if !is_digit && i > start {
                    push(&mut stack, Item::Str(v[start..i].to_owned(), true));
                    start = i;
                    stack.push(Vec::new());
                }
                is_digit = true;
            } else {
                if is_digit && i > start {
                    push(&mut stack, parse_item(true, &v[start..i]));
                    start = i;
                    stack.push(Vec::new());
                }
                is_digit = false;
            }
        }
        if bytes.len() > start {
            push(&mut stack, parse_item(is_digit, &v[start..]));
        }
        // Close lists from the innermost out, normalizing each and attaching it to its parent.
        let mut closed: Option<Vec<Item>> = None;
        while let Some(mut list) = stack.pop() {
            if let Some(inner) = closed.take() {
                list.push(Item::List(inner));
            }
            normalize(&mut list);
            closed = Some(list);
        }
        Some(MavenVersion {
            root: closed.unwrap_or_default(),
        })
    }
}

impl PartialEq for MavenVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for MavenVersion {}

impl PartialOrd for MavenVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MavenVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        Item::List(self.root.clone()).cmp_item(Some(&Item::List(other.root.clone())))
    }
}

// ---------------------------------------------------------------------------------------------
// Interval ranges (Maven and NuGet)
// ---------------------------------------------------------------------------------------------

/// One interval with optional inclusive or exclusive bounds, as raw version strings.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Interval {
    lo: Option<(String, bool)>,
    hi: Option<(String, bool)>,
}

/// A parsed Maven or NuGet version requirement.
#[derive(Clone, Debug)]
pub struct IntervalRange {
    intervals: Vec<Interval>,
    /// A bare version with no brackets.
    soft: Option<String>,
    nuget: bool,
}

impl IntervalRange {
    /// Parses `[1.0,2.0)`, `(,1.0]`, `[1.5]` and unions such as `(,1.0],[1.2,)`. A bare version
    /// is a soft requirement in Maven and a minimum (`>=`) in NuGet.
    pub fn parse(text: &str, nuget: bool) -> Option<IntervalRange> {
        let t = text.trim();
        if t.is_empty() {
            return None;
        }
        if !t.starts_with(['[', '(']) {
            if t.contains(['[', '(', ']', ')', ',']) {
                return None;
            }
            return Some(IntervalRange {
                intervals: Vec::new(),
                soft: Some(t.to_owned()),
                nuget,
            });
        }
        let mut intervals = Vec::new();
        let mut rest = t;
        while !rest.is_empty() {
            let lo_incl = match rest.as_bytes()[0] {
                b'[' => true,
                b'(' => false,
                _ => return None,
            };
            let end = rest.find([']', ')'])?;
            let hi_incl = rest.as_bytes()[end] == b']';
            let inner = &rest[1..end];
            let iv = match inner.split_once(',') {
                None => {
                    if !(lo_incl && hi_incl) || inner.trim().is_empty() {
                        return None;
                    }
                    let v = inner.trim().to_owned();
                    Interval {
                        lo: Some((v.clone(), true)),
                        hi: Some((v, true)),
                    }
                }
                Some((a, b)) => {
                    let (a, b) = (a.trim(), b.trim());
                    Interval {
                        lo: (!a.is_empty()).then(|| (a.to_owned(), lo_incl)),
                        hi: (!b.is_empty()).then(|| (b.to_owned(), hi_incl)),
                    }
                }
            };
            intervals.push(iv);
            rest = rest[end + 1..].trim_start_matches([',', ' ']);
        }
        Some(IntervalRange {
            intervals,
            soft: None,
            nuget,
        })
    }

    /// True when `version` is allowed. A Maven soft requirement allows everything.
    pub fn matches(&self, version: &str) -> bool {
        let Some(v) = MavenVersion::parse(version) else {
            return false;
        };
        if let Some(soft) = &self.soft {
            if !self.nuget {
                return true;
            }
            return MavenVersion::parse(soft).is_some_and(|m| v >= m);
        }
        self.intervals.iter().any(|iv| {
            let lo_ok = iv.lo.as_ref().is_none_or(|(b, incl)| {
                MavenVersion::parse(b).is_some_and(|b| if *incl { v >= b } else { v > b })
            });
            let hi_ok = iv.hi.as_ref().is_none_or(|(b, incl)| {
                MavenVersion::parse(b).is_some_and(|b| if *incl { v <= b } else { v < b })
            });
            lo_ok && hi_ok
        })
    }

    /// The lowest version the requirement names.
    pub fn min_version(&self) -> Option<String> {
        if let Some(s) = &self.soft {
            return Some(s.clone());
        }
        self.intervals
            .iter()
            .filter_map(|iv| iv.lo.as_ref().map(|(v, _)| v.clone()))
            .min_by(
                |a, b| match (MavenVersion::parse(a), MavenVersion::parse(b)) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    _ => a.cmp(b),
                },
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> MavenVersion {
        MavenVersion::parse(s).unwrap()
    }

    #[test]
    fn qualifier_ordering() {
        let chain = [
            "1-alpha2",
            "1-alpha10",
            "1-beta1",
            "1-milestone1",
            "1-rc1",
            "1-snapshot",
            "1",
            "1-sp1",
            "1.0.1",
            "1.1",
            "2",
        ];
        for w in chain.windows(2) {
            assert!(m(w[0]) < m(w[1]), "{} < {}", w[0], w[1]);
        }
    }

    #[test]
    fn equivalences() {
        assert_eq!(m("1"), m("1.0"));
        assert_eq!(m("1"), m("1.0.0"));
        assert_eq!(m("1.0"), m("1-ga"));
        assert_eq!(m("1.0-final"), m("1"));
        assert_eq!(m("1-cr1"), m("1-rc1"));
        assert_eq!(m("1.0a1"), m("1.0-alpha-1"));
        assert_eq!(m("1.0-SNAPSHOT"), m("1.0-snapshot"));
        assert!(m("2.14.1") < m("2.15.0"));
        assert!(m("2.9") < m("2.10"));
        assert!(m("1.0-foo") > m("1.0-sp1"));
        assert!(m("1.0.0.RELEASE") == m("1.0"));
    }

    #[test]
    fn log4shell_range_boundaries() {
        assert!(m("2.14.1") < m("2.15.0"));
        assert!(m("2.0-beta9") < m("2.0"));
        assert!(m("2.0-beta9") < m("2.0-rc1"));
    }

    #[test]
    fn interval_ranges() {
        let r = IntervalRange::parse("[1.0,2.0)", false).unwrap();
        assert!(r.matches("1.0") && r.matches("1.9") && !r.matches("2.0") && !r.matches("0.9"));
        let r = IntervalRange::parse("(,1.0],[1.2,)", false).unwrap();
        assert!(
            r.matches("0.5")
                && r.matches("1.0")
                && !r.matches("1.1")
                && r.matches("1.2")
                && r.matches("9")
        );
        let r = IntervalRange::parse("[1.5]", false).unwrap();
        assert!(r.matches("1.5") && !r.matches("1.5.1"));
        assert!(IntervalRange::parse("1.2", false).unwrap().matches("0.1"));
        let n = IntervalRange::parse("1.2", true).unwrap();
        assert!(n.matches("1.3") && !n.matches("1.1"));
        assert_eq!(
            IntervalRange::parse("[1.0,2.0)", true)
                .unwrap()
                .min_version()
                .as_deref(),
            Some("1.0")
        );
        assert!(IntervalRange::parse("[1.0", false).is_none());
    }
}
