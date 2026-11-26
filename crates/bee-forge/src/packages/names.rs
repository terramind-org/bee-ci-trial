//! Package name and version rules of each ecosystem: validation, the normalized key that makes
//! two spellings the same package, and version ordering where a registry needs it.
//!
//! All functions are pure. Validation failures are `INVALID_ARGUMENT` (`bad_name`,
//! `bad_version`), the subcodes the proto documents for `BeginPackageUpload`.

use std::cmp::Ordering;

use super::model::Eco;
use crate::error::{ForgeError, Result};

fn bad_name(eco: Eco, name: &str, why: &str) -> ForgeError {
    ForgeError::invalid(
        "bad_name",
        format!("`{name}` is not a valid {} package name: {why}", eco.slug()),
    )
}

fn bad_version(eco: Eco, v: &str) -> ForgeError {
    ForgeError::invalid(
        "bad_version",
        format!("`{v}` is not a valid {} version", eco.slug()),
    )
}

fn all_of(s: &str, ok: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.chars().all(ok)
}

/// Checks `name` against the ecosystem's naming rules.
pub fn validate_name(eco: Eco, name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 255 {
        return Err(bad_name(eco, name, "must be 1 to 255 characters"));
    }
    match eco {
        Eco::Container => {
            for comp in name.split('/') {
                let ok = !comp.is_empty()
                    && comp
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphanumeric())
                    && comp
                        .chars()
                        .last()
                        .is_some_and(|c| c.is_ascii_alphanumeric())
                    && comp.chars().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
                    });
                if !ok {
                    return Err(bad_name(
                        eco,
                        name,
                        "components are lower-case alphanumerics separated by `.`, `_` or `-`",
                    ));
                }
            }
            Ok(())
        }
        Eco::Npm => {
            let (scope, rest) = match name.strip_prefix('@') {
                Some(s) => match s.split_once('/') {
                    Some((sc, r)) => (Some(sc), r),
                    None => return Err(bad_name(eco, name, "a scoped name is `@scope/name`")),
                },
                None => (None, name),
            };
            let part_ok = |p: &str| {
                all_of(p, |c| {
                    c.is_ascii_lowercase()
                        || c.is_ascii_digit()
                        || matches!(c, '-' | '.' | '_' | '~')
                }) && !p.starts_with('.')
                    && !p.starts_with('_')
            };
            if !part_ok(rest) || scope.is_some_and(|s| !part_ok(s)) {
                return Err(bad_name(
                    eco,
                    name,
                    "lower-case URL-safe characters, not starting with `.` or `_`",
                ));
            }
            if name.len() > 214 {
                return Err(bad_name(eco, name, "at most 214 characters"));
            }
            Ok(())
        }
        Eco::Maven => {
            let Some((g, a)) = name.split_once(':') else {
                return Err(bad_name(eco, name, "use `groupId:artifactId`"));
            };
            let ok = |s: &str| {
                all_of(s, |c| {
                    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
                })
            };
            if !ok(g) || !ok(a) || g.starts_with('.') || g.ends_with('.') || g.contains("..") {
                return Err(bad_name(eco, name, "invalid groupId or artifactId"));
            }
            Ok(())
        }
        Eco::Nuget => {
            if all_of(name, |c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
            }) && !name.starts_with('.')
                && !name.ends_with('.')
            {
                Ok(())
            } else {
                Err(bad_name(eco, name, "letters, digits, `.`, `_` and `-`"))
            }
        }
        Eco::Rubygems => {
            if all_of(name, |c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
            }) && name.chars().any(|c| c.is_ascii_alphabetic())
            {
                Ok(())
            } else {
                Err(bad_name(eco, name, "letters, digits, `.`, `_` and `-`"))
            }
        }
        Eco::Cargo => {
            let mut cs = name.chars();
            let first_ok = cs.next().is_some_and(|c| c.is_ascii_alphabetic());
            if first_ok
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            {
                Ok(())
            } else {
                Err(bad_name(
                    eco,
                    name,
                    "start with a letter; letters, digits, `-` and `_`; at most 64 characters",
                ))
            }
        }
    }
}

/// The key under which two spellings of a name are the same package.
pub fn name_key(eco: Eco, name: &str) -> String {
    let k = match eco {
        Eco::Maven => name.to_string(),
        Eco::Cargo => name.to_lowercase().replace('_', "-"),
        _ => name.to_lowercase(),
    };
    format!("{}:{k}", eco.slug())
}

/// A parsed semantic version (2.0.0).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemVer {
    /// Major, minor, patch.
    pub core: [u64; 3],
    /// Dot-separated pre-release identifiers.
    pub pre: Vec<String>,
    /// Build metadata (ignored in ordering).
    pub build: String,
}

