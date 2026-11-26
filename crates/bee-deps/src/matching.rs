//! Vulnerability matching: a [`DependencyGraph`] against an [`AdvisorySource`].
//!
//! A [`Finding`] is the unit the forge turns into a `bee.v1.DependencyAlert`: advisory id and
//! aliases, package, installed version, fixed versions, severity and the dependency path.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::cvss::Severity;
use crate::model::{DependencyGraph, Ecosystem, Package, Relationship, Scope};
use crate::osv::{Advisory, AdvisorySource, Affected, Event, VersionRange};
use crate::version;

/// Answers commit ancestry questions for OSV `GIT` ranges.
pub trait GitAncestry {
    /// Whether `ancestor` is an ancestor of, or equal to, `descendant` in the repository at
    /// `repo`. `None` when the repository or either commit is unknown.
    fn is_ancestor(&self, repo: &str, ancestor: &str, descendant: &str) -> Option<bool>;
}

/// Options for [`match_graph`].
#[derive(Default)]
pub struct MatchOptions<'a> {
    /// Ancestry oracle for `GIT` ranges. Without one, a `GIT` range matches only when the
    /// package's commit equals one of the range's `introduced` or `last_affected` commits.
    pub git: Option<&'a dyn GitAncestry>,
}

/// One vulnerable package occurrence.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    /// The advisory id; a GHSA id when the alias group has one.
    pub advisory_id: String,
    /// Every other id of the same vulnerability (CVE, GHSA, vendor ids), sorted.
    pub aliases: Vec<String>,
    /// One-line summary.
    pub summary: String,
    /// Severity rating.
    pub severity: Severity,
    /// CVSS base score, when known.
    pub cvss_score: Option<f64>,
    /// CVSS vector, when known.
    pub cvss_vector: Option<String>,
    /// The package ecosystem.
    pub ecosystem: Ecosystem,
    /// The package name.
    pub package: String,
    /// The package URL.
    pub purl: String,
    /// The installed (or declared minimum) version.
    pub installed_version: String,
    /// Versions that fix the vulnerability and are newer than the installed one, ascending.
    pub fixed_versions: Vec<String>,
    /// Runtime or development use.
    pub scope: Scope,
    /// Direct or indirect.
    pub relationship: Relationship,
    /// The manifest or lockfile that declares the package.
    pub manifest_path: String,
    /// Purls from a direct dependency down to the package.
    pub dependency_path: Vec<String>,
}

fn is_hex_commit(s: &str) -> bool {
    s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The commit a package version identifies, when it is one.
fn commit_of(eco: Ecosystem, version: &str) -> Option<String> {
    if eco == Ecosystem::Go
        && let Some(p) = version::go_pseudo(version)
    {
        return Some(p.commit);
    }
    is_hex_commit(version).then(|| version.to_ascii_lowercase())
}

fn commit_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    a.starts_with(&b) || b.starts_with(&a)
}

fn range_ordering(range_ty: &str, eco: Ecosystem) -> Ecosystem {
    // SEMVER ranges always use SemVer precedence, whatever the package ecosystem.
    if range_ty == "SEMVER" {
        Ecosystem::Cargo
    } else {
        eco
    }
}

fn event_version(e: &Event) -> Option<(&str, u8)> {
    // Order within one version: introduced, then fixed / last_affected.
    if let Some(v) = &e.introduced {
        Some((v.as_str(), 0))
    } else if let Some(v) = &e.fixed {
        Some((v.as_str(), 1))
    } else {
        e.last_affected.as_deref().map(|v| (v, 2))
    }
}

