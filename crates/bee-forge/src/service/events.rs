//! `EventService`: subscriptions and event log paging (read access to the repo, or
//! membership of the org for org logs) and webhooks (repo admin, or org owner/admin).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use bee_policy::Action;
use bee_proto::v1::event_service_server::EventService;
use bee_proto::v1::{
    CreateWebhookRequest, DeleteWebhookRequest, DeleteWebhookResponse, Event, EventLogInfo,
    GetEventLogInfoRequest, GetWebhookDeliveryRequest, GetWebhookRequest, ListEventsRequest,
    ListEventsResponse, ListWebhookDeliveriesRequest, ListWebhookDeliveriesResponse,
    ListWebhooksRequest, ListWebhooksResponse, PingWebhookRequest, RedeliverWebhookRequest,
    RepoRef, RepoRole, SubscribeRequest, UpdateWebhookRequest, Webhook, WebhookDelivery,
    WebhookFormat,
};
use prost::Message as _;
use tokio::time::Instant;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::{ForgeServices, ResponseStream, next, page, reply};
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::events::webhooks::{HookOwner, HookPatch, NewHook};
use crate::events::{org_log, repo_log};

/// How often an open subscription re-decides read access.
pub const REAUTHORIZE_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// Runs an access decision at most once a second for an open stream.
pub(crate) async fn reauthorize_every<F, Fut>(
    checked: &mut tokio::time::Instant,
    check: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    if checked.elapsed() >= REAUTHORIZE_EVERY {
        check().await?;
        *checked = tokio::time::Instant::now();
    }
    Ok(())
}

/// The last access decision of the streams that share one: when it was made and how it went.
type Decided = tokio::sync::Mutex<Option<(Instant, Result<()>)>>;

/// The access decisions open subscriptions share.
///
/// Every open subscription re-decides read access each [`REAUTHORIZE_EVERY`] (SPEC: at least
/// every second while open). Done per stream, 10,000 open streams asked the forge database for
/// 10,000 decisions a second, which starved every other forge call: lands ran past their
/// forge deadline and events reached subscribers 12 to 20 seconds late. A decision depends on
/// the credential, the caller's address and the log, not on the stream, so the streams that
/// have those in common take turns: the first whose decision is a second old decides again and
/// the others adopt that decision and its time. Each stream still never forwards an event on a
/// decision older than a second, and a refusal ends every one of them.
#[derive(Default)]
pub(crate) struct StreamAccess {
    shared: Mutex<HashMap<StreamKey, Weak<Decided>>>,
    decisions: AtomicU64,
}

/// What an access decision for an open stream depends on: the bearer credential, the
/// caller's address (network allow lists), and the repo or org log named.
type StreamKey = (String, String, Vec<u8>, String);

impl StreamAccess {
    pub(crate) fn decisions(&self) -> u64 {
        self.decisions.load(Ordering::Relaxed)
    }

    /// The decision shared by the streams of `caller` on the log `repo` or `org` names.
    fn of(&self, caller: &Caller, repo: &Option<RepoRef>, org: &str) -> Arc<Decided> {
        let key = (
            caller.credential().to_string(),
            caller.cx.ip.clone(),
            repo.as_ref().map(|r| r.encode_to_vec()).unwrap_or_default(),
            org.to_string(),
        );
        let mut shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(d) = shared.get(&key).and_then(Weak::upgrade) {
            return d;
        }
        // Entries of streams that have all ended are dropped as the map grows.
        if shared.len() >= 1024 && shared.len().is_power_of_two() {
            shared.retain(|_, d| d.strong_count() > 0);
        }
        let d = Arc::new(Decided::default());
        shared.insert(key, Arc::downgrade(&d));
        d
    }
}

impl ForgeServices {
    /// Decides again whether `caller` may read the log, or adopts the decision another stream
    /// sharing `decided` made within the last [`REAUTHORIZE_EVERY`]. Returns when the decision
    /// in force was made.
    async fn redecide(
        &self,
        decided: &Decided,
        caller: &Caller,
        repo: &Option<RepoRef>,
        org: &str,
    ) -> Result<Instant> {
        let mut last = decided.lock().await;
        if let Some((at, result)) = last.as_ref()
            && at.elapsed() < REAUTHORIZE_EVERY
        {
            return result.clone().map(|()| *at);
        }
        self.stream_access.decisions.fetch_add(1, Ordering::Relaxed);
        let result = async {
            let fresh = self.fresh(caller)?;
            self.readable_log(&fresh, repo, org).await.map(|_| ())
        }
        .await;
        let at = Instant::now();
        *last = Some((at, result.clone()));
        result.map(|()| at)
    }

