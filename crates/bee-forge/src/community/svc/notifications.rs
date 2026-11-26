//! `NotificationService`: the caller's inbox, subscriptions, repo watches, settings,
//! reminders and reply addresses. Every call acts on the caller's own data, so a token that
//! allows `read` is enough; watching a repo needs read access to it.

use std::time::Duration;

use bee_policy::Action;
use bee_proto::v1::notification_service_server::NotificationService;
use bee_proto::v1::{
    CreateNotificationReminderRequest, DeleteNotificationReminderRequest,
    DeleteNotificationReminderResponse, DeleteRepoWatchRequest, DeleteRepoWatchResponse,
    GetNotificationReminderRequest, GetNotificationReplyAddressRequest,
    GetNotificationSettingsRequest, GetNotificationSubscriptionRequest,
    GetNotificationThreadRequest, GetNotificationUnreadCountRequest,
    GetNotificationUnreadCountResponse, GetRepoWatchRequest, IgnoreNotificationThreadRequest,
    ListNotificationRemindersRequest, ListNotificationRemindersResponse,
    ListNotificationThreadsRequest, ListNotificationThreadsResponse, ListRepoWatchersRequest,
    ListRepoWatchersResponse, ListWatchedReposRequest, ListWatchedReposResponse,
    MarkNotificationThreadsDoneRequest, MarkNotificationThreadsDoneResponse,
    MarkNotificationThreadsReadRequest, MarkNotificationThreadsReadResponse,
    MarkNotificationThreadsUnreadRequest, MarkNotificationThreadsUnreadResponse,
    NotificationInboxEvent, NotificationInboxEventKind, NotificationReminder,
    NotificationReplyAddress, NotificationSettings, NotificationSubscription,
    NotificationSubscriptionState, NotificationThread, NotificationThreadFilter,
    NotificationThreadSelector, RepoRef, RepoWatch, RepoWatchEventKind, RepoWatchLevel,
    SaveNotificationThreadsRequest, SaveNotificationThreadsResponse, SetRepoWatchRequest,
    StreamNotificationInboxRequest, SubjectRef, SubscribeNotificationThreadRequest,
    UnsaveNotificationThreadsRequest, UnsaveNotificationThreadsResponse,
    UnsubscribeNotificationThreadRequest, UpdateNotificationReminderRequest,
    UpdateNotificationSettingsRequest, repo_ref,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::auth::{Caller, OrgPerm};
use crate::community::common::{ref_repo_id, repo_ref_id};
use crate::community::notifications::{Filter, Mark, Selector, ThreadRec};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::service::{ForgeServices, ResponseStream, next, page, reply};

const NONE: &str = "\u{0}none";

impl ForgeServices {
    async fn nfilter(
        &self,
        caller: &Caller,
        f: &Option<NotificationThreadFilter>,
    ) -> Result<Filter> {
        let Some(f) = f else {
            return Ok(Filter::default());
        };
        let mut repo_ids = Vec::new();
        for r in &f.repos {
            match self.fg().orgs.resolve_repo(&caller.cx, r).await {
                Ok(repo) => repo_ids.push(repo.id),
                Err(e) if e.reason == bee_proto::v1::ErrorReason::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        if !f.repos.is_empty() && repo_ids.is_empty() {
            repo_ids.push(NONE.to_string());
        }
        let org_id = if f.org.is_empty() {
            String::new()
        } else {
            match self.fg().orgs.get_org(&caller.cx, &f.org).await {
                Ok(o) => o.id,
                Err(e) if e.reason == bee_proto::v1::ErrorReason::NotFound => NONE.to_string(),
                Err(e) => return Err(e),
            }
        };
        Ok(Filter {
            unread_only: f.unread_only,
            participating_only: f.participating_only,
            reasons: f.reasons.clone(),
            repo_ids,
            saved_only: f.saved_only,
            include_done: f.include_done,
            done_only: f.done_only,
            subject_kinds: f.subject_kinds.clone(),
            since_ms: f.since.as_ref().map(ts_ms).unwrap_or(0),
            before_ms: f.before.as_ref().map(ts_ms).unwrap_or(0),
            org_id,
        })
    }

    async fn nselector(
        &self,
        caller: &Caller,
        s: &Option<NotificationThreadSelector>,
    ) -> Result<(Selector, u64)> {
        let s = s.as_ref().ok_or_else(|| ForgeError::missing("selector"))?;
        let before = s.before.as_ref().map(ts_ms).unwrap_or(0);
        if !s.thread_ids.is_empty() {
            return Ok((
                Selector {
                    thread_ids: s.thread_ids.clone(),
                    filter: None,
                },
                before,
            ));
        }
        let Some(f) = &s.filter else {
            return Err(ForgeError::missing("selector"));
        };
        Ok((
            Selector {
                thread_ids: Vec::new(),
                filter: Some(self.nfilter(caller, &Some(f.clone())).await?),
            },
            before,
        ))
    }

    /// A subject whose repo (if any) is resolved to an id and checked for read access.
    async fn nsubject(&self, caller: &Caller, s: &Option<SubjectRef>) -> Result<SubjectRef> {
        let s = s.as_ref().ok_or_else(|| ForgeError::missing("subject"))?;
        if s.id.is_empty() {
            return Err(ForgeError::missing("subject.id"));
        }
        let mut out = s.clone();
        if s.repo.as_ref().is_some_and(|r| r.repo.is_some()) {
            let repo = self.repo(caller, &s.repo).await?;
            self.fg()
                .authz
                .repo(caller, &repo, Action::Read, &[], None, None)
                .await?;
            out.repo = repo_ref_id(&repo.id);
        }
        Ok(out)
    }

    async fn mark_threads(
        &self,
        caller: &Caller,
        sel: &Option<NotificationThreadSelector>,
        mark: Mark,
    ) -> Result<u32> {
        let cx = self.user_cx(caller, Action::Read)?;
        let (selector, before) = self.nselector(caller, sel).await?;
        self.community()
            .notifications
            .mark(&cx, &selector, mark, before)
            .await
    }

    /// An org id for a reminder owner (name or id).
    async fn reminder_org(&self, caller: &Caller, org: &str, team: bool) -> Result<String> {
        if org.is_empty() {
            return Ok(String::new());
        }
        let o = self.fg().orgs.get_org(&caller.cx, org).await?;
        let perm = if team {
            OrgPerm::Manage
        } else {
            OrgPerm::ViewMembers
        };
        self.fg().authz.org(caller, &o.id, perm).await?;
        Ok(o.id)
    }
}

#[tonic::async_trait]
impl NotificationService for ForgeServices {
    async fn list_notification_threads(
        &self,
        req: Request<ListNotificationThreadsRequest>,
    ) -> Result<Response<ListNotificationThreadsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let f = self.nfilter(&caller, &r.filter).await?;
            let (token, limit) = page(&r.page);
            let (threads, nt) = self
                .community()
                .notifications
                .list(&cx, &f, &token, limit)
                .await?;
            Ok(ListNotificationThreadsResponse {
                threads,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_notification_thread(
        &self,
        req: Request<GetNotificationThreadRequest>,
    ) -> Result<Response<NotificationThread>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            self.community().notifications.get(&cx, &r.thread_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_notification_unread_count(
        &self,
        req: Request<GetNotificationUnreadCountRequest>,
    ) -> Result<Response<GetNotificationUnreadCountResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let f = self.nfilter(&caller, &r.filter).await?;
            let (total, by_reason, by_repo) =
                self.community().notifications.unread_count(&cx, &f).await?;
            Ok(GetNotificationUnreadCountResponse {
                total,
                by_reason: by_reason.into_iter().collect(),
                by_repo: by_repo.into_iter().collect(),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn mark_notification_threads_read(
        &self,
        req: Request<MarkNotificationThreadsReadRequest>,
    ) -> Result<Response<MarkNotificationThreadsReadResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            Ok(MarkNotificationThreadsReadResponse {
                updated_count: self.mark_threads(&caller, &r.selector, Mark::Read).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn mark_notification_threads_unread(
        &self,
        req: Request<MarkNotificationThreadsUnreadRequest>,
    ) -> Result<Response<MarkNotificationThreadsUnreadResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            Ok(MarkNotificationThreadsUnreadResponse {
                updated_count: self
                    .mark_threads(&caller, &r.selector, Mark::Unread)
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn mark_notification_threads_done(
        &self,
        req: Request<MarkNotificationThreadsDoneRequest>,
    ) -> Result<Response<MarkNotificationThreadsDoneResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            Ok(MarkNotificationThreadsDoneResponse {
                updated_count: self.mark_threads(&caller, &r.selector, Mark::Done).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn save_notification_threads(
        &self,
        req: Request<SaveNotificationThreadsRequest>,
    ) -> Result<Response<SaveNotificationThreadsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            Ok(SaveNotificationThreadsResponse {
                updated_count: self.mark_threads(&caller, &r.selector, Mark::Save).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn unsave_notification_threads(
        &self,
        req: Request<UnsaveNotificationThreadsRequest>,
    ) -> Result<Response<UnsaveNotificationThreadsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            Ok(UnsaveNotificationThreadsResponse {
                updated_count: self
                    .mark_threads(&caller, &r.selector, Mark::Unsave)
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_notification_subscription(
        &self,
        req: Request<GetNotificationSubscriptionRequest>,
    ) -> Result<Response<NotificationSubscription>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let s = self.nsubject(&caller, &r.subject).await?;
            self.community().notifications.subscription(&cx, &s).await
        }
        .await;
        reply(&caller, out)
    }

    async fn subscribe_notification_thread(
        &self,
        req: Request<SubscribeNotificationThreadRequest>,
    ) -> Result<Response<NotificationSubscription>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let s = self.nsubject(&caller, &r.subject).await?;
            self.community()
                .notifications
                .set_subscription(&cx, &s, NotificationSubscriptionState::Subscribed)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn unsubscribe_notification_thread(
        &self,
        req: Request<UnsubscribeNotificationThreadRequest>,
    ) -> Result<Response<NotificationSubscription>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let s = self.nsubject(&caller, &r.subject).await?;
            self.community()
                .notifications
                .set_subscription(&cx, &s, NotificationSubscriptionState::Unsubscribed)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn ignore_notification_thread(
        &self,
        req: Request<IgnoreNotificationThreadRequest>,
    ) -> Result<Response<NotificationSubscription>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let s = self.nsubject(&caller, &r.subject).await?;
            self.community()
                .notifications
                .set_subscription(&cx, &s, NotificationSubscriptionState::Ignored)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_repo_watch(
        &self,
        req: Request<SetRepoWatchRequest>,
    ) -> Result<Response<RepoWatch>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let repo = self.repo(&caller, &r.repo).await?;
            self.fg()
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let level = RepoWatchLevel::try_from(r.level).unwrap_or_default();
            let kinds: Vec<RepoWatchEventKind> = r
                .custom_events
                .iter()
                .map(|k| RepoWatchEventKind::try_from(*k).unwrap_or_default())
                .collect();
            let mut w = self
                .community()
                .notifications
                .set_watch(&cx, &repo.id, level, &kinds)
                .await?;
            w.repo = Some(RepoRef {
                repo: Some(repo_ref::Repo::FullName(repo.full_name)),
            });
            Ok(w)
        }
        .await;
        reply(&caller, out)
    }

    async fn get_repo_watch(
        &self,
        req: Request<GetRepoWatchRequest>,
    ) -> Result<Response<RepoWatch>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let repo = self.repo(&caller, &r.repo).await?;
            self.fg()
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let mut w = self.community().notifications.watch(&cx, &repo.id).await?;
            w.repo = Some(RepoRef {
                repo: Some(repo_ref::Repo::FullName(repo.full_name)),
            });
            Ok(w)
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_repo_watch(
        &self,
        req: Request<DeleteRepoWatchRequest>,
    ) -> Result<Response<DeleteRepoWatchResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let repo = self.repo(&caller, &r.repo).await?;
            self.community()
                .notifications
                .delete_watch(&cx, &repo.id)
                .await?;
            Ok(DeleteRepoWatchResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_watched_repos(
        &self,
        req: Request<ListWatchedReposRequest>,
    ) -> Result<Response<ListWatchedReposResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let level = RepoWatchLevel::try_from(r.level).unwrap_or_default();
            let (token, limit) = page(&r.page);
            let (mut watches, nt) = self
                .community()
                .notifications
                .watched(&cx, level, &token, limit)
                .await?;
            watches.retain(|w| !ref_repo_id(&w.repo).is_empty());
            for w in &mut watches {
                if let Ok(repo) = self.fg().orgs.repo_any(&cx, &ref_repo_id(&w.repo)).await {
                    w.repo = Some(RepoRef {
                        repo: Some(repo_ref::Repo::FullName(repo.full_name)),
                    });
                }
            }
            Ok(ListWatchedReposResponse {
                watches,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_repo_watchers(
        &self,
        req: Request<ListRepoWatchersRequest>,
    ) -> Result<Response<ListRepoWatchersResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let repo = self.repo(&caller, &r.repo).await?;
            self.fg()
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let (token, limit) = page(&r.page);
            let (watchers, total_count, nt) = self
                .community()
                .notifications
                .watchers(&cx, &repo.id, &token, limit)
                .await?;
            Ok(ListRepoWatchersResponse {
                watchers,
                total_count,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_notification_settings(
        &self,
        req: Request<GetNotificationSettingsRequest>,
    ) -> Result<Response<NotificationSettings>, Status> {
        let caller = self.caller(&req)?;
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            self.community().notifications.settings(&cx).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_notification_settings(
        &self,
        req: Request<UpdateNotificationSettingsRequest>,
    ) -> Result<Response<NotificationSettings>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let verified = crate::community::notifications::EmailDirectory::verified(
                self.community().users.as_ref(),
                &cx.actor_id,
            )
            .await;
            let check = |e: &str| -> Result<()> {
                if verified.iter().any(|v| v.eq_ignore_ascii_case(e)) {
                    Ok(())
                } else {
                    Err(ForgeError::invalid(
                        "unverified_email",
                        format!("{e} is not one of your verified emails"),
                    ))
                }
            };
            if let Some(e) = &r.default_email
                && !e.is_empty()
            {
                check(e)?;
            }
            let mut routes = Vec::new();
            for rt in &r.email_routes {
                check(&rt.email)?;
                let org = self.fg().orgs.get_org(&cx, &rt.org).await.map_err(|_| {
                    ForgeError::invalid("unknown_org", format!("no org {}", rt.org))
                })?;
                routes.push(bee_proto::v1::NotificationEmailRoute {
                    org: org.id,
                    email: rt.email.to_lowercase(),
                });
            }
            for d in &r.deliveries {
                if bee_proto::v1::NotificationReason::try_from(d.reason).unwrap_or_default()
                    == bee_proto::v1::NotificationReason::Unspecified
                {
                    return Err(ForgeError::invalid(
                        "unknown_reason",
                        "a delivery names an unknown reason",
                    ));
                }
            }
            self.community()
                .notifications
                .update_settings(&cx, move |s| {
                    for d in r.deliveries {
                        match s.deliveries.iter_mut().find(|x| x.reason == d.reason) {
                            Some(x) => *x = d,
                            None => s.deliveries.push(d),
                        }
                    }
                    if let Some(e) = r.default_email {
                        s.default_email = e.to_lowercase();
                    }
                    if r.replace_email_routes {
                        s.email_routes = routes;
                    }
                    if let Some(v) = r.auto_watch_repos {
                        s.auto_watch_repos = v;
                    }
                    if let Some(v) = r.auto_watch_team_repos {
                        s.auto_watch_team_repos = v;
                    }
                    if let Some(v) = r.include_own_activity {
                        s.include_own_activity = v;
                    }
                    if let Some(v) = r.email_ci_failures_only {
                        s.email_ci_failures_only = v;
                    }
                    if let Some(v) = r.reply_by_email {
                        s.reply_by_email = v;
                    }
                    Ok(())
                })
                .await
        }
        .await;
        reply(&caller, out)
    }

    type StreamNotificationInboxStream = ResponseStream<NotificationInboxEvent>;

    async fn stream_notification_inbox(
        &self,
        req: Request<StreamNotificationInboxRequest>,
    ) -> Result<Response<Self::StreamNotificationInboxStream>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let filter = self.nfilter(&caller, &r.filter).await?;
            let forge = self.fg().clone();
            let heartbeat = Duration::from_secs(if r.heartbeat_seconds == 0 {
                30
            } else {
                u64::from(r.heartbeat_seconds).clamp(1, 300)
            });
            // Validate the resume cursor before the stream starts.
            if !r.resume_cursor.is_empty() && r.resume_cursor.parse::<i64>().is_err() {
                return Err(ForgeError::invalid(
                    "bad_cursor",
                    "the resume cursor is not valid",
                ));
            }
            let (tx, rx) = tokio::sync::mpsc::channel::<
                std::result::Result<NotificationInboxEvent, Status>,
            >(64);
            let rid = cx.request_id.clone();
            tokio::spawn(async move {
                let n = forge.community.notifications.clone();
                let wake = n.waiter();
                let fresh = |c: &Cx| c.clone().with_timeout(Duration::from_secs(10));
                let mut cursor = r.resume_cursor.clone();
                let send_err = |e: ForgeError| e.to_status(&rid);
                if r.initial_snapshot && cursor.is_empty() {
                    // Remember where the feed was before listing, so nothing slips between.
                    cursor = match n.feed_head(&fresh(&cx)).await {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(Err(send_err(e))).await;
                            return;
                        }
                    };
                    let mut token = String::new();
                    loop {
                        match n.list(&fresh(&cx), &filter, &token, 200).await {
                            Ok((threads, nt)) => {
                                for t in threads {
                                    let ev = NotificationInboxEvent {
                                        kind: NotificationInboxEventKind::Snapshot as i32,
                                        thread: Some(t),
                                        cursor: String::new(),
                                        at: None,
                                    };
                                    if tx.send(Ok(ev)).await.is_err() {
                                        return;
                                    }
                                }
                                if nt.is_empty() {
                                    break;
                                }
                                token = nt;
                            }
                            Err(e) => {
                                let _ = tx.send(Err(send_err(e))).await;
                                return;
                            }
                        }
                    }
                    let end = NotificationInboxEvent {
                        kind: NotificationInboxEventKind::SnapshotEnd as i32,
                        thread: None,
                        cursor: cursor.clone(),
                        at: None,
                    };
                    if tx.send(Ok(end)).await.is_err() {
                        return;
                    }
                } else if cursor.is_empty() {
                    cursor = n.feed_head(&fresh(&cx)).await.unwrap_or_default();
                }
                let mut last_sent = tokio::time::Instant::now();
                loop {
                    if tx.is_closed() {
                        return;
                    }
                    let events = match n.feed_after(&fresh(&cx), &cursor, 100).await {
                        Ok(e) => e,
                        Err(e) => {
                            let _ = tx.send(Err(send_err(e))).await;
                            return;
                        }
                    };
                    let idle = events.is_empty();
                    for ev in events {
                        cursor = ev.cursor.clone();
                        let removed = ev.kind == NotificationInboxEventKind::Removed as i32;
                        let pass = match &ev.thread {
                            Some(t) if !removed => {
                                let rec: Option<ThreadRec> =
                                    n.thread_rec(&fresh(&cx), &t.id).await.ok().flatten();
                                rec.is_some_and(|rec| {
                                    let mut f = filter.clone();
                                    f.before_ms = 0;
                                    f.since_ms = 0;
                                    f.matches(&rec)
                                })
                            }
                            _ => true,
                        };
                        if pass {
                            if tx.send(Ok(ev)).await.is_err() {
                                return;
                            }
                            last_sent = tokio::time::Instant::now();
                        }
                    }
                    if idle {
                        if last_sent.elapsed() >= heartbeat {
                            let hb = NotificationInboxEvent {
                                kind: NotificationInboxEventKind::Heartbeat as i32,
                                thread: None,
                                cursor: cursor.clone(),
                                at: None,
                            };
                            if tx.send(Ok(hb)).await.is_err() {
                                return;
                            }
                            last_sent = tokio::time::Instant::now();
                        }
                        tokio::select! {
                            _ = wake.notified() => {}
                            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                        }
                    }
                }
            });
            Ok(Box::pin(ReceiverStream::new(rx)) as ResponseStream<NotificationInboxEvent>)
        }
        .await;
        reply(&caller, out)
    }

    async fn create_notification_reminder(
        &self,
        req: Request<CreateNotificationReminderRequest>,
    ) -> Result<Response<NotificationReminder>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let org = self
                .reminder_org(&caller, &r.org, !r.team_id.is_empty())
                .await?;
            if org.is_empty() && !r.team_id.is_empty() {
                return Err(ForgeError::invalid(
                    "team_needs_org",
                    "a team reminder names its org",
                ));
            }
            self.community()
                .notifications
                .create_reminder(
                    &cx,
                    NotificationReminder {
                        org,
                        team_id: r.team_id,
                        channel: r.channel,
                        channel_target: r.channel_target,
                        days: r.days,
                        times: r.times,
                        time_zone: r.time_zone,
                        filter: r.filter,
                        real_time_alerts: r.real_time_alerts,
                        ..Default::default()
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_notification_reminder(
        &self,
        req: Request<GetNotificationReminderRequest>,
    ) -> Result<Response<NotificationReminder>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            self.community()
                .notifications
                .reminder(&cx, &r.reminder_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_notification_reminders(
        &self,
        req: Request<ListNotificationRemindersRequest>,
    ) -> Result<Response<ListNotificationRemindersResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let org = self.reminder_org(&caller, &r.org, false).await?;
            let (token, limit) = page(&r.page);
            let (reminders, nt) = self
                .community()
                .notifications
                .reminders(&cx, &org, &token, limit)
                .await?;
            Ok(ListNotificationRemindersResponse {
                reminders,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_notification_reminder(
        &self,
        req: Request<UpdateNotificationReminderRequest>,
    ) -> Result<Response<NotificationReminder>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            self.community()
                .notifications
                .update_reminder(&cx, &r.reminder_id, move |m| {
                    if let Some(v) = r.channel {
                        m.channel = v;
                    }
                    if let Some(v) = r.channel_target {
                        m.channel_target = v;
                    }
                    if r.replace_schedule {
                        m.days = r.days;
                        m.times = r.times;
                    }
                    if let Some(v) = r.time_zone {
                        m.time_zone = v;
                    }
                    if let Some(v) = r.filter {
                        m.filter = Some(v);
                    }
                    if let Some(v) = r.enabled {
                        m.enabled = v;
                    }
                    if let Some(v) = r.real_time_alerts {
                        m.real_time_alerts = v;
                    }
                })
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_notification_reminder(
        &self,
        req: Request<DeleteNotificationReminderRequest>,
    ) -> Result<Response<DeleteNotificationReminderResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            self.community()
                .notifications
                .delete_reminder(&cx, &r.reminder_id)
                .await?;
            Ok(DeleteNotificationReminderResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn get_notification_reply_address(
        &self,
        req: Request<GetNotificationReplyAddressRequest>,
    ) -> Result<Response<NotificationReplyAddress>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let settings = self.community().notifications.settings(&cx).await?;
            if !settings.reply_by_email {
                return Err(ForgeError::invalid_state(
                    "reply by email is turned off in your notification settings",
                )
                .with_subcode("reply_by_email_off"));
            }
            self.community()
                .notifications
                .reply_address(&cx, &r.thread_id)
                .await
        }
        .await;
        reply(&caller, out)
    }
}
