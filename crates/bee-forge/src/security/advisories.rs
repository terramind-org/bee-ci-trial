//! The advisory index: OSV records from the [`super::ports::AdvisoryFeed`] plus advisories
//! published from repos, with the global database RPCs and the incremental re-evaluation that
//! runs when the index changes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bee_deps::Ecosystem;
use bee_deps::osv::{Advisory as Osv, AdvisorySource, FileAdvisorySource};
use bee_proto::v1::{
    Advisory, AdvisoryAffectedPackage, AdvisoryType, GetAdvisoryRequest, ListAdvisoriesRequest,
};
use sha2::{Digest, Sha256};

use super::model::{GlobalAdvRec, GraphRec};
use super::{GLOBAL_ADVISORIES, GRAPHS, Security, page_slice, sev};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts, ts_ms};
use crate::store::{Scan, Surface};

/// Where an indexed advisory came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Feed,
    Published,
}

/// One indexed advisory.
#[derive(Clone)]
pub(crate) struct Entry {
    digest: String,
    keys: Vec<(Ecosystem, String)>,
    pub(crate) origin: Origin,
    pub(crate) osv: Arc<Osv>,
}

/// The matching index.
#[derive(Default)]
pub(crate) struct AdvisoryIndex {
    pub(crate) source: FileAdvisorySource,
    pub(crate) entries: BTreeMap<String, Entry>,
    pub(crate) feed_version: Option<String>,
    pub(crate) generation: u64,
    pub(crate) published_loaded: bool,
}

fn keys_of(a: &Osv) -> Vec<(Ecosystem, String)> {
    let mut keys: Vec<(Ecosystem, String)> = a
        .affected
        .iter()
        .filter_map(|x| x.package.as_ref())
        .filter_map(|p| {
            Ecosystem::from_osv_name(&p.ecosystem).map(|e| (e, e.normalize_name(&p.name)))
        })
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

fn digest_of(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

/// What one refresh did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefreshReport {
    /// Advisories new to the index.
    pub added: usize,
    /// Advisories whose record changed.
    pub changed: usize,
    /// Advisories no longer in the feed.
    pub removed: usize,
    /// Records that were not valid OSV.
    pub invalid: usize,
    /// Repos whose alerts were re-evaluated.
    pub repos_evaluated: usize,
}

/// Parses an RFC 3339 time (`2026-01-02T03:04:05Z`, fractions and offsets allowed) into
/// milliseconds since the epoch.
pub(crate) fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let (time, offset_secs) = if let Some(t) = rest.strip_suffix(['Z', 'z']) {
        (t, 0i64)
    } else if let Some(i) = rest.rfind(['+', '-']).filter(|i| *i > 0) {
        let (t, o) = rest.split_at(i);
        let sign = if o.starts_with('-') { -1 } else { 1 };
        let mut p = o[1..].split(':');
        let oh: i64 = p.next()?.parse().ok()?;
        let om: i64 = p.next().unwrap_or("0").parse().ok()?;
        (t, sign * (oh * 3600 + om * 60))
    } else {
        (rest, 0)
    };
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss_full = t.next().unwrap_or("0");
    let (ss, frac) = ss_full.split_once('.').unwrap_or((ss_full, "0"));
    let ss: i64 = ss.parse().ok()?;
    let frac_ms: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss - offset_secs;
    u64::try_from(secs * 1000 + frac_ms).ok()
}

/// The text of an OSV record's ranges for one affected entry: `>= 1.0, < 1.4.2`.
fn range_text(a: &bee_deps::osv::Affected) -> (String, String) {
    let mut parts = Vec::new();
    let mut first_fixed = String::new();
    for r in &a.ranges {
        let mut bounds = Vec::new();
        for e in &r.events {
            if let Some(i) = &e.introduced
                && i != "0"
            {
                bounds.push(format!(">= {i}"));
            }
            if let Some(f) = &e.fixed {
                bounds.push(format!("< {f}"));
                if first_fixed.is_empty() {
                    first_fixed = f.clone();
                }
            }
            if let Some(l) = &e.last_affected {
                bounds.push(format!("<= {l}"));
            }
        }
        if !bounds.is_empty() {
            parts.push(bounds.join(", "));
        }
    }
    for v in &a.versions {
        parts.push(format!("= {v}"));
    }
    (parts.join(" || "), first_fixed)
}