impl SemVer {
    /// Parses a strict semantic version (no `v` prefix, no leading zeros).
    pub fn parse(s: &str) -> Option<SemVer> {
        let (rest, build) = match s.split_once('+') {
            Some((r, b)) => (r, b.to_string()),
            None => (s, String::new()),
        };
        if s.contains('+') && (build.is_empty() || !build.split('.').all(valid_ident)) {
            return None;
        }
        let (core, pre) = match rest.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (rest, None),
        };
        let nums: Vec<&str> = core.split('.').collect();
        if nums.len() != 3 {
            return None;
        }
        let mut out = [0u64; 3];
        for (i, n) in nums.iter().enumerate() {
            if n.is_empty()
                || !n.chars().all(|c| c.is_ascii_digit())
                || (n.len() > 1 && n.starts_with('0'))
            {
                return None;
            }
            out[i] = n.parse().ok()?;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) => {
                let ids: Vec<String> = p.split('.').map(str::to_string).collect();
                if !ids.iter().all(|i| {
                    valid_ident(i)
                        && !(i.chars().all(|c| c.is_ascii_digit())
                            && i.len() > 1
                            && i.starts_with('0'))
                }) {
                    return None;
                }
                ids
            }
        };
        Some(SemVer {
            core: out,
            pre,
            build,
        })
    }
}

fn valid_ident(s: &str) -> bool {
    all_of(s, |c| c.is_ascii_alphanumeric() || c == '-')
}

impl Ord for SemVer {
    fn cmp(&self, other: &SemVer) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let o = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &SemVer) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// NuGet's normalized version: up to four numeric parts, the fourth dropped when zero,
/// three parts at least, leading zeros removed, build metadata removed, lower-cased.
pub fn nuget_normalize(v: &str) -> Option<String> {
    let v = v.trim();
    let (main, _build) = v.split_once('+').unwrap_or((v, ""));
    let (nums, pre) = match main.split_once('-') {
        Some((n, p)) => (n, Some(p)),
        None => (main, None),
    };
    let mut parts: Vec<u64> = Vec::new();
    for p in nums.split('.') {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        parts.push(p.parse().ok()?);
    }
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    while parts.len() < 3 {
        parts.push(0);
    }
    if parts.len() == 4 && parts[3] == 0 {
        parts.pop();
    }
    let mut out = parts
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".");
    if let Some(p) = pre {
        if p.is_empty() || !p.split('.').all(valid_ident) {
            return None;
        }
        out.push('-');
        out.push_str(&p.to_lowercase());
    }
    Some(out)
}

fn gem_version_ok(v: &str) -> bool {
    // Gem::Version: digits and letters separated by dots, optionally `-` pre-release sugar.
    let v = v.replace('-', ".pre.");
    !v.is_empty()
        && v.split('.')
            .all(|p| all_of(p, |c| c.is_ascii_alphanumeric()))
        && v.chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn maven_version_ok(v: &str) -> bool {
    all_of(v, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+')
    })
}

fn oci_tag_ok(v: &str) -> bool {
    v.len() <= 128
        && v.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Validates a version string (a container "version" is a digest or tag).
pub fn validate_version(eco: Eco, v: &str) -> Result<()> {
    let ok = match eco {
        Eco::Npm | Eco::Cargo => SemVer::parse(v).is_some(),
        Eco::Maven => maven_version_ok(v),
        Eco::Nuget => nuget_normalize(v).is_some(),
        Eco::Rubygems => gem_version_ok(v),
        Eco::Container => oci_tag_ok(v) || is_digest(v),
    };
    if ok { Ok(()) } else { Err(bad_version(eco, v)) }
}

/// The key under which a version is unique within its package.
pub fn version_key(eco: Eco, v: &str) -> String {
    match eco {
        Eco::Nuget => nuget_normalize(v).unwrap_or_else(|| v.to_lowercase()),
        Eco::Maven | Eco::Rubygems => v.to_string(),
        Eco::Npm | Eco::Cargo => v.to_string(),
        Eco::Container => v.to_string(),
    }
}

/// Whether `s` is a `sha256:<64 hex>` digest.
pub fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')))
}

#[derive(Debug, PartialEq, Eq)]
enum Item {
    Num(u64),
    Str(String),
}

fn qualifier_rank(s: &str) -> i32 {
    match s {
        "alpha" | "a" => 1,
        "beta" | "b" => 2,
        "milestone" | "m" => 3,
        "rc" | "cr" => 4,
        "snapshot" => 5,
        "" | "ga" | "final" | "release" => 6,
        "sp" => 7,
        _ => 8,
    }
}

