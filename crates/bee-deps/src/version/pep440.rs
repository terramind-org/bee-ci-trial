//! PEP 440 versions and version specifiers.

use std::cmp::Ordering;

/// A segment of a local version label.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Local {
    /// Alphabetic segments sort below numeric ones.
    Alpha(String),
    Num(u64),
}

/// A parsed PEP 440 version.
#[derive(Clone, Debug)]
pub struct Pep440 {
    epoch: u64,
    release: Vec<u64>,
    /// `(kind, n)` with kind 0 = alpha, 1 = beta, 2 = release candidate.
    pre: Option<(u8, u64)>,
    post: Option<u64>,
    dev: Option<u64>,
    local: Option<Vec<Local>>,
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn digits(&mut self) -> Option<u64> {
        let start = self.i;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.i += 1;
        }
        if start == self.i {
            return None;
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()?
            .parse()
            .ok()
    }

    fn sep(&mut self) {
        if self.peek().is_some_and(|b| matches!(b, b'-' | b'_' | b'.')) {
            self.i += 1;
        }
    }

    fn word(&mut self, options: &[&str]) -> Option<usize> {
        // Longest match first so `alpha` wins over `a`.
        let mut best: Option<(usize, usize)> = None;
        for (idx, w) in options.iter().enumerate() {
            if self.s[self.i..].starts_with(w.as_bytes()) && best.is_none_or(|(_, l)| w.len() > l) {
                best = Some((idx, w.len()));
            }
        }
        let (idx, len) = best?;
        self.i += len;
        Some(idx)
    }
}

impl Pep440 {
    /// Parses a version using the PEP 440 normalization rules (case folding, alternate
    /// spellings of pre/post/dev markers, implicit zero numbers).
    pub fn parse(text: &str) -> Option<Pep440> {
        let lower = text.trim().to_ascii_lowercase();
        let body = lower.strip_prefix('v').unwrap_or(&lower);
        let mut c = Cursor {
            s: body.as_bytes(),
            i: 0,
        };
        let first = c.digits()?;
        let (epoch, first) = if c.peek() == Some(b'!') {
            c.i += 1;
            (first, c.digits()?)
        } else {
            (0, first)
        };
        let mut release = vec![first];
        while c.peek() == Some(b'.') && c.s.get(c.i + 1).is_some_and(u8::is_ascii_digit) {
            c.i += 1;
            release.push(c.digits()?);
        }
        let mut pre = None;
        let mut post = None;
        let mut dev = None;
        let mut local = None;
        // Pre-release.
        let save = c.i;
        c.sep();
        if let Some(k) = c.word(&["alpha", "beta", "preview", "pre", "rc", "a", "b", "c"]) {
            let kind = match k {
                0 | 5 => 0,
                1 | 6 => 1,
                _ => 2,
            };
            c.sep();
            pre = Some((kind, c.digits().unwrap_or(0)));
        } else {
            c.i = save;
        }
        // Post-release: `-N` or `.postN`.
        let save = c.i;
        if c.peek() == Some(b'-') && c.s.get(c.i + 1).is_some_and(u8::is_ascii_digit) {
            c.i += 1;
            post = Some(c.digits()?);
        } else {
            c.sep();
            if c.word(&["post", "rev", "r"]).is_some() {
                c.sep();
                post = Some(c.digits().unwrap_or(0));
            } else {
                c.i = save;
            }
        }
        // Dev release.
        let save = c.i;
        c.sep();
        if c.word(&["dev"]).is_some() {
            c.sep();
            dev = Some(c.digits().unwrap_or(0));
        } else {
            c.i = save;
        }
        // Local label.
        if c.peek() == Some(b'+') {
            c.i += 1;
            let rest = &body[c.i..];
            if rest.is_empty() {
                return None;
            }
            let mut segs = Vec::new();
            for seg in rest.split(['.', '-', '_']) {
                if seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    return None;
                }
                match seg.parse::<u64>() {
                    Ok(n) if seg.bytes().all(|b| b.is_ascii_digit()) => segs.push(Local::Num(n)),
                    _ => segs.push(Local::Alpha(seg.to_owned())),
                }
            }
            local = Some(segs);
            c.i = body.len();
        }
        if c.i != body.len() {
            return None;
        }
        Some(Pep440 {
            epoch,
            release,
            pre,
            post,
            dev,
            local,
        })
    }

    /// True for pre-releases and dev releases.
    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some() || self.dev.is_some()
    }

    fn trimmed_release(&self) -> &[u64] {
        let mut n = self.release.len();
        while n > 1 && self.release[n - 1] == 0 {
            n -= 1;
        }
        &self.release[..n]
    }

    fn pre_key(&self) -> (i8, u8, u64) {
        match (self.pre, self.post, self.dev) {
            (None, None, Some(_)) => (-1, 0, 0),
            (None, ..) => (1, 0, 0),
            (Some((k, n)), ..) => (0, k, n),
        }
    }

    fn base_cmp(&self, other: &Pep440) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| self.trimmed_release().cmp(other.trimmed_release()))
            .then_with(|| self.pre_key().cmp(&other.pre_key()))
            .then_with(|| self.post.cmp(&other.post))
            .then_with(|| match (self.dev, other.dev) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(&b),
            })
    }

    /// The release segments with trailing zeros kept.
    pub fn release(&self) -> &[u64] {
        &self.release
    }

    fn without_local(&self) -> Pep440 {
        Pep440 {
            local: None,
            ..self.clone()
        }
    }
}

