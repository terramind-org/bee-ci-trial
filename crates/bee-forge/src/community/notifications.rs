//! Notifications (PRODUCT §4.16, AS-COLLAB-02): the per-principal inbox, thread
//! subscriptions, repo watching, settings, reply-by-email addresses and scheduled
//! reminders. The fan-out that fills the inbox from forge events is in
//! [`super::fanout`].
//!
//! # Model
//!
//! A **thread** is one principal's view of one subject (a change request, issue, discussion,
//! release, ...). Its id is the subject key `<kind>:<repo id>:<subject id>`, so new activity
//! on a subject upserts the same row. A thread carries the reasons it applies for
//! (`review_requested`, `mention`, `assign`, `author`, `comment`, `state_change`,
//! `subscribed`, ...), flags (`unread`, `saved`, `done`) and the latest activity. New
//! activity marks a thread unread and takes it out of *done*.
//!
//! | table | scope | id | keys |
//! |---|---|---|---|
//! | threads | principal | subject key | `n1` updated |
//! | subscriptions | subject key | principal | `k1` state |
//! | watches | repo id | principal | `k1` principal |
//! | settings | `""` | principal | |
//! | feed | principal | sequence | `n1` sequence |
//! | reminders | owner | ULID | `n1` next fire |
//! | deliveries | `""` | ULID | `k1` channel, `n1` due |
//!
//! Subscription states: no row = participating only; `SUBSCRIBED` (implicit with a reason,
//! or manual); `UNSUBSCRIBED` and `IGNORED` silence the thread except for direct review
//! requests and `@mentions`. Watch levels follow GitHub: `ALL`, `PARTICIPATING`, `IGNORE`,
//! `CUSTOM` (participating plus chosen event kinds).
//!
//! Every inbox change appends to a per-principal **feed** with a sequence number; the
//! `StreamNotificationInbox` RPC replays it from a cursor and follows it live.
//!
//! # Reply by email
//!
//! A thread's reply address is `reply+<token>@<reply domain>`; the token row maps back to the
//! principal and subject ([`Notifications::resolve_reply`]) and expires after 30 days. The
//! inbound mail worker posts the reply as that principal.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{
    NotificationInboxEvent, NotificationInboxEventKind, NotificationReason,
    NotificationReasonDelivery, NotificationReminder, NotificationReminderChannel,
    NotificationReminderDay, NotificationReminderFilter, NotificationReplyAddress,
    NotificationSettings, NotificationSubscription, NotificationSubscriptionState,
    NotificationThread, Principal, RepoWatch, RepoWatchEventKind, RepoWatchLevel, SubjectKind,
    SubjectRef,
};
use prost::Message;
use tokio::sync::Notify;

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::hooks::PrincipalDirectory;
use crate::ids::{ts, ts_ms};
use crate::module::EventKind;
use crate::store::{Db, Index, Key, Order, Row, Scan, Surface, TableDef, Tx};

use super::common::{draft, emit, page_vec, repo_ref_id, subject};
use super::tz::{Zone, days as civil_days, weekday};

/// The notifications surface: its own pool, so inbox traffic cannot starve other areas.
pub const NOTIFICATIONS: Surface = Surface("notifications");

/// Threads, scoped by principal, keyed by subject key; `n1` updated.
pub static THREADS: TableDef = TableDef {
    name: "forge_notif_threads",
    surface: NOTIFICATIONS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1, Index::ScopeK1],
};
/// Subscriptions, scoped by subject key, keyed by principal.
pub static SUBS: TableDef = TableDef {
    name: "forge_notif_subs",
    surface: NOTIFICATIONS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};
