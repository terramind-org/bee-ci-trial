//! The append-only, hash-chained audit log (PRODUCT §3.3, §4.10).
//!
//! Every state-changing RPC records who (the full delegation chain), what, when, on which
//! repo and target, and the decision that allowed it. Entries are keyed by a sequence number;
//! entry `n` stores the hash of entry `n - 1`, and its own hash is BLAKE3 over
//! `swarm-audit-v1\0 ‖ prev_hash ‖ canonical JSON of the entry without its hash`. An entry is
//! inserted only if its key is absent, so concurrent writers (several processes) can never
//! fork the chain. Appends are batched: one writer task drains every waiting entry into one
//! transaction and then acknowledges each caller.
//!
//! [`AuditSink`] is the seam a forge audit module plugs into; [`KvAudit`] is the built-in
//! implementation on the server store.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::errors::{ApiError, ApiResult};
use crate::kv::{Kv, KvOp};
use bee_proto::v1::ErrorReason;

/// Namespace of audit entries.
pub const NS_AUDIT: &str = "audit";

/// One audit entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Sequence number (assigned on append).
    #[serde(default)]
    pub seq: u64,
    /// Time.
    pub at_ms: u64,
    /// Acting principal, then each delegator.
    pub chain: Vec<String>,
    /// The operation, e.g. `/bee.v1.WriteService/Land`.
    pub action: String,
    /// Repo id, when repo-scoped.
    #[serde(default)]
    pub repo: String,
    /// What changed: a ref, changeset, token id, object id.
    #[serde(default)]
    pub target: String,
    /// The rule or decision that allowed it.
    #[serde(default)]
    pub decision: String,
    /// Extra structured detail (never secrets or file contents).
    #[serde(default)]
    pub detail: serde_json::Value,
    /// Request id.
    #[serde(default)]
    pub request_id: String,
    /// Hash of the previous entry (hex; empty for the first).
    #[serde(default)]
    pub prev_hash: String,
    /// This entry's hash (hex).
    #[serde(default)]
    pub hash: String,
}

impl AuditEntry {
    /// The chained hash of this entry given its `prev_hash`.
    pub fn compute_hash(&self) -> String {
        let mut e = self.clone();
        e.hash = String::new();
        let json = serde_json::to_vec(&e).unwrap_or_default();
        let mut h = blake3::Hasher::new();
        h.update(b"swarm-audit-v1\0");
        h.update(self.prev_hash.as_bytes());
        h.update(&json);
        bee_core::hex::encode(h.finalize().as_bytes())
    }
}

/// Where audit entries go.
#[async_trait]
pub trait AuditSink: Send + Sync {
    /// Appends one entry durably; returns its sequence number.
    async fn record(&self, entry: AuditEntry) -> ApiResult<u64>;
    /// Entries with `seq >= from`, at most `limit`.
    async fn read(&self, from: u64, limit: usize) -> ApiResult<Vec<AuditEntry>>;
}

type Job = (AuditEntry, oneshot::Sender<ApiResult<u64>>);

/// The built-in audit log on the server store.
pub struct KvAudit {
    kv: Arc<dyn Kv>,
    tx: mpsc::Sender<Job>,
}

fn key(seq: u64) -> Vec<u8> {
    seq.to_be_bytes().to_vec()
}

async fn head(kv: &dyn Kv) -> ApiResult<(u64, String)> {
    // The last entry: scan backwards is not available, so keep a head hint and walk forward.
    let hint = crate::auth::read_counter(kv, "audit_head").await?;
    let mut seq = hint;
    let mut hash = String::new();
    if seq > 0
        && let Some(b) = kv.get(NS_AUDIT, &key(seq)).await?
    {
        let e: AuditEntry = serde_json::from_slice(&b).unwrap_or_default();
        hash = e.hash;
    }
    loop {
        match kv.get(NS_AUDIT, &key(seq + 1)).await? {
            Some(b) => {
                let e: AuditEntry = serde_json::from_slice(&b).unwrap_or_default();
                seq += 1;
                hash = e.hash;
            }
            None => return Ok((seq, hash)),
        }
    }
}

