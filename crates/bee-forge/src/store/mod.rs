//! The forge store: one transactional row store with three backends.
//!
//! Forge data never lives in the VCS [`bee_store::MetaStore`] (SPEC §15.1). It lives in a
//! separate database (SQLite file set or Postgres schema, chosen by [`sql::SqlConfig`]) behind
//! the [`ForgeStore`] trait:
//!
//! - [`memory::MemoryStore`]: in-process, for tests and `--dev` without a database.
//! - [`sql::SqlStore`]: SQLite (always available, one file per surface) or Postgres (one
//!   schema, one bounded connection pool per surface).
//! - [`unavailable::UnavailableStore`]: always `UNAVAILABLE`, for failure-isolation tests.
//!
//! # Surfaces own their tables and pools
//!
//! Every table belongs to exactly one [`Surface`] (orgs, leases, checks, events, webhooks,
//! audit, and later changes, work, ...). A transaction is opened on one surface with
//! [`ForgeStore::begin`] and may touch only that surface's tables; touching another surface's
//! table is an `INTERNAL` error. Each surface has its own connection pool (SQL) or lock
//! (memory), so a backlog in one surface cannot take another's connections (SPEC §15.1).
//!
//! # Rows
//!
//! All forge tables share one physical shape, so a module defines tables declaratively
//! ([`TableDef`]) and backends need no per-table code:
//!
//! | column  | type        | meaning                                                     |
//! |---------|-------------|-------------------------------------------------------------|
//! | scope   | TEXT        | partition key: an org id, a repo id, or `""` for global     |
//! | id      | TEXT        | row id within the scope (ULID or natural key)               |
//! | k1, k2  | TEXT NULL   | secondary keys (names, changeset ids, holders), indexable   |
//! | n1      | BIGINT NULL | ordering number (sequence, expiry, time), indexable         |
//! | version | BIGINT      | row version for optimistic concurrency, starts at 1         |
//! | body    | BLOB/BYTEA  | the record, protobuf-encoded                                |
//!
//! Keys are text ULIDs (128-bit) and 64-bit integers; there is no 32-bit key anywhere
//! (SPEC §15.3).
//!
//! # Transactions
//!
//! One store method is one transaction: [`Db::begin`] opens it, [`Tx::commit`] commits it,
//! and dropping a [`Tx`] without committing rolls back. Updates are compare-and-swap on the
//! row version ([`ForgeTx::update`] fails with `VERSION_MISMATCH`). Every wait (pool permit,
//! lock, statement) is bounded by the [`crate::cx::Cx`] deadline.
//!
//! # Outbox
//!
//! A transaction records the events and audit entries its change implies with
//! [`Tx::emit_event`] and [`Tx::audit`]. They are written to the surface's own outbox table in
//! the same transaction, so they are durable exactly when the change is, and a relay moves
//! them to the event log and the audit chain ([`crate::outbox`]). The event log being down
//! therefore never fails a lease or a check report; their events arrive when it recovers.

pub mod conformance;
pub mod memory;
pub mod migrate;
pub mod sql;
pub mod unavailable;

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{AuditEntry, Event};
use prost::Message;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::IdGen;
pub use migrate::{Migration, MigrationOp, MigrationReport, Phase};

/// A failure domain that owns tables and a connection pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Surface(pub &'static str);

impl Surface {
    /// Orgs, members, invitations, repos and repo settings.
    pub const ORGS: Surface = Surface("orgs");
    /// Path leases.
    pub const LEASES: Surface = Surface("leases");
    /// Check runs, suites, annotations and commit statuses.
    pub const CHECKS: Surface = Surface("checks");
    /// The per-repo and per-org event logs.
    pub const EVENTS: Surface = Surface("events");
    /// Webhooks, delivery queue and delivery log.
    pub const WEBHOOKS: Surface = Surface("webhooks");
    /// The hash-chained audit log.
    pub const AUDIT: Surface = Surface("audit");

