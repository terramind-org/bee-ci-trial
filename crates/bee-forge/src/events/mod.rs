//! Per-repo and per-org append-only event logs (SPEC §11.7), subscriptions with resume,
//! retention, and webhooks ([`webhooks`], [`delivery`]).
//!
//! # Logs
//!
//! Every repo has a log keyed by its repo id; every org has one keyed `org:<org id>`. An event
//! about a repo is appended to the repo's log and to its org's log (each with its own
//! sequence), so org-level subscribers and webhooks see every repo of the org. Events about
//! an org only go to the org log.
//!
//! Sequence numbers are allocated in blocks ([`EventLog::with_block_size`]): the log's counter
//! row moves once per block, not once per event, so a hot repo does not serialize on one
//! counter update per event. Numbers are strictly increasing per log; unused numbers of a
//! block leave gaps, which readers tolerate. One process appends to a given log at a time
//! (appends to one log are serialized in-process; a cell owns its orgs, SPEC §15.2), so
//! sequence order is commit order and a subscriber resuming from a sequence never misses an
//! event committed later.
//!
//! # Subscribe and resume
//!
//! [`EventLog::subscribe`] streams events with `seq >= from_seq` and then follows the log.
//! A subscriber that reconnects passes its last seen sequence plus one. A `from_seq` that
//! retention already removed fails with `INVALID_ARGUMENT` / `seq_not_retained`; `0` means
//! "from the oldest retained event".
//!
//! # Retention
//!
//! [`EventLog::prune`] deletes events older than the retention window (default 30 days)
//! and records the highest removed sequence.
//!
//! # Listeners
//!
//! [`EventListener`]s run inline after a single [`EventLog::append`] (domain events). Events
//! appended in batches ([`EventLog::append_batch`], the VCS bridge) go to a bounded queue per
//! listener ([`LISTENER_QUEUE`]) drained in order by that listener's own task, so fan-out to
//! subscribers and webhooks never waits on another module's reaction (SPEC §15.1).

pub mod bridge;
pub mod delivery;
pub mod webhooks;

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use bee_proto::v1::Event;
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, mpsc};

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::module::{EventKind, ForgeModule};
use crate::store::{Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, Surface, TableDef};

/// Default retention of an event log.
pub const DEFAULT_RETENTION_DAYS: u32 = 30;
/// Default sequence block size.
pub const DEFAULT_BLOCK_SIZE: u64 = 32;
/// Default and maximum page of [`EventLog::read`].
pub const DEFAULT_READ_LIMIT: usize = 100;
/// Maximum page of [`EventLog::read`].
pub const MAX_READ_LIMIT: usize = 1000;
/// Events a listener may fall behind on batched appends ([`EventLog::append_batch`]) before
/// the appender waits for it.
pub const LISTENER_QUEUE: usize = 4096;
/// Events retained in the in-process fan-out tail. Consumers that fall this far behind are
/// explicitly told to resume from their durable cursor rather than silently losing events.
const SUBSCRIBER_TAIL: usize = 4096;

/// Events, keyed by log, ordered by sequence.
pub static EVENTS: TableDef = TableDef {
    name: "forge_events",
    surface: Surface::EVENTS,
    unique_k1: false,
    unique_k2: true,
    indexes: &[Index::ScopeN1, Index::ScopeK1],
};

/// Per-log counters: `seq` (highest allocated) and `pruned` (highest removed by retention).
pub static COUNTERS: TableDef = TableDef {
    name: "forge_event_counters",
    surface: Surface::EVENTS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

static M1: Migration = Migration {
    module: "events",
    version: 1,
    name: "create_event_log",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&EVENTS),
        MigrationOp::CreateTable(&COUNTERS),
    ],
};

/// What the events module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "events",
    surfaces: &[Surface::EVENTS],
    migrations: &[&M1],
    event_kinds: &[
        EventKind::gh(
            "ref.updated",
            "A ref moved (bridged from the VCS store)",
            "push",
            "",
        ),
        EventKind::new("policy.changed", "The repo's policy epoch changed"),
        EventKind::new("token.revoked", "A token was revoked"),
    ],
    audit_prefixes: &[],
};

