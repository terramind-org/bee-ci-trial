//! Component watchers ("superfans"), the anomaly feed and automatic agent review.
//!
//! A watcher is an agent scoped to one component: its paths, service and telemetry
//! selectors. Changes to its paths trigger a run after a debounce window
//! ([`super::triggers`]); the agent reports anomalies with evidence, which can open or link
//! issues. Anomalies carry a per-repo sequence that moves on every update, so streams resume
//! with `after_seq`; an open anomaly with the same fingerprint is updated instead of
//! duplicated.

use bee_proto::v1::{
    AgentReviewConfig, AgentReviewTrigger, AgentRunBudget, Anomaly, AnomalyEvidence,
    AnomalySeverity, AnomalyState, Event, Principal, Repo, SubjectKind, SubjectRef, Watcher,
    WatcherState, WatcherTelemetrySelector,
};

use super::model::WatcherRec;
use super::registry::repo_ref_id;
use super::*;
use crate::audit::AuditDraft;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::store::{Key, Row, Scan, Tx};
use crate::work::{Actor, NewIssue, RepoInfo};

/// Most watchers per repo.
pub const MAX_WATCHERS: u64 = 200;
/// Most evidence items kept on one anomaly.
pub const MAX_EVIDENCE: usize = 100;

/// A new watcher.
#[derive(Clone, Debug, Default)]
pub struct NewWatcher {
    /// Name, unique per repo.
    pub name: String,
    /// Component name.
    pub component: String,
    /// Paths the component owns.
    pub paths: Vec<Vec<u8>>,
    /// Service name.
    pub service_name: String,
    /// Telemetry selectors.
    pub telemetry: Option<WatcherTelemetrySelector>,
    /// Runtime id.
    pub runtime_id: String,
    /// The agent that watches.
    pub agent: Principal,
    /// Extra guidance.
    pub instructions: String,
    /// Open issues for anomalies automatically.
    pub auto_open_issues: bool,
    /// Labels of issues it opens.
    pub labels: Vec<String>,
    /// Debounce window and check interval (0 = the module default).
    pub check_interval_seconds: u64,
    /// Budget per day.
    pub budget: Option<AgentRunBudget>,
}

/// A partial watcher update.
#[derive(Clone, Debug, Default)]
pub struct WatcherPatch {
    /// Name.
    pub name: Option<String>,
    /// Paths (non-empty replaces).
    pub paths: Vec<Vec<u8>>,
    /// Service name.
    pub service_name: Option<String>,
    /// Telemetry selectors.
    pub telemetry: Option<WatcherTelemetrySelector>,
    /// State.
    pub state: Option<WatcherState>,
    /// Instructions.
    pub instructions: Option<String>,
    /// Auto-open issues.
    pub auto_open_issues: Option<bool>,
    /// Check interval.
    pub check_interval_seconds: Option<u64>,
}

/// A reported anomaly.
#[derive(Clone, Debug, Default)]
pub struct NewAnomaly {
    /// Watcher.
    pub watcher_id: String,
    /// Title.
    pub title: String,
    /// Description, Markdown.
    pub description: String,
    /// Severity.
    pub severity: AnomalySeverity,
    /// Evidence.
    pub evidence: Vec<AnomalyEvidence>,
    /// When it started (ms; 0 = now).
    pub started_ms: u64,
    /// Dedup key.
    pub fingerprint: String,
}

fn norm_paths(paths: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
    let mut v: Vec<String> = paths
        .iter()
        .map(|p| crate::leases::normalize_path(p))
        .collect::<Result<_>>()?;
    v.sort();
    v.dedup();
    Ok(v.into_iter().map(String::into_bytes).collect())
}

/// Whether a changed path falls under one of the watcher's paths (empty = whole repo).
pub fn watches(w: &Watcher, path: &str) -> bool {
    w.paths.is_empty()
        || w.paths
            .iter()
            .any(|p| crate::leases::covers(&String::from_utf8_lossy(p), path))
}