fn version_in_range(eco: Ecosystem, r: &VersionRange, installed: &str) -> bool {
    let ord = range_ordering(&r.ty, eco);
    let mut events: Vec<&Event> = r
        .events
        .iter()
        .filter(|e| event_version(e).is_some())
        .collect();
    events.sort_by(|a, b| {
        let (va, ka) = event_version(a).unwrap_or(("", 0));
        let (vb, kb) = event_version(b).unwrap_or(("", 0));
        let by_version = if va == "0" && vb == "0" {
            Ordering::Equal
        } else if va == "0" {
            Ordering::Less
        } else if vb == "0" {
            Ordering::Greater
        } else {
            version::compare(ord, va, vb)
        };
        by_version.then(ka.cmp(&kb))
    });
    let mut affected = false;
    for e in events {
        let Some((v, kind)) = event_version(e) else {
            continue;
        };
        let cmp_installed = if v == "0" {
            Ordering::Greater
        } else {
            version::compare(ord, installed, v)
        };
        match kind {
            0 => {
                if cmp_installed != Ordering::Less {
                    affected = true;
                }
            }
            1 => {
                if cmp_installed != Ordering::Less {
                    affected = false;
                }
            }
            _ => {
                if cmp_installed == Ordering::Greater {
                    affected = false;
                }
            }
        }
    }
    affected
}

fn git_in_range(r: &VersionRange, commit: &str, git: Option<&dyn GitAncestry>) -> bool {
    let repo = r.repo.as_deref().unwrap_or("");
    let mut introduced: Vec<&str> = Vec::new();
    let mut fixed: Vec<&str> = Vec::new();
    let mut last: Vec<&str> = Vec::new();
    for e in &r.events {
        if let Some(v) = &e.introduced {
            introduced.push(v);
        }
        if let Some(v) = &e.fixed {
            fixed.push(v);
        }
        if let Some(v) = &e.last_affected {
            last.push(v);
        }
    }
    if fixed.iter().any(|f| commit_eq(f, commit)) {
        return false;
    }
    if let Some(g) = git {
        let anc = |a: &str, d: &str| g.is_ancestor(repo, a, d);
        let started = introduced
            .iter()
            .any(|i| *i == "0" || anc(i, commit) == Some(true));
        let ended = fixed.iter().any(|f| anc(f, commit) == Some(true))
            || last
                .iter()
                .any(|l| !commit_eq(l, commit) && anc(l, commit) == Some(true));
        return started && !ended;
    }
    introduced
        .iter()
        .chain(last.iter())
        .any(|c| *c != "0" && commit_eq(c, commit))
}

/// Whether `installed` is affected by one `affected` entry.
pub fn is_affected(
    eco: Ecosystem,
    installed: &str,
    affected: &Affected,
    git: Option<&dyn GitAncestry>,
) -> bool {
    if installed.is_empty() {
        return false;
    }
    if affected
        .versions
        .iter()
        .any(|v| version::compare(eco, v, installed) == Ordering::Equal)
    {
        return true;
    }
    let commit = commit_of(eco, installed);
    affected.ranges.iter().any(|r| match r.ty.as_str() {
        "GIT" => commit.as_deref().is_some_and(|c| git_in_range(r, c, git)),
        "SEMVER" | "ECOSYSTEM" => version_in_range(eco, r, installed),
        _ => false,
    })
}

fn package_matches(eco: Ecosystem, name: &str, a: &Affected) -> bool {
    let Some(p) = &a.package else { return false };
    let norm = eco.normalize_name(name);
    if Ecosystem::from_osv_name(&p.ecosystem) == Some(eco) && eco.normalize_name(&p.name) == norm {
        return true;
    }
    // A purl-only entry: compare the purl identity.
    p.purl.as_deref().is_some_and(|u| {
        crate::purl::Purl::parse(u)
            .is_ok_and(|pp| pp.without_version() == eco.purl(name, "").without_version())
    })
}

/// The fixed versions of `affected` newer than `installed`, ascending.
fn fixed_versions(eco: Ecosystem, installed: &str, affected: &Affected) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in affected.ranges.iter().filter(|r| r.ty != "GIT") {
        let ord = range_ordering(&r.ty, eco);
        for e in &r.events {
            if let Some(f) = &e.fixed
                && version::compare(ord, f, installed) == Ordering::Greater
                && !out.contains(f)
            {
                out.push(f.clone());
            }
        }
    }
    out.sort_by(|a, b| version::compare(eco, a, b));
    out
}