/// The log key of a repo.
pub fn repo_log(repo_id: &str) -> String {
    repo_id.to_string()
}

/// The log key of an org.
pub fn org_log(org_id: &str) -> String {
    format!("org:{org_id}")
}

/// The logs an event is appended to: its repo's (if any) and its org's (if any).
pub fn logs_of(event: &Event) -> Vec<String> {
    let mut keys = Vec::new();
    if !event.repo_id.is_empty() {
        keys.push(repo_log(&event.repo_id));
    }
    if !event.org_id.is_empty() {
        keys.push(org_log(&event.org_id));
    }
    if keys.is_empty() {
        keys.push("system".to_string());
    }
    keys
}

/// Whether `kind` passes `filters` (empty = all; `x.*` matches the `x.` prefix; `*` all).
pub fn kind_matches(filters: &[String], kind: &str) -> bool {
    filters.is_empty()
        || filters.iter().any(|f| {
            f == "*"
                || f == kind
                || f.strip_suffix(".*").is_some_and(|p| {
                    kind.len() > p.len() && kind.starts_with(p) && kind.as_bytes()[p.len()] == b'.'
                })
        })
}

fn seq_id(seq: u64) -> String {
    format!("{seq:020}")
}

/// Called after an event is appended to its primary log (the repo log for repo events).
#[async_trait]
pub trait EventListener: Send + Sync {
    /// Reacts to one event. Failures are the listener's to handle; the event stays appended.
    async fn on_event(&self, event: &Event);
}

#[derive(Clone, Copy, Default)]
struct Block {
    next: u64,
    end: u64,
}

/// Retention summary of one log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogInfo {
    /// Oldest retained sequence (0 if empty).
    pub oldest_seq: u64,
    /// Newest sequence (0 if empty).
    pub latest_seq: u64,
    /// Retention window.
    pub retention_days: u32,
}

/// The event logs.
pub struct EventLog {
    db: Db,
    retention_days: u32,
    block_size: u64,
    blocks: Mutex<HashMap<String, Block>>,
    appenders: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    wake: Arc<Notify>,
    tails: Mutex<HashMap<String, Arc<SharedTail>>>,
    listeners: RwLock<Vec<Arc<dyn EventListener>>>,
    /// Per-listener queues of batched appends, index-aligned with `listeners`.
    queues: Mutex<Vec<mpsc::Sender<Event>>>,
    pending: Arc<Pending>,
}

/// One durable reader and fan-out channel per log.  The durable reader is deliberately shared:
/// opening N streams must not turn one append into N storage scans.
struct SharedTail {
    events: broadcast::Sender<Event>,
    durable_reads: AtomicUsize,
}

/// Events queued to listeners and not yet handled.
#[derive(Default)]
struct Pending {
    n: AtomicUsize,
    idle: Notify,
}

impl Pending {
    fn started(&self) {
        self.n.fetch_add(1, Ordering::SeqCst);
    }

    fn finished(&self) {
        if self.n.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.idle.notify_waiters();
        }
    }
}

/// Marks one queued event handled when dropped (also if the listener panics).
struct Finished<'a>(&'a Pending);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        self.0.finished();
    }
}

impl EventLog {
    /// Logs over `db` with default retention and block size.
    pub fn new(db: Db) -> EventLog {
        EventLog {
            db,
            retention_days: DEFAULT_RETENTION_DAYS,
            block_size: DEFAULT_BLOCK_SIZE,
            blocks: Mutex::new(HashMap::new()),
            appenders: Mutex::new(HashMap::new()),
            wake: Arc::new(Notify::new()),
            tails: Mutex::new(HashMap::new()),
            listeners: RwLock::new(Vec::new()),
            queues: Mutex::new(Vec::new()),
            pending: Arc::new(Pending::default()),
        }
    }

