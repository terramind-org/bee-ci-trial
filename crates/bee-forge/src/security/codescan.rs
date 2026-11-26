//! Code scanning: SARIF 2.1 ingest, alerts deduplicated by fingerprint across revisions and
//! refs, analyses, the check run published per analysis, the default setup and code quality
//! findings.
//!
//! An alert is identified by `<tool>|<rule id>|<path>|<fingerprint>`, where the fingerprint is
//! the SARIF `partialFingerprints` value (else `fingerprints`, else a hash of the rule, path and
//! snippet). Results of one run that share all four get a `:n` suffix so each stays distinct.
//! Each (ref, category) holds one instance of an alert; the alert is open while any instance is
//! open, fixed when every instance is fixed, and dismissed alerts stay dismissed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use bee_proto::v1::{
    AnnotationLevel, CheckAnnotation, CheckOutput, CheckStatus, CodeQualityFindingState,
    CodeScanningAlert, CodeScanningAlertInstance, CodeScanningAlertState, CodeScanningAnalysis,
    CodeScanningDefaultSetup, CodeScanningDismissReason, CodeScanningLocation, CodeScanningRule,
    CodeScanningSarifStatus, CodeScanningSarifUpload, CodeScanningSetupState,
    ListCodeQualityFindingsRequest, ListCodeScanningAlertsRequest, ObjectId, Repo,
    SecurityCodeQualityFinding, SecuritySeverity, SubjectKind, UpdateCodeQualityFindingRequest,
    UpdateCodeScanningAlertRequest, UpdateCodeScanningDefaultSetupRequest, UploadSarifRequest,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::model::{CodeAlertRec, InstanceRec, QualityRec, SarifRec};
use super::{
    ANALYSES, CODE_ALERTS, CODE_INSTANCES, CONFIG, Change, QUALITY, SARIF, Security, num_id,
    page_slice, record, repo_ref_of,
};
use crate::checks::Report;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::store::{Key, Row, Scan, Surface};

/// Largest gzip document accepted inline.
pub const MAX_SARIF_GZIP: usize = 10 << 20;
/// Largest SARIF document after decompression.
pub const MAX_SARIF_BYTES: usize = 100 << 20;
/// Most runs in one SARIF log.
pub const MAX_RUNS: usize = 20;
/// Most results in one run.
pub const MAX_RESULTS: usize = 25_000;

const QUALITY_TAGS: &[&str] = &[
    "maintainability",
    "reliability",
    "readability",
    "code-quality",
    "quality",
];

/// One result of a run, normalized.
struct Item {
    rule: CodeScanningRule,
    key: String,
    path: String,
    start_line: u32,
    end_line: u32,
    start_column: u32,
    end_column: u32,
    message: String,
    level: String,
    classifications: Vec<String>,
    quality: bool,
}

struct Run {
    tool: String,
    version: String,
    category: String,
    items: Vec<Item>,
    rules: usize,
    warnings: Vec<String>,
}

fn bad(msg: impl Into<String>) -> ForgeError {
    ForgeError::invalid("invalid_sarif", msg)
}