/// The wire form of an OSV record.
pub(crate) fn to_proto(a: &Osv) -> Advisory {
    let s = a.severity();
    let primary = a
        .ghsa_id()
        .map(str::to_string)
        .unwrap_or_else(|| a.id.clone());
    Advisory {
        ghsa_id: primary,
        cve_id: a.cve_id().unwrap_or_default().to_string(),
        r#type: if a.id.starts_with("MAL-") {
            AdvisoryType::Malware
        } else {
            AdvisoryType::Reviewed
        } as i32,
        summary: a.summary.clone().unwrap_or_default(),
        description: a.details.clone().unwrap_or_default(),
        severity: sev(s.severity) as i32,
        cvss_vector: s.vector.unwrap_or_default(),
        cvss_score: s.score.unwrap_or(0.0),
        cwe_ids: a.cwe_ids(),
        affected: a
            .affected
            .iter()
            .filter_map(|x| {
                let p = x.package.as_ref()?;
                let eco = Ecosystem::from_osv_name(&p.ecosystem);
                let (range, fixed) = range_text(x);
                Some(AdvisoryAffectedPackage {
                    ecosystem: eco
                        .map(|e| e.proto_name().to_string())
                        .unwrap_or_else(|| p.ecosystem.clone()),
                    name: p.name.clone(),
                    vulnerable_range: range,
                    first_patched_version: fixed,
                    vulnerable_functions: Vec::new(),
                })
            })
            .collect(),
        references: a.references.iter().map(|r| r.url.clone()).collect(),
        published_at: a.published.as_deref().and_then(parse_rfc3339).map(ts),
        updated_at: a.modified.as_deref().and_then(parse_rfc3339).map(ts),
        withdrawn_at: a
            .withdrawn
            .as_deref()
            .filter(|w| !w.is_empty())
            .and_then(parse_rfc3339)
            .map(ts)
            .or_else(|| a.is_withdrawn().then(|| ts(0))),
        source_repo: None,
    }
}

/// Builds the OSV record of a repo advisory being published.
#[allow(clippy::too_many_arguments)]
pub(crate) fn osv_of_repo_advisory(
    ghsa_id: &str,
    cve_id: &str,
    summary: &str,
    description: &str,
    cvss_vector: &str,
    cwe_ids: &[String],
    affected: &[AdvisoryAffectedPackage],
    published_ms: u64,
) -> Result<Osv> {
    let mut aff = Vec::new();
    for p in affected {
        let eco = Ecosystem::from_proto_name(&p.ecosystem)
            .or_else(|| Ecosystem::from_osv_name(&p.ecosystem))
            .ok_or_else(|| {
                ForgeError::invalid(
                    "unknown_ecosystem",
                    format!("ecosystem {:?} is not supported", p.ecosystem),
                )
            })?;
        let mut events = vec![serde_json::json!({"introduced": "0"})];
        let mut versions = Vec::new();
        let mut has_lower = false;
        let mut upper_fixed = false;
        for clause in p
            .vulnerable_range
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            if let Some(v) = clause.strip_prefix(">=") {
                events[0] = serde_json::json!({"introduced": v.trim()});
                has_lower = true;
            } else if let Some(v) = clause.strip_prefix('>') {
                events[0] = serde_json::json!({"introduced": v.trim()});
                has_lower = true;
            } else if let Some(v) = clause.strip_prefix("<=") {
                events.push(serde_json::json!({"last_affected": v.trim()}));
                upper_fixed = true;
            } else if let Some(v) = clause.strip_prefix('<') {
                events.push(serde_json::json!({"fixed": v.trim()}));
                upper_fixed = true;
            } else if let Some(v) = clause.strip_prefix('=') {
                versions.push(v.trim().to_string());
            } else {
                return Err(ForgeError::invalid(
                    "bad_vulnerable_range",
                    format!("cannot read range clause {clause:?}"),
                ));
            }
        }
        if !upper_fixed && !p.first_patched_version.is_empty() {
            events.push(serde_json::json!({"fixed": p.first_patched_version}));
        }
        let _ = has_lower;
        let ranges = if versions.is_empty() || events.len() > 1 {
            vec![serde_json::json!({"type": "ECOSYSTEM", "events": events})]
        } else {
            Vec::new()
        };
        aff.push(serde_json::json!({
            "package": {"ecosystem": eco.osv_name(), "name": p.name},
            "ranges": ranges,
            "versions": versions,
        }));
    }
    let mut doc = serde_json::json!({
        "id": ghsa_id,
        "summary": summary,
        "details": description,
        "published": super::rfc3339(published_ms),
        "modified": super::rfc3339(published_ms),
        "affected": aff,
        "database_specific": {"cwe_ids": cwe_ids},
    });
    if !cve_id.is_empty() {
        doc["aliases"] = serde_json::json!([cve_id]);
    }
    if !cvss_vector.is_empty() {
        doc["severity"] = serde_json::json!([{"type": "CVSS_V3", "score": cvss_vector}]);
    }
    Osv::from_json(&doc.to_string()).map_err(|e| ForgeError::invalid("bad_advisory", e.message))
}