    /// The log a request names, authorized for reading.
    async fn readable_log(
        &self,
        caller: &Caller,
        repo: &Option<RepoRef>,
        org: &str,
    ) -> Result<(String, Cx)> {
        if !org.is_empty() {
            let org_id = self.org_id(caller, org).await?;
            let cx = self
                .forge
                .authz
                .org(caller, &org_id, OrgPerm::ViewMembers)
                .await?;
            return Ok((org_log(&org_id), cx));
        }
        let repo = self.repo(caller, repo).await?;
        let cx = self
            .forge
            .authz
            .repo(caller, &repo, Action::Read, &[], None, None)
            .await?;
        Ok((repo_log(&repo.id), cx))
    }

    /// Forwards a subscription's events while the caller may still read the log: read access
    /// is decided again at most every [`REAUTHORIZE_EVERY`] (and at least that often while
    /// idle), so a repo deleted, made private or out of the caller's reach ends the stream
    /// with the refusal (`NOT_FOUND`, `PERMISSION_DENIED`) instead of streaming on. Streams of
    /// one credential on one log share each decision ([`StreamAccess`]).
    fn reauthorized(
        &self,
        caller: &Caller,
        repo: &Option<RepoRef>,
        org: &str,
        mut upstream: tokio::sync::mpsc::Receiver<Result<Event>>,
    ) -> tokio::sync::mpsc::Receiver<Result<Event>> {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let me = self.clone();
        let decided = self.stream_access.of(caller, repo, org);
        let caller = caller.clone();
        let repo = repo.clone();
        let org = org.to_string();
        // The request's deadline bounded the call that opened the stream, not the stream: each
        // access check runs under a deadline of its own (SPEC §15.3), so a subscription that
        // outlives the request's budget keeps going while the caller may read the log.
        tokio::spawn(async move {
            // The decision that opened the stream.
            let mut checked = Instant::now();
            loop {
                let item = tokio::select! {
                    item = upstream.recv() => item,
                    _ = tokio::time::sleep_until(checked + REAUTHORIZE_EVERY) => {
                        match me.redecide(&decided, &caller, &repo, &org).await {
                            Ok(at) => checked = at,
                            Err(e) => {
                                let _ = tx.send(Err(e)).await;
                                return;
                            }
                        }
                        continue;
                    }
                };
                let Some(item) = item else { return };
                if checked.elapsed() >= REAUTHORIZE_EVERY {
                    match me.redecide(&decided, &caller, &repo, &org).await {
                        Ok(at) => checked = at,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    }
                }
                if tx.send(item).await.is_err() {
                    return;
                }
            }
        });
        rx
    }

    /// The webhook owner a request names, authorized for managing webhooks.
    async fn hook_owner(
        &self,
        caller: &Caller,
        repo: &Option<RepoRef>,
        org: &str,
        action: &str,
    ) -> Result<(HookOwner, Cx)> {
        if !org.is_empty() {
            let org_id = self.org_id(caller, org).await?;
            let auth = self.forge.authz.org(caller, &org_id, OrgPerm::Manage).await;
            let cx = self.guarded(caller, &org_id, "", action, auth).await?;
            return Ok((HookOwner::org(&org_id), cx));
        }
        let repo = self.repo(caller, repo).await?;
        let auth = self
            .forge
            .authz
            .repo(
                caller,
                &repo,
                Action::ManageSettings,
                &[],
                None,
                Some(RepoRole::Admin),
            )
            .await;
        let cx = self
            .guarded(caller, &repo.org_id, &repo.id, action, auth)
            .await?;
        Ok((HookOwner::repo(&repo.id, &repo.org_id), cx))
    }
}

#[tonic::async_trait]
impl EventService for ForgeServices {
    type SubscribeStream = ResponseStream<Event>;

