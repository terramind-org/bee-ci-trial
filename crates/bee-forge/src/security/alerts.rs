//! Vulnerability alerts: matching the default branch's dependency graph against the advisory
//! index, alert states, dismissal and reopening.
//!
//! An alert is identified by `<purl>|<manifest>|<advisory id>`. Evaluation is idempotent: it
//! opens alerts for new findings, reopens a fixed alert whose finding returned, marks an open
//! alert fixed when its finding is gone, and auto-dismisses it when its advisory was withdrawn.
//! Dismissed alerts stay dismissed.

use std::collections::{BTreeMap, BTreeSet};

use bee_deps::DependencyGraph;
use bee_proto::v1::{
    Dependency, DependencyAlert, DependencyAlertDismissReason, DependencyAlertState,
    DependencyScope, ListDependencyAlertsRequest, Repo, SubjectKind, UpdateDependencyAlertRequest,
};

use super::depgraph::pkg_to_proto;
use super::model::DepAlertRec;
use super::{Change, DEP_ALERTS, Security, num_id, page_slice, record, repo_ref_of, sev, sev_of};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Row, Scan, Surface};

/// What an evaluation changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvalReport {
    /// Alert numbers opened.
    pub opened: Vec<u64>,
    /// Alert numbers marked fixed.
    pub fixed: Vec<u64>,
    /// Alert numbers reopened.
    pub reopened: Vec<u64>,
    /// Alert numbers auto-dismissed.
    pub auto_dismissed: Vec<u64>,
}

pub(crate) fn state_name(s: DependencyAlertState) -> &'static str {
    match s {
        DependencyAlertState::Open => "open",
        DependencyAlertState::Dismissed => "dismissed",
        DependencyAlertState::Fixed => "fixed",
        DependencyAlertState::AutoDismissed => "auto_dismissed",
        DependencyAlertState::Unspecified => "unspecified",
    }
}

fn state_of(a: &DependencyAlert) -> DependencyAlertState {
    DependencyAlertState::try_from(a.state).unwrap_or(DependencyAlertState::Unspecified)
}

fn json_of(a: &DependencyAlert) -> String {
    serde_json::json!({
        "number": a.number,
        "state": state_name(state_of(a)),
        "severity": sev_of(a.severity).as_str_name(),
        "package": a.dependency.as_ref().map(|d| d.package_url.clone()),
        "advisory": a.advisory.as_ref().map(|d| d.ghsa_id.clone()),
        "update_change_request": a.update_change_request,
    })
    .to_string()
}