struct Hit {
    advisory: std::sync::Arc<Advisory>,
    fixed: Vec<String>,
}

fn hits_for(source: &dyn AdvisorySource, p: &Package, opts: &MatchOptions<'_>) -> Vec<Hit> {
    let mut hits = Vec::new();
    for adv in source.candidates(p.ecosystem, &p.name) {
        if adv.is_withdrawn() {
            continue;
        }
        let mut fixed: Vec<String> = Vec::new();
        let mut matched = false;
        for a in adv
            .affected
            .iter()
            .filter(|a| package_matches(p.ecosystem, &p.name, a))
        {
            if is_affected(p.ecosystem, &p.version, a, opts.git) {
                matched = true;
                for f in fixed_versions(p.ecosystem, &p.version, a) {
                    if !fixed.contains(&f) {
                        fixed.push(f);
                    }
                }
            }
        }
        if matched {
            fixed.sort_by(|a, b| version::compare(p.ecosystem, a, b));
            hits.push(Hit {
                advisory: adv,
                fixed,
            });
        }
    }
    hits
}

/// Picks the canonical id of an alias group: GHSA first, then CVE, then the smallest id.
fn canonical_id<'a>(ids: &BTreeSet<&'a str>) -> &'a str {
    ids.iter()
        .copied()
        .min_by_key(|i| {
            (
                !i.starts_with("GHSA-"),
                !i.starts_with("CVE-"),
                i.to_owned(),
            )
        })
        .unwrap_or("")
}

/// Matches every package of `graph` against `source`.
///
/// Withdrawn advisories never match. Advisories that alias each other and hit the same package
/// become one finding. Output is sorted by manifest path, package purl and advisory id.
pub fn match_graph(
    graph: &DependencyGraph,
    source: &dyn AdvisorySource,
    opts: &MatchOptions<'_>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for p in graph.packages() {
        let hits = hits_for(source, p, opts);
        if hits.is_empty() {
            continue;
        }
        // Union hits whose ids or aliases overlap.
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (i, h) in hits.iter().enumerate() {
            let ids: BTreeSet<&str> = std::iter::once(h.advisory.id.as_str())
                .chain(h.advisory.aliases.iter().map(String::as_str))
                .collect();
            let overlapping: Vec<usize> = groups
                .iter()
                .enumerate()
                .filter(|(_, g)| {
                    g.iter().any(|j| {
                        let other = &hits[*j].advisory;
                        std::iter::once(other.id.as_str())
                            .chain(other.aliases.iter().map(String::as_str))
                            .any(|x| ids.contains(x))
                    })
                })
                .map(|(gi, _)| gi)
                .collect();
            match overlapping.split_first() {
                None => groups.push(vec![i]),
                Some((first, rest)) => {
                    let mut merged: Vec<usize> = Vec::new();
                    for gi in rest.iter().rev() {
                        merged.extend(groups.remove(*gi));
                    }
                    groups[*first].extend(merged);
                    groups[*first].push(i);
                }
            }
        }
        let path = graph.dependency_path(p);
        for g in groups {
            let mut ids: BTreeSet<&str> = BTreeSet::new();
            let mut fixed: Vec<String> = Vec::new();
            let mut best: Option<(&Hit, crate::osv::AdvisorySeverity)> = None;
            for j in &g {
                let h = &hits[*j];
                ids.insert(&h.advisory.id);
                ids.extend(h.advisory.aliases.iter().map(String::as_str));
                for f in &h.fixed {
                    if !fixed.contains(f) {
                        fixed.push(f.clone());
                    }
                }
                let sev = h.advisory.severity();
                let better = match &best {
                    None => true,
                    Some((_, b)) => {
                        (sev.severity, sev.score.map(|s| (s * 10.0) as i64))
                            > (b.severity, b.score.map(|s| (s * 10.0) as i64))
                    }
                };
                if better {
                    best = Some((h, sev));
                }
            }
            fixed.sort_by(|a, b| version::compare(p.ecosystem, a, b));
            let Some((hit, sev)) = best else { continue };
            let id = canonical_id(&ids);
            let summary = g
                .iter()
                .filter_map(|j| hits[*j].advisory.summary.clone())
                .find(|s| !s.is_empty())
                .unwrap_or_else(|| hit.advisory.id.clone());
            out.push(Finding {
                advisory_id: id.to_owned(),
                aliases: ids
                    .iter()
                    .filter(|i| **i != id)
                    .map(|s| (*s).to_owned())
                    .collect(),
                summary,
                severity: sev.severity,
                cvss_score: sev.score,
                cvss_vector: sev.vector,
                ecosystem: p.ecosystem,
                package: p.name.clone(),
                purl: p.purl.clone(),
                installed_version: p.version.clone(),
                fixed_versions: fixed,
                scope: p.scope,
                relationship: p.relationship,
                manifest_path: p.source_path.clone(),
                dependency_path: path.clone(),
            });
        }
    }
    out.sort_by(|a, b| {
        (&a.manifest_path, &a.purl, &a.advisory_id).cmp(&(
            &b.manifest_path,
            &b.purl,
            &b.advisory_id,
        ))
    });
    out
}

