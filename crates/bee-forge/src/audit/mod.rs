//! The append-only, hash-chained audit log (PRODUCT §3.3, `AuditService`).
//!
//! Every state-changing forge call records an [`AuditDraft`] in its own transaction (through
//! the surface outbox, so the entry is durable exactly when the change is); denied
//! state-changing calls are appended directly with the denial reason as `result`. Each entry
//! names who acted (the full delegation chain and its root), what (`action`, `target`,
//! `before`/`after` summaries), when, from where (request id, address, user agent, token id)
//! and the policy decision that allowed it.
//!
//! # Chain
//!
//! Each org has its own log (`org:<id>`; entries without an org go to `system`) with
//! sequence numbers 1, 2, 3, ... without gaps. Entry `n` stores `prev_hash` = hash of entry
//! `n-1` (32 zero bytes for the first) and
//!
//! ```text
//! hash = BLAKE3(prev_hash || protobuf(entry with hash cleared))
//! ```
//!
//! prost encodes fields in tag order and the entry has no maps, so the encoding is canonical.
//! [`AuditLog::verify`] recomputes the chain and reports the first broken sequence: an edited
//! entry (`hash_mismatch`), a re-linked one (`prev_hash_mismatch`), a removed one (`gap`) or
//! a head that differs from an anchor the caller recorded (`head_mismatch`).
//!
//! # Policy decision
//!
//! `AuditEntry` has no field for the policy decision, so `after` is always a JSON object
//! `{"policy_decision": "...", "state": <summary or null>}` (recorded as an API gap in
//! `docs/spec/forge-core.md`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bee_proto::v1::{
    AuditEntry, AuditLogFilter, Principal, RepoRef, SubjectKind, SubjectRef,
    VerifyAuditChainResponse, repo_ref,
};
use prost::Message;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::module::ForgeModule;
use crate::store::{Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, Surface, TableDef};

/// Audit entries, scoped by log, ordered by sequence; `k1` is the entry ULID, `k2` the actor.
pub static ENTRIES: TableDef = TableDef {
    name: "forge_audit_entries",
    surface: Surface::AUDIT,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::ScopeN1, Index::ScopeK2],
};

/// Chain heads: per log, `n1` = last sequence, body = [`Head`].
pub static HEADS: TableDef = TableDef {
    name: "forge_audit_heads",
    surface: Surface::AUDIT,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

static M1: Migration = Migration {
    module: "audit",
    version: 1,
    name: "create_audit_log",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&ENTRIES),
        MigrationOp::CreateTable(&HEADS),
    ],
};

/// What the audit module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "audit",
    surfaces: &[Surface::AUDIT],
    migrations: &[&M1],
    event_kinds: &[],
    audit_prefixes: &[],
};

/// The chain head record.
#[derive(Clone, PartialEq, Message)]
pub struct Head {
    /// Hash of the last entry.
    #[prost(bytes = "vec", tag = "1")]
    pub hash: Vec<u8>,
}

/// `prev_hash` of the first entry.
pub const GENESIS: [u8; 32] = [0; 32];

/// The audit log key of an org.
pub fn org_scope(org_id: &str) -> String {
    if org_id.is_empty() {
        "system".to_string()
    } else {
        format!("org:{org_id}")
    }
}

/// The log an entry belongs to.
pub fn scope_of(e: &AuditEntry) -> String {
    if !e.org.is_empty() {
        org_scope(&e.org)
    } else if !e.enterprise.is_empty() {
        format!("enterprise:{}", e.enterprise)
    } else {
        "system".to_string()
    }
}

/// `BLAKE3(prev_hash || protobuf(entry without hash))`.
pub fn entry_hash(e: &AuditEntry) -> Vec<u8> {
    let mut clean = e.clone();
    clean.hash.clear();
    let mut h = blake3::Hasher::new();
    h.update(&e.prev_hash);
    h.update(&clean.encode_to_vec());
    h.finalize().as_bytes().to_vec()
}