fn text_of(v: &Value) -> String {
    v.get("text")
        .and_then(Value::as_str)
        .or_else(|| v.get("markdown").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

fn sev_from_score(score: f64) -> SecuritySeverity {
    if score >= 9.0 {
        SecuritySeverity::Critical
    } else if score >= 7.0 {
        SecuritySeverity::High
    } else if score >= 4.0 {
        SecuritySeverity::Medium
    } else if score > 0.0 {
        SecuritySeverity::Low
    } else {
        SecuritySeverity::Unspecified
    }
}

fn sev_from_level(level: &str) -> SecuritySeverity {
    match level {
        "error" => SecuritySeverity::High,
        "warning" => SecuritySeverity::Medium,
        "note" => SecuritySeverity::Low,
        _ => SecuritySeverity::Unspecified,
    }
}

fn normalize_path(uri: &str, checkout: &str) -> String {
    let mut p = uri.strip_prefix("file://").unwrap_or(uri).to_string();
    if !checkout.is_empty() {
        let c = checkout.strip_prefix("file://").unwrap_or(checkout);
        if let Some(rest) = p.strip_prefix(c) {
            p = rest.to_string();
        }
    }
    while let Some(r) = p.strip_prefix("./") {
        p = r.to_string();
    }
    p.trim_start_matches('/').to_string()
}

fn classify_path(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let segs: Vec<&str> = path.split('/').collect();
    let base = segs.last().copied().unwrap_or("");
    if segs
        .iter()
        .any(|s| matches!(*s, "test" | "tests" | "__tests__" | "spec" | "specs"))
        || base.contains("_test.")
        || base.contains(".test.")
        || base.contains(".spec.")
    {
        out.push("test".to_string());
    }
    if segs.contains(&"generated")
        || base.ends_with(".pb.go")
        || base.ends_with(".g.dart")
        || base.ends_with(".min.js")
    {
        out.push("generated".to_string());
    }
    out
}

/// Decompresses a gzip document with a size cap.
pub fn gunzip(bytes: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut d = flate2::read::GzDecoder::new(bytes);
    let mut out = Vec::new();
    d.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| bad(format!("the SARIF document is not valid gzip: {e}")))?;
    if out.len() > limit {
        return Err(ForgeError::too_large(
            "sarif_bytes",
            limit as u64,
            out.len() as u64,
        ));
    }
    Ok(out)
}

/// Parses and normalizes a SARIF 2.1 log into runs.
fn parse_sarif(
    doc: &[u8],
    tool_override: &str,
    category_override: &str,
    checkout: &str,
) -> Result<Vec<Run>> {
    let v: Value = serde_json::from_slice(doc).map_err(|e| bad(format!("not JSON: {e}")))?;
    if v.get("version").and_then(Value::as_str) != Some("2.1.0") {
        return Err(bad("`version` must be \"2.1.0\""));
    }
    let runs = v
        .get("runs")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("`runs` must be an array"))?;
    if runs.len() > MAX_RUNS {
        return Err(ForgeError::too_large(
            "sarif_runs",
            MAX_RUNS as u64,
            runs.len() as u64,
        ));
    }
    let mut out = Vec::new();
    for (ri, run) in runs.iter().enumerate() {
        let driver = run
            .pointer("/tool/driver")
            .ok_or_else(|| bad(format!("runs[{ri}].tool.driver is missing")))?;
        let driver_name = driver
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| bad(format!("runs[{ri}].tool.driver.name is missing")))?;
        let tool = if tool_override.is_empty() {
            driver_name
        } else {
            tool_override
        }
        .to_string();
        let version = driver
            .get("semanticVersion")
            .or_else(|| driver.get("version"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let category = if category_override.is_empty() {
            run.pointer("/automationDetails/id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_string()
        } else {
            category_override.to_string()
        };
        let rules: Vec<&Value> = driver
            .get("rules")
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        let results = run
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| bad(format!("runs[{ri}].results must be an array")))?;
        if results.len() > MAX_RESULTS {
            return Err(ForgeError::too_large(
                "sarif_results",
                MAX_RESULTS as u64,
                results.len() as u64,
            ));
        }
        let mut warnings = Vec::new();
        let mut items: Vec<Item> = Vec::new();
        let mut seen: BTreeMap<String, u32> = BTreeMap::new();
        for (i, r) in results.iter().enumerate() {
            let idx = r
                .get("ruleIndex")
                .or_else(|| r.pointer("/rule/index"))
                .and_then(Value::as_u64)
                .map(|n| n as usize);
            let rule_def = idx.and_then(|n| rules.get(n).copied());
            let rule_id = r
                .get("ruleId")
                .or_else(|| r.pointer("/rule/id"))
                .and_then(Value::as_str)
                .or_else(|| rule_def.and_then(|d| d.get("id").and_then(Value::as_str)))
                .unwrap_or_default()
                .to_string();
            if rule_id.is_empty() {
                warnings.push(format!("result {i} has no rule id and was skipped"));
                continue;
            }
            let rule_def = rule_def.or_else(|| {
                rules
                    .iter()
                    .copied()
                    .find(|d| d.get("id").and_then(Value::as_str) == Some(rule_id.as_str()))
            });
            if r.get("suppressions")
                .and_then(Value::as_array)
                .is_some_and(|s| !s.is_empty())
            {
                continue;
            }
            let level = r
                .get("level")
                .and_then(Value::as_str)
                .or_else(|| {
                    rule_def.and_then(|d| {
                        d.pointer("/defaultConfiguration/level")
                            .and_then(Value::as_str)
                    })
                })
                .unwrap_or("warning")
                .to_string();
            if level == "none" {
                continue;
            }
            let props = rule_def.and_then(|d| d.get("properties"));
            let tags: Vec<String> = props
                .and_then(|p| p.get("tags"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let score = props
                .and_then(|p| p.get("security-severity"))
                .and_then(|s| {
                    s.as_str()
                        .map(str::to_string)
                        .or_else(|| s.as_f64().map(|f| f.to_string()))
                })
                .and_then(|s| s.parse::<f64>().ok());
            let security_severity = score
                .map(sev_from_score)
                .unwrap_or_else(|| sev_from_level(&level));
            let loc = r.pointer("/locations/0/physicalLocation");
            let path = loc
                .and_then(|l| l.pointer("/artifactLocation/uri").and_then(Value::as_str))
                .map(|u| normalize_path(u, checkout))
                .unwrap_or_default();
            let region = loc.and_then(|l| l.get("region"));
            let num = |k: &str| {
                region
                    .and_then(|g| g.get(k))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32
            };
            let start_line = num("startLine").max(1);
            let end_line = num("endLine").max(start_line);
            let message = r.get("message").map(text_of).unwrap_or_default();
            let snippet = region
                .and_then(|g| g.pointer("/snippet/text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let fp = fingerprint_of(r, &rule_id, &path, snippet, &message);
            let n = seen.entry(format!("{rule_id}|{path}|{fp}")).or_insert(0);
            *n += 1;
            let fp = if *n > 1 {
                format!("{fp}:{}", *n - 1)
            } else {
                fp
            };
            let quality = tags
                .iter()
                .any(|t| QUALITY_TAGS.contains(&t.to_ascii_lowercase().as_str()))
                && !tags.iter().any(|t| t.eq_ignore_ascii_case("security"));
            let mut classifications = classify_path(&path);
            if let Some(c) = r
                .pointer("/properties/classifications")
                .and_then(Value::as_array)
            {
                for x in c.iter().filter_map(Value::as_str) {
                    if !classifications.iter().any(|y| y == x) {
                        classifications.push(x.to_string());
                    }
                }
            }
            items.push(Item {
                rule: CodeScanningRule {
                    id: rule_id.clone(),
                    name: rule_def
                        .and_then(|d| d.get("name").and_then(Value::as_str))
                        .unwrap_or(&rule_id)
                        .to_string(),
                    description: rule_def
                        .map(|d| {
                            let s = d.get("shortDescription").map(text_of).unwrap_or_default();
                            if s.is_empty() {
                                d.get("fullDescription").map(text_of).unwrap_or_default()
                            } else {
                                s
                            }
                        })
                        .unwrap_or_default(),
                    security_severity: security_severity as i32,
                    severity: level.clone(),
                    tags,
                    help: rule_def
                        .and_then(|d| d.get("help"))
                        .map(text_of)
                        .unwrap_or_default(),
                },
                key: format!("{tool}|{rule_id}|{path}|{fp}"),
                path,
                start_line,
                end_line,
                start_column: num("startColumn"),
                end_column: num("endColumn"),
                message,
                level,
                classifications,
                quality,
            });
        }
        out.push(Run {
            tool,
            version,
            category,
            items,
            rules: rules.len(),
            warnings,
        });
    }
    Ok(out)
}

fn fingerprint_of(r: &Value, rule: &str, path: &str, snippet: &str, message: &str) -> String {
    for field in ["partialFingerprints", "fingerprints"] {
        if let Some(m) = r.get(field).and_then(Value::as_object) {
            let pick = m
                .get("primaryLocationLineHash")
                .or_else(|| m.values().next())
                .and_then(Value::as_str);
            if let Some(f) = pick.filter(|f| !f.is_empty()) {
                return f.to_string();
            }
        }
    }
    let norm: String = if snippet.is_empty() { message } else { snippet }
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut h = Sha256::new();
    h.update(rule.as_bytes());
    h.update([0]);
    h.update(path.as_bytes());
    h.update([0]);
    h.update(norm.as_bytes());
    crate::ids::hex(&h.finalize())[..32].to_string()
}

fn alert_state(a: &CodeScanningAlert) -> CodeScanningAlertState {
    CodeScanningAlertState::try_from(a.state).unwrap_or(CodeScanningAlertState::Unspecified)
}

fn state_label(s: CodeScanningAlertState) -> &'static str {
    match s {
        CodeScanningAlertState::Open => "open",
        CodeScanningAlertState::Dismissed => "dismissed",
        CodeScanningAlertState::Fixed => "fixed",
        CodeScanningAlertState::Unspecified => "unspecified",
    }
}

fn alert_json(a: &CodeScanningAlert) -> String {
    serde_json::json!({
        "number": a.number,
        "state": state_label(alert_state(a)),
        "rule": a.rule.as_ref().map(|r| r.id.clone()),
        "tool": a.tool_name,
        "dismissed_reason": CodeScanningDismissReason::try_from(a.dismissed_reason)
            .map(|r| r.as_str_name()).unwrap_or(""),
    })
    .to_string()
}

fn instance_id(r: &str, category: &str) -> String {
    format!("{r}|{category}")
}

impl Security {
    // ---- uploads --------------------------------------------------------------------------

    /// `UploadSarif`: validates the log, stores the upload and processes it. Bad SARIF is
    /// `INVALID_ARGUMENT`; the returned upload says whether processing completed.
    pub async fn upload_sarif(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &UploadSarifRequest,
    ) -> Result<CodeScanningSarifUpload> {
        use bee_proto::v1::upload_sarif_request::Sarif;
        let changeset = req
            .changeset
            .clone()
            .filter(|c| !c.raw.is_empty())
            .ok_or_else(|| ForgeError::missing("changeset"))?;
        let ref_name = if req.r#ref.is_empty() {
            repo.default_ref.clone()
        } else {
            req.r#ref.clone()
        };
        let gzip = match &req.sarif {
            Some(Sarif::SarifGzip(b)) => {
                if b.len() > MAX_SARIF_GZIP {
                    return Err(ForgeError::too_large(
                        "sarif_gzip_bytes",
                        MAX_SARIF_GZIP as u64,
                        b.len() as u64,
                    ));
                }
                b.clone()
            }
            Some(Sarif::SarifUrl(u)) => self.ports.fetcher.fetch(u, MAX_SARIF_GZIP).await?,
            None => return Err(ForgeError::missing("sarif")),
        };
        let doc = gunzip(&gzip, MAX_SARIF_BYTES)?;
        let runs = parse_sarif(&doc, &req.tool_name, &req.category, &req.checkout_uri)?;
        let now = self.now();
        let tool_name = runs.first().map(|r| r.tool.clone()).unwrap_or_default();
        let category = runs.first().map(|r| r.category.clone()).unwrap_or_default();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let id = tx.new_id()?;
        let upload = CodeScanningSarifUpload {
            id: id.clone(),
            repo: repo_ref_of(repo),
            changeset: Some(changeset),
            r#ref: ref_name,
            tool_name,
            category,
            status: CodeScanningSarifStatus::Pending as i32,
            errors: Vec::new(),
            analysis_ids: Vec::new(),
            created_at: Some(ts(now)),
            processed_at: None,
        };
        let rec = SarifRec {
            upload: Some(upload.clone()),
            sarif_gzip: gzip,
            started_ms: req.started_at.as_ref().map(ts_ms).unwrap_or(0),
            checkout_uri: req.checkout_uri.clone(),
            uploader: cx.actor_id.clone(),
            tool_override: req.tool_name.clone(),
            category_override: req.category.clone(),
        };
        tx.insert(
            &SARIF,
            &Row::new(&repo.id, &id)
                .k2("pending")
                .n1(now as i64)
                .msg(&rec),
        )
        .await?;
        tx.commit().await?;
        self.process_sarif(cx, repo, &id).await
    }

    /// Processes uploads left `PENDING` (a crash between store and processing).
    pub async fn process_pending_sarif(&self, cx: &Cx) -> Result<usize> {
        let pending: Vec<(String, String)> = {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            tx.scan_msgs::<SarifRec>(
                &SARIF,
                &Scan {
                    key: Some(Key::K2("pending".into())),
                    limit: 100,
                    ..Scan::default()
                },
            )
            .await?
            .into_iter()
            .map(|(_, row)| (row.scope, row.id))
            .collect()
        };
        let mut n = 0;
        for (repo_id, id) in pending {
            let Ok(repo) = self.orgs.repo_any(cx, &repo_id).await else {
                continue;
            };
            self.process_sarif(cx, &repo, &id).await?;
            n += 1;
        }
        Ok(n)
    }

    async fn process_sarif(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
    ) -> Result<CodeScanningSarifUpload> {
        let (rec, row) = {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            tx.get_msg::<SarifRec>(&SARIF, &repo.id, id)
                .await?
                .ok_or_else(|| ForgeError::not_found(format!("SARIF upload {id}")))?
        };
        let mut upload = rec.upload.clone().unwrap_or_default();
        if upload.status != CodeScanningSarifStatus::Pending as i32 {
            return Ok(upload);
        }
        let outcome = match gunzip(&rec.sarif_gzip, MAX_SARIF_BYTES).and_then(|d| {
            parse_sarif(
                &d,
                &rec.tool_override,
                &rec.category_override,
                &rec.checkout_uri,
            )
        }) {
            Ok(runs) => self.apply_runs(cx, repo, &upload, &rec, runs).await,
            Err(e) => Err(e),
        };
        let now = self.now();
        match outcome {
            Ok(ids) => {
                upload.status = CodeScanningSarifStatus::Complete as i32;
                upload.analysis_ids = ids;
            }
            Err(e) => {
                upload.status = CodeScanningSarifStatus::Failed as i32;
                upload.errors = vec![e.message.clone()];
            }
        }
        upload.processed_at = Some(ts(now));
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let mut done = rec.clone();
        done.upload = Some(upload.clone());
        done.sarif_gzip.clear();
        let label = if upload.status == CodeScanningSarifStatus::Complete as i32 {
            "complete"
        } else {
            "failed"
        };
        tx.put(
            &SARIF,
            &Row::new(&repo.id, id)
                .k2(label)
                .n1(row.n1.unwrap_or(0))
                .msg(&done),
        )
        .await?;
        tx.commit().await?;
        Ok(upload)
    }

    /// `GetSarifUpload`.
    pub async fn get_sarif_upload(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
    ) -> Result<CodeScanningSarifUpload> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.get_msg::<SarifRec>(&SARIF, &repo.id, id)
            .await?
            .and_then(|(r, _)| r.upload)
            .ok_or_else(|| ForgeError::not_found(format!("SARIF upload {id}")))
    }

    /// Stores each run as an analysis, updates alert instances and publishes check runs.
    async fn apply_runs(
        &self,
        cx: &Cx,
        repo: &Repo,
        upload: &CodeScanningSarifUpload,
        rec: &SarifRec,
        runs: Vec<Run>,
    ) -> Result<Vec<String>> {
        let changeset = upload.changeset.clone().unwrap_or_default();
        let mut ids = Vec::new();
        #[allow(clippy::type_complexity)]
        let mut reports: Vec<(Run, String, Vec<(String, u64)>)> = Vec::new();
        for run in runs {
            let now = self.now();
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            let analysis_id = tx.new_id()?;
            let mut present: BTreeSet<String> = BTreeSet::new();
            let mut touched: Vec<(String, u64)> = Vec::new();
            for it in run.items.iter().filter(|i| !i.quality) {
                present.insert(it.key.clone());
                let n = self
                    .upsert_code_item(
                        &mut tx,
                        repo,
                        &run,
                        it,
                        upload,
                        &analysis_id,
                        &changeset,
                        now,
                    )
                    .await?;
                touched.push((it.level.clone(), n));
            }
            self.fix_absent_code_items(
                &mut tx,
                repo,
                &run,
                upload,
                &present,
                &analysis_id,
                &changeset,
                now,
            )
            .await?;
            self.sync_quality(&mut tx, repo, &run, upload, now).await?;
            let analysis = CodeScanningAnalysis {
                id: analysis_id.clone(),
                repo: repo_ref_of(repo),
                r#ref: upload.r#ref.clone(),
                changeset: Some(changeset.clone()),
                tool_name: run.tool.clone(),
                tool_version: run.version.clone(),
                category: run.category.clone(),
                results_count: run.items.len() as u32,
                rules_count: run.rules as u32,
                sarif_upload_id: upload.id.clone(),
                error: String::new(),
                warnings: run.warnings.clone(),
                created_at: Some(ts(now)),
            };
            tx.insert(
                &ANALYSES,
                &Row::new(&repo.id, &analysis_id)
                    .k1(&analysis.r#ref)
                    .k2(&analysis.tool_name)
                    .n1(now as i64)
                    .msg(&analysis),
            )
            .await?;
            record(
                &mut tx,
                Change {
                    repo,
                    event: "code_scanning.analysis_processed",
                    action: "security.code_scanning.analysis",
                    subject: SubjectKind::Repo,
                    id: &analysis_id,
                    payload: serde_json::to_value(&analysis).unwrap_or_default(),
                    before: String::new(),
                    after: serde_json::json!({
                        "analysis_id": analysis_id, "tool": run.tool, "ref": upload.r#ref,
                        "results": run.items.len(),
                    })
                    .to_string(),
                },
            )
            .await?;
            tx.commit().await?;
            ids.push(analysis_id.clone());
            reports.push((run, analysis_id, touched));
        }
        for (run, analysis_id, _) in reports {
            self.publish_check(repo, upload, &run, &analysis_id, rec)
                .await?;
        }
        Ok(ids)
    }

    #[allow(clippy::too_many_arguments)]
    async fn upsert_code_item(
        &self,
        tx: &mut crate::store::Tx,
        repo: &Repo,
        run: &Run,
        it: &Item,
        upload: &CodeScanningSarifUpload,
        analysis_id: &str,
        changeset: &ObjectId,
        now: u64,
    ) -> Result<u64> {
        let instance = CodeScanningAlertInstance {
            r#ref: upload.r#ref.clone(),
            analysis_id: analysis_id.to_string(),
            category: run.category.clone(),
            state: CodeScanningAlertState::Open as i32,
            changeset: Some(changeset.clone()),
            location: Some(CodeScanningLocation {
                path: it.path.as_bytes().to_vec(),
                start_line: it.start_line,
                end_line: it.end_line,
                start_column: it.start_column,
                end_column: it.end_column,
            }),
            message: it.message.clone(),
            classifications: it.classifications.clone(),
        };
        let found = tx
            .scan_msgs::<CodeAlertRec>(
                &CODE_ALERTS,
                &Scan::scope(&repo.id, 1).key(Key::K1(it.key.clone())),
            )
            .await?
            .into_iter()
            .next();
        let number;
        match found {
            None => {
                number = super::next_number(tx, &repo.id, "code_alert").await?;
                let alert = CodeScanningAlert {
                    number,
                    repo: repo_ref_of(repo),
                    state: CodeScanningAlertState::Open as i32,
                    rule: Some(it.rule.clone()),
                    tool_name: run.tool.clone(),
                    tool_version: run.version.clone(),
                    most_recent_instance: Some(instance.clone()),
                    created_at: Some(ts(now)),
                    updated_at: Some(ts(now)),
                    url: format!("/{}/security/code-scanning/{number}", repo.full_name),
                    ..Default::default()
                };
                let rec = CodeAlertRec {
                    alert: Some(alert.clone()),
                    key: it.key.clone(),
                    first_changeset: instance.changeset.clone(),
                };
                tx.insert(
                    &CODE_ALERTS,
                    &Row::new(&repo.id, num_id(number))
                        .k1(&it.key)
                        .k2("open")
                        .n1(now as i64)
                        .msg(&rec),
                )
                .await?;
                record(
                    tx,
                    Change {
                        repo,
                        event: "code_scanning_alert.created",
                        action: "security.code_alert.open",
                        subject: SubjectKind::SecurityAlert,
                        id: &number.to_string(),
                        payload: serde_json::to_value(&alert).unwrap_or_default(),
                        before: String::new(),
                        after: alert_json(&alert),
                    },
                )
                .await?;
            }
            Some((mut rec, row)) => {
                let mut alert = rec.alert.clone().unwrap_or_default();
                number = alert.number;
                let before = alert_json(&alert);
                let was_fixed = alert_state(&alert) == CodeScanningAlertState::Fixed;
                alert.most_recent_instance = Some(instance.clone());
                alert.tool_version = run.version.clone();
                alert.rule = Some(it.rule.clone());
                alert.updated_at = Some(ts(now));
                if was_fixed {
                    alert.state = CodeScanningAlertState::Open as i32;
                    alert.fixed_at = None;
                }
                rec.alert = Some(alert.clone());
                tx.update(
                    &CODE_ALERTS,
                    &row.clone()
                        .k2(state_label(alert_state(&alert)))
                        .n1(now as i64)
                        .msg(&rec),
                )
                .await?;
                if was_fixed {
                    record(
                        tx,
                        Change {
                            repo,
                            event: "code_scanning_alert.reopened",
                            action: "security.code_alert.reopen",
                            subject: SubjectKind::SecurityAlert,
                            id: &number.to_string(),
                            payload: serde_json::to_value(&alert).unwrap_or_default(),
                            before,
                            after: alert_json(&alert),
                        },
                    )
                    .await?;
                }
            }
        }
        let scope = format!("{}:{}", repo.id, num_id(number));
        let iid = instance_id(&upload.r#ref, &run.category);
        tx.put(
            &CODE_INSTANCES,
            &Row::new(&scope, &iid).k1(&upload.r#ref).msg(&InstanceRec {
                instance: Some(instance),
            }),
        )
        .await?;
        Ok(number)
    }

    /// Instances of this tool on this ref and category that the run no longer reports are fixed
    /// there; an alert with no open instance left is fixed.
    #[allow(clippy::too_many_arguments)]
    async fn fix_absent_code_items(
        &self,
        tx: &mut crate::store::Tx,
        repo: &Repo,
        run: &Run,
        upload: &CodeScanningSarifUpload,
        present: &BTreeSet<String>,
        analysis_id: &str,
        changeset: &ObjectId,
        now: u64,
    ) -> Result<()> {
        let alerts = tx
            .scan_msgs::<CodeAlertRec>(&CODE_ALERTS, &Scan::scope(&repo.id, crate::store::MAX_SCAN))
            .await?;
        for (mut rec, row) in alerts {
            let Some(mut alert) = rec.alert.clone() else {
                continue;
            };
            if alert.tool_name != run.tool || present.contains(&rec.key) {
                continue;
            }
            let scope = format!("{}:{}", repo.id, num_id(alert.number));
            let iid = instance_id(&upload.r#ref, &run.category);
            let Some((mut inst_rec, inst_row)) = tx
                .get_msg::<InstanceRec>(&CODE_INSTANCES, &scope, &iid)
                .await?
            else {
                continue;
            };
            let Some(mut inst) = inst_rec.instance.clone() else {
                continue;
            };
            if inst.state != CodeScanningAlertState::Open as i32 {
                continue;
            }
            inst.state = CodeScanningAlertState::Fixed as i32;
            inst.analysis_id = analysis_id.to_string();
            inst.changeset = Some(changeset.clone());
            inst_rec.instance = Some(inst.clone());
            tx.update(&CODE_INSTANCES, &inst_row.clone().msg(&inst_rec))
                .await?;
            let others = tx
                .scan_msgs::<InstanceRec>(&CODE_INSTANCES, &Scan::scope(&scope, 1000))
                .await?;
            let any_open = others.iter().any(|(o, _)| {
                o.instance
                    .as_ref()
                    .is_some_and(|i| i.state == CodeScanningAlertState::Open as i32)
            });
            alert.most_recent_instance = Some(inst);
            alert.updated_at = Some(ts(now));
            if !any_open && alert_state(&alert) == CodeScanningAlertState::Open {
                let before = alert_json(&alert);
                alert.state = CodeScanningAlertState::Fixed as i32;
                alert.fixed_at = Some(ts(now));
                rec.alert = Some(alert.clone());
                tx.update(
                    &CODE_ALERTS,
                    &row.clone().k2("fixed").n1(now as i64).msg(&rec),
                )
                .await?;
                record(
                    tx,
                    Change {
                        repo,
                        event: "code_scanning_alert.fixed",
                        action: "security.code_alert.fix",
                        subject: SubjectKind::SecurityAlert,
                        id: &alert.number.to_string(),
                        payload: serde_json::to_value(&alert).unwrap_or_default(),
                        before,
                        after: alert_json(&alert),
                    },
                )
                .await?;
            } else {
                rec.alert = Some(alert);
                tx.update(&CODE_ALERTS, &row.clone().n1(now as i64).msg(&rec))
                    .await?;
            }
        }
        Ok(())
    }

    /// Code quality findings of a run: open for present results, fixed for absent ones.
    async fn sync_quality(
        &self,
        tx: &mut crate::store::Tx,
        repo: &Repo,
        run: &Run,
        upload: &CodeScanningSarifUpload,
        now: u64,
    ) -> Result<()> {
        let existing = tx
            .scan_msgs::<QualityRec>(&QUALITY, &Scan::scope(&repo.id, crate::store::MAX_SCAN))
            .await?;
        let mut by_key: BTreeMap<String, (QualityRec, Row)> = existing
            .into_iter()
            .map(|(r, row)| (r.key.clone(), (r, row)))
            .collect();
        let mut present = BTreeSet::new();
        let scope_prefix = format!("{}|{}|", upload.r#ref, run.category);
        for it in run.items.iter().filter(|i| i.quality) {
            let key = format!("{}|{}|{}", upload.r#ref, run.category, it.key);
            present.insert(key.clone());
            let loc = CodeScanningLocation {
                path: it.path.as_bytes().to_vec(),
                start_line: it.start_line,
                end_line: it.end_line,
                start_column: it.start_column,
                end_column: it.end_column,
            };
            let category = it
                .rule
                .tags
                .iter()
                .find(|t| QUALITY_TAGS.contains(&t.to_ascii_lowercase().as_str()))
                .cloned()
                .unwrap_or_else(|| "maintainability".into());
            match by_key.remove(&key) {
                Some((mut rec, row)) => {
                    let mut f = rec.finding.clone().unwrap_or_default();
                    if f.state == CodeQualityFindingState::Fixed as i32 {
                        f.state = CodeQualityFindingState::Open as i32;
                    }
                    f.location = Some(loc);
                    f.message = it.message.clone();
                    rec.finding = Some(f.clone());
                    let label = if f.state == CodeQualityFindingState::Open as i32 {
                        "open"
                    } else {
                        "dismissed"
                    };
                    tx.update(&QUALITY, &row.clone().k2(label).msg(&rec))
                        .await?;
                }
                None => {
                    let number = super::next_number(tx, &repo.id, "quality").await?;
                    let f = SecurityCodeQualityFinding {
                        number,
                        repo: repo_ref_of(repo),
                        rule_id: it.rule.id.clone(),
                        category,
                        severity: super::sev_of(it.rule.security_severity) as i32,
                        state: CodeQualityFindingState::Open as i32,
                        location: Some(loc),
                        message: it.message.clone(),
                        r#ref: upload.r#ref.clone(),
                        tool_name: run.tool.clone(),
                        created_at: Some(ts(now)),
                    };
                    let rec = QualityRec {
                        finding: Some(f.clone()),
                        key: key.clone(),
                    };
                    tx.insert(
                        &QUALITY,
                        &Row::new(&repo.id, num_id(number))
                            .k1(&key)
                            .k2("open")
                            .msg(&rec),
                    )
                    .await?;
                    record(
                        tx,
                        Change {
                            repo,
                            event: "code_quality_finding.updated",
                            action: "security.code_quality.open",
                            subject: SubjectKind::SecurityAlert,
                            id: &number.to_string(),
                            payload: serde_json::to_value(&f).unwrap_or_default(),
                            before: String::new(),
                            after: serde_json::json!({"number": number, "state": "open"})
                                .to_string(),
                        },
                    )
                    .await?;
                }
            }
        }
        for (key, (mut rec, row)) in by_key {
            let Some(mut f) = rec.finding.clone() else {
                continue;
            };
            if f.tool_name != run.tool
                || !key.starts_with(&scope_prefix)
                || present.contains(&key)
                || f.state != CodeQualityFindingState::Open as i32
            {
                continue;
            }
            f.state = CodeQualityFindingState::Fixed as i32;
            rec.finding = Some(f.clone());
            tx.update(&QUALITY, &row.clone().k2("fixed").msg(&rec))
                .await?;
            record(
                tx,
                Change {
                    repo,
                    event: "code_quality_finding.updated",
                    action: "security.code_quality.fix",
                    subject: SubjectKind::SecurityAlert,
                    id: &f.number.to_string(),
                    payload: serde_json::to_value(&f).unwrap_or_default(),
                    before: String::new(),
                    after: serde_json::json!({"number": f.number, "state": "fixed"}).to_string(),
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Publishes the check run of one analysis into the checks module.
    async fn publish_check(
        &self,
        repo: &Repo,
        upload: &CodeScanningSarifUpload,
        run: &Run,
        analysis_id: &str,
        rec: &SarifRec,
    ) -> Result<()> {
        let name = if run.category.is_empty() {
            format!("code-scanning/{}", run.tool)
        } else {
            format!("code-scanning/{}/{}", run.tool, run.category)
        };
        let errors = run
            .items
            .iter()
            .filter(|i| {
                i.level == "error" || i.rule.security_severity >= SecuritySeverity::High as i32
            })
            .count();
        let status = if run.items.is_empty() {
            CheckStatus::Passed
        } else if errors > 0 {
            CheckStatus::Failed
        } else {
            CheckStatus::Neutral
        };
        let total = run.items.len();
        let annotations: Vec<CheckAnnotation> = run
            .items
            .iter()
            .take(1000)
            .map(|i| CheckAnnotation {
                path: i.path.as_bytes().to_vec(),
                start_line: i.start_line,
                end_line: i.end_line,
                start_column: i.start_column,
                end_column: i.end_column,
                level: match i.level.as_str() {
                    "error" => AnnotationLevel::Failure,
                    "warning" => AnnotationLevel::Warning,
                    _ => AnnotationLevel::Notice,
                } as i32,
                message: i.message.clone(),
                title: i.rule.id.clone(),
                raw_details: String::new(),
            })
            .collect();
        let mut summary = format!(
            "{total} result(s) from {} {} on {} ({errors} blocking).",
            run.tool, run.version, upload.r#ref
        );
        if total > 1000 {
            summary.push_str(" Only the first 1000 are annotated.");
        }
        let mut sys = Cx::system();
        sys.decision = "security:code-scanning".into();
        sys.actor_id = if rec.uploader.is_empty() {
            sys.actor_id
        } else {
            rec.uploader.clone()
        };
        sys.chain = vec![sys.actor_id.clone()];
        self.checks
            .report(
                &sys,
                &repo.org_id,
                &repo.id,
                Report {
                    changeset: upload.changeset.clone(),
                    name,
                    status,
                    summary: summary.clone(),
                    external_id: analysis_id.to_string(),
                    output: Some(CheckOutput {
                        title: format!("{} found {total} result(s)", run.tool),
                        summary,
                        annotations,
                        ..Default::default()
                    }),
                    ..Report::default()
                },
            )
            .await?;
        Ok(())
    }

    // ---- alerts ---------------------------------------------------------------------------

    pub(crate) async fn code_alert_recs(
        &self,
        cx: &Cx,
        repo: &Repo,
    ) -> Result<Vec<(CodeAlertRec, Row)>> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let mut rows = tx
            .scan_msgs::<CodeAlertRec>(&CODE_ALERTS, &Scan::scope(&repo.id, crate::store::MAX_SCAN))
            .await?;
        rows.reverse();
        Ok(rows)
    }

    pub(crate) async fn has_analyses(&self, cx: &Cx, repo: &Repo) -> Result<bool> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        Ok(tx.count(&ANALYSES, &repo.id, None).await? > 0)
    }

    async fn instances_of(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
    ) -> Result<Vec<CodeScanningAlertInstance>> {
        let scope = format!("{}:{}", repo.id, num_id(number));
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        Ok(tx
            .scan_msgs::<InstanceRec>(&CODE_INSTANCES, &Scan::scope(&scope, 1000))
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.instance)
            .collect())
    }

    /// Open code-scanning alerts on the code being landed, for the code-scanning land rule
    /// (F49-008): alerts with an open instance analysed at one of `heads` (raw changeset
    /// ids: the change request's head and the landed head) or on one of `refs` (the target
    /// ref, so alerts already open there keep blocking until fixed or dismissed).
    pub async fn open_alerts_for_land(
        &self,
        cx: &Cx,
        repo: &Repo,
        heads: &[Vec<u8>],
        refs: &[String],
    ) -> Result<Vec<bee_policy::CodeAlert>> {
        let mut out = Vec::new();
        for (rec, _) in self.code_alert_recs(cx, repo).await? {
            let Some(a) = rec.alert else { continue };
            if a.state != CodeScanningAlertState::Open as i32 {
                continue;
            }
            let relevant = self
                .instances_of(cx, repo, a.number)
                .await?
                .iter()
                .any(|i| {
                    i.state == CodeScanningAlertState::Open as i32
                        && (i.changeset.as_ref().is_some_and(|c| heads.contains(&c.raw))
                            || refs.contains(&i.r#ref))
                });
            if !relevant {
                continue;
            }
            let rule = a.rule.clone().unwrap_or_default();
            let level = match rule.severity.to_ascii_lowercase().as_str() {
                "error" => bee_policy::AlertLevel::Error,
                "warning" => bee_policy::AlertLevel::Warning,
                _ => bee_policy::AlertLevel::Note,
            };
            let security = match SecuritySeverity::try_from(rule.security_severity) {
                Ok(SecuritySeverity::Low) => Some(bee_policy::SecuritySeverity::Low),
                Ok(SecuritySeverity::Medium) => Some(bee_policy::SecuritySeverity::Medium),
                Ok(SecuritySeverity::High) => Some(bee_policy::SecuritySeverity::High),
                Ok(SecuritySeverity::Critical) => Some(bee_policy::SecuritySeverity::Critical),
                _ => None,
            };
            out.push(bee_policy::CodeAlert {
                tool: a.tool_name.clone(),
                number: a.number,
                level,
                security,
            });
        }
        out.sort_by_key(|a| a.number);
        Ok(out)
    }

    /// `ListCodeScanningAlerts` (empty `states` = open).
    pub async fn list_code_alerts(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &ListCodeScanningAlertsRequest,
        token: &str,
        size: usize,
    ) -> Result<(Vec<CodeScanningAlert>, String)> {
        let states: Vec<i32> = if req.states.is_empty() {
            vec![CodeScanningAlertState::Open as i32]
        } else {
            req.states.clone()
        };
        let mut out = Vec::new();
        for (rec, _) in self.code_alert_recs(cx, repo).await? {
            let Some(mut a) = rec.alert else { continue };
            if !states.contains(&a.state)
                || (!req.tool_name.is_empty() && a.tool_name != req.tool_name)
            {
                continue;
            }
            let sev = a.rule.as_ref().map(|r| r.security_severity).unwrap_or(0);
            if !req.severities.is_empty() && !req.severities.contains(&sev) {
                continue;
            }
            if !req.r#ref.is_empty() {
                let insts = self.instances_of(cx, repo, a.number).await?;
                match insts.into_iter().find(|i| i.r#ref == req.r#ref) {
                    Some(i) => a.most_recent_instance = Some(i),
                    None => continue,
                }
            }
            if !req.path_prefix.is_empty()
                && !a
                    .most_recent_instance
                    .as_ref()
                    .and_then(|i| i.location.as_ref())
                    .is_some_and(|l| l.path.starts_with(&req.path_prefix))
            {
                continue;
            }
            out.push(a);
        }
        page_slice(&out, token, size)
    }

    /// `GetCodeScanningAlert`.
    pub async fn get_code_alert(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
    ) -> Result<CodeScanningAlert> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.get_msg::<CodeAlertRec>(&CODE_ALERTS, &repo.id, &num_id(number))
            .await?
            .and_then(|(r, _)| r.alert)
            .ok_or_else(|| ForgeError::not_found(format!("code scanning alert #{number}")))
    }

    /// `UpdateCodeScanningAlert`: dismiss (a reason is required) or reopen; a fixed alert
    /// cannot change.
    pub async fn update_code_alert(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &UpdateCodeScanningAlertRequest,
    ) -> Result<CodeScanningAlert> {
        let target = CodeScanningAlertState::try_from(req.state)
            .unwrap_or(CodeScanningAlertState::Unspecified);
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<CodeAlertRec>(&CODE_ALERTS, &repo.id, &num_id(req.number))
            .await?
        else {
            return Err(ForgeError::not_found(format!(
                "code scanning alert #{}",
                req.number
            )));
        };
        let mut a = rec.alert.clone().unwrap_or_default();
        if alert_state(&a) == CodeScanningAlertState::Fixed {
            return Err(ForgeError::invalid_state("a fixed alert cannot be changed"));
        }
        let before = alert_json(&a);
        let (event, action) = match target {
            CodeScanningAlertState::Dismissed => {
                if CodeScanningDismissReason::try_from(req.dismissed_reason)
                    .unwrap_or(CodeScanningDismissReason::Unspecified)
                    == CodeScanningDismissReason::Unspecified
                {
                    return Err(ForgeError::invalid(
                        "missing_reason",
                        "dismissing an alert needs a dismissed_reason",
                    ));
                }
                a.state = CodeScanningAlertState::Dismissed as i32;
                a.dismissed_reason = req.dismissed_reason;
                a.dismissed_comment = req.dismissed_comment.clone();
                a.dismissed_by = Some(cx.principal());
                a.dismissed_at = Some(ts(now));
                (
                    "code_scanning_alert.closed_by_user",
                    "security.code_alert.dismiss",
                )
            }
            CodeScanningAlertState::Open => {
                a.state = CodeScanningAlertState::Open as i32;
                a.dismissed_reason = 0;
                a.dismissed_comment.clear();
                a.dismissed_by = None;
                a.dismissed_at = None;
                ("code_scanning_alert.reopened", "security.code_alert.reopen")
            }
            _ => {
                return Err(ForgeError::invalid(
                    "invalid_state",
                    "state must be OPEN or DISMISSED",
                ));
            }
        };
        a.updated_at = Some(ts(now));
        rec.alert = Some(a.clone());
        tx.update(
            &CODE_ALERTS,
            &row.clone()
                .k2(state_label(alert_state(&a)))
                .n1(now as i64)
                .msg(&rec),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event,
                action,
                subject: SubjectKind::SecurityAlert,
                id: &a.number.to_string(),
                payload: serde_json::to_value(&a).unwrap_or_default(),
                before,
                after: alert_json(&a),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(a)
    }

    /// `ListCodeScanningAlertInstances`.
    pub async fn list_code_instances(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
        ref_name: &str,
        token: &str,
        size: usize,
    ) -> Result<(Vec<CodeScanningAlertInstance>, String)> {
        self.get_code_alert(cx, repo, number).await?;
        let insts: Vec<CodeScanningAlertInstance> = self
            .instances_of(cx, repo, number)
            .await?
            .into_iter()
            .filter(|i| ref_name.is_empty() || i.r#ref == ref_name)
            .collect();
        page_slice(&insts, token, size)
    }

    /// `ListCodeScanningAnalyses`, newest first.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_analyses(
        &self,
        cx: &Cx,
        repo: &Repo,
        ref_name: &str,
        tool: &str,
        changeset: Option<&ObjectId>,
        token: &str,
        size: usize,
    ) -> Result<(Vec<CodeScanningAnalysis>, String)> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let rows = tx
            .scan_msgs::<CodeScanningAnalysis>(
                &ANALYSES,
                &Scan::scope(&repo.id, crate::store::MAX_SCAN).by_n1().desc(),
            )
            .await?;
        let all: Vec<CodeScanningAnalysis> = rows
            .into_iter()
            .map(|(a, _)| a)
            .filter(|a| ref_name.is_empty() || a.r#ref == ref_name)
            .filter(|a| tool.is_empty() || a.tool_name == tool)
            .filter(|a| changeset.is_none_or(|c| a.changeset.as_ref() == Some(c)))
            .collect();
        page_slice(&all, token, size)
    }

    /// `GetCodeScanningAnalysis`.
    pub async fn get_analysis(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
    ) -> Result<CodeScanningAnalysis> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.get_msg::<CodeScanningAnalysis>(&ANALYSES, &repo.id, id)
            .await?
            .map(|(a, _)| a)
            .ok_or_else(|| ForgeError::not_found(format!("analysis {id}")))
    }

    // ---- default setup and autofix -----------------------------------------------------------

    /// `GetCodeScanningDefaultSetup`.
    pub async fn get_default_setup(
        &self,
        cx: &Cx,
        repo: &Repo,
    ) -> Result<CodeScanningDefaultSetup> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        Ok(tx
            .get_msg::<CodeScanningDefaultSetup>(&CONFIG, &repo.id, "code_setup")
            .await?
            .map(|(s, _)| s)
            .unwrap_or_else(|| CodeScanningDefaultSetup {
                repo: repo_ref_of(repo),
                state: CodeScanningSetupState::NotConfigured as i32,
                languages: Vec::new(),
                query_suite: "default".into(),
                schedule: "weekly".into(),
                updated_at: None,
            }))
    }

    /// `UpdateCodeScanningDefaultSetup`.
    pub async fn update_default_setup(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &UpdateCodeScanningDefaultSetupRequest,
    ) -> Result<CodeScanningDefaultSetup> {
        let mut s = self.get_default_setup(cx, repo).await?;
        let before = serde_json::to_string(&s).unwrap_or_default();
        if let Some(st) = req.state {
            let st = CodeScanningSetupState::try_from(st)
                .map_err(|_| ForgeError::invalid("invalid_setting", "unknown setup state"))?;
            if st == CodeScanningSetupState::Unspecified {
                return Err(ForgeError::invalid("invalid_setting", "state is required"));
            }
            s.state = st as i32;
        }
        if !req.languages.is_empty() {
            let mut l = req.languages.clone();
            l.sort();
            l.dedup();
            s.languages = l;
        }
        if let Some(q) = &req.query_suite {
            if !matches!(q.as_str(), "default" | "extended") {
                return Err(ForgeError::invalid(
                    "invalid_setting",
                    "query_suite is default or extended",
                ));
            }
            s.query_suite = q.clone();
        }
        if let Some(sc) = &req.schedule {
            if !super::updates::valid_schedule(sc) {
                return Err(ForgeError::invalid(
                    "invalid_setting",
                    "schedule is daily, weekly, monthly or a cron expression",
                ));
            }
            s.schedule = sc.clone();
        }
        if s.state == CodeScanningSetupState::Configured as i32 && s.languages.is_empty() {
            return Err(ForgeError::invalid(
                "invalid_setting",
                "a configured setup needs languages",
            ));
        }
        s.repo = repo_ref_of(repo);
        s.updated_at = Some(ts(self.now()));
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.put(
            &CONFIG,
            &Row::new(&repo.id, "code_setup").k1("code_setup").msg(&s),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "code_scanning.setup_updated",
                action: "security.code_scanning.setup",
                subject: SubjectKind::Repo,
                id: &repo.id,
                payload: serde_json::to_value(&s).unwrap_or_default(),
                before,
                after: serde_json::to_string(&s).unwrap_or_default(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    /// `RequestCodeScanningAutofix`: asks the autofix runner for an agent run on an open alert.
    pub async fn request_autofix(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
        runtime: &str,
    ) -> Result<String> {
        let a = self.get_code_alert(cx, repo, number).await?;
        if alert_state(&a) != CodeScanningAlertState::Open {
            return Err(ForgeError::invalid_state(
                "only an open alert can be autofixed",
            ));
        }
        let rule = a.rule.clone().unwrap_or_default();
        let run_id = self
            .ports
            .autofix
            .start(repo, number, &rule.id, runtime)
            .await?;
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "code_scanning.autofix_requested",
                action: "security.code_alert.autofix",
                subject: SubjectKind::SecurityAlert,
                id: &number.to_string(),
                payload: serde_json::json!({"alert": number, "agent_run_id": run_id}),
                before: String::new(),
                after: serde_json::json!({"alert": number, "agent_run_id": run_id}).to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(run_id)
    }

    /// Records the change request an autofix run opened (the agents module calls this).
    pub async fn link_autofix_change(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
        cr_number: u64,
    ) -> Result<()> {
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<CodeAlertRec>(&CODE_ALERTS, &repo.id, &num_id(number))
            .await?
        else {
            return Err(ForgeError::not_found(format!(
                "code scanning alert #{number}"
            )));
        };
        let mut a = rec.alert.clone().unwrap_or_default();
        a.fix_change_request = cr_number.to_string();
        a.updated_at = Some(ts(now));
        rec.alert = Some(a.clone());
        tx.update(&CODE_ALERTS, &row.clone().n1(now as i64).msg(&rec))
            .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "code_scanning_alert.reopened",
                action: "security.code_alert.link_fix",
                subject: SubjectKind::SecurityAlert,
                id: &number.to_string(),
                payload: serde_json::to_value(&a).unwrap_or_default(),
                before: String::new(),
                after: alert_json(&a),
            },
        )
        .await?;
        tx.commit().await
    }

    // ---- code quality ----------------------------------------------------------------------

    /// `ListCodeQualityFindings`.
    pub async fn list_quality(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &ListCodeQualityFindingsRequest,
        token: &str,
        size: usize,
    ) -> Result<(Vec<SecurityCodeQualityFinding>, String)> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let mut rows = tx
            .scan_msgs::<QualityRec>(&QUALITY, &Scan::scope(&repo.id, crate::store::MAX_SCAN))
            .await?;
        rows.reverse();
        let all: Vec<SecurityCodeQualityFinding> = rows
            .into_iter()
            .filter_map(|(r, _)| r.finding)
            .filter(|f| req.r#ref.is_empty() || f.r#ref == req.r#ref)
            .filter(|f| req.state == 0 || f.state == req.state)
            .filter(|f| req.category.is_empty() || f.category == req.category)
            .filter(|f| {
                req.path_prefix.is_empty()
                    || f.location
                        .as_ref()
                        .is_some_and(|l| l.path.starts_with(&req.path_prefix))
            })
            .collect();
        page_slice(&all, token, size)
    }

    /// `UpdateCodeQualityFinding`.
    pub async fn update_quality(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &UpdateCodeQualityFindingRequest,
    ) -> Result<SecurityCodeQualityFinding> {
        let target = CodeQualityFindingState::try_from(req.state)
            .unwrap_or(CodeQualityFindingState::Unspecified);
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<QualityRec>(&QUALITY, &repo.id, &num_id(req.number))
            .await?
        else {
            return Err(ForgeError::not_found(format!(
                "code quality finding #{}",
                req.number
            )));
        };
        let mut f = rec.finding.clone().unwrap_or_default();
        if f.state == CodeQualityFindingState::Fixed as i32 {
            return Err(ForgeError::invalid_state(
                "a fixed finding cannot be changed",
            ));
        }
        let was = f.state;
        f.state = match target {
            CodeQualityFindingState::Open => CodeQualityFindingState::Open,
            CodeQualityFindingState::Dismissed => CodeQualityFindingState::Dismissed,
            _ => {
                return Err(ForgeError::invalid(
                    "invalid_state",
                    "state must be OPEN or DISMISSED",
                ));
            }
        } as i32;
        rec.finding = Some(f.clone());
        let label = if target == CodeQualityFindingState::Open {
            "open"
        } else {
            "dismissed"
        };
        tx.update(&QUALITY, &row.clone().k2(label).msg(&rec))
            .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "code_quality_finding.updated",
                action: "security.code_quality.update",
                subject: SubjectKind::SecurityAlert,
                id: &f.number.to_string(),
                payload: serde_json::to_value(&f).unwrap_or_default(),
                before: serde_json::json!({"state": was}).to_string(),
                after: serde_json::json!({"state": label, "comment": req.comment}).to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(f)
    }
}
