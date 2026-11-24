//! Repo and org webhooks: CRUD, the dispatcher that turns log events into delivery jobs, the
//! bounded delivery worker, the delivery log, redelivery and ping.
//!
//! Webhooks own the `webhooks` surface: their queue and log never share a connection pool
//! with the event log, leases or checks, and deliveries run on their own bounded worker pool
//! ([`DeliveryConfig::concurrency`]), so a slow receiver backs up only webhook delivery
//! (SPEC §15.1).
//!
//! Delivery is at least once: the dispatcher advances its per-log cursor in the same
//! transaction that queues the jobs; a job is claimed with a version CAS before sending, and
//! deleted (or rescheduled) after the attempt is logged.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bee_proto::v1::{Event, Webhook, WebhookDelivery, WebhookFormat};
use prost::Message;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::delivery::{HookTarget, Outcome, Prepared, RetryPolicy, Sender, prepare, reprepare};
use super::{EventLog, kind_matches};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::module::{EventKind, ForgeModule, Registry};
use crate::store::{
    Cursor, Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, Surface, TableDef,
};

/// Most webhooks per repo or org.
pub const MAX_HOOKS_PER_OWNER: u64 = 20;

/// Webhooks, scoped by owner log key.
pub static HOOKS: TableDef = TableDef {
    name: "forge_webhooks",
    surface: Surface::WEBHOOKS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::K1],
};

/// Pending delivery jobs, global scope, ordered by due time.
pub static JOBS: TableDef = TableDef {
    name: "forge_webhook_jobs",
    surface: Surface::WEBHOOKS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::N1, Index::K1],
};

/// Delivery log, scoped by webhook id; `k1` is `ok` or `failed`.
pub static DELIVERIES: TableDef = TableDef {
    name: "forge_webhook_deliveries",
    surface: Surface::WEBHOOKS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Dispatcher cursors per owner log.
pub static CURSORS: TableDef = TableDef {
    name: "forge_webhook_cursors",
    surface: Surface::WEBHOOKS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

static M1: Migration = Migration {
    module: "webhooks",
    version: 1,
    name: "create_webhooks",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&HOOKS),
        MigrationOp::CreateTable(&JOBS),
        MigrationOp::CreateTable(&DELIVERIES),
        MigrationOp::CreateTable(&CURSORS),
    ],
};

/// What the webhooks module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "webhooks",
    surfaces: &[Surface::WEBHOOKS],
    migrations: &[&M1],
    event_kinds: &[EventKind::gh("ping", "Sent by PingWebhook", "ping", "")],
    audit_prefixes: &["webhook."],
};

/// Stored webhook with its write-only secret.
#[derive(Clone, PartialEq, Message)]
pub struct HookRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub webhook: Option<Webhook>,
    /// HMAC key.
    #[prost(string, tag = "2")]
    pub secret: String,
}

/// A queued delivery.
#[derive(Clone, PartialEq, Message)]
pub struct Job {
    /// Webhook id.
    #[prost(string, tag = "1")]
    pub webhook_id: String,
    /// Owner log key.
    #[prost(string, tag = "2")]
    pub owner: String,
    /// The event.
    #[prost(message, optional, tag = "3")]
    pub event: Option<Event>,
    /// Attempt number of the next try (1-based).
    #[prost(uint32, tag = "4")]
    pub attempt: u32,
    /// Whether this is a manual redelivery.
    #[prost(bool, tag = "5")]
    pub redelivery: bool,
}

/// Who owns a webhook.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookOwner {
    /// Log key: a repo id, or `org:<id>`.
    pub log: String,
    /// Repo id (empty for org hooks).
    pub repo_id: String,
    /// Org id.
    pub org_id: String,
}

impl HookOwner {
    /// A repo's hooks.
    pub fn repo(repo_id: &str, org_id: &str) -> HookOwner {
        HookOwner {
            log: super::repo_log(repo_id),
            repo_id: repo_id.into(),
            org_id: org_id.into(),
        }
    }

    /// An org's hooks.
    pub fn org(org_id: &str) -> HookOwner {
        HookOwner {
            log: super::org_log(org_id),
            repo_id: String::new(),
            org_id: org_id.into(),
        }
    }
}