impl KvAudit {
    /// Starts the writer task.
    pub fn start(kv: Arc<dyn Kv>, metrics: Arc<crate::metrics::Metrics>) -> Arc<KvAudit> {
        let (tx, mut rx) = mpsc::channel::<Job>(65_536);
        let writer_kv = kv.clone();
        tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                let mut batch = vec![first];
                while batch.len() < 1024 {
                    match rx.try_recv() {
                        Ok(j) => batch.push(j),
                        Err(_) => break,
                    }
                }
                let result = write_batch(writer_kv.as_ref(), &mut batch).await;
                for (i, (entry, reply)) in batch.into_iter().enumerate() {
                    let r = match &result {
                        Ok(first_seq) => {
                            metrics
                                .audit_entries
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            Ok(first_seq + i as u64)
                        }
                        Err(e) => Err(e.clone()),
                    };
                    let _ = entry;
                    let _ = reply.send(r);
                }
            }
        });
        Arc::new(KvAudit { kv, tx })
    }

    /// Verifies the chain from the first entry; returns the number of entries checked or the
    /// first broken sequence number.
    pub async fn verify(&self) -> ApiResult<Result<u64, u64>> {
        let mut prev = String::new();
        let mut n = 0u64;
        loop {
            let page = self.read(n + 1, 1000).await?;
            if page.is_empty() {
                return Ok(Ok(n));
            }
            for e in page {
                if e.seq != n + 1 || e.prev_hash != prev || e.compute_hash() != e.hash {
                    return Ok(Err(e.seq));
                }
                prev = e.hash.clone();
                n = e.seq;
            }
        }
    }
}

async fn write_batch(kv: &dyn Kv, batch: &mut [Job]) -> ApiResult<u64> {
    for _ in 0..100 {
        let (mut seq, mut prev) = head(kv).await?;
        let first = seq + 1;
        let mut ops = Vec::with_capacity(batch.len() * 2 + 1);
        for (entry, _) in batch.iter_mut() {
            seq += 1;
            entry.seq = seq;
            entry.prev_hash = prev.clone();
            entry.hash = entry.compute_hash();
            prev = entry.hash.clone();
            ops.push(KvOp::expect(NS_AUDIT, key(seq), None));
            ops.push(KvOp::put(
                NS_AUDIT,
                key(seq),
                serde_json::to_vec(entry).unwrap_or_default(),
            ));
        }
        ops.push(KvOp::put(
            crate::auth::NS_COUNTER,
            b"audit_head".to_vec(),
            seq.to_be_bytes().to_vec(),
        ));
        if kv.apply(ops).await? {
            return Ok(first);
        }
    }
    Err(ApiError::new(
        ErrorReason::Unavailable,
        "audit log contended",
    ))
}

#[async_trait]
impl AuditSink for KvAudit {
    async fn record(&self, entry: AuditEntry) -> ApiResult<u64> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send((entry, tx))
            .await
            .map_err(|_| ApiError::new(ErrorReason::Unavailable, "audit writer stopped"))?;
        rx.await
            .map_err(|_| ApiError::new(ErrorReason::Unavailable, "audit writer stopped"))?
    }

    async fn read(&self, from: u64, limit: usize) -> ApiResult<Vec<AuditEntry>> {
        let start = from.max(1);
        let after = key(start - 1);
        let rows = self
            .kv
            .scan(
                NS_AUDIT,
                b"",
                if start > 1 { Some(&after) } else { None },
                limit,
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_slice(&v).ok())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::SqliteKv;

    #[tokio::test]
    async fn chain_verifies_and_detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let kv: Arc<dyn Kv> = Arc::new(SqliteKv::open(&dir.path().join("kv.db")).unwrap());
        let audit = KvAudit::start(kv.clone(), Arc::new(crate::metrics::Metrics::new()));
        let mut handles = Vec::new();
        for i in 0..20u64 {
            let a = audit.clone();
            handles.push(tokio::spawn(async move {
                a.record(AuditEntry {
                    at_ms: i,
                    chain: vec!["P".into()],
                    action: "test".into(),
                    ..AuditEntry::default()
                })
                .await
                .unwrap()
            }));
        }
        let mut seqs = Vec::new();
        for h in handles {
            seqs.push(h.await.unwrap());
        }
        seqs.sort();
        assert_eq!(seqs, (1..=20).collect::<Vec<_>>());
        assert_eq!(audit.verify().await.unwrap(), Ok(20));
        let mut e: AuditEntry =
            serde_json::from_slice(&kv.get(NS_AUDIT, &key(7)).await.unwrap().unwrap()).unwrap();
        e.action = "forged".into();
        kv.put(NS_AUDIT, &key(7), serde_json::to_vec(&e).unwrap())
            .await
            .unwrap();
        assert_eq!(audit.verify().await.unwrap(), Err(7));
    }
}