/// Repo watches, scoped by repo id, keyed by principal; `k1` principal.
pub static WATCHES: TableDef = TableDef {
    name: "forge_notif_watches",
    surface: NOTIFICATIONS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::K1, Index::ScopeN1],
};
/// Notification settings per principal.
pub static SETTINGS: TableDef = TableDef {
    name: "forge_notif_settings",
    surface: NOTIFICATIONS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Per-principal inbox feed.
pub static FEED: TableDef = TableDef {
    name: "forge_notif_feed",
    surface: NOTIFICATIONS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1],
};
/// Counters (feed sequences).
pub static COUNTERS: TableDef = TableDef {
    name: "forge_notif_counters",
    surface: NOTIFICATIONS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Reply-by-email tokens.
pub static TOKENS: TableDef = TableDef {
    name: "forge_notif_tokens",
    surface: NOTIFICATIONS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Reminders, scoped by owner (principal id); `n1` next fire (absent while disabled).
pub static REMINDERS: TableDef = TableDef {
    name: "forge_notif_reminders",
    surface: NOTIFICATIONS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1, Index::N1],
};
/// Outgoing deliveries (email, Slack, Teams); `k1` channel, `n1` due.
pub static DELIVERIES: TableDef = TableDef {
    name: "forge_notif_deliveries",
    surface: NOTIFICATIONS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeN1],
};
/// Fan-out jobs: one per event to derive notifications from.
pub static JOBS: TableDef = TableDef {
    name: "forge_notif_jobs",
    surface: NOTIFICATIONS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1],
};
/// Per event-log dispatch cursors.
pub static CURSORS: TableDef = TableDef {
    name: "forge_notif_cursors",
    surface: NOTIFICATIONS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Notification event kinds.
pub const EVENTS: &[EventKind] = &[
    EventKind::new("notification.updated", "Threads of an inbox changed state"),
    EventKind::new(
        "notification.subscription_changed",
        "A thread subscription changed",
    ),
    EventKind::new("notification.watch_changed", "A repo watch changed"),
    EventKind::new(
        "notification.settings_updated",
        "Notification settings changed",
    ),
    EventKind::new(
        "notification.reminder_changed",
        "A reminder was created, changed or deleted",
    ),
    EventKind::new(
        "notification.reminder_sent",
        "A scheduled reminder was sent",
    ),
];

/// Most reminders per owner.
pub const MAX_REMINDERS: usize = 20;
/// Lifetime of a reply token.
pub const REPLY_TTL_MS: u64 = 30 * 24 * 3600 * 1000;
/// Most inbox threads a list or count considers.
pub const MAX_THREADS: usize = 10_000;

/// A stored thread.
#[derive(Clone, PartialEq, Message)]
pub struct ThreadRec {
    /// The thread as returned to clients.
    #[prost(message, optional, tag = "1")]
    pub thread: Option<NotificationThread>,
    /// Org of the subject.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Repo of the subject.
    #[prost(string, tag = "3")]
    pub repo_id: String,
    /// When the reply token expires (ms).
    #[prost(uint64, tag = "4")]
    pub reply_expires_ms: u64,
}

/// A reply token's target.
#[derive(Clone, PartialEq, Message)]
pub struct TokenRec {
    /// Principal the token belongs to.
    #[prost(string, tag = "1")]
    pub principal_id: String,
    /// Thread id.
    #[prost(string, tag = "2")]
    pub thread_id: String,
    /// The subject.
    #[prost(message, optional, tag = "3")]
    pub subject: Option<SubjectRef>,
    /// Expiry (ms).
    #[prost(uint64, tag = "4")]
    pub expires_ms: u64,
}

/// A reminder with its schedule state.
#[derive(Clone, PartialEq, Message)]
pub struct ReminderRec {
    /// The reminder.
    #[prost(message, optional, tag = "1")]
    pub reminder: Option<NotificationReminder>,
}

/// An outgoing delivery.
#[derive(Clone, PartialEq, Message)]
pub struct DeliveryRec {
    /// Delivery id.
    #[prost(string, tag = "1")]
    pub id: String,
    /// [`NotificationReminderChannel`].
    #[prost(int32, tag = "2")]
    pub channel: i32,
    /// Principal the message is for (empty for channel posts).
    #[prost(string, tag = "3")]
    pub principal_id: String,
    /// Address, or Slack/Teams channel target.
    #[prost(string, tag = "4")]
    pub to: String,
    /// `Reply-To` address (reply-by-email).
    #[prost(string, tag = "5")]
    pub reply_to: String,
    /// Subject line.
    #[prost(string, tag = "6")]
    pub subject: String,
    /// Plain-text body.
    #[prost(string, tag = "7")]
    pub text: String,
    /// Thread id (email threading).
    #[prost(string, tag = "8")]
    pub thread_id: String,
    /// Reply token in `reply_to`.
    #[prost(string, tag = "9")]
    pub reply_token: String,
    /// Send attempts so far.
    #[prost(uint32, tag = "10")]
    pub attempts: u32,
    /// 0 queued, 1 sent, 2 failed.
    #[prost(int32, tag = "11")]
    pub state: i32,
    /// Created (ms).
    #[prost(uint64, tag = "12")]
    pub created_ms: u64,
    /// Last error.
    #[prost(string, tag = "13")]
    pub last_error: String,
    /// Reminder that produced it.
    #[prost(string, tag = "14")]
    pub reminder_id: String,
}

/// Delivery states.
pub mod delivery_state {
    /// Waiting to be sent.
    pub const QUEUED: i32 = 0;
    /// Sent.
    pub const SENT: i32 = 1;
    /// Gave up.
    pub const FAILED: i32 = 2;
}

/// Who may see repo content: the fan-out never notifies a principal who cannot read the
/// subject's repo (or is not in the subject's org).
#[async_trait]
pub trait ReadAccess: Send + Sync {
    /// Whether `principal_id` may read repo `repo_id` (or, with an empty repo id, belongs to
    /// org `org_id`).
    async fn can_read(&self, repo_id: &str, org_id: &str, principal_id: &str) -> bool;

    /// The full name of a repo, empty when unknown (used in thread titles and links).
    async fn repo_name(&self, _repo_id: &str) -> String {
        String::new()
    }
}

/// Everyone may read (for stores used without an org layer).
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

#[async_trait]
impl ReadAccess for AllowAll {
    async fn can_read(&self, _: &str, _: &str, _: &str) -> bool {
        true
    }
}

/// Verified email addresses of principals, for email delivery.
#[async_trait]
pub trait EmailDirectory: Send + Sync {
    /// The primary verified address.
    async fn primary_verified(&self, principal_id: &str) -> Option<String>;
    /// Every verified address.
    async fn verified(&self, principal_id: &str) -> Vec<String>;
}

/// No email addresses known: email deliveries are skipped.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEmails;

#[async_trait]
impl EmailDirectory for NoEmails {
    async fn primary_verified(&self, _: &str) -> Option<String> {
        None
    }
    async fn verified(&self, _: &str) -> Vec<String> {
        Vec::new()
    }
}

/// Fan-out and delivery settings.
#[derive(Clone, Debug)]
pub struct NotifyConfig {
    /// Jobs processed concurrently.
    pub concurrency: usize,
    /// Jobs claimed per pass.
    pub batch: usize,
    /// Attempts before a job or delivery is dropped.
    pub max_attempts: u32,
    /// Domain of reply addresses.
    pub reply_domain: String,
    /// `From` address of notification emails.
    pub from_address: String,
    /// Base URL for links.
    pub base_url: String,
    /// First retry delay of a failed delivery (doubles per attempt).
    pub retry_base_ms: u64,
}

impl Default for NotifyConfig {
    fn default() -> NotifyConfig {
        NotifyConfig {
            concurrency: 4,
            batch: 64,
            max_attempts: 5,
            reply_domain: "reply.beehive.invalid".into(),
            from_address: "notifications@beehive.invalid".into(),
            base_url: "https://beehive.invalid".into(),
            retry_base_ms: 30_000,
        }
    }
}

/// The subject key (and thread id) of a subject.
pub fn subject_key(kind: i32, repo_id: &str, id: &str) -> String {
    format!("{kind}:{repo_id}:{id}")
}

/// The subject key of a subject reference whose repo is an id (or absent).
pub fn key_of(s: &SubjectRef) -> String {
    subject_key(s.kind, &super::common::ref_repo_id(&s.repo), &s.id)
}

/// Thread filter with repos and org already resolved to ids.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Only unread.
    pub unread_only: bool,
    /// Only threads with a reason other than watching.
    pub participating_only: bool,
    /// Any of these reasons.
    pub reasons: Vec<i32>,
    /// Any of these repo ids.
    pub repo_ids: Vec<String>,
    /// Only saved.
    pub saved_only: bool,
    /// Include done threads.
    pub include_done: bool,
    /// Only done threads.
    pub done_only: bool,
    /// Any of these subject kinds.
    pub subject_kinds: Vec<i32>,
    /// Updated at or after (ms, 0 = any).
    pub since_ms: u64,
    /// Updated strictly before (ms, 0 = any).
    pub before_ms: u64,
    /// Only this org.
    pub org_id: String,
}

