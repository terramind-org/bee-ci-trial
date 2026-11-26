//! Bridges events the VCS store writes (`ref.updated` and other data-path events) into the
//! forge event log.
//!
//! The dependency points one way: the forge reads the VCS store's per-repo event rows
//! ([`bee_store::MetaStore::read_events`]); the VCS data path never touches forge tables
//! (SPEC §15.1). If the forge is down, VCS events simply wait in the VCS store and are
//! bridged after recovery; the per-repo cursor and the dedupe key `vcs:<repo>:<seq>` make
//! bridging idempotent.
//!
//! Payload contract: a VCS event payload is a protobuf-encoded `bee.v1.Event` (its `seq`,
//! `repo_id` and `org_id` are overwritten here). A payload that does not decode as an
//! `Event` is treated as an encoded `bee.v1.Ref` for `ref.updated`, and otherwise bridged
//! with only its kind and time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bee_core::Ulid;
use bee_proto::v1::{Event, Ref, event};
use bee_store::{EventRow, MetaStore, ReadFrom, RepoId};
use prost::Message;
use tokio::sync::Mutex as AsyncMutex;

use super::{COUNTERS, EventLog};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Db, Surface};

/// Most VCS events bridged per call.
pub const BATCH: usize = 500;

/// Copies VCS events into the forge log.
pub struct VcsBridge {
    meta: Arc<dyn MetaStore>,
    log: Arc<EventLog>,
    db: Db,
    /// One sync per repo at a time in this process (the bridge worker and EventService reads
    /// both sync).
    repos: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl VcsBridge {
    /// A bridge from `meta` into `log`.
    pub fn new(meta: Arc<dyn MetaStore>, log: Arc<EventLog>, db: Db) -> VcsBridge {
        VcsBridge {
            meta,
            log,
            db,
            repos: Mutex::new(HashMap::new()),
        }
    }

    /// Bridges up to [`BATCH`] new events of one repo in one forge transaction (which also
    /// advances the repo's cursor); returns how many were appended. Listeners receive the
    /// events through their queues, so a slow listener never holds the log back.
    pub async fn sync_repo(&self, cx: &Cx, repo_id: &str, org_id: &str) -> Result<usize> {
        let ulid = Ulid::parse(repo_id).map_err(|e| ForgeError::invalid("bad_id", e.message))?;
        let lock = self
            .repos
            .lock()
            .map_err(|_| ForgeError::internal("bridge locks poisoned"))?
            .entry(repo_id.to_string())
            .or_default()
            .clone();
        let _one = lock.lock().await;
        let cursor_scope = format!("vcs:{repo_id}");
        let cursor = {
            let mut tx = self.db.begin(Surface::EVENTS, cx).await?;
            tx.get(&COUNTERS, &cursor_scope, "cursor")
                .await?
                .and_then(|r| r.n1)
                .unwrap_or(0) as u64
        };
        let rows = self
            .meta
            .read_events(RepoId(ulid), cursor + 1, BATCH, ReadFrom::Primary)
            .await
            .map_err(|e| ForgeError::unavailable(format!("vcs store: {e}")))?;
        let Some(last) = rows.last().map(|r| r.seq) else {
            return Ok(0);
        };
        let events = rows
            .into_iter()
            .map(|row| {
                let ev = bridged_event(&row, repo_id, org_id);
                (ev, format!("vcs:{repo_id}:{}", row.seq))
            })
            .collect();
        self.log
            .append_bridged(cx, events, &cursor_scope, last)
            .await
    }
}

/// The forge event for one VCS event row.
fn bridged_event(row: &EventRow, repo_id: &str, org_id: &str) -> Event {
    let decoded = match json_event(&row.kind, &row.payload) {
        Some(j) => Ok(j),
        None => Event::decode(row.payload.as_slice()),
    };
    let mut ev = match decoded {
        Ok(e) if !e.kind.is_empty() => e,
        _ => {
            let mut e = Event {
                kind: row.kind.clone(),
                ..Default::default()
            };
            if row.kind == "ref.updated"
                && let Ok(r) = Ref::decode(row.payload.as_slice())
            {
                e.body = Some(event::Body::RefUpdated(r));
            }
            e
        }
    };
    ev.kind = row.kind.clone();
    ev.repo_id = repo_id.to_string();
    ev.org_id = org_id.to_string();
    if ev.time.is_none() {
        ev.time = Some(ts(row.at_ms));
    }
    if ev.principal.is_empty() {
        ev.principal = "system".into();
    }
    ev
}
/// Decodes a JSON event payload (the server writes `ref.updated` as a JSON object with
/// `ref`, `new`, `version` and `principal_chain`): the object becomes `payload_json`, a
/// `ref.updated` gets its typed `Ref` body, and the chain names the principal. `None` when
/// the payload is not a JSON object.
fn json_event(kind: &str, payload: &[u8]) -> Option<Event> {
    if payload.first() != Some(&b'{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let obj = v.as_object()?;
    let chain: Vec<String> = obj
        .get("principal_chain")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut e = Event {
        kind: kind.to_string(),
        payload_json: v.to_string(),
        principal: chain.first().cloned().unwrap_or_default(),
        on_behalf_of: chain.iter().skip(1).cloned().collect(),
        ..Default::default()
    };
    if kind == "ref.updated" {
        let target = obj
            .get("new")
            .and_then(|n| n.as_str())
            .and_then(|t| bee_core::ObjectId::parse(t).ok())
            .map(|id| bee_proto::v1::ObjectId {
                raw: id.as_bytes().to_vec(),
            });
        e.body = Some(event::Body::RefUpdated(Ref {
            name: obj
                .get("ref")
                .and_then(|r| r.as_str())
                .unwrap_or_default()
                .to_string(),
            target,
            version: obj.get("version").and_then(|n| n.as_u64()).unwrap_or(0),
        }));
    }
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_ref_updates_get_a_typed_body() {
        let id = bee_object::Tree::empty().id().to_text();
        let payload = serde_json::json!({
            "ref": "main", "new": id, "version": 4,
            "principal_chain": ["agent1", "owner1"],
        })
        .to_string();
        let e = json_event("ref.updated", payload.as_bytes()).expect("json");
        assert_eq!(e.principal, "agent1");
        assert_eq!(e.on_behalf_of, vec!["owner1".to_string()]);
        match e.body {
            Some(event::Body::RefUpdated(r)) => {
                assert_eq!(r.name, "main");
                assert_eq!(r.version, 4);
                assert_eq!(r.target.map(|t| t.raw.len()), Some(bee_core::OBJECT_ID_LEN));
            }
            other => panic!("unexpected body {other:?}"),
        }
        assert!(json_event("ref.updated", b"\x0a\x04main").is_none());
    }
}