/// What a state-changing call records; the context supplies who, from where and why.
#[derive(Clone, Debug, Default)]
pub struct AuditDraft {
    action: String,
    org: String,
    repo_id: String,
    target: Option<SubjectRef>,
    before: String,
    after: String,
    result: String,
}

impl AuditDraft {
    /// An entry for `action` (e.g. `lease.acquire`) in `org_id`'s log.
    pub fn new(action: &str, org_id: &str) -> AuditDraft {
        AuditDraft {
            action: action.to_string(),
            org: org_id.to_string(),
            ..AuditDraft::default()
        }
    }

    /// The repo acted on (ignored when empty).
    pub fn repo_id(mut self, id: &str) -> AuditDraft {
        self.repo_id = id.to_string();
        self
    }

    /// The object acted on.
    pub fn target(mut self, kind: SubjectKind, id: &str) -> AuditDraft {
        self.target = Some(SubjectRef {
            kind: kind as i32,
            repo: None,
            id: id.to_string(),
        });
        self
    }

    /// The object acted on, without a subject kind.
    pub fn target_id(self, id: &str) -> AuditDraft {
        self.target(SubjectKind::Unspecified, id)
    }

    /// JSON summary of the state before.
    pub fn before(mut self, json: &str) -> AuditDraft {
        self.before = json.to_string();
        self
    }

    /// JSON summary of the state after.
    pub fn after(mut self, json: &str) -> AuditDraft {
        self.after = json.to_string();
        self
    }

    /// `ok` (default) or an `ErrorReason` name for an audited denial.
    pub fn result(mut self, r: &str) -> AuditDraft {
        self.result = r.to_string();
        self
    }

    /// The full entry (without seq and hashes).
    pub fn into_entry(self, cx: &Cx, now_ms: u64, id: String) -> AuditEntry {
        let state: serde_json::Value = if self.after.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&self.after)
                .unwrap_or(serde_json::Value::String(self.after.clone()))
        };
        let after = serde_json::json!({ "policy_decision": cx.decision, "state": state });
        let repo = (!self.repo_id.is_empty()).then(|| RepoRef {
            repo: Some(repo_ref::Repo::Id(self.repo_id.clone())),
        });
        let mut target = self.target;
        if let Some(t) = target.as_mut()
            && t.repo.is_none()
        {
            t.repo = repo.clone();
        }
        let target_display = target
            .as_ref()
            .map(|t| {
                let kind = bee_proto::v1::SubjectKind::try_from(t.kind)
                    .map(|k| {
                        k.as_str_name()
                            .trim_start_matches("SUBJECT_KIND_")
                            .to_lowercase()
                    })
                    .unwrap_or_default();
                format!("{kind} {}", t.id).trim().to_string()
            })
            .unwrap_or_default();
        AuditEntry {
            seq: 0,
            id,
            target_display,
            policy_rule: cx.decision.clone(),
            prev_hash: Vec::new(),
            hash: Vec::new(),
            time: Some(ts(now_ms)),
            actor: Some(Principal {
                id: cx.actor_id.clone(),
                kind: cx.actor_kind as i32,
                ..Default::default()
            }),
            delegation_chain: cx.chain.clone(),
            on_behalf_of: cx.root().to_string(),
            action: self.action,
            target,
            repo,
            org: self.org,
            enterprise: String::new(),
            request_id: cx.request_id.clone(),
            ip: cx.ip.clone(),
            ip_hashed: false,
            user_agent: cx.user_agent.clone(),
            before: self.before,
            after: after.to_string(),
            reason: cx.reason.clone(),
            token_id: cx.token_id.clone(),
            result: if self.result.is_empty() {
                "ok".to_string()
            } else {
                self.result
            },
            country_code: String::new(),
        }
    }
}

/// Lookup key of [`AuditLog::get`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKey {
    /// By sequence.
    Seq(u64),
    /// By ULID.
    Id(String),
}