impl Filter {
    /// Whether a stored thread passes the filter.
    pub fn matches(&self, rec: &ThreadRec) -> bool {
        let Some(t) = &rec.thread else { return false };
        if self.done_only {
            if !t.done {
                return false;
            }
        } else if t.done && !self.include_done {
            return false;
        }
        if self.unread_only && !t.unread {
            return false;
        }
        if self.saved_only && !t.saved {
            return false;
        }
        if self.participating_only
            && !t
                .reasons
                .iter()
                .any(|r| *r != NotificationReason::Subscribed as i32)
        {
            return false;
        }
        if !self.reasons.is_empty() && !t.reasons.iter().any(|r| self.reasons.contains(r)) {
            return false;
        }
        if !self.repo_ids.is_empty() && !self.repo_ids.contains(&rec.repo_id) {
            return false;
        }
        if !self.subject_kinds.is_empty()
            && !t
                .subject
                .as_ref()
                .is_some_and(|s| self.subject_kinds.contains(&s.kind))
        {
            return false;
        }
        let at = t.updated_at.as_ref().map(ts_ms).unwrap_or(0);
        if self.since_ms > 0 && at < self.since_ms {
            return false;
        }
        if self.before_ms > 0 && at >= self.before_ms {
            return false;
        }
        if !self.org_id.is_empty() && rec.org_id != self.org_id {
            return false;
        }
        true
    }
}

/// Which threads a bulk call touches.
#[derive(Clone, Debug, Default)]
pub struct Selector {
    /// Explicit thread ids.
    pub thread_ids: Vec<String>,
    /// Or everything matching this filter updated before `before_ms`.
    pub filter: Option<Filter>,
}

/// What a bulk call changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Read.
    Read,
    /// Unread.
    Unread,
    /// Done.
    Done,
    /// Saved.
    Save,
    /// Not saved.
    Unsave,
}

impl Mark {
    fn name(self) -> &'static str {
        match self {
            Mark::Read => "read",
            Mark::Unread => "unread",
            Mark::Done => "done",
            Mark::Save => "save",
            Mark::Unsave => "unsave",
        }
    }
}

/// A thread that does not exist or is not the principal's is `NOT_FOUND`.
fn no_thread(id: &str) -> ForgeError {
    ForgeError::not_found(format!("notification thread {id}"))
}

/// The default settings of a principal.
pub fn default_settings() -> NotificationSettings {
    use NotificationReason as R;
    NotificationSettings {
        deliveries: [
            R::ReviewRequested,
            R::Mention,
            R::TeamMention,
            R::Assign,
            R::Author,
            R::Comment,
            R::StateChange,
            R::CiActivity,
            R::SecurityAlert,
            R::Subscribed,
            R::Manual,
            R::AgentRun,
        ]
        .into_iter()
        .map(|r| NotificationReasonDelivery {
            reason: r as i32,
            web: true,
            email: true,
            mobile: false,
        })
        .collect(),
        default_email: String::new(),
        email_routes: Vec::new(),
        auto_watch_repos: true,
        auto_watch_team_repos: false,
        include_own_activity: false,
        email_ci_failures_only: false,
        reply_by_email: true,
    }
}

/// Notifications over the store.
pub struct Notifications {
    pub(super) db: Db,
    pub(super) directory: Arc<dyn PrincipalDirectory>,
    pub(super) access: Arc<dyn ReadAccess>,
    pub(super) emails: Arc<dyn EmailDirectory>,
    pub(super) cfg: NotifyConfig,
    wake: Arc<Notify>,
}

impl Notifications {
    /// Notifications over `db`.
    pub fn new(
        db: Db,
        directory: Arc<dyn PrincipalDirectory>,
        access: Arc<dyn ReadAccess>,
        emails: Arc<dyn EmailDirectory>,
        cfg: NotifyConfig,
    ) -> Notifications {
        Notifications {
            db,
            directory,
            access,
            emails,
            cfg,
            wake: Arc::new(Notify::new()),
        }
    }

