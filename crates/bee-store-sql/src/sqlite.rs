//! The SQLite backend (dev mode): one writer connection and a few reader connections on a
//! WAL database, every statement run on tokio's blocking pool.
//!
//! Durability: `journal_mode = WAL`, `synchronous = FULL`, and on macOS `fullfsync = ON` and
//! `checkpoint_fullfsync = ON` (a plain `fsync` there does not flush the drive's cache).
//! Single-writer semantics: write transactions take the one writer connection for their whole
//! duration and start with `BEGIN IMMEDIATE`, so they are serial; readers see the last
//! committed state.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{Connection, OpenFlags, ToSql};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::backend::{Backend, Dialect, Fail, Mode, Row, Tx, Value};
use crate::migrate;

/// Where the SQLite database lives.
#[derive(Clone, Debug)]
pub struct SqliteOptions {
    /// Database file (created if missing).
    pub path: PathBuf,
    /// A read replica file (for example one restored by a WAL shipper), opened read-only;
    /// reads that may lag go there.
    pub replica_path: Option<PathBuf>,
    /// Reader connections.
    pub readers: usize,
    /// Deadline of every statement (interrupts long statements; bounds busy waits).
    pub query_timeout: Duration,
    /// How long a replica read with a `min_version` waits for the replica before it is
    /// forwarded to the primary (SPEC §6.3 `read_wait_ms`).
    pub read_wait: Duration,
}

impl SqliteOptions {
    /// Defaults for `path`: 4 readers, 5 s statements, 50 ms replica wait.
    pub fn new(path: impl AsRef<Path>) -> SqliteOptions {
        SqliteOptions {
            path: path.as_ref().to_path_buf(),
            replica_path: None,
            readers: 4,
            query_timeout: Duration::from_secs(5),
            read_wait: Duration::from_millis(50),
        }
    }
}

impl ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Value::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Value::I64(v) => ToSqlOutput::Borrowed(ValueRef::Integer(*v)),
            Value::Bytes(v) => ToSqlOutput::Borrowed(ValueRef::Blob(v)),
            Value::Text(v) => ToSqlOutput::Borrowed(ValueRef::Text(v.as_bytes())),
            Value::I64s(_) | Value::Bytess(_) | Value::Texts(_) => {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    "array parameters are not supported by SQLite".into(),
                ));
            }
        })
    }
}

/// Classifies a SQLite error.
pub(crate) fn classify(e: rusqlite::Error) -> Fail {
    if let rusqlite::Error::SqliteFailure(f, _) = &e {
        match f.code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                return Fail::conflict(e);
            }
            rusqlite::ErrorCode::OperationInterrupted => return Fail::deadline(e),
            rusqlite::ErrorCode::ConstraintViolation => return Fail::conflict(e),
            rusqlite::ErrorCode::CannotOpen | rusqlite::ErrorCode::SystemIoFailure => {
                return Fail::unavailable(e);
            }
            _ => {}
        }
    }
    Fail::internal(e)
}

fn convert(v: ValueRef<'_>) -> Result<Value, Fail> {
    Ok(match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::I64(i),
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
        ValueRef::Text(t) => Value::Text(
            String::from_utf8(t.to_vec()).map_err(|e| Fail::internal(format!("bad text: {e}")))?,
        ),
        ValueRef::Real(_) => return Err(Fail::internal("unexpected REAL column")),
    })
}