    fn tail(self: &Arc<Self>, log: &str) -> Result<Arc<SharedTail>> {
        let mut tails = self
            .tails
            .lock()
            .map_err(|_| ForgeError::internal("event tails poisoned"))?;
        if let Some(tail) = tails.get(log) {
            return Ok(tail.clone());
        }
        let (events, _) = broadcast::channel(SUBSCRIBER_TAIL);
        let tail = Arc::new(SharedTail {
            events,
            durable_reads: AtomicUsize::new(0),
        });
        let sender = tail.events.clone();
        let tail_for_reader = tail.clone();
        let me = self.clone();
        let log = log.to_string();
        let tail_log = log.clone();
        tokio::spawn(async move {
            let mut next = 1;
            loop {
                let wake = me.wake.notified();
                let cx = Cx::system().with_timeout(crate::cx::DEFAULT_DEADLINE);
                tail_for_reader.durable_reads.fetch_add(1, Ordering::SeqCst);
                // A transient reader failure must not permanently stop a tail. Subscribers
                // retain their durable cursor and can reconnect if their stream sees lag.
                if let Ok((batch, n)) = me.read(&cx, &tail_log, next, &[], DEFAULT_READ_LIMIT).await
                {
                    next = n;
                    let full = batch.len() == DEFAULT_READ_LIMIT;
                    for event in batch {
                        let _ = sender.send(event);
                    }
                    if full {
                        continue;
                    }
                }
                tokio::select! {
                    _ = wake => {}
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                }
            }
        });
        tails.insert(log, tail.clone());
        Ok(tail)
    }

    /// Number of durable reads performed by the shared live tail for `log`.
    ///
    /// This is intentionally exposed for the event-delivery complexity gate: subscriptions
    /// take their own initial snapshot, but live delivery must remain one durable read per log
    /// regardless of the number of subscribers.
    pub fn tail_durable_reads(self: &Arc<Self>, log: &str) -> Result<usize> {
        Ok(self.tail(log)?.durable_reads.load(Ordering::SeqCst))
    }

    /// Resets the shared-tail read counter used by the delivery complexity gate.
    pub fn reset_tail_durable_reads(self: &Arc<Self>, log: &str) -> Result<()> {
        self.tail(log)?.durable_reads.store(0, Ordering::SeqCst);
        Ok(())
    }

    /// Sets the retention window.
    pub fn with_retention_days(mut self, days: u32) -> EventLog {
        self.retention_days = days.max(1);
        self
    }

    /// Sets the sequence block size.
    pub fn with_block_size(mut self, n: u64) -> EventLog {
        self.block_size = n.max(1);
        self
    }

    /// The retention window.
    pub fn retention_days(&self) -> u32 {
        self.retention_days
    }

    /// Registers a listener.
    pub fn add_listener(&self, l: Arc<dyn EventListener>) {
        if let Ok(mut v) = self.listeners.write() {
            v.push(l);
        }
    }

    fn appender(&self, key: &str) -> Result<Arc<AsyncMutex<()>>> {
        let mut map = self
            .appenders
            .lock()
            .map_err(|_| ForgeError::internal("event appenders poisoned"))?;
        Ok(map.entry(key.to_string()).or_default().clone())
    }

    /// Appends `event` to each of its logs (see [`logs_of`]) in one transaction, once per
    /// `dedupe` key: appending the same key again is a no-op. Returns `(log, seq)` per log.
    /// Listeners run before this returns (domain events are low volume and their effects,
    /// such as lease auto-release on `cr.landed`, are expected right after the append).
    pub async fn append(&self, cx: &Cx, event: Event, dedupe: &str) -> Result<Vec<(String, u64)>> {
        // Boxed: listeners append in turn, and the transaction's state would otherwise sit on
        // the stack once per nesting level.
        let mut out =
            Box::pin(self.append_many(cx, vec![(event, dedupe.to_string())], None)).await?;
        let Some((ev, seqs)) = out.pop() else {
            return Ok(Vec::new());
        };
        let listeners: Vec<Arc<dyn EventListener>> =
            self.listeners.read().map(|v| v.clone()).unwrap_or_default();
        for l in listeners {
            l.on_event(&ev).await;
        }
        Ok(seqs)
    }

    /// Appends many events in one transaction (each to each of its logs, once per dedupe
    /// key), returning `(log, seq)` per log for each event in order. Listeners receive the
    /// events through their queues ([`LISTENER_QUEUE`]) instead of inline, so a slow listener
    /// delays only itself: the log, its subscribers and webhooks advance as soon as the
    /// transaction commits.
    pub async fn append_batch(
        &self,
        cx: &Cx,
        events: Vec<(Event, String)>,
    ) -> Result<Vec<Vec<(String, u64)>>> {
        let out = Box::pin(self.append_many(cx, events, None)).await?;
        let seqs = out.iter().map(|(_, s)| s.clone()).collect();
        self.queue_for_listeners(out.into_iter().map(|(e, _)| e).collect())
            .await;
        Ok(seqs)
    }