    async fn subscribe(
        &self,
        req: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (log, cx) = self.readable_log(&caller, &r.repo, &r.org).await?;
            for k in &r.kinds {
                if !self.forge.registry.filter_known(k) {
                    return Err(ForgeError::invalid(
                        "unknown_event_kind",
                        format!("unknown event kind filter {k:?}"),
                    ));
                }
            }
            let upstream = self
                .forge
                .events
                .subscribe(&cx, &log, r.from_seq, r.kinds)
                .await?;
            let rx = self.reauthorized(&caller, &r.repo, &r.org, upstream);
            let rid = cx.request_id.clone();
            let stream =
                ReceiverStream::new(rx).map(move |item| item.map_err(|e| e.to_status(&rid)));
            Ok(Box::pin(stream) as ResponseStream<Event>)
        }
        .await;
        reply(&caller, out)
    }

    async fn create_webhook(
        &self,
        req: Request<CreateWebhookRequest>,
    ) -> Result<Response<Webhook>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self
                .hook_owner(&caller, &r.repo, &r.org, "webhook.create")
                .await?;
            self.forge
                .webhooks
                .create(
                    &cx,
                    &owner,
                    NewHook {
                        url: r.url,
                        kinds: r.kinds,
                        secret: r.secret,
                        format: WebhookFormat::try_from(r.format).unwrap_or(WebhookFormat::Bee),
                        active: !r.inactive,
                        insecure_ssl: r.insecure_ssl,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_webhook(
        &self,
        req: Request<DeleteWebhookRequest>,
    ) -> Result<Response<DeleteWebhookResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self
                .hook_owner(&caller, &r.repo, &r.org, "webhook.delete")
                .await?;
            self.forge.webhooks.delete(&cx, &owner, &r.id).await?;
            Ok(DeleteWebhookResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_webhooks(
        &self,
        req: Request<ListWebhooksRequest>,
    ) -> Result<Response<ListWebhooksResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self
                .hook_owner(&caller, &r.repo, &r.org, "webhook.list")
                .await?;
            let (after, size) = page(&r.page);
            let (webhooks, token) = self.forge.webhooks.list(&cx, &owner, &after, size).await?;
            Ok(ListWebhooksResponse {
                webhooks,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_webhook(
        &self,
        req: Request<GetWebhookRequest>,
    ) -> Result<Response<Webhook>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self
                .hook_owner(&caller, &r.repo, &r.org, "webhook.get")
                .await?;
            self.forge.webhooks.get(&cx, &owner, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_webhook(
        &self,
        req: Request<UpdateWebhookRequest>,
    ) -> Result<Response<Webhook>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self
                .hook_owner(&caller, &r.repo, &r.org, "webhook.update")
                .await?;
            let patch = HookPatch {
                url: r.url,
                kinds: r.set_kinds.then_some(r.kinds),
                secret: r.secret,
                format: r
                    .format
                    .map(|f| WebhookFormat::try_from(f).unwrap_or(WebhookFormat::Bee)),
                active: r.active,
                insecure_ssl: r.insecure_ssl,
            };
            self.forge.webhooks.update(&cx, &owner, &r.id, patch).await
        }
        .await;
        reply(&caller, out)
    }

    async fn ping_webhook(
        &self,
        req: Request<PingWebhookRequest>,
    ) -> Result<Response<WebhookDelivery>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self
                .hook_owner(&caller, &r.repo, &r.org, "webhook.ping")
                .await?;
            self.forge.webhooks.ping(&cx, &owner, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_webhook_deliveries(
        &self,
        req: Request<ListWebhookDeliveriesRequest>,
    ) -> Result<Response<ListWebhookDeliveriesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self
                .hook_owner(&caller, &r.repo, &r.org, "webhook.deliveries")
                .await?;
            let (after, size) = page(&r.page);
            let (deliveries, token) = self
                .forge
                .webhooks
                .deliveries(&cx, &owner, &r.webhook_id, r.success, &after, size)
                .await?;
            Ok(ListWebhookDeliveriesResponse {
                deliveries,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_webhook_delivery(
        &self,
        req: Request<GetWebhookDeliveryRequest>,
    ) -> Result<Response<WebhookDelivery>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self
                .hook_owner(&caller, &r.repo, &r.org, "webhook.delivery")
                .await?;
            self.forge
                .webhooks
                .delivery(&cx, &owner, &r.webhook_id, &r.delivery_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn redeliver_webhook(
        &self,
        req: Request<RedeliverWebhookRequest>,
    ) -> Result<Response<WebhookDelivery>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self
                .hook_owner(&caller, &r.repo, &r.org, "webhook.redeliver")
                .await?;
            self.forge
                .webhooks
                .redeliver(&cx, &owner, &r.webhook_id, &r.delivery_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_events(
        &self,
        req: Request<ListEventsRequest>,
    ) -> Result<Response<ListEventsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (log, cx) = self.readable_log(&caller, &r.repo, &r.org).await?;
            let (events, next_seq) = self
                .forge
                .events
                .read(&cx, &log, r.from_seq, &r.kinds, r.limit as usize)
                .await?;
            Ok(ListEventsResponse { events, next_seq })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_event_log_info(
        &self,
        req: Request<GetEventLogInfoRequest>,
    ) -> Result<Response<EventLogInfo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (log, cx) = self.readable_log(&caller, &r.repo, &r.org).await?;
            let info = self.forge.events.info(&cx, &log).await?;
            Ok(EventLogInfo {
                oldest_seq: info.oldest_seq,
                latest_seq: info.latest_seq,
                retention_days: info.retention_days,
            })
        }
        .await;
        reply(&caller, out)
    }
}