impl Security {
    /// Rebuilds the index from the feed and the published advisories when the feed changed (or
    /// on first use), then re-evaluates the repos whose dependencies the changed advisories
    /// touch.
    pub async fn refresh_advisories(&self, cx: &Cx) -> Result<RefreshReport> {
        let version = self.ports.feed.version().await?;
        let (same, loaded) = {
            let idx = self.index.read().map_err(poisoned)?;
            (
                idx.feed_version.as_deref() == Some(version.as_str()),
                idx.published_loaded,
            )
        };
        if same && loaded {
            return Ok(RefreshReport::default());
        }
        let records = self.ports.feed.load().await?;
        let published = self.published_records(cx).await?;
        let mut report = RefreshReport::default();
        let mut fresh: BTreeMap<String, Entry> = BTreeMap::new();
        for text in &records {
            match Osv::from_json(text) {
                Ok(a) => {
                    fresh.insert(
                        a.id.clone(),
                        Entry {
                            digest: digest_of(text),
                            keys: keys_of(&a),
                            origin: Origin::Feed,
                            osv: Arc::new(a),
                        },
                    );
                }
                Err(_) => report.invalid += 1,
            }
        }
        for text in &published {
            if let Ok(a) = Osv::from_json(text) {
                fresh.insert(
                    a.id.clone(),
                    Entry {
                        digest: digest_of(text),
                        keys: keys_of(&a),
                        origin: Origin::Published,
                        osv: Arc::new(a),
                    },
                );
            }
        }
        let mut touched: BTreeSet<(Ecosystem, String)> = BTreeSet::new();
        {
            let mut idx = self.index.write().map_err(poisoned)?;
            let first_load = !idx.published_loaded && idx.feed_version.is_none();
            for (id, e) in &fresh {
                match idx.entries.get(id) {
                    None => {
                        report.added += 1;
                        touched.extend(e.keys.iter().cloned());
                    }
                    Some(old) if old.digest != e.digest => {
                        report.changed += 1;
                        touched.extend(e.keys.iter().cloned());
                        touched.extend(old.keys.iter().cloned());
                    }
                    Some(_) => {}
                }
            }
            for (id, old) in &idx.entries {
                if !fresh.contains_key(id) {
                    report.removed += 1;
                    touched.extend(old.keys.iter().cloned());
                }
            }
            let mut source = FileAdvisorySource::new();
            for e in fresh.values() {
                source.insert((*e.osv).clone());
            }
            idx.source = source;
            idx.entries = fresh;
            idx.feed_version = Some(version);
            idx.published_loaded = true;
            idx.generation += 1;
            if first_load {
                // Nothing was evaluated against an empty index: every repo with dependencies
                // may now have alerts.
                touched = idx
                    .entries
                    .values()
                    .flat_map(|e| e.keys.iter().cloned())
                    .collect();
            }
        }
        report.repos_evaluated = self.reevaluate_for(cx, &touched).await?;
        Ok(report)
    }