/// The Markdown body of an issue opened for an anomaly.
pub fn issue_body(a: &Anomaly, w: Option<&Watcher>) -> String {
    let mut s = String::new();
    if !a.description.is_empty() {
        s.push_str(&a.description);
        s.push_str("\n\n");
    }
    let sev = AnomalySeverity::try_from(a.severity)
        .unwrap_or(AnomalySeverity::Unspecified)
        .as_str_name()
        .trim_start_matches("ANOMALY_SEVERITY_")
        .to_lowercase();
    s.push_str(&format!("**Severity:** {sev}\n"));
    if let Some(w) = w {
        s.push_str(&format!(
            "**Component:** {} (watcher `{}`)\n",
            w.component, w.name
        ));
    }
    s.push_str(&format!(
        "**Anomaly:** `{}` (fingerprint `{}`)\n\n### Evidence\n\n",
        a.id, a.fingerprint
    ));
    for e in &a.evidence {
        let labels: Vec<String> = e
            .labels
            .iter()
            .map(|l| format!("{}={}", l.key, l.value))
            .collect();
        let mut line = format!("- {}", if e.kind.is_empty() { "note" } else { &e.kind });
        if !e.metric_name.is_empty() {
            line.push_str(&format!(" `{}`", e.metric_name));
        }
        if !labels.is_empty() {
            line.push_str(&format!(" {{{}}}", labels.join(", ")));
        }
        if e.baseline != 0.0 || e.observed != 0.0 {
            line.push_str(&format!(
                ": baseline {} → observed {}",
                e.baseline, e.observed
            ));
            if e.baseline != 0.0 {
                let pct = (e.observed - e.baseline) / e.baseline * 100.0;
                line.push_str(&format!(" ({pct:+.0}%)"));
            }
        }
        if let Some(cs) = &e.suspect_changeset
            && !cs.raw.is_empty()
        {
            line.push_str(&format!("; suspect changeset `{}`", hex(&cs.raw)));
        }
        if !e.trace_id.is_empty() {
            line.push_str(&format!("; trace `{}`", e.trace_id));
        }
        if !e.log_query.is_empty() {
            line.push_str(&format!("; logs `{}`", e.log_query));
        }
        if !e.note.is_empty() {
            line.push_str(&format!(" — {}", e.note));
        }
        s.push_str(&line);
        s.push('\n');
    }
    s
}

fn watcher_row(rec: &WatcherRec, repo_id: &str, version: u64) -> Row {
    let w = rec.w();
    Row::new(repo_id, w.id.clone())
        .k1(lc(&w.name))
        .n1(w.created_at.as_ref().map(crate::ids::ts_ms).unwrap_or(0) as i64)
        .version(version)
        .msg(rec)
}

fn anomaly_row(a: &Anomaly, repo_id: &str, version: u64) -> Row {
    let open = a.state != AnomalyState::Resolved as i32;
    let mut row = Row::new(repo_id, a.id.clone())
        .k1(a.watcher_id.clone())
        .n1(a.seq as i64)
        .version(version)
        .msg(a);
    if open && !a.fingerprint.is_empty() {
        row = row.k2(a.fingerprint.clone());
    }
    row
}

fn repo_event(kind: &str, repo: &Repo, sk: SubjectKind, id: &str, json: String) -> Event {
    Event {
        kind: kind.into(),
        repo_id: repo.id.clone(),
        org_id: repo.org_id.clone(),
        subject: Some(SubjectRef {
            kind: sk as i32,
            repo: repo_ref_id(&repo.id),
            id: id.to_string(),
        }),
        payload_json: json,
        ..Default::default()
    }
}

