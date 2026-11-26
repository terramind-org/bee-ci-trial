//! The metadata store logic, written once over [`Backend`] and shared by Postgres and SQLite.
//!
//! # Transactions and locking
//!
//! Every method is one transaction, retried as a whole on serialization failures under
//! [`RetryPolicy::TRANSACTION`]. Read methods are single statements over immutable rows or
//! strongly read rows (refs, epochs, counters), so each runs as one autocommit statement.
//! Writes on Postgres:
//!
//! - Every write that can touch a GC candidate first takes `FOR KEY SHARE` on its org row and
//!   reads `gc_seq`, recording it as the touched generation of the rows it touches.
//!   `gc_begin`, `gc_sweep` and `redact` take `FOR UPDATE` on the org row instead, so a mark
//!   or sweep never interleaves with such a write, and a redaction never races an upload.
//! - `update_ref` runs SERIALIZABLE and locks the ref row `FOR UPDATE` before checking the
//!   expected version; K steps write K versions, K ref-log rows, the landed rows and the
//!   events (one block from the counter) in the same transaction.
//! - Content-addressed rows are inserted with `ON CONFLICT DO NOTHING`, batched through
//!   `unnest` arrays.
//!
//! On SQLite the single writer connection serializes all writes, so the lock clauses are
//! omitted and batches run as per-row prepared statements.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use bee_core::{
    BeeError, ChangeId, ObjectId, ObjectIdPrefix, ObjectKind, Reason, RefName, RepoPath, Subcode,
};
use bee_object::{Changeset, DeltaOp, hash_id};
use bee_store::retry::RetryPolicy;
use bee_store::{
    AccessCounters, ChangesetRow, DerivedRow, EventHead, EventRow, Expected, GcEpoch, LandedRow,
    MinVersion, NewChangeset, NewEvent, NewObject, ObjectRow, ObjectState, OrgId, OrgRow,
    PutOutcome, ReadFrom, Redacted, RedactionRow, RefLogEntry, RefRow, RefUpdate, RefUpdated,
    RepoId, RepoRow, SweepOutcome, SweepPlan, Table, UploadRecord, UploadRow, is_inline,
};

use crate::backend::{Backend, Dialect, Fail, Mode, Row, Tx, Value};
use crate::codec::{
    corrupt, decode_landed, encode_landed, int, limit, oid, parse_change_id, parse_oid,
    parse_opt_oid, parse_org, parse_path, parse_ref, parse_repo, parse_state, path_bytes,
    prefix_range, state_code, ulid_bytes,
};

type Res<T> = Result<T, Fail>;

/// Row reads and writes counted during one call; writes are flushed only on commit.
#[derive(Default)]
pub(crate) struct Tally {
    reads: Vec<(Table, u64)>,
    writes: Vec<(Table, u64)>,
    fetches: Vec<ObjectKind>,
    replica_reads: u64,
    replica_forwards: u64,
    changeset_bytes: u64,
}

impl Tally {
    fn read(&mut self, t: Table, n: u64) {
        self.reads.push((t, n));
    }

    fn write(&mut self, t: Table, n: u64) {
        self.writes.push((t, n));
    }
}

/// Internal surrogate keys of a repo.
#[derive(Clone, Copy, Debug)]
struct RepoKeys {
    org: i64,
    repo: i64,
    org_id: OrgId,
}

/// Elements of an `= ANY($n)` parameter.
enum Elems {
    Bytes(Vec<Vec<u8>>),
    I64(Vec<i64>),
}

impl Elems {
    fn ids<'a>(ids: impl IntoIterator<Item = &'a ObjectId>) -> Elems {
        Elems::Bytes(ids.into_iter().map(|i| i.as_bytes().to_vec()).collect())
    }

    fn is_empty(&self) -> bool {
        match self {
            Elems::Bytes(v) => v.is_empty(),
            Elems::I64(v) => v.is_empty(),
        }
    }

    fn array(&self) -> Value {
        match self {
            Elems::Bytes(v) => Value::Bytess(v.iter().cloned().map(Some).collect()),
            Elems::I64(v) => Value::I64s(v.iter().copied().map(Some).collect()),
        }
    }

    fn each(&self) -> Vec<Value> {
        match self {
            Elems::Bytes(v) => v.iter().cloned().map(Value::Bytes).collect(),
            Elems::I64(v) => v.iter().copied().map(Value::I64).collect(),
        }
    }
}

/// A column of a batched insert.
enum Col {
    I64(Vec<i64>),
    Bytes(Vec<Vec<u8>>),
    OptBytes(Vec<Option<Vec<u8>>>),
    Text(Vec<String>),
}

impl Col {
    fn len(&self) -> usize {
        match self {
            Col::I64(v) => v.len(),
            Col::Bytes(v) => v.len(),
            Col::OptBytes(v) => v.len(),
            Col::Text(v) => v.len(),
        }
    }

    fn array(&self) -> Value {
        match self {
            Col::I64(v) => Value::I64s(v.iter().copied().map(Some).collect()),
            Col::Bytes(v) => Value::Bytess(v.iter().cloned().map(Some).collect()),
            Col::OptBytes(v) => Value::Bytess(v.clone()),
            Col::Text(v) => Value::Texts(v.iter().cloned().map(Some).collect()),
        }
    }

    fn at(&self, i: usize) -> Value {
        match self {
            Col::I64(v) => Value::I64(v[i]),
            Col::Bytes(v) => Value::Bytes(v[i].clone()),
            Col::OptBytes(v) => v[i].clone().map(Value::Bytes).unwrap_or(Value::Null),
            Col::Text(v) => Value::Text(v[i].clone()),
        }
    }
}

fn not_found(what: impl std::fmt::Display) -> BeeError {
    BeeError::new(Reason::NotFound, format!("{what} not found"))
}

fn missing(id: &ObjectId) -> BeeError {
    BeeError::new(Reason::MissingObject, format!("object {id} has no row"))
        .with_payload("object", id.to_text())
}

fn redacted_error(id: &ObjectId) -> BeeError {
    BeeError::new(Reason::Redacted, format!("{id} is redacted"))
        .with_payload("object", id.to_text())
}

fn invalid(message: impl Into<String>) -> BeeError {
    BeeError::invalid(Subcode::InvalidField, message)
}

/// The same validation as the in-memory store: kind, inline rule, size and hash.
fn check_new_object(o: &NewObject) -> Result<(), BeeError> {
    let kind = o.id.kind();
    match kind {
        ObjectKind::Blob | ObjectKind::Tree | ObjectKind::Meta | ObjectKind::Derived(_) => {}
        ObjectKind::Changeset => {
            return Err(BeeError::invalid(
                Subcode::WrongKind,
                "changesets are stored only in their changeset row",
            ));
        }
        ObjectKind::Foreign => {
            return Err(BeeError::invalid(
                Subcode::WrongKind,
                "foreign objects are not stored",
            ));
        }
    }
    match &o.inline {
        Some(bytes) => {
            if !is_inline(kind, bytes.len()) {
                return Err(invalid(format!("{} cannot be stored inline", o.id)));
            }
            if bytes.len() as u64 != o.size {
                return Err(invalid(format!(
                    "size of {} disagrees with its inline bytes",
                    o.id
                )));
            }
            if hash_id(kind, bytes) != o.id {
                return Err(BeeError::new(
                    Reason::Integrity,
                    format!("inline bytes do not hash to {}", o.id),
                ));
            }
        }
        None => {
            if usize::try_from(o.size).is_ok_and(|s| is_inline(kind, s)) {
                return Err(invalid(format!("{} must be stored inline", o.id)));
            }
        }
    }
    int(o.size)?;
    Ok(())
}

/// Runs `$body` (an expression using `$tx: &mut B::Tx` and `$t: &mut Tally`) as one
/// transaction in `$mode`, retrying serialization failures.
macro_rules! txn {
    ($self:ident, $mode:expr, |$tx:ident, $t:ident| $body:expr) => {{
        $self.counters.transaction();
        if matches!($mode, Mode::Write | Mode::Serializable) {
            $self.counters.write_transaction();
        }
        let seed = $self.seed.fetch_add(1, Ordering::Relaxed);
        let policy = RetryPolicy::TRANSACTION;
        let mut tally = Tally::default();
        let mut attempt: u32 = 0;
        let out = loop {
            tally.writes.clear();
            let res = async {
                let mut owned = $self.db.begin($mode).await?;
                let r = {
                    let $tx = &mut owned;
                    let $t = &mut tally;
                    $body.await
                };
                match r {
                    Ok(v) => {
                        owned.commit().await?;
                        Ok(v)
                    }
                    Err(e) => {
                        owned.rollback().await;
                        Err(e)
                    }
                }
            }
            .await;
            match res {
                Ok(v) => break Ok(v),
                Err(Fail::Err(e)) => break Err(e),
                Err(Fail::Retry(e)) => {
                    attempt += 1;
                    if attempt >= policy.max_attempts {
                        break Err(e);
                    }
                    tokio::time::sleep(policy.delay(attempt, seed)).await;
                }
            }
        };
        $self.flush(&tally, out.is_ok());
        out
    }};
}

/// The shared store.
pub(crate) struct Core<B: Backend> {
    db: B,
    counters: Arc<AccessCounters>,
    read_wait: Duration,
    orgs: RwLock<HashMap<OrgId, i64>>,
    repos: RwLock<HashMap<RepoId, RepoKeys>>,
    seed: AtomicU64,
    /// Turns for ref updates, by ref (striped). See [`Core::ref_turn`].
    ref_turns: Vec<tokio::sync::Mutex<()>>,
}

/// Stripes of [`Core::ref_turns`]: two refs share a turn once in this many pairs.
const REF_TURNS: usize = 256;

/// Bytes of a changeset row read for its root: the encoding's heads and keys, up to 16
/// parent ids and the root id take at most 6 + 17 × 36 = 618 bytes.
const ROOT_PREFIX: usize = 1024;

fn d<B: Backend>() -> Dialect {
    B::DIALECT
}

impl<B: Backend> Core<B> {
    pub(crate) fn new(db: B, counters: Arc<AccessCounters>, read_wait: Duration) -> Core<B> {
        Core {
            db,
            counters,
            read_wait,
            orgs: RwLock::new(HashMap::new()),
            repos: RwLock::new(HashMap::new()),
            seed: AtomicU64::new(0x5eed),
            ref_turns: (0..REF_TURNS)
                .map(|_| tokio::sync::Mutex::new(()))
                .collect(),
        }
    }