/// Fields of a new webhook.
#[derive(Clone, Debug, Default)]
pub struct NewHook {
    /// Receiver URL (`http://` or `https://`).
    pub url: String,
    /// Kind filters (empty = all).
    pub kinds: Vec<String>,
    /// HMAC key.
    pub secret: String,
    /// Payload format.
    pub format: WebhookFormat,
    /// Created enabled.
    pub active: bool,
    /// Stored; certificates are always verified.
    pub insecure_ssl: bool,
}

/// A partial update.
#[derive(Clone, Debug, Default)]
pub struct HookPatch {
    /// New URL.
    pub url: Option<String>,
    /// New kind filters.
    pub kinds: Option<Vec<String>>,
    /// New secret.
    pub secret: Option<String>,
    /// New format.
    pub format: Option<WebhookFormat>,
    /// Enable or disable.
    pub active: Option<bool>,
    /// Stored flag.
    pub insecure_ssl: Option<bool>,
}

/// Delivery worker settings.
#[derive(Clone, Copy, Debug)]
pub struct DeliveryConfig {
    /// Concurrent HTTP requests (the bounded pool).
    pub concurrency: usize,
    /// Retry schedule.
    pub retry: RetryPolicy,
    /// Per-request timeout.
    pub timeout: Duration,
    /// How long a claimed job is hidden from other workers.
    pub claim: Duration,
}

impl Default for DeliveryConfig {
    fn default() -> DeliveryConfig {
        DeliveryConfig {
            concurrency: 8,
            retry: RetryPolicy::default(),
            timeout: Duration::from_secs(10),
            claim: Duration::from_secs(60),
        }
    }
}

fn validate_url(url: &str) -> Result<()> {
    let uri: http::Uri = url
        .parse()
        .map_err(|_| ForgeError::invalid("bad_url", "webhook url does not parse"))?;
    match uri.scheme_str() {
        Some("http") | Some("https") if uri.host().is_some() => Ok(()),
        _ => Err(ForgeError::invalid(
            "bad_url",
            "webhook url must be http:// or https:// with a host",
        )),
    }
}

/// Webhooks over the store.
pub struct Webhooks {
    db: Db,
    log: Arc<EventLog>,
    registry: Arc<Registry>,
    cfg: DeliveryConfig,
    sender: Sender,
    pool: Arc<Semaphore>,
}

impl Webhooks {
    /// Webhooks reading `log`, validating kinds against `registry`.
    pub fn new(
        db: Db,
        log: Arc<EventLog>,
        registry: Arc<Registry>,
        cfg: DeliveryConfig,
    ) -> Webhooks {
        Webhooks {
            sender: Sender::new(cfg.timeout),
            pool: Arc::new(Semaphore::new(cfg.concurrency.max(1))),
            db,
            log,
            registry,
            cfg,
        }
    }

    fn check_kinds(&self, kinds: &[String]) -> Result<()> {
        for k in kinds {
            if !self.registry.filter_known(k) {
                return Err(ForgeError::invalid(
                    "unknown_event_kind",
                    format!("unknown event kind filter {k:?}"),
                ));
            }
        }
        Ok(())
    }

    async fn last_delivery(&self, cx: &Cx, hook_id: &str) -> Result<Option<WebhookDelivery>> {
        let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
        let rows = tx
            .scan(&DELIVERIES, &Scan::scope(hook_id, 1).desc())
            .await?;
        Ok(match rows.first() {
            Some(r) => Some(summary(r.decode()?)),
            None => None,
        })
    }

