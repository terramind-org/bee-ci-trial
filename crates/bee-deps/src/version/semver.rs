//! Semantic versions and the npm, Cargo and Composer range grammars.
//!
//! The version parser is deliberately loose (one to four numeric parts, optional `v` prefix) so
//! it also covers Go module versions, NuGet's four-part versions and short Cargo requirements.
//! Ordering follows SemVer 2.0.0: numeric parts, then pre-release identifiers, build metadata
//! ignored.

use std::cmp::Ordering;

/// One dot-separated pre-release identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Ident {
    /// A numeric identifier; sorts below every alphanumeric one.
    Num(u64),
    /// An alphanumeric identifier, compared as bytes.
    Alpha(String),
}

/// A parsed semantic version.
#[derive(Clone, Debug)]
pub struct SemVer {
    nums: Vec<u64>,
    pre: Vec<Ident>,
    build: String,
}

impl SemVer {
    /// Parses a version. Accepts a leading `v`, one to four numeric parts, `-pre` and `+build`.
    pub fn parse(text: &str) -> Option<SemVer> {
        let t = text.trim();
        let t = t.strip_prefix(['v', 'V']).unwrap_or(t);
        let (rest, build) = match t.split_once('+') {
            Some((r, b)) => (r, b),
            None => (t, ""),
        };
        let (num_part, pre_part) = match rest.split_once('-') {
            Some((n, p)) => (n, Some(p)),
            None => (rest, None),
        };
        if num_part.is_empty() {
            return None;
        }
        let mut nums = Vec::new();
        for seg in num_part.split('.') {
            if seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            nums.push(seg.parse::<u64>().ok()?);
        }
        if nums.len() > 4 {
            return None;
        }
        let mut pre = Vec::new();
        if let Some(p) = pre_part {
            if p.is_empty() {
                return None;
            }
            for id in p.split('.') {
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return None;
                }
                if id.bytes().all(|b| b.is_ascii_digit()) {
                    match id.parse::<u64>() {
                        Ok(n) => pre.push(Ident::Num(n)),
                        Err(_) => pre.push(Ident::Alpha(id.to_owned())),
                    }
                } else {
                    pre.push(Ident::Alpha(id.to_owned()));
                }
            }
        }
        if build.contains(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-')) {
            return None;
        }
        Some(SemVer {
            nums,
            pre,
            build: build.to_owned(),
        })
    }

    /// Builds a version from three numeric parts.
    pub fn new(major: u64, minor: u64, patch: u64) -> SemVer {
        SemVer {
            nums: vec![major, minor, patch],
            pre: Vec::new(),
            build: String::new(),
        }
    }

    /// The numeric part at `i`, zero when absent.
    pub fn part(&self, i: usize) -> u64 {
        self.nums.get(i).copied().unwrap_or(0)
    }

    /// The major version.
    pub fn major(&self) -> u64 {
        self.part(0)
    }

    /// The minor version.
    pub fn minor(&self) -> u64 {
        self.part(1)
    }

    /// The patch version.
    pub fn patch(&self) -> u64 {
        self.part(2)
    }

    /// True when the version has pre-release identifiers.
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// The build metadata, empty when absent.
    pub fn build(&self) -> &str {
        &self.build
    }

    fn same_triple(&self, other: &SemVer) -> bool {
        self.major() == other.major()
            && self.minor() == other.minor()
            && self.patch() == other.patch()
    }

    /// Renders as `major.minor.patch[-pre]`, dropping build metadata.
    pub fn to_core_string(&self) -> String {
        let mut s = format!("{}.{}.{}", self.major(), self.minor(), self.patch());
        if !self.pre.is_empty() {
            s.push('-');
            let ids: Vec<String> = self
                .pre
                .iter()
                .map(|i| match i {
                    Ident::Num(n) => n.to_string(),
                    Ident::Alpha(a) => a.clone(),
                })
                .collect();
            s.push_str(&ids.join("."));
        }
        s
    }
}

impl PartialEq for SemVer {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for SemVer {}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.nums.len().max(other.nums.len());
        for i in 0..n {
            let o = self.part(i).cmp(&other.part(i));
            if o != Ordering::Equal {
                return o;
            }
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => self.pre.cmp(&other.pre),
        }
    }
}