    /// The turn a writer of `name` in `repo` waits for before its ref update.
    ///
    /// On Postgres a ref update is SERIALIZABLE and locks the ref row: of the writers waiting
    /// on that row, every one but the winner fails with a serialization error when it
    /// commits, and retries. Sixteen writers on one ref ran out of the ten retries that way.
    /// Writers in one process therefore take turns, so each sees the committed row and gets
    /// `VERSION_MISMATCH` or succeeds at once; only writers in other processes can still
    /// collide, and the retries are for them. The stripe is not part of any id.
    fn ref_turn(&self, repo: RepoId, name: &str) -> &tokio::sync::Mutex<()> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        repo.hash(&mut h);
        name.hash(&mut h);
        &self.ref_turns[(h.finish() % REF_TURNS as u64) as usize]
    }

    pub(crate) fn counters(&self) -> &Arc<AccessCounters> {
        &self.counters
    }

    fn flush(&self, t: &Tally, committed: bool) {
        for (table, n) in &t.reads {
            self.counters.read(*table, *n);
        }
        if committed {
            for (table, n) in &t.writes {
                self.counters.write(*table, *n);
            }
        }
        for k in &t.fetches {
            self.counters.fetch(*k);
        }
        for _ in 0..t.replica_reads {
            self.counters.replica_read();
        }
        for _ in 0..t.replica_forwards {
            self.counters.replica_forward();
        }
        self.counters.changeset_bytes(t.changeset_bytes);
    }

    // ----- helpers ----------------------------------------------------------------------

    /// Runs `sql` (containing `= ANY($n)` as its last parameter) for all elements.
    async fn select_in(
        tx: &mut B::Tx,
        sql: &str,
        mut fixed: Vec<Value>,
        elems: Elems,
    ) -> Res<Vec<Row>> {
        if elems.is_empty() {
            return Ok(Vec::new());
        }
        if d::<B>().arrays() {
            fixed.push(elems.array());
            return tx.query(sql, &fixed).await;
        }
        let sql = d::<B>().per_element(sql);
        let mut out = Vec::new();
        for e in elems.each() {
            let mut p = fixed.clone();
            p.push(e);
            out.extend(tx.query(&sql, &p).await?);
        }
        Ok(out)
    }

    /// Like [`Core::select_in`] for statements without rows; returns rows changed.
    async fn exec_in(tx: &mut B::Tx, sql: &str, mut fixed: Vec<Value>, elems: Elems) -> Res<u64> {
        if elems.is_empty() {
            return Ok(0);
        }
        if d::<B>().arrays() {
            fixed.push(elems.array());
            return tx.exec(sql, &fixed).await;
        }
        let sql = d::<B>().per_element(sql);
        let mut n = 0;
        for e in elems.each() {
            let mut p = fixed.clone();
            p.push(e);
            n += tx.exec(&sql, &p).await?;
        }
        Ok(n)
    }

    /// A batched insert: `pg` takes the columns as arrays after the fixed parameters;
    /// `lite` takes one row's values in the same parameter positions.
    async fn insert_rows(
        tx: &mut B::Tx,
        pg: &str,
        lite: &str,
        fixed: Vec<Value>,
        cols: Vec<Col>,
        returning: bool,
    ) -> Res<(Vec<Row>, u64)> {
        let n = cols.first().map(Col::len).unwrap_or(0);
        if n == 0 {
            return Ok((Vec::new(), 0));
        }
        if d::<B>().arrays() {
            let mut p = fixed;
            p.extend(cols.iter().map(Col::array));
            return if returning {
                let rows = tx.query(pg, &p).await?;
                let k = rows.len() as u64;
                Ok((rows, k))
            } else {
                Ok((Vec::new(), tx.exec(pg, &p).await?))
            };
        }
        let mut rows = Vec::new();
        let mut count = 0;
        for i in 0..n {
            let mut p = fixed.clone();
            p.extend(cols.iter().map(|c| c.at(i)));
            if returning {
                let r = tx.query(lite, &p).await?;
                count += r.len() as u64;
                rows.extend(r);
            } else {
                count += tx.exec(lite, &p).await?;
            }
        }
        Ok((rows, count))
    }

    fn cached_repo(&self, repo: RepoId) -> Option<RepoKeys> {
        self.repos
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&repo)
            .copied()
    }

    async fn repo_keys(&self, tx: &mut B::Tx, repo: RepoId) -> Res<RepoKeys> {
        if let Some(k) = self.cached_repo(repo) {
            return Ok(k);
        }
        let rows = tx
            .query(
                "SELECT r.org_key, r.repo_key, o.id FROM repos r JOIN orgs o ON o.org_key = r.org_key WHERE r.id = $1",
                &[ulid_bytes(repo.0)],
            )
            .await?;
        let row = rows.first().ok_or_else(|| not_found(repo))?;
        let k = RepoKeys {
            org: row.i64(0)?,
            repo: row.i64(1)?,
            org_id: parse_org(&row.bytes(2)?)?,
        };
        self.repos
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(repo, k);
        Ok(k)
    }

    async fn org_key(&self, tx: &mut B::Tx, org: OrgId) -> Res<i64> {
        if let Some(k) = self
            .orgs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&org)
        {
            return Ok(*k);
        }
        let rows = tx
            .query(
                "SELECT org_key FROM orgs WHERE id = $1",
                &[ulid_bytes(org.0)],
            )
            .await?;
        let k = rows.first().ok_or_else(|| not_found(org))?.i64(0)?;
        self.orgs
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(org, k);
        Ok(k)
    }

    /// Resolves repo keys on the primary (so a lagging replica never answers NOT_FOUND for
    /// a repo that exists).
    async fn repo_keys_primary(&self, repo: RepoId) -> Res<RepoKeys> {
        if let Some(k) = self.cached_repo(repo) {
            return Ok(k);
        }
        let mut tx = self.db.begin(Mode::Read).await?;
        let r = self.repo_keys(&mut tx, repo).await;
        tx.rollback().await;
        r
    }

    async fn org_key_primary(&self, org: OrgId) -> Res<i64> {
        let mut tx = self.db.begin(Mode::Read).await?;
        let r = self.org_key(&mut tx, org).await;
        tx.rollback().await;
        r
    }

    /// Takes the org row lock and returns `gc_seq` (see the module docs).
    async fn lock_org(tx: &mut B::Tx, org_key: i64, exclusive: bool) -> Res<i64> {
        let clause = if exclusive {
            d::<B>().lock("FOR UPDATE")
        } else {
            d::<B>().lock("FOR KEY SHARE")
        };
        let rows = tx
            .query(
                &format!("SELECT gc_seq FROM orgs WHERE org_key = $1 {clause}"),
                &[Value::I64(org_key)],
            )
            .await?;
        Ok(rows
            .first()
            .ok_or_else(|| Fail::internal("org row vanished"))?
            .i64(0)?)
    }

    /// Where a read with `read` runs (SPEC §6.3): the replica, unless the caller's
    /// `min_version` is newer than the replica after waiting up to `read_wait`.
    async fn read_mode(&self, read: &ReadFrom, t: &mut Tally) -> Result<Mode, BeeError> {
        match read {
            ReadFrom::Primary => Ok(Mode::Read),
            ReadFrom::Replica(min) => {
                t.replica_reads += 1;
                if !self.db.has_replica() {
                    return Ok(Mode::Read);
                }
                if let Some(m) = min
                    && !self.replica_reached(m).await.map_err(fail_err)?
                {
                    t.replica_forwards += 1;
                    return Ok(Mode::Read);
                }
                Ok(Mode::Replica)
            }
        }
    }

    async fn replica_reached(&self, min: &MinVersion) -> Res<bool> {
        let (sql, repo, last) = match min {
            MinVersion::Ref {
                repo,
                name,
                version,
            } => (
                "SELECT 1 FROM ref_log WHERE org_key = $1 AND repo_key = $2 AND name = $3 AND version = $4",
                *repo,
                vec![Value::from(name.as_str()), Value::I64(int(*version)?)],
            ),
            MinVersion::Event { repo, seq } => (
                "SELECT 1 FROM events WHERE org_key = $1 AND repo_key = $2 AND seq = $3",
                *repo,
                vec![Value::I64(int(*seq)?)],
            ),
        };
        let k = match self.repo_keys_primary(repo).await {
            Ok(k) => k,
            Err(Fail::Err(e)) if e.reason == Reason::NotFound => return Ok(true),
            Err(e) => return Err(e),
        };
        let mut params = vec![Value::I64(k.org), Value::I64(k.repo)];
        params.extend(last);
        let deadline = Instant::now() + self.read_wait;
        loop {
            let mut tx = self.db.begin(Mode::Replica).await?;
            let found = tx.query(sql, &params).await;
            tx.rollback().await;
            if !found?.is_empty() {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    // ----- orgs and repos ---------------------------------------------------------------

    pub(crate) async fn create_org(&self, org: OrgRow) -> Result<PutOutcome, BeeError> {
        txn!(self, Mode::Write, |tx, t| async {
            t.read(Table::Orgs, 1);
            let rows = tx
                .query(
                    "INSERT INTO orgs (id, name, created_at_ms) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING RETURNING org_key",
                    &[
                        ulid_bytes(org.id.0),
                        Value::from(org.name.as_str()),
                        Value::I64(int(org.created_at_ms)?),
                    ],
                )
                .await?;
            if !rows.is_empty() {
                t.write(Table::Orgs, 1);
                return Ok(PutOutcome::Inserted);
            }
            let existing = tx
                .query(
                    "SELECT name, created_at_ms FROM orgs WHERE id = $1",
                    &[ulid_bytes(org.id.0)],
                )
                .await?;
            match existing.first() {
                Some(r) => {
                    let same = OrgRow {
                        id: org.id,
                        name: r.text(0)?,
                        created_at_ms: r.u64(1)?,
                    } == org;
                    if same {
                        Ok(PutOutcome::AlreadyPresent)
                    } else {
                        Err(BeeError::new(
                            Reason::AlreadyExists,
                            format!("{} exists with different fields", org.id),
                        )
                        .into())
                    }
                }
                None => Err(BeeError::new(
                    Reason::AlreadyExists,
                    format!("org name {:?} is taken", org.name),
                )
                .into()),
            }
        })
    }

    fn org_row(id: OrgId, r: &Row, name_col: usize) -> Result<OrgRow, BeeError> {
        Ok(OrgRow {
            id,
            name: r.text(name_col)?,
            created_at_ms: r.u64(name_col + 1)?,
        })
    }

    pub(crate) async fn get_org(&self, id: OrgId) -> Result<Option<OrgRow>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            t.read(Table::Orgs, 1);
            let rows = tx
                .query(
                    "SELECT name, created_at_ms FROM orgs WHERE id = $1",
                    &[ulid_bytes(id.0)],
                )
                .await?;
            Ok(rows.first().map(|r| Self::org_row(id, r, 0)).transpose()?)
        })
    }

    pub(crate) async fn org_by_name(&self, name: &str) -> Result<Option<OrgRow>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            t.read(Table::Orgs, 1);
            let rows = tx
                .query(
                    "SELECT id, name, created_at_ms FROM orgs WHERE name = $1",
                    &[Value::from(name)],
                )
                .await?;
            Ok(rows
                .first()
                .map(|r| Self::org_row(parse_org(&r.bytes(0)?)?, r, 1))
                .transpose()?)
        })
    }

    fn repo_row(r: &Row) -> Result<RepoRow, BeeError> {
        Ok(RepoRow {
            id: parse_repo(&r.bytes(0)?)?,
            org: parse_org(&r.bytes(1)?)?,
            name: r.text(2)?,
            created_at_ms: r.u64(3)?,
            draft_retention_days: u32::try_from(r.i64(4)?)
                .map_err(|_| corrupt("draft retention"))?,
        })
    }

    const REPO_COLS: &'static str = "r.id, o.id, r.name, r.created_at_ms, r.draft_retention_days FROM repos r JOIN orgs o ON o.org_key = r.org_key";

    pub(crate) async fn create_repo(&self, repo: RepoRow) -> Result<PutOutcome, BeeError> {
        let by_id = format!("SELECT {} WHERE r.id = $1", Self::REPO_COLS);
        txn!(self, Mode::Write, |tx, t| async {
            t.read(Table::Repos, 1);
            let existing = tx.query(&by_id, &[ulid_bytes(repo.id.0)]).await?;
            let same_or_taken = |rows: &[Row]| -> Res<Option<PutOutcome>> {
                match rows.first() {
                    Some(r) if Self::repo_row(r)? == repo => Ok(Some(PutOutcome::AlreadyPresent)),
                    Some(_) => Err(BeeError::new(
                        Reason::AlreadyExists,
                        format!("{} exists with different fields", repo.id),
                    )
                    .into()),
                    None => Ok(None),
                }
            };
            if let Some(o) = same_or_taken(&existing)? {
                return Ok(o);
            }
            let org_key = self.org_key(tx, repo.org).await?;
            let rows = tx
                .query(
                    "INSERT INTO repos (org_key, id, name, created_at_ms, draft_retention_days) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING RETURNING repo_key",
                    &[
                        Value::I64(org_key),
                        ulid_bytes(repo.id.0),
                        Value::from(repo.name.as_str()),
                        Value::I64(int(repo.created_at_ms)?),
                        Value::I64(repo.draft_retention_days.into()),
                    ],
                )
                .await?;
            let Some(row) = rows.first() else {
                let again = tx.query(&by_id, &[ulid_bytes(repo.id.0)]).await?;
                if let Some(o) = same_or_taken(&again)? {
                    return Ok(o);
                }
                return Err(BeeError::new(
                    Reason::AlreadyExists,
                    format!("repo name {:?} is taken", repo.name),
                )
                .into());
            };
            let repo_key = row.i64(0)?;
            let keys = [Value::I64(org_key), Value::I64(repo_key)];
            tx.exec(
                "INSERT INTO event_counters (org_key, repo_key, allocated) VALUES ($1, $2, 0)",
                &keys,
            )
            .await?;
            tx.exec(
                "INSERT INTO redaction_epochs (org_key, repo_key, epoch) VALUES ($1, $2, 0)",
                &keys,
            )
            .await?;
            t.write(Table::Repos, 1);
            Ok(PutOutcome::Inserted)
        })
    }

    pub(crate) async fn get_repo(&self, id: RepoId) -> Result<Option<RepoRow>, BeeError> {
        let sql = format!("SELECT {} WHERE r.id = $1", Self::REPO_COLS);
        txn!(self, Mode::Read, |tx, t| async {
            t.read(Table::Repos, 1);
            let rows = tx.query(&sql, &[ulid_bytes(id.0)]).await?;
            Ok(rows.first().map(Self::repo_row).transpose()?)
        })
    }

    pub(crate) async fn repo_by_name(
        &self,
        org: OrgId,
        name: &str,
    ) -> Result<Option<RepoRow>, BeeError> {
        let sql = format!("SELECT {} WHERE o.id = $1 AND r.name = $2", Self::REPO_COLS);
        txn!(self, Mode::Read, |tx, t| async {
            t.read(Table::Repos, 1);
            let rows = tx
                .query(&sql, &[ulid_bytes(org.0), Value::from(name)])
                .await?;
            Ok(rows.first().map(Self::repo_row).transpose()?)
        })
    }

    pub(crate) async fn list_repos(&self, org: OrgId) -> Result<Vec<RepoRow>, BeeError> {
        let sql = format!("SELECT {} WHERE o.id = $1 ORDER BY r.id", Self::REPO_COLS);
        txn!(self, Mode::Read, |tx, t| async {
            let rows = tx.query(&sql, &[ulid_bytes(org.0)]).await?;
            t.read(Table::Repos, rows.len() as u64);
            Ok(rows
                .iter()
                .map(Self::repo_row)
                .collect::<Result<Vec<_>, _>>()?)
        })
    }

    // ----- objects ----------------------------------------------------------------------

    pub(crate) async fn put_objects(
        &self,
        org: OrgId,
        objects: Vec<NewObject>,
        upload: Option<UploadRecord>,
        at_ms: u64,
    ) -> Result<Vec<PutOutcome>, BeeError> {
        txn!(self, Mode::Write, |tx, t| self.put_objects_in(
            tx,
            t,
            org,
            &objects,
            upload.as_ref(),
            at_ms
        ))
    }

    /// The body of [`Core::put_objects`], inside the caller's write transaction.
    async fn put_objects_in(
        &self,
        tx: &mut B::Tx,
        t: &mut Tally,
        org: OrgId,
        objects: &[NewObject],
        upload: Option<&UploadRecord>,
        at_ms: u64,
    ) -> Res<Vec<PutOutcome>> {
        let g = d::<B>().greatest();
        let upload_pg = format!(
            "INSERT INTO object_uploads (org_key, object, principal, repo_key, at_ms) \
             SELECT $1::bigint, u.o, $2::text, $3::bigint, $4::bigint FROM unnest($5::bytea[]) AS u(o) \
             ON CONFLICT (org_key, object, principal) DO UPDATE SET \
             at_ms = {g}(object_uploads.at_ms, excluded.at_ms), \
             repo_key = COALESCE(excluded.repo_key, object_uploads.repo_key)"
        );
        let upload_lite = format!(
            "INSERT INTO object_uploads (org_key, object, principal, repo_key, at_ms) \
             VALUES ($1, $5, $2, $3, $4) \
             ON CONFLICT (org_key, object, principal) DO UPDATE SET \
             at_ms = {g}(object_uploads.at_ms, excluded.at_ms), \
             repo_key = COALESCE(excluded.repo_key, object_uploads.repo_key)"
        );
        let org_key = self.org_key(tx, org).await?;
        let gc = Self::lock_org(tx, org_key, false).await?;
        let ids: Vec<ObjectId> = objects.iter().map(|o| o.id).collect();
        let redacted: BTreeSet<Vec<u8>> = Self::select_in(
            tx,
            "SELECT object FROM redactions WHERE org_key = $1 AND object = ANY($2)",
            vec![Value::I64(org_key)],
            Elems::ids(&ids),
        )
        .await?
        .iter()
        .map(|r| r.bytes(0))
        .collect::<Result<_, _>>()?;
        for obj in objects {
            check_new_object(obj)?;
            t.read(Table::Redactions, 1);
            if redacted.contains(obj.id.as_bytes().as_slice()) {
                return Err(redacted_error(&obj.id).into());
            }
        }
        let upload_repo = match upload.and_then(|u| u.repo) {
            Some(repo) => {
                let k = self.repo_keys(tx, repo).await?;
                if k.org != org_key {
                    return Err(not_found(repo).into());
                }
                Some(k.repo)
            }
            None => None,
        };
        // First occurrence of each id, in input order.
        let mut first: BTreeMap<ObjectId, usize> = BTreeMap::new();
        for (i, o) in objects.iter().enumerate() {
            first.entry(o.id).or_insert(i);
        }
        let uniq: Vec<&NewObject> = {
            let mut idx: Vec<usize> = first.values().copied().collect();
            idx.sort_unstable();
            idx.into_iter().map(|i| &objects[i]).collect()
        };
        t.read(Table::Objects, objects.len() as u64);
        let (rows, _) = Self::insert_rows(
            tx,
            "INSERT INTO objects (org_key, object, size, state, inline_bytes, created_at_ms, touched_gen) \
             SELECT $1::bigint, u.o, u.s, 0, u.i, $2::bigint, $3::bigint \
             FROM unnest($4::bytea[], $5::int8[], $6::bytea[]) AS u(o, s, i) \
             ON CONFLICT DO NOTHING RETURNING object",
            "INSERT INTO objects (org_key, object, size, state, inline_bytes, created_at_ms, touched_gen) \
             VALUES ($1, $4, $5, 0, $6, $2, $3) ON CONFLICT DO NOTHING RETURNING object",
            vec![Value::I64(org_key), Value::I64(int(at_ms)?), Value::I64(gc)],
            vec![
                Col::Bytes(uniq.iter().map(|o| o.id.as_bytes().to_vec()).collect()),
                Col::I64(uniq.iter().map(|o| o.size as i64).collect()),
                Col::OptBytes(uniq.iter().map(|o| o.inline.clone()).collect()),
            ],
            true,
        )
        .await?;
        let inserted: BTreeSet<ObjectId> = rows
            .iter()
            .map(|r| parse_oid(&r.bytes(0)?))
            .collect::<Result<_, _>>()?;
        let existing: Vec<&NewObject> = uniq
            .iter()
            .copied()
            .filter(|o| !inserted.contains(&o.id))
            .collect();
        let mut writes = inserted.len() as u64;
        if !existing.is_empty() {
            // A quarantined row becomes live again with the verified bytes.
            let (_, revived) = Self::insert_rows(
                tx,
                "UPDATE objects AS t SET state = 0, inline_bytes = u.i, size = u.s, touched_gen = $2 \
                 FROM unnest($3::bytea[], $4::int8[], $5::bytea[]) AS u(o, s, i) \
                 WHERE t.org_key = $1 AND t.object = u.o AND t.state = 1",
                "UPDATE objects SET state = 0, inline_bytes = $5, size = $4, touched_gen = $2 \
                 WHERE org_key = $1 AND object = $3 AND state = 1",
                vec![Value::I64(org_key), Value::I64(gc)],
                vec![
                    Col::Bytes(existing.iter().map(|o| o.id.as_bytes().to_vec()).collect()),
                    Col::I64(existing.iter().map(|o| o.size as i64).collect()),
                    Col::OptBytes(existing.iter().map(|o| o.inline.clone()).collect()),
                ],
                false,
            )
            .await?;
            writes += revived;
            Self::exec_in(
                tx,
                "UPDATE objects SET touched_gen = $2 WHERE org_key = $1 AND object = ANY($3)",
                vec![Value::I64(org_key), Value::I64(gc)],
                Elems::ids(existing.iter().map(|o| &o.id)),
            )
            .await?;
        }
        t.write(Table::Objects, writes);
        if let Some(u) = upload {
            Self::insert_rows(
                tx,
                &upload_pg,
                &upload_lite,
                vec![
                    Value::I64(org_key),
                    Value::from(u.principal.as_str()),
                    upload_repo.map(Value::I64).unwrap_or(Value::Null),
                    Value::I64(int(at_ms)?),
                ],
                vec![Col::Bytes(
                    uniq.iter().map(|o| o.id.as_bytes().to_vec()).collect(),
                )],
                false,
            )
            .await?;
            t.write(Table::Uploads, objects.len() as u64);
        }
        let mut seen = BTreeSet::new();
        Ok(objects
            .iter()
            .map(|o| {
                if inserted.contains(&o.id) && seen.insert(o.id) {
                    PutOutcome::Inserted
                } else {
                    PutOutcome::AlreadyPresent
                }
            })
            .collect())
    }

    fn object_row(org: OrgId, r: &Row) -> Result<ObjectRow, BeeError> {
        Ok(ObjectRow {
            org,
            id: parse_oid(&r.bytes(0)?)?,
            size: r.u64(1)?,
            state: parse_state(r.i64(2)?)?,
            inline: r.opt_bytes(3)?,
            created_at_ms: r.u64(4)?,
        })
    }

    pub(crate) async fn get_objects(
        &self,
        org: OrgId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ObjectRow>>, BeeError> {
        let org_key = self.org_key_primary(org).await.map_err(fail_err)?;
        let mut pre = Tally::default();
        let mode = self.read_mode(&read, &mut pre).await?;
        self.flush(&pre, true);
        txn!(self, mode, |tx, t| async {
            let rows = Self::select_in(
                tx,
                "SELECT object, size, state, inline_bytes, created_at_ms FROM objects WHERE org_key = $1 AND object = ANY($2)",
                vec![Value::I64(org_key)],
                Elems::ids(ids),
            )
            .await?;
            let mut found = HashMap::new();
            for r in &rows {
                let row = Self::object_row(org, r)?;
                found.insert(row.id, row);
            }
            Ok(ids
                .iter()
                .map(|id| {
                    t.read(Table::Objects, 1);
                    let row = found.get(id).cloned();
                    if row.as_ref().is_some_and(|r| r.inline.is_some()) {
                        t.fetches.push(id.kind());
                    }
                    row
                })
                .collect())
        })
    }

    pub(crate) async fn set_object_state(
        &self,
        org: OrgId,
        id: &ObjectId,
        state: ObjectState,
    ) -> Result<(), BeeError> {
        txn!(self, Mode::Write, |tx, t| async {
            let org_key = self.org_key(tx, org).await?;
            let gc = Self::lock_org(tx, org_key, false).await?;
            let rows = tx
                .query(
                    "SELECT state FROM objects WHERE org_key = $1 AND object = $2",
                    &[Value::I64(org_key), oid(id)],
                )
                .await?;
            let cur = parse_state(rows.first().ok_or_else(|| not_found(id))?.i64(0)?)?;
            if state == ObjectState::Purged || cur == ObjectState::Purged {
                return Err(invalid("only redaction moves an object to or from purged").into());
            }
            tx.exec(
                "UPDATE objects SET state = $3, touched_gen = $4 WHERE org_key = $1 AND object = $2",
                &[
                    Value::I64(org_key),
                    oid(id),
                    Value::I64(state_code(state)),
                    Value::I64(gc),
                ],
            )
            .await?;
            t.write(Table::Objects, 1);
            Ok(())
        })
    }

    pub(crate) async fn list_objects(
        &self,
        org: OrgId,
        after: Option<ObjectId>,
        lim: usize,
    ) -> Result<Vec<ObjectRow>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let org_key = self.org_key(tx, org).await?;
            let rows = tx
                .query(
                    "SELECT object, size, state, inline_bytes, created_at_ms FROM objects \
                     WHERE org_key = $1 AND object > $2 ORDER BY object LIMIT $3",
                    &[
                        Value::I64(org_key),
                        Value::Bytes(after.map(|a| a.as_bytes().to_vec()).unwrap_or_default()),
                        Value::I64(limit(lim)),
                    ],
                )
                .await?;
            t.read(Table::Objects, rows.len() as u64);
            Ok(rows
                .iter()
                .map(|r| Self::object_row(org, r))
                .collect::<Result<Vec<_>, _>>()?)
        })
    }

    fn upload_row(org: OrgId, r: &Row) -> Result<UploadRow, BeeError> {
        Ok(UploadRow {
            org,
            object: parse_oid(&r.bytes(0)?)?,
            repo: r.opt_bytes(1)?.map(|b| parse_repo(&b)).transpose()?,
            principal: r.text(2)?,
            at_ms: r.u64(3)?,
        })
    }

    pub(crate) async fn get_uploads(
        &self,
        org: OrgId,
        id: &ObjectId,
    ) -> Result<Vec<UploadRow>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let org_key = self.org_key(tx, org).await?;
            let rows = tx
                .query(
                    "SELECT u.object, r.id, u.principal, u.at_ms FROM object_uploads u \
                     LEFT JOIN repos r ON r.repo_key = u.repo_key \
                     WHERE u.org_key = $1 AND u.object = $2 ORDER BY u.principal",
                    &[Value::I64(org_key), oid(id)],
                )
                .await?;
            t.read(Table::Uploads, rows.len() as u64);
            Ok(rows
                .iter()
                .map(|r| Self::upload_row(org, r))
                .collect::<Result<Vec<_>, _>>()?)
        })
    }

    pub(crate) async fn list_uploads(
        &self,
        org: OrgId,
        since_ms: u64,
    ) -> Result<Vec<UploadRow>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let org_key = self.org_key(tx, org).await?;
            let rows = tx
                .query(
                    "SELECT u.object, r.id, u.principal, u.at_ms FROM object_uploads u \
                     LEFT JOIN repos r ON r.repo_key = u.repo_key \
                     WHERE u.org_key = $1 AND u.at_ms >= $2 ORDER BY u.object, u.principal",
                    &[Value::I64(org_key), Value::I64(int(since_ms)?)],
                )
                .await?;
            t.read(Table::Uploads, rows.len() as u64);
            Ok(rows
                .iter()
                .map(|r| Self::upload_row(org, r))
                .collect::<Result<Vec<_>, _>>()?)
        })
    }

    // ----- changesets -------------------------------------------------------------------

    pub(crate) async fn insert_changeset(&self, new: NewChangeset) -> Result<PutOutcome, BeeError> {
        txn!(self, Mode::Write, |tx, t| self
            .insert_changeset_in(tx, t, &new))
    }

    /// Object rows and then changesets in one write transaction (see
    /// [`bee_store::MetaStore::put_batch`]).
    pub(crate) async fn put_batch(
        &self,
        objects: Vec<(OrgId, Vec<NewObject>, u64)>,
        changesets: Vec<NewChangeset>,
    ) -> Result<(), BeeError> {
        txn!(self, Mode::Write, |tx, t| async {
            for (org, rows, at_ms) in &objects {
                self.put_objects_in(tx, t, *org, rows, None, *at_ms).await?;
            }
            for c in &changesets {
                self.insert_changeset_in(tx, t, c).await?;
            }
            Ok(())
        })
    }

    /// A new changeset's objects (with their upload record) and its row in one write
    /// transaction (see [`bee_store::MetaStore::insert_changeset_with_objects`]).
    pub(crate) async fn insert_changeset_with_objects(
        &self,
        org: OrgId,
        objects: Vec<NewObject>,
        upload: Option<UploadRecord>,
        at_ms: u64,
        new: NewChangeset,
    ) -> Result<PutOutcome, BeeError> {
        txn!(self, Mode::Write, |tx, t| async {
            if !objects.is_empty() {
                self.put_objects_in(tx, t, org, &objects, upload.as_ref(), at_ms)
                    .await?;
            }
            self.insert_changeset_in(tx, t, &new).await
        })
    }

    /// The body of [`Core::insert_changeset`], inside the caller's write transaction.
    async fn insert_changeset_in(
        &self,
        tx: &mut B::Tx,
        t: &mut Tally,
        new: &NewChangeset,
    ) -> Res<PutOutcome> {
        let g = d::<B>().greatest();
        let touch_existing = format!(
            "UPDATE changesets SET last_used_ms = {g}(last_used_ms, $4), touched_gen = $5 \
             WHERE org_key = $1 AND repo_key = $2 AND id = $3"
        );
        let touch_parents = format!(
            "UPDATE changesets SET last_used_ms = {g}(last_used_ms, $3), touched_gen = $4 \
             WHERE org_key = $1 AND repo_key = $2 AND id = ANY($5)"
        );
        let cs = &new.changeset;
        let bytes = cs.encode();
        let id = hash_id(ObjectKind::Changeset, &bytes);
        let mut referenced: BTreeSet<ObjectId> = BTreeSet::new();
        referenced.insert(*cs.root());
        referenced.insert(*cs.meta());
        for e in cs.delta() {
            if let DeltaOp::Put { id, .. } = e.op {
                referenced.insert(id);
            }
        }
        for row in &new.object_paths {
            referenced.insert(row.object);
        }
        referenced.retain(|id| id.kind() != ObjectKind::Foreign);
        let k = self.repo_keys(tx, new.repo).await?;
        let gc = Self::lock_org(tx, k.org, false).await?;
        let at = Value::I64(int(new.at_ms)?);
        let rk = vec![Value::I64(k.org), Value::I64(k.repo)];
        let parents: BTreeSet<ObjectId> = Self::select_in(
            tx,
            "SELECT id FROM changesets WHERE org_key = $1 AND repo_key = $2 AND id = ANY($3)",
            rk.clone(),
            Elems::ids(cs.parents()),
        )
        .await?
        .iter()
        .map(|r| parse_oid(&r.bytes(0)?))
        .collect::<Result<_, _>>()?;
        for p in cs.parents() {
            t.read(Table::Changesets, 1);
            if !parents.contains(p) {
                return Err(missing(p).into());
            }
        }
        let states: HashMap<ObjectId, ObjectState> = Self::select_in(
            tx,
            "SELECT object, state FROM objects WHERE org_key = $1 AND object = ANY($2)",
            vec![Value::I64(k.org)],
            Elems::ids(&referenced),
        )
        .await?
        .iter()
        .map(|r| Ok((parse_oid(&r.bytes(0)?)?, parse_state(r.i64(1)?)?)))
        .collect::<Result<_, BeeError>>()?;
        for oid in &referenced {
            t.read(Table::Objects, 1);
            match states.get(oid) {
                Some(s) if *s != ObjectState::Quarantined => {}
                _ => return Err(missing(oid).into()),
            }
        }
        Self::exec_in(
            tx,
            "UPDATE objects SET touched_gen = $2 WHERE org_key = $1 AND object = ANY($3)",
            vec![Value::I64(k.org), Value::I64(gc)],
            Elems::ids(&referenced),
        )
        .await?;
        let n = tx
            .exec(
                "INSERT INTO changesets (org_key, repo_key, id, change_id, bytes, created_at_ms, last_used_ms, touched_gen) \
                 VALUES ($1, $2, $3, $4, $5, $6, $6, $7) ON CONFLICT DO NOTHING",
                &[
                    Value::I64(k.org),
                    Value::I64(k.repo),
                    oid(&id),
                    Value::Bytes(cs.change_id().as_bytes().to_vec()),
                    Value::Bytes(bytes.clone()),
                    at.clone(),
                    Value::I64(gc),
                ],
            )
            .await?;
        let outcome = if n == 1 {
            t.write(Table::Changesets, 1);
            PutOutcome::Inserted
        } else {
            tx.exec(
                &touch_existing,
                &[
                    Value::I64(k.org),
                    Value::I64(k.repo),
                    oid(&id),
                    at.clone(),
                    Value::I64(gc),
                ],
            )
            .await?;
            PutOutcome::AlreadyPresent
        };
        let (_, edges) = Self::insert_rows(
            tx,
            "INSERT INTO parent_edges (org_key, repo_key, child, idx, parent) \
             SELECT $1::bigint, $2::bigint, $3::bytea, u.i, u.p FROM unnest($4::int8[], $5::bytea[]) AS u(i, p) \
             ON CONFLICT DO NOTHING",
            "INSERT INTO parent_edges (org_key, repo_key, child, idx, parent) VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT DO NOTHING",
            vec![Value::I64(k.org), Value::I64(k.repo), oid(&id)],
            vec![
                Col::I64((0..cs.parents().len() as i64).collect()),
                Col::Bytes(cs.parents().iter().map(|p| p.as_bytes().to_vec()).collect()),
            ],
            false,
        )
        .await?;
        t.write(Table::ParentEdges, edges);
        let mut tp = rk.clone();
        tp.push(at.clone());
        tp.push(Value::I64(gc));
        Self::exec_in(tx, &touch_parents, tp, Elems::ids(cs.parents())).await?;
        let (_, paths) = Self::insert_rows(
            tx,
            "INSERT INTO object_paths (org_key, repo_key, object, path) \
             SELECT $1::bigint, $2::bigint, u.o, u.p FROM unnest($3::bytea[], $4::bytea[]) AS u(o, p) \
             ON CONFLICT DO NOTHING",
            "INSERT INTO object_paths (org_key, repo_key, object, path) VALUES ($1, $2, $3, $4) \
             ON CONFLICT DO NOTHING",
            rk.clone(),
            vec![
                Col::Bytes(new.object_paths.iter().map(|r| r.object.as_bytes().to_vec()).collect()),
                Col::Bytes(new.object_paths.iter().map(|r| path_bytes(&r.path)).collect()),
            ],
            false,
        )
        .await?;
        t.write(Table::ObjectPaths, paths);
        let derived = Self::insert_derived(tx, k, &new.derived).await?;
        t.write(Table::Derived, derived);
        Ok(outcome)
    }

    async fn insert_derived(tx: &mut B::Tx, k: RepoKeys, rows: &[DerivedRow]) -> Res<u64> {
        let (_, n) = Self::insert_rows(
            tx,
            "INSERT INTO derived (org_key, repo_key, dkey, type_name, version, value) \
             SELECT $1::bigint, $2::bigint, u.k, u.t, u.v, u.x \
             FROM unnest($3::bytea[], $4::text[], $5::int8[], $6::bytea[]) AS u(k, t, v, x) \
             ON CONFLICT DO NOTHING",
            "INSERT INTO derived (org_key, repo_key, dkey, type_name, version, value) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
            vec![Value::I64(k.org), Value::I64(k.repo)],
            vec![
                Col::Bytes(rows.iter().map(|r| r.key.as_bytes().to_vec()).collect()),
                Col::Text(rows.iter().map(|r| r.type_name.clone()).collect()),
                Col::I64(rows.iter().map(|r| i64::from(r.version)).collect()),
                Col::Bytes(rows.iter().map(|r| r.value.clone()).collect()),
            ],
            false,
        )
        .await?;
        Ok(n)
    }

    fn changeset_row(repo: RepoId, r: &Row) -> Result<ChangesetRow, BeeError> {
        let id = parse_oid(&r.bytes(0)?)?;
        let bytes = r.bytes(1)?;
        let changeset =
            Changeset::decode(&bytes).map_err(|e| corrupt(format!("changeset {id}: {e}")))?;
        Ok(ChangesetRow {
            repo,
            id,
            bytes,
            changeset,
            created_at_ms: r.u64(2)?,
            last_used_ms: r.u64(3)?,
        })
    }

    pub(crate) async fn get_changesets(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
        read: ReadFrom,
    ) -> Result<Vec<Option<ChangesetRow>>, BeeError> {
        let k = self.repo_keys_primary(repo).await.map_err(fail_err)?;
        let mut pre = Tally::default();
        let mode = self.read_mode(&read, &mut pre).await?;
        self.flush(&pre, true);
        txn!(self, mode, |tx, t| async {
            let rows = Self::select_in(
                tx,
                "SELECT id, bytes, created_at_ms, last_used_ms FROM changesets \
                 WHERE org_key = $1 AND repo_key = $2 AND id = ANY($3)",
                vec![Value::I64(k.org), Value::I64(k.repo)],
                Elems::ids(ids),
            )
            .await?;
            let mut found = HashMap::new();
            for r in &rows {
                let row = Self::changeset_row(repo, r)?;
                t.changeset_bytes += row.bytes.len() as u64;
                found.insert(row.id, row);
            }
            t.read(Table::Changesets, ids.len() as u64);
            Ok(ids.iter().map(|id| found.get(id).cloned()).collect())
        })
    }

    pub(crate) async fn list_changesets(
        &self,
        repo: RepoId,
        after: Option<ObjectId>,
        lim: usize,
    ) -> Result<Vec<ChangesetRow>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            let rows = tx
                .query(
                    "SELECT id, bytes, created_at_ms, last_used_ms FROM changesets \
                     WHERE org_key = $1 AND repo_key = $2 AND id > $3 ORDER BY id LIMIT $4",
                    &[
                        Value::I64(k.org),
                        Value::I64(k.repo),
                        Value::Bytes(after.map(|a| a.as_bytes().to_vec()).unwrap_or_default()),
                        Value::I64(limit(lim)),
                    ],
                )
                .await?;
            t.read(Table::Changesets, rows.len() as u64);
            let rows = rows
                .iter()
                .map(|r| Self::changeset_row(repo, r))
                .collect::<Result<Vec<_>, _>>()?;
            t.changeset_bytes += rows.iter().map(|r| r.bytes.len() as u64).sum::<u64>();
            Ok(rows)
        })
    }

    /// The root of a changeset from the first [`ROOT_PREFIX`] bytes of its row (see
    /// [`bee_store::MetaStore::get_changeset_root`]); the whole row only when the
    /// prefix does not reach the root (more parents than the prefix holds).
    pub(crate) async fn get_changeset_root(
        &self,
        repo: RepoId,
        id: &ObjectId,
        read: ReadFrom,
    ) -> Result<Option<ObjectId>, BeeError> {
        let k = self.repo_keys_primary(repo).await.map_err(fail_err)?;
        let mut pre = Tally::default();
        let mode = self.read_mode(&read, &mut pre).await?;
        self.flush(&pre, true);
        // `substr` and `length` on a byte string mean the same on SQLite (BLOB) and
        // Postgres (bytea); only the prefix crosses to this process and is decoded.
        let sql = format!(
            "SELECT substr(bytes, 1, {ROOT_PREFIX}), length(bytes) FROM changesets \
             WHERE org_key = $1 AND repo_key = $2 AND id = $3"
        );
        let found = txn!(self, mode, |tx, t| async {
            let rows = tx
                .query(
                    &sql,
                    &[
                        Value::I64(k.org),
                        Value::I64(k.repo),
                        Value::Bytes(id.as_bytes().to_vec()),
                    ],
                )
                .await?;
            t.read(Table::Changesets, 1);
            let Some(r) = rows.first() else {
                return Ok(None);
            };
            let prefix = r.bytes(0)?;
            t.changeset_bytes += prefix.len() as u64;
            let total = usize::try_from(r.u64(1)?).map_err(|_| corrupt("changeset length"))?;
            Ok(Some(
                Changeset::root_from_prefix(&prefix, total)
                    .map_err(|e| corrupt(format!("changeset {id}: {e}")))?,
            ))
        })?;
        match found {
            None => Ok(None),
            Some(Some(root)) => Ok(Some(root)),
            Some(None) => Ok(self
                .get_changesets(repo, std::slice::from_ref(id), read)
                .await?
                .pop()
                .flatten()
                .map(|row| *row.changeset.root())),
        }
    }

    /// One-column id list query on a repo.
    async fn repo_ids(
        &self,
        repo: RepoId,
        table: Table,
        sql: &'static str,
        arg: Value,
    ) -> Result<Vec<ObjectId>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            let rows = tx
                .query(sql, &[Value::I64(k.org), Value::I64(k.repo), arg.clone()])
                .await?;
            t.read(table, rows.len() as u64);
            Ok(rows
                .iter()
                .map(|r| parse_oid(&r.bytes(0)?))
                .collect::<Result<Vec<_>, _>>()?)
        })
    }

    pub(crate) async fn changesets_by_change_id(
        &self,
        repo: RepoId,
        change_id: &ChangeId,
    ) -> Result<Vec<ObjectId>, BeeError> {
        self.repo_ids(
            repo,
            Table::Changesets,
            "SELECT id FROM changesets WHERE org_key = $1 AND repo_key = $2 AND change_id = $3 ORDER BY id",
            Value::Bytes(change_id.as_bytes().to_vec()),
        )
        .await
    }

    pub(crate) async fn parents(
        &self,
        repo: RepoId,
        id: &ObjectId,
    ) -> Result<Vec<ObjectId>, BeeError> {
        self.repo_ids(
            repo,
            Table::ParentEdges,
            "SELECT parent FROM parent_edges WHERE org_key = $1 AND repo_key = $2 AND child = $3 ORDER BY idx",
            oid(id),
        )
        .await
    }

    pub(crate) async fn children(
        &self,
        repo: RepoId,
        id: &ObjectId,
    ) -> Result<Vec<ObjectId>, BeeError> {
        self.repo_ids(
            repo,
            Table::ParentEdges,
            "SELECT DISTINCT child FROM parent_edges WHERE org_key = $1 AND repo_key = $2 AND parent = $3 ORDER BY child",
            oid(id),
        )
        .await
    }

    pub(crate) async fn changesets_with_prefix(
        &self,
        repo: RepoId,
        prefix: &ObjectIdPrefix,
        lim: usize,
    ) -> Result<Vec<ObjectId>, BeeError> {
        let (lo, hi) = prefix_range(prefix);
        txn!(self, Mode::Read, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            let mut params = vec![
                Value::I64(k.org),
                Value::I64(k.repo),
                Value::Bytes(lo.clone()),
                Value::I64(limit(lim)),
            ];
            let sql = match &hi {
                Some(h) => {
                    params.push(Value::Bytes(h.clone()));
                    "SELECT id FROM changesets WHERE org_key = $1 AND repo_key = $2 AND id >= $3 AND id < $5 ORDER BY id LIMIT $4"
                }
                None => {
                    "SELECT id FROM changesets WHERE org_key = $1 AND repo_key = $2 AND id >= $3 ORDER BY id LIMIT $4"
                }
            };
            let rows = tx.query(sql, &params).await?;
            let out: Vec<ObjectId> = rows
                .iter()
                .map(|r| parse_oid(&r.bytes(0)?))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|id| prefix.matches(id))
                .collect();
            t.read(Table::Changesets, out.len() as u64);
            Ok(out)
        })
    }

    pub(crate) async fn touch_changesets(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
        at_ms: u64,
    ) -> Result<(), BeeError> {
        let g = d::<B>().greatest();
        let sql = format!(
            "UPDATE changesets SET last_used_ms = {g}(last_used_ms, $3), touched_gen = $4 \
             WHERE org_key = $1 AND repo_key = $2 AND id = ANY($5)"
        );
        txn!(self, Mode::Write, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            let gc = Self::lock_org(tx, k.org, false).await?;
            let n = Self::exec_in(
                tx,
                &sql,
                vec![
                    Value::I64(k.org),
                    Value::I64(k.repo),
                    Value::I64(int(at_ms)?),
                    Value::I64(gc),
                ],
                Elems::ids(ids),
            )
            .await?;
            t.write(Table::Changesets, n);
            Ok(())
        })
    }

    pub(crate) async fn object_paths(
        &self,
        repo: RepoId,
        object: &ObjectId,
        read: ReadFrom,
    ) -> Result<Vec<Option<RepoPath>>, BeeError> {
        let k = self.repo_keys_primary(repo).await.map_err(fail_err)?;
        let mut pre = Tally::default();
        let mode = self.read_mode(&read, &mut pre).await?;
        self.flush(&pre, true);
        txn!(self, mode, |tx, t| async {
            let rows = tx
                .query(
                    "SELECT path FROM object_paths WHERE org_key = $1 AND repo_key = $2 AND object = $3 ORDER BY path",
                    &[Value::I64(k.org), Value::I64(k.repo), oid(object)],
                )
                .await?;
            t.read(Table::ObjectPaths, rows.len() as u64);
            Ok(rows
                .iter()
                .map(|r| parse_path(r.bytes(0)?))
                .collect::<Result<Vec<_>, _>>()?)
        })
    }

    pub(crate) async fn put_derived(
        &self,
        repo: RepoId,
        rows: Vec<DerivedRow>,
    ) -> Result<(), BeeError> {
        txn!(self, Mode::Write, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            Self::lock_org(tx, k.org, false).await?;
            let n = Self::insert_derived(tx, k, &rows).await?;
            t.write(Table::Derived, n);
            Ok(())
        })
    }

    pub(crate) async fn get_derived(
        &self,
        repo: RepoId,
        key: &ObjectId,
        type_name: &str,
        version: u32,
        read: ReadFrom,
    ) -> Result<Option<Vec<u8>>, BeeError> {
        let k = self.repo_keys_primary(repo).await.map_err(fail_err)?;
        let mut pre = Tally::default();
        let mode = self.read_mode(&read, &mut pre).await?;
        self.flush(&pre, true);
        txn!(self, mode, |tx, t| async {
            t.read(Table::Derived, 1);
            let rows = tx
                .query(
                    "SELECT value FROM derived WHERE org_key = $1 AND repo_key = $2 AND dkey = $3 \
                     AND type_name = $4 AND version = $5",
                    &[
                        Value::I64(k.org),
                        Value::I64(k.repo),
                        oid(key),
                        Value::from(type_name),
                        Value::I64(version.into()),
                    ],
                )
                .await?;
            Ok(rows.first().map(|r| r.bytes(0)).transpose()?)
        })
    }

    pub(crate) async fn list_derived_by_type(
        &self,
        repo: RepoId,
        type_name: &str,
        after: Option<ObjectId>,
        lim: usize,
    ) -> Result<Vec<(ObjectId, u32, Vec<u8>)>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            let rows = tx
                .query(
                    "SELECT dkey, version, value FROM derived WHERE org_key = $1 AND repo_key = $2 \
                     AND type_name = $3 AND dkey > $4 ORDER BY dkey, version LIMIT $5",
                    &[
                        Value::I64(k.org),
                        Value::I64(k.repo),
                        Value::from(type_name),
                        Value::Bytes(after.map(|a| a.as_bytes().to_vec()).unwrap_or_default()),
                        Value::I64(limit(lim)),
                    ],
                )
                .await?;
            t.read(Table::Derived, rows.len() as u64);
            Ok(rows
                .iter()
                .map(|r| Ok((parse_oid(&r.bytes(0)?)?, r.u64(1)? as u32, r.bytes(2)?)))
                .collect::<Result<Vec<_>, BeeError>>()?)
        })
    }

    pub(crate) async fn delete_derived_type(
        &self,
        repo: RepoId,
        type_name: &str,
    ) -> Result<u64, BeeError> {
        txn!(self, Mode::Write, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            Self::lock_org(tx, k.org, false).await?;
            let n = tx
                .exec(
                    "DELETE FROM derived WHERE org_key = $1 AND repo_key = $2 AND type_name = $3",
                    &[
                        Value::I64(k.org),
                        Value::I64(k.repo),
                        Value::from(type_name),
                    ],
                )
                .await?;
            t.write(Table::Derived, n);
            Ok(n)
        })
    }

    // ----- refs -------------------------------------------------------------------------

    pub(crate) async fn get_ref(
        &self,
        repo: RepoId,
        name: &RefName,
    ) -> Result<Option<RefRow>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            t.read(Table::Refs, 1);
            let rows = tx
                .query(
                    "SELECT target, version FROM refs WHERE org_key = $1 AND repo_key = $2 AND name = $3",
                    &[Value::I64(k.org), Value::I64(k.repo), Value::from(name.as_str())],
                )
                .await?;
            Ok(rows
                .first()
                .map(|r| -> Result<RefRow, BeeError> {
                    Ok(RefRow {
                        repo,
                        name: name.clone(),
                        target: parse_opt_oid(r.opt_bytes(0)?)?,
                        version: r.u64(1)?,
                    })
                })
                .transpose()?)
        })
    }

    pub(crate) async fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
    ) -> Result<Vec<RefRow>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            let rows = tx
                .query(
                    "SELECT name, target, version FROM refs WHERE org_key = $1 AND repo_key = $2 \
                     AND name >= $3 AND substr(name, 1, length($3)) = $3 ORDER BY name",
                    &[Value::I64(k.org), Value::I64(k.repo), Value::from(prefix)],
                )
                .await?;
            t.read(Table::Refs, rows.len() as u64);
            Ok(rows
                .iter()
                .map(|r| {
                    Ok(RefRow {
                        repo,
                        name: parse_ref(&r.text(0)?)?,
                        target: parse_opt_oid(r.opt_bytes(1)?)?,
                        version: r.u64(2)?,
                    })
                })
                .collect::<Result<Vec<_>, BeeError>>()?)
        })
    }

    pub(crate) async fn update_ref(&self, update: RefUpdate) -> Result<RefUpdated, BeeError> {
        let g = d::<B>().greatest();
        let touch = format!(
            "UPDATE changesets SET last_used_ms = {g}(last_used_ms, $3), touched_gen = $4 \
             WHERE org_key = $1 AND repo_key = $2 AND id = ANY($5)"
        );
        let lock_ref = format!(
            "SELECT target, version FROM refs WHERE org_key = $1 AND repo_key = $2 AND name = $3 {}",
            d::<B>().lock("FOR UPDATE")
        );
        let _turn = self
            .ref_turn(update.repo, update.name.as_str())
            .lock()
            .await;
        txn!(self, Mode::Serializable, |tx, t| async {
            let k = self.repo_keys(tx, update.repo).await?;
            if update.steps.is_empty() {
                return Err(invalid("a ref update needs a step").into());
            }
            let gc = Self::lock_org(tx, k.org, false).await?;
            let name = Value::from(update.name.as_str());
            let rk = vec![Value::I64(k.org), Value::I64(k.repo)];
            let mut p = rk.clone();
            p.push(name.clone());
            t.read(Table::Refs, 1);
            let cur = tx.query(&lock_ref, &p).await?;
            let (exists, cur_target, cur_version) = match cur.first() {
                Some(r) => (true, parse_opt_oid(r.opt_bytes(0)?)?, r.u64(1)?),
                None => (false, None, 0),
            };
            let ok = match update.expected {
                Expected::Absent => cur_target.is_none(),
                Expected::Version(v) => exists && cur_version == v,
            };
            if !ok {
                return Err(BeeError::new(
                    Reason::VersionMismatch,
                    format!(
                        "{} is at version {cur_version}, expected {:?}",
                        update.name, update.expected
                    ),
                )
                .with_payload("current_version", cur_version as i64)
                .into());
            }
            let mut wanted: BTreeSet<ObjectId> = BTreeSet::new();
            let mut cids: BTreeSet<ChangeId> = BTreeSet::new();
            for s in &update.steps {
                wanted.extend(s.target);
                for l in &s.landed {
                    wanted.insert(l.source);
                    wanted.insert(l.result);
                    cids.insert(l.change_id);
                }
            }
            let present: BTreeSet<ObjectId> = Self::select_in(
                tx,
                "SELECT id FROM changesets WHERE org_key = $1 AND repo_key = $2 AND id = ANY($3)",
                rk.clone(),
                Elems::ids(&wanted),
            )
            .await?
            .iter()
            .map(|r| parse_oid(&r.bytes(0)?))
            .collect::<Result<_, _>>()?;
            let landed_now: HashMap<ChangeId, ObjectId> = Self::select_in(
                tx,
                "SELECT change_id, source FROM landed WHERE org_key = $1 AND repo_key = $2 \
                 AND ref_name = $3 AND change_id = ANY($4)",
                p.clone(),
                Elems::Bytes(cids.iter().map(|c| c.as_bytes().to_vec()).collect()),
            )
            .await?
            .iter()
            .map(|r| Ok((parse_change_id(&r.bytes(0)?)?, parse_oid(&r.bytes(1)?)?)))
            .collect::<Result<_, BeeError>>()?;
            let mut seen: HashMap<ChangeId, ObjectId> = HashMap::new();
            for step in &update.steps {
                if let Some(target) = &step.target {
                    t.read(Table::Changesets, 1);
                    if !present.contains(target) {
                        return Err(missing(target).into());
                    }
                } else if !step.landed.is_empty() {
                    return Err(invalid("a ref deletion cannot land changes").into());
                }
                for l in &step.landed {
                    for id in [&l.source, &l.result] {
                        t.read(Table::Changesets, 1);
                        if !present.contains(id) {
                            return Err(missing(id).into());
                        }
                    }
                    t.read(Table::Landed, 1);
                    let existing = landed_now
                        .get(&l.change_id)
                        .or_else(|| seen.get(&l.change_id))
                        .copied();
                    if let Some(src) = existing {
                        return Err(BeeError::new(
                            Reason::AlreadyLanded,
                            format!("{} already landed on {}", l.change_id, update.name),
                        )
                        .with_payload("change_id", l.change_id.to_text())
                        .with_payload("existing_source", src.to_text())
                        .into());
                    }
                    seen.insert(l.change_id, l.source);
                }
            }

            // Writes: K versions, K ref-log rows, the landed rows, the ref, the events.
            let at = Value::I64(int(update.at_ms)?);
            let mut versions = Vec::new();
            let mut olds = Vec::new();
            let mut news = Vec::new();
            let mut blobs = Vec::new();
            let (mut l_cid, mut l_src, mut l_res, mut l_ver) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let mut version = int(cur_version)?;
            let mut old = cur_target;
            for step in &update.steps {
                version += 1;
                versions.push(version);
                olds.push(old.map(|o| o.as_bytes().to_vec()));
                news.push(step.target.map(|o| o.as_bytes().to_vec()));
                blobs.push(encode_landed(&step.landed));
                for l in &step.landed {
                    l_cid.push(l.change_id.as_bytes().to_vec());
                    l_src.push(l.source.as_bytes().to_vec());
                    l_res.push(l.result.as_bytes().to_vec());
                    l_ver.push(version);
                }
                old = step.target;
            }
            let k_steps = versions.len() as u64;
            Self::insert_rows(
                tx,
                "INSERT INTO ref_log (org_key, repo_key, name, version, old_target, new_target, principal, at_ms, landed) \
                 SELECT $1::bigint, $2::bigint, $3::text, u.v, u.o, u.n, $4::text, $5::bigint, u.l \
                 FROM unnest($6::int8[], $7::bytea[], $8::bytea[], $9::bytea[]) AS u(v, o, n, l)",
                "INSERT INTO ref_log (org_key, repo_key, name, version, old_target, new_target, principal, at_ms, landed) \
                 VALUES ($1, $2, $3, $6, $7, $8, $4, $5, $9)",
                vec![
                    Value::I64(k.org),
                    Value::I64(k.repo),
                    name.clone(),
                    Value::from(update.principal.as_str()),
                    at.clone(),
                ],
                vec![
                    Col::I64(versions),
                    Col::OptBytes(olds),
                    Col::OptBytes(news),
                    Col::Bytes(blobs),
                ],
                false,
            )
            .await?;
            t.write(Table::RefLog, k_steps);
            let n_landed = l_cid.len() as u64;
            Self::insert_rows(
                tx,
                "INSERT INTO landed (org_key, repo_key, ref_name, change_id, source, result, version) \
                 SELECT $1::bigint, $2::bigint, $3::text, u.c, u.s, u.r, u.v \
                 FROM unnest($4::bytea[], $5::bytea[], $6::bytea[], $7::int8[]) AS u(c, s, r, v)",
                "INSERT INTO landed (org_key, repo_key, ref_name, change_id, source, result, version) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                p.clone(),
                vec![
                    Col::Bytes(l_cid),
                    Col::Bytes(l_src),
                    Col::Bytes(l_res),
                    Col::I64(l_ver),
                ],
                false,
            )
            .await?;
            t.write(Table::Landed, n_landed);
            let targets: BTreeSet<ObjectId> =
                update.steps.iter().filter_map(|s| s.target).collect();
            let mut tp = rk.clone();
            tp.push(at.clone());
            tp.push(Value::I64(gc));
            Self::exec_in(tx, &touch, tp, Elems::ids(&targets)).await?;
            let mut rp = p.clone();
            rp.push(old.map(|o| oid(&o)).unwrap_or(Value::Null));
            rp.push(Value::I64(version));
            if exists {
                tx.exec(
                    "UPDATE refs SET target = $4, version = $5 WHERE org_key = $1 AND repo_key = $2 AND name = $3",
                    &rp,
                )
                .await?;
            } else {
                tx.exec(
                    "INSERT INTO refs (org_key, repo_key, name, target, version) VALUES ($1, $2, $3, $4, $5)",
                    &rp,
                )
                .await?;
            }
            t.write(Table::Refs, 1);
            let event_seqs = if update.events.is_empty() {
                Vec::new()
            } else {
                Self::append(tx, t, k, &update.events).await?
            };
            Ok(RefUpdated {
                old_version: cur_version,
                new_version: version as u64,
                event_seqs,
            })
        })
    }

    pub(crate) async fn ref_log(
        &self,
        repo: RepoId,
        name: &RefName,
        from_version: u64,
        lim: usize,
    ) -> Result<Vec<RefLogEntry>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            let rows = tx
                .query(
                    "SELECT version, old_target, new_target, principal, at_ms, landed FROM ref_log \
                     WHERE org_key = $1 AND repo_key = $2 AND name = $3 AND version >= $4 \
                     ORDER BY version LIMIT $5",
                    &[
                        Value::I64(k.org),
                        Value::I64(k.repo),
                        Value::from(name.as_str()),
                        Value::I64(i64::try_from(from_version).unwrap_or(i64::MAX)),
                        Value::I64(limit(lim)),
                    ],
                )
                .await?;
            t.read(Table::RefLog, rows.len() as u64);
            Ok(rows
                .iter()
                .map(|r| {
                    Ok(RefLogEntry {
                        repo,
                        name: name.clone(),
                        version: r.u64(0)?,
                        old: parse_opt_oid(r.opt_bytes(1)?)?,
                        new: parse_opt_oid(r.opt_bytes(2)?)?,
                        principal: r.text(3)?,
                        at_ms: r.u64(4)?,
                        landed: decode_landed(&r.bytes(5)?)?,
                    })
                })
                .collect::<Result<Vec<_>, BeeError>>()?)
        })
    }

    fn landed_row(
        repo: RepoId,
        name: RefName,
        r: &Row,
        base: usize,
    ) -> Result<LandedRow, BeeError> {
        Ok(LandedRow {
            repo,
            ref_name: name,
            change_id: parse_change_id(&r.bytes(base)?)?,
            source: parse_oid(&r.bytes(base + 1)?)?,
            result: parse_oid(&r.bytes(base + 2)?)?,
            version: r.u64(base + 3)?,
        })
    }

    pub(crate) async fn get_landed(
        &self,
        repo: RepoId,
        name: &RefName,
        change_ids: &[ChangeId],
    ) -> Result<Vec<Option<LandedRow>>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            t.read(Table::Landed, change_ids.len() as u64);
            let rows = Self::select_in(
                tx,
                "SELECT change_id, source, result, version FROM landed WHERE org_key = $1 \
                 AND repo_key = $2 AND ref_name = $3 AND change_id = ANY($4)",
                vec![
                    Value::I64(k.org),
                    Value::I64(k.repo),
                    Value::from(name.as_str()),
                ],
                Elems::Bytes(change_ids.iter().map(|c| c.as_bytes().to_vec()).collect()),
            )
            .await?;
            let mut found = HashMap::new();
            for r in &rows {
                let row = Self::landed_row(repo, name.clone(), r, 0)?;
                found.insert(row.change_id, row);
            }
            Ok(change_ids.iter().map(|c| found.get(c).cloned()).collect())
        })
    }

    pub(crate) async fn list_landed(
        &self,
        repo: RepoId,
        name: Option<&RefName>,
    ) -> Result<Vec<LandedRow>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            let mut params = vec![Value::I64(k.org), Value::I64(k.repo)];
            let sql = match name {
                Some(n) => {
                    params.push(Value::from(n.as_str()));
                    "SELECT ref_name, change_id, source, result, version FROM landed \
                     WHERE org_key = $1 AND repo_key = $2 AND ref_name = $3 ORDER BY ref_name, change_id"
                }
                None => {
                    "SELECT ref_name, change_id, source, result, version FROM landed \
                     WHERE org_key = $1 AND repo_key = $2 ORDER BY ref_name, change_id"
                }
            };
            let rows = tx.query(sql, &params).await?;
            t.read(Table::Landed, rows.len() as u64);
            Ok(rows
                .iter()
                .map(|r| Self::landed_row(repo, parse_ref(&r.text(0)?)?, r, 1))
                .collect::<Result<Vec<_>, _>>()?)
        })
    }

    // ----- events -----------------------------------------------------------------------

    /// Allocates a block of `n` numbers with one counter update; returns the last one.
    async fn allocate(tx: &mut B::Tx, t: &mut Tally, k: RepoKeys, n: u64) -> Res<u64> {
        let rows = tx
            .query(
                "UPDATE event_counters SET allocated = allocated + $3 \
                 WHERE org_key = $1 AND repo_key = $2 RETURNING allocated",
                &[Value::I64(k.org), Value::I64(k.repo), Value::I64(int(n)?)],
            )
            .await?;
        t.write(Table::EventCounters, 1);
        Ok(rows
            .first()
            .ok_or_else(|| Fail::internal("event counter row missing"))?
            .u64(0)?)
    }

    async fn insert_event_rows(
        tx: &mut B::Tx,
        k: RepoKeys,
        events: &[(u64, &NewEvent)],
    ) -> Res<u64> {
        let (_, n) = Self::insert_rows(
            tx,
            "INSERT INTO events (org_key, repo_key, seq, kind, payload, at_ms) \
             SELECT $1::bigint, $2::bigint, u.s, u.k, u.p, u.a \
             FROM unnest($3::int8[], $4::text[], $5::bytea[], $6::int8[]) AS u(s, k, p, a) \
             ON CONFLICT DO NOTHING",
            "INSERT INTO events (org_key, repo_key, seq, kind, payload, at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
            vec![Value::I64(k.org), Value::I64(k.repo)],
            vec![
                Col::I64(
                    events
                        .iter()
                        .map(|(s, _)| int(*s))
                        .collect::<Result<_, _>>()?,
                ),
                Col::Text(events.iter().map(|(_, e)| e.kind.clone()).collect()),
                Col::Bytes(events.iter().map(|(_, e)| e.payload.clone()).collect()),
                Col::I64(
                    events
                        .iter()
                        .map(|(_, e)| int(e.at_ms))
                        .collect::<Result<_, _>>()?,
                ),
            ],
            false,
        )
        .await?;
        Ok(n)
    }

    async fn append(
        tx: &mut B::Tx,
        t: &mut Tally,
        k: RepoKeys,
        events: &[NewEvent],
    ) -> Res<Vec<u64>> {
        let n = events.len() as u64;
        let end = Self::allocate(tx, t, k, n).await?;
        let start = end + 1 - n;
        let rows: Vec<(u64, &NewEvent)> = events
            .iter()
            .enumerate()
            .map(|(i, e)| (start + i as u64, e))
            .collect();
        let written = Self::insert_event_rows(tx, k, &rows).await?;
        t.write(Table::Events, written);
        Ok(rows.iter().map(|(s, _)| *s).collect())
    }

    pub(crate) async fn allocate_event_seqs(
        &self,
        repo: RepoId,
        n: u64,
    ) -> Result<Range<u64>, BeeError> {
        txn!(self, Mode::Write, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            let end = Self::allocate(tx, t, k, n).await?;
            Ok(end + 1 - n..end + 1)
        })
    }

    pub(crate) async fn insert_events(
        &self,
        repo: RepoId,
        events: Vec<(u64, NewEvent)>,
    ) -> Result<(), BeeError> {
        txn!(self, Mode::Write, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            let rows = tx
                .query(
                    "SELECT allocated FROM event_counters WHERE org_key = $1 AND repo_key = $2",
                    &[Value::I64(k.org), Value::I64(k.repo)],
                )
                .await?;
            let allocated = rows
                .first()
                .ok_or_else(|| Fail::internal("event counter row missing"))?
                .u64(0)?;
            for (s, _) in &events {
                if *s == 0 || *s > allocated {
                    return Err(
                        invalid(format!("event sequence number {s} was never allocated")).into(),
                    );
                }
            }
            let refs: Vec<(u64, &NewEvent)> = events.iter().map(|(s, e)| (*s, e)).collect();
            let n = Self::insert_event_rows(tx, k, &refs).await?;
            t.write(Table::Events, n);
            Ok(())
        })
    }

    pub(crate) async fn append_events(
        &self,
        repo: RepoId,
        events: Vec<NewEvent>,
    ) -> Result<Vec<u64>, BeeError> {
        txn!(self, Mode::Write, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            Self::append(tx, t, k, &events).await
        })
    }

    pub(crate) async fn read_events(
        &self,
        repo: RepoId,
        from_seq: u64,
        lim: usize,
        read: ReadFrom,
    ) -> Result<Vec<EventRow>, BeeError> {
        let k = self.repo_keys_primary(repo).await.map_err(fail_err)?;
        let mut pre = Tally::default();
        let mode = self.read_mode(&read, &mut pre).await?;
        self.flush(&pre, true);
        txn!(self, mode, |tx, t| async {
            let rows = tx
                .query(
                    "SELECT seq, kind, payload, at_ms FROM events WHERE org_key = $1 AND repo_key = $2 \
                     AND seq >= $3 ORDER BY seq LIMIT $4",
                    &[
                        Value::I64(k.org),
                        Value::I64(k.repo),
                        Value::I64(i64::try_from(from_seq).unwrap_or(i64::MAX)),
                        Value::I64(limit(lim)),
                    ],
                )
                .await?;
            t.read(Table::Events, rows.len() as u64);
            Ok(rows
                .iter()
                .map(|r| {
                    Ok(EventRow {
                        repo,
                        seq: r.u64(0)?,
                        kind: r.text(1)?,
                        payload: r.bytes(2)?,
                        at_ms: r.u64(3)?,
                    })
                })
                .collect::<Result<Vec<_>, BeeError>>()?)
        })
    }

    pub(crate) async fn event_head(&self, repo: RepoId) -> Result<EventHead, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let k = self.repo_keys(tx, repo).await?;
            t.read(Table::EventCounters, 1);
            let rows = tx
                .query(
                    "SELECT c.allocated, COALESCE((SELECT MAX(e.seq) FROM events e \
                     WHERE e.org_key = c.org_key AND e.repo_key = c.repo_key), 0) \
                     FROM event_counters c WHERE c.org_key = $1 AND c.repo_key = $2",
                    &[Value::I64(k.org), Value::I64(k.repo)],
                )
                .await?;
            let r = rows
                .first()
                .ok_or_else(|| Fail::internal("event counter row missing"))?;
            Ok(EventHead {
                allocated: r.u64(0)?,
                written: r.u64(1)?,
            })
        })
    }

    // ----- redaction --------------------------------------------------------------------

    pub(crate) async fn redact(&self, row: RedactionRow) -> Result<Redacted, BeeError> {
        if !matches!(row.object.kind(), ObjectKind::Blob | ObjectKind::Meta) {
            return Err(BeeError::invalid(
                Subcode::WrongKind,
                "only blobs and metas can be redacted",
            ));
        }
        txn!(self, Mode::Write, |tx, t| async {
            let org_key = self.org_key(tx, row.org).await?;
            let gc = Self::lock_org(tx, org_key, true).await?;
            t.read(Table::Redactions, 1);
            let already = !tx
                .query(
                    "SELECT 1 FROM redactions WHERE org_key = $1 AND object = $2",
                    &[Value::I64(org_key), oid(&row.object)],
                )
                .await?
                .is_empty();
            let mut repos: Vec<(RepoId, i64)> = tx
                .query(
                    "SELECT DISTINCT r.id, p.repo_key FROM object_paths p \
                     JOIN repos r ON r.repo_key = p.repo_key \
                     WHERE p.org_key = $1 AND p.object = $2",
                    &[Value::I64(org_key), oid(&row.object)],
                )
                .await?
                .iter()
                .map(|r| Ok((parse_repo(&r.bytes(0)?)?, r.i64(1)?)))
                .collect::<Result<_, BeeError>>()?;
            repos.sort();
            let keys = Elems::I64(repos.iter().map(|(_, k)| *k).collect());
            let epoch_rows = if already {
                Self::select_in(
                    tx,
                    "SELECT repo_key, epoch FROM redaction_epochs WHERE org_key = $1 AND repo_key = ANY($2)",
                    vec![Value::I64(org_key)],
                    keys,
                )
                .await?
            } else {
                tx.exec(
                    "INSERT INTO redactions (org_key, object, principal, reason, at_ms) VALUES ($1, $2, $3, $4, $5)",
                    &[
                        Value::I64(org_key),
                        oid(&row.object),
                        Value::from(row.principal.as_str()),
                        Value::from(row.reason.as_str()),
                        Value::I64(int(row.at_ms)?),
                    ],
                )
                .await?;
                t.write(Table::Redactions, 1);
                let purged = tx
                    .exec(
                        "UPDATE objects SET state = 2, inline_bytes = NULL, touched_gen = $3 \
                         WHERE org_key = $1 AND object = $2",
                        &[Value::I64(org_key), oid(&row.object), Value::I64(gc)],
                    )
                    .await?;
                t.write(Table::Objects, purged);
                let rows = Self::select_in(
                    tx,
                    "UPDATE redaction_epochs SET epoch = epoch + 1 WHERE org_key = $1 \
                     AND repo_key = ANY($2) RETURNING repo_key, epoch",
                    vec![Value::I64(org_key)],
                    keys,
                )
                .await?;
                t.write(Table::RedactionEpochs, rows.len() as u64);
                rows
            };
            let by_key: HashMap<i64, u64> = epoch_rows
                .iter()
                .map(|r| Ok((r.i64(0)?, r.u64(1)?)))
                .collect::<Result<_, BeeError>>()?;
            let epochs = repos
                .iter()
                .filter_map(|(id, k)| by_key.get(k).map(|e| (*id, *e)))
                .collect();
            Ok(Redacted {
                newly_redacted: !already,
                epochs,
            })
        })
    }

    fn redaction_row(org: OrgId, r: &Row) -> Result<RedactionRow, BeeError> {
        Ok(RedactionRow {
            org,
            object: parse_oid(&r.bytes(0)?)?,
            principal: r.text(1)?,
            reason: r.text(2)?,
            at_ms: r.u64(3)?,
        })
    }

    pub(crate) async fn get_redactions(
        &self,
        org: OrgId,
        ids: &[ObjectId],
    ) -> Result<Vec<RedactionRow>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let org_key = self.org_key(tx, org).await?;
            t.read(Table::Redactions, ids.len() as u64);
            let rows = Self::select_in(
                tx,
                "SELECT object, principal, reason, at_ms FROM redactions WHERE org_key = $1 AND object = ANY($2)",
                vec![Value::I64(org_key)],
                Elems::ids(ids),
            )
            .await?;
            let mut found = HashMap::new();
            for r in &rows {
                let row = Self::redaction_row(org, r)?;
                found.insert(row.object, row);
            }
            Ok(ids.iter().filter_map(|id| found.get(id).cloned()).collect())
        })
    }

    pub(crate) async fn list_redactions(&self, org: OrgId) -> Result<Vec<RedactionRow>, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            let org_key = self.org_key(tx, org).await?;
            let rows = tx
                .query(
                    "SELECT object, principal, reason, at_ms FROM redactions WHERE org_key = $1 ORDER BY object",
                    &[Value::I64(org_key)],
                )
                .await?;
            t.read(Table::Redactions, rows.len() as u64);
            Ok(rows
                .iter()
                .map(|r| Self::redaction_row(org, r))
                .collect::<Result<Vec<_>, _>>()?)
        })
    }

    pub(crate) async fn redaction_epoch(&self, repo: RepoId) -> Result<u64, BeeError> {
        txn!(self, Mode::Read, |tx, t| async {
            t.read(Table::RedactionEpochs, 1);
            let k = self.repo_keys(tx, repo).await?;
            let rows = tx
                .query(
                    "SELECT epoch FROM redaction_epochs WHERE org_key = $1 AND repo_key = $2",
                    &[Value::I64(k.org), Value::I64(k.repo)],
                )
                .await?;
            Ok(rows.first().map(|r| r.u64(0)).transpose()?.unwrap_or(0))
        })
    }

    // ----- GC ---------------------------------------------------------------------------

    pub(crate) async fn gc_begin(&self, org: OrgId, at_ms: u64) -> Result<GcEpoch, BeeError> {
        txn!(self, Mode::Write, |tx, t| async {
            let org_key = self.org_key(tx, org).await?;
            let seq = Self::lock_org(tx, org_key, true).await? + 1;
            tx.exec(
                "UPDATE orgs SET gc_seq = $2 WHERE org_key = $1",
                &[Value::I64(org_key), Value::I64(seq)],
            )
            .await?;
            tx.exec(
                "INSERT INTO gc_marks (org_key, seq, at_ms) VALUES ($1, $2, $3)",
                &[
                    Value::I64(org_key),
                    Value::I64(seq),
                    Value::I64(int(at_ms)?),
                ],
            )
            .await?;
            t.write(Table::GcMarks, 1);
            Ok(GcEpoch {
                org,
                seq: seq as u64,
                at_ms,
            })
        })
    }

    pub(crate) async fn gc_sweep(
        &self,
        epoch: GcEpoch,
        plan: SweepPlan,
    ) -> Result<SweepOutcome, BeeError> {
        txn!(self, Mode::Write, |tx, t| async {
            let org_key = self.org_key(tx, epoch.org).await?;
            Self::lock_org(tx, org_key, true).await?;
            t.read(Table::GcMarks, 1);
            let e = int(epoch.seq)?;
            let mark = tx
                .query(
                    "SELECT at_ms FROM gc_marks WHERE org_key = $1 AND seq = $2",
                    &[Value::I64(org_key), Value::I64(e)],
                )
                .await?;
            if mark.first().map(|r| r.u64(0)).transpose()? != Some(epoch.at_ms) {
                return Err(invalid("unknown GC epoch").into());
            }
            // Group the planned changesets by repo, keeping plan order for messages.
            let mut keys: BTreeMap<RepoId, RepoKeys> = BTreeMap::new();
            let mut by_repo: BTreeMap<RepoId, Vec<ObjectId>> = BTreeMap::new();
            for (repo, id) in &plan.changesets {
                let k = match keys.get(repo) {
                    Some(k) => *k,
                    None => {
                        let k = self.repo_keys(tx, *repo).await?;
                        keys.insert(*repo, k);
                        k
                    }
                };
                if k.org != org_key || k.org_id != epoch.org {
                    return Err(not_found(repo).into());
                }
                by_repo.entry(*repo).or_default().push(*id);
            }
            let planned: BTreeSet<(RepoId, ObjectId)> = plan.changesets.iter().copied().collect();
            let mut cs_touched: HashMap<(RepoId, ObjectId), i64> = HashMap::new();
            let mut targets: BTreeSet<(RepoId, ObjectId)> = BTreeSet::new();
            let mut kids: BTreeMap<(RepoId, ObjectId), BTreeSet<ObjectId>> = BTreeMap::new();
            for (repo, ids) in &by_repo {
                let k = keys[repo];
                let rk = vec![Value::I64(k.org), Value::I64(k.repo)];
                for r in Self::select_in(
                    tx,
                    "SELECT id, touched_gen FROM changesets WHERE org_key = $1 AND repo_key = $2 AND id = ANY($3)",
                    rk.clone(),
                    Elems::ids(ids),
                )
                .await?
                {
                    cs_touched.insert((*repo, parse_oid(&r.bytes(0)?)?), r.i64(1)?);
                }
                for r in Self::select_in(
                    tx,
                    "SELECT DISTINCT target FROM refs WHERE org_key = $1 AND repo_key = $2 AND target = ANY($3)",
                    rk.clone(),
                    Elems::ids(ids),
                )
                .await?
                {
                    targets.insert((*repo, parse_oid(&r.bytes(0)?)?));
                }
                for r in Self::select_in(
                    tx,
                    "SELECT e.parent, e.child FROM parent_edges e JOIN changesets c \
                     ON c.org_key = e.org_key AND c.repo_key = e.repo_key AND c.id = e.child \
                     WHERE e.org_key = $1 AND e.repo_key = $2 AND e.parent = ANY($3)",
                    rk,
                    Elems::ids(ids),
                )
                .await?
                {
                    kids.entry((*repo, parse_oid(&r.bytes(0)?)?))
                        .or_default()
                        .insert(parse_oid(&r.bytes(1)?)?);
                }
            }
            let mut touched = Vec::new();
            for (repo, id) in &plan.changesets {
                t.read(Table::Changesets, 1);
                let Some(touch_gen) = cs_touched.get(&(*repo, *id)) else {
                    continue;
                };
                if *touch_gen >= e {
                    touched.push(format!("changeset {id} in {repo} was used after the mark"));
                }
                if targets.contains(&(*repo, *id)) {
                    touched.push(format!("changeset {id} in {repo} is a ref target"));
                }
                for kid in kids.get(&(*repo, *id)).into_iter().flatten() {
                    if !planned.contains(&(*repo, *kid)) {
                        touched.push(format!("changeset {id} in {repo} has a kept child {kid}"));
                    }
                }
            }
            let obj_rows: HashMap<ObjectId, (ObjectState, i64)> = Self::select_in(
                tx,
                "SELECT object, state, touched_gen FROM objects WHERE org_key = $1 AND object = ANY($2)",
                vec![Value::I64(org_key)],
                Elems::ids(&plan.objects),
            )
            .await?
            .iter()
            .map(|r| Ok((parse_oid(&r.bytes(0)?)?, (parse_state(r.i64(1)?)?, r.i64(2)?))))
            .collect::<Result<_, BeeError>>()?;
            let mut objects = Vec::new();
            for id in &plan.objects {
                t.read(Table::Objects, 1);
                if let Some((state, touch_gen)) = obj_rows.get(id) {
                    if *state == ObjectState::Purged {
                        continue;
                    }
                    if *touch_gen >= e {
                        touched.push(format!("object {id} was referenced after the mark"));
                    }
                    objects.push(*id);
                }
            }
            if !touched.is_empty() {
                return Ok(SweepOutcome::Deferred { touched });
            }

            let mut changesets = Vec::new();
            let mut done: BTreeSet<(RepoId, ObjectId)> = BTreeSet::new();
            for (repo, id) in &plan.changesets {
                if cs_touched.contains_key(&(*repo, *id)) && done.insert((*repo, *id)) {
                    changesets.push((*repo, *id));
                }
            }
            for (repo, k) in &keys {
                let gone: Vec<ObjectId> = changesets
                    .iter()
                    .filter(|(r, _)| r == repo)
                    .map(|(_, id)| *id)
                    .collect();
                if gone.is_empty() {
                    continue;
                }
                let rk = vec![Value::I64(k.org), Value::I64(k.repo)];
                let n = Self::exec_in(
                    tx,
                    "DELETE FROM changesets WHERE org_key = $1 AND repo_key = $2 AND id = ANY($3)",
                    rk.clone(),
                    Elems::ids(&gone),
                )
                .await?;
                t.write(Table::Changesets, n);
                let n = Self::exec_in(
                    tx,
                    "DELETE FROM parent_edges WHERE org_key = $1 AND repo_key = $2 AND child = ANY($3)",
                    rk.clone(),
                    Elems::ids(&gone),
                )
                .await?;
                t.write(Table::ParentEdges, n);
                let n = Self::exec_in(
                    tx,
                    "DELETE FROM derived WHERE org_key = $1 AND repo_key = $2 AND dkey = ANY($3)",
                    rk,
                    Elems::ids(&gone),
                )
                .await?;
                t.write(Table::Derived, n);
            }
            let ok = vec![Value::I64(org_key)];
            let n = Self::exec_in(
                tx,
                "DELETE FROM object_paths WHERE org_key = $1 AND object = ANY($2)",
                ok.clone(),
                Elems::ids(&objects),
            )
            .await?;
            t.write(Table::ObjectPaths, n);
            let n = Self::exec_in(
                tx,
                "DELETE FROM objects WHERE org_key = $1 AND object = ANY($2)",
                ok.clone(),
                Elems::ids(&objects),
            )
            .await?;
            t.write(Table::Objects, n);
            let n = Self::exec_in(
                tx,
                "DELETE FROM object_uploads WHERE org_key = $1 AND object = ANY($2)",
                ok,
                Elems::ids(&objects),
            )
            .await?;
            t.write(Table::Uploads, n);
            Ok(SweepOutcome::Swept {
                changesets,
                objects,
            })
        })
    }
}

fn fail_err(f: Fail) -> BeeError {
    match f {
        Fail::Retry(e) | Fail::Err(e) => e,
    }
}
