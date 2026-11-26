//! RubyGems version ordering and requirement strings (`~> 1.2`, `>= 1.0, < 2`).

use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Seg {
    Num(String),
    Str(String),
}

fn cmp_seg(a: &Seg, b: &Seg) -> Ordering {
    match (a, b) {
        (Seg::Num(x), Seg::Num(y)) => x.len().cmp(&y.len()).then_with(|| x.cmp(y)),
        (Seg::Str(x), Seg::Str(y)) => x.cmp(y),
        // A letter segment marks a pre-release and sorts below any number.
        (Seg::Str(_), Seg::Num(_)) => Ordering::Less,
        (Seg::Num(_), Seg::Str(_)) => Ordering::Greater,
    }
}

/// A parsed RubyGems version.
#[derive(Clone, Debug)]
pub struct GemVersion {
    /// Canonical segments: trailing zeros trimmed in the numeric part and the pre-release part.
    segments: Vec<Seg>,
}

impl GemVersion {
    /// Parses a gem version such as `1.2.3`, `7.0.0.rc1` or `2.0.pre`.
    pub fn parse(text: &str) -> Option<GemVersion> {
        let t = text.trim().replace('-', ".pre.");
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') {
            return None;
        }
        let mut segs: Vec<Seg> = Vec::new();
        for part in t.split('.') {
            let mut cur = String::new();
            let mut cur_digit = false;
            for c in part.chars() {
                let d = c.is_ascii_digit();
                if !cur.is_empty() && d != cur_digit {
                    segs.push(make_seg(&cur, cur_digit));
                    cur.clear();
                }
                cur_digit = d;
                cur.push(c);
            }
            if !cur.is_empty() {
                segs.push(make_seg(&cur, cur_digit));
            }
        }
        if !matches!(segs.first(), Some(Seg::Num(_))) {
            return None;
        }
        let split = segs
            .iter()
            .position(|s| matches!(s, Seg::Str(_)))
            .unwrap_or(segs.len());
        let (mut nums, mut pre) = (segs[..split].to_vec(), segs[split..].to_vec());
        let is_zero = |s: &Seg| matches!(s, Seg::Num(n) if n.is_empty());
        while nums.last().is_some_and(is_zero) {
            nums.pop();
        }
        while pre.last().is_some_and(is_zero) {
            pre.pop();
        }
        nums.extend(pre);
        Some(GemVersion { segments: nums })
    }

    /// True when the version has a letter segment.
    pub fn is_prerelease(&self) -> bool {
        self.segments.iter().any(|s| matches!(s, Seg::Str(_)))
    }

    /// The first `n` numeric segments as a string (`1.2.3` with n=2 gives `1.2`).
    fn release_prefix(&self, n: usize) -> Vec<String> {
        self.segments
            .iter()
            .take_while(|s| matches!(s, Seg::Num(_)))
            .take(n)
            .map(|s| match s {
                Seg::Num(x) => {
                    if x.is_empty() {
                        "0".to_owned()
                    } else {
                        x.clone()
                    }
                }
                Seg::Str(x) => x.clone(),
            })
            .collect()
    }
}

fn make_seg(text: &str, digit: bool) -> Seg {
    if digit {
        Seg::Num(text.trim_start_matches('0').to_owned())
    } else {
        Seg::Str(text.to_owned())
    }
}

impl PartialEq for GemVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for GemVersion {}

impl PartialOrd for GemVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GemVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let zero = Seg::Num(String::new());
        let n = self.segments.len().max(other.segments.len());
        for i in 0..n {
            let a = self.segments.get(i).unwrap_or(&zero);
            let b = other.segments.get(i).unwrap_or(&zero);
            let o = cmp_seg(a, b);
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }
}

/// A parsed gem requirement: a conjunction of `(operator, version)` pairs.
#[derive(Clone, Debug)]
pub struct GemRequirement {
    clauses: Vec<(String, GemVersion, String)>,
}

impl GemRequirement {
    /// Parses `~> 1.2`, `>= 1.0, < 2.0`, `= 3.1`, or a bare version (meaning `=`).
    pub fn parse(text: &str) -> Option<GemRequirement> {
        let mut clauses = Vec::new();
        for part in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (op, rest) = ["~>", ">=", "<=", "!=", "=", ">", "<"]
                .iter()
                .find_map(|o| part.strip_prefix(o).map(|r| (*o, r.trim())))
                .unwrap_or(("=", part));
            clauses.push((op.to_owned(), GemVersion::parse(rest)?, rest.to_owned()));
        }
        if clauses.is_empty() {
            return None;
        }
        Some(GemRequirement { clauses })
    }

    /// True when every clause allows `version`.
    pub fn matches(&self, version: &GemVersion) -> bool {
        self.clauses.iter().all(|(op, v, _)| match op.as_str() {
            "=" => version == v,
            "!=" => version != v,
            ">" => version > v,
            ">=" => version >= v,
            "<" => version < v,
            "<=" => version <= v,
            _ => {
                // `~> 1.2.3` allows >= 1.2.3 and < 1.3; `~> 1.2` allows >= 1.2 and < 2.
                let nums = v.release_prefix(usize::MAX).len().max(2);
                let keep = nums - 1;
                if version < v {
                    return false;
                }
                let want = v.release_prefix(keep);
                let have = version.release_prefix(keep);
                let mut have = have;
                while have.len() < want.len() {
                    have.push("0".to_owned());
                }
                want == have
            }
        })
    }

    /// The lowest version named by a `=`, `>=`, `>` or `~>` clause, as written.
    pub fn min_version(&self) -> Option<String> {
        self.clauses
            .iter()
            .filter(|(op, ..)| matches!(op.as_str(), "=" | ">=" | ">" | "~>"))
            .max_by(|a, b| a.1.cmp(&b.1))
            .map(|(_, _, raw)| raw.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(s: &str) -> GemVersion {
        GemVersion::parse(s).unwrap()
    }

    #[test]
    fn ordering() {
        let chain = [
            "1.0.a", "1.0.b", "1.0.rc1", "1.0", "1.0.1", "1.1", "1.10", "2",
        ];
        for w in chain.windows(2) {
            assert!(g(w[0]) < g(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(g("1.0"), g("1"));
        assert_eq!(g("1.0.a"), g("1.a"));
        assert_eq!(g("1.0-1"), g("1.0.pre.1"));
        assert!(g("7.0.0.rc1") < g("7.0.0"));
        assert!(g("1.9") < g("1.10"));
        assert!(GemVersion::parse("abc").is_none());
    }

    #[test]
    fn requirements() {
        let sat = |r: &str, v: &str| GemRequirement::parse(r).unwrap().matches(&g(v));
        assert!(sat("~> 1.2", "1.9"));
        assert!(!sat("~> 1.2", "2.0"));
        assert!(sat("~> 1.2.3", "1.2.9"));
        assert!(!sat("~> 1.2.3", "1.3.0"));
        assert!(!sat("~> 1.2.3", "1.2.2"));
        assert!(sat(">= 1.0, < 2.0", "1.5"));
        assert!(!sat(">= 1.0, < 2.0", "2.0"));
        assert!(sat("3.1", "3.1.0"));
        assert!(sat("~> 5", "5.9"));
        assert_eq!(
            GemRequirement::parse(">= 1.0, < 2.0")
                .unwrap()
                .min_version()
                .as_deref(),
            Some("1.0")
        );
    }
}
