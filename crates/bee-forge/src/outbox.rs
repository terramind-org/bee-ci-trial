//! The outbox relay: moves events and audit entries from each surface's outbox to the event
//! log and the audit chain.
//!
//! Each surface is drained independently, in outbox (ULID) order. An item is removed from
//! the outbox only after it was appended; appends are idempotent (keyed by the outbox id for
//! events and the entry id for audit), so a crash between append and removal only repeats a
//! no-op. If the event log or the audit log is down, items wait in their surface's outbox and
//! the surface itself keeps working.
//!
//! Ordering: within a surface, items move in commit order. The background relay runs one
//! task per surface (so a slow surface database delays only its own items); events of
//! different surfaces are therefore ordered by when they were relayed. [`Relay::drain_all`]
//! merges all surfaces by outbox id and moves items in global order.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::audit::AuditLog;
use crate::cx::Cx;
use crate::error::Result;
use crate::events::EventLog;
use crate::store::{Db, Surface};

/// Items moved per transaction.
pub const BATCH: usize = 100;

/// The relay.
pub struct Relay {
    db: Db,
    events: Arc<EventLog>,
    audit: Arc<AuditLog>,
    surfaces: Vec<Surface>,
}

impl Relay {
    /// A relay for `surfaces`.
    pub fn new(
        db: Db,
        events: Arc<EventLog>,
        audit: Arc<AuditLog>,
        surfaces: Vec<Surface>,
    ) -> Relay {
        Relay {
            db,
            events,
            audit,
            surfaces,
        }
    }

    /// Moves one batch from one surface; returns how many items were moved.
    pub async fn drain_batch(&self, cx: &Cx, s: Surface) -> Result<usize> {
        let items = {
            let mut tx = self.db.begin(s, cx).await?;
            tx.raw().outbox_peek(BATCH).await?
        };
        if items.is_empty() {
            return Ok(0);
        }
        let mut done = Vec::new();
        let mut first_err = None;
        for (id, rec) in items {
            let r = async {
                if let Some(e) = rec.event {
                    self.events.append(cx, e, &id).await?;
                }
                if let Some(a) = rec.audit {
                    self.audit.append(cx, a).await?;
                }
                Ok::<(), crate::error::ForgeError>(())
            }
            .await;
            match r {
                Ok(()) => done.push(id),
                Err(e) => {
                    first_err = Some(e);
                    break;
                }
            }
        }
        let n = done.len();
        if !done.is_empty() {
            let mut tx = self.db.begin(s, cx).await?;
            tx.raw().outbox_remove(&done).await?;
            tx.commit().await?;
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(n),
        }
    }

    /// Drains one surface completely.
    pub async fn drain_surface(&self, cx: &Cx, s: Surface) -> Result<usize> {
        let mut total = 0;
        loop {
            let n = self.drain_batch(cx, s).await?;
            total += n;
            if n < BATCH {
                return Ok(total);
            }
        }
    }

    /// Drains every reachable surface, moving items in global outbox-id order (ids come from
    /// one monotonic generator, so this is commit order within a process). An unreachable
    /// surface is skipped and its error returned after the others were drained.
    pub async fn drain_all(&self, cx: &Cx) -> Result<usize> {
        let mut total = 0;
        let mut err = None;
        loop {
            let mut items = Vec::new();
            let mut full = false;
            for s in &self.surfaces {
                let peeked = async {
                    let mut tx = self.db.begin(*s, cx).await?;
                    tx.raw().outbox_peek(BATCH).await
                }
                .await;
                match peeked {
                    Ok(v) => {
                        full |= v.len() == BATCH;
                        items.extend(v.into_iter().map(|(id, rec)| (id, *s, rec)));
                    }
                    Err(e) => {
                        err.get_or_insert(e);
                    }
                }
            }
            if items.is_empty() {
                break;
            }
            items.sort_by(|a, b| a.0.cmp(&b.0));
            let mut done: Vec<(Surface, String)> = Vec::new();
            let mut failed = None;
            for (id, s, rec) in items {
                let r = async {
                    if let Some(e) = rec.event {
                        self.events.append(cx, e, &id).await?;
                    }
                    if let Some(a) = rec.audit {
                        self.audit.append(cx, a).await?;
                    }
                    Ok::<(), crate::error::ForgeError>(())
                }
                .await;
                match r {
                    Ok(()) => done.push((s, id)),
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            total += done.len();
            for s in &self.surfaces {
                let ids: Vec<String> = done
                    .iter()
                    .filter(|(x, _)| x == s)
                    .map(|(_, id)| id.clone())
                    .collect();
                if ids.is_empty() {
                    continue;
                }
                let mut tx = self.db.begin(*s, cx).await?;
                tx.raw().outbox_remove(&ids).await?;
                tx.commit().await?;
            }
            if let Some(e) = failed {
                return Err(e);
            }
            if !full {
                break;
            }
        }
        match err {
            Some(e) => Err(e),
            None => Ok(total),
        }
    }

    /// Runs one task per surface until `stop` turns true. Each task wakes on commits that
    /// wrote to an outbox, and at least every `poll`; failures back off up to 30 seconds.
    pub fn spawn(
        self: Arc<Self>,
        poll: Duration,
        stop: watch::Receiver<bool>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        self.surfaces
            .iter()
            .map(|s| {
                let me = self.clone();
                let s = *s;
                let mut stop = stop.clone();
                tokio::spawn(async move {
                    let mut backoff = poll;
                    loop {
                        if *stop.borrow() {
                            return;
                        }
                        let signal = me.db.outbox_signal().clone();
                        let woken = signal.notified();
                        let wait = match me.drain_surface(&Cx::system(), s).await {
                            Ok(_) => {
                                backoff = poll;
                                poll
                            }
                            Err(_) => {
                                backoff = (backoff * 2).min(Duration::from_secs(30));
                                backoff
                            }
                        };
                        tokio::select! {
                            _ = woken => {}
                            _ = tokio::time::sleep(wait) => {}
                            _ = stop.changed() => {}
                        }
                    }
                })
            })
            .collect()
    }
}
