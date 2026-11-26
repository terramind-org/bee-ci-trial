//! Secret scanning, custom patterns, push protection and delegated bypasses.
//!
//! Detection is `bee-secrets` (built-in rules) plus the repo's and org's published custom
//! patterns. A secret's value is never stored: alerts carry a redacted preview and a keyed hash
//! (BLAKE3 keyed by the repo id) that identifies the same secret across locations. Validity
//! checks hand the value to the [`super::ports::SecretVerifier`] port in memory only.

use std::collections::BTreeMap;

use bee_proto::v1::error_detail::Payload;
use bee_proto::v1::{
    CreateSecretScanningPatternRequest, ErrorReason, ListSecretScanningAlertsRequest, ObjectId,
    PushProtectionBlock, PushProtectionBypass, PushProtectionBypassReason,
    PushProtectionBypassState, Repo, RequestPushProtectionBypassRequest,
    ReviewPushProtectionBypassRequest, SecretFinding, SecretScanningAlert,
    SecretScanningAlertState, SecretScanningLocation, SecretScanningPattern,
    SecretScanningResolution, SecretScanningValidity, SecuritySeverity, SubjectKind, SubjectRef,
    UpdateSecretScanningAlertRequest, UpdateSecretScanningPatternRequest,
};
use bee_secrets::Confidence;
use regex::bytes::{Regex, RegexBuilder};

use super::model::{BlockRec, BypassPolicyRec, BypassRec, FlagRec, SecretAlertRec};
use super::{
    BLOCKS, BYPASSES, CONFIG, Change, PATTERNS, SECRET_ALERTS, Security, num_id, page_slice,
    record, record_id, record_org, repo_ref_of,
};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::store::{Key, Row, Scan, Surface};

/// Most custom patterns per repo or org.
pub const MAX_PATTERNS: usize = 100;
/// How long a block can be turned into a bypass request.
pub const BLOCK_TTL_MS: u64 = 24 * 3600 * 1000;
/// How long a pending bypass request waits for a reviewer.
pub const PENDING_TTL_MS: u64 = 7 * 24 * 3600 * 1000;
/// Default hours an approved bypass stays usable.
pub const DEFAULT_BYPASS_TTL_HOURS: u32 = 24;
/// Most blobs of history one scan reads.
pub const MAX_SCAN_CHANGESETS: usize = 5000;

/// Who owns a custom pattern.
#[derive(Clone, Copy)]
pub enum PatternOwner<'a> {
    /// An org (id, name).
    Org(&'a str, &'a str),
    /// A repo.
    Repo(&'a Repo),
}

/// One detected secret with what alerting needs. The secret itself is kept only in memory.
#[derive(Clone)]
pub(crate) struct Hit {
    pub path: String,
    pub line: u32,
    pub rule_id: String,
    pub secret_type: String,
    pub display: String,
    pub pattern_id: String,
    pub preview: String,
    pub hash: String,
    pub blocking: bool,
    pub severity: SecuritySeverity,
    pub secret: Vec<u8>,
    pub blob: Option<ObjectId>,
}

impl Hit {
    fn key(&self) -> String {
        format!("{}:{}", self.secret_type, self.hash)
    }

    fn finding(&self) -> SecretFinding {
        SecretFinding {
            path: self.path.as_bytes().to_vec(),
            blob: self.blob.clone(),
            line: self.line,
            secret_type: self.secret_type.clone(),
            pattern_id: if self.pattern_id.is_empty() {
                self.rule_id.clone()
            } else {
                self.pattern_id.clone()
            },
        }
    }
}

/// The result of a push-protection check that let the write through.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushOutcome {
    /// The bypass that was exercised, if any.
    pub bypass_id: Option<String>,
    /// Alert numbers opened for the bypassed secrets.
    pub alerts: Vec<u64>,
    /// Non-blocking findings (low confidence) that were let through.
    pub advisory_findings: usize,
}

struct Custom {
    id: String,
    name: String,
    regex: Regex,
    must: Vec<Regex>,
    must_not: Vec<Regex>,
    block: bool,
}

fn compile(src: &str, what: &str) -> Result<Regex> {
    RegexBuilder::new(src)
        .unicode(false)
        .size_limit(8 << 20)
        .build()
        .map_err(|e| ForgeError::invalid("invalid_regex", format!("{what} does not compile: {e}")))
}

fn composed(p: &SecretScanningPattern) -> String {
    let mut s = String::new();
    if !p.before_regex.is_empty() {
        s.push_str(&format!("(?:{})", p.before_regex));
    }
    s.push_str(&format!("(?P<secret>{})", p.regex));
    if !p.after_regex.is_empty() {
        s.push_str(&format!("(?:{})", p.after_regex));
    }
    s
}

fn validate_pattern(p: &SecretScanningPattern) -> Result<Custom> {
    if p.name.trim().is_empty() {
        return Err(ForgeError::missing("name"));
    }
    if p.name.len() > 100 {
        return Err(ForgeError::invalid(
            "invalid_name",
            "name is longer than 100 bytes",
        ));
    }
    if p.regex.is_empty() {
        return Err(ForgeError::missing("regex"));
    }
    compile(&p.regex, "regex")?;
    if !p.before_regex.is_empty() {
        compile(&p.before_regex, "before_regex")?;
    }
    if !p.after_regex.is_empty() {
        compile(&p.after_regex, "after_regex")?;
    }
    let regex = compile(&composed(p), "pattern")?;
    let must = p
        .must_match
        .iter()
        .map(|m| compile(m, "must_match"))
        .collect::<Result<Vec<_>>>()?;
    let must_not = p
        .must_not_match
        .iter()
        .map(|m| compile(m, "must_not_match"))
        .collect::<Result<Vec<_>>>()?;
    Ok(Custom {
        id: p.id.clone(),
        name: p.name.clone(),
        regex,
        must,
        must_not,
        block: p.push_protection,
    })
}

fn snake(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.trim_end_matches('_').to_string()
}

fn redact(secret: &[u8]) -> String {
    let keep = (secret.len() / 4).min(4);
    format!("{}****", String::from_utf8_lossy(&secret[..keep]))
}