impl Agents {
    /// Creates a watcher.
    pub async fn create_watcher(&self, cx: &Cx, repo: &Repo, n: NewWatcher) -> Result<Watcher> {
        let name = check_name("name", &n.name)?;
        check_len("instructions", &n.instructions, 64 * 1024)?;
        if n.labels.len() > 20 {
            return Err(ForgeError::too_large("labels", 20, n.labels.len() as u64));
        }
        let paths = norm_paths(&n.paths)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx
            .scan(
                &WATCHERS,
                &Scan::scope(repo.id.clone(), 1).key(Key::K1(lc(&name))),
            )
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("watcher {name}")));
        }
        if tx.count(&WATCHERS, &repo.id, None).await? >= MAX_WATCHERS {
            return Err(ForgeError::too_large(
                "watchers",
                MAX_WATCHERS,
                MAX_WATCHERS + 1,
            ));
        }
        let now = tx.now_ms();
        let w = Watcher {
            id: tx.new_id()?,
            repo: repo_ref_id(&repo.id),
            name,
            component: n.component.trim().to_string(),
            paths,
            service_name: n.service_name.clone(),
            telemetry: n.telemetry.clone(),
            runtime_id: n.runtime_id.clone(),
            agent: Some(n.agent.clone()),
            state: WatcherState::Active as i32,
            instructions: n.instructions.clone(),
            auto_open_issues: n.auto_open_issues,
            labels: n.labels.clone(),
            check_interval_seconds: n.check_interval_seconds,
            budget: n.budget,
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
        };
        let rec = WatcherRec {
            watcher: Some(w.clone()),
            org_id: repo.org_id.clone(),
            created_by: cx.actor_id.clone(),
            ..Default::default()
        };
        tx.insert(&WATCHERS, &watcher_row(&rec, &repo.id, 0))
            .await?;
        self.record_watcher(&mut tx, repo, &w, "agent.watcher_create")
            .await?;
        tx.commit().await?;
        Ok(w)
    }

    async fn record_watcher(
        &self,
        tx: &mut Tx,
        repo: &Repo,
        w: &Watcher,
        action: &str,
    ) -> Result<()> {
        let json = pjson(w);
        tx.emit_event(repo_event(
            "watcher.updated",
            repo,
            SubjectKind::Repo,
            &w.id,
            json.clone(),
        ))
        .await?;
        tx.audit(
            AuditDraft::new(action, &repo.org_id)
                .repo_id(&repo.id)
                .target_id(&w.id)
                .after(&json),
        )
        .await
    }

    pub(crate) async fn watcher_rec(
        &self,
        cx: &Cx,
        repo_id: &str,
        id: &str,
    ) -> Result<(WatcherRec, Row)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<WatcherRec>(&WATCHERS, repo_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("watcher {id}")))
    }

    /// A watcher.
    pub async fn get_watcher(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Watcher> {
        Ok(self.watcher_rec(cx, &repo.id, id).await?.0.w())
    }

    /// Watchers of a repo, filtered by state, paged by id.
    pub async fn list_watchers(
        &self,
        cx: &Cx,
        repo: &Repo,
        state: i32,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Watcher>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut v: Vec<Watcher> = tx
            .scan_msgs::<WatcherRec>(
                &WATCHERS,
                &Scan::scope(repo.id.clone(), crate::store::MAX_SCAN),
            )
            .await?
            .into_iter()
            .map(|(r, _)| r.w())
            .filter(|w| state == 0 || w.state == state)
            .filter(|w| after.is_empty() || w.id.as_str() > after)
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        let more = v.len() > limit;
        v.truncate(limit);
        let next = if more {
            v.last().map(|w| w.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((v, next))
    }

    /// Updates a watcher.
    pub async fn update_watcher(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        p: WatcherPatch,
    ) -> Result<Watcher> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = tx
            .get_msg::<WatcherRec>(&WATCHERS, &repo.id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("watcher {id}")))?;
        let mut w = rec.w();
        if let Some(n) = p.name {
            let n = check_name("name", &n)?;
            if let Some(other) = tx
                .scan(
                    &WATCHERS,
                    &Scan::scope(repo.id.clone(), 1).key(Key::K1(lc(&n))),
                )
                .await?
                .first()
                && other.id != id
            {
                return Err(ForgeError::already_exists(format!("watcher {n}")));
            }
            w.name = n;
        }
        if !p.paths.is_empty() {
            w.paths = norm_paths(&p.paths)?;
        }
        if let Some(s) = p.service_name {
            w.service_name = s;
        }
        if let Some(t) = p.telemetry {
            w.telemetry = Some(t);
        }
        if let Some(s) = p.state {
            if s == WatcherState::Unspecified {
                return Err(ForgeError::invalid("invalid_state", "state must be set"));
            }
            w.state = s as i32;
        }
        if let Some(i) = p.instructions {
            check_len("instructions", &i, 64 * 1024)?;
            w.instructions = i;
        }
        if let Some(a) = p.auto_open_issues {
            w.auto_open_issues = a;
        }
        if let Some(c) = p.check_interval_seconds {
            w.check_interval_seconds = c;
        }
        w.updated_at = Some(ts(tx.now_ms()));
        rec.watcher = Some(w.clone());
        tx.update(&WATCHERS, &watcher_row(&rec, &repo.id, row.version))
            .await?;
        if w.state != WatcherState::Active as i32 {
            tx.delete(&TRIGGERS, "", id).await?;
        }
        self.record_watcher(&mut tx, repo, &w, "agent.watcher_update")
            .await?;
        tx.commit().await?;
        Ok(w)
    }

    /// Deletes a watcher; its anomalies stay readable.
    pub async fn delete_watcher(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = tx
            .get_msg::<WatcherRec>(&WATCHERS, &repo.id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("watcher {id}")))?;
        tx.delete(&WATCHERS, &repo.id, id).await?;
        tx.delete(&TRIGGERS, "", id).await?;
        self.record_watcher(&mut tx, repo, &rec.w(), "agent.watcher_delete")
            .await?;
        tx.commit().await
    }

    // ---- anomalies ----

    /// Records an anomaly (or updates the open one with the same fingerprint). Opens an issue
    /// with the evidence when the watcher auto-opens issues.
    pub async fn report_anomaly(&self, cx: &Cx, repo: &Repo, n: NewAnomaly) -> Result<Anomaly> {
        let title = check_name("title", &n.title).or_else(|_| {
            if n.title.trim().is_empty() {
                Err(ForgeError::missing("title"))
            } else {
                Ok(n.title.chars().take(256).collect())
            }
        })?;
        check_len("description", &n.description, 64 * 1024)?;
        check_len("fingerprint", &n.fingerprint, 256)?;
        if n.evidence.len() > MAX_EVIDENCE {
            return Err(ForgeError::too_large(
                "evidence",
                MAX_EVIDENCE as u64,
                n.evidence.len() as u64,
            ));
        }
        let (wrec, _) = self.watcher_rec(cx, &repo.id, &n.watcher_id).await?;
        let w = wrec.w();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let seq = tx.add_n1(&COUNTERS, &repo.id, "anomaly_seq", 1).await? as u64;
        let existing = if n.fingerprint.is_empty() {
            None
        } else {
            tx.scan_msgs::<Anomaly>(
                &ANOMALIES,
                &Scan::scope(repo.id.clone(), 10).key(Key::K2(n.fingerprint.clone())),
            )
            .await?
            .into_iter()
            .find(|(a, _)| a.state != AnomalyState::Resolved as i32)
        };
        let (a, kind, created) = match existing {
            Some((mut a, row)) => {
                a.seq = seq;
                a.title = title;
                if !n.description.is_empty() {
                    a.description = n.description.clone();
                }
                a.severity = a.severity.max(n.severity as i32);
                a.evidence.extend(n.evidence.iter().cloned());
                let over = a.evidence.len().saturating_sub(MAX_EVIDENCE);
                a.evidence.drain(..over);
                a.detected_at = Some(ts(now));
                tx.update(&ANOMALIES, &anomaly_row(&a, &repo.id, row.version))
                    .await?;
                (a, "anomaly.updated", false)
            }
            None => {
                let a = Anomaly {
                    id: tx.new_id()?,
                    seq,
                    repo: repo_ref_id(&repo.id),
                    watcher_id: w.id.clone(),
                    title,
                    description: n.description.clone(),
                    severity: n.severity as i32,
                    state: AnomalyState::Open as i32,
                    evidence: n.evidence.clone(),
                    started_at: Some(ts(if n.started_ms == 0 { now } else { n.started_ms })),
                    detected_at: Some(ts(now)),
                    fingerprint: n.fingerprint.clone(),
                    ..Default::default()
                };
                tx.insert(&ANOMALIES, &anomaly_row(&a, &repo.id, 0)).await?;
                (a, "anomaly.reported", true)
            }
        };
        let json = pjson(&a);
        tx.emit_event(repo_event(
            kind,
            repo,
            SubjectKind::Repo,
            &a.id,
            json.clone(),
        ))
        .await?;
        tx.audit(
            AuditDraft::new(
                if created {
                    "agent.anomaly_report"
                } else {
                    "agent.anomaly_update"
                },
                &repo.org_id,
            )
            .repo_id(&repo.id)
            .target_id(&a.id)
            .after(&json),
        )
        .await?;
        tx.commit().await?;
        self.run_done.notify_waiters();
        if w.auto_open_issues && a.issue.is_empty() {
            let agent = w.agent.clone().unwrap_or_default();
            let actor = Actor {
                principal: agent.clone(),
                triage: true,
                admin: false,
                association: "MEMBER".into(),
            };
            let acx = Cx::new(agent.id.clone(), bee_proto::v1::PrincipalKind::Agent)
                .with_chain(self.owner_chain(cx, &agent).await.unwrap_or_default());
            return self
                .link_anomaly(&acx, repo, &actor, &a.id, 0, "", &w.labels)
                .await;
        }
        Ok(a)
    }

    /// Anomalies of a repo (or one watcher), newest first, paged by sequence.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_anomalies(
        &self,
        cx: &Cx,
        repo: &Repo,
        watcher_id: &str,
        states: &[i32],
        min_severity: i32,
        since_ms: u64,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Anomaly>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut scan = Scan::scope(repo.id.clone(), crate::store::MAX_SCAN)
            .by_n1()
            .desc();
        if let Ok(a) = after.parse::<i64>() {
            scan = scan.n1_max(a - 1);
        }
        let mut v: Vec<Anomaly> = tx
            .scan_msgs::<Anomaly>(&ANOMALIES, &scan)
            .await?
            .into_iter()
            .map(|(a, _)| a)
            .filter(|a| watcher_id.is_empty() || a.watcher_id == watcher_id)
            .filter(|a| states.is_empty() || states.contains(&a.state))
            .filter(|a| a.severity >= min_severity)
            .filter(|a| a.detected_at.as_ref().map(crate::ids::ts_ms).unwrap_or(0) >= since_ms)
            .collect();
        let more = v.len() > limit;
        v.truncate(limit);
        let next = if more {
            v.last().map(|a| a.seq.to_string()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((v, next))
    }

    /// Anomalies with a sequence after `after_seq`, oldest first (stream resume).
    pub async fn anomalies_after(
        &self,
        cx: &Cx,
        repo_id: &str,
        watcher_id: &str,
        after_seq: u64,
        min_severity: i32,
    ) -> Result<Vec<Anomaly>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<Anomaly>(
                &ANOMALIES,
                &Scan::scope(repo_id, 1000)
                    .by_n1()
                    .n1_min(after_seq as i64 + 1),
            )
            .await?
            .into_iter()
            .map(|(a, _)| a)
            .filter(|a| watcher_id.is_empty() || a.watcher_id == watcher_id)
            .filter(|a| a.severity >= min_severity)
            .collect())
    }

    /// One anomaly.
    pub async fn get_anomaly(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Anomaly> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<Anomaly>(&ANOMALIES, &repo.id, id)
            .await?
            .map(|(a, _)| a)
            .ok_or_else(|| ForgeError::not_found(format!("anomaly {id}")))
    }

    /// Acknowledges (or resolves) an anomaly.
    pub async fn acknowledge_anomaly(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        note: &str,
        resolve: bool,
    ) -> Result<Anomaly> {
        check_len("note", note, 16 * 1024)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut a, row) = tx
            .get_msg::<Anomaly>(&ANOMALIES, &repo.id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("anomaly {id}")))?;
        if a.state == AnomalyState::Resolved as i32 {
            return Err(ForgeError::invalid_state(format!(
                "anomaly {id} is resolved"
            )));
        }
        let now = tx.now_ms();
        a.seq = tx.add_n1(&COUNTERS, &repo.id, "anomaly_seq", 1).await? as u64;
        a.acknowledged_by = Some(cx.principal());
        if resolve {
            a.state = AnomalyState::Resolved as i32;
            a.resolved_at = Some(ts(now));
        } else {
            a.state = AnomalyState::Acknowledged as i32;
        }
        if !note.is_empty() && a.evidence.len() < MAX_EVIDENCE {
            a.evidence.push(AnomalyEvidence {
                kind: "note".into(),
                note: note.to_string(),
                ..Default::default()
            });
        }
        tx.update(&ANOMALIES, &anomaly_row(&a, &repo.id, row.version))
            .await?;
        let json = pjson(&a);
        tx.emit_event(repo_event(
            "anomaly.updated",
            repo,
            SubjectKind::Repo,
            id,
            json.clone(),
        ))
        .await?;
        tx.audit(
            AuditDraft::new(
                if resolve {
                    "agent.anomaly_resolve"
                } else {
                    "agent.anomaly_acknowledge"
                },
                &repo.org_id,
            )
            .repo_id(&repo.id)
            .target_id(id)
            .after(&json),
        )
        .await?;
        tx.commit().await?;
        self.run_done.notify_waiters();
        Ok(a)
    }

    /// Links an anomaly to issue `number`, or opens a new issue with its evidence (number 0).
    #[allow(clippy::too_many_arguments)]
    pub async fn link_anomaly(
        &self,
        cx: &Cx,
        repo: &Repo,
        actor: &Actor,
        id: &str,
        number: u64,
        title: &str,
        labels: &[String],
    ) -> Result<Anomaly> {
        let a = self.get_anomaly(cx, repo, id).await?;
        let info = RepoInfo::from(repo);
        let number = if number > 0 {
            self.forge
                .work
                .get_issue_by_number(cx, &info, number)
                .await?
                .number
        } else {
            let w = self
                .watcher_rec(cx, &repo.id, &a.watcher_id)
                .await
                .ok()
                .map(|(r, _)| r.w());
            self.forge
                .work
                .create_issue(
                    cx,
                    &info,
                    actor,
                    NewIssue {
                        title: if title.trim().is_empty() {
                            a.title.clone()
                        } else {
                            title.trim().to_string()
                        },
                        body: issue_body(&a, w.as_ref()),
                        labels: labels.to_vec(),
                        ..NewIssue::default()
                    },
                )
                .await?
                .number
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut a, row) = tx
            .get_msg::<Anomaly>(&ANOMALIES, &repo.id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("anomaly {id}")))?;
        a.seq = tx.add_n1(&COUNTERS, &repo.id, "anomaly_seq", 1).await? as u64;
        a.issue = number.to_string();
        tx.update(&ANOMALIES, &anomaly_row(&a, &repo.id, row.version))
            .await?;
        let json = pjson(&a);
        tx.emit_event(repo_event(
            "anomaly.updated",
            repo,
            SubjectKind::Repo,
            id,
            json.clone(),
        ))
        .await?;
        tx.audit(
            AuditDraft::new("agent.anomaly_link_issue", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Issue, &number.to_string())
                .after(&json),
        )
        .await?;
        tx.commit().await?;
        self.run_done.notify_waiters();
        Ok(a)
    }

    // ---- review configuration ----

    /// The repo's automatic agent review configuration (disabled when never set).
    pub async fn review_config(&self, cx: &Cx, repo: &Repo) -> Result<AgentReviewConfig> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .get_msg::<AgentReviewConfig>(&REVIEW_CONFIG, &repo.id, "config")
            .await?
            .map(|(c, _)| c)
            .unwrap_or_else(|| AgentReviewConfig {
                repo: repo_ref_id(&repo.id),
                trigger: AgentReviewTrigger::Disabled as i32,
                ..Default::default()
            }))
    }

    /// Sets the configuration (`None` fields unchanged; non-empty `paths` replaces).
    #[allow(clippy::too_many_arguments)]
    pub async fn set_review_config(
        &self,
        cx: &Cx,
        repo: &Repo,
        trigger: Option<AgentReviewTrigger>,
        runtime_id: Option<String>,
        agent_id: Option<String>,
        paths: Vec<Vec<u8>>,
        include_drafts: Option<bool>,
        review_agent_authored: Option<bool>,
        instructions: Option<String>,
        budget: Option<AgentRunBudget>,
    ) -> Result<AgentReviewConfig> {
        let mut c = self.review_config(cx, repo).await?;
        if let Some(t) = trigger {
            c.trigger = t as i32;
        }
        if let Some(r) = runtime_id {
            c.runtime_id = r;
        }
        if let Some(a) = agent_id {
            if !a.is_empty() {
                let d = self.definition(cx, &repo.org_id, &a).await?;
                if !d.toolsets.iter().any(|t| t == "review") {
                    return Err(ForgeError::invalid(
                        "invalid_agent",
                        format!("agent {} may not review (no `review` toolset)", d.name),
                    ));
                }
            }
            c.agent_principal_id = a;
        }
        if !paths.is_empty() {
            c.paths = norm_paths(&paths)?;
        }
        if let Some(v) = include_drafts {
            c.include_drafts = v;
        }
        if let Some(v) = review_agent_authored {
            c.review_agent_authored = v;
        }
        if let Some(i) = instructions {
            check_len("instructions", &i, 64 * 1024)?;
            c.instructions = i;
        }
        if let Some(b) = budget {
            c.budget = Some(b);
        }
        let enabled = !matches!(
            AgentReviewTrigger::try_from(c.trigger).unwrap_or(AgentReviewTrigger::Unspecified),
            AgentReviewTrigger::Disabled | AgentReviewTrigger::Unspecified
        );
        if enabled && c.agent_principal_id.is_empty() {
            return Err(ForgeError::missing("agent_principal_id"));
        }
        c.repo = repo_ref_id(&repo.id);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.put(&REVIEW_CONFIG, &Row::new(repo.id.clone(), "config").msg(&c))
            .await?;
        let json = pjson(&c);
        tx.emit_event(repo_event(
            "agent_review_config.updated",
            repo,
            SubjectKind::Repo,
            &repo.id,
            json.clone(),
        ))
        .await?;
        tx.audit(
            AuditDraft::new("agent.review_config_set", &repo.org_id)
                .repo_id(&repo.id)
                .after(&json),
        )
        .await?;
        tx.commit().await?;
        Ok(c)
    }
}
