//! Per-repo security configuration, the org overview and campaigns.

use std::collections::BTreeMap;

use bee_proto::v1::{
    CodeQualityFindingState, CodeScanningAlertState, CreateSecurityCampaignRequest,
    DependencyAlertState, GetSecurityOverviewRequest, Repo, SecretScanningAlertState,
    SecurityAlertCounts, SecurityAlertFilter, SecurityAlertKind, SecurityCampaign,
    SecurityCampaignProgress, SecurityCampaignState, SecurityConfiguration, SecurityOverview,
    SecurityOverviewRepo, SecurityOverviewTool, SecuritySeverity, SubjectKind,
    UpdateSecurityCampaignRequest, UpdateSecurityConfigurationRequest,
};

use super::model::{CodeAlertRec, DepAlertRec, QualityRec, SecretAlertRec};
use super::{
    CAMPAIGNS, CODE_ALERTS, CONFIG, Change, DEP_ALERTS, QUALITY, SECRET_ALERTS, Security,
    page_slice, record, record_org, sev_of,
};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::store::{Row, Scan, Surface};

/// Alert state as the overview counts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewState {
    Open,
    Fixed,
    Dismissed,
}

/// One alert reduced to what counting needs.
#[derive(Clone, Debug)]
pub(crate) struct View {
    pub kind: SecurityAlertKind,
    pub severity: SecuritySeverity,
    pub state: ViewState,
    pub tool: String,
    pub rule: String,
    pub created_ms: u64,
    pub closed_ms: u64,
    pub has_cr: bool,
    pub text: String,
}

fn default_config(repo: &Repo) -> SecurityConfiguration {
    SecurityConfiguration {
        repo: super::repo_ref_of(repo),
        dependency_graph: true,
        dependency_alerts: true,
        secret_scanning: true,
        secret_scanning_push_protection: true,
        ..Default::default()
    }
}

fn counts_of(views: &[&View], window: (u64, u64)) -> SecurityAlertCounts {
    let mut c = SecurityAlertCounts::default();
    for v in views {
        match v.state {
            ViewState::Open => {
                c.total_open += 1;
                match v.severity {
                    SecuritySeverity::Critical => c.critical += 1,
                    SecuritySeverity::High => c.high += 1,
                    SecuritySeverity::Medium => c.medium += 1,
                    SecuritySeverity::Low => c.low += 1,
                    SecuritySeverity::Unspecified => {}
                }
            }
            ViewState::Fixed if in_window(v.closed_ms, window) => c.fixed_in_window += 1,
            ViewState::Dismissed if in_window(v.closed_ms, window) => c.dismissed_in_window += 1,
            _ => {}
        }
    }
    c
}

fn in_window(ms: u64, window: (u64, u64)) -> bool {
    ms >= window.0 && (window.1 == 0 || ms <= window.1)
}

fn add(a: &mut SecurityAlertCounts, b: &SecurityAlertCounts) {
    a.critical += b.critical;
    a.high += b.high;
    a.medium += b.medium;
    a.low += b.low;
    a.total_open += b.total_open;
    a.fixed_in_window += b.fixed_in_window;
    a.dismissed_in_window += b.dismissed_in_window;
}

fn campaign_state_name(s: i32) -> &'static str {
    match SecurityCampaignState::try_from(s).unwrap_or(SecurityCampaignState::Unspecified) {
        SecurityCampaignState::Active => "active",
        SecurityCampaignState::Completed => "completed",
        SecurityCampaignState::Cancelled => "cancelled",
        SecurityCampaignState::Unspecified => "unspecified",
    }
}