impl From<&audit_entry_key::Key> for EntryKey {
    fn from(k: &audit_entry_key::Key) -> EntryKey {
        match k {
            audit_entry_key::Key::Seq(s) => EntryKey::Seq(*s),
            audit_entry_key::Key::Id(i) => EntryKey::Id(i.clone()),
        }
    }
}

/// Re-export of the request oneof, so callers need not name the generated module.
pub mod audit_entry_key {
    pub use bee_proto::v1::get_audit_entry_request::Key;
}

/// The audit log.
pub struct AuditLog {
    db: Db,
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    wake: Arc<Notify>,
}

fn seq_id(seq: u64) -> String {
    format!("{seq:020}")
}

impl AuditLog {
    /// The log over `db`.
    pub fn new(db: Db) -> AuditLog {
        AuditLog {
            db,
            locks: Mutex::new(HashMap::new()),
            wake: Arc::new(Notify::new()),
        }
    }

    fn lock(&self, scope: &str) -> Result<Arc<AsyncMutex<()>>> {
        let mut m = self
            .locks
            .lock()
            .map_err(|_| ForgeError::internal("audit locks poisoned"))?;
        Ok(m.entry(scope.to_string()).or_default().clone())
    }

    /// Chains `entry` into its log; appending an entry id twice returns the stored entry.
    pub async fn append(&self, cx: &Cx, mut entry: AuditEntry) -> Result<AuditEntry> {
        if entry.id.is_empty() {
            entry.id = self.db.ids().ulid()?;
        }
        let scope = scope_of(&entry);
        let lock = self.lock(&scope)?;
        let _g = lock.lock().await;
        let mut tx = self.db.begin(Surface::AUDIT, cx).await?;
        let dup = tx
            .scan(
                &ENTRIES,
                &Scan::scope(scope.clone(), 1).key(Key::K1(entry.id.clone())),
            )
            .await?;
        if let Some(row) = dup.first() {
            return row.decode();
        }
        let head = tx.get_for_update(&HEADS, &scope, "head").await?;
        let (last, prev) = match &head {
            Some(row) => (row.n1.unwrap_or(0) as u64, row.decode::<Head>()?.hash),
            None => (0, GENESIS.to_vec()),
        };
        entry.seq = last + 1;
        entry.prev_hash = prev;
        entry.hash = entry_hash(&entry);
        let actor = entry
            .actor
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_default();
        tx.insert(
            &ENTRIES,
            &Row::new(scope.clone(), seq_id(entry.seq))
                .k1(entry.id.clone())
                .k2(actor)
                .n1(entry.seq as i64)
                .msg(&entry),
        )
        .await?;
        tx.put(
            &HEADS,
            &Row::new(scope, "head").n1(entry.seq as i64).msg(&Head {
                hash: entry.hash.clone(),
            }),
        )
        .await?;
        tx.commit().await?;
        self.wake.notify_waiters();
        Ok(entry)
    }

    /// One entry by sequence or id.
    pub async fn get(&self, cx: &Cx, scope: &str, key: &EntryKey) -> Result<AuditEntry> {
        let mut tx = self.db.begin(Surface::AUDIT, cx).await?;
        let row = match key {
            EntryKey::Seq(s) => tx.get(&ENTRIES, scope, &seq_id(*s)).await?,
            EntryKey::Id(id) => tx
                .scan(&ENTRIES, &Scan::scope(scope, 1).key(Key::K1(id.clone())))
                .await?
                .into_iter()
                .next(),
        };
        match row {
            Some(r) => r.decode(),
            None => Err(ForgeError::not_found("audit entry")),
        }
    }