fn line_of(bytes: &[u8], offset: usize) -> u32 {
    1 + bytes[..offset.min(bytes.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count() as u32
}

fn keyed_hash(repo_id: &str, secret: &[u8]) -> String {
    let key = blake3::hash(repo_id.as_bytes());
    crate::ids::hex(blake3::keyed_hash(key.as_bytes(), secret).as_bytes())
}

fn alert_state(a: &SecretScanningAlert) -> SecretScanningAlertState {
    SecretScanningAlertState::try_from(a.state).unwrap_or(SecretScanningAlertState::Unspecified)
}

fn alert_json(a: &SecretScanningAlert) -> String {
    serde_json::json!({
        "number": a.number,
        "state": alert_state(a).as_str_name(),
        "secret_type": a.secret_type,
        "resolution": SecretScanningResolution::try_from(a.resolution)
            .map(|r| r.as_str_name()).unwrap_or(""),
        "push_protection_bypassed": a.push_protection_bypassed,
        "validity": SecretScanningValidity::try_from(a.validity)
            .map(|r| r.as_str_name()).unwrap_or(""),
    })
    .to_string()
}

fn bypass_state(b: &PushProtectionBypass) -> PushProtectionBypassState {
    PushProtectionBypassState::try_from(b.state).unwrap_or(PushProtectionBypassState::Unspecified)
}

fn bypass_json(b: &PushProtectionBypass) -> String {
    serde_json::json!({
        "id": b.id,
        "block_id": b.block_id,
        "state": bypass_state(b).as_str_name(),
        "reason": PushProtectionBypassReason::try_from(b.reason)
            .map(|r| r.as_str_name()).unwrap_or(""),
        "findings": b.findings.iter().map(|f| serde_json::json!({
            "path": String::from_utf8_lossy(&f.path), "line": f.line,
            "secret_type": f.secret_type,
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

fn state_key(s: PushProtectionBypassState) -> &'static str {
    match s {
        PushProtectionBypassState::Pending => "pending",
        PushProtectionBypassState::Approved => "approved",
        PushProtectionBypassState::Denied => "denied",
        PushProtectionBypassState::Expired => "expired",
        PushProtectionBypassState::Granted => "granted",
        PushProtectionBypassState::Unspecified => "unspecified",
    }
}

impl Security {
    // ---- custom patterns ----------------------------------------------------------------

    async fn pattern_count(&self, cx: &Cx, scope: &str) -> Result<usize> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        Ok(tx.count(&PATTERNS, scope, None).await? as usize)
    }

    /// `CreateSecretScanningPattern`.
    pub async fn create_pattern(
        &self,
        cx: &Cx,
        owner: PatternOwner<'_>,
        req: &CreateSecretScanningPatternRequest,
    ) -> Result<SecretScanningPattern> {
        let (scope, org_name) = match owner {
            PatternOwner::Org(id, name) => (id.to_string(), name.to_string()),
            PatternOwner::Repo(r) => (r.id.clone(), String::new()),
        };
        let now = self.now();
        let mut p = SecretScanningPattern {
            id: String::new(),
            org: org_name,
            repo: match owner {
                PatternOwner::Repo(r) => repo_ref_of(r),
                PatternOwner::Org(..) => None,
            },
            name: req.name.trim().to_string(),
            regex: req.regex.clone(),
            before_regex: req.before_regex.clone(),
            after_regex: req.after_regex.clone(),
            must_match: req.must_match.clone(),
            must_not_match: req.must_not_match.clone(),
            push_protection: req.push_protection,
            published: req.published,
            created_by: Some(cx.principal()),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
        };
        validate_pattern(&p)?;
        if self.pattern_count(cx, &scope).await? >= MAX_PATTERNS {
            return Err(ForgeError::too_large(
                "custom_patterns",
                MAX_PATTERNS as u64,
                MAX_PATTERNS as u64 + 1,
            ));
        }
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        p.id = tx.new_id()?;
        let lname = p.name.to_lowercase();
        let dup = tx
            .scan_msgs::<SecretScanningPattern>(
                &PATTERNS,
                &Scan::scope(&scope, 1).key(Key::K1(lname.clone())),
            )
            .await?;
        if !dup.is_empty() {
            return Err(ForgeError::already_exists(format!("pattern {}", p.name)));
        }
        tx.insert(
            &PATTERNS,
            &Row::new(&scope, &p.id)
                .k1(&lname)
                .k2(&p.id)
                .n1(now as i64)
                .msg(&p),
        )
        .await?;
        match owner {
            PatternOwner::Repo(r) => {
                record(
                    &mut tx,
                    Change {
                        repo: r,
                        event: "secret_scanning_pattern.created",
                        action: "security.secret_pattern.create",
                        subject: SubjectKind::Repo,
                        id: &p.id,
                        payload: serde_json::to_value(&p).unwrap_or_default(),
                        before: String::new(),
                        after: serde_json::json!({"id": p.id, "name": p.name}).to_string(),
                    },
                )
                .await?
            }
            PatternOwner::Org(id, _) => {
                record_org(
                    &mut tx,
                    id,
                    "secret_scanning_pattern.created",
                    "security.secret_pattern.create",
                    SubjectKind::Org,
                    &p.id,
                    serde_json::to_value(&p).unwrap_or_default(),
                    serde_json::json!({"id": p.id, "name": p.name}).to_string(),
                )
                .await?
            }
        }
        tx.commit().await?;
        if p.published {
            self.rescan_for_pattern(cx, owner).await;
        }
        Ok(p)
    }

    /// A published pattern changes what history contains: scan it again (best effort; a repo
    /// whose content is unavailable is skipped).
    async fn rescan_for_pattern(&self, cx: &Cx, owner: PatternOwner<'_>) {
        let repos: Vec<Repo> = match owner {
            PatternOwner::Repo(r) => vec![r.clone()],
            PatternOwner::Org(id, _) => self.org_repos(cx, id).await.unwrap_or_default(),
        };
        for r in repos {
            if let Ok(tip) = self.ports.vcs.resolve(&r, "").await {
                let _ = self
                    .scan_history(&Cx::system(), &r, &tip, MAX_SCAN_CHANGESETS)
                    .await;
            }
        }
    }

    /// The pattern with `id` and its scope (repo or org id).
    pub async fn pattern_by_id(
        &self,
        cx: &Cx,
        id: &str,
    ) -> Result<(SecretScanningPattern, String)> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let rows = tx
            .scan_msgs::<SecretScanningPattern>(
                &PATTERNS,
                &Scan {
                    key: Some(Key::K2(id.to_string())),
                    limit: 1,
                    ..Scan::default()
                },
            )
            .await?;
        rows.into_iter()
            .next()
            .map(|(p, row)| (p, row.scope))
            .ok_or_else(|| ForgeError::not_found(format!("pattern {id}")))
    }

    /// `ListSecretScanningPatterns`.
    pub async fn list_patterns(
        &self,
        cx: &Cx,
        scope: &str,
        token: &str,
        size: usize,
    ) -> Result<(Vec<SecretScanningPattern>, String)> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let rows = tx
            .scan_msgs::<SecretScanningPattern>(
                &PATTERNS,
                &Scan::scope(scope, crate::store::MAX_SCAN).by_n1(),
            )
            .await?;
        let all: Vec<SecretScanningPattern> = rows.into_iter().map(|(p, _)| p).collect();
        page_slice(&all, token, size)
    }

    /// `UpdateSecretScanningPattern`: alerts the edited pattern no longer matches are closed.
    pub async fn update_pattern(
        &self,
        cx: &Cx,
        req: &UpdateSecretScanningPatternRequest,
    ) -> Result<SecretScanningPattern> {
        let (mut p, scope) = self.pattern_by_id(cx, &req.id).await?;
        let before = serde_json::json!({"regex": p.regex, "name": p.name}).to_string();
        let regex_changed =
            req.regex.is_some() || req.before_regex.is_some() || req.after_regex.is_some();
        if let Some(n) = &req.name {
            p.name = n.trim().to_string();
        }
        if let Some(r) = &req.regex {
            p.regex = r.clone();
        }
        if let Some(r) = &req.before_regex {
            p.before_regex = r.clone();
        }
        if let Some(r) = &req.after_regex {
            p.after_regex = r.clone();
        }
        if let Some(b) = req.push_protection {
            p.push_protection = b;
        }
        if let Some(b) = req.published {
            p.published = b;
        }
        validate_pattern(&p)?;
        let now = self.now();
        p.updated_at = Some(ts(now));
        let owner_repo = self.pattern_repo(cx, &p, &scope).await?;
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((_, row)) = tx
            .get_msg::<SecretScanningPattern>(&PATTERNS, &scope, &req.id)
            .await?
        else {
            return Err(ForgeError::not_found(format!("pattern {}", req.id)));
        };
        let lname = p.name.to_lowercase();
        if lname != row.k1.clone().unwrap_or_default() {
            let dup = tx
                .scan_msgs::<SecretScanningPattern>(
                    &PATTERNS,
                    &Scan::scope(&scope, 1).key(Key::K1(lname.clone())),
                )
                .await?;
            if !dup.is_empty() {
                return Err(ForgeError::already_exists(format!("pattern {}", p.name)));
            }
        }
        tx.update(&PATTERNS, &row.clone().k1(&lname).msg(&p))
            .await?;
        let after = serde_json::json!({"regex": p.regex, "name": p.name}).to_string();
        match &owner_repo {
            Some(r) => {
                record(
                    &mut tx,
                    Change {
                        repo: r,
                        event: "secret_scanning_pattern.updated",
                        action: "security.secret_pattern.update",
                        subject: SubjectKind::Repo,
                        id: &p.id,
                        payload: serde_json::to_value(&p).unwrap_or_default(),
                        before,
                        after,
                    },
                )
                .await?
            }
            None => {
                record_org(
                    &mut tx,
                    &scope,
                    "secret_scanning_pattern.updated",
                    "security.secret_pattern.update",
                    SubjectKind::Org,
                    &p.id,
                    serde_json::to_value(&p).unwrap_or_default(),
                    after,
                )
                .await?
            }
        }
        tx.commit().await?;
        if regex_changed || req.published.is_some() {
            self.recheck_pattern_alerts(cx, &p, &scope).await?;
            if p.published {
                match &owner_repo {
                    Some(r) => self.rescan_for_pattern(cx, PatternOwner::Repo(r)).await,
                    None => {
                        self.rescan_for_pattern(cx, PatternOwner::Org(&scope, &p.org))
                            .await
                    }
                }
            }
        }
        Ok(p)
    }

    async fn pattern_repo(
        &self,
        cx: &Cx,
        p: &SecretScanningPattern,
        scope: &str,
    ) -> Result<Option<Repo>> {
        if p.repo.is_some() {
            Ok(Some(self.orgs.repo_any(cx, scope).await?))
        } else {
            Ok(None)
        }
    }

    /// The repos a pattern applies to.
    async fn pattern_repos(
        &self,
        cx: &Cx,
        p: &SecretScanningPattern,
        scope: &str,
    ) -> Result<Vec<Repo>> {
        match self.pattern_repo(cx, p, scope).await? {
            Some(r) => Ok(vec![r]),
            None => self.org_repos(cx, scope).await,
        }
    }

    /// After an edit: open alerts of the pattern whose locations no longer match close as
    /// `PATTERN_EDITED`.
    async fn recheck_pattern_alerts(
        &self,
        cx: &Cx,
        p: &SecretScanningPattern,
        scope: &str,
    ) -> Result<()> {
        let custom = if p.published {
            Some(validate_pattern(p)?)
        } else {
            None
        };
        for repo in self.pattern_repos(cx, p, scope).await? {
            let recs = self.secret_alert_recs(cx, &repo).await?;
            for (rec, _) in recs {
                let Some(alert) = rec.alert.clone() else {
                    continue;
                };
                if alert.pattern_id != p.id || alert_state(&alert) != SecretScanningAlertState::Open
                {
                    continue;
                }
                let mut still = false;
                if let Some(c) = &custom {
                    for loc in &rec.locations {
                        let Some(f) = &loc.location else { continue };
                        let bee_proto::v1::secret_scanning_location::Location::Blob(b) = f else {
                            continue;
                        };
                        let Some(cs) = &loc.changeset else { continue };
                        let path = String::from_utf8_lossy(&b.path).into_owned();
                        let Ok(Some(bytes)) = self.ports.vcs.read_file(&repo, cs, &path).await
                        else {
                            continue;
                        };
                        if scan_custom(&[c], &path, &bytes, &repo.id)
                            .iter()
                            .any(|h| h.hash == rec.secret_hash)
                        {
                            still = true;
                            break;
                        }
                    }
                }
                if !still {
                    self.resolve_alert_system(
                        cx,
                        &repo,
                        alert.number,
                        SecretScanningResolution::PatternEdited,
                        "the pattern was edited and no longer matches",
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    /// `DeleteSecretScanningPattern`: its open alerts resolve as `PATTERN_DELETED`.
    pub async fn delete_pattern(&self, cx: &Cx, id: &str) -> Result<()> {
        let (p, scope) = self.pattern_by_id(cx, id).await?;
        let owner_repo = self.pattern_repo(cx, &p, &scope).await?;
        for repo in self.pattern_repos(cx, &p, &scope).await? {
            for (rec, _) in self.secret_alert_recs(cx, &repo).await? {
                if let Some(a) = &rec.alert
                    && a.pattern_id == id
                    && alert_state(a) == SecretScanningAlertState::Open
                {
                    self.resolve_alert_system(
                        cx,
                        &repo,
                        a.number,
                        SecretScanningResolution::PatternDeleted,
                        "the pattern was deleted",
                    )
                    .await?;
                }
            }
        }
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.delete(&PATTERNS, &scope, id).await?;
        let after = serde_json::json!({"id": id, "name": p.name}).to_string();
        match &owner_repo {
            Some(r) => {
                record(
                    &mut tx,
                    Change {
                        repo: r,
                        event: "secret_scanning_pattern.deleted",
                        action: "security.secret_pattern.delete",
                        subject: SubjectKind::Repo,
                        id,
                        payload: serde_json::json!({"id": id, "name": p.name}),
                        before: String::new(),
                        after,
                    },
                )
                .await?
            }
            None => {
                record_org(
                    &mut tx,
                    &scope,
                    "secret_scanning_pattern.deleted",
                    "security.secret_pattern.delete",
                    SubjectKind::Org,
                    id,
                    serde_json::json!({"id": id, "name": p.name}),
                    after,
                )
                .await?
            }
        }
        tx.commit().await
    }

    async fn custom_patterns(&self, cx: &Cx, repo: &Repo) -> Result<Vec<Custom>> {
        let mut out = Vec::new();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        for scope in [repo.id.as_str(), repo.org_id.as_str()] {
            for (p, _) in tx
                .scan_msgs::<SecretScanningPattern>(
                    &PATTERNS,
                    &Scan::scope(scope, crate::store::MAX_SCAN),
                )
                .await?
            {
                if p.published
                    && let Ok(c) = validate_pattern(&p)
                {
                    out.push(c);
                }
            }
        }
        Ok(out)
    }

    // ---- scanning -----------------------------------------------------------------------

    /// Scans `files` with the built-in rules and the repo's and org's published patterns.
    pub(crate) async fn scan_files(
        &self,
        cx: &Cx,
        repo: &Repo,
        files: &[(String, Vec<u8>, Option<ObjectId>)],
    ) -> Result<Vec<Hit>> {
        let customs = self.custom_patterns(cx, repo).await?;
        let custom_refs: Vec<&Custom> = customs.iter().collect();
        let descriptions: BTreeMap<&str, String> = self
            .scanner
            .rules()
            .iter()
            .map(|r| {
                let d = r.description();
                (
                    r.id(),
                    if d.is_empty() {
                        r.provider().to_string()
                    } else {
                        d.to_string()
                    },
                )
            })
            .collect();
        let mut hits = Vec::new();
        for (path, bytes, blob) in files {
            for f in self.scanner.scan(path, bytes) {
                let secret = bytes[f.start..f.end].to_vec();
                hits.push(Hit {
                    path: f.path.clone(),
                    line: f.line as u32,
                    secret_type: f.rule_id.replace('-', "_"),
                    display: descriptions
                        .get(f.rule_id.as_str())
                        .cloned()
                        .unwrap_or_else(|| f.provider.clone()),
                    rule_id: f.rule_id.clone(),
                    pattern_id: String::new(),
                    preview: f.redacted_preview.clone(),
                    hash: keyed_hash(&repo.id, &secret),
                    blocking: f.confidence.blocks(),
                    severity: match f.confidence {
                        Confidence::High => SecuritySeverity::High,
                        Confidence::Medium => SecuritySeverity::Medium,
                        Confidence::Low => SecuritySeverity::Low,
                    },
                    secret,
                    blob: blob.clone(),
                });
            }
            for mut h in scan_custom(&custom_refs, path, bytes, &repo.id) {
                h.blob = blob.clone();
                hits.push(h);
            }
        }
        Ok(hits)
    }

    pub(crate) async fn secret_alert_recs(
        &self,
        cx: &Cx,
        repo: &Repo,
    ) -> Result<Vec<(SecretAlertRec, Row)>> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let mut rows = tx
            .scan_msgs::<SecretAlertRec>(
                &SECRET_ALERTS,
                &Scan::scope(&repo.id, crate::store::MAX_SCAN),
            )
            .await?;
        rows.reverse();
        Ok(rows)
    }

    async fn validity_enabled(&self, cx: &Cx, repo: &Repo) -> Result<bool> {
        if self
            .config_of(cx, repo)
            .await?
            .secret_scanning_validity_checks
        {
            return Ok(true);
        }
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        Ok(tx
            .get_msg::<FlagRec>(&CONFIG, &repo.org_id, "validity")
            .await?
            .is_some_and(|(f, _)| f.enabled))
    }

    /// Opens or extends alerts for `hits`; a hit that is a blob location carries its changeset.
    /// Returns the numbers of alerts opened.
    pub(crate) async fn upsert_hits(
        &self,
        cx: &Cx,
        repo: &Repo,
        hits: &[(Hit, Option<ObjectId>, Option<SubjectRef>)],
        bypass_id: &str,
    ) -> Result<Vec<u64>> {
        if hits.is_empty() {
            return Ok(Vec::new());
        }
        let validity_on = self.validity_enabled(cx, repo).await?;
        // Group by secret.
        #[allow(clippy::type_complexity)]
        let mut groups: BTreeMap<
            String,
            Vec<&(Hit, Option<ObjectId>, Option<SubjectRef>)>,
        > = BTreeMap::new();
        for h in hits {
            groups.entry(h.0.key()).or_default().push(h);
        }
        let mut validities: BTreeMap<String, SecretScanningValidity> = BTreeMap::new();
        if validity_on {
            for (k, g) in &groups {
                let h = &g[0].0;
                validities.insert(
                    k.clone(),
                    self.ports.verifier.verify(&h.secret_type, &h.secret).await,
                );
            }
        }
        let public = repo.visibility == bee_proto::v1::Visibility::Public as i32;
        let now = self.now();
        let mut opened = Vec::new();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        for (key, group) in groups {
            let first = &group[0].0;
            let locs: Vec<SecretScanningLocation> = group
                .iter()
                .map(|(h, cs, subj)| SecretScanningLocation {
                    location: Some(match subj {
                        Some(s) => {
                            bee_proto::v1::secret_scanning_location::Location::Subject(s.clone())
                        }
                        None => {
                            bee_proto::v1::secret_scanning_location::Location::Blob(h.finding())
                        }
                    }),
                    changeset: cs.clone(),
                })
                .collect();
            let existing = tx
                .scan_msgs::<SecretAlertRec>(
                    &SECRET_ALERTS,
                    &Scan::scope(&repo.id, 1).key(Key::K1(key.clone())),
                )
                .await?
                .into_iter()
                .next();
            match existing {
                Some((mut rec, row)) => {
                    let mut dirty = false;
                    for l in locs {
                        if !rec.locations.iter().any(|x| same_location(x, &l)) {
                            rec.locations.push(l);
                            dirty = true;
                        }
                    }
                    if !bypass_id.is_empty()
                        && let Some(a) = rec.alert.as_mut()
                        && !a.push_protection_bypassed
                    {
                        a.push_protection_bypassed = true;
                        a.push_protection_bypass_id = bypass_id.to_string();
                        dirty = true;
                    }
                    if dirty {
                        if let Some(a) = rec.alert.as_mut() {
                            a.updated_at = Some(ts(now));
                        }
                        tx.update(&SECRET_ALERTS, &row.clone().n1(now as i64).msg(&rec))
                            .await?;
                    }
                }
                None => {
                    let number = super::next_number(&mut tx, &repo.id, "secret_alert").await?;
                    let alert = SecretScanningAlert {
                        number,
                        repo: repo_ref_of(repo),
                        state: SecretScanningAlertState::Open as i32,
                        secret_type: first.secret_type.clone(),
                        secret_type_display_name: first.display.clone(),
                        secret_preview: first.preview.clone(),
                        pattern_id: first.pattern_id.clone(),
                        validity: validities
                            .get(&key)
                            .copied()
                            .unwrap_or(SecretScanningValidity::Unspecified)
                            as i32,
                        push_protection_bypassed: !bypass_id.is_empty(),
                        push_protection_bypass_id: bypass_id.to_string(),
                        created_at: Some(ts(now)),
                        updated_at: Some(ts(now)),
                        publicly_leaked: public,
                        url: format!("/{}/security/secret-scanning/{number}", repo.full_name),
                        severity: first.severity as i32,
                        ..Default::default()
                    };
                    let rec = SecretAlertRec {
                        alert: Some(alert.clone()),
                        locations: locs,
                        secret_hash: first.hash.clone(),
                        key: key.clone(),
                    };
                    tx.insert(
                        &SECRET_ALERTS,
                        &Row::new(&repo.id, num_id(number))
                            .k1(&key)
                            .k2("open")
                            .n1(now as i64)
                            .msg(&rec),
                    )
                    .await?;
                    record(
                        &mut tx,
                        Change {
                            repo,
                            event: "secret_scanning_alert.created",
                            action: "security.secret_alert.open",
                            subject: SubjectKind::SecurityAlert,
                            id: &number.to_string(),
                            payload: serde_json::to_value(&alert).unwrap_or_default(),
                            before: String::new(),
                            after: alert_json(&alert),
                        },
                    )
                    .await?;
                    opened.push(number);
                }
            }
        }
        tx.commit().await?;
        Ok(opened)
    }

    /// Scans the history reachable from `tip` (newest `limit` changesets) and opens alerts for
    /// every distinct secret, each with all of its locations and the oldest changeset that
    /// introduced it. Returns the numbers of alerts opened.
    pub async fn scan_history(
        &self,
        cx: &Cx,
        repo: &Repo,
        tip: &ObjectId,
        limit: usize,
    ) -> Result<Vec<u64>> {
        let cfg = self.config_of(cx, repo).await?;
        if !cfg.secret_scanning {
            return Ok(Vec::new());
        }
        let changesets = self
            .ports
            .vcs
            .history(repo, tip, limit.min(MAX_SCAN_CHANGESETS))
            .await?;
        let mut acc: Vec<(Hit, Option<ObjectId>, Option<SubjectRef>)> = Vec::new();
        for cs in &changesets {
            let blobs = self.ports.vcs.added_blobs(repo, cs).await?;
            let files: Vec<(String, Vec<u8>, Option<ObjectId>)> = blobs
                .into_iter()
                .map(|b| (b.path, b.bytes, Some(b.blob)))
                .collect();
            for h in self.scan_files(cx, repo, &files).await? {
                acc.push((h, Some(cs.clone()), None));
            }
        }
        self.upsert_hits(cx, repo, &acc, "").await
    }

    /// Scans one changeset's added blobs (the server calls this after a write lands).
    pub async fn scan_changeset(&self, cx: &Cx, repo: &Repo, at: &ObjectId) -> Result<Vec<u64>> {
        self.scan_history(cx, repo, at, 1).await
    }

    /// Scans text of an issue, change request, discussion or comment body.
    pub async fn scan_subject_text(
        &self,
        cx: &Cx,
        repo: &Repo,
        subject: SubjectRef,
        text: &str,
    ) -> Result<Vec<u64>> {
        if !self.config_of(cx, repo).await?.secret_scanning {
            return Ok(Vec::new());
        }
        let hits = self
            .scan_files(
                cx,
                repo,
                &[("<text>".into(), text.as_bytes().to_vec(), None)],
            )
            .await?;
        let acc: Vec<_> = hits
            .into_iter()
            .map(|h| (h, None, Some(subject.clone())))
            .collect();
        self.upsert_hits(cx, repo, &acc, "").await
    }

    // ---- alerts -------------------------------------------------------------------------

    /// `ListSecretScanningAlerts`.
    pub async fn list_secret_alerts(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &ListSecretScanningAlertsRequest,
        token: &str,
        size: usize,
    ) -> Result<(Vec<SecretScanningAlert>, String)> {
        let alerts: Vec<SecretScanningAlert> = self
            .secret_alert_recs(cx, repo)
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.alert)
            .filter(|a| req.state == 0 || a.state == req.state)
            .filter(|a| req.secret_types.is_empty() || req.secret_types.contains(&a.secret_type))
            .filter(|a| req.resolutions.is_empty() || req.resolutions.contains(&a.resolution))
            .filter(|a| req.validity == 0 || a.validity == req.validity)
            .collect();
        page_slice(&alerts, token, size)
    }

    pub(crate) async fn secret_alert_rec(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
    ) -> Result<(SecretAlertRec, Row)> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.get_msg::<SecretAlertRec>(&SECRET_ALERTS, &repo.id, &num_id(number))
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("secret scanning alert #{number}")))
    }

    /// `GetSecretScanningAlert`. With validity checks on, an open alert's validity is
    /// re-verified from one of its locations first.
    pub async fn get_secret_alert(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
    ) -> Result<SecretScanningAlert> {
        let (rec, _) = self.secret_alert_rec(cx, repo, number).await?;
        let alert = rec
            .alert
            .clone()
            .ok_or_else(|| ForgeError::internal("alert record without alert"))?;
        if alert_state(&alert) == SecretScanningAlertState::Open
            && self.validity_enabled(cx, repo).await?
        {
            return self.refresh_validity(cx, repo, number).await;
        }
        Ok(alert)
    }

    /// Re-verifies an alert's secret and records a change of validity.
    pub async fn refresh_validity(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
    ) -> Result<SecretScanningAlert> {
        let (rec, _) = self.secret_alert_rec(cx, repo, number).await?;
        let alert = rec.alert.clone().unwrap_or_default();
        let mut secret: Option<Vec<u8>> = None;
        for loc in &rec.locations {
            let Some(bee_proto::v1::secret_scanning_location::Location::Blob(b)) = &loc.location
            else {
                continue;
            };
            let Some(cs) = &loc.changeset else { continue };
            let path = String::from_utf8_lossy(&b.path).into_owned();
            let Ok(Some(bytes)) = self.ports.vcs.read_file(repo, cs, &path).await else {
                continue;
            };
            let files = [(path, bytes, None)];
            if let Some(h) = self
                .scan_files(cx, repo, &files)
                .await?
                .into_iter()
                .find(|h| h.hash == rec.secret_hash)
            {
                secret = Some(h.secret);
                break;
            }
        }
        let Some(secret) = secret else {
            return Ok(alert);
        };
        let validity = self
            .ports
            .verifier
            .verify(&alert.secret_type, &secret)
            .await;
        if validity as i32 == alert.validity {
            return Ok(alert);
        }
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<SecretAlertRec>(&SECRET_ALERTS, &repo.id, &num_id(number))
            .await?
        else {
            return Err(ForgeError::not_found(format!(
                "secret scanning alert #{number}"
            )));
        };
        let mut a = rec.alert.clone().unwrap_or_default();
        let before = alert_json(&a);
        a.validity = validity as i32;
        a.updated_at = Some(ts(now));
        rec.alert = Some(a.clone());
        tx.update(&SECRET_ALERTS, &row.clone().n1(now as i64).msg(&rec))
            .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "secret_scanning_alert.validated",
                action: "security.secret_alert.validate",
                subject: SubjectKind::SecurityAlert,
                id: &number.to_string(),
                payload: serde_json::to_value(&a).unwrap_or_default(),
                before,
                after: alert_json(&a),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(a)
    }

    async fn resolve_alert_system(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
        resolution: SecretScanningResolution,
        comment: &str,
    ) -> Result<()> {
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<SecretAlertRec>(&SECRET_ALERTS, &repo.id, &num_id(number))
            .await?
        else {
            return Ok(());
        };
        let mut a = rec.alert.clone().unwrap_or_default();
        if alert_state(&a) != SecretScanningAlertState::Open {
            return Ok(());
        }
        let before = alert_json(&a);
        a.state = SecretScanningAlertState::Resolved as i32;
        a.resolution = resolution as i32;
        a.resolution_comment = comment.to_string();
        a.resolved_by = Some(cx.principal());
        a.resolved_at = Some(ts(now));
        a.updated_at = Some(ts(now));
        rec.alert = Some(a.clone());
        tx.update(
            &SECRET_ALERTS,
            &row.clone().k2("resolved").n1(now as i64).msg(&rec),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "secret_scanning_alert.resolved",
                action: "security.secret_alert.resolve",
                subject: SubjectKind::SecurityAlert,
                id: &number.to_string(),
                payload: serde_json::to_value(&a).unwrap_or_default(),
                before,
                after: alert_json(&a),
            },
        )
        .await?;
        tx.commit().await
    }

    /// `UpdateSecretScanningAlert`: resolve (a user reason is required) or reopen.
    pub async fn update_secret_alert(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &UpdateSecretScanningAlertRequest,
    ) -> Result<SecretScanningAlert> {
        let target = SecretScanningAlertState::try_from(req.state)
            .unwrap_or(SecretScanningAlertState::Unspecified);
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<SecretAlertRec>(&SECRET_ALERTS, &repo.id, &num_id(req.number))
            .await?
        else {
            return Err(ForgeError::not_found(format!(
                "secret scanning alert #{}",
                req.number
            )));
        };
        let mut a = rec.alert.clone().unwrap_or_default();
        let before = alert_json(&a);
        let (event, action) = match target {
            SecretScanningAlertState::Resolved => {
                let r = SecretScanningResolution::try_from(req.resolution)
                    .unwrap_or(SecretScanningResolution::Unspecified);
                if !matches!(
                    r,
                    SecretScanningResolution::FalsePositive
                        | SecretScanningResolution::WontFix
                        | SecretScanningResolution::Revoked
                        | SecretScanningResolution::UsedInTests
                ) {
                    return Err(ForgeError::invalid(
                        "missing_resolution",
                        "resolving needs one of false_positive, wont_fix, revoked, used_in_tests",
                    ));
                }
                a.state = SecretScanningAlertState::Resolved as i32;
                a.resolution = r as i32;
                a.resolution_comment = req.resolution_comment.clone();
                a.resolved_by = Some(cx.principal());
                a.resolved_at = Some(ts(now));
                (
                    "secret_scanning_alert.resolved",
                    "security.secret_alert.resolve",
                )
            }
            SecretScanningAlertState::Open => {
                if a.resolution == SecretScanningResolution::PatternDeleted as i32 {
                    return Err(ForgeError::invalid_state(
                        "the alert's pattern was deleted; it cannot be reopened",
                    ));
                }
                a.state = SecretScanningAlertState::Open as i32;
                a.resolution = 0;
                a.resolution_comment.clear();
                a.resolved_by = None;
                a.resolved_at = None;
                (
                    "secret_scanning_alert.reopened",
                    "security.secret_alert.reopen",
                )
            }
            _ => {
                return Err(ForgeError::invalid(
                    "invalid_state",
                    "state must be OPEN or RESOLVED",
                ));
            }
        };
        a.updated_at = Some(ts(now));
        rec.alert = Some(a.clone());
        let label = if alert_state(&a) == SecretScanningAlertState::Open {
            "open"
        } else {
            "resolved"
        };
        tx.update(
            &SECRET_ALERTS,
            &row.clone().k2(label).n1(now as i64).msg(&rec),
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

    /// `ListSecretScanningLocations`.
    pub async fn secret_locations(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
        token: &str,
        size: usize,
    ) -> Result<(Vec<SecretScanningLocation>, String)> {
        let (rec, _) = self.secret_alert_rec(cx, repo, number).await?;
        page_slice(&rec.locations, token, size)
    }

    // ---- validity switch ------------------------------------------------------------------

    /// `SetSecretScanningValidityChecks` for an org.
    pub async fn set_org_validity_checks(
        &self,
        cx: &Cx,
        org_id: &str,
        enabled: bool,
    ) -> Result<bool> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.put(
            &CONFIG,
            &Row::new(org_id, "validity")
                .k1("validity")
                .msg(&FlagRec { enabled }),
        )
        .await?;
        record_org(
            &mut tx,
            org_id,
            "security_config.updated",
            "security.validity_checks.update",
            SubjectKind::Org,
            org_id,
            serde_json::json!({"secret_scanning_validity_checks": enabled}),
            serde_json::json!({"enabled": enabled}).to_string(),
        )
        .await?;
        tx.commit().await?;
        Ok(enabled)
    }

    // ---- push protection ------------------------------------------------------------------

    /// The repo's bypass policy (`review` unless set).
    pub async fn bypass_policy(&self, cx: &Cx, repo: &Repo) -> Result<BypassPolicyRec> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        Ok(tx
            .get_msg::<BypassPolicyRec>(&CONFIG, &repo.id, "bypass_policy")
            .await?
            .map(|(p, _)| p)
            .unwrap_or_else(|| BypassPolicyRec {
                mode: "review".into(),
                reviewer_ids: Vec::new(),
                ttl_hours: DEFAULT_BYPASS_TTL_HOURS,
            }))
    }

    /// Sets how bypass requests are decided: `review` (a reviewer approves), `allow` (granted
    /// at once, audited) or `deny` (blocks cannot be bypassed); `reviewer_ids` may review in
    /// addition to repo admins and org security managers.
    pub async fn set_bypass_policy(
        &self,
        cx: &Cx,
        repo: &Repo,
        mode: &str,
        reviewer_ids: Vec<String>,
        ttl_hours: u32,
    ) -> Result<BypassPolicyRec> {
        if !matches!(mode, "review" | "allow" | "deny") {
            return Err(ForgeError::invalid(
                "invalid_setting",
                "bypass mode must be review, allow or deny",
            ));
        }
        let policy = BypassPolicyRec {
            mode: mode.to_string(),
            reviewer_ids,
            ttl_hours: if ttl_hours == 0 {
                DEFAULT_BYPASS_TTL_HOURS
            } else {
                ttl_hours
            },
        };
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.put(
            &CONFIG,
            &Row::new(&repo.id, "bypass_policy")
                .k1("bypass_policy")
                .msg(&policy),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "security_config.updated",
                action: "security.bypass_policy.update",
                subject: SubjectKind::Repo,
                id: &repo.id,
                payload: serde_json::json!({"push_protection_bypass": mode}),
                before: String::new(),
                after: serde_json::json!({"mode": mode, "reviewers": policy.reviewer_ids})
                    .to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(policy)
    }

    fn block_error(&self, repo: &Repo, block: &BlockRec, allowed: bool) -> ForgeError {
        let rules: Vec<&str> = block
            .findings
            .iter()
            .map(|f| f.pattern_id.as_str())
            .collect();
        ForgeError::new(
            ErrorReason::SecretDetected,
            format!(
                "{} secret(s) detected ({}); remove them or request a push-protection bypass",
                block.findings.len(),
                rules.join(", ")
            ),
        )
        .with_payload(Payload::PushProtection(PushProtectionBlock {
            findings: block.findings.clone(),
            bypass_allowed: allowed,
            bypass_request_url: format!(
                "/{}/security/push-protection/{}",
                repo.full_name, block.id
            ),
        }))
    }

    /// Push protection for a write of `files` by the caller of `cx`: `Ok` lets the write
    /// through (with the exercised bypass, if any), `Err` is `SECRET_DETECTED` carrying the
    /// block (its request URL ends in the block id the bypass request names). The server calls
    /// this at CreateChangeset, Land and git push.
    pub async fn protect(
        &self,
        cx: &Cx,
        repo: &Repo,
        files: &[(String, Vec<u8>, Option<ObjectId>)],
        bypass_id: Option<&str>,
    ) -> Result<PushOutcome> {
        let cfg = self.config_of(cx, repo).await?;
        if !cfg.secret_scanning_push_protection {
            return Ok(PushOutcome::default());
        }
        let hits = self.scan_files(cx, repo, files).await?;
        let customs_blocking: std::collections::BTreeSet<String> = {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            let mut s = std::collections::BTreeSet::new();
            for scope in [repo.id.as_str(), repo.org_id.as_str()] {
                for (p, _) in tx
                    .scan_msgs::<SecretScanningPattern>(
                        &PATTERNS,
                        &Scan::scope(scope, crate::store::MAX_SCAN),
                    )
                    .await?
                {
                    if p.published && p.push_protection {
                        s.insert(p.id);
                    }
                }
            }
            s
        };
        let blocking: Vec<&Hit> = hits
            .iter()
            .filter(|h| {
                if h.pattern_id.is_empty() {
                    h.blocking
                } else {
                    customs_blocking.contains(&h.pattern_id)
                }
            })
            .collect();
        let advisory = hits.len() - blocking.len();
        if blocking.is_empty() {
            return Ok(PushOutcome {
                advisory_findings: advisory,
                ..PushOutcome::default()
            });
        }
        let policy = self.bypass_policy(cx, repo).await?;
        let now = self.now();
        if let Some(bid) = bypass_id.filter(|b| !b.is_empty()) {
            return self
                .exercise_bypass(cx, repo, bid, &blocking, advisory, now)
                .await;
        }
        let block = BlockRec {
            id: self.db.ids().ulid()?,
            findings: blocking.iter().map(|h| h.finding()).collect(),
            requester: cx.actor_id.clone(),
            created_ms: now,
            expires_ms: now + BLOCK_TTL_MS,
            previews: blocking.iter().map(|h| h.preview.clone()).collect(),
            hashes: blocking.iter().map(|h| h.hash.clone()).collect(),
        };
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.insert(
            &BLOCKS,
            &Row::new(&repo.id, &block.id)
                .n1(block.expires_ms as i64)
                .msg(&block),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "push_protection.blocked",
                action: "push_protection.block",
                subject: SubjectKind::Repo,
                id: &block.id,
                payload: serde_json::json!({
                    "block_id": block.id,
                    "findings": block.findings.iter().zip(&block.previews).map(|(f, p)| serde_json::json!({
                        "path": String::from_utf8_lossy(&f.path), "line": f.line,
                        "secret_type": f.secret_type, "preview": p,
                    })).collect::<Vec<_>>(),
                }),
                before: String::new(),
                after: serde_json::json!({"block_id": block.id, "count": block.findings.len()}).to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Err(self.block_error(repo, &block, policy.mode != "deny"))
    }

    async fn exercise_bypass(
        &self,
        cx: &Cx,
        repo: &Repo,
        bypass_id: &str,
        blocking: &[&Hit],
        advisory: usize,
        now: u64,
    ) -> Result<PushOutcome> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<BypassRec>(&BYPASSES, &repo.id, bypass_id)
            .await?
        else {
            return Err(ForgeError::not_found(format!(
                "push-protection bypass {bypass_id}"
            )));
        };
        let mut b = rec.bypass.clone().unwrap_or_default();
        let requester = b
            .requester
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        if !cx.chain.contains(&requester) {
            return Err(ForgeError::permission_denied(
                "this bypass was granted to another principal",
            ));
        }
        let state = effective_state(&b, rec.consumed, now);
        if !matches!(
            state,
            PushProtectionBypassState::Approved | PushProtectionBypassState::Granted
        ) || rec.consumed
        {
            return Err(ForgeError::invalid_state(format!(
                "bypass {bypass_id} is {} and cannot be used",
                if rec.consumed {
                    "already used".to_string()
                } else {
                    state_key(state).to_string()
                }
            )));
        }
        let covered: std::collections::BTreeSet<(Vec<u8>, String)> = b
            .findings
            .iter()
            .map(|f| (f.path.clone(), f.pattern_id.clone()))
            .collect();
        for h in blocking {
            let f = h.finding();
            if !covered.contains(&(f.path.clone(), f.pattern_id.clone())) {
                return Err(ForgeError::permission_denied(format!(
                    "bypass {bypass_id} does not cover the secret in {}",
                    h.path
                )));
            }
        }
        rec.consumed = true;
        b.audit_entry_id = record_id(
            &mut tx,
            Change {
                repo,
                event: "push_protection.bypassed",
                action: "push_protection.bypass",
                subject: SubjectKind::Repo,
                id: bypass_id,
                payload: serde_json::json!({
                    "bypass_id": bypass_id,
                    "reason": PushProtectionBypassReason::try_from(b.reason).map(|r| r.as_str_name()).unwrap_or(""),
                    "findings": blocking.iter().map(|h| serde_json::json!({
                        "path": h.path, "line": h.line, "rule": h.rule_id, "preview": h.preview,
                    })).collect::<Vec<_>>(),
                }),
                before: String::new(),
                after: bypass_json(&b),
            },
        )
        .await?;
        rec.bypass = Some(b.clone());
        tx.update(&BYPASSES, &row.clone().msg(&rec)).await?;
        tx.commit().await?;
        let acc: Vec<_> = blocking
            .iter()
            .map(|h| ((*h).clone(), None, None))
            .collect();
        let alerts = self.upsert_hits(cx, repo, &acc, bypass_id).await?;
        // Existing alerts for the same secrets are marked as bypassed by upsert_hits.
        Ok(PushOutcome {
            bypass_id: Some(bypass_id.to_string()),
            alerts,
            advisory_findings: advisory,
        })
    }

    /// `RequestPushProtectionBypass`.
    pub async fn request_bypass(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &RequestPushProtectionBypassRequest,
    ) -> Result<PushProtectionBypass> {
        if req.block_id.is_empty() {
            return Err(ForgeError::missing("block_id"));
        }
        let reason = PushProtectionBypassReason::try_from(req.reason)
            .unwrap_or(PushProtectionBypassReason::Unspecified);
        if reason == PushProtectionBypassReason::Unspecified {
            return Err(ForgeError::invalid(
                "missing_reason",
                "a bypass needs a reason: used_in_tests, false_positive or will_fix_later",
            ));
        }
        let policy = self.bypass_policy(cx, repo).await?;
        if policy.mode == "deny" {
            return Err(ForgeError::permission_denied(
                "this repo does not allow push-protection bypasses",
            ));
        }
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((block, _)) = tx
            .get_msg::<BlockRec>(&BLOCKS, &repo.id, &req.block_id)
            .await?
        else {
            return Err(ForgeError::not_found(format!("block {}", req.block_id)));
        };
        if block.expires_ms < now {
            return Err(ForgeError::invalid_state(
                "the block expired; write again to get a new one",
            ));
        }
        if !cx.chain.contains(&block.requester) {
            return Err(ForgeError::permission_denied(
                "only the principal that was blocked may request its bypass",
            ));
        }
        // One live request per block.
        for (other, _) in tx
            .scan_msgs::<BypassRec>(
                &BYPASSES,
                &Scan::scope(&repo.id, crate::store::MAX_SCAN)
                    .key(Key::K1(block.requester.clone())),
            )
            .await?
        {
            if let Some(o) = &other.bypass
                && o.block_id == req.block_id
                && !other.consumed
                && matches!(
                    effective_state(o, other.consumed, now),
                    PushProtectionBypassState::Pending
                        | PushProtectionBypassState::Approved
                        | PushProtectionBypassState::Granted
                )
            {
                return Err(ForgeError::already_exists(format!(
                    "a bypass request for block {}",
                    req.block_id
                )));
            }
        }
        let findings: Vec<SecretFinding> = if req.findings.is_empty() {
            block.findings.clone()
        } else {
            for f in &req.findings {
                if !block
                    .findings
                    .iter()
                    .any(|b| b.path == f.path && b.pattern_id == f.pattern_id)
                {
                    return Err(ForgeError::invalid(
                        "findings_mismatch",
                        "a finding does not belong to the block",
                    ));
                }
            }
            req.findings.clone()
        };
        let ttl_ms = u64::from(policy.ttl_hours.max(1)) * 3600 * 1000;
        let granted = policy.mode == "allow";
        let mut b = PushProtectionBypass {
            id: tx.new_id()?,
            repo: repo_ref_of(repo),
            block_id: req.block_id.clone(),
            findings,
            reason: reason as i32,
            comment: req.comment.clone(),
            state: if granted {
                PushProtectionBypassState::Granted
            } else {
                PushProtectionBypassState::Pending
            } as i32,
            requester: Some(cx.principal()),
            reviewer: None,
            review_comment: String::new(),
            created_at: Some(ts(now)),
            reviewed_at: None,
            expires_at: Some(ts(now + if granted { ttl_ms } else { PENDING_TTL_MS })),
            audit_entry_id: String::new(),
        };
        b.requester = Some(self.principal(&cx.actor_id).await);
        b.audit_entry_id = record_id(
            &mut tx,
            Change {
                repo,
                event: "push_protection.bypass_requested",
                action: "push_protection.bypass_request",
                subject: SubjectKind::Repo,
                id: &b.id,
                payload: serde_json::json!({"bypass_id": b.id, "block_id": b.block_id, "state": bypass_state(&b).as_str_name()}),
                before: String::new(),
                after: bypass_json(&b),
            },
        )
        .await?;
        let rec = BypassRec {
            bypass: Some(b.clone()),
            consumed: false,
        };
        tx.insert(
            &BYPASSES,
            &Row::new(&repo.id, &b.id)
                .k1(&block.requester)
                .k2(state_key(bypass_state(&b)))
                .n1(now as i64)
                .msg(&rec),
        )
        .await?;
        tx.commit().await?;
        Ok(b)
    }

    async fn bypass_row(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<(BypassRec, Row)> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.get_msg::<BypassRec>(&BYPASSES, &repo.id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("push-protection bypass {id}")))
    }

    /// `GetPushProtectionBypass` (expiry applied).
    pub async fn get_bypass(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<PushProtectionBypass> {
        let (rec, _) = self.bypass_row(cx, repo, id).await?;
        Ok(present(&rec, self.now()))
    }

    /// `ListPushProtectionBypasses`, newest first.
    pub async fn list_bypasses(
        &self,
        cx: &Cx,
        repo: &Repo,
        state: i32,
        requester_id: &str,
        token: &str,
        size: usize,
    ) -> Result<(Vec<PushProtectionBypass>, String)> {
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let mut q = Scan::scope(&repo.id, crate::store::MAX_SCAN).by_n1().desc();
        if !requester_id.is_empty() {
            q = q.key(Key::K1(requester_id.to_string()));
        }
        let all: Vec<PushProtectionBypass> = tx
            .scan_msgs::<BypassRec>(&BYPASSES, &q)
            .await?
            .into_iter()
            .map(|(r, _)| present(&r, now))
            .filter(|b| state == 0 || b.state == state)
            .collect();
        page_slice(&all, token, size)
    }

    /// Whether `principal_id` may review bypass requests through the repo's own policy list.
    pub async fn is_listed_reviewer(
        &self,
        cx: &Cx,
        repo: &Repo,
        principal_id: &str,
    ) -> Result<bool> {
        Ok(self
            .bypass_policy(cx, repo)
            .await?
            .reviewer_ids
            .iter()
            .any(|r| r == principal_id))
    }

    /// `ReviewPushProtectionBypass`: approve or deny a pending request. The reviewer must not be
    /// the requester (the service has already checked the reviewer role).
    pub async fn review_bypass(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &ReviewPushProtectionBypassRequest,
    ) -> Result<PushProtectionBypass> {
        let now = self.now();
        let policy = self.bypass_policy(cx, repo).await?;
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<BypassRec>(&BYPASSES, &repo.id, &req.id)
            .await?
        else {
            return Err(ForgeError::not_found(format!(
                "push-protection bypass {}",
                req.id
            )));
        };
        let mut b = rec.bypass.clone().unwrap_or_default();
        let state = effective_state(&b, rec.consumed, now);
        if state != PushProtectionBypassState::Pending {
            return Err(ForgeError::invalid_state(format!(
                "bypass {} is already {}",
                req.id,
                state_key(state)
            )));
        }
        let requester = b
            .requester
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        if cx.chain.contains(&requester) {
            return Err(ForgeError::permission_denied(
                "a bypass request cannot be reviewed by its requester",
            ));
        }
        let before = bypass_json(&b);
        b.state = if req.approve {
            PushProtectionBypassState::Approved
        } else {
            PushProtectionBypassState::Denied
        } as i32;
        b.reviewer = Some(self.principal(&cx.actor_id).await);
        b.review_comment = req.comment.clone();
        b.reviewed_at = Some(ts(now));
        if req.approve {
            let ttl_ms = u64::from(policy.ttl_hours.max(1)) * 3600 * 1000;
            b.expires_at = Some(ts(now + ttl_ms));
        }
        b.audit_entry_id = record_id(
            &mut tx,
            Change {
                repo,
                event: "push_protection.bypass_reviewed",
                action: "push_protection.bypass_review",
                subject: SubjectKind::Repo,
                id: &b.id,
                payload: serde_json::json!({"bypass_id": b.id, "approved": req.approve}),
                before,
                after: bypass_json(&b),
            },
        )
        .await?;
        rec.bypass = Some(b.clone());
        tx.update(
            &BYPASSES,
            &row.clone().k2(state_key(bypass_state(&b))).msg(&rec),
        )
        .await?;
        tx.commit().await?;
        Ok(b)
    }

    /// Marks lapsed bypass requests `EXPIRED` and removes old blocks.
    pub async fn expire_blocks(&self, cx: &Cx) -> Result<()> {
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let blocks = tx
            .scan_msgs::<BlockRec>(
                &BLOCKS,
                &Scan {
                    scope: None,
                    order: crate::store::Order::N1,
                    n1_max: Some(now.saturating_sub(30 * 24 * 3600 * 1000) as i64),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?;
        for (_, row) in blocks {
            tx.delete(&BLOCKS, &row.scope, &row.id).await?;
        }
        let lapsed = tx
            .scan_msgs::<BypassRec>(
                &BYPASSES,
                &Scan {
                    scope: None,
                    key: Some(Key::K2("pending".into())),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?;
        for (mut rec, row) in lapsed {
            let Some(mut b) = rec.bypass.clone() else {
                continue;
            };
            if effective_state(&b, rec.consumed, now) == PushProtectionBypassState::Expired {
                b.state = PushProtectionBypassState::Expired as i32;
                rec.bypass = Some(b);
                tx.update(&BYPASSES, &row.clone().k2("expired").msg(&rec))
                    .await?;
            }
        }
        tx.commit().await
    }

    /// The block with `id`, for the server to render the refusal again.
    pub async fn block_findings(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
    ) -> Result<Vec<SecretFinding>> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.get_msg::<BlockRec>(&BLOCKS, &repo.id, id)
            .await?
            .map(|(b, _)| b.findings)
            .ok_or_else(|| ForgeError::not_found(format!("block {id}")))
    }
}

/// The bypass as clients see it: lazily expired.
fn present(rec: &BypassRec, now: u64) -> PushProtectionBypass {
    let mut b = rec.bypass.clone().unwrap_or_default();
    b.state = effective_state(&b, rec.consumed, now) as i32;
    b
}

fn effective_state(
    b: &PushProtectionBypass,
    consumed: bool,
    now: u64,
) -> PushProtectionBypassState {
    let s = bypass_state(b);
    let expired = b.expires_at.as_ref().is_some_and(|e| ts_ms(e) < now);
    match s {
        PushProtectionBypassState::Pending
        | PushProtectionBypassState::Approved
        | PushProtectionBypassState::Granted
            if expired && !consumed =>
        {
            PushProtectionBypassState::Expired
        }
        other => other,
    }
}

fn same_location(a: &SecretScanningLocation, b: &SecretScanningLocation) -> bool {
    use bee_proto::v1::secret_scanning_location::Location;
    match (&a.location, &b.location) {
        (Some(Location::Blob(x)), Some(Location::Blob(y))) => {
            x.path == y.path && x.line == y.line && x.blob == y.blob
        }
        (Some(Location::Subject(x)), Some(Location::Subject(y))) => x == y,
        _ => false,
    }
}

fn scan_custom(customs: &[&Custom], path: &str, bytes: &[u8], repo_id: &str) -> Vec<Hit> {
    let mut out = Vec::new();
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return out;
    }
    for c in customs {
        for caps in c.regex.captures_iter(bytes) {
            let Some(m) = caps.name("secret") else {
                continue;
            };
            let secret = m.as_bytes();
            if !c.must.iter().all(|r| r.is_match(secret))
                || c.must_not.iter().any(|r| r.is_match(secret))
            {
                continue;
            }
            out.push(Hit {
                path: path.to_string(),
                line: line_of(bytes, m.start()),
                rule_id: format!("custom-{}", c.id),
                secret_type: format!("custom_{}", snake(&c.name)),
                display: c.name.clone(),
                pattern_id: c.id.clone(),
                preview: redact(secret),
                hash: keyed_hash(repo_id, secret),
                blocking: c.block,
                severity: SecuritySeverity::High,
                secret: secret.to_vec(),
                blob: None,
            });
        }
    }
    out
}