impl PartialEq for Pep440 {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Pep440 {}

impl PartialOrd for Pep440 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Pep440 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.base_cmp(other)
            .then_with(|| self.local.cmp(&other.local))
    }
}

/// A parsed set of PEP 440 version specifiers (a comma-joined conjunction).
#[derive(Clone, Debug)]
pub struct Specifiers {
    clauses: Vec<Clause>,
}

#[derive(Clone, Debug)]
struct Clause {
    op: SpecOp,
    version: Pep440,
    /// `==1.*` style prefix match.
    prefix: bool,
    raw_version: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpecOp {
    Compatible,
    Eq,
    Ne,
    Le,
    Ge,
    Lt,
    Gt,
    Arbitrary,
}

impl Specifiers {
    /// Parses `>=1.0,<2.0`, `~=1.4.2`, `==1.*`, `!=1.5` and friends. An empty string allows
    /// every version.
    pub fn parse(text: &str) -> Option<Specifiers> {
        let mut clauses = Vec::new();
        for part in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (op, rest) = ["===", "~=", "==", "!=", "<=", ">=", "<", ">"]
                .iter()
                .find_map(|o| part.strip_prefix(o).map(|r| (*o, r.trim())))?;
            let op = match op {
                "===" => SpecOp::Arbitrary,
                "~=" => SpecOp::Compatible,
                "==" => SpecOp::Eq,
                "!=" => SpecOp::Ne,
                "<=" => SpecOp::Le,
                ">=" => SpecOp::Ge,
                "<" => SpecOp::Lt,
                _ => SpecOp::Gt,
            };
            let (v, prefix) = match rest.strip_suffix(".*") {
                Some(v) if matches!(op, SpecOp::Eq | SpecOp::Ne) => (v, true),
                _ => (rest, false),
            };
            clauses.push(Clause {
                op,
                version: Pep440::parse(v)?,
                prefix,
                raw_version: rest.to_owned(),
            });
        }
        Some(Specifiers { clauses })
    }

    fn clause_matches(c: &Clause, v: &Pep440) -> bool {
        match c.op {
            SpecOp::Arbitrary => c.raw_version.eq_ignore_ascii_case(&render(v)),
            SpecOp::Eq | SpecOp::Ne => {
                let hit = if c.prefix {
                    v.epoch == c.version.epoch
                        && starts_with_release(&v.release, &c.version.release)
                } else if c.version.local.is_some() {
                    v == &c.version
                } else {
                    v.without_local() == c.version
                };
                hit == (c.op == SpecOp::Eq)
            }
            SpecOp::Le => v.without_local() <= c.version,
            SpecOp::Ge => v.without_local() >= c.version,
            SpecOp::Lt => {
                let w = v.without_local();
                w < c.version
                    && !(c.version.pre.is_none()
                        && c.version.dev.is_none()
                        && w.base_release_eq(&c.version)
                        && w.is_prerelease())
            }
            SpecOp::Gt => {
                let w = v.without_local();
                w > c.version
                    && !(c.version.post.is_none()
                        && w.post.is_some()
                        && w.base_release_eq(&c.version))
            }
            SpecOp::Compatible => {
                if c.version.release.len() < 2 {
                    return false;
                }
                let mut prefix = c.version.release.clone();
                prefix.pop();
                v.without_local() >= c.version
                    && v.epoch == c.version.epoch
                    && starts_with_release(&v.release, &prefix)
            }
        }
    }

    /// True when every clause allows `version`. A pre-release only matches when some clause
    /// names a pre-release itself.
    pub fn matches(&self, version: &Pep440) -> bool {
        if version.is_prerelease() && !self.clauses.iter().any(|c| c.version.is_prerelease()) {
            return false;
        }
        self.clauses
            .iter()
            .all(|c| Self::clause_matches(c, version))
    }

    /// The lowest version the specifiers pin or bound from below (`==`, `>=`, `~=`, `>`).
    pub fn min_version(&self) -> Option<String> {
        self.clauses
            .iter()
            .filter(|c| {
                matches!(
                    c.op,
                    SpecOp::Eq | SpecOp::Ge | SpecOp::Compatible | SpecOp::Gt
                ) && !c.prefix
            })
            .map(|c| &c.version)
            .max()
            .map(render)
    }

