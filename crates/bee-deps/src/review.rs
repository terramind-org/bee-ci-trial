//! Dependency review: the difference between two dependency graphs, with the vulnerabilities and
//! license problems the change introduces.
//!
//! The result maps onto `bee.v1.DependencyReview` and can be rendered as a required check
//! result with [`DependencyReview::to_check_result`].

use std::collections::{BTreeMap, BTreeSet};

use crate::cvss::Severity;
use crate::license::{LicenseExpr, combine_declared};
use crate::matching::{Finding, MatchOptions, match_graph};
use crate::model::{DependencyGraph, Ecosystem, Package, Scope};
use crate::osv::AdvisorySource;
use crate::version;

/// How a dependency differs between base and head (`bee.v1.DependencyChangeType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeType {
    /// Present only at head.
    Added,
    /// Present only at base.
    Removed,
    /// Present at both with a different version.
    Changed,
}

/// One changed dependency (`bee.v1.DependencyChange`).
#[derive(Clone, Debug, PartialEq)]
pub struct DependencyChange {
    /// Added, removed or changed.
    pub change_type: ChangeType,
    /// The manifest or lockfile.
    pub manifest_path: String,
    /// The ecosystem.
    pub ecosystem: Ecosystem,
    /// The package name.
    pub name: String,
    /// The version at base, absent for additions.
    pub base_version: Option<String>,
    /// The version at head, absent for removals.
    pub head_version: Option<String>,
    /// The head purl, or the base purl for removals.
    pub purl: String,
    /// Licenses at head (base for removals).
    pub licenses: Vec<String>,
    /// Licenses at base, for changed dependencies.
    pub base_licenses: Vec<String>,
    /// True when the set of licenses differs between base and head.
    pub license_changed: bool,
    /// True when the head licenses cannot be satisfied under the policy.
    pub license_denied: bool,
    /// Runtime or development.
    pub scope: Scope,
    /// Vulnerabilities that affect the head version and did not affect the package at base.
    pub vulnerabilities: Vec<Finding>,
    /// Advisories that affected the base version and no longer affect the head.
    pub fixed_advisories: Vec<String>,
}

/// Policy for [`review`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewPolicy {
    /// Fail on new vulnerabilities at or above this severity. `None` never fails on them.
    pub fail_on_severity: Option<Severity>,
    /// Also fail on vulnerabilities in development-scope dependencies (default: runtime only).
    pub include_development: bool,
    /// Licenses (SPDX ids) that are not allowed. Matching is case-insensitive.
    pub deny_licenses: Vec<String>,
    /// When non-empty, only these licenses are allowed.
    pub allow_licenses: Vec<String>,
    /// Treat a dependency whose license is unknown as denied when `allow_licenses` is set.
    pub deny_unknown_licenses: bool,
}

impl Default for ReviewPolicy {
    fn default() -> ReviewPolicy {
        ReviewPolicy {
            fail_on_severity: Some(Severity::Low),
            include_development: false,
            deny_licenses: Vec::new(),
            allow_licenses: Vec::new(),
            deny_unknown_licenses: false,
        }
    }
}

/// The review result (`bee.v1.DependencyReview`).
#[derive(Clone, Debug, PartialEq)]
pub struct DependencyReview {
    /// Every changed dependency, sorted by manifest, ecosystem, name and change type.
    pub changes: Vec<DependencyChange>,
    /// Number of added or changed dependencies that carry a new vulnerability.
    pub vulnerable_additions: u32,
    /// The highest severity among new vulnerabilities.
    pub highest_severity: Severity,
    /// Whether the change meets the policy.
    pub passes: bool,
    /// Why the review fails; empty when it passes.
    pub reasons: Vec<String>,
}

/// A check run conclusion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conclusion {
    /// The review passed.
    Success,
    /// The review failed and should block merging when required.
    Failure,
}

/// A required-check result: conclusion, short title and Markdown summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    /// Pass or fail.
    pub conclusion: Conclusion,
    /// One-line title.
    pub title: String,
    /// Markdown body.
    pub summary: String,
}

type Key = (String, Ecosystem, String);

fn group(g: &DependencyGraph) -> BTreeMap<Key, BTreeMap<String, &Package>> {
    let mut m: BTreeMap<Key, BTreeMap<String, &Package>> = BTreeMap::new();
    for p in g.packages() {
        m.entry((p.source_path.clone(), p.ecosystem, p.normalized_name()))
            .or_default()
            .insert(p.version.clone(), p);
    }
    m
}

