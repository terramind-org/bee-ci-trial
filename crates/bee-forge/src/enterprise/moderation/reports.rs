//! Content reports and the moderation queues.
//!
//! Anyone who can read a repo can report its content. A report goes to one queue: the repo's
//! maintainers (shown in the repo's queue and in its org's queue) or the site administrators.
//! Staff resolve or dismiss it, recording the action taken; resolving twice is
//! `INVALID_STATE`. All reports live in one table, filtered by queue through secondary keys:
//! `k1` is `repo:<id>` or `site`, `k2` is `org:<id>` or `site`.

use bee_proto::v1::{
    ModerationReport, ModerationReportAction, ModerationReportAudience, ModerationReportCategory,
    ModerationReportState, Principal, SubjectRef,
};
use serde_json::json;

use super::tables::REPORTS;
use super::{Change, Moderation, SURFACE, filtered_page, record};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Key, Row};

const SCOPE: &str = "all";
/// Longest report body.
pub const MAX_BODY: usize = 10_000;

/// A moderation queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportQueue {
    /// Reports for the maintainers of one repo (the repo id).
    Repo(String),
    /// Reports for the maintainers of any repo of an org (the org id).
    Org(String),
    /// Reports for site administrators.
    Site,
}

/// A report to file.
#[derive(Clone, Debug)]
pub struct NewReport {
    /// What is reported.
    pub subject: SubjectRef,
    /// Why.
    pub category: ModerationReportCategory,
    /// Who handles it.
    pub audience: ModerationReportAudience,
    /// What the reporter says.
    pub body: String,
    /// Org of the subject's repo, when it has one.
    pub org_id: Option<String>,
    /// The subject's repo, when it has one.
    pub repo_id: Option<String>,
    /// Who wrote the reported content, when known.
    pub content_author: Option<Principal>,
}

/// A stored report with the queue keys it was filed under.
#[derive(Clone, Debug)]
pub struct StoredReport {
    /// The report.
    pub report: ModerationReport,
    /// The repo whose maintainers handle it (none for the site queue).
    pub repo_id: Option<String>,
    /// The org whose maintainers handle it (none for the site queue).
    pub org_id: Option<String>,
}

fn strip(k: &Option<String>, prefix: &str) -> Option<String> {
    k.as_deref()
        .and_then(|s| s.strip_prefix(prefix))
        .map(str::to_string)
}

