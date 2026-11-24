//! Per-ecosystem version ordering and requirement matching.
//!
//! [`compare`] is a total order for every ecosystem: an unparseable version sorts below every
//! parseable one, and two unparseable versions compare as strings.

use std::cmp::Ordering;

use crate::model::Ecosystem;

pub mod maven;
pub mod pep440;
pub mod ruby;
pub mod semver;

pub use maven::{IntervalRange, MavenVersion};
pub use pep440::{Pep440, Specifiers};
pub use ruby::{GemRequirement, GemVersion};
pub use semver::{GoPseudo, Range, RangeFlavor, SemVer, go_pseudo};

fn cmp_parsed<T: Ord>(parse: impl Fn(&str) -> Option<T>, a: &str, b: &str) -> Ordering {
    match (parse(a), parse(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

/// Compares two versions using the ordering rules of `ecosystem`.
///
/// Cargo, npm, Go and NuGet use SemVer precedence (Go pseudo-versions order correctly because
/// their timestamp is a pre-release identifier), PyPI uses PEP 440, Maven uses
/// `ComparableVersion`, RubyGems uses `Gem::Version`, Composer uses SemVer on its numeric
/// form. GitHub Actions refs are compared as SemVer when they look like versions and as strings
/// otherwise.
pub fn compare(ecosystem: Ecosystem, a: &str, b: &str) -> Ordering {
    match ecosystem {
        Ecosystem::PyPI => cmp_parsed(Pep440::parse, a, b),
        Ecosystem::Maven => cmp_parsed(MavenVersion::parse, a, b),
        Ecosystem::RubyGems => cmp_parsed(GemVersion::parse, a, b),
        _ => cmp_parsed(SemVer::parse, a, b),
    }
}

/// True when `version` parses under the ecosystem's grammar.
pub fn is_valid(ecosystem: Ecosystem, version: &str) -> bool {
    match ecosystem {
        Ecosystem::PyPI => Pep440::parse(version).is_some(),
        Ecosystem::Maven => MavenVersion::parse(version).is_some(),
        Ecosystem::RubyGems => GemVersion::parse(version).is_some(),
        _ => SemVer::parse(version).is_some(),
    }
}

/// True when the version is a pre-release in the ecosystem's terms.
pub fn is_prerelease(ecosystem: Ecosystem, version: &str) -> bool {
    match ecosystem {
        Ecosystem::PyPI => Pep440::parse(version).is_some_and(|v| v.is_prerelease()),
        Ecosystem::RubyGems => GemVersion::parse(version).is_some_and(|v| v.is_prerelease()),
        Ecosystem::Maven => {
            let l = version.to_ascii_lowercase();
            ["alpha", "beta", "rc", "snapshot", "milestone", "-m"]
                .iter()
                .any(|q| l.contains(q))
        }
        _ => SemVer::parse(version).is_some_and(|v| v.is_prerelease()),
    }
}

/// Whether `version` satisfies the declared `requirement`.
///
/// Returns `None` when the requirement is not a version range (a tag, URL or workspace
/// reference) or either side does not parse. Requirement grammars: npm ranges, Cargo
/// requirements, PEP 440 specifiers, RubyGems requirements, Composer constraints, Maven and
/// NuGet intervals. Go requirements are minimum versions; GitHub Actions requirements are exact
/// refs.
pub fn satisfies(ecosystem: Ecosystem, version: &str, requirement: &str) -> Option<bool> {
    match ecosystem {
        Ecosystem::Npm => {
            Some(Range::parse(requirement, RangeFlavor::Npm)?.matches(&SemVer::parse(version)?))
        }
        Ecosystem::Cargo => {
            Some(Range::parse(requirement, RangeFlavor::Cargo)?.matches(&SemVer::parse(version)?))
        }
        Ecosystem::Composer => Some(
            Range::parse(requirement, RangeFlavor::Composer)?.matches(&SemVer::parse(version)?),
        ),
        Ecosystem::PyPI => Some(Specifiers::parse(requirement)?.matches(&Pep440::parse(version)?)),
        Ecosystem::RubyGems => {
            Some(GemRequirement::parse(requirement)?.matches(&GemVersion::parse(version)?))
        }
        Ecosystem::Maven => Some(IntervalRange::parse(requirement, false)?.matches(version)),
        Ecosystem::NuGet => Some(IntervalRange::parse(requirement, true)?.matches(version)),
        Ecosystem::Go => Some(SemVer::parse(version)? >= SemVer::parse(requirement)?),
        Ecosystem::GitHubActions => Some(version == requirement),
    }
}

/// The lowest version a requirement allows, as a string, when it has a lower bound.
///
/// This is the version recorded for a dependency that has no lockfile.
pub fn min_version(ecosystem: Ecosystem, requirement: &str) -> Option<String> {
    match ecosystem {
        Ecosystem::Npm => Range::parse(requirement, RangeFlavor::Npm)?
            .min_version()
            .map(|v| v.to_core_string()),
        Ecosystem::Cargo => Range::parse(requirement, RangeFlavor::Cargo)?
            .min_version()
            .map(|v| v.to_core_string()),
        Ecosystem::Composer => Range::parse(requirement, RangeFlavor::Composer)?
            .min_version()
            .map(|v| v.to_core_string()),
        Ecosystem::PyPI => Specifiers::parse(requirement)?.min_version(),
        Ecosystem::RubyGems => GemRequirement::parse(requirement)?.min_version(),
        Ecosystem::Maven => IntervalRange::parse(requirement, false)?.min_version(),
        Ecosystem::NuGet => IntervalRange::parse(requirement, true)?.min_version(),
        Ecosystem::Go | Ecosystem::GitHubActions => {
            let r = requirement.trim();
            (!r.is_empty()).then(|| r.to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn dispatch_orders_per_ecosystem() {
        use Ecosystem::*;
        assert_eq!(compare(Cargo, "1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(compare(PyPI, "1.0rc1", "1.0"), Ordering::Less);
        assert_eq!(compare(Maven, "1.0-SNAPSHOT", "1.0"), Ordering::Less);
        assert_eq!(compare(RubyGems, "1.0.pre", "1.0"), Ordering::Less);
        assert_eq!(
            compare(Go, "v1.2.3", "v1.2.4-0.20200101000000-aaaaaaaaaaaa"),
            Ordering::Less
        );
        assert_eq!(compare(Npm, "garbage", "0.0.1"), Ordering::Less);
        assert_eq!(compare(Npm, "a", "b"), Ordering::Less);
    }

    #[test]
    fn satisfies_and_min_version() {
        use Ecosystem::*;
        assert_eq!(satisfies(Npm, "1.2.5", "^1.2.0"), Some(true));
        assert_eq!(satisfies(Npm, "1.2.5", "latest"), None);
        assert_eq!(satisfies(Cargo, "0.9.0", "0.8"), Some(false));
        assert_eq!(satisfies(PyPI, "2.28.1", "==2.28.*"), Some(true));
        assert_eq!(satisfies(Go, "v1.5.0", "v1.4.0"), Some(true));
        assert_eq!(min_version(Npm, "^1.2"), Some("1.2.0".to_owned()));
        assert_eq!(min_version(Cargo, "1"), Some("1.0.0".to_owned()));
        assert_eq!(min_version(PyPI, "==2.0.1"), Some("2.0.1".to_owned()));
        assert_eq!(min_version(Maven, "[1.0,2.0)"), Some("1.0".to_owned()));
    }

    fn semverish() -> impl Strategy<Value = String> {
        (
            0u64..5,
            0u64..5,
            0u64..5,
            proptest::option::of("(alpha|beta|rc)(\\.[0-9])?"),
        )
            .prop_map(|(a, b, c, pre)| match pre {
                Some(p) => format!("{a}.{b}.{c}-{p}"),
                None => format!("{a}.{b}.{c}"),
            })
    }

    fn pepish() -> impl Strategy<Value = String> {
        (
            0u64..4,
            0u64..4,
            proptest::option::of("(a|b|rc)[0-9]"),
            proptest::option::of(0u64..3),
            proptest::option::of(0u64..3),
        )
            .prop_map(|(a, b, pre, post, dev)| {
                let mut s = format!("{a}.{b}");
                if let Some(p) = pre {
                    s.push_str(&p);
                }
                if let Some(p) = post {
                    s.push_str(&format!(".post{p}"));
                }
                if let Some(d) = dev {
                    s.push_str(&format!(".dev{d}"));
                }
                s
            })
    }

    fn mavenish() -> impl Strategy<Value = String> {
        (
            0u64..4,
            0u64..4,
            proptest::option::of(0u64..4),
            proptest::option::of("(alpha|beta|rc|snapshot|sp)[0-9]?"),
        )
            .prop_map(|(a, b, c, q)| {
                let mut s = format!("{a}.{b}");
                if let Some(c) = c {
                    s.push_str(&format!(".{c}"));
                }
                if let Some(q) = q {
                    s.push_str(&format!("-{q}"));
                }
                s
            })
    }

    fn gemish() -> impl Strategy<Value = String> {
        (
            0u64..4,
            0u64..4,
            proptest::option::of(0u64..4),
            proptest::option::of("(a|b|rc|pre)[0-9]?"),
        )
            .prop_map(|(a, b, c, q)| {
                let mut s = format!("{a}.{b}");
                if let Some(c) = c {
                    s.push_str(&format!(".{c}"));
                }
                if let Some(q) = q {
                    s.push_str(&format!(".{q}"));
                }
                s
            })
    }

    fn check_total_order(eco: Ecosystem, a: &str, b: &str, c: &str) -> Result<(), TestCaseError> {
        let ab = compare(eco, a, b);
        prop_assert_eq!(ab, compare(eco, b, a).reverse());
        prop_assert_eq!(compare(eco, a, a), Ordering::Equal);
        let bc = compare(eco, b, c);
        let ac = compare(eco, a, c);
        if ab != Ordering::Greater && bc != Ordering::Greater {
            prop_assert!(ac != Ordering::Greater, "{a} <= {b} <= {c} but {a} > {c}");
        }
        if ab == Ordering::Equal {
            prop_assert_eq!(ac, bc);
        }
        Ok(())
    }

    proptest! {
        #[test]
        fn semver_is_a_total_order(a in semverish(), b in semverish(), c in semverish()) {
            check_total_order(Ecosystem::Cargo, &a, &b, &c)?;
            check_total_order(Ecosystem::Go, &a, &b, &c)?;
        }

        #[test]
        fn pep440_is_a_total_order(a in pepish(), b in pepish(), c in pepish()) {
            check_total_order(Ecosystem::PyPI, &a, &b, &c)?;
        }

        #[test]
        fn maven_is_a_total_order(a in mavenish(), b in mavenish(), c in mavenish()) {
            check_total_order(Ecosystem::Maven, &a, &b, &c)?;
        }

        #[test]
        fn rubygems_is_a_total_order(a in gemish(), b in gemish(), c in gemish()) {
            check_total_order(Ecosystem::RubyGems, &a, &b, &c)?;
        }

        #[test]
        fn release_bump_is_greater(a in 0u64..50, b in 0u64..50, c in 0u64..50) {
            for eco in [Ecosystem::Cargo, Ecosystem::PyPI, Ecosystem::Maven, Ecosystem::RubyGems, Ecosystem::Npm] {
                let lo = format!("{a}.{b}.{c}");
                let hi = format!("{a}.{b}.{}", c + 1);
                prop_assert_eq!(compare(eco, &lo, &hi), Ordering::Less);
                prop_assert_eq!(compare(eco, &hi, &lo), Ordering::Greater);
            }
        }

        #[test]
        fn npm_caret_never_crosses_major(a in 1u64..9, b in 0u64..9, c in 0u64..9, d in 0u64..9) {
            let req = format!("^{a}.{b}.{c}");
            let next_major = format!("{}.{b}.{d}", a + 1);
            prop_assert_eq!(satisfies(Ecosystem::Npm, &next_major, &req), Some(false));
            let same = format!("{a}.{}.{d}", b + 1);
            prop_assert_eq!(satisfies(Ecosystem::Npm, &same, &req), Some(true));
        }
    }
}