    async fn published_records(&self, cx: &Cx) -> Result<Vec<String>> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let rows = tx
            .scan_msgs::<GlobalAdvRec>(&GLOBAL_ADVISORIES, &Scan::scope("", crate::store::MAX_SCAN))
            .await?;
        Ok(rows.into_iter().map(|(r, _)| r.osv_json).collect())
    }

    /// Makes sure the index is loaded (a first call loads it from the ports).
    pub(crate) async fn ensure_index(&self, cx: &Cx) -> Result<()> {
        let loaded = self.index.read().map_err(poisoned)?.published_loaded;
        if !loaded {
            self.refresh_advisories(cx).await?;
        }
        Ok(())
    }

    /// Adds a published repo advisory to the index at once and re-evaluates affected repos.
    pub(crate) async fn index_published(&self, cx: &Cx, osv: Osv) -> Result<()> {
        self.ensure_index(cx).await?;
        let keys = keys_of(&osv);
        {
            let mut idx = self.index.write().map_err(poisoned)?;
            let text = serde_json::to_string(&osv).unwrap_or_default();
            idx.source.insert(osv.clone());
            idx.entries.insert(
                osv.id.clone(),
                Entry {
                    digest: digest_of(&text),
                    keys: keys.clone(),
                    origin: Origin::Published,
                    osv: Arc::new(osv),
                },
            );
            idx.generation += 1;
        }
        let touched: BTreeSet<_> = keys.into_iter().collect();
        self.reevaluate_for(cx, &touched).await?;
        Ok(())
    }

    /// Re-evaluates the repos whose stored graph holds a package in `touched`; returns how many.
    pub(crate) async fn reevaluate_for(
        &self,
        cx: &Cx,
        touched: &BTreeSet<(Ecosystem, String)>,
    ) -> Result<usize> {
        if touched.is_empty() {
            return Ok(0);
        }
        let mut candidates: Vec<(String, GraphRec)> = Vec::new();
        {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            let rows = tx
                .scan_msgs::<GraphRec>(&GRAPHS, &Scan::scope("", crate::store::MAX_SCAN))
                .await?;
            for (g, row) in rows {
                let hit = g.packages.iter().any(|p| {
                    Ecosystem::from_proto_name(&p.ecosystem)
                        .is_some_and(|e| touched.contains(&(e, e.normalize_name(&p.name))))
                });
                if hit {
                    candidates.push((row.id.clone(), g));
                }
            }
        }
        let mut n = 0;
        for (repo_id, g) in candidates {
            let Ok(repo) = self.orgs.repo_any(cx, &repo_id).await else {
                continue;
            };
            if repo.archived || repo.deleted_at.is_some() {
                continue;
            }
            let config = self.config_of(cx, &repo).await?;
            if !(config.dependency_graph && config.dependency_alerts) {
                continue;
            }
            let graph = super::depgraph::graph_of_rec(&g);
            self.evaluate_graph(cx, &repo, &graph).await?;
            n += 1;
        }
        Ok(n)
    }

    /// `ListAdvisories`: the global database.
    pub async fn list_advisories(
        &self,
        cx: &Cx,
        req: &ListAdvisoriesRequest,
        token: &str,
        size: usize,
    ) -> Result<(Vec<Advisory>, String)> {
        self.ensure_index(cx).await?;
        let eco = if req.ecosystem.is_empty() {
            None
        } else {
            Some(
                Ecosystem::from_proto_name(&req.ecosystem)
                    .or_else(|| Ecosystem::from_osv_name(&req.ecosystem))
                    .ok_or_else(|| {
                        ForgeError::invalid("unknown_ecosystem", "unknown ecosystem filter")
                    })?,
            )
        };
        let mut out: Vec<Advisory> = {
            let idx = self.index.read().map_err(poisoned)?;
            idx.entries
                .values()
                .filter(|e| {
                    eco.is_none_or(|x| e.keys.iter().any(|k| k.0 == x))
                        && (req.package_name.is_empty()
                            || e.keys.iter().any(|k| {
                                eco.map(|x| x.normalize_name(&req.package_name))
                                    .unwrap_or_else(|| req.package_name.clone())
                                    == k.1
                                    || k.1 == req.package_name
                            }))
                })
                .map(|e| to_proto(&e.osv))
                .filter(|a| {
                    (req.severity == 0 || a.severity == req.severity)
                        && (req.r#type == 0 || a.r#type == req.r#type)
                        && (req.cwe_id.is_empty() || a.cwe_ids.contains(&req.cwe_id))
                        && req.published_after.as_ref().is_none_or(|t| {
                            a.published_at.as_ref().is_some_and(|p| ts_ms(p) > ts_ms(t))
                        })
                        && req.updated_after.as_ref().is_none_or(|t| {
                            a.updated_at.as_ref().is_some_and(|p| ts_ms(p) > ts_ms(t))
                        })
                })
                .collect()
        };
        out.sort_by(|a, b| {
            let pa = a.published_at.as_ref().map(ts_ms).unwrap_or(0);
            let pb = b.published_at.as_ref().map(ts_ms).unwrap_or(0);
            pb.cmp(&pa).then_with(|| a.ghsa_id.cmp(&b.ghsa_id))
        });
        page_slice(&out, token, size)
    }

    /// `GetAdvisory`: one advisory by GHSA or CVE id (or any alias).
    pub async fn get_advisory(&self, cx: &Cx, req: &GetAdvisoryRequest) -> Result<Advisory> {
        use bee_proto::v1::get_advisory_request::Key;
        self.ensure_index(cx).await?;
        let id = match &req.key {
            Some(Key::GhsaId(i)) | Some(Key::CveId(i)) if !i.is_empty() => i.clone(),
            _ => return Err(ForgeError::missing("ghsa_id or cve_id")),
        };
        let (mut adv, published) = {
            let idx = self.index.read().map_err(poisoned)?;
            let adv = idx
                .source
                .get(&id)
                .map(|a| to_proto(&a))
                .ok_or_else(|| ForgeError::not_found(format!("advisory {id}")))?;
            (adv, found_origin(&idx, &id) == Some(Origin::Published))
        };
        if published {
            // Published repo advisories keep the repo they came from.
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            if let Some((g, _)) = tx
                .get_msg::<GlobalAdvRec>(&GLOBAL_ADVISORIES, "", &adv.ghsa_id)
                .await?
                && let Some(p) = g.advisory
            {
                adv.source_repo = p.source_repo;
            }
        }
        Ok(adv)
    }

    /// The wire advisory of an indexed id, if any.
    pub(crate) fn advisory_proto(&self, id: &str) -> Option<Advisory> {
        let idx = self.index.read().ok()?;
        idx.source.get(id).map(|a| to_proto(&a))
    }

    /// Whether the advisory is gone or withdrawn.
    pub(crate) fn advisory_gone(&self, id: &str) -> bool {
        match self.index.read() {
            Ok(idx) => idx.source.get(id).is_none_or(|a| a.is_withdrawn()),
            Err(_) => false,
        }
    }

    /// The advisory index generation.
    pub(crate) fn generation(&self) -> u64 {
        self.index.read().map(|i| i.generation).unwrap_or(0)
    }
}

fn found_origin(idx: &AdvisoryIndex, id: &str) -> Option<Origin> {
    idx.entries
        .get(id)
        .or_else(|| {
            idx.entries
                .values()
                .find(|e| e.osv.aliases.iter().any(|a| a == id))
        })
        .map(|e| e.origin)
}

/// A poisoned lock is an internal error.
pub(crate) fn poisoned<T>(_: std::sync::PoisonError<T>) -> ForgeError {
    ForgeError::internal("advisory index lock poisoned")
}