fn maven_items(v: &str) -> Vec<Item> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut numeric = false;
    let flush = |cur: &mut String, numeric: bool, items: &mut Vec<Item>| {
        if cur.is_empty() {
            return;
        }
        if numeric {
            items.push(Item::Num(cur.parse().unwrap_or(0)));
        } else {
            items.push(Item::Str(cur.to_lowercase()));
        }
        cur.clear();
    };
    for c in v.chars() {
        if c == '.' || c == '-' || c == '_' {
            flush(&mut cur, numeric, &mut items);
            continue;
        }
        let d = c.is_ascii_digit();
        if !cur.is_empty() && d != numeric {
            flush(&mut cur, numeric, &mut items);
        }
        numeric = d;
        cur.push(c);
    }
    flush(&mut cur, numeric, &mut items);
    while matches!(items.last(), Some(Item::Num(0))) {
        items.pop();
    }
    items
}

/// Orders two Maven versions with the usual rules: numeric parts compare numerically,
/// qualifiers `alpha < beta < milestone < rc < snapshot < release < sp`, a release sorts
/// after its pre-releases.
pub fn maven_cmp(a: &str, b: &str) -> Ordering {
    let (x, y) = (maven_items(a), maven_items(b));
    for i in 0..x.len().max(y.len()) {
        let o = match (x.get(i), y.get(i)) {
            (Some(Item::Num(p)), Some(Item::Num(q))) => p.cmp(q),
            (Some(Item::Str(p)), Some(Item::Str(q))) => {
                qualifier_rank(p).cmp(&qualifier_rank(q)).then_with(|| {
                    if qualifier_rank(p) == 8 {
                        p.cmp(q)
                    } else {
                        Ordering::Equal
                    }
                })
            }
            (Some(Item::Num(_)), Some(Item::Str(_))) => Ordering::Greater,
            (Some(Item::Str(_)), Some(Item::Num(_))) => Ordering::Less,
            (Some(Item::Num(p)), None) => p.cmp(&0),
            (None, Some(Item::Num(q))) => 0.cmp(q),
            (Some(Item::Str(p)), None) => qualifier_rank(p).cmp(&6),
            (None, Some(Item::Str(q))) => 6.cmp(&qualifier_rank(q)),
            (None, None) => Ordering::Equal,
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_validate_per_ecosystem() {
        assert!(validate_name(Eco::Npm, "@acme/widget").is_ok());
        assert!(validate_name(Eco::Npm, "Widget").is_err());
        assert!(validate_name(Eco::Npm, "@acme").is_err());
        assert!(validate_name(Eco::Container, "acme/app-1.0").is_ok());
        assert!(validate_name(Eco::Container, "Acme/App").is_err());
        assert!(validate_name(Eco::Maven, "com.acme:lib").is_ok());
        assert!(validate_name(Eco::Maven, "com.acme").is_err());
        assert!(validate_name(Eco::Cargo, "my_crate").is_ok());
        assert!(validate_name(Eco::Cargo, "1crate").is_err());
        assert_eq!(name_key(Eco::Cargo, "My_Crate"), "cargo:my-crate");
        assert_eq!(name_key(Eco::Nuget, "Acme.Lib"), "nuget:acme.lib");
    }

    #[test]
    fn semver_orders() {
        let v = |s: &str| SemVer::parse(s).unwrap();
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") < v("1.0.0-beta"));
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert!(v("1.2.10") > v("1.2.9"));
        assert!(SemVer::parse("1.02.3").is_none());
        assert!(SemVer::parse("1.2").is_none());
        assert!(SemVer::parse("v1.2.3").is_none());
        assert!(SemVer::parse("1.2.3+build.5").is_some());
    }

    #[test]
    fn nuget_versions_normalize() {
        assert_eq!(nuget_normalize("1.0").as_deref(), Some("1.0.0"));
        assert_eq!(nuget_normalize("1.0.0.0").as_deref(), Some("1.0.0"));
        assert_eq!(
            nuget_normalize("1.02.3-Beta+x").as_deref(),
            Some("1.2.3-beta")
        );
        assert_eq!(nuget_normalize("1.2.3.4").as_deref(), Some("1.2.3.4"));
        assert!(nuget_normalize("a.b").is_none());
    }

    #[test]
    fn maven_versions_order() {
        use Ordering::*;
        assert_eq!(maven_cmp("1.0", "1.0.0"), Equal);
        assert_eq!(maven_cmp("1.0-alpha", "1.0-beta"), Less);
        assert_eq!(maven_cmp("1.0-rc1", "1.0"), Less);
        assert_eq!(maven_cmp("1.0-SNAPSHOT", "1.0"), Less);
        assert_eq!(maven_cmp("1.10", "1.9"), Greater);
        assert_eq!(maven_cmp("1.0-sp1", "1.0"), Greater);
    }

    #[test]
    fn digests() {
        assert!(is_digest(&format!("sha256:{}", "a".repeat(64))));
        assert!(!is_digest("sha256:abc"));
    }
}
