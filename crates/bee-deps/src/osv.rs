//! OSV schema advisories and the [`AdvisorySource`] abstraction.
//!
//! [`Advisory`] models the fields of the OSV schema (<https://ossf.github.io/osv-schema/>) that
//! matching needs, and keeps the rest of the record in [`Advisory::database_specific`].
//! [`FileAdvisorySource`] is the offline implementation: it loads OSV JSON dumps (a single
//! record, an array, newline-delimited JSON, or a directory of `*.json` files) and indexes them
//! by ecosystem and package name. No network access is ever made.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cvss::{self, Severity};
use crate::error::{DepsError, MAX_FILE_BYTES, Result};
use crate::model::Ecosystem;
use crate::purl::Purl;

/// One OSV severity entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeverityEntry {
    /// `CVSS_V2`, `CVSS_V3` or `CVSS_V4`.
    #[serde(rename = "type", default)]
    pub ty: String,
    /// The vector string.
    #[serde(default)]
    pub score: String,
}

/// The package an `affected` entry applies to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffectedPackage {
    /// OSV ecosystem name (`npm`, `crates.io`, `PyPI`, ...).
    #[serde(default)]
    pub ecosystem: String,
    /// The package name in that ecosystem.
    #[serde(default)]
    pub name: String,
    /// An optional package URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purl: Option<String>,
}

/// One event in a range.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// The first affected version (`0` means from the start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introduced: Option<String>,
    /// The first unaffected version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed: Option<String>,
    /// The last affected version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_affected: Option<String>,
    /// An exclusive upper bound for the whole range (GIT ranges).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
}

/// A version range: `SEMVER`, `ECOSYSTEM` or `GIT`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRange {
    /// The range type.
    #[serde(rename = "type", default)]
    pub ty: String,
    /// The repository URL for GIT ranges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The events, in any order.
    #[serde(default)]
    pub events: Vec<Event>,
}

/// One `affected` entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Affected {
    /// The package.
    #[serde(default)]
    pub package: Option<AffectedPackage>,
    /// Version ranges.
    #[serde(default)]
    pub ranges: Vec<VersionRange>,
    /// Explicitly listed affected versions.
    #[serde(default)]
    pub versions: Vec<String>,
}

/// A reference link.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    /// `ADVISORY`, `FIX`, `WEB`, ...
    #[serde(rename = "type", default)]
    pub ty: String,
    /// The URL.
    #[serde(default)]
    pub url: String,
}

/// An OSV record.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Advisory {
    /// The identifier (`GHSA-...`, `CVE-...`, `RUSTSEC-...`, ...).
    pub id: String,
    /// Other identifiers for the same vulnerability.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// One-line summary.
    #[serde(default)]
    pub summary: Option<String>,
    /// Markdown details.
    #[serde(default)]
    pub details: Option<String>,
    /// Last modification time (RFC 3339).
    #[serde(default)]
    pub modified: Option<String>,
    /// Publication time (RFC 3339).
    #[serde(default)]
    pub published: Option<String>,
    /// Withdrawal time; a withdrawn advisory never matches.
    #[serde(default)]
    pub withdrawn: Option<String>,
    /// CVSS severities.
    #[serde(default)]
    pub severity: Vec<SeverityEntry>,
    /// Affected packages.
    #[serde(default)]
    pub affected: Vec<Affected>,
    /// Reference links.
    #[serde(default)]
    pub references: Vec<Reference>,
    /// Database-specific fields (`severity`, `cwe_ids`, ...).
    #[serde(default)]
    pub database_specific: Option<Value>,
}

/// The severity of an advisory: the highest scoring parsable CVSS vector, else the
/// `database_specific.severity` label.
#[derive(Clone, Debug, PartialEq)]
pub struct AdvisorySeverity {
    /// The qualitative rating.
    pub severity: Severity,
    /// The CVSS base score, when a vector parsed.
    pub score: Option<f64>,
    /// The CVSS vector the score came from.
    pub vector: Option<String>,
}