    /// The surface name.
    pub fn name(self) -> &'static str {
        self.0
    }

    /// Whether `name` is usable as a surface name (lower-case letters, digits, `_`).
    pub fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }
}

impl fmt::Display for Surface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// A secondary index a table declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Index {
    /// `(scope, k1)`.
    ScopeK1,
    /// `(scope, k2)`.
    ScopeK2,
    /// `(scope, n1)`.
    ScopeN1,
    /// `(k1)` across scopes.
    K1,
    /// `(k2)` across scopes.
    K2,
    /// `(n1)` across scopes.
    N1,
}

impl Index {
    /// Short name used in index names.
    pub fn suffix(self) -> &'static str {
        match self {
            Index::ScopeK1 => "sk1",
            Index::ScopeK2 => "sk2",
            Index::ScopeN1 => "sn1",
            Index::K1 => "k1",
            Index::K2 => "k2",
            Index::N1 => "n1",
        }
    }

    /// Indexed columns.
    pub fn columns(self) -> &'static str {
        match self {
            Index::ScopeK1 => "scope, k1",
            Index::ScopeK2 => "scope, k2",
            Index::ScopeN1 => "scope, n1",
            Index::K1 => "k1",
            Index::K2 => "k2",
            Index::N1 => "n1",
        }
    }
}

/// A table declaration. Names start with `forge_` and are unique across all modules.
#[derive(Debug, PartialEq, Eq)]
pub struct TableDef {
    /// Physical table name.
    pub name: &'static str,
    /// Owning surface.
    pub surface: Surface,
    /// `k1` is unique within a scope (NULLs never collide).
    pub unique_k1: bool,
    /// `k2` is unique within a scope (NULLs never collide).
    pub unique_k2: bool,
    /// Secondary indexes created with the table.
    pub indexes: &'static [Index],
}

/// One stored row (see the module docs for the column meanings).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    /// Partition key.
    pub scope: String,
    /// Row id within the scope.
    pub id: String,
    /// Secondary key 1.
    pub k1: Option<String>,
    /// Secondary key 2.
    pub k2: Option<String>,
    /// Ordering number.
    pub n1: Option<i64>,
    /// Row version; on [`ForgeTx::update`] the expected version.
    pub version: u64,
    /// Encoded record.
    pub body: Vec<u8>,
}

impl Row {
    /// A row with a scope and id and an empty body.
    pub fn new(scope: impl Into<String>, id: impl Into<String>) -> Row {
        Row {
            scope: scope.into(),
            id: id.into(),
            ..Row::default()
        }
    }

    /// Sets `k1`.
    pub fn k1(mut self, v: impl Into<String>) -> Row {
        self.k1 = Some(v.into());
        self
    }

    /// Sets `k2`.
    pub fn k2(mut self, v: impl Into<String>) -> Row {
        self.k2 = Some(v.into());
        self
    }

    /// Sets `n1`.
    pub fn n1(mut self, v: i64) -> Row {
        self.n1 = Some(v);
        self
    }

    /// Sets the version.
    pub fn version(mut self, v: u64) -> Row {
        self.version = v;
        self
    }

    /// Sets the body to the protobuf encoding of `m`.
    pub fn msg<M: Message>(mut self, m: &M) -> Row {
        self.body = m.encode_to_vec();
        self
    }

    /// Decodes the body.
    pub fn decode<M: Message + Default>(&self) -> Result<M> {
        Ok(M::decode(self.body.as_slice())?)
    }
}

/// A secondary-key filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// `k1 = value`.
    K1(String),
    /// `k2 = value`.
    K2(String),
}

/// Scan order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    /// By `(scope, id)`.
    #[default]
    Id,
    /// By `(n1, scope, id)`; rows with a NULL `n1` are skipped.
    N1,
}

/// A position in a scan: the last row returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// `n1` of the last row (ignored for [`Order::Id`]).
    pub n1: i64,
    /// Scope of the last row.
    pub scope: String,
    /// Id of the last row.
    pub id: String,
}