fn license_denied(p: &Package, policy: &ReviewPolicy) -> bool {
    let Some(expr) = combine_declared(&p.licenses) else {
        return policy.deny_unknown_licenses
            && !policy.allow_licenses.is_empty()
            && p.licenses.is_empty();
    };
    let denied = |l: &str| {
        policy
            .deny_licenses
            .iter()
            .any(|d| d.eq_ignore_ascii_case(l))
    };
    let allowed = |l: &str| {
        policy.allow_licenses.is_empty()
            || policy.allow_licenses.iter().any(|a| {
                let base = l.split(" WITH ").next().unwrap_or(l);
                a.eq_ignore_ascii_case(l)
                    || a.eq_ignore_ascii_case(base)
                    || a.eq_ignore_ascii_case(base.trim_end_matches('+'))
            })
    };
    !expr.satisfiable(&|l| !denied(l) && allowed(l))
}

fn license_set(p: &Package) -> BTreeSet<String> {
    p.licenses
        .iter()
        .map(|l| {
            LicenseExpr::parse(l).map_or_else(|_| l.clone(), |e| e.to_string().to_ascii_uppercase())
        })
        .collect()
}

fn finding_ids(f: &Finding) -> BTreeSet<&str> {
    std::iter::once(f.advisory_id.as_str())
        .chain(f.aliases.iter().map(String::as_str))
        .collect()
}

/// Reviews the change from `base` to `head` against `source` and `policy`.
pub fn review(
    base: &DependencyGraph,
    head: &DependencyGraph,
    source: &dyn AdvisorySource,
    policy: &ReviewPolicy,
    opts: &MatchOptions<'_>,
) -> DependencyReview {
    let base_findings = match_graph(base, source, opts);
    let head_findings = match_graph(head, source, opts);
    let bg = group(base);
    let hg = group(head);
    let keys: BTreeSet<&Key> = bg.keys().chain(hg.keys()).collect();
    let mut changes: Vec<DependencyChange> = Vec::new();
    for key in keys {
        let empty = BTreeMap::new();
        let b = bg.get(key).unwrap_or(&empty);
        let h = hg.get(key).unwrap_or(&empty);
        let eco = key.1;
        let mut removed: Vec<&Package> = b
            .iter()
            .filter(|(v, _)| !h.contains_key(*v))
            .map(|(_, p)| *p)
            .collect();
        let mut added: Vec<&Package> = h
            .iter()
            .filter(|(v, _)| !b.contains_key(*v))
            .map(|(_, p)| *p)
            .collect();
        removed.sort_by(|x, y| version::compare(eco, &x.version, &y.version));
        added.sort_by(|x, y| version::compare(eco, &x.version, &y.version));
        let pairs = removed.len().min(added.len());
        // Advisory ids that affected this package name at base, any version.
        let base_ids: BTreeSet<&str> = base_findings
            .iter()
            .filter(|f| {
                f.manifest_path == key.0
                    && f.ecosystem == eco
                    && eco.normalize_name(&f.package) == key.2
            })
            .flat_map(finding_ids)
            .collect();
        let mk = |change_type: ChangeType,
                  old: Option<&Package>,
                  new: Option<&Package>|
         -> DependencyChange {
            let shown = new.or(old).expect("a change has a side");
            let vulns: Vec<Finding> = match new {
                Some(n) => head_findings
                    .iter()
                    .filter(|f| f.manifest_path == n.source_path && f.purl == n.purl)
                    .filter(|f| finding_ids(f).is_disjoint(&base_ids))
                    .cloned()
                    .collect(),
                None => Vec::new(),
            };
            let fixed: Vec<String> = match old {
                Some(o) => {
                    let still: BTreeSet<&str> = head_findings
                        .iter()
                        .filter(|f| {
                            f.manifest_path == o.source_path
                                && f.ecosystem == eco
                                && eco.normalize_name(&f.package) == key.2
                        })
                        .flat_map(finding_ids)
                        .collect();
                    base_findings
                        .iter()
                        .filter(|f| f.manifest_path == o.source_path && f.purl == o.purl)
                        .filter(|f| finding_ids(f).is_disjoint(&still))
                        .map(|f| f.advisory_id.clone())
                        .collect()
                }
                None => Vec::new(),
            };
            let (base_licenses, license_changed) = match (old, new) {
                (Some(o), Some(n)) => (o.licenses.clone(), license_set(o) != license_set(n)),
                _ => (Vec::new(), false),
            };
            DependencyChange {
                change_type,
                manifest_path: key.0.clone(),
                ecosystem: eco,
                name: shown.name.clone(),
                base_version: old.map(|p| p.version.clone()),
                head_version: new.map(|p| p.version.clone()),
                purl: shown.purl.clone(),
                licenses: shown.licenses.clone(),
                base_licenses,
                license_changed,
                license_denied: new.is_some_and(|n| license_denied(n, policy)),
                scope: shown.scope,
                vulnerabilities: vulns,
                fixed_advisories: fixed,
            }
        };
        for i in 0..pairs {
            changes.push(mk(ChangeType::Changed, Some(removed[i]), Some(added[i])));
        }
        for p in &removed[pairs..] {
            changes.push(mk(ChangeType::Removed, Some(p), None));
        }
        for p in &added[pairs..] {
            changes.push(mk(ChangeType::Added, None, Some(p)));
        }
    }
    changes.sort_by(|a, b| {
        (
            &a.manifest_path,
            a.ecosystem,
            a.name.as_str(),
            a.change_type,
            &a.head_version,
            &a.base_version,
        )
            .cmp(&(
                &b.manifest_path,
                b.ecosystem,
                b.name.as_str(),
                b.change_type,
                &b.head_version,
                &b.base_version,
            ))
    });

    let vulnerable_additions = u32::try_from(
        changes
            .iter()
            .filter(|c| !c.vulnerabilities.is_empty())
            .count(),
    )
    .unwrap_or(u32::MAX);
    let highest_severity = changes
        .iter()
        .flat_map(|c| c.vulnerabilities.iter().map(|f| f.severity))
        .max()
        .unwrap_or(Severity::None);
    let mut reasons = Vec::new();
    if let Some(threshold) = policy.fail_on_severity {
        for c in &changes {
            if c.scope == Scope::Development && !policy.include_development {
                continue;
            }
            for f in c.vulnerabilities.iter().filter(|f| f.severity >= threshold) {
                reasons.push(format!(
                    "{} {} introduces {} ({})",
                    c.name,
                    c.head_version.as_deref().unwrap_or(""),
                    f.advisory_id,
                    f.severity.label()
                ));
            }
        }
    }
    for c in changes.iter().filter(|c| c.license_denied) {
        reasons.push(format!(
            "{} {} has a disallowed license ({})",
            c.name,
            c.head_version.as_deref().unwrap_or(""),
            if c.licenses.is_empty() {
                "unknown".to_owned()
            } else {
                c.licenses.join(", ")
            }
        ));
    }
    DependencyReview {
        changes,
        vulnerable_additions,
        highest_severity,
        passes: reasons.is_empty(),
        reasons,
    }
}