    /// [`EventLog::append_batch`] that also advances the counter `cursor` of `scope` to `to`
    /// in the same transaction (the VCS bridge's per-repo cursor), so the cursor and the
    /// events it covers commit together. Returns how many events were appended.
    pub(crate) async fn append_bridged(
        &self,
        cx: &Cx,
        events: Vec<(Event, String)>,
        scope: &str,
        to: u64,
    ) -> Result<usize> {
        let out = Box::pin(self.append_many(cx, events, Some((scope, to)))).await?;
        let n = out.len();
        self.queue_for_listeners(out.into_iter().map(|(e, _)| e).collect())
            .await;
        Ok(n)
    }

    /// The append transaction: every event of `items` to each of its logs, plus an optional
    /// cursor advance. Returns each event with `seq` set to its primary log's sequence.
    async fn append_many(
        &self,
        cx: &Cx,
        mut items: Vec<(Event, String)>,
        cursor: Option<(&str, u64)>,
    ) -> Result<Vec<(Event, Vec<(String, u64)>)>> {
        let now = self.db.now_ms();
        for (e, _) in &mut items {
            if e.time.is_none() {
                e.time = Some(crate::ids::ts(now));
            }
        }
        let keys: BTreeSet<String> = items.iter().flat_map(|(e, _)| logs_of(e)).collect();
        let mut guards = Vec::new();
        for k in &keys {
            guards.push(self.appender(k)?.lock_owned().await);
        }
        let mut tx = self.db.begin(Surface::EVENTS, cx).await?;
        let mut local: HashMap<String, Block> = HashMap::new();
        let mut out = Vec::with_capacity(items.len());
        for (event, dedupe) in items {
            let mut ks = logs_of(&event);
            ks.sort();
            ks.dedup();
            let mut seqs = Vec::with_capacity(ks.len());
            for key in &ks {
                let dup = tx
                    .scan(
                        &EVENTS,
                        &Scan::scope(key.clone(), 1).key(Key::K2(dedupe.clone())),
                    )
                    .await?;
                if let Some(row) = dup.first() {
                    seqs.push((key.clone(), row.n1.unwrap_or(0) as u64));
                    continue;
                }
                let mut block = match local.get(key) {
                    Some(b) => *b,
                    None => self
                        .blocks
                        .lock()
                        .map_err(|_| ForgeError::internal("event blocks poisoned"))?
                        .get(key)
                        .copied()
                        .unwrap_or_default(),
                };
                if block.next == 0 || block.next > block.end {
                    let top = tx
                        .add_n1(&COUNTERS, key, "seq", self.block_size as i64)
                        .await? as u64;
                    block = Block {
                        next: top + 1 - self.block_size,
                        end: top,
                    };
                }
                let seq = block.next;
                block.next += 1;
                let mut ev = event.clone();
                ev.seq = seq;
                let row = Row::new(key.clone(), seq_id(seq))
                    .k1(ev.kind.clone())
                    .k2(dedupe.clone())
                    .n1(seq as i64)
                    .msg(&ev);
                tx.insert(&EVENTS, &row).await?;
                local.insert(key.clone(), block);
                seqs.push((key.clone(), seq));
            }
            out.push((event, seqs));
        }
        if let Some((scope, to)) = cursor {
            let cur = tx
                .get(&COUNTERS, scope, "cursor")
                .await?
                .and_then(|r| r.n1)
                .unwrap_or(0);
            let to = i64::try_from(to).unwrap_or(i64::MAX);
            if to > cur {
                tx.add_n1(&COUNTERS, scope, "cursor", to - cur).await?;
            }
        }
        tx.commit().await?;
        if let Ok(mut blocks) = self.blocks.lock() {
            blocks.extend(local);
        }
        drop(guards);
        self.wake.notify_waiters();
        Ok(out
            .into_iter()
            .map(|(mut ev, seqs)| {
                let primary = logs_of(&ev).into_iter().next().unwrap_or_default();
                if let Some((_, seq)) = seqs.iter().find(|(k, _)| *k == primary) {
                    ev.seq = *seq;
                }
                (ev, seqs)
            })
            .collect())
    }

