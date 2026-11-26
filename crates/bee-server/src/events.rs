//! Per-repo event log writes (SPEC §11.7, §7.4 "hot counters").
//!
//! Ref updates carry their `ref.updated` events inside the ref CAS transaction (the store
//! allocates one block per transaction, so a coalesced batch of K lands costs one counter
//! update). Other events (`token.revoked`, `redaction.*`, `import.*`) go through
//! [`EventLog`], which reserves sequence numbers in blocks of [`BLOCK`] and writes events
//! under them; numbers left unused when a process stops are gaps readers tolerate.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use bee_store::{MetaStore, NewEvent, RepoId};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::errors::ApiResult;
use crate::metrics::Metrics;

/// Sequence numbers reserved per allocation.
pub const BLOCK: u64 = 64;

/// The event payload encoding: compact JSON.
pub fn payload(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

/// A new event.
pub fn new_event(kind: &str, body: &Value, at_ms: u64) -> NewEvent {
    NewEvent {
        kind: kind.to_string(),
        payload: payload(body),
        at_ms,
    }
}

/// Block-allocating event writer.
pub struct EventLog {
    meta: Arc<dyn MetaStore>,
    blocks: Mutex<HashMap<RepoId, Range<u64>>>,
    metrics: Arc<Metrics>,
}

impl EventLog {
    /// A writer over `meta`.
    pub fn new(meta: Arc<dyn MetaStore>, metrics: Arc<Metrics>) -> EventLog {
        EventLog {
            meta,
            blocks: Mutex::new(HashMap::new()),
            metrics,
        }
    }

    /// Appends one event and returns its sequence number.
    pub async fn append(
        &self,
        repo: RepoId,
        kind: &str,
        body: &Value,
        at_ms: u64,
    ) -> ApiResult<u64> {
        let seq = {
            let mut blocks = self.blocks.lock().await;
            let range = blocks.entry(repo).or_insert(0..0);
            if range.is_empty() {
                *range = self.meta.allocate_event_seqs(repo, BLOCK).await?;
            }
            let s = range.start;
            range.start += 1;
            s
        };
        self.meta
            .insert_events(repo, vec![(seq, new_event(kind, body, at_ms))])
            .await?;
        self.metrics
            .events
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(seq)
    }
}