    /// Creates a webhook.
    pub async fn create(&self, cx: &Cx, owner: &HookOwner, new: NewHook) -> Result<Webhook> {
        validate_url(&new.url)?;
        self.check_kinds(&new.kinds)?;
        let latest = self.log.info(cx, &owner.log).await?.latest_seq;
        let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
        tx.add_n1(&CURSORS, &format!("hook_cap:{}", owner.log), "create", 0)
            .await?;
        if tx.count(&HOOKS, &owner.log, None).await? >= MAX_HOOKS_PER_OWNER {
            return Err(ForgeError::too_large(
                "webhooks_per_owner",
                MAX_HOOKS_PER_OWNER,
                MAX_HOOKS_PER_OWNER + 1,
            ));
        }
        let id = tx.new_id()?;
        let now = tx.now_ms();
        let hook = Webhook {
            id: id.clone(),
            repo_id: owner.repo_id.clone(),
            url: new.url,
            kinds: new.kinds,
            org_id: owner.org_id.clone(),
            app_id: String::new(),
            format: new.format as i32,
            active: new.active,
            created_at: Some(ts(now)),
            last_delivery: None,
            insecure_ssl: new.insecure_ssl,
        };
        let rec = HookRecord {
            webhook: Some(hook.clone()),
            secret: new.secret,
        };
        tx.insert(
            &HOOKS,
            &Row::new(owner.log.clone(), id.clone())
                .k1(id.clone())
                .n1(now as i64)
                .msg(&rec),
        )
        .await?;
        if tx.get(&CURSORS, "", &owner.log).await?.is_none() {
            tx.put(&CURSORS, &Row::new("", owner.log.clone()).n1(latest as i64))
                .await?;
        }
        tx.audit(
            AuditDraft::new("webhook.create", &owner.org_id)
                .repo_id(&owner.repo_id)
                .after(&json_hook(&hook)),
        )
        .await?;
        tx.commit().await?;
        Ok(hook)
    }

    async fn load(&self, cx: &Cx, owner: &HookOwner, id: &str) -> Result<(HookRecord, Row)> {
        let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
        match tx.get_msg::<HookRecord>(&HOOKS, &owner.log, id).await? {
            Some(x) => Ok(x),
            None => Err(ForgeError::not_found(format!("webhook {id}"))),
        }
    }

    /// Reads one webhook with its last delivery.
    pub async fn get(&self, cx: &Cx, owner: &HookOwner, id: &str) -> Result<Webhook> {
        let (rec, _) = self.load(cx, owner, id).await?;
        let mut hook = rec.webhook.unwrap_or_default();
        hook.last_delivery = self.last_delivery(cx, id).await?;
        Ok(hook)
    }