impl Cursor {
    /// The cursor after `row`.
    pub fn after(row: &Row) -> Cursor {
        Cursor {
            n1: row.n1.unwrap_or(0),
            scope: row.scope.clone(),
            id: row.id.clone(),
        }
    }
}

/// A range scan over one table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// Only rows of this scope; `None` scans every scope.
    pub scope: Option<String>,
    /// Secondary-key filter.
    pub key: Option<Key>,
    /// Only rows with `n1 >= n1_min`.
    pub n1_min: Option<i64>,
    /// Only rows with `n1 <= n1_max`.
    pub n1_max: Option<i64>,
    /// Order.
    pub order: Order,
    /// Descending instead of ascending.
    pub descending: bool,
    /// Start strictly after this position (in scan order).
    pub after: Option<Cursor>,
    /// Maximum rows returned (capped at [`MAX_SCAN`]).
    pub limit: usize,
}

/// Largest number of rows one scan returns.
pub const MAX_SCAN: usize = 10_000;

impl Scan {
    /// All rows of one scope, by id, up to `limit`.
    pub fn scope(scope: impl Into<String>, limit: usize) -> Scan {
        Scan {
            scope: Some(scope.into()),
            limit,
            ..Scan::default()
        }
    }

    /// Adds a key filter.
    pub fn key(mut self, key: Key) -> Scan {
        self.key = Some(key);
        self
    }

    /// Orders by `n1`.
    pub fn by_n1(mut self) -> Scan {
        self.order = Order::N1;
        self
    }

    /// Reverses the order.
    pub fn desc(mut self) -> Scan {
        self.descending = true;
        self
    }

    /// Starts after a cursor.
    pub fn after(mut self, c: Option<Cursor>) -> Scan {
        self.after = c;
        self
    }

    /// Bounds `n1` from below (inclusive).
    pub fn n1_min(mut self, v: i64) -> Scan {
        self.n1_min = Some(v);
        self
    }

    /// Bounds `n1` from above (inclusive).
    pub fn n1_max(mut self, v: i64) -> Scan {
        self.n1_max = Some(v);
        self
    }

    /// The effective limit.
    pub fn effective_limit(&self) -> usize {
        self.limit.clamp(1, MAX_SCAN)
    }

    /// Whether `row` passes the filters (not the cursor). Backends without SQL use it.
    pub fn matches(&self, row: &Row) -> bool {
        if self.scope.as_ref().is_some_and(|s| *s != row.scope) {
            return false;
        }
        match &self.key {
            Some(Key::K1(v)) if row.k1.as_deref() != Some(v.as_str()) => return false,
            Some(Key::K2(v)) if row.k2.as_deref() != Some(v.as_str()) => return false,
            _ => {}
        }
        if self.order == Order::N1 && row.n1.is_none() {
            return false;
        }
        if self.n1_min.is_some() || self.n1_max.is_some() {
            let Some(n) = row.n1 else { return false };
            if self.n1_min.is_some_and(|m| n < m) || self.n1_max.is_some_and(|m| n > m) {
                return false;
            }
        }
        true
    }

    /// The sort key of `row` in this scan's order.
    pub fn sort_key(&self, row: &Row) -> (i64, String, String) {
        match self.order {
            Order::Id => (0, row.scope.clone(), row.id.clone()),
            Order::N1 => (row.n1.unwrap_or(0), row.scope.clone(), row.id.clone()),
        }
    }

    /// Whether `row` is strictly after the cursor in scan order.
    pub fn past_cursor(&self, row: &Row) -> bool {
        let Some(c) = &self.after else { return true };
        let key = self.sort_key(row);
        let ck = match self.order {
            Order::Id => (0, c.scope.clone(), c.id.clone()),
            Order::N1 => (c.n1, c.scope.clone(), c.id.clone()),
        };
        if self.descending { key < ck } else { key > ck }
    }
}