    /// Entries matching `filter`, newest first unless `oldest_first`, paged by sequence.
    pub async fn query(
        &self,
        cx: &Cx,
        scope: &str,
        filter: &AuditLogFilter,
        oldest_first: bool,
        page_token: &str,
        page_size: usize,
    ) -> Result<(Vec<AuditEntry>, String)> {
        let f = Filter::compile(filter)?;
        let page_size = page_size.clamp(1, 500);
        let mut cursor: Option<i64> = if page_token.is_empty() {
            None
        } else {
            Some(
                page_token
                    .parse()
                    .map_err(|_| ForgeError::invalid("bad_page_token", "bad page token"))?,
            )
        };
        let mut tx = self.db.begin(Surface::AUDIT, cx).await?;
        let mut out = Vec::new();
        for _ in 0..20 {
            let mut q = Scan::scope(scope, 500).by_n1();
            if !oldest_first {
                q = q.desc();
            }
            if let Some(c) = cursor {
                q = if oldest_first {
                    q.n1_min(c + 1)
                } else {
                    q.n1_max(c - 1)
                };
            }
            let rows = tx.scan(&ENTRIES, &q).await?;
            let exhausted = rows.len() < 500;
            for row in rows {
                cursor = row.n1;
                let e: AuditEntry = row.decode()?;
                if f.matches(&e) {
                    out.push(e);
                    if out.len() == page_size {
                        let next = cursor.map(|c| c.to_string()).unwrap_or_default();
                        return Ok((out, next));
                    }
                }
            }
            if exhausted {
                return Ok((out, String::new()));
            }
        }
        Ok((out, cursor.map(|c| c.to_string()).unwrap_or_default()))
    }