impl Advisory {
    /// Parses one OSV record from JSON text.
    pub fn from_json(text: &str) -> Result<Advisory> {
        let a: Advisory = serde_json::from_str(text)
            .map_err(|e| DepsError::invalid("", format!("invalid OSV record: {e}")))?;
        a.validate()
    }

    fn validate(self) -> Result<Advisory> {
        if self.id.is_empty() {
            return Err(DepsError::invalid("", "OSV record without an id"));
        }
        Ok(self)
    }

    /// True when the record has been withdrawn.
    pub fn is_withdrawn(&self) -> bool {
        self.withdrawn.as_deref().is_some_and(|w| !w.is_empty())
    }

    /// The advisory's severity. See [`AdvisorySeverity`].
    pub fn severity(&self) -> AdvisorySeverity {
        let best = self
            .severity
            .iter()
            .filter(|s| s.ty.starts_with("CVSS_V3") || s.ty.starts_with("CVSS_V4"))
            .filter_map(|s| cvss::parse(&s.score).ok())
            .max_by(|a, b| a.score.total_cmp(&b.score));
        if let Some(c) = best {
            return AdvisorySeverity {
                severity: c.severity,
                score: Some(c.score),
                vector: Some(c.vector),
            };
        }
        let label = self
            .database_specific
            .as_ref()
            .and_then(|d| d.get("severity"))
            .and_then(Value::as_str)
            .and_then(Severity::from_label);
        AdvisorySeverity {
            severity: label.unwrap_or(Severity::None),
            score: None,
            vector: None,
        }
    }

    /// The CVE id among the id and aliases, if any.
    pub fn cve_id(&self) -> Option<&str> {
        std::iter::once(&self.id)
            .chain(self.aliases.iter())
            .map(String::as_str)
            .find(|i| i.starts_with("CVE-"))
    }

    /// The GHSA id among the id and aliases, if any.
    pub fn ghsa_id(&self) -> Option<&str> {
        std::iter::once(&self.id)
            .chain(self.aliases.iter())
            .map(String::as_str)
            .find(|i| i.starts_with("GHSA-"))
    }