/// A record waiting in a surface's outbox: an event for the event log or an audit entry for
/// the audit chain.
#[derive(Clone, PartialEq, Message)]
pub struct OutboxRecord {
    /// An event to append to its repo or org log.
    #[prost(message, optional, tag = "1")]
    pub event: Option<Event>,
    /// An audit entry to chain into its org's log (seq and hashes are assigned on append).
    #[prost(message, optional, tag = "2")]
    pub audit: Option<AuditEntry>,
}

/// A transaction on one surface. Dropping it without [`ForgeTx::commit`] rolls back.
#[async_trait]
pub trait ForgeTx: Send {
    /// The surface this transaction is bound to.
    fn surface(&self) -> Surface;

    /// Reads one row.
    async fn get(&mut self, t: &TableDef, scope: &str, id: &str) -> Result<Option<Row>>;

    /// Reads one row and locks it until the transaction ends (Postgres `FOR UPDATE`; the other
    /// backends serialize whole transactions per surface).
    async fn get_for_update(&mut self, t: &TableDef, scope: &str, id: &str) -> Result<Option<Row>>;

    /// Inserts a new row at version 1. `ALREADY_EXISTS` if `(scope, id)` or a unique key is
    /// taken.
    async fn insert(&mut self, t: &TableDef, row: &Row) -> Result<()>;

    /// Replaces a row if its stored version equals `row.version`; returns the new version.
    /// `NOT_FOUND` if absent, `VERSION_MISMATCH` (with the current version) if stale,
    /// `ALREADY_EXISTS` if a unique key is taken.
    async fn update(&mut self, t: &TableDef, row: &Row) -> Result<u64>;

    /// Inserts or replaces a row regardless of version; returns the new version.
    async fn put(&mut self, t: &TableDef, row: &Row) -> Result<u64>;

    /// Deletes a row; returns whether it existed.
    async fn delete(&mut self, t: &TableDef, scope: &str, id: &str) -> Result<bool>;

    /// Range scan.
    async fn scan(&mut self, t: &TableDef, q: &Scan) -> Result<Vec<Row>>;

    /// Number of rows in a scope (optionally with a key).
    async fn count(&mut self, t: &TableDef, scope: &str, key: Option<&Key>) -> Result<u64>;

    /// Adds `delta` to the `n1` of a counter row, creating it at `delta` if absent; returns the
    /// new value. The row stays locked until the transaction ends.
    async fn add_n1(&mut self, t: &TableDef, scope: &str, id: &str, delta: i64) -> Result<i64>;

    /// Appends to this surface's outbox under `id` (ULID, so the outbox is ordered).
    async fn outbox_push(&mut self, id: &str, record: &OutboxRecord) -> Result<()>;

    /// The oldest outbox entries.
    async fn outbox_peek(&mut self, limit: usize) -> Result<Vec<(String, OutboxRecord)>>;

    /// Removes outbox entries.
    async fn outbox_remove(&mut self, ids: &[String]) -> Result<()>;

    /// Commits.
    async fn commit(self: Box<Self>) -> Result<()>;
}

/// A backend-owned, drop-released safety publication lock.
pub type PublicationLock = Box<dyn Send + Sync>;

/// A forge store backend.
#[async_trait]
pub trait ForgeStore: Send + Sync {
    /// `memory`, `sqlite`, `postgres` or `unavailable`.
    fn backend(&self) -> &'static str;

    /// Applies pending migrations of every registered module (expand migrations always,
    /// contract migrations only when the store allows them).
    async fn migrate(&self, migrations: &[&Migration]) -> Result<MigrationReport>;

    /// Opens a transaction on `surface`, waiting for a pool slot no later than `deadline`.
    async fn begin(&self, surface: Surface, deadline: Instant) -> Result<Box<dyn ForgeTx>>;