    /// Recomputes the chain over `[from_seq, to_seq]` (0 = first / latest).
    pub async fn verify(
        &self,
        cx: &Cx,
        scope: &str,
        from_seq: u64,
        to_seq: u64,
        expected_head: &[u8],
    ) -> Result<VerifyAuditChainResponse> {
        let mut tx = self.db.begin(Surface::AUDIT, cx).await?;
        let head = tx.get(&HEADS, scope, "head").await?;
        let head_seq = head.as_ref().and_then(|r| r.n1).unwrap_or(0) as u64;
        let head_hash = match &head {
            Some(r) => r.decode::<Head>()?.hash,
            None => Vec::new(),
        };
        let from = from_seq.max(1);
        let to = if to_seq == 0 {
            head_seq
        } else {
            to_seq.min(head_seq)
        };
        let mut resp = VerifyAuditChainResponse {
            ok: true,
            verified_from_seq: from,
            verified_to_seq: 0,
            ..Default::default()
        };
        if head_seq == 0 {
            resp.verified_from_seq = 0;
            return Ok(resp);
        }
        let fail = |resp: &mut VerifyAuditChainResponse, seq: u64, why: &str| {
            resp.ok = false;
            resp.first_broken_seq = seq;
            resp.failure = why.to_string();
        };
        let mut expect_seq = from;
        let mut prev: Option<Vec<u8>> = if from == 1 {
            Some(GENESIS.to_vec())
        } else {
            None
        };
        let mut last_hash = Vec::new();
        'outer: while expect_seq <= to {
            let rows = tx
                .scan(
                    &ENTRIES,
                    &Scan::scope(scope, 500)
                        .by_n1()
                        .n1_min(expect_seq as i64)
                        .n1_max(to as i64),
                )
                .await?;
            if rows.is_empty() {
                fail(&mut resp, expect_seq, "gap");
                break;
            }
            for row in rows {
                let e: AuditEntry = row.decode()?;
                if e.seq != expect_seq || row.n1 != Some(expect_seq as i64) {
                    fail(&mut resp, expect_seq, "gap");
                    break 'outer;
                }
                if let Some(p) = &prev
                    && *p != e.prev_hash
                {
                    fail(&mut resp, e.seq, "prev_hash_mismatch");
                    break 'outer;
                }
                if entry_hash(&e) != e.hash {
                    fail(&mut resp, e.seq, "hash_mismatch");
                    break 'outer;
                }
                resp.verified_to_seq = e.seq;
                last_hash = e.hash.clone();
                prev = Some(e.hash);
                expect_seq += 1;
            }
        }
        if resp.ok && to == head_seq && last_hash != head_hash {
            fail(&mut resp, head_seq, "head_mismatch");
        }
        if resp.ok && !expected_head.is_empty() && last_hash != expected_head {
            fail(&mut resp, to, "head_mismatch");
        }
        resp.head_hash = last_hash;
        Ok(resp)
    }

    /// Streams entries after `after_seq` (0 = only new ones) that match `filter`.
    pub async fn stream(
        self: &Arc<Self>,
        cx: &Cx,
        scope: &str,
        after_seq: u64,
        filter: &AuditLogFilter,
    ) -> Result<mpsc::Receiver<Result<AuditEntry>>> {
        let f = Filter::compile(filter)?;
        let mut cursor = if after_seq == 0 {
            let mut tx = self.db.begin(Surface::AUDIT, cx).await?;
            tx.get(&HEADS, scope, "head")
                .await?
                .and_then(|r| r.n1)
                .unwrap_or(0) as u64
        } else {
            after_seq
        };
        let (tx, rx) = mpsc::channel(256);
        let me = self.clone();
        let scope = scope.to_string();
        let base = cx.clone();
        tokio::spawn(async move {
            loop {
                let woken = me.wake.notified();
                let cx = base.clone().with_timeout(crate::cx::DEFAULT_DEADLINE);
                let rows = async {
                    let mut t = me.db.begin(Surface::AUDIT, &cx).await?;
                    t.scan(
                        &ENTRIES,
                        &Scan::scope(scope.clone(), 500)
                            .by_n1()
                            .n1_min(cursor as i64 + 1),
                    )
                    .await
                }
                .await;
                match rows {
                    Ok(rows) => {
                        let full = rows.len() == 500;
                        for row in rows {
                            cursor = row.n1.unwrap_or(0) as u64;
                            match row.decode::<AuditEntry>() {
                                Ok(e) if f.matches(&e) => {
                                    if tx.send(Ok(e)).await.is_err() {
                                        return;
                                    }
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    let _ = tx.send(Err(e)).await;
                                    return;
                                }
                            }
                        }
                        if full {
                            continue;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
                tokio::select! {
                    _ = woken => {}
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    _ = tx.closed() => return,
                }
            }
        });
        Ok(rx)
    }
}

/// A compiled [`AuditLogFilter`].
#[derive(Clone, Debug, Default)]
struct Filter {
    actor: Vec<String>,
    root: String,
    actions: Vec<String>,
    repo: Option<RepoRef>,
    repo_text: Vec<String>,
    target: Option<SubjectRef>,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    request_id: String,
    words: Vec<String>,
    denied_only: bool,
}

impl Filter {
    fn compile(f: &AuditLogFilter) -> Result<Filter> {
        let mut out = Filter {
            actor: if f.actor_id.is_empty() {
                Vec::new()
            } else {
                vec![f.actor_id.clone()]
            },
            root: f.on_behalf_of.clone(),
            actions: f.action_prefixes.clone(),
            repo: f.repo.clone().filter(|r| r.repo.is_some()),
            target: f.target.clone(),
            start_ms: f.start.as_ref().map(ts_ms),
            end_ms: f.end.as_ref().map(ts_ms),
            request_id: f.request_id.clone(),
            denied_only: f.denied_only,
            ..Filter::default()
        };
        for tok in f.phrase.split_whitespace() {
            match tok.split_once(':') {
                Some(("actor", v)) => out.actor.push(v.to_string()),
                Some(("action", v)) => out.actions.push(v.to_string()),
                Some(("repo", v)) => out.repo_text.push(v.to_string()),
                Some(("created", v)) => {
                    let (op, date) = if let Some(d) = v.strip_prefix(">=") {
                        (">=", d)
                    } else if let Some(d) = v.strip_prefix("<=") {
                        ("<=", d)
                    } else if let Some(d) = v.strip_prefix('>') {
                        (">", d)
                    } else if let Some(d) = v.strip_prefix('<') {
                        ("<", d)
                    } else {
                        ("=", v)
                    };
                    let day = parse_date(date).ok_or_else(|| {
                        ForgeError::invalid("bad_phrase", format!("bad date in {tok:?}"))
                    })?;
                    let next = day + 86_400_000;
                    match op {
                        ">=" => out.start_ms = Some(day),
                        ">" => out.start_ms = Some(next),
                        "<=" => out.end_ms = Some(next - 1),
                        "<" => out.end_ms = Some(day.saturating_sub(1)),
                        _ => {
                            out.start_ms = Some(day);
                            out.end_ms = Some(next - 1);
                        }
                    }
                }
                _ => out.words.push(tok.to_lowercase()),
            }
        }
        Ok(out)
    }

    fn matches(&self, e: &AuditEntry) -> bool {
        let actor = e.actor.as_ref().map(|a| a.id.as_str()).unwrap_or("");
        if !self.actor.is_empty() && !self.actor.iter().any(|a| a == actor) {
            return false;
        }
        if !self.root.is_empty() && self.root != e.on_behalf_of {
            return false;
        }
        if !self.actions.is_empty() && !self.actions.iter().any(|p| e.action.starts_with(p)) {
            return false;
        }
        let entry_repo = match e.repo.as_ref().and_then(|r| r.repo.as_ref()) {
            Some(repo_ref::Repo::Id(i)) | Some(repo_ref::Repo::FullName(i)) => i.as_str(),
            None => "",
        };
        if let Some(r) = &self.repo {
            let want = match &r.repo {
                Some(repo_ref::Repo::Id(i)) | Some(repo_ref::Repo::FullName(i)) => i.as_str(),
                None => "",
            };
            if want != entry_repo {
                return false;
            }
        }
        if !self.repo_text.is_empty() && !self.repo_text.iter().any(|r| r == entry_repo) {
            return false;
        }
        if let Some(t) = &self.target {
            let Some(et) = &e.target else { return false };
            if (t.kind != 0 && t.kind != et.kind) || (!t.id.is_empty() && t.id != et.id) {
                return false;
            }
        }
        let at = e.time.as_ref().map(ts_ms).unwrap_or(0);
        if self.start_ms.is_some_and(|s| at < s) || self.end_ms.is_some_and(|s| at > s) {
            return false;
        }
        if !self.request_id.is_empty() && self.request_id != e.request_id {
            return false;
        }
        // Denied calls record their ErrorReason as `result`; allowed ones "ok" or nothing.
        if self.denied_only && (e.result.is_empty() || e.result == "ok") {
            return false;
        }
        if !self.words.is_empty() {
            let hay = format!(
                "{} {} {} {}",
                e.action,
                e.target.as_ref().map(|t| t.id.as_str()).unwrap_or(""),
                e.before,
                e.after
            )
            .to_lowercase();
            if !self.words.iter().all(|w| hay.contains(w)) {
                return false;
            }
        }
        true
    }
}

/// Milliseconds at midnight UTC of a `YYYY-MM-DD` date.
fn parse_date(s: &str) -> Option<u64> {
    let mut it = s.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from 1970-01-01 (proleptic Gregorian), Howard Hinnant's algorithm.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days).ok().map(|d| d * 86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_parse() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2000-03-01"), Some(951_868_800_000));
        assert!(parse_date("2000-13-01").is_none());
    }

    #[test]
    fn hash_covers_every_field() {
        let mut e = AuditEntry {
            seq: 1,
            id: "A".into(),
            prev_hash: GENESIS.to_vec(),
            action: "repo.create".into(),
            ..Default::default()
        };
        let h = entry_hash(&e);
        e.hash = h.clone();
        assert_eq!(entry_hash(&e), h, "the stored hash is excluded");
        e.action = "repo.delete".into();
        assert_ne!(entry_hash(&e), h);
    }
}
