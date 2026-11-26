//! Repository security advisories: drafts, collaboration, credits, CVE requests, publication to
//! the global database and private vulnerability reports from outsiders.
//!
//! Lifecycle: a private report starts in `TRIAGE`; maintainers accept it (`DRAFT`) or close it.
//! A draft is edited, gets collaborators and credits, may request a CVE id, and is published
//! (`PUBLISHED`, written to the global database and matched against dependency graphs at
//! once). A published advisory can be withdrawn (final). Unpublished advisories are visible
//! only to maintainers, the author, the reporter and collaborators.

use bee_deps::Ecosystem;
use bee_proto::v1::{
    AddSecurityAdvisoryCollaboratorRequest, AdvisoryAffectedPackage, AdvisoryType,
    CreateSecurityAdvisoryRequest, Principal, RemoveSecurityAdvisoryCollaboratorRequest, Repo,
    SecurityAdvisoryCredit, SecurityAdvisoryState, SecurityRepoAdvisory, SecuritySeverity,
    SubjectKind, SubmitVulnerabilityReportRequest, UpdateSecurityAdvisoryRequest,
};

use super::advisories::{osv_of_repo_advisory, to_proto};
use super::model::{AdvisoryRec, CveRequestRec, GlobalAdvRec};
use super::{
    CVE_REQUESTS, Change, GLOBAL_ADVISORIES, REPO_ADVISORIES, Security, page_slice, record,
    repo_ref_of, sev,
};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Key, Row, Scan, Surface};

/// Credit roles (GitHub's credit types plus `collaborator`).
pub const CREDIT_ROLES: &[&str] = &[
    "finder",
    "reporter",
    "analyst",
    "coordinator",
    "remediation_developer",
    "remediation_reviewer",
    "remediation_verifier",
    "tool",
    "sponsor",
    "other",
    "collaborator",
];
/// Most open private reports one reporter may have in one repo.
pub const MAX_OPEN_REPORTS: usize = 10;
const MAX_TEXT: usize = 65_536;
const ALPHABET: &[u8] = b"23456789cfghjmpqrvwx";

/// Who is asking, for visibility.
#[derive(Clone, Debug, Default)]
pub struct Viewer {
    /// The caller's delegation chain (actor first).
    pub ids: Vec<String>,
    /// A maintainer (or org security manager): sees every advisory and may manage them.
    pub privileged: bool,
}

fn state_of(a: &SecurityRepoAdvisory) -> SecurityAdvisoryState {
    SecurityAdvisoryState::try_from(a.state).unwrap_or(SecurityAdvisoryState::Unspecified)
}

fn state_label(s: SecurityAdvisoryState) -> &'static str {
    match s {
        SecurityAdvisoryState::Triage => "triage",
        SecurityAdvisoryState::Draft => "draft",
        SecurityAdvisoryState::Published => "published",
        SecurityAdvisoryState::Closed => "closed",
        SecurityAdvisoryState::Withdrawn => "withdrawn",
        SecurityAdvisoryState::Unspecified => "unspecified",
    }
}

fn json_of(a: &SecurityRepoAdvisory) -> String {
    serde_json::json!({
        "ghsa_id": a.ghsa_id,
        "state": state_label(state_of(a)),
        "summary": a.summary,
        "severity": SecuritySeverity::try_from(a.severity).map(|s| s.as_str_name()).unwrap_or(""),
        "cve_id": a.cve_id,
        "cve_requested": a.cve_requested,
        "collaborators": a.collaborators.len(),
    })
    .to_string()
}

fn is_public(s: SecurityAdvisoryState) -> bool {
    matches!(
        s,
        SecurityAdvisoryState::Published | SecurityAdvisoryState::Withdrawn
    )
}

fn involved(a: &SecurityRepoAdvisory, v: &Viewer) -> bool {
    let p = |x: &Option<Principal>| x.as_ref().is_some_and(|x| v.ids.contains(&x.id));
    p(&a.author)
        || p(&a.reporter)
        || a.collaborators
            .iter()
            .any(|c| !c.principal_id.is_empty() && v.ids.contains(&c.principal_id))
}

fn visible(a: &SecurityRepoAdvisory, v: &Viewer) -> bool {
    v.privileged || is_public(state_of(a)) || involved(a, v)
}