impl DependencyReview {
    /// Renders the review as a required-check result.
    pub fn to_check_result(&self) -> CheckResult {
        let added = self
            .changes
            .iter()
            .filter(|c| c.change_type == ChangeType::Added)
            .count();
        let removed = self
            .changes
            .iter()
            .filter(|c| c.change_type == ChangeType::Removed)
            .count();
        let changed = self
            .changes
            .iter()
            .filter(|c| c.change_type == ChangeType::Changed)
            .count();
        let title = if self.passes {
            format!("Dependency review passed: {added} added, {changed} changed, {removed} removed")
        } else {
            format!(
                "Dependency review failed: {} problem(s)",
                self.reasons.len()
            )
        };
        let mut s = String::new();
        s.push_str(&format!(
            "{added} added, {changed} changed, {removed} removed.\n\n"
        ));
        if !self.reasons.is_empty() {
            s.push_str("### Problems\n\n");
            for r in &self.reasons {
                s.push_str(&format!("- {r}\n"));
            }
            s.push('\n');
        }
        if !self.changes.is_empty() {
            s.push_str("| Change | Package | Base | Head | Manifest | Notes |\n|---|---|---|---|---|---|\n");
            for c in &self.changes {
                let mut notes: Vec<String> = c
                    .vulnerabilities
                    .iter()
                    .map(|f| format!("{} {}", f.advisory_id, f.severity.label()))
                    .collect();
                if c.license_denied {
                    notes.push("license denied".to_owned());
                } else if c.license_changed {
                    notes.push("license changed".to_owned());
                }
                for id in &c.fixed_advisories {
                    notes.push(format!("fixes {id}"));
                }
                s.push_str(&format!(
                    "| {:?} | {} | {} | {} | {} | {} |\n",
                    c.change_type,
                    c.name,
                    c.base_version.as_deref().unwrap_or("-"),
                    c.head_version.as_deref().unwrap_or("-"),
                    c.manifest_path,
                    notes.join("; ")
                ));
            }
        }
        CheckResult {
            conclusion: if self.passes {
                Conclusion::Success
            } else {
                Conclusion::Failure
            },
            title,
            summary: s,
        }
    }
}