    /// Hands `events` to every listener's queue, in order, starting a listener's worker the
    /// first time. Waits only when a listener's queue is full (it is [`LISTENER_QUEUE`]
    /// events behind), which bounds the memory a slow listener can pin.
    async fn queue_for_listeners(&self, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        let queues = self.listener_queues();
        for ev in events {
            for q in &queues {
                self.pending.started();
                if q.send(ev.clone()).await.is_err() {
                    self.pending.finished();
                }
            }
        }
    }

    /// One queue per registered listener, each drained in order by its own task.
    fn listener_queues(&self) -> Vec<mpsc::Sender<Event>> {
        let listeners: Vec<Arc<dyn EventListener>> =
            self.listeners.read().map(|v| v.clone()).unwrap_or_default();
        let Ok(mut queues) = self.queues.lock() else {
            return Vec::new();
        };
        while queues.len() < listeners.len() {
            let l = listeners[queues.len()].clone();
            let (tx, mut rx) = mpsc::channel::<Event>(LISTENER_QUEUE);
            let pending = self.pending.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    let _done = Finished(&pending);
                    l.on_event(&ev).await;
                }
            });
            queues.push(tx);
        }
        queues.clone()
    }

    /// Waits until every listener has handled every event queued so far (tests and
    /// orderly shutdown; the log itself never waits for this).
    pub async fn settle(&self) {
        loop {
            let idle = self.pending.idle.notified();
            if self.pending.n.load(Ordering::SeqCst) == 0 {
                return;
            }
            idle.await;
        }
    }

    /// Events of `log` with `seq >= from_seq` whose kind passes `kinds`, at most `limit`
    /// (default 100, max 1000), and the sequence to continue from.
    pub async fn read(
        &self,
        cx: &Cx,
        log: &str,
        from_seq: u64,
        kinds: &[String],
        limit: usize,
    ) -> Result<(Vec<Event>, u64)> {
        let limit = if limit == 0 {
            DEFAULT_READ_LIMIT
        } else {
            limit.min(MAX_READ_LIMIT)
        };
        let mut tx = self.db.begin(Surface::EVENTS, cx).await?;
        let pruned = tx
            .get(&COUNTERS, log, "pruned")
            .await?
            .and_then(|r| r.n1)
            .unwrap_or(0) as u64;
        if from_seq != 0 && from_seq <= pruned {
            return Err(ForgeError::invalid(
                "seq_not_retained",
                format!(
                    "events up to {pruned} were removed by retention; resume from {}",
                    pruned + 1
                ),
            ));
        }
        let mut next = from_seq.max(1);
        let mut out = Vec::new();
        // Scan at most ten pages to fill one filtered page.
        for _ in 0..10 {
            let rows = tx
                .scan(
                    &EVENTS,
                    &Scan::scope(log, limit).by_n1().n1_min(next as i64),
                )
                .await?;
            let exhausted = rows.len() < limit;
            for row in rows {
                let seq = row.n1.unwrap_or(0) as u64;
                next = seq + 1;
                if kind_matches(kinds, row.k1.as_deref().unwrap_or("")) {
                    out.push(row.decode::<Event>()?);
                    if out.len() == limit {
                        return Ok((out, next));
                    }
                }
            }
            if exhausted {
                break;
            }
        }
        Ok((out, next))
    }

    /// Oldest and newest sequence of a log.
    pub async fn info(&self, cx: &Cx, log: &str) -> Result<LogInfo> {
        let mut tx = self.db.begin(Surface::EVENTS, cx).await?;
        let first = tx.scan(&EVENTS, &Scan::scope(log, 1).by_n1()).await?;
        let last = tx
            .scan(&EVENTS, &Scan::scope(log, 1).by_n1().desc())
            .await?;
        Ok(LogInfo {
            oldest_seq: first.first().and_then(|r| r.n1).unwrap_or(0) as u64,
            latest_seq: last.first().and_then(|r| r.n1).unwrap_or(0) as u64,
            retention_days: self.retention_days,
        })
    }

    /// Streams events of `log` from `from_seq`, then follows new appends. The stream ends
    /// with an error item if the store fails, or when the receiver is dropped.
    pub async fn subscribe(
        self: &Arc<Self>,
        cx: &Cx,
        log: &str,
        from_seq: u64,
        kinds: Vec<String>,
    ) -> Result<mpsc::Receiver<Result<Event>>> {
        // Subscribe before taking the snapshot. The shared tail then covers the race between
        // this read and registration; sequence filtering below removes its duplicate edge.
        let tail = self.tail(log)?;
        let mut live = tail.events.subscribe();
        // Validate the start before streaming so the caller gets a plain error.
        let mut next = from_seq;
        let mut first = Vec::new();
        loop {
            let (page, after) = self.read(cx, log, next, &kinds, DEFAULT_READ_LIMIT).await?;
            let full = page.len() == DEFAULT_READ_LIMIT;
            first.extend(page);
            next = after;
            if !full {
                break;
            }
        }
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            for e in first {
                if tx.send(Ok(e)).await.is_err() {
                    return;
                }
            }
            loop {
                match live.recv().await {
                    Ok(event) => {
                        if event.seq < next || !kind_matches(&kinds, &event.kind) {
                            continue;
                        }
                        next = event.seq + 1;
                        if tx.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let err = ForgeError::unavailable(format!(
                            "subscriber_lagged; resume from sequence {next}"
                        ))
                        .with_subcode("subscriber_lagged");
                        let _ = tx.send(Err(err)).await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(rx)
    }

    /// Removes events older than the retention window from every log; returns how many.
    pub async fn prune(&self, cx: &Cx) -> Result<u64> {
        let cutoff = self
            .db
            .now_ms()
            .saturating_sub(u64::from(self.retention_days) * 86_400_000);
        let logs: Vec<String> = {
            let mut tx = self.db.begin(Surface::EVENTS, cx).await?;
            let rows = tx
                .scan(
                    &COUNTERS,
                    &Scan {
                        limit: crate::store::MAX_SCAN,
                        ..Scan::default()
                    },
                )
                .await?;
            rows.into_iter()
                .filter(|r| r.id == "seq")
                .map(|r| r.scope)
                .collect()
        };
        let mut removed = 0;
        for log in logs {
            loop {
                let mut tx = self.db.begin(Surface::EVENTS, cx).await?;
                let rows = tx
                    .scan(&EVENTS, &Scan::scope(log.clone(), 500).by_n1())
                    .await?;
                let mut last = None;
                let mut n = 0;
                for row in &rows {
                    let ev: Event = row.decode()?;
                    if ev.time.as_ref().map(ts_ms).unwrap_or(0) >= cutoff {
                        break;
                    }
                    tx.delete(&EVENTS, &log, &row.id).await?;
                    last = row.n1;
                    n += 1;
                }
                if let Some(seq) = last {
                    let cur = tx
                        .get(&COUNTERS, &log, "pruned")
                        .await?
                        .and_then(|r| r.n1)
                        .unwrap_or(0);
                    tx.add_n1(&COUNTERS, &log, "pruned", seq - cur).await?;
                }
                tx.commit().await?;
                removed += n;
                if n < 500 {
                    break;
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_filters() {
        let f = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(kind_matches(&[], "cr.opened"));
        assert!(kind_matches(&f(&["cr.*"]), "cr.opened"));
        assert!(!kind_matches(&f(&["cr.*"]), "crx.opened"));
        assert!(!kind_matches(&f(&["cr.*"]), "lease.acquired"));
        assert!(kind_matches(
            &f(&["lease.acquired", "cr.*"]),
            "lease.acquired"
        ));
        assert!(kind_matches(&f(&["*"]), "anything"));
    }

    #[test]
    fn logs_of_repo_events_include_the_org() {
        let e = Event {
            repo_id: "R".into(),
            org_id: "O".into(),
            ..Default::default()
        };
        assert_eq!(logs_of(&e), vec!["R".to_string(), "org:O".to_string()]);
    }
}
