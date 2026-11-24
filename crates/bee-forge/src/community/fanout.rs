//! Notification fan-out: events in, inbox threads, feed entries and email deliveries out.
//!
//! # Pipeline (the forge-core pattern)
//!
//! 1. **Dispatch.** [`Notifications::dispatch_once`] reads every org event log after its
//!    cursor (repo events are in their org's log too) and queues one job per event that can
//!    notify anyone. The cursor and the jobs commit in one transaction, so a crash loses and
//!    duplicates nothing.
//! 2. **Process.** [`Notifications::process_jobs`] claims a batch and runs it on a bounded
//!    pool ([`NotifyConfig::concurrency`] tasks). A job is processed in one transaction that
//!    also deletes it, so effects happen once; a failing job is retried and dropped (with an
//!    audit entry) after [`NotifyConfig::max_attempts`].
//! 3. **Deliver.** Email and reminder messages are rows in the delivery queue; the server's
//!    mail, Slack and Teams workers claim and complete them
//!    ([`Notifications::claim_deliveries`], [`Notifications::complete_delivery`]).
//!
//! # Who is notified
//!
//! [`derive`] turns an event into a [`Derived`]: the thread's subject, who participates
//! (author, assignees, commenters, mentioned), who is notified directly (requested reviewers,
//! assignees, mentioned logins, the author of an answered comment), and whether subscribers
//! and repo watchers hear about it.
//!
//! - A **subscriber** (implicit through participation, or manual) is notified of every event
//!   on the subject with the reason they subscribed for.
//! - A **repo watcher** at `ALL` is notified of every derived event of the repo; at `CUSTOM`
//!   of the chosen kinds; `PARTICIPATING` and `IGNORE` add nothing (`IGNORE` also silences
//!   subscriptions).
//! - **Direct** reasons (`review_requested`, `mention`, `assign`) override unsubscribe and
//!   ignore, but never the read-access check.
//! - Nobody is notified of their own action unless `include_own_activity`; nobody who cannot
//!   read the repo (or is not in the org, for org subjects) is notified at all.
//!
//! Event shapes consumed: typed bodies for `cr.*` (ChangeRequest) and `review.*`; JSON
//! payloads for `issue.*` / `issue_comment.created` (proto3-JSON of the issue, optionally
//! under `issue`, with `comment` for comments), this module's `discussion.*` and
//! `release.published`, and `security_alert.*`, `advisory.*`, `agent_run.*`,
//! `workflow_run.*` through their `subject`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bee_proto::v1::event::Body;
use bee_proto::v1::{
    ChangeState, Event, NotificationInboxEventKind, NotificationReason,
    NotificationReminderChannel, NotificationSubscription, NotificationSubscriptionState,
    NotificationThread, Principal, RepoWatch, RepoWatchEventKind, RepoWatchLevel, Verdict,
};
use bee_proto::v1::{SubjectKind, SubjectRef};
use prost::Message;
use serde_json::Value;
use tokio::task::JoinSet;

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::events::EventLog;
use crate::ids::{ts, ts_ms};
use crate::store::{Key, Order, Row, Scan, Surface, Tx};

use super::common::{draft, emit, mentions, subject};
use super::notifications::{
    CURSORS, DELIVERIES, DeliveryRec, JOBS, Notifications, REMINDERS, ReminderRec, SUBS, THREADS,
    TOKENS, ThreadRec, TokenRec, WATCHES, delivery_state, key_of, next_fire, reminder_repo_ok,
    subject_key,
};

/// What an event means for notifications.
#[derive(Clone, Debug, Default)]
pub struct Derived {
    /// The thread's subject (kind, repo id, id).
    pub subject: SubjectRef,
    /// Subject title (empty keeps the thread's).
    pub title: String,
    /// Per-repo number, for email subjects.
    pub number: u64,
    /// Repo id (empty for org subjects).
    pub repo_id: String,
    /// Org id.
    pub org_id: String,
    /// URL path under the repo (`changes/12`), without the host and repo.
    pub url_path: String,
    /// The acting principal.
    pub actor: Principal,
    /// Short summary of what happened, e.g. `commented`.
    pub activity: String,
    /// Subject state: `open`, `draft`, `landed`, `closed`, `failed`, ...
    pub state: String,
    /// Latest comment id.
    pub comment_id: String,
    /// Text to scan for mentions.
    pub mention_text: String,
    /// First lines of the content, for emails.
    pub excerpt: String,
    /// Watch kind of the subject, if repo watchers hear about it.
    pub watch_kind: Option<RepoWatchEventKind>,
    /// Principals who become subscribers.
    pub participants: Vec<(String, NotificationReason)>,
    /// Principals notified regardless of subscription.
    pub notify: Vec<(String, NotificationReason)>,
    /// Subscribers are notified.
    pub notify_subscribers: bool,
    /// Reason subscribers get (`None` = the reason they subscribed with).
    pub subscribers_reason: Option<NotificationReason>,
    /// Repo watchers are notified.
    pub notify_watchers: bool,
}

fn js<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    Some(cur)
}