    /// Lists an owner's webhooks (by id), paged.
    pub async fn list(
        &self,
        cx: &Cx,
        owner: &HookOwner,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Webhook>, String)> {
        let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
        let cursor = (!after.is_empty()).then(|| Cursor {
            n1: 0,
            scope: owner.log.clone(),
            id: after.to_string(),
        });
        let rows = tx
            .scan_msgs::<HookRecord>(
                &HOOKS,
                &Scan::scope(owner.log.clone(), limit + 1).after(cursor),
            )
            .await?;
        drop(tx);
        let more = rows.len() > limit;
        let mut out = Vec::new();
        for (rec, _) in rows.into_iter().take(limit) {
            let mut h = rec.webhook.unwrap_or_default();
            h.last_delivery = self.last_delivery(cx, &h.id).await?;
            out.push(h);
        }
        let next = if more {
            out.last().map(|h| h.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((out, next))
    }

    /// Updates a webhook.
    pub async fn update(
        &self,
        cx: &Cx,
        owner: &HookOwner,
        id: &str,
        patch: HookPatch,
    ) -> Result<Webhook> {
        if let Some(u) = &patch.url {
            validate_url(u)?;
        }
        if let Some(k) = &patch.kinds {
            self.check_kinds(k)?;
        }
        let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
        let Some((mut rec, row)) = tx.get_msg::<HookRecord>(&HOOKS, &owner.log, id).await? else {
            return Err(ForgeError::not_found(format!("webhook {id}")));
        };
        let mut hook = rec.webhook.clone().unwrap_or_default();
        let before = json_hook(&hook);
        if let Some(u) = patch.url {
            hook.url = u;
        }
        if let Some(k) = patch.kinds {
            hook.kinds = k;
        }
        if let Some(f) = patch.format {
            hook.format = f as i32;
        }
        if let Some(a) = patch.active {
            hook.active = a;
        }
        if let Some(i) = patch.insecure_ssl {
            hook.insecure_ssl = i;
        }
        if let Some(s) = patch.secret {
            rec.secret = s;
        }
        rec.webhook = Some(hook.clone());
        tx.update(&HOOKS, &row.clone().msg(&rec)).await?;
        tx.audit(
            AuditDraft::new("webhook.update", &owner.org_id)
                .repo_id(&owner.repo_id)
                .before(&before)
                .after(&json_hook(&hook)),
        )
        .await?;
        tx.commit().await?;
        Ok(hook)
    }

    /// Deletes a webhook and its pending jobs.
    pub async fn delete(&self, cx: &Cx, owner: &HookOwner, id: &str) -> Result<()> {
        let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
        if !tx.delete(&HOOKS, &owner.log, id).await? {
            return Err(ForgeError::not_found(format!("webhook {id}")));
        }
        let jobs = tx
            .scan(
                &JOBS,
                &Scan {
                    key: Some(Key::K1(id.to_string())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        for j in jobs {
            tx.delete(&JOBS, "", &j.id).await?;
        }
        tx.audit(
            AuditDraft::new("webhook.delete", &owner.org_id)
                .repo_id(&owner.repo_id)
                .target_id(id),
        )
        .await?;
        tx.commit().await
    }

    /// Delivery log of a webhook, newest first, without request and response bodies.
    pub async fn deliveries(
        &self,
        cx: &Cx,
        owner: &HookOwner,
        hook_id: &str,
        success: Option<bool>,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<WebhookDelivery>, String)> {
        self.load(cx, owner, hook_id).await?;
        let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
        let mut q = Scan::scope(hook_id, limit + 1)
            .desc()
            .after((!after.is_empty()).then(|| Cursor {
                n1: 0,
                scope: hook_id.to_string(),
                id: after.to_string(),
            }));
        if let Some(ok) = success {
            q = q.key(Key::K1(if ok { "ok" } else { "failed" }.into()));
        }
        let rows = tx.scan_msgs::<WebhookDelivery>(&DELIVERIES, &q).await?;
        let more = rows.len() > limit;
        let out: Vec<WebhookDelivery> = rows
            .into_iter()
            .take(limit)
            .map(|(d, _)| summary(d))
            .collect();
        let next = if more {
            out.last().map(|d| d.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((out, next))
    }

    /// One delivery with request and response.
    pub async fn delivery(
        &self,
        cx: &Cx,
        owner: &HookOwner,
        hook_id: &str,
        delivery_id: &str,
    ) -> Result<WebhookDelivery> {
        self.load(cx, owner, hook_id).await?;
        let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
        match tx
            .get_msg::<WebhookDelivery>(&DELIVERIES, hook_id, delivery_id)
            .await?
        {
            Some((d, _)) => Ok(d),
            None => Err(ForgeError::not_found(format!("delivery {delivery_id}"))),
        }
    }

    async fn attempt(
        &self,
        cx: &Cx,
        rec: &HookRecord,
        req: Prepared,
        event: &Event,
        attempt: u32,
        redelivery: bool,
    ) -> Result<WebhookDelivery> {
        let hook = rec.webhook.clone().unwrap_or_default();
        let outcome = {
            let _permit = self
                .pool
                .acquire()
                .await
                .map_err(|_| ForgeError::unavailable("delivery pool closed"))?;
            self.sender.send(&hook.url, &req).await
        };
        self.log_delivery(cx, &hook, req, event, attempt, redelivery, outcome)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn log_delivery(
        &self,
        cx: &Cx,
        hook: &Webhook,
        req: Prepared,
        event: &Event,
        attempt: u32,
        redelivery: bool,
        outcome: Outcome,
    ) -> Result<WebhookDelivery> {
        let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
        let id = tx.new_id()?;
        let d = WebhookDelivery {
            id: id.clone(),
            webhook_id: hook.id.clone(),
            event_seq: event.seq,
            event_kind: event.kind.clone(),
            delivered_at: Some(ts(tx.now_ms())),
            status_code: u32::from(outcome.status),
            duration_ms: outcome.duration_ms,
            success: outcome.success(),
            redelivery,
            attempt,
            request_headers_json: serde_json::to_string(&req.headers).unwrap_or_default(),
            request_body: req.body,
            response_headers_json: serde_json::to_string(&outcome.headers).unwrap_or_default(),
            response_body: outcome.body,
            error: outcome.error,
        };
        let status = if d.success { "ok" } else { "failed" };
        tx.insert(
            &DELIVERIES,
            &Row::new(hook.id.clone(), id)
                .k1(status)
                .n1(ts_ms(d.delivered_at.as_ref().unwrap_or(&ts(0))) as i64)
                .msg(&d),
        )
        .await?;
        tx.commit().await?;
        Ok(d)
    }

    /// Sends a `ping` event now and returns the logged delivery.
    pub async fn ping(&self, cx: &Cx, owner: &HookOwner, id: &str) -> Result<WebhookDelivery> {
        let (rec, _) = self.load(cx, owner, id).await?;
        let hook = rec.webhook.clone().unwrap_or_default();
        let event = Event {
            kind: "ping".into(),
            repo_id: owner.repo_id.clone(),
            org_id: owner.org_id.clone(),
            time: Some(ts(self.db.now_ms())),
            principal: cx.actor_id.clone(),
            payload_json: serde_json::json!({ "hook_id": id }).to_string(),
            ..Default::default()
        };
        let req = self.prepare(&hook, &rec.secret, &event)?;
        self.attempt(cx, &rec, req, &event, 1, false).await
    }

    /// Re-sends a logged delivery's request (re-signed, new GUID) and returns the new
    /// delivery.
    pub async fn redeliver(
        &self,
        cx: &Cx,
        owner: &HookOwner,
        hook_id: &str,
        delivery_id: &str,
    ) -> Result<WebhookDelivery> {
        let (rec, _) = self.load(cx, owner, hook_id).await?;
        let old = self.delivery(cx, owner, hook_id, delivery_id).await?;
        let headers: BTreeMap<String, String> =
            serde_json::from_str(&old.request_headers_json).unwrap_or_default();
        let guid = self.db.ids().random_hex(16)?;
        let req = reprepare(&headers, &old.request_body, &guid, &rec.secret);
        let event = Event {
            seq: old.event_seq,
            kind: old.event_kind.clone(),
            ..Default::default()
        };
        self.attempt(cx, &rec, req, &event, 1, true).await
    }

    fn prepare(&self, hook: &Webhook, secret: &str, event: &Event) -> Result<Prepared> {
        let guid = self.db.ids().random_hex(16)?;
        let target = HookTarget {
            hook_id: hook.id.clone(),
            repo_id: hook.repo_id.clone(),
            org_id: hook.org_id.clone(),
        };
        let format = WebhookFormat::try_from(hook.format).unwrap_or(WebhookFormat::Bee);
        Ok(prepare(
            &self.registry,
            format,
            &target,
            event,
            &guid,
            secret,
        ))
    }

    /// Turns new log events into jobs for every active, matching webhook; returns the number
    /// of jobs queued.
    pub async fn dispatch_once(&self, cx: &Cx) -> Result<usize> {
        let hooks: Vec<(HookRecord, Row)> = {
            let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
            tx.scan_msgs(
                &HOOKS,
                &Scan {
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
        };
        let mut by_owner: BTreeMap<String, Vec<Webhook>> = BTreeMap::new();
        for (rec, row) in hooks {
            if let Some(h) = rec.webhook
                && h.active
            {
                by_owner.entry(row.scope).or_default().push(h);
            }
        }
        let mut queued = 0;
        for (log, hooks) in by_owner {
            let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
            // Lock (or create) the cursor row before reading it. Unlike get_for_update, this
            // also serializes the first dispatch when no cursor row exists yet. Keep this
            // WEBHOOKS transaction open while EventLog reads the separate EVENTS surface.
            tx.add_n1(&CURSORS, "", &log, 0).await?;
            let cursor = tx
                .get(&CURSORS, "", &log)
                .await?
                .and_then(|r| r.n1)
                .unwrap_or(0) as u64;
            let (events, next) = self.log.read(cx, &log, cursor + 1, &[], 500).await?;
            if events.is_empty() && next <= cursor + 1 {
                tx.commit().await?;
                continue;
            }
            let now = tx.now_ms() as i64;
            for e in &events {
                let at = e.time.as_ref().map(ts_ms).unwrap_or(0);
                for h in &hooks {
                    let created = h.created_at.as_ref().map(ts_ms).unwrap_or(0);
                    if at < created || !kind_matches(&h.kinds, &e.kind) {
                        continue;
                    }
                    let job = Job {
                        webhook_id: h.id.clone(),
                        owner: log.clone(),
                        event: Some(e.clone()),
                        attempt: 1,
                        redelivery: false,
                    };
                    let id = tx.new_id()?;
                    tx.insert(&JOBS, &Row::new("", id).k1(h.id.clone()).n1(now).msg(&job))
                        .await?;
                    queued += 1;
                }
            }
            tx.put(&CURSORS, &Row::new("", log.clone()).n1(next as i64 - 1))
                .await?;
            tx.commit().await?;
        }
        Ok(queued)
    }

    /// Claims and sends every due job (bounded by the worker pool), logging each attempt and
    /// rescheduling failures; returns the number of attempts made.
    pub async fn deliver_due(self: &Arc<Self>, cx: &Cx) -> Result<usize> {
        let now = self.db.now_ms() as i64;
        let due: Vec<Row> = {
            let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
            tx.scan(
                &JOBS,
                &Scan {
                    scope: Some(String::new()),
                    order: crate::store::Order::N1,
                    n1_max: Some(now),
                    limit: self.cfg.concurrency * 4,
                    ..Scan::default()
                },
            )
            .await?
        };
        let mut set = JoinSet::new();
        for row in due {
            let claimed = {
                let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
                let until = now + self.cfg.claim.as_millis() as i64;
                match tx.update(&JOBS, &row.clone().n1(until)).await {
                    Ok(v) => {
                        tx.commit().await?;
                        Some(row.clone().n1(until).version(v))
                    }
                    Err(e) if e.reason == bee_proto::v1::ErrorReason::VersionMismatch => None,
                    Err(e) if e.reason == bee_proto::v1::ErrorReason::NotFound => None,
                    Err(e) => return Err(e),
                }
            };
            let Some(row) = claimed else { continue };
            let me = self.clone();
            let cx = cx.clone();
            set.spawn(async move { me.run_job(&cx, row).await });
        }
        let mut n = 0;
        while let Some(r) = set.join_next().await {
            match r {
                Ok(Ok(())) => n += 1,
                Ok(Err(e)) => return Err(e),
                Err(e) => return Err(ForgeError::internal(format!("delivery task: {e}"))),
            }
        }
        Ok(n)
    }

    async fn run_job(&self, cx: &Cx, row: Row) -> Result<()> {
        let job: Job = row.decode()?;
        let event = job.event.clone().unwrap_or_default();
        let hook = {
            let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
            tx.get_msg::<HookRecord>(&HOOKS, &job.owner, &job.webhook_id)
                .await?
        };
        let Some((rec, _)) = hook else {
            let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
            tx.delete(&JOBS, "", &row.id).await?;
            return tx.commit().await;
        };
        let h = rec.webhook.clone().unwrap_or_default();
        let req = self.prepare(&h, &rec.secret, &event)?;
        let d = self
            .attempt(cx, &rec, req, &event, job.attempt, job.redelivery)
            .await?;
        let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
        if d.success || job.attempt >= self.cfg.retry.max_attempts || !h.active {
            tx.delete(&JOBS, "", &row.id).await?;
        } else {
            let next = Job {
                attempt: job.attempt + 1,
                ..job
            };
            let due =
                self.db.now_ms() as i64 + self.cfg.retry.delay(next.attempt - 1).as_millis() as i64;
            tx.update(&JOBS, &row.clone().n1(due).msg(&next)).await?;
        }
        tx.commit().await
    }

    /// Number of queued jobs (for health and tests).
    pub async fn queued(&self, cx: &Cx) -> Result<u64> {
        let mut tx = self.db.begin(Surface::WEBHOOKS, cx).await?;
        tx.count(&JOBS, "", None).await
    }
}

fn summary(mut d: WebhookDelivery) -> WebhookDelivery {
    d.request_headers_json.clear();
    d.request_body.clear();
    d.response_headers_json.clear();
    d.response_body.clear();
    d
}

fn json_hook(h: &Webhook) -> String {
    serde_json::json!({
        "id": h.id,
        "url": h.url,
        "kinds": h.kinds,
        "active": h.active,
        "format": h.format,
    })
    .to_string()
}