impl Security {
    /// The repo's security configuration (defaults when never changed).
    pub async fn config_of(&self, cx: &Cx, repo: &Repo) -> Result<SecurityConfiguration> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        Ok(tx
            .get_msg::<SecurityConfiguration>(&CONFIG, &repo.id, "config")
            .await?
            .map(|(c, _)| c)
            .unwrap_or_else(|| default_config(repo)))
    }

    /// `UpdateSecurityConfiguration`: unset fields are unchanged.
    pub async fn update_config(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &UpdateSecurityConfigurationRequest,
    ) -> Result<SecurityConfiguration> {
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let existing = tx
            .get_msg::<SecurityConfiguration>(&CONFIG, &repo.id, "config")
            .await?;
        let (mut cfg, version) = match existing {
            Some((c, row)) => (c, row.version),
            None => (default_config(repo), 0),
        };
        let security_updates_were_enabled = cfg.security_updates;
        let before = serde_json::to_string(&cfg).unwrap_or_default();
        macro_rules! set {
            ($($f:ident),*) => { $( if let Some(v) = req.$f { cfg.$f = v; } )* };
        }
        set!(
            dependency_graph,
            dependency_alerts,
            security_updates,
            code_scanning_default_setup,
            secret_scanning,
            secret_scanning_push_protection,
            secret_scanning_validity_checks,
            private_vulnerability_reporting,
            code_quality,
            dependency_review_required
        );
        if cfg.secret_scanning_push_protection && !cfg.secret_scanning {
            return Err(ForgeError::invalid(
                "invalid_setting",
                "push protection needs secret scanning",
            ));
        }
        if cfg.dependency_alerts && !cfg.dependency_graph {
            return Err(ForgeError::invalid(
                "invalid_setting",
                "dependency alerts need the dependency graph",
            ));
        }
        if cfg.security_updates && !cfg.dependency_alerts {
            return Err(ForgeError::invalid(
                "invalid_setting",
                "security updates need dependency alerts",
            ));
        }
        cfg.repo = super::repo_ref_of(repo);
        cfg.updated_at = Some(ts(now));
        let row = Row::new(&repo.id, "config").k1("config").msg(&cfg);
        if version == 0 {
            tx.insert(&CONFIG, &row).await?;
        } else {
            tx.update(&CONFIG, &row.version(version)).await?;
        }
        record(
            &mut tx,
            Change {
                repo,
                event: "security_config.updated",
                action: "security.config.update",
                subject: SubjectKind::Repo,
                id: &repo.id,
                payload: serde_json::to_value(&cfg).unwrap_or_default(),
                before,
                after: serde_json::to_string(&cfg).unwrap_or_default(),
            },
        )
        .await?;
        tx.commit().await?;
        // Turning on the graph or alerts evaluates the default branch now.
        if (req.dependency_graph == Some(true) || req.dependency_alerts == Some(true))
            && let Ok(tip) = self.ports.vcs.resolve(repo, "").await
        {
            self.force_graph_evaluation(cx, repo, &tip).await?;
        }
        // Alerts may predate this switch. Reconcile after committing the setting so either
        // ordering (alert first or switch first) reaches the same update-run path.
        if !security_updates_were_enabled && cfg.security_updates {
            self.auto_security_updates(cx, repo).await;
        }
        Ok(cfg)
    }

    /// The repo's security policy: the first of `SECURITY.md`, `.github/SECURITY.md` and
    /// `docs/SECURITY.md` on the default branch, as (path, text).
    pub async fn security_policy(&self, repo: &Repo) -> Result<Option<(String, String)>> {
        let tip = self.ports.vcs.resolve(repo, "").await?;
        for p in ["SECURITY.md", ".github/SECURITY.md", "docs/SECURITY.md"] {
            if let Some(b) = self.ports.vcs.read_file(repo, &tip, p).await? {
                return Ok(Some((
                    p.to_string(),
                    String::from_utf8_lossy(&b).into_owned(),
                )));
            }
        }
        Ok(None)
    }

    /// Every alert of a repo as counting views.
    pub(crate) async fn alert_views(&self, cx: &Cx, repo: &Repo) -> Result<Vec<View>> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let mut out = Vec::new();
        let scan = || Scan::scope(&repo.id, crate::store::MAX_SCAN);
        for (r, _) in tx.scan_msgs::<DepAlertRec>(&DEP_ALERTS, &scan()).await? {
            let Some(a) = r.alert else { continue };
            let (state, closed) = match DependencyAlertState::try_from(a.state)
                .unwrap_or(DependencyAlertState::Unspecified)
            {
                DependencyAlertState::Open => (ViewState::Open, 0),
                DependencyAlertState::Fixed => (
                    ViewState::Fixed,
                    a.fixed_at.as_ref().map(ts_ms).unwrap_or(0),
                ),
                _ => (
                    ViewState::Dismissed,
                    a.dismissed_at.as_ref().map(ts_ms).unwrap_or(0),
                ),
            };
            out.push(View {
                kind: SecurityAlertKind::Dependency,
                severity: sev_of(a.severity),
                state,
                tool: "dependency-graph".into(),
                rule: a
                    .advisory
                    .as_ref()
                    .map(|d| d.ghsa_id.clone())
                    .unwrap_or_default(),
                created_ms: a.created_at.as_ref().map(ts_ms).unwrap_or(0),
                closed_ms: closed,
                has_cr: !a.update_change_request.is_empty(),
                text: format!(
                    "{} {}",
                    a.dependency
                        .as_ref()
                        .map(|d| d.name.clone())
                        .unwrap_or_default(),
                    a.advisory
                        .as_ref()
                        .map(|d| d.summary.clone())
                        .unwrap_or_default()
                ),
            });
        }
        for (r, _) in tx.scan_msgs::<CodeAlertRec>(&CODE_ALERTS, &scan()).await? {
            let Some(a) = r.alert else { continue };
            let rule = a.rule.clone().unwrap_or_default();
            let (state, closed) = match CodeScanningAlertState::try_from(a.state)
                .unwrap_or(CodeScanningAlertState::Unspecified)
            {
                CodeScanningAlertState::Open => (ViewState::Open, 0),
                CodeScanningAlertState::Fixed => (
                    ViewState::Fixed,
                    a.fixed_at.as_ref().map(ts_ms).unwrap_or(0),
                ),
                _ => (
                    ViewState::Dismissed,
                    a.dismissed_at.as_ref().map(ts_ms).unwrap_or(0),
                ),
            };
            out.push(View {
                kind: SecurityAlertKind::CodeScanning,
                severity: sev_of(rule.security_severity),
                state,
                tool: a.tool_name.clone(),
                rule: rule.id.clone(),
                created_ms: a.created_at.as_ref().map(ts_ms).unwrap_or(0),
                closed_ms: closed,
                has_cr: !a.fix_change_request.is_empty(),
                text: format!("{} {}", rule.id, rule.description),
            });
        }
        for (r, _) in tx
            .scan_msgs::<SecretAlertRec>(&SECRET_ALERTS, &scan())
            .await?
        {
            let Some(a) = r.alert else { continue };
            let open = a.state == SecretScanningAlertState::Open as i32;
            out.push(View {
                kind: SecurityAlertKind::SecretScanning,
                severity: sev_of(a.severity),
                state: if open {
                    ViewState::Open
                } else {
                    ViewState::Dismissed
                },
                tool: "secret-scanning".into(),
                rule: a.secret_type.clone(),
                created_ms: a.created_at.as_ref().map(ts_ms).unwrap_or(0),
                closed_ms: a.resolved_at.as_ref().map(ts_ms).unwrap_or(0),
                has_cr: false,
                text: a.secret_type_display_name.clone(),
            });
        }
        for (r, _) in tx.scan_msgs::<QualityRec>(&QUALITY, &scan()).await? {
            let Some(f) = r.finding else { continue };
            let state = match CodeQualityFindingState::try_from(f.state)
                .unwrap_or(CodeQualityFindingState::Unspecified)
            {
                CodeQualityFindingState::Open => ViewState::Open,
                CodeQualityFindingState::Fixed => ViewState::Fixed,
                _ => ViewState::Dismissed,
            };
            out.push(View {
                kind: SecurityAlertKind::CodeQuality,
                severity: sev_of(f.severity),
                state,
                tool: f.tool_name.clone(),
                rule: f.rule_id.clone(),
                created_ms: f.created_at.as_ref().map(ts_ms).unwrap_or(0),
                closed_ms: 0,
                has_cr: false,
                text: f.message.clone(),
            });
        }
        Ok(out)
    }

    pub(crate) async fn org_repos(&self, cx: &Cx, org_id: &str) -> Result<Vec<Repo>> {
        let mut out = Vec::new();
        let mut after = String::new();
        loop {
            let (page, next) = self.orgs.list_repos(cx, org_id, &after, 200).await?;
            out.extend(page);
            if next.is_empty() {
                break;
            }
            after = next;
        }
        out.sort_by(|a, b| a.full_name.cmp(&b.full_name));
        Ok(out)
    }

    /// `GetSecurityOverview`.
    pub async fn overview(
        &self,
        cx: &Cx,
        org_id: &str,
        org_name: &str,
        req: &GetSecurityOverviewRequest,
        token: &str,
        size: usize,
    ) -> Result<SecurityOverview> {
        let kinds: Vec<i32> = if req.kinds.is_empty() {
            vec![
                SecurityAlertKind::Dependency as i32,
                SecurityAlertKind::CodeScanning as i32,
                SecurityAlertKind::SecretScanning as i32,
                SecurityAlertKind::CodeQuality as i32,
            ]
        } else {
            req.kinds.clone()
        };
        let window = (
            req.window_start.as_ref().map(ts_ms).unwrap_or(0),
            req.window_end.as_ref().map(ts_ms).unwrap_or(0),
        );
        let repos: Vec<Repo> = self
            .org_repos(cx, org_id)
            .await?
            .into_iter()
            .filter(|r| r.deleted_at.is_none())
            .filter(|r| repo_matches(r, &req.repo_query))
            .collect();
        let mut totals = SecurityAlertCounts::default();
        let mut per_repo: Vec<SecurityOverviewRepo> = Vec::new();
        let mut tools: BTreeMap<(i32, String), Vec<View>> = BTreeMap::new();
        let (mut with_dep, mut with_code, mut with_secret, mut with_push) =
            (0u32, 0u32, 0u32, 0u32);
        let mut mttr_sum = 0u128;
        let mut mttr_n = 0u128;
        for repo in &repos {
            let cfg = self.config_of(cx, repo).await?;
            let views: Vec<View> = self
                .alert_views(cx, repo)
                .await?
                .into_iter()
                .filter(|v| kinds.contains(&(v.kind as i32)))
                .collect();
            let refs: Vec<&View> = views.iter().collect();
            let counts = counts_of(&refs, window);
            add(&mut totals, &counts);
            let mut enabled = Vec::new();
            if cfg.dependency_alerts {
                with_dep += 1;
                enabled.push(SecurityAlertKind::Dependency as i32);
            }
            let has_code = cfg.code_scanning_default_setup
                || views
                    .iter()
                    .any(|v| v.kind == SecurityAlertKind::CodeScanning)
                || self.has_analyses(cx, repo).await?;
            if has_code {
                with_code += 1;
                enabled.push(SecurityAlertKind::CodeScanning as i32);
            }
            if cfg.secret_scanning {
                with_secret += 1;
                enabled.push(SecurityAlertKind::SecretScanning as i32);
            }
            if cfg.secret_scanning_push_protection {
                with_push += 1;
            }
            if cfg.code_quality {
                enabled.push(SecurityAlertKind::CodeQuality as i32);
            }
            for v in views {
                if v.state == ViewState::Fixed
                    && in_window(v.closed_ms, window)
                    && v.closed_ms >= v.created_ms
                    && v.created_ms > 0
                {
                    mttr_sum += u128::from((v.closed_ms - v.created_ms) / 1000);
                    mttr_n += 1;
                }
                tools
                    .entry((v.kind as i32, v.tool.clone()))
                    .or_default()
                    .push(v);
            }
            per_repo.push(SecurityOverviewRepo {
                repo: super::repo_ref_of(repo),
                counts: Some(counts),
                enabled_features: enabled,
            });
        }
        let tool_rows: Vec<SecurityOverviewTool> = tools
            .into_iter()
            .map(|((kind, tool), vs)| {
                let refs: Vec<&View> = vs.iter().collect();
                SecurityOverviewTool {
                    kind,
                    tool_name: tool,
                    counts: Some(counts_of(&refs, window)),
                }
            })
            .collect();
        let (page, next) = page_slice(&per_repo, token, size)?;
        Ok(SecurityOverview {
            org: org_name.to_string(),
            totals: Some(totals),
            repos: page,
            tools: tool_rows,
            repos_total: repos.len() as u32,
            repos_with_dependency_alerts_enabled: with_dep,
            repos_with_code_scanning_enabled: with_code,
            repos_with_secret_scanning_enabled: with_secret,
            repos_with_push_protection_enabled: with_push,
            mean_time_to_remediate_seconds: mttr_sum.checked_div(mttr_n).unwrap_or(0) as u64,
            page: crate::service::next(next),
        })
    }

    // ---- campaigns --------------------------------------------------------------------

    async fn campaign_progress(
        &self,
        cx: &Cx,
        org_id: &str,
        filter: &SecurityAlertFilter,
        agent_runs: u64,
    ) -> Result<SecurityCampaignProgress> {
        let mut p = SecurityCampaignProgress {
            agent_runs,
            ..Default::default()
        };
        for repo in self.org_repos(cx, org_id).await? {
            if !filter.repo_ids.is_empty() && !filter.repo_ids.contains(&repo.id) {
                continue;
            }
            for v in self.alert_views(cx, &repo).await? {
                if !view_matches(&v, filter) {
                    continue;
                }
                p.total_alerts += 1;
                match v.state {
                    ViewState::Open => {
                        p.open_alerts += 1;
                        if v.has_cr {
                            p.open_change_requests += 1;
                        }
                    }
                    ViewState::Fixed => p.fixed_alerts += 1,
                    ViewState::Dismissed => p.dismissed_alerts += 1,
                }
            }
        }
        Ok(p)
    }

    /// `CreateSecurityCampaign`.
    pub async fn create_campaign(
        &self,
        cx: &Cx,
        org_id: &str,
        org_name: &str,
        req: &CreateSecurityCampaignRequest,
    ) -> Result<SecurityCampaign> {
        if req.name.trim().is_empty() {
            return Err(ForgeError::missing("name"));
        }
        let filter = req
            .filter
            .clone()
            .ok_or_else(|| ForgeError::missing("filter"))?;
        let now = self.now();
        let mut agent_runs = 0u64;
        if req.start_agent_runs
            && !req.assignee_principal_ids.is_empty()
            && filter.kind == SecurityAlertKind::CodeScanning as i32
        {
            for repo in self.org_repos(cx, org_id).await? {
                if !filter.repo_ids.is_empty() && !filter.repo_ids.contains(&repo.id) {
                    continue;
                }
                for (rec, _) in self.code_alert_recs(cx, &repo).await? {
                    let Some(a) = rec.alert else { continue };
                    let rule = a.rule.clone().unwrap_or_default();
                    let v = View {
                        kind: SecurityAlertKind::CodeScanning,
                        severity: sev_of(rule.security_severity),
                        state: if a.state == CodeScanningAlertState::Open as i32 {
                            ViewState::Open
                        } else {
                            ViewState::Dismissed
                        },
                        tool: a.tool_name.clone(),
                        rule: rule.id.clone(),
                        created_ms: 0,
                        closed_ms: 0,
                        has_cr: false,
                        text: rule.id.clone(),
                    };
                    if v.state == ViewState::Open && view_matches(&v, &filter) {
                        self.ports
                            .autofix
                            .start(&repo, a.number, &rule.id, &req.assignee_principal_ids[0])
                            .await?;
                        agent_runs += 1;
                    }
                }
            }
        }
        let mut c = SecurityCampaign {
            id: String::new(),
            org: org_name.to_string(),
            name: req.name.trim().to_string(),
            description: req.description.clone(),
            filter: Some(filter.clone()),
            assignee_principal_ids: req.assignee_principal_ids.clone(),
            assignee_team_ids: req.assignee_team_ids.clone(),
            state: SecurityCampaignState::Active as i32,
            progress: None,
            due_at: req.due_at,
            created_by: Some(cx.principal()),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
        };
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        c.id = tx.new_id()?;
        // Names are unique among an org's active campaigns.
        for (other, _) in tx
            .scan_msgs::<SecurityCampaign>(&CAMPAIGNS, &Scan::scope(org_id, crate::store::MAX_SCAN))
            .await?
        {
            if other.name.eq_ignore_ascii_case(&c.name)
                && other.state == SecurityCampaignState::Active as i32
            {
                return Err(ForgeError::already_exists(format!("campaign {}", c.name)));
            }
        }
        tx.insert(
            &CAMPAIGNS,
            &Row::new(org_id, &c.id).k2("active").n1(now as i64).msg(&c),
        )
        .await?;
        record_org(
            &mut tx,
            org_id,
            "security_campaign.created",
            "security.campaign.create",
            SubjectKind::Org,
            &c.id,
            serde_json::to_value(&c).unwrap_or_default(),
            serde_json::json!({"id": c.id, "name": c.name}).to_string(),
        )
        .await?;
        tx.commit().await?;
        c.progress = Some(
            self.campaign_progress(cx, org_id, &filter, agent_runs)
                .await?,
        );
        Ok(c)
    }

    async fn campaign_row(
        &self,
        cx: &Cx,
        org_id: &str,
        id: &str,
    ) -> Result<(SecurityCampaign, Row)> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.get_msg::<SecurityCampaign>(&CAMPAIGNS, org_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("campaign {id}")))
    }

    /// `GetSecurityCampaign`.
    pub async fn get_campaign(&self, cx: &Cx, org_id: &str, id: &str) -> Result<SecurityCampaign> {
        let (mut c, _) = self.campaign_row(cx, org_id, id).await?;
        let runs = c.progress.as_ref().map(|p| p.agent_runs).unwrap_or(0);
        if let Some(f) = c.filter.clone() {
            c.progress = Some(self.campaign_progress(cx, org_id, &f, runs).await?);
        }
        Ok(c)
    }

    /// `ListSecurityCampaigns`.
    pub async fn list_campaigns(
        &self,
        cx: &Cx,
        org_id: &str,
        state: i32,
        token: &str,
        size: usize,
    ) -> Result<(Vec<SecurityCampaign>, String)> {
        let rows = {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            tx.scan_msgs::<SecurityCampaign>(
                &CAMPAIGNS,
                &Scan::scope(org_id, crate::store::MAX_SCAN).by_n1().desc(),
            )
            .await?
        };
        let all: Vec<SecurityCampaign> = rows
            .into_iter()
            .map(|(c, _)| c)
            .filter(|c| state == 0 || c.state == state)
            .collect();
        let (page, next) = page_slice(&all, token, size)?;
        let mut out = Vec::new();
        for mut c in page {
            let runs = c.progress.as_ref().map(|p| p.agent_runs).unwrap_or(0);
            if let Some(f) = c.filter.clone() {
                c.progress = Some(self.campaign_progress(cx, org_id, &f, runs).await?);
            }
            out.push(c);
        }
        Ok((out, next))
    }

    /// `UpdateSecurityCampaign`.
    pub async fn update_campaign(
        &self,
        cx: &Cx,
        org_id: &str,
        req: &UpdateSecurityCampaignRequest,
    ) -> Result<SecurityCampaign> {
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((mut c, row)) = tx
            .get_msg::<SecurityCampaign>(&CAMPAIGNS, org_id, &req.id)
            .await?
        else {
            return Err(ForgeError::not_found(format!("campaign {}", req.id)));
        };
        let before = campaign_state_name(c.state);
        if let Some(n) = &req.name {
            if n.trim().is_empty() {
                return Err(ForgeError::invalid("invalid_name", "name cannot be empty"));
            }
            c.name = n.trim().to_string();
        }
        if let Some(d) = &req.description {
            c.description = d.clone();
        }
        if let Some(s) = req.state {
            let st = SecurityCampaignState::try_from(s)
                .map_err(|_| ForgeError::invalid("invalid_state", "unknown campaign state"))?;
            if st == SecurityCampaignState::Unspecified {
                return Err(ForgeError::invalid("invalid_state", "state is required"));
            }
            if c.state != SecurityCampaignState::Active as i32
                && st != SecurityCampaignState::Active
                && c.state != s
            {
                return Err(ForgeError::invalid_state("the campaign is already closed"));
            }
            c.state = s;
        }
        if req.due_at.is_some() {
            c.due_at = req.due_at;
        }
        if req.replace_assignees {
            c.assignee_principal_ids = req.assignee_principal_ids.clone();
            c.assignee_team_ids = req.assignee_team_ids.clone();
        }
        c.updated_at = Some(ts(now));
        let label = campaign_state_name(c.state);
        tx.update(&CAMPAIGNS, &row.clone().k2(label).msg(&c))
            .await?;
        record_org(
            &mut tx,
            org_id,
            "security_campaign.updated",
            "security.campaign.update",
            SubjectKind::Org,
            &c.id,
            serde_json::to_value(&c).unwrap_or_default(),
            serde_json::json!({"id": c.id, "state": label, "was": before}).to_string(),
        )
        .await?;
        tx.commit().await?;
        let runs = c.progress.as_ref().map(|p| p.agent_runs).unwrap_or(0);
        if let Some(f) = c.filter.clone() {
            c.progress = Some(self.campaign_progress(cx, org_id, &f, runs).await?);
        }
        Ok(c)
    }
}