    /// CWE ids from `database_specific.cwe_ids`.
    pub fn cwe_ids(&self) -> Vec<String> {
        self.database_specific
            .as_ref()
            .and_then(|d| d.get("cwe_ids"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A place to look advisories up. Implementations must be deterministic and must not block on
/// the network inside matching; fetching and mirroring happen elsewhere.
pub trait AdvisorySource {
    /// Advisories that list a package with this ecosystem and name in an `affected` entry,
    /// sorted by id. Names are compared in the ecosystem's canonical form.
    fn candidates(&self, ecosystem: Ecosystem, name: &str) -> Vec<Arc<Advisory>>;

    /// One advisory by id or alias.
    fn get(&self, id: &str) -> Option<Arc<Advisory>>;
}

/// An in-memory advisory index loaded from OSV JSON files.
#[derive(Clone, Debug, Default)]
pub struct FileAdvisorySource {
    by_package: BTreeMap<(Ecosystem, String), Vec<Arc<Advisory>>>,
    by_id: BTreeMap<String, Arc<Advisory>>,
}

impl FileAdvisorySource {
    /// An empty source.
    pub fn new() -> FileAdvisorySource {
        FileAdvisorySource::default()
    }

    /// Adds one advisory. An advisory with the same id replaces the earlier one.
    pub fn insert(&mut self, advisory: Advisory) {
        let adv = Arc::new(advisory);
        if let Some(old) = self.by_id.get(&adv.id).cloned() {
            self.remove_from_packages(&old);
        }
        let mut keys: Vec<(Ecosystem, String)> = Vec::new();
        for a in &adv.affected {
            if let Some(k) = package_key(a)
                && !keys.contains(&k)
            {
                keys.push(k);
            }
        }
        for k in keys {
            let list = self.by_package.entry(k).or_default();
            list.push(Arc::clone(&adv));
            list.sort_by(|a, b| a.id.cmp(&b.id));
        }
        self.by_id.insert(adv.id.clone(), Arc::clone(&adv));
    }

    fn remove_from_packages(&mut self, old: &Arc<Advisory>) {
        for list in self.by_package.values_mut() {
            list.retain(|a| a.id != old.id);
        }
    }

    /// Loads OSV JSON text: one record, an array of records, or newline-delimited records.
    /// Returns how many records were added.
    pub fn load_json_str(&mut self, path: &str, text: &str) -> Result<usize> {
        if text.len() > MAX_FILE_BYTES {
            return Err(DepsError {
                kind: crate::error::DepsErrorKind::TooLarge,
                path: path.to_owned(),
                message: "advisory file too large".to_owned(),
            });
        }
        let trimmed = text.trim();
        let mut records: Vec<Advisory> = Vec::new();
        let first_brace = trimmed.find('{');
        let parse_one = |v: Value| -> Result<Advisory> {
            serde_json::from_value::<Advisory>(v)
                .map_err(|e| DepsError::invalid(path, format!("invalid OSV record: {e}")))?
                .validate()
                .map_err(|e| DepsError::invalid(path, e.message))
        };
        if trimmed.starts_with('[') {
            let v: Value = serde_json::from_str(trimmed)
                .map_err(|e| DepsError::invalid(path, format!("invalid JSON: {e}")))?;
            for item in v.as_array().cloned().unwrap_or_default() {
                records.push(parse_one(item)?);
            }
        } else if first_brace.is_some() {
            match serde_json::from_str::<Value>(trimmed) {
                Ok(v) => records.push(parse_one(v)?),
                Err(_) => {
                    for (n, line) in trimmed
                        .lines()
                        .enumerate()
                        .filter(|(_, l)| !l.trim().is_empty())
                    {
                        let v: Value = serde_json::from_str(line).map_err(|e| {
                            DepsError::invalid(path, format!("line {}: invalid JSON: {e}", n + 1))
                        })?;
                        records.push(parse_one(v)?);
                    }
                }
            }
        }
        let n = records.len();
        for r in records {
            self.insert(r);
        }
        Ok(n)
    }

    /// Loads every `*.json` and `*.jsonl` file under `dir`, recursively, in sorted path order.
    pub fn load_dir(&mut self, dir: &Path) -> Result<usize> {
        let mut files = Vec::new();
        collect_json(dir, &mut files).map_err(|e| {
            DepsError::invalid(
                &dir.to_string_lossy(),
                format!("cannot read directory: {e}"),
            )
        })?;
        files.sort();
        let mut total = 0;
        for f in files {
            let text = std::fs::read_to_string(&f).map_err(|e| {
                DepsError::invalid(&f.to_string_lossy(), format!("cannot read file: {e}"))
            })?;
            total += self.load_json_str(&f.to_string_lossy(), &text)?;
        }
        Ok(total)
    }

    /// Number of advisories loaded.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// True when no advisory is loaded.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Every advisory, sorted by id.
    pub fn all(&self) -> Vec<Arc<Advisory>> {
        self.by_id.values().cloned().collect()
    }
}

fn collect_json(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_dir() {
            collect_json(&p, out)?;
        } else if matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("json" | "jsonl")
        ) {
            out.push(p);
        }
    }
    Ok(())
}

/// The `(ecosystem, canonical name)` an `affected` entry refers to.
fn package_key(a: &Affected) -> Option<(Ecosystem, String)> {
    let p = a.package.as_ref()?;
    if let Some(eco) = Ecosystem::from_osv_name(&p.ecosystem)
        && !p.name.is_empty()
    {
        return Some((eco, eco.normalize_name(&p.name)));
    }
    let purl = Purl::parse(p.purl.as_deref()?).ok()?;
    let eco = Ecosystem::from_purl_type(&purl.ty)?;
    let name = match eco {
        Ecosystem::Maven => format!("{}:{}", purl.namespace.join("."), purl.name),
        _ if purl.namespace.is_empty() => purl.name.clone(),
        _ => format!("{}/{}", purl.namespace.join("/"), purl.name),
    };
    Some((eco, eco.normalize_name(&name)))
}

impl AdvisorySource for FileAdvisorySource {
    fn candidates(&self, ecosystem: Ecosystem, name: &str) -> Vec<Arc<Advisory>> {
        self.by_package
            .get(&(ecosystem, ecosystem.normalize_name(name)))
            .cloned()
            .unwrap_or_default()
    }

    fn get(&self, id: &str) -> Option<Arc<Advisory>> {
        self.by_id.get(id).cloned().or_else(|| {
            self.by_id
                .values()
                .find(|a| a.aliases.iter().any(|x| x == id))
                .cloned()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LODASH: &str = r#"{
      "id": "GHSA-35jh-r3h4-6jhm",
      "aliases": ["CVE-2021-23337"],
      "summary": "Command Injection in lodash",
      "modified": "2024-01-01T00:00:00Z",
      "severity": [{"type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:H/UI:N/S:U/C:H/I:H/A:H"}],
      "affected": [{"package": {"ecosystem": "npm", "name": "lodash"},
        "ranges": [{"type": "SEMVER", "events": [{"introduced": "0"}, {"fixed": "4.17.21"}]}]}],
      "database_specific": {"cwe_ids": ["CWE-77"], "severity": "HIGH"}
    }"#;

    #[test]
    fn parses_a_record_and_computes_severity() {
        let a = Advisory::from_json(LODASH).unwrap();
        assert_eq!(a.cve_id(), Some("CVE-2021-23337"));
        assert_eq!(a.ghsa_id(), Some("GHSA-35jh-r3h4-6jhm"));
        assert_eq!(a.cwe_ids(), vec!["CWE-77"]);
        let s = a.severity();
        assert_eq!(s.score, Some(7.2));
        assert_eq!(s.severity, Severity::High);
    }

    #[test]
    fn severity_falls_back_to_the_label() {
        let a = Advisory::from_json(r#"{"id":"X-1","database_specific":{"severity":"MODERATE"}}"#)
            .unwrap();
        assert_eq!(a.severity().severity, Severity::Medium);
        assert_eq!(a.severity().score, None);
        let none = Advisory::from_json(r#"{"id":"X-2"}"#).unwrap();
        assert_eq!(none.severity().severity, Severity::None);
    }

    #[test]
    fn rejects_records_without_an_id() {
        assert!(Advisory::from_json(r#"{"summary":"x"}"#).is_err());
        assert!(Advisory::from_json("{").is_err());
    }

    #[test]
    fn source_loads_single_array_and_ndjson_forms() {
        let mut s = FileAdvisorySource::new();
        assert_eq!(s.load_json_str("a.json", LODASH).unwrap(), 1);
        let arr = format!(
            "[{LODASH}, {}]",
            r#"{"id":"PYSEC-1","affected":[{"package":{"ecosystem":"PyPI","name":"Django_Thing"}}]}"#
        );
        assert_eq!(s.load_json_str("b.json", &arr).unwrap(), 2);
        assert_eq!(s.len(), 2, "same id replaces");
        let nd = "{\"id\":\"A-1\",\"affected\":[{\"package\":{\"ecosystem\":\"Go\",\"name\":\"example.com/x\"}}]}\n{\"id\":\"A-2\"}\n";
        assert_eq!(s.load_json_str("c.jsonl", nd).unwrap(), 2);
        assert_eq!(
            s.candidates(Ecosystem::PyPI, "django.thing").len(),
            1,
            "PEP 503 name folding"
        );
        assert_eq!(s.candidates(Ecosystem::Npm, "lodash").len(), 1);
        assert!(s.candidates(Ecosystem::Npm, "left-pad").is_empty());
        assert!(s.get("CVE-2021-23337").is_some(), "lookup by alias");
        assert!(s.get("nope").is_none());
    }

    #[test]
    fn purl_only_affected_entries_are_indexed() {
        let mut s = FileAdvisorySource::new();
        s.load_json_str(
            "p.json",
            r#"{"id":"M-1","affected":[{"package":{"purl":"pkg:maven/org.apache.logging.log4j/log4j-core"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            s.candidates(Ecosystem::Maven, "org.apache.logging.log4j:log4j-core")
                .len(),
            1
        );
    }
}