    /// Signalled whenever an inbox changes (stream followers wait on it).
    pub fn waiter(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    pub(super) fn wake(&self) {
        self.wake.notify_waiters();
    }

    /// The configuration.
    pub fn config(&self) -> &NotifyConfig {
        &self.cfg
    }

    // ---- feed -------------------------------------------------------------------------

    pub(super) async fn push_feed(
        tx: &mut Tx,
        principal: &str,
        kind: NotificationInboxEventKind,
        thread: Option<NotificationThread>,
    ) -> Result<String> {
        let seq = tx.add_n1(&COUNTERS, principal, "feed", 1).await?;
        let cursor = format!("{seq:020}");
        let ev = NotificationInboxEvent {
            kind: kind as i32,
            thread,
            cursor: cursor.clone(),
            at: Some(ts(tx.now_ms())),
        };
        tx.insert(&FEED, &Row::new(principal, cursor.clone()).n1(seq).msg(&ev))
            .await?;
        Ok(cursor)
    }

    /// Feed events after `cursor` (empty = from the oldest retained), at most `limit`.
    pub async fn feed_after(
        &self,
        cx: &Cx,
        cursor: &str,
        limit: usize,
    ) -> Result<Vec<NotificationInboxEvent>> {
        let after: i64 = if cursor.is_empty() {
            0
        } else {
            cursor
                .parse()
                .map_err(|_| ForgeError::invalid("bad_cursor", "the resume cursor is not valid"))?
        };
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let rows = tx
            .scan(
                &FEED,
                &Scan::scope(cx.actor_id.clone(), limit.max(1))
                    .by_n1()
                    .n1_min(after + 1),
            )
            .await?;
        rows.iter()
            .map(|r| r.decode::<NotificationInboxEvent>())
            .collect()
    }

    /// The newest feed cursor (empty when nothing was ever delivered).
    pub async fn feed_head(&self, cx: &Cx) -> Result<String> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        Ok(tx
            .get(&COUNTERS, &cx.actor_id, "feed")
            .await?
            .and_then(|r| r.n1)
            .map(|n| format!("{n:020}"))
            .unwrap_or_default())
    }

    /// Removes feed events older than `keep_ms` beyond the newest `keep_min` per principal.
    pub async fn prune_feed(&self, cx: &Cx, older_than_ms: u64) -> Result<usize> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let rows = tx
            .scan(
                &FEED,
                &Scan {
                    scope: None,
                    order: Order::N1,
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        let mut n = 0;
        for r in rows {
            let ev: NotificationInboxEvent = r.decode()?;
            if ev.at.as_ref().map(ts_ms).unwrap_or(0) < older_than_ms {
                tx.delete(&FEED, &r.scope, &r.id).await?;
                n += 1;
            }
        }
        tx.commit().await?;
        Ok(n)
    }

    // ---- threads ----------------------------------------------------------------------

    /// Threads matching a filter, most recently updated first.
    pub async fn list(
        &self,
        cx: &Cx,
        f: &Filter,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<NotificationThread>, String)> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let rows = tx
            .scan_msgs::<ThreadRec>(
                &THREADS,
                &Scan::scope(cx.actor_id.clone(), MAX_THREADS).by_n1().desc(),
            )
            .await?;
        let hits: Vec<NotificationThread> = rows
            .into_iter()
            .filter(|(r, _)| f.matches(r))
            .filter_map(|(r, _)| r.thread)
            .collect();
        page_vec(hits, token, limit)
    }