/// The highest severity among findings, `None` when there are none.
pub fn highest_severity(findings: &[Finding]) -> Severity {
    findings
        .iter()
        .map(|f| f.severity)
        .max()
        .unwrap_or(Severity::None)
}

/// Groups findings by package purl, for callers that raise one alert per package and advisory.
pub fn by_package(findings: &[Finding]) -> BTreeMap<&str, Vec<&Finding>> {
    let mut m: BTreeMap<&str, Vec<&Finding>> = BTreeMap::new();
    for f in findings {
        m.entry(f.purl.as_str()).or_default().push(f);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aff(json: &str) -> Affected {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn semver_range_boundaries() {
        let a = aff(
            r#"{"package":{"ecosystem":"npm","name":"x"},"ranges":[{"type":"SEMVER","events":[{"introduced":"1.0.0"},{"fixed":"1.4.2"},{"introduced":"2.0.0"},{"fixed":"2.1.0"}]}]}"#,
        );
        let t = |v: &str| is_affected(Ecosystem::Npm, v, &a, None);
        assert!(!t("0.9.9"));
        assert!(t("1.0.0"));
        assert!(t("1.4.1"));
        assert!(!t("1.4.2"));
        assert!(!t("1.9.0"));
        assert!(t("2.0.5"));
        assert!(!t("2.1.0"));
        assert!(!t(""));
    }

    #[test]
    fn introduced_zero_and_last_affected() {
        let a = aff(
            r#"{"ranges":[{"type":"ECOSYSTEM","events":[{"introduced":"0"},{"last_affected":"1.2.3"}]}]}"#,
        );
        assert!(is_affected(Ecosystem::PyPI, "0.0.1", &a, None));
        assert!(is_affected(Ecosystem::PyPI, "1.2.3", &a, None));
        assert!(!is_affected(Ecosystem::PyPI, "1.2.4", &a, None));
    }

    #[test]
    fn events_may_come_unsorted() {
        let a = aff(
            r#"{"ranges":[{"type":"SEMVER","events":[{"fixed":"3.0.0"},{"introduced":"2.0.0"}]}]}"#,
        );
        assert!(is_affected(Ecosystem::Cargo, "2.5.0", &a, None));
        assert!(!is_affected(Ecosystem::Cargo, "3.0.0", &a, None));
        assert!(!is_affected(Ecosystem::Cargo, "1.0.0", &a, None));
    }

    #[test]
    fn ecosystem_ranges_use_native_ordering() {
        let pep = aff(
            r#"{"ranges":[{"type":"ECOSYSTEM","events":[{"introduced":"0"},{"fixed":"3.2"}]}]}"#,
        );
        assert!(
            is_affected(Ecosystem::PyPI, "3.2rc1", &pep, None),
            "pre-release sorts below the fix"
        );
        assert!(!is_affected(Ecosystem::PyPI, "3.2.post1", &pep, None));
        let mvn = aff(
            r#"{"ranges":[{"type":"ECOSYSTEM","events":[{"introduced":"2.0-beta9"},{"fixed":"2.15.0"}]}]}"#,
        );
        assert!(is_affected(Ecosystem::Maven, "2.14.1", &mvn, None));
        assert!(!is_affected(Ecosystem::Maven, "2.15.0", &mvn, None));
        assert!(!is_affected(Ecosystem::Maven, "2.0-alpha1", &mvn, None));
        let gem = aff(
            r#"{"ranges":[{"type":"ECOSYSTEM","events":[{"introduced":"0"},{"fixed":"6.1.4.1"}]}]}"#,
        );
        assert!(is_affected(Ecosystem::RubyGems, "6.1.4", &gem, None));
        assert!(!is_affected(Ecosystem::RubyGems, "6.1.10", &gem, None));
        let go = aff(
            r#"{"ranges":[{"type":"SEMVER","events":[{"introduced":"0"},{"fixed":"0.3.8"}]}]}"#,
        );
        assert!(is_affected(Ecosystem::Go, "v0.3.6", &go, None));
        assert!(is_affected(
            Ecosystem::Go,
            "v0.0.0-20191109021931-daa7c04131f5",
            &go,
            None
        ));
        assert!(!is_affected(Ecosystem::Go, "v0.3.8", &go, None));
    }

    #[test]
    fn explicit_versions_list() {
        let a = aff(r#"{"versions":["1.0","2.0.0"]}"#);
        assert!(
            is_affected(Ecosystem::Maven, "1.0.0", &a, None),
            "1.0 equals 1.0.0 in Maven"
        );
        assert!(!is_affected(Ecosystem::Maven, "1.1", &a, None));
    }

    struct Fake;
    impl GitAncestry for Fake {
        fn is_ancestor(&self, _repo: &str, ancestor: &str, descendant: &str) -> Option<bool> {
            // Linear history a1 < b2 < c3 < d4.
            let order = ["a1", "b2", "c3", "d4"];
            let a = order.iter().position(|x| *x == ancestor)?;
            let d = order.iter().position(|x| *x == descendant)?;
            Some(a <= d)
        }
    }

    #[test]
    fn git_ranges_with_and_without_ancestry() {
        let a = aff(
            r#"{"ranges":[{"type":"GIT","repo":"https://x/y","events":[{"introduced":"b2"},{"fixed":"d4"}]}]}"#,
        );
        let g = Fake;
        let with = |c: &str| git_in_range(&a.ranges[0], c, Some(&g));
        assert!(!with("a1"));
        assert!(with("b2"));
        assert!(with("c3"));
        assert!(!with("d4"));
        // No oracle: only the exact introduced commit matches.
        assert!(git_in_range(&a.ranges[0], "b2", None));
        assert!(!git_in_range(&a.ranges[0], "c3", None));
        assert!(!git_in_range(&a.ranges[0], "d4", None));
        // End to end through is_affected: a Go pseudo-version names its commit.
        let full = aff(
            r#"{"ranges":[{"type":"GIT","events":[{"introduced":"daa7c04131f5aaaa"},{"fixed":"ffffffffffffffff"}]}]}"#,
        );
        assert!(is_affected(
            Ecosystem::Go,
            "v0.0.0-20191109021931-daa7c04131f5",
            &full,
            None
        ));
        assert!(!is_affected(Ecosystem::Go, "v1.0.0", &full, None));
    }

    #[test]
    fn commit_extraction() {
        assert_eq!(
            commit_of(Ecosystem::Go, "v0.0.0-20191109021931-daa7c04131f5").as_deref(),
            Some("daa7c04131f5")
        );
        assert_eq!(
            commit_of(
                Ecosystem::GitHubActions,
                "8F152DE45CC393BB48CE5D89D36B731F54556E65"
            )
            .unwrap()
            .len(),
            40
        );
        assert_eq!(commit_of(Ecosystem::GitHubActions, "v4"), None);
    }
}