/// Runs one statement with a deadline: its rows, and how many rows it changed (or returned).
fn run(
    conn: &mut Connection,
    sql: &str,
    params: &[Value],
    timeout: Duration,
) -> Result<(Vec<Row>, u64), Fail> {
    let deadline = Instant::now() + timeout;
    conn.progress_handler(1_000, Some(move || Instant::now() > deadline))
        .map_err(classify)?;
    let res = (|| -> Result<(Vec<Row>, u64), Fail> {
        let mut stmt = conn.prepare_cached(sql).map_err(classify)?;
        for (i, p) in params.iter().enumerate() {
            let name = format!("${}", i + 1);
            if let Some(idx) = stmt.parameter_index(&name).map_err(classify)? {
                stmt.raw_bind_parameter(idx, p).map_err(classify)?;
            }
        }
        let ncols = stmt.column_count();
        if ncols == 0 {
            let n = stmt.raw_execute().map_err(classify)?;
            return Ok((Vec::new(), n as u64));
        }
        let mut rows = stmt.raw_query();
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(classify)? {
            let mut vals = Vec::with_capacity(ncols);
            for i in 0..ncols {
                vals.push(convert(r.get_ref(i).map_err(classify)?)?);
            }
            out.push(Row(vals));
        }
        drop(rows);
        let n = out.len() as u64;
        Ok((out, n))
    })();
    let cleared = conn
        .progress_handler(0, None::<fn() -> bool>)
        .map_err(classify);
    let out = res?;
    cleared?;
    Ok(out)
}

fn configure(conn: &Connection, timeout: Duration, writer: bool) -> Result<(), Fail> {
    conn.busy_timeout(timeout).map_err(classify)?;
    if writer {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(classify)?;
    }
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(classify)?;
    if cfg!(target_os = "macos") {
        conn.pragma_update(None, "fullfsync", "ON")
            .map_err(classify)?;
        conn.pragma_update(None, "checkpoint_fullfsync", "ON")
            .map_err(classify)?;
    }
    conn.pragma_update(None, "foreign_keys", "OFF")
        .map_err(classify)?;
    conn.set_prepared_statement_cache_capacity(256);
    Ok(())
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Result<R, Fail> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Fail::internal(format!("blocking task failed: {e}")))
}

/// A group of connections handed out round-robin.
struct Conns {
    conns: Vec<Arc<Mutex<Connection>>>,
    next: AtomicUsize,
}

impl Conns {
    async fn get(&self) -> OwnedMutexGuard<Connection> {
        let n = self.conns.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        for k in 0..n {
            if let Ok(g) = self.conns[(start + k) % n].clone().try_lock_owned() {
                return g;
            }
        }
        self.conns[start % n].clone().lock_owned().await
    }
}

/// The SQLite backend.
pub(crate) struct Lite {
    writer: Arc<Mutex<Connection>>,
    readers: Conns,
    replica: Option<Conns>,
    timeout: Duration,
}