impl Moderation {
    /// Files a report. `INVALID_ARGUMENT` when it names no subject, or asks for the
    /// maintainers of content that has no repo.
    pub async fn create_report(&self, cx: &Cx, new: NewReport) -> Result<ModerationReport> {
        if new.subject.id.is_empty() && new.subject.repo.is_none() {
            return Err(ForgeError::missing("subject"));
        }
        if new.body.len() > MAX_BODY {
            return Err(ForgeError::too_large(
                "report_body",
                MAX_BODY as u64,
                new.body.len() as u64,
            ));
        }
        let audience = match new.audience {
            ModerationReportAudience::Unspecified => ModerationReportAudience::Maintainers,
            a => a,
        };
        let category = match new.category {
            ModerationReportCategory::Unspecified => ModerationReportCategory::Other,
            c => c,
        };
        let (k1, k2) = match (audience, &new.repo_id, &new.org_id) {
            (ModerationReportAudience::Maintainers, Some(r), Some(o)) => {
                (format!("repo:{r}"), format!("org:{o}"))
            }
            (ModerationReportAudience::Maintainers, _, _) => {
                return Err(ForgeError::invalid(
                    "no_maintainers",
                    "this content has no repo, so it can only be reported to site admins",
                ));
            }
            _ => ("site".to_string(), "site".to_string()),
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let id = tx.new_id()?;
        let report = ModerationReport {
            id: id.clone(),
            subject: Some(new.subject.clone()),
            category: category as i32,
            audience: audience as i32,
            body: new.body,
            reporter: Some(cx.principal()),
            content_author: new.content_author.clone(),
            state: ModerationReportState::Open as i32,
            action: ModerationReportAction::Unspecified as i32,
            resolution_note: String::new(),
            resolved_by: None,
            created_at: Some(ts(now)),
            resolved_at: None,
        };
        tx.insert(
            &REPORTS,
            &Row::new(SCOPE, id.clone())
                .k1(k1.clone())
                .k2(k2)
                .msg(&report),
        )
        .await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.reported",
                action: "moderation.report",
                org: new.org_id.clone().unwrap_or_default(),
                repo: new.repo_id.clone().unwrap_or_default(),
                subject: Some(new.subject),
                before: None,
                after: json!({
                    "report": id,
                    "queue": k1,
                    "category": category.as_str_name(),
                    "audience": audience.as_str_name(),
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(report)
    }

    /// Reports in a queue, optionally of one state and category (unspecified = any), paged
    /// by report id (oldest first).
    pub async fn list_reports(
        &self,
        cx: &Cx,
        queue: &ReportQueue,
        state: ModerationReportState,
        category: ModerationReportCategory,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<ModerationReport>, String)> {
        let key = match queue {
            ReportQueue::Repo(r) => Key::K1(format!("repo:{r}")),
            ReportQueue::Org(o) => Key::K2(format!("org:{o}")),
            ReportQueue::Site => Key::K1("site".into()),
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        filtered_page::<ModerationReport>(
            &mut tx,
            &REPORTS,
            SCOPE,
            Some(key),
            after,
            limit,
            |r, _| {
                (state == ModerationReportState::Unspecified || r.state == state as i32)
                    && (category == ModerationReportCategory::Unspecified
                        || r.category == category as i32)
            },
        )
        .await
    }

    /// One report with its queue keys.
    pub async fn report(&self, cx: &Cx, id: &str) -> Result<StoredReport> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (report, row) = tx
            .get_msg::<ModerationReport>(&REPORTS, SCOPE, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("report {id}")))?;
        Ok(StoredReport {
            report,
            repo_id: strip(&row.k1, "repo:"),
            org_id: strip(&row.k2, "org:"),
        })
    }

    /// Resolves or dismisses a report and records the action taken. `INVALID_STATE` when it
    /// was resolved already.
    pub async fn resolve_report(
        &self,
        cx: &Cx,
        id: &str,
        state: ModerationReportState,
        action: ModerationReportAction,
        note: &str,
    ) -> Result<ModerationReport> {
        if !matches!(
            state,
            ModerationReportState::Resolved | ModerationReportState::Dismissed
        ) {
            return Err(ForgeError::invalid(
                "bad_state",
                "state must be RESOLVED or DISMISSED",
            ));
        }
        let action = match action {
            ModerationReportAction::Unspecified => ModerationReportAction::None,
            a => a,
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some((mut report, row)) = tx.get_msg::<ModerationReport>(&REPORTS, SCOPE, id).await?
        else {
            return Err(ForgeError::not_found(format!("report {id}")));
        };
        if report.state != ModerationReportState::Open as i32 {
            return Err(ForgeError::invalid_state("report is already resolved"));
        }
        report.state = state as i32;
        report.action = action as i32;
        report.resolution_note = note.to_string();
        report.resolved_by = Some(cx.principal());
        report.resolved_at = Some(ts(now));
        let org = strip(&row.k2, "org:").unwrap_or_default();
        let repo = strip(&row.k1, "repo:").unwrap_or_default();
        let next = Row {
            body: prost::Message::encode_to_vec(&report),
            ..row
        };
        tx.update(&REPORTS, &next).await?;
        record(
            &mut tx,
            Change {
                kind: "moderation.report_resolved",
                action: "moderation.report_resolve",
                org,
                repo,
                subject: report.subject.clone(),
                before: Some(json!({ "state": "open" })),
                after: json!({
                    "report": id,
                    "state": state.as_str_name(),
                    "action": action.as_str_name(),
                    "note": note,
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(report)
    }
}