fn jstr(v: &Value, path: &[&str]) -> String {
    js(v, path)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn jnum(v: &Value, path: &[&str]) -> u64 {
    match js(v, path) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn jids(v: &Value, path: &[&str]) -> Vec<String> {
    js(v, path)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| p.get("id").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn excerpt(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(600)
        .collect()
}

fn principal_from(v: Option<&Value>, fallback: &str) -> Principal {
    let id = v
        .and_then(|p| p.get("id"))
        .and_then(Value::as_str)
        .unwrap_or(fallback);
    Principal {
        id: id.to_string(),
        display: v
            .and_then(|p| p.get("display"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        ..Default::default()
    }
}

/// Whether events of this kind can notify anyone (the dispatcher queues only these).
pub fn relevant_kind(kind: &str) -> bool {
    kind.starts_with("cr.")
        || kind.starts_with("review.")
        || kind.starts_with("issue.")
        || kind == "issue_comment.created"
        || kind.starts_with("discussion.")
        || kind.starts_with("discussion_comment.")
        || kind == "release.published"
        || kind.starts_with("security_alert.")
        || kind.starts_with("advisory.")
        || kind.starts_with("agent_run.")
        || kind.starts_with("workflow_run.")
}

fn actor_of(e: &Event) -> Principal {
    Principal {
        id: e.principal.clone(),
        ..Default::default()
    }
}

fn cr_state(cr: &bee_proto::v1::ChangeRequest) -> &'static str {
    match ChangeState::try_from(cr.state).unwrap_or(ChangeState::Open) {
        ChangeState::Landed => "landed",
        ChangeState::Closed => "closed",
        _ if cr.draft => "draft",
        _ => "open",
    }
}

fn derive_cr(e: &Event, cr: &bee_proto::v1::ChangeRequest) -> Option<Derived> {
    let kind = e.kind.as_str();
    let (activity, notify_subscribers) = match kind {
        "cr.opened" => ("opened this change request", false),
        "cr.ready_for_review" => ("marked this change request ready for review", true),
        "cr.review_requested" => ("requested a review", true),
        "cr.revised" => ("pushed a new revision", true),
        "cr.landed" => ("landed this change request", true),
        "cr.closed" => ("closed this change request", true),
        "cr.reopened" => ("reopened this change request", true),
        _ => return None,
    };
    let mut participants = Vec::new();
    if let Some(a) = &cr.author {
        participants.push((a.id.clone(), NotificationReason::Author));
    }
    for a in &cr.assignees {
        participants.push((a.id.clone(), NotificationReason::Assign));
    }
    let mut notify = Vec::new();
    match kind {
        "cr.opened" | "cr.ready_for_review" => {
            for r in &cr.review_requests {
                if let Some(p) = &r.reviewer {
                    notify.push((p.id.clone(), NotificationReason::ReviewRequested));
                }
            }
            if kind == "cr.opened" {
                for a in &cr.assignees {
                    notify.push((a.id.clone(), NotificationReason::Assign));
                }
            }
        }
        "cr.review_requested" => {
            let newest = cr
                .review_requests
                .iter()
                .filter_map(|r| r.requested_at.as_ref().map(ts_ms))
                .max()
                .unwrap_or(0);
            for r in &cr.review_requests {
                if let Some(p) = &r.reviewer
                    && r.requested_at.as_ref().map(ts_ms).unwrap_or(0) == newest
                {
                    notify.push((p.id.clone(), NotificationReason::ReviewRequested));
                    participants.push((p.id.clone(), NotificationReason::ReviewRequested));
                }
            }
        }
        _ => {}
    }
    let state_change = matches!(
        kind,
        "cr.landed" | "cr.closed" | "cr.reopened" | "cr.revised"
    );
    Some(Derived {
        subject: SubjectRef {
            kind: SubjectKind::ChangeRequest as i32,
            repo: super::common::repo_ref_id(&cr.repo_id),
            id: cr.id.clone(),
        },
        title: cr.title.clone(),
        number: cr.number,
        repo_id: cr.repo_id.clone(),
        org_id: e.org_id.clone(),
        url_path: format!("changes/{}", cr.number),
        actor: actor_of(e),
        activity: activity.into(),
        state: cr_state(cr).into(),
        mention_text: if kind == "cr.opened" {
            cr.body.clone()
        } else {
            String::new()
        },
        excerpt: excerpt(&cr.body),
        watch_kind: Some(RepoWatchEventKind::ChangeRequests),
        participants,
        notify,
        notify_subscribers,
        subscribers_reason: state_change.then_some(NotificationReason::StateChange),
        notify_watchers: true,
        ..Default::default()
    })
}

fn derive_review(e: &Event, rv: &bee_proto::v1::Review) -> Option<Derived> {
    let dismissed = e.kind == "review.dismissed";
    if e.kind != "review.submitted" && !dismissed {
        return None;
    }
    let activity = if dismissed {
        "dismissed a review".to_string()
    } else {
        match Verdict::try_from(rv.verdict).unwrap_or(Verdict::Comment) {
            Verdict::Approve => "approved this change request",
            Verdict::RequestChanges => "requested changes",
            _ => "reviewed this change request",
        }
        .to_string()
    };
    let reviewer = rv.reviewer.clone().unwrap_or_default();
    Some(Derived {
        subject: SubjectRef {
            kind: SubjectKind::ChangeRequest as i32,
            repo: super::common::repo_ref_id(&e.repo_id),
            id: rv.change_id.clone(),
        },
        repo_id: e.repo_id.clone(),
        org_id: e.org_id.clone(),
        actor: if reviewer.id.is_empty() {
            actor_of(e)
        } else {
            reviewer.clone()
        },
        activity,
        comment_id: rv.id.clone(),
        mention_text: rv.body.clone(),
        excerpt: excerpt(&rv.body),
        watch_kind: Some(RepoWatchEventKind::ChangeRequests),
        participants: vec![(reviewer.id, NotificationReason::Comment)],
        notify_subscribers: true,
        subscribers_reason: dismissed.then_some(NotificationReason::StateChange),
        notify_watchers: true,
        ..Default::default()
    })
}

fn derive_issue(e: &Event, v: &Value) -> Option<Derived> {
    let kind = e.kind.as_str();
    let issue = v.get("issue").unwrap_or(v);
    let comment = v.get("comment");
    let subj = match &e.subject {
        Some(s) if s.kind != 0 => s.clone(),
        _ => {
            let id = jstr(issue, &["id"]);
            if id.is_empty() {
                return None;
            }
            SubjectRef {
                kind: SubjectKind::Issue as i32,
                repo: super::common::repo_ref_id(&e.repo_id),
                id,
            }
        }
    };
    let is_cr = subj.kind == SubjectKind::ChangeRequest as i32;
    let noun = if is_cr { "change request" } else { "issue" };
    let author = jstr(issue, &["author", "id"]);
    let assignees = jids(issue, &["assignees"]);
    let mut participants = Vec::new();
    if !author.is_empty() {
        participants.push((author, NotificationReason::Author));
    }
    for a in &assignees {
        participants.push((a.clone(), NotificationReason::Assign));
    }
    let (activity, state, notify, subs_reason, mention_text, comment_id, subs) = match kind {
        "issue.opened" => (
            format!("opened this {noun}"),
            "open",
            assignees
                .iter()
                .map(|a| (a.clone(), NotificationReason::Assign))
                .collect::<Vec<_>>(),
            None,
            jstr(issue, &["body"]),
            String::new(),
            false,
        ),
        "issue.closed" => (
            format!("closed this {noun}"),
            "closed",
            vec![],
            Some(NotificationReason::StateChange),
            String::new(),
            String::new(),
            true,
        ),
        "issue.reopened" => (
            format!("reopened this {noun}"),
            "open",
            vec![],
            Some(NotificationReason::StateChange),
            String::new(),
            String::new(),
            true,
        ),
        "issue_comment.created" => {
            let c = comment?;
            if let Some(a) = c
                .get("author")
                .and_then(|a| a.get("id"))
                .and_then(Value::as_str)
            {
                participants.push((a.to_string(), NotificationReason::Comment));
            }
            (
                "commented".to_string(),
                "open",
                vec![],
                None,
                jstr(c, &["body"]),
                jstr(c, &["id"]),
                true,
            )
        }
        _ => return None,
    };
    let number = jnum(issue, &["number"]);
    Some(Derived {
        subject: subj,
        title: jstr(issue, &["title"]),
        number,
        repo_id: e.repo_id.clone(),
        org_id: e.org_id.clone(),
        url_path: format!("{}/{number}", if is_cr { "changes" } else { "issues" }),
        actor: principal_from(comment.and_then(|c| c.get("author")), &e.principal),
        activity,
        state: state.into(),
        comment_id,
        excerpt: excerpt(&mention_text),
        mention_text,
        watch_kind: Some(if is_cr {
            RepoWatchEventKind::ChangeRequests
        } else {
            RepoWatchEventKind::Issues
        }),
        participants,
        notify,
        notify_subscribers: subs,
        subscribers_reason: subs_reason,
        notify_watchers: true,
    })
}

fn derive_discussion(e: &Event, v: &Value) -> Option<Derived> {
    let kind = e.kind.as_str();
    let d = v.get("discussion")?;
    let subj = e.subject.clone().filter(|s| s.kind != 0)?;
    let number = jnum(d, &["number"]);
    let author = jstr(d, &["author", "id"]);
    let mut participants = Vec::new();
    if !author.is_empty() {
        participants.push((author, NotificationReason::Author));
    }
    let mut notify = Vec::new();
    let (activity, state, subs_reason, text, comment_id, subs) = match kind {
        "discussion.created" => (
            "started this discussion",
            "open",
            None,
            jstr(d, &["body"]),
            String::new(),
            false,
        ),
        "discussion_comment.created" => {
            let c = v.get("comment")?;
            if let Some(a) = c
                .get("author")
                .and_then(|a| a.get("id"))
                .and_then(Value::as_str)
            {
                participants.push((a.to_string(), NotificationReason::Comment));
            }
            (
                "commented",
                "open",
                None,
                jstr(c, &["body"]),
                jstr(c, &["id"]),
                true,
            )
        }
        "discussion.answered" => {
            if let Some(a) = v
                .get("answer")
                .and_then(|a| a.get("author"))
                .and_then(|a| a.get("id"))
                .and_then(Value::as_str)
            {
                notify.push((a.to_string(), NotificationReason::StateChange));
            }
            (
                "marked an answer",
                "answered",
                Some(NotificationReason::StateChange),
                String::new(),
                jstr(v, &["answer", "id"]),
                true,
            )
        }
        "discussion.closed" => (
            "closed this discussion",
            "closed",
            Some(NotificationReason::StateChange),
            String::new(),
            String::new(),
            true,
        ),
        "discussion.reopened" => (
            "reopened this discussion",
            "open",
            Some(NotificationReason::StateChange),
            String::new(),
            String::new(),
            true,
        ),
        _ => return None,
    };
    let home_repo = e.repo_id.clone();
    Some(Derived {
        subject: subj,
        title: jstr(d, &["title"]),
        number,
        repo_id: home_repo,
        org_id: e.org_id.clone(),
        url_path: format!("discussions/{number}"),
        actor: actor_of(e),
        activity: activity.into(),
        state: state.into(),
        comment_id,
        excerpt: excerpt(&text),
        mention_text: text,
        watch_kind: Some(RepoWatchEventKind::Discussions),
        participants,
        notify,
        notify_subscribers: subs,
        subscribers_reason: subs_reason,
        notify_watchers: true,
    })
}

fn derive_release(e: &Event, v: &Value) -> Option<Derived> {
    let r = v.get("release")?;
    let subj = e.subject.clone().filter(|s| s.kind != 0)?;
    let tag = jstr(r, &["tagName"]);
    let name = jstr(r, &["name"]);
    let author = jstr(r, &["author", "id"]);
    let body = jstr(r, &["body"]);
    Some(Derived {
        subject: subj,
        title: if name.is_empty() { tag.clone() } else { name },
        repo_id: e.repo_id.clone(),
        org_id: e.org_id.clone(),
        url_path: format!("releases/tag/{tag}"),
        actor: actor_of(e),
        activity: "published a release".into(),
        state: "published".into(),
        excerpt: excerpt(&body),
        mention_text: body,
        watch_kind: Some(RepoWatchEventKind::Releases),
        participants: if author.is_empty() {
            vec![]
        } else {
            vec![(author, NotificationReason::Author)]
        },
        notify_watchers: true,
        ..Default::default()
    })
}

fn derive_generic(e: &Event, v: &Value) -> Option<Derived> {
    let subj = e.subject.clone().filter(|s| s.kind != 0)?;
    let (reason, watch) = match SubjectKind::try_from(subj.kind).ok()? {
        SubjectKind::SecurityAlert | SubjectKind::Advisory => (
            NotificationReason::SecurityAlert,
            Some(RepoWatchEventKind::SecurityAlerts),
        ),
        SubjectKind::AgentRun => (
            NotificationReason::AgentRun,
            Some(RepoWatchEventKind::AgentRuns),
        ),
        SubjectKind::WorkflowRun => (
            NotificationReason::CiActivity,
            Some(RepoWatchEventKind::WorkflowRuns),
        ),
        _ => return None,
    };
    let title = ["title", "name", "summary"]
        .iter()
        .map(|k| jstr(v, &[k]))
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let state = jstr(v, &["state"]);
    let owner = ["owner", "started_by", "startedBy"]
        .iter()
        .map(|k| jstr(v, &[k, "id"]))
        .find(|s| !s.is_empty());
    Some(Derived {
        subject: subj,
        title,
        repo_id: e.repo_id.clone(),
        org_id: e.org_id.clone(),
        actor: actor_of(e),
        activity: e
            .kind
            .rsplit('.')
            .next()
            .unwrap_or("updated")
            .replace('_', " "),
        state,
        watch_kind: watch,
        notify: owner.into_iter().map(|o| (o, reason)).collect(),
        notify_subscribers: true,
        subscribers_reason: Some(reason),
        notify_watchers: true,
        ..Default::default()
    })
}

/// What an event means for notifications, or `None` when it notifies nobody.
pub fn derive(e: &Event) -> Option<Derived> {
    if !relevant_kind(&e.kind) {
        return None;
    }
    match &e.body {
        Some(Body::Change(cr)) if e.kind.starts_with("cr.") => return derive_cr(e, cr),
        Some(Body::Review(rv)) if e.kind.starts_with("review.") => return derive_review(e, rv),
        _ => {}
    }
    let v: Value = serde_json::from_str(&e.payload_json).ok()?;
    let k = e.kind.as_str();
    if k.starts_with("issue.") || k == "issue_comment.created" {
        derive_issue(e, &v)
    } else if k.starts_with("discussion.") || k.starts_with("discussion_comment.") {
        derive_discussion(e, &v)
    } else if k == "release.published" {
        derive_release(e, &v)
    } else {
        derive_generic(e, &v)
    }
}

/// Reason priority: lower sorts first and becomes the thread's headline reason.
fn rank(r: NotificationReason) -> u8 {
    use NotificationReason as R;
    match r {
        R::ReviewRequested => 0,
        R::Mention => 1,
        R::TeamMention => 2,
        R::Assign => 3,
        R::Author => 4,
        R::Comment => 5,
        R::StateChange => 6,
        R::CiActivity => 7,
        R::SecurityAlert => 8,
        R::AgentRun => 9,
        R::Manual => 10,
        R::Subscribed => 11,
        R::Unspecified => 12,
    }
}

fn phrase(r: NotificationReason) -> &'static str {
    use NotificationReason as R;
    match r {
        R::ReviewRequested => "were requested to review this",
        R::Mention => "were mentioned",
        R::TeamMention => "are on a team that was mentioned",
        R::Assign => "were assigned",
        R::Author => "authored the thread",
        R::Comment => "commented",
        R::StateChange => "changed the state of the thread",
        R::CiActivity => "are following CI activity",
        R::SecurityAlert => "are watching security alerts",
        R::AgentRun => "own the agent run",
        R::Manual => "subscribed to the thread",
        R::Subscribed => "are watching the repository",
        R::Unspecified => "are participating",
    }
}

/// Why an event reaches someone (`event`), and the standing reasons they also have on the
/// thread (`also`), which join the thread's reason list but not its headline.
#[derive(Default)]
struct Recipient {
    event: BTreeSet<i32>,
    also: BTreeSet<i32>,
}

fn sorted(set: &BTreeSet<i32>) -> Vec<NotificationReason> {
    let mut v: Vec<NotificationReason> = set
        .iter()
        .filter_map(|r| NotificationReason::try_from(*r).ok())
        .collect();
    v.sort_by_key(|r| rank(*r));
    v
}

impl Recipient {
    fn add(&mut self, r: NotificationReason) {
        self.event.insert(r as i32);
    }

    fn add_also(&mut self, r: NotificationReason) {
        self.also.insert(r as i32);
    }
}

impl Notifications {
    // ---- dispatch ---------------------------------------------------------------------

    /// Queues jobs for events appended to the org logs since the last dispatch.
    pub async fn dispatch_once(&self, cx: &Cx, events: &EventLog) -> Result<usize> {
        let logs: Vec<String> = {
            let mut tx = self.db.begin(Surface::EVENTS, cx).await?;
            let rows = tx
                .scan(
                    &crate::events::COUNTERS,
                    &Scan {
                        limit: crate::store::MAX_SCAN,
                        ..Scan::default()
                    },
                )
                .await?;
            rows.into_iter()
                .filter(|r| r.id == "seq" && r.scope.starts_with("org:"))
                .map(|r| r.scope)
                .collect()
        };
        let mut queued = 0;
        for log in logs {
            let cursor = {
                let mut tx = self
                    .db
                    .begin(super::notifications::NOTIFICATIONS, cx)
                    .await?;
                tx.get(&CURSORS, "", &log)
                    .await?
                    .and_then(|r| r.n1)
                    .unwrap_or(0) as u64
            };
            let read = match events.read(cx, &log, cursor + 1, &[], 500).await {
                Ok(x) => x,
                Err(e) if e.subcode == "seq_not_retained" => {
                    events.read(cx, &log, 0, &[], 500).await?
                }
                Err(e) => return Err(e),
            };
            let (batch, next) = read;
            if batch.is_empty() && next <= cursor + 1 {
                continue;
            }
            let mut tx = self
                .db
                .begin(super::notifications::NOTIFICATIONS, cx)
                .await?;
            for e in batch.iter().filter(|e| relevant_kind(&e.kind)) {
                let id = tx.new_id()?;
                tx.insert(
                    &JOBS,
                    &Row::new("", id).n1(0).msg(&Job {
                        event: Some(e.clone()),
                        attempts: 0,
                    }),
                )
                .await?;
                queued += 1;
            }
            tx.put(&CURSORS, &Row::new("", log).n1(next as i64 - 1))
                .await?;
            tx.commit().await?;
        }
        Ok(queued)
    }

    /// Processes up to one batch of jobs on the bounded pool; returns how many ran.
    pub async fn process_jobs(self: &Arc<Self>, cx: &Cx) -> Result<usize> {
        let rows: Vec<Row> = {
            let mut tx = self
                .db
                .begin(super::notifications::NOTIFICATIONS, cx)
                .await?;
            tx.scan(&JOBS, &Scan::scope("", self.cfg.batch)).await?
        };
        let mut done = 0;
        for chunk in rows.chunks(self.cfg.concurrency.max(1)) {
            let mut set = JoinSet::new();
            for row in chunk {
                let me = self.clone();
                let id = row.id.clone();
                let cx = cx.clone();
                set.spawn(async move { me.run_job(&cx, &id).await });
            }
            while let Some(r) = set.join_next().await {
                match r {
                    Ok(Ok(())) => done += 1,
                    Ok(Err(e)) => return Err(e),
                    Err(e) => {
                        return Err(ForgeError::internal(format!("fan-out task failed: {e}")));
                    }
                }
            }
        }
        if done > 0 {
            self.wake();
        }
        Ok(done)
    }

    async fn run_job(&self, cx: &Cx, id: &str) -> Result<()> {
        let job = {
            let mut tx = self
                .db
                .begin(super::notifications::NOTIFICATIONS, cx)
                .await?;
            match tx.get_msg::<Job>(&JOBS, "", id).await? {
                Some((j, _)) => j,
                None => return Ok(()),
            }
        };
        let Some(event) = job.event.clone() else {
            return self.drop_job(cx, id, "job without event").await;
        };
        match self.process_event(cx, &event, Some(id)).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let mut tx = self
                    .db
                    .begin(super::notifications::NOTIFICATIONS, cx)
                    .await?;
                let Some(mut row) = tx.get_for_update(&JOBS, "", id).await? else {
                    return Ok(());
                };
                let mut j: Job = row.decode()?;
                j.attempts += 1;
                if j.attempts >= self.cfg.max_attempts {
                    drop(tx);
                    return self.drop_job(cx, id, &e.to_string()).await;
                }
                row.body = j.encode_to_vec();
                tx.update(&JOBS, &row).await?;
                tx.commit().await
            }
        }
    }

    async fn drop_job(&self, cx: &Cx, id: &str, why: &str) -> Result<()> {
        let mut tx = self
            .db
            .begin(super::notifications::NOTIFICATIONS, cx)
            .await?;
        tx.delete(&JOBS, "", id).await?;
        tx.audit(draft(
            "notification.fanout_dropped",
            "",
            "",
            SubjectKind::Unspecified,
            id,
            &serde_json::json!({"reason": why}),
        ))
        .await?;
        tx.commit().await
    }

    // ---- fan-out of one event ---------------------------------------------------------

    /// Derives and writes the notifications of one event; deletes `job_id` in the same
    /// transaction. Returns the number of inbox entries written.
    pub async fn process_event(&self, cx: &Cx, e: &Event, job_id: Option<&str>) -> Result<usize> {
        let Some(mut d) = derive(e) else {
            return self.finish_job(cx, job_id).await.map(|_| 0);
        };
        if d.actor.id.is_empty() {
            d.actor.id = e.principal.clone();
        }
        let key = key_of(&d.subject);
        // Resolve mentions to principals (outside any transaction).
        let mut direct: BTreeMap<String, Recipient> = BTreeMap::new();
        for (p, r) in &d.notify {
            direct.entry(p.clone()).or_default().add(*r);
        }
        let mut mentioned = Vec::new();
        for login in mentions(&d.mention_text) {
            if let Some(p) = self.directory.by_login(&login).await {
                direct
                    .entry(p.id.clone())
                    .or_default()
                    .add(NotificationReason::Mention);
                mentioned.push(p.id);
            }
        }
        // Read what the subject's subscribers and the repo's watchers say.
        let (subs, watches): (Vec<(String, NotificationSubscription)>, Vec<RepoWatch>) = {
            let mut tx = self
                .db
                .begin(super::notifications::NOTIFICATIONS, cx)
                .await?;
            let subs = tx
                .scan_msgs::<NotificationSubscription>(&SUBS, &Scan::scope(key.clone(), 10_000))
                .await?
                .into_iter()
                .map(|(s, r)| (r.id, s))
                .collect();
            let watches = if d.repo_id.is_empty() {
                Vec::new()
            } else {
                tx.scan_msgs::<RepoWatch>(&WATCHES, &Scan::scope(d.repo_id.clone(), 10_000))
                    .await?
                    .into_iter()
                    .map(|(w, _)| w)
                    .collect()
            };
            (subs, watches)
        };
        let mut silenced: BTreeSet<String> = BTreeSet::new();
        let mut recipients: BTreeMap<String, Recipient> = direct;
        for (p, s) in &subs {
            match NotificationSubscriptionState::try_from(s.state)
                .unwrap_or(NotificationSubscriptionState::Unspecified)
            {
                NotificationSubscriptionState::Subscribed => {
                    if d.notify_subscribers {
                        let reason = d.subscribers_reason.unwrap_or_else(|| {
                            NotificationReason::try_from(s.reason)
                                .unwrap_or(NotificationReason::Subscribed)
                        });
                        let r = recipients.entry(p.clone()).or_default();
                        r.add(reason);
                        if let Ok(own) = NotificationReason::try_from(s.reason)
                            && own != NotificationReason::Unspecified
                        {
                            r.add_also(own);
                        }
                    }
                }
                NotificationSubscriptionState::Unsubscribed
                | NotificationSubscriptionState::Ignored => {
                    silenced.insert(p.clone());
                }
                NotificationSubscriptionState::Unspecified => {}
            }
        }
        for w in &watches {
            let Some(p) = w.watcher.as_ref().map(|p| p.id.clone()) else {
                continue;
            };
            match RepoWatchLevel::try_from(w.level).unwrap_or(RepoWatchLevel::Unspecified) {
                RepoWatchLevel::All if d.notify_watchers => recipients
                    .entry(p)
                    .or_default()
                    .add(NotificationReason::Subscribed),
                RepoWatchLevel::Custom if d.notify_watchers => {
                    if d.watch_kind
                        .is_some_and(|k| w.custom_events.contains(&(k as i32)))
                    {
                        recipients
                            .entry(p)
                            .or_default()
                            .add(NotificationReason::Subscribed);
                    }
                }
                RepoWatchLevel::Ignore => {
                    silenced.insert(p);
                }
                _ => {}
            }
        }
        // Silenced principals keep only their direct reasons.
        for p in &silenced {
            if let Some(r) = recipients.get_mut(p) {
                r.also.clear();
                r.event.retain(|x| {
                    matches!(
                        NotificationReason::try_from(*x),
                        Ok(NotificationReason::ReviewRequested
                            | NotificationReason::Mention
                            | NotificationReason::TeamMention
                            | NotificationReason::Assign)
                    )
                });
            }
        }
        recipients.retain(|_, r| !r.event.is_empty());
        let repo_name = if d.repo_id.is_empty() {
            String::new()
        } else {
            self.access.repo_name(&d.repo_id).await
        };
        // Per-recipient filtering: own activity, read access, delivery preferences.
        struct Plan {
            principal: String,
            reasons: Vec<NotificationReason>,
            also: Vec<NotificationReason>,
            web: bool,
            email_to: Option<String>,
            reply: bool,
        }
        let mut plans = Vec::new();
        for (p, r) in &recipients {
            let settings = self.settings_of(cx, p).await?;
            if *p == d.actor.id && !settings.include_own_activity {
                continue;
            }
            if !self.access.can_read(&d.repo_id, &d.org_id, p).await {
                continue;
            }
            let reasons = sorted(&r.event);
            let also = sorted(&r.also);
            let pref = |f: fn(&bee_proto::v1::NotificationReasonDelivery) -> bool| {
                reasons.iter().any(|r| {
                    settings
                        .deliveries
                        .iter()
                        .find(|x| x.reason == *r as i32)
                        .map(f)
                        .unwrap_or(true)
                })
            };
            let web = pref(|x| x.web);
            let mut email = pref(|x| x.email);
            if settings.email_ci_failures_only
                && reasons.iter().all(|r| *r == NotificationReason::CiActivity)
                && d.state != "failed"
            {
                email = false;
            }
            let email_to = if email {
                let org_name = repo_name.split('/').next().unwrap_or("");
                match settings
                    .email_routes
                    .iter()
                    .find(|x| x.org == d.org_id || (!org_name.is_empty() && x.org == org_name))
                {
                    Some(route) => Some(route.email.clone()),
                    None if !settings.default_email.is_empty() => {
                        Some(settings.default_email.clone())
                    }
                    None => self.emails.primary_verified(p).await,
                }
            } else {
                None
            };
            if !web && email_to.is_none() {
                continue;
            }
            plans.push(Plan {
                principal: p.clone(),
                reasons,
                also,
                web,
                email_to,
                reply: settings.reply_by_email,
            });
        }
        // One transaction: implicit subscriptions, threads, feed, deliveries, job removal.
        let mut tx = self
            .db
            .begin(super::notifications::NOTIFICATIONS, cx)
            .await?;
        let now = tx.now_ms();
        let at = e.time.as_ref().map(ts_ms).unwrap_or(now);
        if let Some(j) = job_id {
            // Lock the job first: a concurrent worker that already ran it leaves nothing to do.
            if tx.get_for_update(&JOBS, "", j).await?.is_none() {
                return Ok(0);
            }
        }
        let mut implicit: Vec<(String, NotificationReason)> = d
            .participants
            .iter()
            .filter(|(p, _)| !p.is_empty())
            .cloned()
            .collect();
        implicit.extend(
            mentioned
                .iter()
                .map(|p| (p.clone(), NotificationReason::Mention)),
        );
        for (p, r) in implicit {
            if tx.get(&SUBS, &key, &p).await?.is_none() {
                let sub = NotificationSubscription {
                    subject: Some(d.subject.clone()),
                    state: NotificationSubscriptionState::Subscribed as i32,
                    reason: r as i32,
                    updated_at: Some(ts(now)),
                };
                tx.insert(
                    &SUBS,
                    &Row::new(key.clone(), p)
                        .k1((NotificationSubscriptionState::Subscribed as i32).to_string())
                        .msg(&sub),
                )
                .await?;
            }
        }
        let url = if repo_name.is_empty() || d.url_path.is_empty() {
            String::new()
        } else {
            format!("{}/{repo_name}/{}", self.cfg.base_url, d.url_path)
        };
        let mut written = 0;
        for plan in &plans {
            let headline = plan
                .reasons
                .first()
                .copied()
                .unwrap_or(NotificationReason::Subscribed);
            let activity = if plan.reasons.contains(&NotificationReason::ReviewRequested) {
                "requested your review".to_string()
            } else if plan.reasons.contains(&NotificationReason::Mention) {
                "mentioned you".to_string()
            } else if plan.reasons.contains(&NotificationReason::Assign)
                && d.activity.starts_with("opened")
            {
                "assigned you".to_string()
            } else {
                d.activity.clone()
            };
            let mut reply_token = String::new();
            if plan.web {
                let existing = tx.get_for_update(&THREADS, &plan.principal, &key).await?;
                let is_new = existing.is_none();
                let (mut rec, mut row) = match existing {
                    Some(row) => (row.decode::<ThreadRec>()?, row),
                    None => (
                        ThreadRec {
                            thread: Some(NotificationThread {
                                id: key.clone(),
                                subject: Some(d.subject.clone()),
                                ..Default::default()
                            }),
                            org_id: d.org_id.clone(),
                            repo_id: d.repo_id.clone(),
                            reply_expires_ms: 0,
                        },
                        Row::new(plan.principal.clone(), key.clone()),
                    ),
                };
                let mut t = rec.thread.clone().unwrap_or_default();
                if !d.title.is_empty() {
                    t.title = d.title.clone();
                }
                let mut all: BTreeSet<i32> = t.reasons.iter().copied().collect();
                all.extend(plan.reasons.iter().map(|r| *r as i32));
                all.extend(plan.also.iter().map(|r| *r as i32));
                let mut all: Vec<NotificationReason> = all
                    .into_iter()
                    .filter_map(|r| NotificationReason::try_from(r).ok())
                    .collect();
                all.sort_by_key(|r| rank(*r));
                t.reasons = all.iter().map(|r| *r as i32).collect();
                t.reason = headline as i32;
                t.unread = true;
                t.done = false;
                t.last_actor = Some(d.actor.clone());
                t.last_activity = activity.clone();
                if !d.comment_id.is_empty() {
                    t.latest_comment_id = d.comment_id.clone();
                }
                if !d.state.is_empty() {
                    t.subject_state = d.state.clone();
                }
                if !repo_name.is_empty() {
                    t.repo_full_name = repo_name.clone();
                }
                if !url.is_empty() {
                    t.url = url.clone();
                }
                let newest = row.n1.unwrap_or(0).max(at as i64);
                t.updated_at = Some(ts(newest as u64));
                if plan.email_to.is_some()
                    && plan.reply
                    && (t.reply_token.is_empty() || rec.reply_expires_ms <= now)
                {
                    let token = self.db.ids().random_hex(16)?;
                    let expires = now + super::notifications::REPLY_TTL_MS;
                    tx.insert(
                        &TOKENS,
                        &Row::new("", token.clone())
                            .n1(expires as i64)
                            .msg(&TokenRec {
                                principal_id: plan.principal.clone(),
                                thread_id: key.clone(),
                                subject: Some(d.subject.clone()),
                                expires_ms: expires,
                            }),
                    )
                    .await?;
                    t.reply_token = token;
                    rec.reply_expires_ms = expires;
                }
                reply_token = t.reply_token.clone();
                rec.thread = Some(t.clone());
                row.n1 = Some(newest);
                row.body = rec.encode_to_vec();
                if is_new {
                    tx.insert(&THREADS, &row).await?;
                } else {
                    tx.update(&THREADS, &row).await?;
                }
                let kind = if is_new {
                    NotificationInboxEventKind::New
                } else {
                    NotificationInboxEventKind::Updated
                };
                Notifications::push_feed(&mut tx, &plan.principal, kind, Some(t)).await?;
                written += 1;
            }
            if let Some(to) = &plan.email_to {
                if plan.reply && reply_token.is_empty() {
                    reply_token = self.db.ids().random_hex(16)?;
                    let expires = now + super::notifications::REPLY_TTL_MS;
                    tx.insert(
                        &TOKENS,
                        &Row::new("", reply_token.clone())
                            .n1(expires as i64)
                            .msg(&TokenRec {
                                principal_id: plan.principal.clone(),
                                thread_id: key.clone(),
                                subject: Some(d.subject.clone()),
                                expires_ms: expires,
                            }),
                    )
                    .await?;
                }
                let actor = if d.actor.display.is_empty() {
                    d.actor.id.clone()
                } else {
                    d.actor.display.clone()
                };
                let reason_line = plan
                    .reasons
                    .first()
                    .map(|r| format!("You are receiving this because you {}.", phrase(*r)))
                    .unwrap_or_default();
                let mut text = format!("{actor} {activity}.\n");
                if !d.excerpt.is_empty() {
                    text.push_str(&format!("\n{}\n", d.excerpt));
                }
                if !url.is_empty() {
                    text.push_str(&format!("\n-- \nView it on Beehive: {url}\n"));
                }
                if !reply_token.is_empty() {
                    text.push_str("Reply to this email to comment.\n");
                }
                text.push_str(&format!("{reason_line}\n"));
                let number = if d.number > 0 {
                    format!(" (#{})", d.number)
                } else {
                    String::new()
                };
                let subject_line = if repo_name.is_empty() {
                    format!("{}{number}", d.title)
                } else {
                    format!("[{repo_name}] {}{number}", d.title)
                };
                let id = tx.new_id()?;
                let rec = DeliveryRec {
                    id: id.clone(),
                    channel: NotificationReminderChannel::Email as i32,
                    principal_id: plan.principal.clone(),
                    to: to.clone(),
                    reply_to: if reply_token.is_empty() {
                        String::new()
                    } else {
                        format!("reply+{reply_token}@{}", self.cfg.reply_domain)
                    },
                    subject: subject_line,
                    text,
                    thread_id: key.clone(),
                    reply_token: reply_token.clone(),
                    attempts: 0,
                    state: delivery_state::QUEUED,
                    created_ms: now,
                    last_error: String::new(),
                    reminder_id: String::new(),
                };
                tx.insert(
                    &DELIVERIES,
                    &Row::new("", id)
                        .k1((NotificationReminderChannel::Email as i32).to_string())
                        .n1(now as i64)
                        .msg(&rec),
                )
                .await?;
            }
            if plan.reasons.contains(&NotificationReason::ReviewRequested) {
                self.real_time_alerts(&mut tx, &plan.principal, &d, &url, now)
                    .await?;
            }
        }
        if let Some(j) = job_id {
            tx.delete(&JOBS, "", j).await?;
        }
        tx.commit().await?;
        if written > 0 {
            self.wake();
        }
        Ok(written)
    }

    async fn finish_job(&self, cx: &Cx, job_id: Option<&str>) -> Result<()> {
        if let Some(j) = job_id {
            let mut tx = self
                .db
                .begin(super::notifications::NOTIFICATIONS, cx)
                .await?;
            tx.delete(&JOBS, "", j).await?;
            tx.commit().await?;
        }
        Ok(())
    }

    async fn real_time_alerts(
        &self,
        tx: &mut Tx,
        principal: &str,
        d: &Derived,
        url: &str,
        now: u64,
    ) -> Result<()> {
        let rows = tx
            .scan_msgs::<ReminderRec>(&REMINDERS, &Scan::scope(principal, 1000))
            .await?;
        for (rec, _) in rows {
            let Some(r) = rec.reminder else { continue };
            if !r.enabled || !r.real_time_alerts || !r.org.is_empty() && r.org != d.org_id {
                continue;
            }
            if let Some(f) = &r.filter
                && !reminder_repo_ok(f, &d.repo_id)
            {
                continue;
            }
            let id = tx.new_id()?;
            let actor = if d.actor.display.is_empty() {
                d.actor.id.clone()
            } else {
                d.actor.display.clone()
            };
            let rec = DeliveryRec {
                id: id.clone(),
                channel: r.channel,
                principal_id: principal.to_string(),
                to: r.channel_target.clone(),
                reply_to: String::new(),
                subject: "Review requested".into(),
                text: format!("{actor} requested your review: {} {url}", d.title),
                thread_id: subject_key(d.subject.kind, &d.repo_id, &d.subject.id),
                reply_token: String::new(),
                attempts: 0,
                state: delivery_state::QUEUED,
                created_ms: now,
                last_error: String::new(),
                reminder_id: r.id.clone(),
            };
            tx.insert(
                &DELIVERIES,
                &Row::new("", id)
                    .k1(r.channel.to_string())
                    .n1(now as i64)
                    .msg(&rec),
            )
            .await?;
        }
        Ok(())
    }

    // ---- deliveries -------------------------------------------------------------------

    /// Claims due deliveries of a channel (all channels when `Unspecified`) for `lease_ms`;
    /// a claimed delivery that is not completed becomes due again after the lease.
    pub async fn claim_deliveries(
        &self,
        cx: &Cx,
        channel: NotificationReminderChannel,
        limit: usize,
        lease_ms: u64,
    ) -> Result<Vec<DeliveryRec>> {
        let mut tx = self
            .db
            .begin(super::notifications::NOTIFICATIONS, cx)
            .await?;
        let now = tx.now_ms();
        let mut scan = Scan {
            scope: Some(String::new()),
            order: Order::N1,
            n1_max: Some(now as i64),
            limit: limit.max(1),
            ..Scan::default()
        };
        if channel != NotificationReminderChannel::Unspecified {
            scan.key = Some(Key::K1((channel as i32).to_string()));
        }
        let rows = tx.scan(&DELIVERIES, &scan).await?;
        let mut out = Vec::new();
        for mut row in rows {
            let rec: DeliveryRec = row.decode()?;
            row.n1 = Some((now + lease_ms) as i64);
            tx.update(&DELIVERIES, &row).await?;
            out.push(rec);
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Records the outcome of a claimed delivery: sent, or failed with a reason (retried with
    /// exponential backoff until [`NotifyConfig::max_attempts`], then kept as failed).
    pub async fn complete_delivery(
        &self,
        cx: &Cx,
        id: &str,
        outcome: std::result::Result<(), String>,
    ) -> Result<()> {
        let mut tx = self
            .db
            .begin(super::notifications::NOTIFICATIONS, cx)
            .await?;
        let Some(mut row) = tx.get_for_update(&DELIVERIES, "", id).await? else {
            return Err(ForgeError::not_found(format!("delivery {id}")));
        };
        let mut rec: DeliveryRec = row.decode()?;
        let now = tx.now_ms();
        match outcome {
            Ok(()) => {
                rec.state = delivery_state::SENT;
                row.n1 = None;
            }
            Err(why) => {
                rec.attempts += 1;
                rec.last_error = why;
                if rec.attempts >= self.cfg.max_attempts {
                    rec.state = delivery_state::FAILED;
                    row.n1 = None;
                } else {
                    row.n1 = Some(
                        (now + self.cfg.retry_base_ms * (1u64 << (rec.attempts - 1).min(20)))
                            as i64,
                    );
                }
            }
        }
        row.body = rec.encode_to_vec();
        tx.update(&DELIVERIES, &row).await?;
        tx.commit().await
    }

    /// A delivery record (any state).
    pub async fn delivery(&self, cx: &Cx, id: &str) -> Result<DeliveryRec> {
        let mut tx = self
            .db
            .begin(super::notifications::NOTIFICATIONS, cx)
            .await?;
        tx.get_msg::<DeliveryRec>(&DELIVERIES, "", id)
            .await?
            .map(|(r, _)| r)
            .ok_or_else(|| ForgeError::not_found(format!("delivery {id}")))
    }

    // ---- reminders --------------------------------------------------------------------

    /// Sends every due reminder: a digest of the owner's waiting review requests (plus
    /// mentions and assigned issues when the filter asks) goes to the delivery queue and the
    /// schedule moves to the next time. Returns the number of reminders that produced a
    /// message.
    pub async fn fire_due_reminders(&self, cx: &Cx) -> Result<usize> {
        let now = self.db.now_ms();
        let due: Vec<Row> = {
            let mut tx = self
                .db
                .begin(super::notifications::NOTIFICATIONS, cx)
                .await?;
            tx.scan(
                &REMINDERS,
                &Scan {
                    scope: None,
                    order: Order::N1,
                    n1_max: Some(now as i64),
                    limit: 100,
                    ..Scan::default()
                },
            )
            .await?
        };
        let mut sent = 0;
        for row in due {
            let owner = row.scope.clone();
            let mut tx = self
                .db
                .begin(super::notifications::NOTIFICATIONS, cx)
                .await?;
            let Some(mut row) = tx.get_for_update(&REMINDERS, &owner, &row.id).await? else {
                continue;
            };
            if row.n1.is_none_or(|n| n as u64 > now) {
                continue;
            }
            let mut r = row.decode::<ReminderRec>()?.reminder.unwrap_or_default();
            let filter = r.filter.clone().unwrap_or_default();
            let threads = tx
                .scan_msgs::<ThreadRec>(
                    &THREADS,
                    &Scan::scope(owner.clone(), super::notifications::MAX_THREADS),
                )
                .await?;
            let mut lines: Vec<(u64, String)> = Vec::new();
            for (rec, _) in threads {
                let Some(t) = rec.thread else { continue };
                if t.done {
                    continue;
                }
                let has = |x: NotificationReason| t.reasons.contains(&(x as i32));
                let wanted = has(NotificationReason::ReviewRequested)
                    || (filter.include_mentions
                        && (has(NotificationReason::Mention)
                            || has(NotificationReason::TeamMention)))
                    || (filter.include_assigned_issues && has(NotificationReason::Assign));
                if !wanted || !reminder_repo_ok(&filter, &rec.repo_id) {
                    continue;
                }
                if !r.org.is_empty()
                    && rec.org_id != r.org
                    && !t.repo_full_name.starts_with(&format!("{}/", r.org))
                {
                    continue;
                }
                if (filter.ignore_drafts && t.subject_state == "draft")
                    || (filter.ignore_approved && t.subject_state == "approved")
                {
                    continue;
                }
                let at = t.updated_at.as_ref().map(ts_ms).unwrap_or(0);
                let age_h = now.saturating_sub(at) / 3_600_000;
                if age_h < u64::from(filter.min_age_hours) {
                    continue;
                }
                lines.push((
                    at,
                    format!(
                        "- {} ({}) waiting {}h {}",
                        t.title, t.repo_full_name, age_h, t.url
                    )
                    .trim()
                    .to_string(),
                ));
            }
            lines.sort();
            if !lines.is_empty() {
                let id = tx.new_id()?;
                let text = format!(
                    "{} item{} waiting for you:\n{}",
                    lines.len(),
                    if lines.len() == 1 { "" } else { "s" },
                    lines
                        .iter()
                        .map(|(_, l)| l.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                let rec = DeliveryRec {
                    id: id.clone(),
                    channel: r.channel,
                    principal_id: owner.clone(),
                    to: r.channel_target.clone(),
                    reply_to: String::new(),
                    subject: "Reminder: review requests waiting for you".into(),
                    text,
                    thread_id: String::new(),
                    reply_token: String::new(),
                    attempts: 0,
                    state: delivery_state::QUEUED,
                    created_ms: now,
                    last_error: String::new(),
                    reminder_id: r.id.clone(),
                };
                tx.insert(
                    &DELIVERIES,
                    &Row::new("", id)
                        .k1(r.channel.to_string())
                        .n1(now as i64)
                        .msg(&rec),
                )
                .await?;
                sent += 1;
            }
            r.last_sent_at = Some(ts(now));
            row.n1 = Some(next_fire(&r, now)? as i64);
            row.body = ReminderRec {
                reminder: Some(r.clone()),
            }
            .encode_to_vec();
            tx.update(&REMINDERS, &row).await?;
            let payload = serde_json::json!({"reminder_id": r.id, "items": lines.len()});
            emit(
                &mut tx,
                "notification.reminder_sent",
                &r.org,
                "",
                subject(SubjectKind::User, "", &owner),
                payload.clone(),
            )
            .await?;
            tx.audit(draft(
                "notification.reminder_send",
                &r.org,
                "",
                SubjectKind::User,
                &r.id,
                &payload,
            ))
            .await?;
            tx.commit().await?;
        }
        Ok(sent)
    }

    // ---- maintenance ------------------------------------------------------------------

    /// Removes expired reply tokens, finished deliveries older than a week, feed entries older
    /// than a week and done threads untouched for 90 days.
    pub async fn maintain(&self, cx: &Cx) -> Result<()> {
        let now = self.db.now_ms();
        let week = 7 * 24 * 3600 * 1000;
        self.prune_feed(cx, now.saturating_sub(week)).await?;
        let mut tx = self
            .db
            .begin(super::notifications::NOTIFICATIONS, cx)
            .await?;
        for r in tx
            .scan(
                &TOKENS,
                &Scan {
                    scope: Some(String::new()),
                    order: Order::N1,
                    n1_max: Some(now as i64),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?
        {
            tx.delete(&TOKENS, "", &r.id).await?;
        }
        for r in tx
            .scan(
                &DELIVERIES,
                &Scan {
                    scope: Some(String::new()),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
        {
            if r.n1.is_none() && r.decode::<DeliveryRec>()?.created_ms + week < now {
                tx.delete(&DELIVERIES, "", &r.id).await?;
            }
        }
        for r in tx
            .scan(
                &THREADS,
                &Scan {
                    scope: None,
                    order: Order::N1,
                    n1_max: Some(now.saturating_sub(90 * 24 * 3600 * 1000) as i64),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?
        {
            if r.decode::<ThreadRec>()?.thread.is_some_and(|t| t.done) {
                tx.delete(&THREADS, &r.scope, &r.id).await?;
            }
        }
        tx.commit().await
    }

    /// Dispatches, processes until the queue is empty (bounded), and fires due reminders:
    /// deterministic progress for tests and `--dev` tooling.
    pub async fn flush(self: &Arc<Self>, cx: &Cx, events: &EventLog) -> Result<()> {
        self.dispatch_once(cx, events).await?;
        for _ in 0..50 {
            if self.process_jobs(cx).await? == 0 {
                break;
            }
        }
        self.fire_due_reminders(cx).await?;
        Ok(())
    }
}

/// A queued fan-out job.
#[derive(Clone, PartialEq, Message)]
pub struct Job {
    /// The event.
    #[prost(message, optional, tag = "1")]
    pub event: Option<Event>,
    /// Failed attempts.
    #[prost(uint32, tag = "2")]
    pub attempts: u32,
}