impl Lite {
    /// Opens (creating if needed) the database and applies pending migrations.
    pub(crate) async fn open(opts: &SqliteOptions) -> Result<Lite, Fail> {
        let o = opts.clone();
        let (writer, readers, replica) = blocking(move || -> Result<_, Fail> {
            let mut w = Connection::open(&o.path).map_err(classify)?;
            configure(&w, o.query_timeout, true)?;
            migrate_conn(&mut w)?;
            let mut readers = Vec::new();
            for _ in 0..o.readers.max(1) {
                let r = Connection::open_with_flags(
                    &o.path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(classify)?;
                configure(&r, o.query_timeout, false)?;
                readers.push(Arc::new(Mutex::new(r)));
            }
            let replica = match &o.replica_path {
                Some(p) => {
                    let mut v = Vec::new();
                    for _ in 0..o.readers.max(1) {
                        let r = Connection::open_with_flags(
                            p,
                            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                        )
                        .map_err(classify)?;
                        r.busy_timeout(o.query_timeout).map_err(classify)?;
                        v.push(Arc::new(Mutex::new(r)));
                    }
                    Some(v)
                }
                None => None,
            };
            Ok((w, readers, replica))
        })
        .await??;
        Ok(Lite {
            writer: Arc::new(Mutex::new(writer)),
            readers: Conns {
                conns: readers,
                next: AtomicUsize::new(0),
            },
            replica: replica.map(|conns| Conns {
                conns,
                next: AtomicUsize::new(0),
            }),
            timeout: opts.query_timeout,
        })
    }
}

fn migrate_conn(conn: &mut Connection) -> Result<(), Fail> {
    let d = Dialect::Sqlite;
    conn.execute_batch("BEGIN IMMEDIATE").map_err(classify)?;
    let res = (|| {
        conn.execute_batch(&migrate::schema_table_sql(d))
            .map_err(classify)?;
        let applied = {
            let mut stmt = conn
                .prepare("SELECT version, digest FROM bee_schema ORDER BY version")
                .map_err(classify)?;
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .map_err(classify)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(classify)?
        };
        for (m, rendered) in migrate::pending(&applied, d)? {
            conn.execute_batch(&rendered).map_err(classify)?;
            conn.execute(
                "INSERT INTO bee_schema (version, name, digest) VALUES (?1, ?2, ?3)",
                rusqlite::params![m.version, m.name, migrate::digest(&rendered)],
            )
            .map_err(classify)?;
        }
        Ok::<(), Fail>(())
    })();
    match res {
        Ok(()) => conn.execute_batch("COMMIT").map_err(classify),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// A checked-out connection that rolls back a transaction left open when it is dropped: a
/// caller cancelled mid-transaction (a crashed task, an expired deadline) must not keep the
/// database's write lock until this store happens to begin again, because another store on
/// the same file (another server process) would wait on it forever.
struct ConnGuard(OwnedMutexGuard<Connection>);

impl std::ops::Deref for ConnGuard {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.0
    }
}

impl std::ops::DerefMut for ConnGuard {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.0
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if !self.0.is_autocommit() {
            let _ = self.0.execute_batch("ROLLBACK");
        }
    }
}

/// An open SQLite transaction (writer) or a reader connection running autocommit statements.
pub(crate) struct LiteTx {
    conn: Option<ConnGuard>,
    in_txn: bool,
    timeout: Duration,
}

impl LiteTx {
    async fn with<R: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut Connection) -> Result<R, Fail> + Send + 'static,
    ) -> Result<R, Fail> {
        let mut g = self
            .conn
            .take()
            .ok_or_else(|| Fail::internal("connection lost by an earlier failure"))?;
        let (g, r) = blocking(move || {
            let r = f(&mut g);
            (g, r)
        })
        .await?;
        self.conn = Some(g);
        r
    }
}

impl Backend for Lite {
    type Tx = LiteTx;
    const DIALECT: Dialect = Dialect::Sqlite;

    async fn begin(&self, mode: Mode) -> Result<LiteTx, Fail> {
        let (guard, begin) = match mode {
            Mode::Read => (self.readers.get().await, false),
            Mode::Replica => match &self.replica {
                Some(r) => (r.get().await, false),
                None => (self.readers.get().await, false),
            },
            Mode::Write | Mode::Serializable => (self.writer.clone().lock_owned().await, true),
        };
        let mut tx = LiteTx {
            conn: Some(ConnGuard(guard)),
            in_txn: begin,
            timeout: self.timeout,
        };
        tx.with(move |c| {
            // A transaction abandoned by a cancelled caller is rolled back here.
            if !c.is_autocommit() {
                c.execute_batch("ROLLBACK").map_err(classify)?;
            }
            if begin {
                c.execute_batch("BEGIN IMMEDIATE").map_err(classify)?;
            }
            Ok(())
        })
        .await?;
        Ok(tx)
    }

    fn has_replica(&self) -> bool {
        self.replica.is_some()
    }
}

impl Tx for LiteTx {
    async fn query(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Row>, Fail> {
        let sql = sql.to_string();
        let params = params.to_vec();
        let timeout = self.timeout;
        self.with(move |c| run(c, &sql, &params, timeout).map(|(rows, _)| rows))
            .await
    }

    async fn exec(&mut self, sql: &str, params: &[Value]) -> Result<u64, Fail> {
        let sql = sql.to_string();
        let params = params.to_vec();
        let timeout = self.timeout;
        self.with(move |c| run(c, &sql, &params, timeout).map(|(_, n)| n))
            .await
    }

    async fn commit(mut self) -> Result<(), Fail> {
        if self.in_txn {
            self.with(|c| c.execute_batch("COMMIT").map_err(classify))
                .await?;
        }
        Ok(())
    }

    async fn rollback(mut self) {
        if self.in_txn {
            let _ = self
                .with(|c| c.execute_batch("ROLLBACK").map_err(classify))
                .await;
        }
    }
}