/// Pieces of a Go pseudo-version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoPseudo {
    /// The 14-digit UTC timestamp, `yyyymmddhhmmss`.
    pub timestamp: String,
    /// The 12-hex-digit commit prefix.
    pub commit: String,
}

/// Recognizes a Go pseudo-version such as `v0.0.0-20191109021931-daa7c04131f5`.
pub fn go_pseudo(version: &str) -> Option<GoPseudo> {
    let v = version.strip_suffix("+incompatible").unwrap_or(version);
    SemVer::parse(v)?;
    let mut parts = v.rsplitn(3, '-');
    let commit = parts.next()?;
    let ts = parts.next()?;
    parts.next()?;
    // The timestamp may be glued to a pre-release dot: `v1.2.3-pre.0.20191109021931-abc`.
    let ts = ts.rsplit('.').next().unwrap_or(ts);
    if commit.len() == 12
        && commit.bytes().all(|b| b.is_ascii_hexdigit())
        && ts.len() == 14
        && ts.bytes().all(|b| b.is_ascii_digit())
    {
        Some(GoPseudo {
            timestamp: ts.to_owned(),
            commit: commit.to_owned(),
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Ranges
// ---------------------------------------------------------------------------------------------

/// A comparison operator in a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

/// One bound of a range.
#[derive(Clone, Debug)]
pub(crate) struct Comparator {
    pub(crate) op: Op,
    pub(crate) version: SemVer,
}

/// Which grammar a range string is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeFlavor {
    /// npm ranges: `||` alternatives, space-joined comparators, `^`, `~`, x-ranges, hyphens.
    Npm,
    /// Cargo requirements: comma-joined comparators, bare versions mean `^`.
    Cargo,
    /// Composer constraints: `||` or `|` alternatives, comma or space joined, `~1.2` rolls the
    /// minor version.
    Composer,
}

/// A parsed range: alternatives, each a conjunction of comparators.
#[derive(Clone, Debug)]
pub struct Range {
    sets: Vec<Vec<Comparator>>,
    flavor: RangeFlavor,
}

#[derive(Clone, Debug)]
struct Partial {
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Option<SemVer>,
}

fn parse_partial(text: &str) -> Option<Partial> {
    let t = text.trim();
    let t = t.strip_prefix(['v', 'V', '=']).unwrap_or(t);
    let is_wild = |s: &str| matches!(s, "x" | "X" | "*");
    if t.is_empty() || is_wild(t) {
        return Some(Partial {
            major: None,
            minor: None,
            patch: None,
            pre: None,
        });
    }
    let (core, rest) = match t.find(['-', '+']) {
        Some(i) => (&t[..i], &t[i..]),
        None => (t, ""),
    };
    let segs: Vec<&str> = core.split('.').collect();
    if segs.len() > 3 {
        return None;
    }
    let mut vals: [Option<u64>; 3] = [None; 3];
    let mut wild = false;
    for (i, s) in segs.iter().enumerate() {
        if is_wild(s) {
            wild = true;
        } else if wild || s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        } else {
            vals[i] = Some(s.parse().ok()?);
        }
    }
    let pre = if rest.is_empty() {
        None
    } else if vals[2].is_some() {
        let full = format!("{}.{}.{}{}", vals[0]?, vals[1]?, vals[2]?, rest);
        Some(SemVer::parse(&full)?)
    } else {
        return None;
    };
    Some(Partial {
        major: vals[0],
        minor: vals[1],
        patch: vals[2],
        pre,
    })
}

impl Partial {
    fn lower(&self) -> SemVer {
        match &self.pre {
            Some(p) => p.clone(),
            None => SemVer::new(
                self.major.unwrap_or(0),
                self.minor.unwrap_or(0),
                self.patch.unwrap_or(0),
            ),
        }
    }
}

fn cmp(op: Op, v: SemVer) -> Comparator {
    Comparator { op, version: v }
}

fn any_version() -> Vec<Comparator> {
    vec![cmp(Op::Ge, SemVer::new(0, 0, 0))]
}

fn caret(p: &Partial) -> Vec<Comparator> {
    let Some(major) = p.major else {
        return any_version();
    };
    let lo = p.lower();
    let hi = match (p.minor, p.patch) {
        _ if major > 0 => SemVer::new(major + 1, 0, 0),
        (None, _) => SemVer::new(1, 0, 0),
        (Some(minor), _) if minor > 0 => SemVer::new(0, minor + 1, 0),
        (Some(_), None) => SemVer::new(0, 1, 0),
        (Some(_), Some(patch)) => SemVer::new(0, 0, patch + 1),
    };
    vec![cmp(Op::Ge, lo), cmp(Op::Lt, hi)]
}

fn tilde(p: &Partial, composer: bool) -> Vec<Comparator> {
    let Some(major) = p.major else {
        return any_version();
    };
    let lo = p.lower();
    let hi = match p.minor {
        None => SemVer::new(major + 1, 0, 0),
        Some(_) if composer && p.patch.is_none() => SemVer::new(major + 1, 0, 0),
        Some(minor) => SemVer::new(major, minor + 1, 0),
    };
    vec![cmp(Op::Ge, lo), cmp(Op::Lt, hi)]
}

fn exact_or_wild(p: &Partial) -> Vec<Comparator> {
    match (p.major, p.minor, p.patch) {
        (None, ..) => any_version(),
        (Some(a), None, _) => vec![
            cmp(Op::Ge, SemVer::new(a, 0, 0)),
            cmp(Op::Lt, SemVer::new(a + 1, 0, 0)),
        ],
        (Some(a), Some(b), None) => {
            vec![
                cmp(Op::Ge, SemVer::new(a, b, 0)),
                cmp(Op::Lt, SemVer::new(a, b + 1, 0)),
            ]
        }
        _ => vec![cmp(Op::Eq, p.lower())],
    }
}

fn with_op(op: &str, p: &Partial) -> Option<Vec<Comparator>> {
    let bump_up = |p: &Partial| -> Option<SemVer> {
        // The smallest version above everything the partial covers.
        match (p.major, p.minor, p.patch) {
            (None, ..) => None,
            (Some(a), None, _) => Some(SemVer::new(a + 1, 0, 0)),
            (Some(a), Some(b), None) => Some(SemVer::new(a, b + 1, 0)),
            _ => None,
        }
    };
    Some(match op {
        ">" => match bump_up(p) {
            Some(v) => vec![cmp(Op::Ge, v)],
            None if p.major.is_none() => vec![cmp(Op::Lt, SemVer::new(0, 0, 0))],
            None => vec![cmp(Op::Gt, p.lower())],
        },
        ">=" => vec![cmp(Op::Ge, p.lower())],
        "<" => vec![cmp(Op::Lt, p.lower())],
        "<=" => match bump_up(p) {
            Some(v) => vec![cmp(Op::Lt, v)],
            None if p.major.is_none() => any_version(),
            None => vec![cmp(Op::Le, p.lower())],
        },
        _ => return None,
    })
}

fn parse_comparator_token(tok: &str, flavor: RangeFlavor) -> Option<Vec<Comparator>> {
    let composer = flavor == RangeFlavor::Composer;
    let t = tok.trim();
    let t = if composer {
        t.split('@').next().unwrap_or(t)
    } else {
        t
    };
    if t.is_empty() {
        return Some(any_version());
    }
    for op in [">=", "<=", "~>", ">", "<", "^", "~", "=", "!="] {
        if let Some(rest) = t.strip_prefix(op) {
            if op == "!=" {
                return None;
            }
            let p = parse_partial(rest)?;
            return match op {
                "^" => Some(caret(&p)),
                "~" | "~>" => Some(tilde(&p, composer)),
                "=" => Some(exact_or_wild(&p)),
                _ => with_op(op, &p),
            };
        }
    }
    let p = parse_partial(t)?;
    if flavor == RangeFlavor::Cargo
        && (p.minor.is_some() || p.major.is_some())
        && !t.contains(['x', 'X', '*'])
    {
        return Some(caret(&p));
    }
    Some(exact_or_wild(&p))
}

fn parse_and_set(text: &str, flavor: RangeFlavor) -> Option<Vec<Comparator>> {
    // Hyphen range: `1.2.3 - 2.3.4` (spaces required).
    if let Some((a, b)) = text.split_once(" - ") {
        let lo = parse_partial(a)?;
        let hi = parse_partial(b)?;
        let mut out = vec![cmp(Op::Ge, lo.lower())];
        match (hi.major, hi.minor, hi.patch) {
            (None, ..) => {}
            (Some(x), None, _) => out.push(cmp(Op::Lt, SemVer::new(x + 1, 0, 0))),
            (Some(x), Some(y), None) => out.push(cmp(Op::Lt, SemVer::new(x, y + 1, 0))),
            _ => out.push(cmp(Op::Le, hi.lower())),
        }
        return Some(out);
    }
    // Tokenize: split on whitespace and commas, gluing a bare operator to the next token.
    let mut tokens: Vec<String> = Vec::new();
    let mut pending = String::new();
    for raw in text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
    {
        if matches!(raw, ">" | ">=" | "<" | "<=" | "=" | "^" | "~" | "~>") {
            pending = raw.to_owned();
            continue;
        }
        tokens.push(format!("{pending}{raw}"));
        pending.clear();
    }
    if !pending.is_empty() {
        return None;
    }
    if tokens.is_empty() {
        return Some(any_version());
    }
    let mut out = Vec::new();
    for t in tokens {
        out.extend(parse_comparator_token(&t, flavor)?);
    }
    Some(out)
}

impl Range {
    /// Parses a range in the given grammar. Returns `None` for tags, URLs and anything else
    /// that is not a version range (`latest`, `git+https://...`, `workspace:*`, `dev-master`).
    pub fn parse(text: &str, flavor: RangeFlavor) -> Option<Range> {
        let t = text.trim();
        let alternatives: Vec<&str> = match flavor {
            RangeFlavor::Cargo => vec![t],
            RangeFlavor::Npm => t.split("||").collect(),
            RangeFlavor::Composer => t.split("||").flat_map(|s| s.split(" | ")).collect(),
        };
        let mut sets = Vec::new();
        for alt in alternatives {
            sets.push(parse_and_set(alt.trim(), flavor)?);
        }
        Some(Range { sets, flavor })
    }

    /// True when `version` is allowed by some alternative.
    ///
    /// For npm and Cargo a pre-release only matches an alternative that names a pre-release of
    /// the same `major.minor.patch`.
    pub fn matches(&self, version: &SemVer) -> bool {
        self.sets.iter().any(|set| {
            let ok = set.iter().all(|c| match c.op {
                Op::Lt => version < &c.version,
                Op::Le => version <= &c.version,
                Op::Gt => version > &c.version,
                Op::Ge => version >= &c.version,
                Op::Eq => version == &c.version,
            });
            if !ok {
                return false;
            }
            if version.is_prerelease() && self.flavor != RangeFlavor::Composer {
                return set
                    .iter()
                    .any(|c| c.version.is_prerelease() && c.version.same_triple(version));
            }
            true
        })
    }

    /// The lowest version the range allows, when it has a lower bound.
    pub fn min_version(&self) -> Option<SemVer> {
        self.sets
            .iter()
            .filter_map(|set| {
                set.iter()
                    .filter(|c| matches!(c.op, Op::Ge | Op::Gt | Op::Eq))
                    .map(|c| c.version.clone())
                    .max()
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(s: &str) -> SemVer {
        SemVer::parse(s).unwrap()
    }

    fn sat(flavor: RangeFlavor, range: &str, v: &str) -> bool {
        Range::parse(range, flavor).unwrap().matches(&sv(v))
    }

    #[test]
    fn semver_precedence_chain() {
        let chain = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.1.0",
            "2.0.0",
        ];
        for w in chain.windows(2) {
            assert!(sv(w[0]) < sv(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(sv("1.0.0+a"), sv("v1.0.0+b"));
        assert_eq!(sv("1.2"), sv("1.2.0"));
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "x.y.z",
            "1..2",
            "1.2.3.4.5",
            "1.2.3-",
            "1.2.3-a..b",
            "latest",
        ] {
            assert!(SemVer::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn go_pseudo_versions() {
        let p = go_pseudo("v0.0.0-20191109021931-daa7c04131f5").unwrap();
        assert_eq!(p.commit, "daa7c04131f5");
        assert_eq!(p.timestamp, "20191109021931");
        assert!(go_pseudo("v1.2.4-0.20191109021931-daa7c04131f5").is_some());
        assert!(go_pseudo("v1.2.4-pre.0.20191109021931-daa7c04131f5+incompatible").is_some());
        assert!(go_pseudo("v1.2.3").is_none());
        assert!(go_pseudo("v1.2.3-rc.1").is_none());
        assert!(
            sv("v0.0.0-20191109021931-daa7c04131f5") < sv("v0.0.0-20200101000000-aaaaaaaaaaaa")
        );
        assert!(sv("v1.2.4-0.20191109021931-daa7c04131f5") < sv("v1.2.4"));
    }

    #[test]
    fn npm_ranges() {
        use RangeFlavor::Npm;
        assert!(sat(Npm, "^1.2.3", "1.9.0"));
        assert!(!sat(Npm, "^1.2.3", "2.0.0"));
        assert!(!sat(Npm, "^1.2.3", "1.2.2"));
        assert!(sat(Npm, "^0.2.3", "0.2.9"));
        assert!(!sat(Npm, "^0.2.3", "0.3.0"));
        assert!(sat(Npm, "^0.0.3", "0.0.3"));
        assert!(!sat(Npm, "^0.0.3", "0.0.4"));
        assert!(sat(Npm, "~1.2.3", "1.2.9"));
        assert!(!sat(Npm, "~1.2.3", "1.3.0"));
        assert!(sat(Npm, "~1.2", "1.2.7"));
        assert!(sat(Npm, "1.x", "1.5.5"));
        assert!(sat(Npm, "*", "9.9.9"));
        assert!(sat(Npm, ">=1.2.0 <1.5.0", "1.4.9"));
        assert!(!sat(Npm, ">=1.2.0 <1.5.0", "1.5.0"));
        assert!(sat(Npm, ">= 1.2.0", "3.0.0"));
        assert!(sat(Npm, "1.2.3 - 2.3.4", "2.3.4"));
        assert!(sat(Npm, "1.2.3 - 2.3", "2.3.9"));
        assert!(sat(Npm, "<1.0.0 || >=2.0.0", "2.1.0"));
        assert!(!sat(Npm, "<1.0.0 || >=2.0.0", "1.5.0"));
        assert!(!sat(Npm, "^1.2.3", "1.3.0-beta.1"));
        assert!(sat(Npm, "^1.3.0-beta.1", "1.3.0-beta.2"));
        assert!(sat(Npm, "1.2.3", "1.2.3"));
        assert!(Range::parse("latest", Npm).is_none());
        assert!(Range::parse("git+https://example.com/x.git", Npm).is_none());
        assert!(Range::parse("workspace:*", Npm).is_none());
    }

    #[test]
    fn cargo_requirements() {
        use RangeFlavor::Cargo;
        assert!(sat(Cargo, "1.2.3", "1.9.9"));
        assert!(!sat(Cargo, "1.2.3", "2.0.0"));
        assert!(sat(Cargo, "0.3", "0.3.9"));
        assert!(!sat(Cargo, "0.3", "0.4.0"));
        assert!(sat(Cargo, "=1.2.3", "1.2.3"));
        assert!(!sat(Cargo, "=1.2.3", "1.2.4"));
        assert!(sat(Cargo, ">=1.2, <1.5", "1.4.0"));
        assert!(!sat(Cargo, ">=1.2, <1.5", "1.5.0"));
        assert!(sat(Cargo, "~1.2.3", "1.2.8"));
        assert!(sat(Cargo, "1.*", "1.4.0"));
        assert!(sat(Cargo, "*", "0.0.1"));
        assert_eq!(
            Range::parse("^1.4", Cargo).unwrap().min_version(),
            Some(sv("1.4.0"))
        );
    }

    #[test]
    fn composer_constraints() {
        use RangeFlavor::Composer;
        assert!(sat(Composer, "^2.0", "2.9.0"));
        assert!(sat(Composer, "~1.2", "1.9.0"));
        assert!(!sat(Composer, "~1.2", "2.0.0"));
        assert!(!sat(Composer, "~1.2.3", "1.3.0"));
        assert!(sat(Composer, ">=1.0 <2.0 || >=3.0", "3.1.0"));
        assert!(sat(Composer, "1.0.*", "1.0.7"));
        assert!(sat(Composer, ">=1.0,<2.0", "1.5.0"));
        assert!(Range::parse("dev-master", Composer).is_none());
    }
}