    /// The exact version when the specifiers pin one with `==` or `===`.
    pub fn pinned(&self) -> Option<String> {
        self.clauses
            .iter()
            .find(|c| matches!(c.op, SpecOp::Eq | SpecOp::Arbitrary) && !c.prefix)
            .map(|c| c.raw_version.clone())
    }
}

impl Pep440 {
    fn base_release_eq(&self, other: &Pep440) -> bool {
        self.epoch == other.epoch && self.trimmed_release() == other.trimmed_release()
    }
}

fn starts_with_release(v: &[u64], prefix: &[u64]) -> bool {
    (0..prefix.len().max(v.len()))
        .take(prefix.len())
        .all(|i| v.get(i).copied().unwrap_or(0) == prefix[i])
}

/// Renders the canonical form of a version.
pub(crate) fn render(v: &Pep440) -> String {
    let mut s = String::new();
    if v.epoch != 0 {
        s.push_str(&format!("{}!", v.epoch));
    }
    let rel: Vec<String> = v.release.iter().map(u64::to_string).collect();
    s.push_str(&rel.join("."));
    if let Some((k, n)) = v.pre {
        s.push_str(["a", "b", "rc"][usize::from(k.min(2))]);
        s.push_str(&n.to_string());
    }
    if let Some(n) = v.post {
        s.push_str(&format!(".post{n}"));
    }
    if let Some(n) = v.dev {
        s.push_str(&format!(".dev{n}"));
    }
    if let Some(l) = &v.local {
        s.push('+');
        let segs: Vec<String> = l
            .iter()
            .map(|x| match x {
                Local::Num(n) => n.to_string(),
                Local::Alpha(a) => a.clone(),
            })
            .collect();
        s.push_str(&segs.join("."));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Pep440 {
        Pep440::parse(s).unwrap_or_else(|| panic!("parse {s}"))
    }

    #[test]
    fn ordering_chain_from_the_pep() {
        let chain = [
            "1.0.dev456",
            "1.0a1",
            "1.0a2.dev456",
            "1.0a12.dev456",
            "1.0a12",
            "1.0b1.dev456",
            "1.0b2",
            "1.0b2.post345.dev456",
            "1.0b2.post345",
            "1.0rc1.dev456",
            "1.0rc1",
            "1.0",
            "1.0+abc.5",
            "1.0+abc.7",
            "1.0+5",
            "1.0.post456.dev34",
            "1.0.post456",
            "1.1.dev1",
        ];
        for w in chain.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
        }
    }

    #[test]
    fn normalization_equivalences() {
        assert_eq!(v("1.0"), v("1"));
        assert_eq!(v("1.0.0"), v("1.0"));
        assert_eq!(v("1.0-alpha1"), v("1.0a1"));
        assert_eq!(v("1.0.RC1"), v("1.0rc1"));
        assert_eq!(v("1.0c1"), v("1.0rc1"));
        assert_eq!(v("1.0-1"), v("1.0.post1"));
        assert_eq!(v("1.0-rev3"), v("1.0.post3"));
        assert_eq!(v("1.0.dev"), v("1.0.dev0"));
        assert_eq!(v("v2.0"), v("2.0"));
        assert!(v("1!0.5") > v("9.9"));
        assert!(Pep440::parse("1.0.x").is_none());
        assert!(Pep440::parse("abc").is_none());
    }

    #[test]
    fn specifiers() {
        let sat = |spec: &str, ver: &str| Specifiers::parse(spec).unwrap().matches(&v(ver));
        assert!(sat(">=1.0,<2.0", "1.5"));
        assert!(!sat(">=1.0,<2.0", "2.0"));
        assert!(!sat(">=1.0,<2.0", "2.0rc1"));
        assert!(sat("~=1.4.2", "1.4.9"));
        assert!(!sat("~=1.4.2", "1.5.0"));
        assert!(sat("~=1.4", "1.9"));
        assert!(sat("==1.2.*", "1.2.7"));
        assert!(!sat("==1.2.*", "1.3.0"));
        assert!(sat("==1.0", "1.0.0"));
        assert!(sat("==1.0", "1.0+local"));
        assert!(!sat("!=1.5", "1.5.0"));
        assert!(sat(">1.0", "1.1"));
        assert!(!sat(">1.0", "1.0.post1"));
        assert!(sat("", "3.1"));
        assert!(sat(">=1.0rc1", "1.0rc2"));
        assert!(Specifiers::parse("somename").is_none());
        let s = Specifiers::parse(">=1.2,<2").unwrap();
        assert_eq!(s.min_version().as_deref(), Some("1.2"));
        assert_eq!(
            Specifiers::parse("==2.28.0").unwrap().pinned().as_deref(),
            Some("2.28.0")
        );
    }
}