    /// A shared publication permit or an exclusive stop/resume barrier for this organization.
    /// This lock must be independent of any lock taken by ordinary agents-store writes.
    async fn publication_lock(
        &self,
        _org: &str,
        _shared: bool,
        _deadline: Instant,
    ) -> Result<PublicationLock> {
        Err(ForgeError::unavailable(
            "Beehive publication lock unavailable",
        ))
    }
}

/// The store plus the id generator and the outbox relay signal: what domain modules hold.
#[derive(Clone)]
pub struct Db {
    store: Arc<dyn ForgeStore>,
    ids: Arc<IdGen>,
    kick: Arc<Notify>,
}

impl fmt::Debug for Db {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Db")
            .field("backend", &self.store.backend())
            .finish()
    }
}

impl Db {
    /// Wraps a store.
    pub fn new(store: Arc<dyn ForgeStore>, ids: Arc<IdGen>) -> Db {
        Db {
            store,
            ids,
            kick: Arc::new(Notify::new()),
        }
    }

    /// The backend.
    pub fn store(&self) -> &Arc<dyn ForgeStore> {
        &self.store
    }

    /// The id generator.
    pub fn ids(&self) -> &Arc<IdGen> {
        &self.ids
    }

    /// Current time from the injected clock.
    pub fn now_ms(&self) -> u64 {
        self.ids.now_ms()
    }

    /// Signalled after every commit that wrote to an outbox.
    pub fn outbox_signal(&self) -> &Arc<Notify> {
        &self.kick
    }

    /// Opens a transaction on `surface` for the call described by `cx`.
    pub async fn begin(&self, surface: Surface, cx: &Cx) -> Result<Tx> {
        cx.check_deadline()?;
        let inner = self.store.begin(surface, cx.deadline).await?;
        Ok(Tx {
            inner,
            ids: self.ids.clone(),
            kick: self.kick.clone(),
            cx: cx.clone(),
            emitted: false,
        })
    }
}

/// An open transaction with the call context: what domain code works with.
pub struct Tx {
    inner: Box<dyn ForgeTx>,
    ids: Arc<IdGen>,
    kick: Arc<Notify>,
    cx: Cx,
    emitted: bool,
}

impl Tx {
    /// The call context.
    pub fn cx(&self) -> &Cx {
        &self.cx
    }

    /// Current time from the injected clock.
    pub fn now_ms(&self) -> u64 {
        self.ids.now_ms()
    }

    /// A new ULID.
    pub fn new_id(&self) -> Result<String> {
        self.ids.ulid()
    }

    /// The raw backend transaction.
    pub fn raw(&mut self) -> &mut dyn ForgeTx {
        self.inner.as_mut()
    }

    /// See [`ForgeTx::get`].
    pub async fn get(&mut self, t: &TableDef, scope: &str, id: &str) -> Result<Option<Row>> {
        self.inner.get(t, scope, id).await
    }

    /// Reads and decodes a row's body.
    pub async fn get_msg<M: Message + Default>(
        &mut self,
        t: &TableDef,
        scope: &str,
        id: &str,
    ) -> Result<Option<(M, Row)>> {
        match self.inner.get(t, scope, id).await? {
            Some(row) => Ok(Some((row.decode()?, row))),
            None => Ok(None),
        }
    }

    /// See [`ForgeTx::get_for_update`].
    pub async fn get_for_update(
        &mut self,
        t: &TableDef,
        scope: &str,
        id: &str,
    ) -> Result<Option<Row>> {
        self.inner.get_for_update(t, scope, id).await
    }

    /// See [`ForgeTx::insert`].
    pub async fn insert(&mut self, t: &TableDef, row: &Row) -> Result<()> {
        self.inner.insert(t, row).await
    }

    /// See [`ForgeTx::update`].
    pub async fn update(&mut self, t: &TableDef, row: &Row) -> Result<u64> {
        self.inner.update(t, row).await
    }