fn repo_matches(r: &Repo, query: &str) -> bool {
    query.split_whitespace().all(|term| {
        if let Some(t) = term.strip_prefix("topic:") {
            r.topics.iter().any(|x| x == t)
        } else if let Some(v) = term.strip_prefix("visibility:") {
            bee_proto::v1::Visibility::try_from(r.visibility)
                .map(|x| x.as_str_name().to_ascii_lowercase().ends_with(v))
                .unwrap_or(false)
        } else if let Some(v) = term.strip_prefix("archived:") {
            r.archived == (v == "true")
        } else {
            let t = term.strip_prefix("name:").unwrap_or(term);
            r.name.to_lowercase().contains(&t.to_lowercase())
        }
    })
}

fn view_matches(v: &View, f: &SecurityAlertFilter) -> bool {
    if f.kind != 0 && v.kind as i32 != f.kind {
        return false;
    }
    if !f.severities.is_empty() && !f.severities.contains(&(v.severity as i32)) {
        return false;
    }
    if !f.tool_name.is_empty() && v.tool != f.tool_name {
        return false;
    }
    if !f.rule_id.is_empty() && v.rule != f.rule_id {
        return false;
    }
    f.query.split_whitespace().all(|term| {
        if let Some(r) = term.strip_prefix("rule:") {
            v.rule == r
        } else if let Some(t) = term.strip_prefix("tool:") {
            v.tool == t
        } else {
            v.text.to_lowercase().contains(&term.to_lowercase())
        }
    })
}