fn valid_cwe(c: &str) -> bool {
    c.strip_prefix("CWE-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn check_text(what: &str, s: &str) -> Result<()> {
    if s.len() > MAX_TEXT {
        return Err(ForgeError::too_large(what, MAX_TEXT as u64, s.len() as u64));
    }
    Ok(())
}

fn check_credit(c: &SecurityAdvisoryCredit) -> Result<()> {
    if c.principal_id.is_empty() == c.team_id.is_empty() {
        return Err(ForgeError::invalid(
            "invalid_credit",
            "a credit names exactly one of principal_id and team_id",
        ));
    }
    if !c.role.is_empty() && !CREDIT_ROLES.contains(&c.role.as_str()) {
        return Err(ForgeError::invalid(
            "invalid_credit",
            format!("unknown credit role {:?}", c.role),
        ));
    }
    Ok(())
}

/// Validates content shared by drafts and reports; returns the severity to store.
fn check_content(
    summary: &str,
    description: &str,
    severity: i32,
    cvss_vector: &str,
    cwe_ids: &[String],
    affected: &[AdvisoryAffectedPackage],
) -> Result<i32> {
    if summary.trim().is_empty() {
        return Err(ForgeError::missing("summary"));
    }
    if summary.len() > 1024 {
        return Err(ForgeError::invalid(
            "invalid_summary",
            "summary is longer than 1024 bytes",
        ));
    }
    check_text("description", description)?;
    for c in cwe_ids {
        if !valid_cwe(c) {
            return Err(ForgeError::invalid(
                "invalid_cwe",
                format!("{c:?} is not a CWE id (CWE-<n>)"),
            ));
        }
    }
    let mut sev_v = severity;
    if !cvss_vector.is_empty() {
        let parsed = bee_deps::cvss::parse(cvss_vector)
            .map_err(|e| ForgeError::invalid("invalid_cvss", e.message))?;
        if severity == 0 {
            sev_v = sev(parsed.severity) as i32;
        }
    }
    if SecuritySeverity::try_from(sev_v).is_err() {
        return Err(ForgeError::invalid("invalid_severity", "unknown severity"));
    }
    for p in affected {
        if Ecosystem::from_proto_name(&p.ecosystem)
            .or_else(|| Ecosystem::from_osv_name(&p.ecosystem))
            .is_none()
        {
            return Err(ForgeError::invalid(
                "unknown_ecosystem",
                format!("ecosystem {:?} is not supported", p.ecosystem),
            ));
        }
        if p.name.is_empty() {
            return Err(ForgeError::missing("affected.name"));
        }
    }
    // The affected ranges must be readable (they become the matching record at publication).
    osv_of_repo_advisory(
        "GHSA-aaaa-aaaa-aaaa",
        "",
        summary,
        description,
        cvss_vector,
        cwe_ids,
        affected,
        0,
    )?;
    Ok(sev_v)
}

fn ghsa_from(bytes: &[u8]) -> String {
    let c: Vec<char> = bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect();
    format!(
        "GHSA-{}-{}-{}",
        c[0..4].iter().collect::<String>(),
        c[4..8].iter().collect::<String>(),
        c[8..12].iter().collect::<String>()
    )
}

impl Security {
    fn new_ghsa(&self) -> Result<String> {
        let hex = self.db.ids().random_hex(12)?;
        let raw = crate::ids::unhex(&hex).unwrap_or_default();
        Ok(ghsa_from(&raw))
    }

    async fn fork_for(
        &self,
        cx: &Cx,
        repo: &Repo,
        ghsa: &str,
        wanted: bool,
    ) -> Result<Option<bee_proto::v1::RepoRef>> {
        let _ = cx;
        if !wanted {
            return Ok(None);
        }
        match &self.ports.forks {
            Some(f) => Ok(repo_ref_of(&f.create_private_fork(repo, ghsa).await?)),
            None => Err(crate::service::unserved("a private advisory fork", "forks")),
        }
    }

    async fn insert_advisory(
        &self,
        cx: &Cx,
        repo: &Repo,
        mut a: SecurityRepoAdvisory,
        event: &str,
        action: &str,
    ) -> Result<SecurityRepoAdvisory> {
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        loop {
            a.ghsa_id = self.new_ghsa()?;
            if tx
                .get(&REPO_ADVISORIES, &repo.id, &a.ghsa_id)
                .await?
                .is_none()
                && tx.get(&GLOBAL_ADVISORIES, "", &a.ghsa_id).await?.is_none()
            {
                break;
            }
        }
        a.url = format!("/{}/security/advisories/{}", repo.full_name, a.ghsa_id);
        a.created_at = Some(ts(now));
        a.updated_at = Some(ts(now));
        tx.insert(
            &REPO_ADVISORIES,
            &Row::new(&repo.id, &a.ghsa_id)
                .k1(state_label(state_of(&a)))
                .n1(now as i64)
                .msg(&AdvisoryRec {
                    advisory: Some(a.clone()),
                }),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event,
                action,
                subject: SubjectKind::Advisory,
                id: &a.ghsa_id,
                payload: serde_json::to_value(&a).unwrap_or_default(),
                before: String::new(),
                after: json_of(&a),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(a)
    }

    /// `CreateSecurityAdvisory`: a draft.
    pub async fn create_advisory(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &CreateSecurityAdvisoryRequest,
    ) -> Result<SecurityRepoAdvisory> {
        let severity = check_content(
            &req.summary,
            &req.description,
            req.severity,
            &req.cvss_vector,
            &req.cwe_ids,
            &req.affected,
        )?;
        let mut credits = Vec::new();
        for c in &req.credits {
            check_credit(c)?;
            if !c.principal_id.is_empty() && self.directory.get(&c.principal_id).await.is_none() {
                return Err(ForgeError::invalid(
                    "unknown_principal",
                    format!("principal {} does not exist", c.principal_id),
                ));
            }
            let mut c = c.clone();
            if c.role.is_empty() {
                c.role = "other".into();
            }
            credits.push(c);
        }
        let fork = self.fork_for(cx, repo, "", req.start_private_fork).await?;
        let a = SecurityRepoAdvisory {
            repo: repo_ref_of(repo),
            state: SecurityAdvisoryState::Draft as i32,
            summary: req.summary.trim().to_string(),
            description: req.description.clone(),
            severity,
            cvss_vector: req.cvss_vector.clone(),
            cwe_ids: req.cwe_ids.clone(),
            affected: req.affected.clone(),
            credits,
            author: Some(self.principal(&cx.actor_id).await),
            private_fork: fork,
            ..Default::default()
        };
        self.insert_advisory(
            cx,
            repo,
            a,
            "security_advisory.created",
            "security.advisory.create",
        )
        .await
    }

    /// `SubmitVulnerabilityReport`: a private report from anyone who can see the repo; it needs
    /// private vulnerability reporting to be on.
    pub async fn submit_report(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &SubmitVulnerabilityReportRequest,
    ) -> Result<SecurityRepoAdvisory> {
        if !self
            .config_of(cx, repo)
            .await?
            .private_vulnerability_reporting
        {
            return Err(ForgeError::invalid_state(
                "private vulnerability reporting is off for this repo",
            ));
        }
        let severity = check_content(
            &req.summary,
            &req.description,
            req.severity,
            &req.cvss_vector,
            &req.cwe_ids,
            &req.affected,
        )?;
        let open = {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            tx.scan_msgs::<AdvisoryRec>(
                &REPO_ADVISORIES,
                &Scan::scope(&repo.id, crate::store::MAX_SCAN).key(Key::K1("triage".into())),
            )
            .await?
            .into_iter()
            .filter(|(r, _)| {
                r.advisory
                    .as_ref()
                    .and_then(|a| a.reporter.as_ref())
                    .is_some_and(|p| p.id == cx.actor_id)
            })
            .count()
        };
        if open >= MAX_OPEN_REPORTS {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::ResourceExhausted,
                format!("{MAX_OPEN_REPORTS} reports from you are still waiting for triage"),
            )
            .with_subcode("too_many_reports"));
        }
        let reporter = self.principal(&cx.actor_id).await;
        let fork = self.fork_for(cx, repo, "", req.start_private_fork).await?;
        let a = SecurityRepoAdvisory {
            repo: repo_ref_of(repo),
            state: SecurityAdvisoryState::Triage as i32,
            summary: req.summary.trim().to_string(),
            description: req.description.clone(),
            severity,
            cvss_vector: req.cvss_vector.clone(),
            cwe_ids: req.cwe_ids.clone(),
            affected: req.affected.clone(),
            credits: vec![SecurityAdvisoryCredit {
                principal_id: reporter.id.clone(),
                team_id: String::new(),
                role: "reporter".into(),
            }],
            author: Some(reporter.clone()),
            reporter: Some(reporter),
            private_fork: fork,
            ..Default::default()
        };
        self.insert_advisory(
            cx,
            repo,
            a,
            "security_advisory.reported",
            "security.advisory.report",
        )
        .await
    }

    async fn advisory_row(
        &self,
        cx: &Cx,
        repo: &Repo,
        ghsa: &str,
        viewer: &Viewer,
    ) -> Result<(SecurityRepoAdvisory, Row)> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let (rec, row) = tx
            .get_msg::<AdvisoryRec>(&REPO_ADVISORIES, &repo.id, ghsa)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("advisory {ghsa}")))?;
        let a = rec
            .advisory
            .ok_or_else(|| ForgeError::internal("advisory record without advisory"))?;
        if !visible(&a, viewer) {
            return Err(ForgeError::not_found(format!("advisory {ghsa}")));
        }
        Ok((a, row))
    }

    /// `GetSecurityAdvisory`.
    pub async fn get_repo_advisory(
        &self,
        cx: &Cx,
        repo: &Repo,
        ghsa: &str,
        viewer: &Viewer,
    ) -> Result<SecurityRepoAdvisory> {
        Ok(self.advisory_row(cx, repo, ghsa, viewer).await?.0)
    }

    /// `ListSecurityAdvisories`, newest first, limited to what `viewer` may see.
    pub async fn list_repo_advisories(
        &self,
        cx: &Cx,
        repo: &Repo,
        state: i32,
        viewer: &Viewer,
        token: &str,
        size: usize,
    ) -> Result<(Vec<SecurityRepoAdvisory>, String)> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let rows = tx
            .scan_msgs::<AdvisoryRec>(
                &REPO_ADVISORIES,
                &Scan::scope(&repo.id, crate::store::MAX_SCAN).by_n1().desc(),
            )
            .await?;
        let all: Vec<SecurityRepoAdvisory> = rows
            .into_iter()
            .filter_map(|(r, _)| r.advisory)
            .filter(|a| visible(a, viewer))
            .filter(|a| state == 0 || a.state == state)
            .collect();
        page_slice(&all, token, size)
    }

    /// `UpdateSecurityAdvisory`. `viewer.privileged` or a collaborator may edit; accepting or
    /// closing a report and withdrawing need `viewer.privileged`.
    pub async fn update_repo_advisory(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &UpdateSecurityAdvisoryRequest,
        viewer: &Viewer,
    ) -> Result<SecurityRepoAdvisory> {
        let (mut a, row) = self.advisory_row(cx, repo, &req.ghsa_id, viewer).await?;
        let current = state_of(&a);
        if current == SecurityAdvisoryState::Withdrawn {
            return Err(ForgeError::invalid_state(
                "a withdrawn advisory cannot be changed",
            ));
        }
        let collaborator = a
            .collaborators
            .iter()
            .any(|c| viewer.ids.contains(&c.principal_id));
        if !viewer.privileged && !(collaborator && !is_public(current)) {
            return Err(ForgeError::permission_denied(
                "only maintainers and collaborators of an unpublished advisory can edit it",
            ));
        }
        let before = json_of(&a);
        if let Some(s) = &req.summary {
            a.summary = s.trim().to_string();
        }
        if let Some(d) = &req.description {
            a.description = d.clone();
        }
        if let Some(v) = req.severity {
            a.severity = v;
        }
        if let Some(c) = &req.cvss_vector {
            a.cvss_vector = c.clone();
        }
        if !req.cwe_ids.is_empty() {
            a.cwe_ids = req.cwe_ids.clone();
        }
        if !req.affected.is_empty() {
            a.affected = req.affected.clone();
        }
        a.severity = check_content(
            &a.summary,
            &a.description,
            a.severity,
            &a.cvss_vector,
            &a.cwe_ids,
            &a.affected,
        )?;
        if let Some(s) = req.state {
            let target = SecurityAdvisoryState::try_from(s)
                .map_err(|_| ForgeError::invalid("invalid_state", "unknown advisory state"))?;
            let allowed = match (current, target) {
                (c, t) if c == t => true,
                (
                    SecurityAdvisoryState::Triage,
                    SecurityAdvisoryState::Draft | SecurityAdvisoryState::Closed,
                ) => true,
                (SecurityAdvisoryState::Draft, SecurityAdvisoryState::Closed) => true,
                (SecurityAdvisoryState::Closed, SecurityAdvisoryState::Draft) => true,
                (SecurityAdvisoryState::Published, SecurityAdvisoryState::Withdrawn) => true,
                _ => false,
            };
            if !allowed {
                return Err(ForgeError::invalid_state(format!(
                    "an advisory cannot go from {} to {}",
                    state_label(current),
                    state_label(target)
                )));
            }
            if target != current && !viewer.privileged {
                return Err(ForgeError::permission_denied(
                    "only maintainers can change the state of an advisory",
                ));
            }
            a.state = target as i32;
        }
        let now = self.now();
        a.updated_at = Some(ts(now));
        let new_state = state_of(&a);
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.update(
            &REPO_ADVISORIES,
            &row.clone()
                .k1(state_label(new_state))
                .n1(now as i64)
                .msg(&AdvisoryRec {
                    advisory: Some(a.clone()),
                }),
        )
        .await?;
        // A published advisory's global record follows its edits and its withdrawal.
        let mut osv_after = None;
        if matches!(
            new_state,
            SecurityAdvisoryState::Published | SecurityAdvisoryState::Withdrawn
        ) {
            let mut osv = osv_of_repo_advisory(
                &a.ghsa_id,
                &a.cve_id,
                &a.summary,
                &a.description,
                &a.cvss_vector,
                &a.cwe_ids,
                &a.affected,
                a.published_at
                    .as_ref()
                    .map(crate::ids::ts_ms)
                    .unwrap_or(now),
            )?;
            osv.modified = Some(super::rfc3339(now));
            if new_state == SecurityAdvisoryState::Withdrawn {
                osv.withdrawn = Some(super::rfc3339(now));
            }
            let mut g = to_proto(&osv);
            g.source_repo = repo_ref_of(repo);
            g.r#type = AdvisoryType::Reviewed as i32;
            tx.put(
                &GLOBAL_ADVISORIES,
                &Row::new("", &a.ghsa_id).n1(now as i64).msg(&GlobalAdvRec {
                    advisory: Some(g),
                    osv_json: serde_json::to_string(&osv).unwrap_or_default(),
                }),
            )
            .await?;
            osv_after = Some(osv);
        }
        record(
            &mut tx,
            Change {
                repo,
                event: "security_advisory.updated",
                action: if new_state == SecurityAdvisoryState::Withdrawn {
                    "security.advisory.withdraw"
                } else {
                    "security.advisory.update"
                },
                subject: SubjectKind::Advisory,
                id: &a.ghsa_id,
                payload: serde_json::to_value(&a).unwrap_or_default(),
                before,
                after: json_of(&a),
            },
        )
        .await?;
        tx.commit().await?;
        if let Some(osv) = osv_after {
            self.index_published(cx, osv).await?;
        }
        Ok(a)
    }

    /// `PublishSecurityAdvisory`: a draft goes to the global database.
    pub async fn publish_advisory(
        &self,
        cx: &Cx,
        repo: &Repo,
        ghsa: &str,
        viewer: &Viewer,
    ) -> Result<SecurityRepoAdvisory> {
        let (mut a, row) = self.advisory_row(cx, repo, ghsa, viewer).await?;
        if state_of(&a) != SecurityAdvisoryState::Draft {
            return Err(ForgeError::invalid_state(format!(
                "only a draft can be published; this advisory is {}",
                state_label(state_of(&a))
            )));
        }
        if a.affected.is_empty() {
            return Err(ForgeError::invalid(
                "missing_field",
                "an advisory needs at least one affected package to be published",
            ));
        }
        if a.description.trim().is_empty() {
            return Err(ForgeError::missing("description"));
        }
        let before = json_of(&a);
        let now = self.now();
        a.state = SecurityAdvisoryState::Published as i32;
        a.published_at = Some(ts(now));
        a.updated_at = Some(ts(now));
        let osv = osv_of_repo_advisory(
            &a.ghsa_id,
            &a.cve_id,
            &a.summary,
            &a.description,
            &a.cvss_vector,
            &a.cwe_ids,
            &a.affected,
            now,
        )?;
        let mut g = to_proto(&osv);
        g.source_repo = repo_ref_of(repo);
        g.r#type = AdvisoryType::Reviewed as i32;
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.update(
            &REPO_ADVISORIES,
            &row.clone()
                .k1("published")
                .n1(now as i64)
                .msg(&AdvisoryRec {
                    advisory: Some(a.clone()),
                }),
        )
        .await?;
        tx.put(
            &GLOBAL_ADVISORIES,
            &Row::new("", &a.ghsa_id).n1(now as i64).msg(&GlobalAdvRec {
                advisory: Some(g),
                osv_json: serde_json::to_string(&osv).unwrap_or_default(),
            }),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "security_advisory.published",
                action: "security.advisory.publish",
                subject: SubjectKind::Advisory,
                id: &a.ghsa_id,
                payload: serde_json::to_value(&a).unwrap_or_default(),
                before,
                after: json_of(&a),
            },
        )
        .await?;
        tx.commit().await?;
        self.index_published(cx, osv).await?;
        Ok(a)
    }

    /// `RequestAdvisoryCve`: records a request for a CNA to fulfil.
    pub async fn request_cve(
        &self,
        cx: &Cx,
        repo: &Repo,
        ghsa: &str,
        viewer: &Viewer,
    ) -> Result<SecurityRepoAdvisory> {
        let (mut a, row) = self.advisory_row(cx, repo, ghsa, viewer).await?;
        if state_of(&a) != SecurityAdvisoryState::Draft {
            return Err(ForgeError::invalid_state(
                "a CVE can be requested for a draft only",
            ));
        }
        if a.cve_requested || !a.cve_id.is_empty() {
            return Err(ForgeError::already_exists(format!(
                "a CVE request for {ghsa}"
            )));
        }
        let before = json_of(&a);
        let now = self.now();
        a.cve_requested = true;
        a.updated_at = Some(ts(now));
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let req = CveRequestRec {
            id: tx.new_id()?,
            ghsa_id: ghsa.to_string(),
            repo_id: repo.id.clone(),
            requested_by: cx.actor_id.clone(),
            requested_ms: now,
            status: "pending".into(),
            cve_id: String::new(),
        };
        tx.insert(
            &CVE_REQUESTS,
            &Row::new("", &req.id).k1(ghsa).n1(now as i64).msg(&req),
        )
        .await?;
        tx.update(
            &REPO_ADVISORIES,
            &row.clone().n1(now as i64).msg(&AdvisoryRec {
                advisory: Some(a.clone()),
            }),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "security_advisory.cve_requested",
                action: "security.advisory.cve_request",
                subject: SubjectKind::Advisory,
                id: ghsa,
                payload: serde_json::json!({"ghsa_id": ghsa, "request_id": req.id}),
                before,
                after: json_of(&a),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(a)
    }

    /// Records the CVE id a CNA assigned to a pending request (operator or CNA integration).
    pub async fn fulfil_cve_request(
        &self,
        cx: &Cx,
        repo: &Repo,
        ghsa: &str,
        cve_id: &str,
    ) -> Result<SecurityRepoAdvisory> {
        if !cve_id.starts_with("CVE-") || cve_id.len() < 9 {
            return Err(ForgeError::invalid(
                "invalid_cve",
                "a CVE id looks like CVE-2026-12345",
            ));
        }
        let all = Viewer {
            ids: cx.chain.clone(),
            privileged: true,
        };
        let (mut a, row) = self.advisory_row(cx, repo, ghsa, &all).await?;
        if !a.cve_requested {
            return Err(ForgeError::invalid_state(
                "no CVE was requested for this advisory",
            ));
        }
        let before = json_of(&a);
        let now = self.now();
        a.cve_id = cve_id.to_string();
        a.updated_at = Some(ts(now));
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        for (mut r, rrow) in tx
            .scan_msgs::<CveRequestRec>(
                &CVE_REQUESTS,
                &Scan::scope("", 100).key(Key::K1(ghsa.to_string())),
            )
            .await?
        {
            r.status = "assigned".into();
            r.cve_id = cve_id.to_string();
            tx.update(&CVE_REQUESTS, &rrow.clone().msg(&r)).await?;
        }
        tx.update(
            &REPO_ADVISORIES,
            &row.clone().n1(now as i64).msg(&AdvisoryRec {
                advisory: Some(a.clone()),
            }),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "security_advisory.updated",
                action: "security.advisory.cve_assign",
                subject: SubjectKind::Advisory,
                id: ghsa,
                payload: serde_json::json!({"ghsa_id": ghsa, "cve_id": cve_id}),
                before,
                after: json_of(&a),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(a)
    }

    /// `AddSecurityAdvisoryCollaborator`.
    pub async fn add_advisory_collaborator(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &AddSecurityAdvisoryCollaboratorRequest,
        viewer: &Viewer,
    ) -> Result<SecurityRepoAdvisory> {
        let mut c = req
            .collaborator
            .clone()
            .ok_or_else(|| ForgeError::missing("collaborator"))?;
        check_credit(&c)?;
        if c.role.is_empty() {
            c.role = "collaborator".into();
        }
        if !c.principal_id.is_empty() && self.directory.get(&c.principal_id).await.is_none() {
            return Err(ForgeError::invalid(
                "unknown_principal",
                format!("principal {} does not exist", c.principal_id),
            ));
        }
        let (mut a, row) = self.advisory_row(cx, repo, &req.ghsa_id, viewer).await?;
        if !matches!(
            state_of(&a),
            SecurityAdvisoryState::Draft | SecurityAdvisoryState::Triage
        ) {
            return Err(ForgeError::invalid_state("collaborators join drafts only"));
        }
        if a.collaborators
            .iter()
            .any(|x| x.principal_id == c.principal_id && x.team_id == c.team_id)
        {
            return Err(ForgeError::already_exists("this collaborator"));
        }
        let before = json_of(&a);
        let now = self.now();
        a.collaborators.push(c);
        a.updated_at = Some(ts(now));
        self.store_collab_change(
            cx,
            repo,
            &row,
            &a,
            before,
            "security.advisory.collaborator_add",
            now,
        )
        .await?;
        Ok(a)
    }

    /// `RemoveSecurityAdvisoryCollaborator`.
    pub async fn remove_advisory_collaborator(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &RemoveSecurityAdvisoryCollaboratorRequest,
        viewer: &Viewer,
    ) -> Result<SecurityRepoAdvisory> {
        if req.principal_id.is_empty() == req.team_id.is_empty() {
            return Err(ForgeError::invalid(
                "invalid_collaborator",
                "name exactly one of principal_id and team_id",
            ));
        }
        let (mut a, row) = self.advisory_row(cx, repo, &req.ghsa_id, viewer).await?;
        if !matches!(
            state_of(&a),
            SecurityAdvisoryState::Draft | SecurityAdvisoryState::Triage
        ) {
            return Err(ForgeError::invalid_state("collaborators leave drafts only"));
        }
        let before = json_of(&a);
        let n = a.collaborators.len();
        a.collaborators
            .retain(|x| !(x.principal_id == req.principal_id && x.team_id == req.team_id));
        if a.collaborators.len() == n {
            return Err(ForgeError::not_found("collaborator"));
        }
        let now = self.now();
        a.updated_at = Some(ts(now));
        self.store_collab_change(
            cx,
            repo,
            &row,
            &a,
            before,
            "security.advisory.collaborator_remove",
            now,
        )
        .await?;
        Ok(a)
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_collab_change(
        &self,
        cx: &Cx,
        repo: &Repo,
        row: &Row,
        a: &SecurityRepoAdvisory,
        before: String,
        action: &str,
        now: u64,
    ) -> Result<()> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.update(
            &REPO_ADVISORIES,
            &row.clone().n1(now as i64).msg(&AdvisoryRec {
                advisory: Some(a.clone()),
            }),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "security_advisory.collaborator_changed",
                action,
                subject: SubjectKind::Advisory,
                id: &a.ghsa_id,
                payload: serde_json::to_value(a).unwrap_or_default(),
                before,
                after: json_of(a),
            },
        )
        .await?;
        tx.commit().await
    }
}