    /// See [`ForgeTx::put`].
    pub async fn put(&mut self, t: &TableDef, row: &Row) -> Result<u64> {
        self.inner.put(t, row).await
    }

    /// See [`ForgeTx::delete`].
    pub async fn delete(&mut self, t: &TableDef, scope: &str, id: &str) -> Result<bool> {
        self.inner.delete(t, scope, id).await
    }

    /// See [`ForgeTx::scan`].
    pub async fn scan(&mut self, t: &TableDef, q: &Scan) -> Result<Vec<Row>> {
        self.inner.scan(t, q).await
    }

    /// Scans and decodes every body.
    pub async fn scan_msgs<M: Message + Default>(
        &mut self,
        t: &TableDef,
        q: &Scan,
    ) -> Result<Vec<(M, Row)>> {
        let rows = self.inner.scan(t, q).await?;
        rows.into_iter()
            .map(|r| Ok((r.decode()?, r)))
            .collect::<Result<Vec<_>>>()
    }

    /// See [`ForgeTx::count`].
    pub async fn count(&mut self, t: &TableDef, scope: &str, key: Option<&Key>) -> Result<u64> {
        self.inner.count(t, scope, key).await
    }

    /// See [`ForgeTx::add_n1`].
    pub async fn add_n1(&mut self, t: &TableDef, scope: &str, id: &str, delta: i64) -> Result<i64> {
        self.inner.add_n1(t, scope, id, delta).await
    }

    /// Records an event for the event log. `time`, `principal` and `on_behalf_of` are filled
    /// from the context when empty.
    pub async fn emit_event(&mut self, mut event: Event) -> Result<()> {
        if event.time.is_none() {
            event.time = Some(crate::ids::ts(self.now_ms()));
        }
        if event.principal.is_empty() {
            event.principal = self.cx.actor_id.clone();
            event.on_behalf_of = self.cx.chain.iter().skip(1).cloned().collect();
        }
        let id = self.ids.ulid()?;
        self.inner
            .outbox_push(
                &id,
                &OutboxRecord {
                    event: Some(event),
                    audit: None,
                },
            )
            .await?;
        self.emitted = true;
        Ok(())
    }

    /// Records an audit entry for this call: actor, chain, request, source and decision come
    /// from the context; the caller names the action, target and state summaries.
    pub async fn audit(&mut self, entry: crate::audit::AuditDraft) -> Result<()> {
        self.audit_id(entry).await.map(|_| ())
    }

    /// [`Tx::audit`], returning the entry's id (`AuditEntry.id`, kept by the relay).
    pub async fn audit_id(&mut self, entry: crate::audit::AuditDraft) -> Result<String> {
        let e = entry.into_entry(&self.cx, self.now_ms(), self.ids.ulid()?);
        let entry_id = e.id.clone();
        let id = self.ids.ulid()?;
        self.inner
            .outbox_push(
                &id,
                &OutboxRecord {
                    event: None,
                    audit: Some(e),
                },
            )
            .await?;
        self.emitted = true;
        Ok(entry_id)
    }

    /// Commits and wakes the outbox relay if anything was emitted.
    pub async fn commit(self) -> Result<()> {
        let emitted = self.emitted;
        let kick = self.kick.clone();
        self.inner.commit().await?;
        if emitted {
            kick.notify_waiters();
            kick.notify_one();
        }
        Ok(())
    }
}

/// Maps a timeout at `deadline` to `DEADLINE_EXCEEDED`.
pub async fn within<T>(
    deadline: Instant,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout_at(deadline, fut).await {
        Ok(r) => r,
        Err(_) => Err(ForgeError::deadline()),
    }
}

/// Fails with `INTERNAL` if `t` belongs to another surface than the transaction.
pub fn check_surface(tx: Surface, t: &TableDef) -> Result<()> {
    if t.surface == tx {
        Ok(())
    } else {
        Err(ForgeError::internal(format!(
            "table {} belongs to surface {}, not {}",
            t.name, t.surface, tx
        )))
    }
}