    /// The stored record of one of the caller's threads.
    pub async fn thread_rec(&self, cx: &Cx, thread_id: &str) -> Result<Option<ThreadRec>> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        Ok(tx
            .get_msg::<ThreadRec>(&THREADS, &cx.actor_id, thread_id)
            .await?
            .map(|(r, _)| r))
    }

    /// One thread of the caller's inbox.
    pub async fn get(&self, cx: &Cx, thread_id: &str) -> Result<NotificationThread> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        tx.get_msg::<ThreadRec>(&THREADS, &cx.actor_id, thread_id)
            .await?
            .and_then(|(r, _)| r.thread)
            .ok_or_else(|| no_thread(thread_id))
    }

    /// Unread counts overall, by reason and by repo (done threads excluded).
    pub async fn unread_count(
        &self,
        cx: &Cx,
        f: &Filter,
    ) -> Result<(u32, BTreeMap<String, u32>, BTreeMap<String, u32>)> {
        let mut f = f.clone();
        f.unread_only = true;
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let rows = tx
            .scan_msgs::<ThreadRec>(&THREADS, &Scan::scope(cx.actor_id.clone(), MAX_THREADS))
            .await?;
        let (mut total, mut by_reason, mut by_repo) = (0u32, BTreeMap::new(), BTreeMap::new());
        for (r, _) in rows.iter().filter(|(r, _)| f.matches(r)) {
            let Some(t) = &r.thread else { continue };
            total += 1;
            let reason = NotificationReason::try_from(t.reason)
                .map(|x| x.as_str_name())
                .unwrap_or("NOTIFICATION_REASON_UNSPECIFIED");
            *by_reason.entry(reason.to_string()).or_insert(0) += 1;
            if !t.repo_full_name.is_empty() {
                *by_repo.entry(t.repo_full_name.clone()).or_insert(0) += 1;
            }
        }
        Ok((total, by_reason, by_repo))
    }

    /// Applies a mark to the selected threads; returns how many changed.
    pub async fn mark(&self, cx: &Cx, sel: &Selector, mark: Mark, before_ms: u64) -> Result<u32> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let mut targets: Vec<(ThreadRec, Row)> = Vec::new();
        if !sel.thread_ids.is_empty() {
            for id in &sel.thread_ids {
                match tx.get_for_update(&THREADS, &cx.actor_id, id).await? {
                    Some(row) => targets.push((row.decode()?, row)),
                    None => return Err(no_thread(id)),
                }
            }
        } else {
            let f = sel
                .filter
                .clone()
                .ok_or_else(|| ForgeError::missing("selector"))?;
            if before_ms == 0 {
                return Err(ForgeError::invalid(
                    "before_required",
                    "a filter selector needs `before`, so threads that arrive meanwhile are untouched",
                ));
            }
            let mut f = f;
            f.before_ms = if f.before_ms == 0 {
                before_ms
            } else {
                f.before_ms.min(before_ms)
            };
            if mark == Mark::Done {
                f.include_done = false;
                f.done_only = false;
            } else {
                f.include_done = true;
            }
            for (r, row) in tx
                .scan_msgs::<ThreadRec>(&THREADS, &Scan::scope(cx.actor_id.clone(), MAX_THREADS))
                .await?
            {
                if f.matches(&r) {
                    targets.push((r, row));
                }
            }
        }
        let now = tx.now_ms();
        let mut changed = 0u32;
        for (mut rec, mut row) in targets {
            let mut t = rec.thread.clone().unwrap_or_default();
            let before = (t.unread, t.saved, t.done);
            match mark {
                Mark::Read => {
                    t.unread = false;
                    t.last_read_at = Some(ts(now));
                }
                Mark::Unread => t.unread = true,
                Mark::Done => {
                    t.done = true;
                    t.unread = false;
                }
                Mark::Save => t.saved = true,
                Mark::Unsave => t.saved = false,
            }
            if (t.unread, t.saved, t.done) == before {
                continue;
            }
            changed += 1;
            let kind = if t.done {
                NotificationInboxEventKind::Removed
            } else {
                NotificationInboxEventKind::Updated
            };
            rec.thread = Some(t.clone());
            row.body = rec.encode_to_vec();
            tx.update(&THREADS, &row).await?;
            Self::push_feed(&mut tx, &cx.actor_id, kind, Some(t)).await?;
        }
        if changed > 0 {
            let payload = serde_json::json!({"principal": cx.actor_id, "action": mark.name(), "count": changed});
            emit(
                &mut tx,
                "notification.updated",
                "",
                "",
                subject(SubjectKind::User, "", &cx.actor_id),
                payload.clone(),
            )
            .await?;
            tx.audit(draft(
                &format!("notification.mark_{}", mark.name()),
                "",
                "",
                SubjectKind::User,
                &cx.actor_id,
                &payload,
            ))
            .await?;
        }
        tx.commit().await?;
        if changed > 0 {
            self.wake();
        }
        Ok(changed)
    }

    // ---- subscriptions ----------------------------------------------------------------

    /// The caller's subscription to a subject (state `UNSPECIFIED` = participating only).
    pub async fn subscription(&self, cx: &Cx, s: &SubjectRef) -> Result<NotificationSubscription> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        Ok(
            match tx
                .get_msg::<NotificationSubscription>(&SUBS, &key_of(s), &cx.actor_id)
                .await?
            {
                Some((sub, _)) => sub,
                None => NotificationSubscription {
                    subject: Some(s.clone()),
                    ..Default::default()
                },
            },
        )
    }

    /// Sets the caller's subscription to a subject.
    pub async fn set_subscription(
        &self,
        cx: &Cx,
        s: &SubjectRef,
        state: NotificationSubscriptionState,
    ) -> Result<NotificationSubscription> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let sub = NotificationSubscription {
            subject: Some(s.clone()),
            state: state as i32,
            reason: if state == NotificationSubscriptionState::Subscribed {
                NotificationReason::Manual as i32
            } else {
                0
            },
            updated_at: Some(ts(tx.now_ms())),
        };
        tx.put(
            &SUBS,
            &Row::new(key_of(s), cx.actor_id.clone())
                .k1((state as i32).to_string())
                .msg(&sub),
        )
        .await?;
        let payload = serde_json::json!({"principal": cx.actor_id, "subject": s, "state": state.as_str_name()});
        emit(
            &mut tx,
            "notification.subscription_changed",
            "",
            "",
            subject(SubjectKind::User, "", &cx.actor_id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "notification.subscription_set",
            "",
            "",
            SubjectKind::User,
            &cx.actor_id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(sub)
    }

    // ---- watches ----------------------------------------------------------------------

    /// Sets how the caller watches a repo; `UNSPECIFIED` stops watching.
    pub async fn set_watch(
        &self,
        cx: &Cx,
        repo_id: &str,
        level: RepoWatchLevel,
        custom: &[RepoWatchEventKind],
    ) -> Result<RepoWatch> {
        match level {
            RepoWatchLevel::Custom if custom.is_empty() => {
                return Err(ForgeError::invalid(
                    "custom_needs_events",
                    "a custom watch names at least one event kind",
                ));
            }
            RepoWatchLevel::Custom => {}
            _ if !custom.is_empty() => {
                return Err(ForgeError::invalid(
                    "custom_only",
                    "custom_events apply to the CUSTOM level only",
                ));
            }
            _ => {}
        }
        if custom.contains(&RepoWatchEventKind::Unspecified) {
            return Err(ForgeError::invalid(
                "invalid_event_kind",
                "unknown watch event kind",
            ));
        }
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let now = tx.now_ms();
        let mut events: Vec<i32> = custom.iter().map(|k| *k as i32).collect();
        events.sort_unstable();
        events.dedup();
        let existing = tx
            .get_msg::<RepoWatch>(&WATCHES, repo_id, &cx.actor_id)
            .await?;
        let w = RepoWatch {
            repo: repo_ref_id(repo_id),
            watcher: Some(cx.principal()),
            level: level as i32,
            custom_events: events,
            created_at: existing
                .as_ref()
                .and_then(|(w, _)| w.created_at)
                .or(Some(ts(now))),
        };
        if level == RepoWatchLevel::Unspecified {
            tx.delete(&WATCHES, repo_id, &cx.actor_id).await?;
        } else {
            tx.put(
                &WATCHES,
                &Row::new(repo_id, cx.actor_id.clone())
                    .k1(cx.actor_id.clone())
                    .n1(now as i64)
                    .msg(&w),
            )
            .await?;
        }
        let payload = serde_json::json!({"principal": cx.actor_id, "repo_id": repo_id, "level": level.as_str_name()});
        emit(
            &mut tx,
            "notification.watch_changed",
            "",
            repo_id,
            subject(SubjectKind::Repo, repo_id, repo_id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "notification.watch_set",
            "",
            repo_id,
            SubjectKind::Repo,
            repo_id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(w)
    }

    /// The caller's watch of a repo (`UNSPECIFIED` level when not watching).
    pub async fn watch(&self, cx: &Cx, repo_id: &str) -> Result<RepoWatch> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        Ok(
            match tx
                .get_msg::<RepoWatch>(&WATCHES, repo_id, &cx.actor_id)
                .await?
            {
                Some((w, _)) => w,
                None => RepoWatch {
                    repo: repo_ref_id(repo_id),
                    watcher: Some(cx.principal()),
                    ..Default::default()
                },
            },
        )
    }

    /// Stops watching.
    pub async fn delete_watch(&self, cx: &Cx, repo_id: &str) -> Result<()> {
        self.set_watch(cx, repo_id, RepoWatchLevel::Unspecified, &[])
            .await
            .map(|_| ())
    }

    /// Watches the caller holds, optionally of one level, newest first.
    pub async fn watched(
        &self,
        cx: &Cx,
        level: RepoWatchLevel,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<RepoWatch>, String)> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let rows = tx
            .scan_msgs::<RepoWatch>(
                &WATCHES,
                &Scan {
                    scope: None,
                    key: Some(Key::K1(cx.actor_id.clone())),
                    order: Order::N1,
                    descending: true,
                    limit: MAX_THREADS,
                    ..Scan::default()
                },
            )
            .await?;
        let v: Vec<RepoWatch> = rows
            .into_iter()
            .map(|(w, _)| w)
            .filter(|w| level == RepoWatchLevel::Unspecified || w.level == level as i32)
            .collect();
        page_vec(v, token, limit)
    }

    /// Principals watching a repo (any level but ignore), with the total.
    pub async fn watchers(
        &self,
        cx: &Cx,
        repo_id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<Principal>, u64, String)> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let rows = tx
            .scan_msgs::<RepoWatch>(&WATCHES, &Scan::scope(repo_id, MAX_THREADS))
            .await?;
        let v: Vec<Principal> = rows
            .into_iter()
            .map(|(w, _)| w)
            .filter(|w| w.level != RepoWatchLevel::Ignore as i32)
            .filter_map(|w| w.watcher)
            .collect();
        let total = v.len() as u64;
        let (page, next) = page_vec(v, token, limit)?;
        Ok((page, total, next))
    }

    // ---- settings ---------------------------------------------------------------------

    /// The caller's settings (defaults until changed).
    pub async fn settings(&self, cx: &Cx) -> Result<NotificationSettings> {
        self.settings_of(cx, &cx.actor_id).await
    }

    pub(super) async fn settings_of(
        &self,
        cx: &Cx,
        principal: &str,
    ) -> Result<NotificationSettings> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        Ok(tx
            .get_msg::<NotificationSettings>(&SETTINGS, "", principal)
            .await?
            .map(|(s, _)| s)
            .unwrap_or_else(default_settings))
    }

    /// Applies `f` to the caller's settings.
    pub async fn update_settings(
        &self,
        cx: &Cx,
        f: impl FnOnce(&mut NotificationSettings) -> Result<()> + Send,
    ) -> Result<NotificationSettings> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let mut s = tx
            .get_msg::<NotificationSettings>(&SETTINGS, "", &cx.actor_id)
            .await?
            .map(|(s, _)| s)
            .unwrap_or_else(default_settings);
        f(&mut s)?;
        tx.put(&SETTINGS, &Row::new("", cx.actor_id.clone()).msg(&s))
            .await?;
        let payload = serde_json::json!({"principal": cx.actor_id, "settings": &s});
        emit(
            &mut tx,
            "notification.settings_updated",
            "",
            "",
            subject(SubjectKind::User, "", &cx.actor_id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "notification.settings_update",
            "",
            "",
            SubjectKind::User,
            &cx.actor_id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    // ---- reply by email ---------------------------------------------------------------

    /// The reply address of a thread, issuing a token when none is valid.
    pub async fn reply_address(
        &self,
        cx: &Cx,
        thread_id: &str,
    ) -> Result<NotificationReplyAddress> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let Some(mut row) = tx.get_for_update(&THREADS, &cx.actor_id, thread_id).await? else {
            return Err(no_thread(thread_id));
        };
        let mut rec: ThreadRec = row.decode()?;
        let now = tx.now_ms();
        let mut t = rec.thread.clone().unwrap_or_default();
        if t.reply_token.is_empty() || rec.reply_expires_ms <= now {
            let token = self.db.ids().random_hex(16)?;
            let expires = now + REPLY_TTL_MS;
            tx.insert(
                &TOKENS,
                &Row::new("", token.clone())
                    .n1(expires as i64)
                    .msg(&TokenRec {
                        principal_id: cx.actor_id.clone(),
                        thread_id: thread_id.to_string(),
                        subject: t.subject.clone(),
                        expires_ms: expires,
                    }),
            )
            .await?;
            t.reply_token = token;
            rec.reply_expires_ms = expires;
            rec.thread = Some(t.clone());
            row.body = rec.encode_to_vec();
            tx.update(&THREADS, &row).await?;
        }
        let expires = rec.reply_expires_ms;
        tx.commit().await?;
        Ok(NotificationReplyAddress {
            thread_id: thread_id.to_string(),
            address: format!("reply+{}@{}", t.reply_token, self.cfg.reply_domain),
            token: t.reply_token,
            expires_at: Some(ts(expires)),
        })
    }

    /// Resolves a reply token to its principal and subject; expired or unknown tokens are
    /// `NOT_FOUND`.
    pub async fn resolve_reply(&self, token: &str) -> Result<TokenRec> {
        let cx = Cx::system();
        let mut tx = self.db.begin(NOTIFICATIONS, &cx).await?;
        match tx.get_msg::<TokenRec>(&TOKENS, "", token).await? {
            Some((t, _)) if t.expires_ms > tx.now_ms() => Ok(t),
            _ => Err(ForgeError::not_found("reply address")),
        }
    }

    // ---- reminders --------------------------------------------------------------------

    fn check_reminder(r: &NotificationReminder) -> Result<()> {
        let channel = NotificationReminderChannel::try_from(r.channel)
            .unwrap_or(NotificationReminderChannel::Unspecified);
        if channel == NotificationReminderChannel::Unspecified {
            return Err(ForgeError::missing("channel"));
        }
        if r.channel_target.trim().is_empty() {
            return Err(ForgeError::missing("channel_target"));
        }
        if channel == NotificationReminderChannel::Email && !r.channel_target.contains('@') {
            return Err(ForgeError::invalid(
                "invalid_email",
                "the target of an email reminder is an address",
            ));
        }
        if r.times.len() > 10 {
            return Err(ForgeError::too_large(
                "reminder_times",
                10,
                r.times.len() as u64,
            ));
        }
        for t in &r.times {
            parse_hhmm(t)?;
        }
        let mut seen = BTreeSet::new();
        for d in &r.days {
            if NotificationReminderDay::try_from(*d).unwrap_or(NotificationReminderDay::Unspecified)
                == NotificationReminderDay::Unspecified
            {
                return Err(ForgeError::invalid("invalid_day", "unknown reminder day"));
            }
            seen.insert(*d);
        }
        if seen.len() != r.days.len() {
            return Err(ForgeError::invalid(
                "duplicate_day",
                "a day is listed twice",
            ));
        }
        Zone::parse(if r.time_zone.is_empty() {
            "UTC"
        } else {
            &r.time_zone
        })?;
        Ok(())
    }

    /// Creates a reminder owned by the caller.
    pub async fn create_reminder(
        &self,
        cx: &Cx,
        mut r: NotificationReminder,
    ) -> Result<NotificationReminder> {
        Self::check_reminder(&r)?;
        if r.time_zone.is_empty() {
            r.time_zone = "UTC".into();
        }
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let mine = tx.count(&REMINDERS, &cx.actor_id, None).await?;
        if mine as usize >= MAX_REMINDERS {
            return Err(ForgeError::too_large(
                "reminders",
                MAX_REMINDERS as u64,
                mine + 1,
            ));
        }
        let now = tx.now_ms();
        r.id = tx.new_id()?;
        r.owner = Some(cx.principal());
        r.enabled = true;
        r.created_at = Some(ts(now));
        let next = next_fire(&r, now)?;
        let mut row = Row::new(cx.actor_id.clone(), r.id.clone()).msg(&ReminderRec {
            reminder: Some(r.clone()),
        });
        row.n1 = Some(next as i64);
        tx.insert(&REMINDERS, &row).await?;
        let payload = serde_json::json!({"reminder": &r, "next_fire_ms": next});
        emit(
            &mut tx,
            "notification.reminder_changed",
            &r.org,
            "",
            subject(SubjectKind::User, "", &cx.actor_id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "notification.reminder_create",
            &r.org,
            "",
            SubjectKind::User,
            &r.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(r)
    }

    /// One reminder of the caller.
    pub async fn reminder(&self, cx: &Cx, id: &str) -> Result<NotificationReminder> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        tx.get_msg::<ReminderRec>(&REMINDERS, &cx.actor_id, id)
            .await?
            .and_then(|(r, _)| r.reminder)
            .ok_or_else(|| ForgeError::not_found(format!("reminder {id}")))
    }

    /// The caller's reminders, personal (`org` empty) or of one org.
    pub async fn reminders(
        &self,
        cx: &Cx,
        org: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<NotificationReminder>, String)> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let rows = tx
            .scan_msgs::<ReminderRec>(&REMINDERS, &Scan::scope(cx.actor_id.clone(), 1000))
            .await?;
        let v: Vec<NotificationReminder> = rows
            .into_iter()
            .filter_map(|(r, _)| r.reminder)
            .filter(|r| r.org == org)
            .collect();
        page_vec(v, token, limit)
    }

    /// Edits a reminder with `f`; the schedule is recomputed.
    pub async fn update_reminder(
        &self,
        cx: &Cx,
        id: &str,
        f: impl FnOnce(&mut NotificationReminder) + Send,
    ) -> Result<NotificationReminder> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let Some(mut row) = tx.get_for_update(&REMINDERS, &cx.actor_id, id).await? else {
            return Err(ForgeError::not_found(format!("reminder {id}")));
        };
        let mut r = row.decode::<ReminderRec>()?.reminder.unwrap_or_default();
        f(&mut r);
        Self::check_reminder(&r)?;
        let now = tx.now_ms();
        row.n1 = if r.enabled {
            Some(next_fire(&r, now)? as i64)
        } else {
            None
        };
        row.body = ReminderRec {
            reminder: Some(r.clone()),
        }
        .encode_to_vec();
        tx.update(&REMINDERS, &row).await?;
        let payload = serde_json::json!({"reminder": &r});
        emit(
            &mut tx,
            "notification.reminder_changed",
            &r.org,
            "",
            subject(SubjectKind::User, "", &cx.actor_id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "notification.reminder_update",
            &r.org,
            "",
            SubjectKind::User,
            &r.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(r)
    }

    /// Deletes a reminder.
    pub async fn delete_reminder(&self, cx: &Cx, id: &str) -> Result<()> {
        let mut tx = self.db.begin(NOTIFICATIONS, cx).await?;
        let Some((rec, _)) = tx
            .get_msg::<ReminderRec>(&REMINDERS, &cx.actor_id, id)
            .await?
        else {
            return Err(ForgeError::not_found(format!("reminder {id}")));
        };
        let r = rec.reminder.unwrap_or_default();
        tx.delete(&REMINDERS, &cx.actor_id, id).await?;
        let payload = serde_json::json!({"reminder_id": id});
        emit(
            &mut tx,
            "notification.reminder_changed",
            &r.org,
            "",
            subject(SubjectKind::User, "", &cx.actor_id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "notification.reminder_delete",
            &r.org,
            "",
            SubjectKind::User,
            id,
            &payload,
        ))
        .await?;
        tx.commit().await
    }

    /// Makes `principal` watch `repo_id` at level `ALL` unless they already chose a level
    /// (the orgs side calls this when someone gains push access and `auto_watch_repos` is on).
    pub async fn auto_watch(&self, principal: &Principal, repo_id: &str) -> Result<bool> {
        let cx = Cx::new(
            principal.id.clone(),
            bee_proto::v1::PrincipalKind::try_from(principal.kind)
                .unwrap_or(bee_proto::v1::PrincipalKind::Human),
        );
        let settings = self.settings_of(&cx, &principal.id).await?;
        if !settings.auto_watch_repos {
            return Ok(false);
        }
        {
            let mut tx = self.db.begin(NOTIFICATIONS, &cx).await?;
            if tx.get(&WATCHES, repo_id, &principal.id).await?.is_some() {
                return Ok(false);
            }
        }
        self.set_watch(&cx, repo_id, RepoWatchLevel::All, &[])
            .await?;
        Ok(true)
    }
}

/// Parses `HH:MM` into minutes after midnight.
pub fn parse_hhmm(t: &str) -> Result<u32> {
    let bad = || {
        ForgeError::invalid(
            "invalid_time",
            format!("{t:?} is not a time of day written HH:MM"),
        )
    };
    let (h, m) = t.split_once(':').ok_or_else(bad)?;
    if h.len() != 2 || m.len() != 2 {
        return Err(bad());
    }
    let (h, m): (u32, u32) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

/// The next time after `now_ms` the reminder fires, in its time zone (UTC milliseconds).
pub fn next_fire(r: &NotificationReminder, now_ms: u64) -> Result<u64> {
    let zone = Zone::parse(if r.time_zone.is_empty() {
        "UTC"
    } else {
        &r.time_zone
    })?;
    let mut times: Vec<u32> = r
        .times
        .iter()
        .map(|t| parse_hhmm(t))
        .collect::<Result<_>>()?;
    if times.is_empty() {
        times.push(9 * 60);
    }
    times.sort_unstable();
    let local_now = zone.to_local(now_ms as i64);
    let today = local_now.div_euclid(86_400_000);
    for off in 0..=8 {
        let day = today + off;
        let iso = (weekday(day) + 6) % 7 + 1; // Monday = 1 .. Sunday = 7
        if !r.days.is_empty() && !r.days.contains(&(iso as i32)) {
            continue;
        }
        for t in &times {
            let local = day * 86_400_000 + i64::from(*t) * 60_000;
            let utc = zone.to_utc(local);
            if utc > now_ms as i64 {
                return Ok(utc as u64);
            }
        }
    }
    Err(ForgeError::internal("reminder has no next fire time"))
}

/// Helper for tests and services: the day number of a civil date.
pub fn day_number(y: i64, m: i64, d: i64) -> i64 {
    civil_days(y, m, d)
}

/// Whether a reminder filter's repo list lets `repo_id` through.
pub fn reminder_repo_ok(f: &NotificationReminderFilter, repo_id: &str) -> bool {
    f.repos.is_empty()
        || f.repos
            .iter()
            .any(|r| super::common::ref_repo_id(&Some(r.clone())) == repo_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reminder_schedule_follows_zone_days_and_times() {
        let r = NotificationReminder {
            days: vec![
                NotificationReminderDay::Monday as i32,
                NotificationReminderDay::Friday as i32,
            ],
            times: vec!["09:00".into(), "15:30".into()],
            time_zone: "America/New_York".into(),
            ..Default::default()
        };
        // 2026-10-18 is a Sunday; the next fire is Monday 09:00 New York (EDT, UTC-4) = 13:00 UTC.
        let now = (civil_days(2026, 10, 18) * 86_400_000 + 12 * 3_600_000) as u64;
        let next = next_fire(&r, now).unwrap();
        assert_eq!(
            next,
            (civil_days(2026, 10, 19) * 86_400_000 + 13 * 3_600_000) as u64
        );
        // Later the same Monday the second time of day fires.
        let after = next_fire(&r, next).unwrap();
        assert_eq!(
            after,
            (civil_days(2026, 10, 19) * 86_400_000 + (15 * 60 + 30 + 240) * 60_000) as u64
        );
    }

    #[test]
    fn times_validate() {
        assert_eq!(parse_hhmm("09:05").unwrap(), 545);
        for bad in ["9:00", "24:00", "09:60", "0900", "ab:cd"] {
            assert!(parse_hhmm(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn filters_default_to_not_done() {
        let rec = |done: bool, unread: bool, reasons: &[NotificationReason]| ThreadRec {
            thread: Some(NotificationThread {
                done,
                unread,
                reasons: reasons.iter().map(|r| *r as i32).collect(),
                updated_at: Some(ts(1000)),
                ..Default::default()
            }),
            ..Default::default()
        };
        let f = Filter::default();
        assert!(f.matches(&rec(false, true, &[NotificationReason::Mention])));
        assert!(!f.matches(&rec(true, false, &[NotificationReason::Mention])));
        let participating = Filter {
            participating_only: true,
            ..Default::default()
        };
        assert!(!participating.matches(&rec(false, true, &[NotificationReason::Subscribed])));
        assert!(participating.matches(&rec(
            false,
            true,
            &[NotificationReason::Subscribed, NotificationReason::Author]
        )));
    }
}