impl Security {
    /// Evaluates `graph` (the default branch's) and updates the repo's alerts.
    pub async fn evaluate_graph(
        &self,
        cx: &Cx,
        repo: &Repo,
        graph: &DependencyGraph,
    ) -> Result<EvalReport> {
        let findings = self.findings_of(graph)?;
        let now = self.now();
        let mut report = EvalReport::default();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let rows = tx
            .scan_msgs::<DepAlertRec>(&DEP_ALERTS, &Scan::scope(&repo.id, crate::store::MAX_SCAN))
            .await?;
        let mut existing: BTreeMap<String, (DepAlertRec, Row)> = rows
            .into_iter()
            .map(|(r, row)| (r.key.clone(), (r, row)))
            .collect();
        let mut seen = BTreeSet::new();
        for f in &findings {
            let key = format!("{}|{}|{}", f.purl, f.manifest_path, f.advisory_id);
            seen.insert(key.clone());
            let dep = graph
                .find_by_purl(&f.purl)
                .find(|p| p.source_path == f.manifest_path)
                .map(|p| pkg_to_proto(p, false))
                .unwrap_or_else(|| Dependency {
                    package_url: f.purl.clone(),
                    ecosystem: f.ecosystem.proto_name().to_string(),
                    name: f.package.clone(),
                    version: f.installed_version.clone(),
                    manifest_path: f.manifest_path.clone(),
                    ..Default::default()
                });
            match existing.remove(&key) {
                None => {
                    let number = super::next_number(&mut tx, &repo.id, "dependency_alert").await?;
                    let advisory = self.advisory_proto(&f.advisory_id);
                    let alert = DependencyAlert {
                        number,
                        repo: repo_ref_of(repo),
                        state: DependencyAlertState::Open as i32,
                        dependency: Some(dep),
                        advisory,
                        severity: sev(f.severity) as i32,
                        created_at: Some(ts(now)),
                        updated_at: Some(ts(now)),
                        url: format!("/{}/security/dependabot/{number}", repo.full_name),
                        ..Default::default()
                    };
                    let rec = DepAlertRec {
                        alert: Some(alert.clone()),
                        fixed_versions: f.fixed_versions.clone(),
                        dependency_path: f.dependency_path.clone(),
                        key: key.clone(),
                        advisory_id: f.advisory_id.clone(),
                        aliases: f.aliases.clone(),
                        update_cr_id: String::new(),
                    };
                    tx.insert(
                        &DEP_ALERTS,
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
                            event: "security_alert.created",
                            action: "security.dependency_alert.open",
                            subject: SubjectKind::SecurityAlert,
                            id: &number.to_string(),
                            payload: serde_json::to_value(&alert).unwrap_or_default(),
                            before: String::new(),
                            after: json_of(&alert),
                        },
                    )
                    .await?;
                    report.opened.push(number);
                }
                Some((mut rec, row)) => {
                    let Some(mut alert) = rec.alert.clone() else {
                        continue;
                    };
                    let mut dirty = false;
                    if rec.fixed_versions != f.fixed_versions {
                        rec.fixed_versions = f.fixed_versions.clone();
                        dirty = true;
                    }
                    if rec.dependency_path != f.dependency_path {
                        rec.dependency_path = f.dependency_path.clone();
                        dirty = true;
                    }
                    let reopened = state_of(&alert) == DependencyAlertState::Fixed;
                    if reopened {
                        let before = json_of(&alert);
                        alert.state = DependencyAlertState::Open as i32;
                        alert.fixed_at = None;
                        alert.dependency = Some(dep);
                        alert.updated_at = Some(ts(now));
                        alert.update_change_request.clear();
                        rec.update_cr_id.clear();
                        rec.alert = Some(alert.clone());
                        tx.update(
                            &DEP_ALERTS,
                            &row.clone().k2("open").n1(now as i64).msg(&rec),
                        )
                        .await?;
                        record(
                            &mut tx,
                            Change {
                                repo,
                                event: "security_alert.reopened",
                                action: "security.dependency_alert.reopen",
                                subject: SubjectKind::SecurityAlert,
                                id: &alert.number.to_string(),
                                payload: serde_json::to_value(&alert).unwrap_or_default(),
                                before,
                                after: json_of(&alert),
                            },
                        )
                        .await?;
                        report.reopened.push(alert.number);
                    } else if dirty {
                        rec.alert = Some(alert);
                        tx.update(&DEP_ALERTS, &row.clone().msg(&rec)).await?;
                    }
                }
            }
        }
        for (key, (mut rec, row)) in existing {
            if seen.contains(&key) {
                continue;
            }
            let Some(mut alert) = rec.alert.clone() else {
                continue;
            };
            if state_of(&alert) != DependencyAlertState::Open {
                continue;
            }
            let before = json_of(&alert);
            alert.updated_at = Some(ts(now));
            let (kind, action, label, bucket) = if self.advisory_gone(&rec.advisory_id) {
                alert.state = DependencyAlertState::AutoDismissed as i32;
                alert.dismissed_reason = DependencyAlertDismissReason::Inaccurate as i32;
                alert.dismissed_comment = "the advisory was withdrawn".into();
                alert.dismissed_at = Some(ts(now));
                (
                    "security_alert.auto_dismissed",
                    "security.dependency_alert.auto_dismiss",
                    "auto_dismissed",
                    1,
                )
            } else {
                alert.state = DependencyAlertState::Fixed as i32;
                alert.fixed_at = Some(ts(now));
                (
                    "security_alert.fixed",
                    "security.dependency_alert.fix",
                    "fixed",
                    0,
                )
            };
            rec.alert = Some(alert.clone());
            tx.update(&DEP_ALERTS, &row.clone().k2(label).n1(now as i64).msg(&rec))
                .await?;
            record(
                &mut tx,
                Change {
                    repo,
                    event: kind,
                    action,
                    subject: SubjectKind::SecurityAlert,
                    id: &alert.number.to_string(),
                    payload: serde_json::to_value(&alert).unwrap_or_default(),
                    before,
                    after: json_of(&alert),
                },
            )
            .await?;
            if bucket == 0 {
                report.fixed.push(alert.number);
            } else {
                report.auto_dismissed.push(alert.number);
            }
        }
        tx.commit().await?;
        if !report.opened.is_empty() || !report.reopened.is_empty() {
            self.auto_security_updates(cx, repo).await;
        }
        Ok(report)
    }

    /// All alert records of a repo, newest first.
    pub(crate) async fn dependency_alert_recs(
        &self,
        cx: &Cx,
        repo: &Repo,
    ) -> Result<Vec<(DepAlertRec, Row)>> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let mut rows = tx
            .scan_msgs::<DepAlertRec>(&DEP_ALERTS, &Scan::scope(&repo.id, crate::store::MAX_SCAN))
            .await?;
        rows.reverse();
        Ok(rows)
    }

    /// `ListDependencyAlerts`.
    pub async fn list_dependency_alerts(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &ListDependencyAlertsRequest,
        token: &str,
        size: usize,
    ) -> Result<(Vec<DependencyAlert>, String)> {
        let states: Vec<i32> = if req.states.is_empty() {
            vec![DependencyAlertState::Open as i32]
        } else {
            req.states.clone()
        };
        let alerts: Vec<DependencyAlert> = self
            .dependency_alert_recs(cx, repo)
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.alert)
            .filter(|a| states.contains(&a.state))
            .filter(|a| req.severities.is_empty() || req.severities.contains(&a.severity))
            .filter(|a| {
                let d = a.dependency.clone().unwrap_or_default();
                (req.ecosystem.is_empty() || d.ecosystem == req.ecosystem)
                    && (req.package_name.is_empty() || d.name == req.package_name)
                    && (req.manifest_path.is_empty() || d.manifest_path == req.manifest_path)
                    && (req.scope == DependencyScope::Unspecified as i32 || d.scope == req.scope)
            })
            .collect();
        page_slice(&alerts, token, size)
    }

    /// `GetDependencyAlert`.
    pub async fn get_dependency_alert(
        &self,
        cx: &Cx,
        repo: &Repo,
        number: u64,
    ) -> Result<DependencyAlert> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.get_msg::<DepAlertRec>(&DEP_ALERTS, &repo.id, &num_id(number))
            .await?
            .and_then(|(r, _)| r.alert)
            .ok_or_else(|| ForgeError::not_found(format!("dependency alert #{number}")))
    }

    /// `UpdateDependencyAlert`: dismiss (a reason is required) or reopen.
    pub async fn update_dependency_alert(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &UpdateDependencyAlertRequest,
    ) -> Result<DependencyAlert> {
        let target =
            DependencyAlertState::try_from(req.state).unwrap_or(DependencyAlertState::Unspecified);
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<DepAlertRec>(&DEP_ALERTS, &repo.id, &num_id(req.number))
            .await?
        else {
            return Err(ForgeError::not_found(format!(
                "dependency alert #{}",
                req.number
            )));
        };
        let mut alert = rec
            .alert
            .clone()
            .ok_or_else(|| ForgeError::internal("alert record without alert"))?;
        let current = state_of(&alert);
        let before = json_of(&alert);
        let (event, action) = match target {
            DependencyAlertState::Dismissed => {
                if req.dismissed_reason == DependencyAlertDismissReason::Unspecified as i32
                    || DependencyAlertDismissReason::try_from(req.dismissed_reason).is_err()
                {
                    return Err(ForgeError::invalid(
                        "missing_reason",
                        "dismissing an alert needs a dismissed_reason",
                    ));
                }
                if current == DependencyAlertState::Fixed {
                    return Err(ForgeError::invalid_state(
                        "a fixed alert cannot be dismissed",
                    ));
                }
                alert.state = DependencyAlertState::Dismissed as i32;
                alert.dismissed_reason = req.dismissed_reason;
                alert.dismissed_comment = req.dismissed_comment.clone();
                alert.dismissed_by = Some(cx.principal());
                alert.dismissed_at = Some(ts(now));
                (
                    "security_alert.dismissed",
                    "security.dependency_alert.dismiss",
                )
            }
            DependencyAlertState::Open => {
                if current == DependencyAlertState::Fixed {
                    return Err(ForgeError::invalid_state(
                        "a fixed alert cannot be reopened",
                    ));
                }
                alert.state = DependencyAlertState::Open as i32;
                alert.dismissed_reason = 0;
                alert.dismissed_comment.clear();
                alert.dismissed_by = None;
                alert.dismissed_at = None;
                (
                    "security_alert.reopened",
                    "security.dependency_alert.reopen",
                )
            }
            _ => {
                return Err(ForgeError::invalid(
                    "invalid_state",
                    "state must be OPEN or DISMISSED",
                ));
            }
        };
        alert.updated_at = Some(ts(now));
        rec.alert = Some(alert.clone());
        let label = state_name(state_of(&alert));
        tx.update(&DEP_ALERTS, &row.clone().k2(label).n1(now as i64).msg(&rec))
            .await?;
        record(
            &mut tx,
            Change {
                repo,
                event,
                action,
                subject: SubjectKind::SecurityAlert,
                id: &alert.number.to_string(),
                payload: serde_json::to_value(&alert).unwrap_or_default(),
                before,
                after: json_of(&alert),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(alert)
    }
}
