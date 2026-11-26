//! SQLite backend: one database file per surface, one bounded pool of connection threads per
//! surface.
//!
//! A rusqlite connection is not `Sync`, so each connection lives on its own thread and runs
//! jobs sent over a channel; async callers await a oneshot reply, bounded by their deadline.
//! A transaction holds one connection from `BEGIN IMMEDIATE` to `COMMIT`; dropping it queues a
//! `ROLLBACK` on that connection before the connection returns to the pool, so the next user
//! always starts clean.

use std::collections::HashMap;
use std::fs::{File, TryLockError};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, ErrorCode};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use super::{Col, Dialect, Exec, SqlBackend, SqlConfig, SqlTx, Val, migrate_surface};
use crate::error::{ForgeError, Result};
use crate::ids::Clock;
use crate::store::{ForgeTx, Migration, MigrationReport, PublicationLock, Surface, within};

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

struct Worker {
    jobs: mpsc::Sender<Job>,
}

impl Worker {
    fn spawn(path: PathBuf, busy_ms: u64) -> Result<Worker> {
        let conn = Connection::open(&path).map_err(map_err)?;
        conn.busy_timeout(Duration::from_millis(busy_ms))
            .map_err(map_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;",
        )
        .map_err(map_err)?;
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("forge-sqlite".into())
            .spawn(move || {
                let mut conn = conn;
                while let Ok(job) = rx.recv() {
                    job(&mut conn);
                }
            })
            .map_err(|e| ForgeError::unavailable(format!("cannot start sqlite thread: {e}")))?;
        Ok(Worker { jobs: tx })
    }

    async fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> R + Send + 'static,
    ) -> Result<R> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.jobs
            .send(Box::new(move |c| {
                let _ = tx.send(f(c));
            }))
            .map_err(|_| ForgeError::unavailable("sqlite connection thread stopped"))?;
        rx.await
            .map_err(|_| ForgeError::unavailable("sqlite connection thread stopped"))
    }

    fn fire(&self, f: impl FnOnce(&mut Connection) + Send + 'static) {
        let _ = self.jobs.send(Box::new(f));
    }
}

struct Pool {
    free: Mutex<Vec<Worker>>,
    permits: Arc<Semaphore>,
}

impl Pool {
    fn put(&self, w: Worker) {
        if let Ok(mut free) = self.free.lock() {
            free.push(w);
        }
    }
}

pub(crate) struct SqliteBackend {
    cfg: SqlConfig,
    clock: Arc<dyn Clock>,
    pools: Mutex<HashMap<&'static str, Arc<Pool>>>,
}

impl SqliteBackend {
    pub(crate) fn new(cfg: SqlConfig, clock: Arc<dyn Clock>) -> SqliteBackend {
        SqliteBackend {
            cfg,
            clock,
            pools: Mutex::new(HashMap::new()),
        }
    }

    fn path(&self, s: Surface) -> Result<PathBuf> {
        match &self.cfg.backend {
            SqlBackend::Sqlite { dir } => Ok(dir.join(format!("forge-{}.db", s.0))),
            SqlBackend::Postgres { .. } => Err(ForgeError::internal("not a sqlite config")),
        }
    }

    pub(crate) async fn publication_lock(
        &self,
        org: &str,
        shared: bool,
        deadline: Instant,
    ) -> Result<PublicationLock> {
        let key = blake3::hash(org.as_bytes()).to_hex();
        let path = self
            .path(Surface("agents"))?
            .with_file_name(format!("safety-{key}.lock"));
        // Keep the file in place: unlinking a locked file would let another replica lock a
        // different inode. The OS releases the lock when the descriptor closes.
        within(deadline, async move {
            let file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .map_err(|e| ForgeError::unavailable(format!("open safety lock: {e}")))?;
            loop {
                let result = if shared {
                    file.try_lock_shared()
                } else {
                    file.try_lock()
                };
                match result {
                    Ok(()) => return Ok(Box::new(file) as PublicationLock),
                    Err(TryLockError::WouldBlock) => {
                        tokio::time::sleep(Duration::from_millis(5)).await
                    }
                    Err(e) => {
                        return Err(ForgeError::unavailable(format!("acquire safety lock: {e}")));
                    }
                }
            }
        })
        .await
    }

    fn pool(&self, s: Surface) -> Result<Arc<Pool>> {
        if !Surface::valid_name(s.0) {
            return Err(ForgeError::internal(format!("bad surface name {}", s.0)));
        }
        let mut pools = self
            .pools
            .lock()
            .map_err(|_| ForgeError::internal("sqlite pool map poisoned"))?;
        if let Some(p) = pools.get(s.0) {
            return Ok(p.clone());
        }
        let n = self.cfg.pool_size(s);
        let path = self.path(s)?;
        let mut workers = Vec::with_capacity(n);
        for _ in 0..n {
            workers.push(Worker::spawn(path.clone(), self.cfg.busy_timeout_ms)?);
        }
        let pool = Arc::new(Pool {
            free: Mutex::new(workers),
            permits: Arc::new(Semaphore::new(n)),
        });
        pools.insert(s.0, pool.clone());
        Ok(pool)
    }

    async fn checkout(&self, s: Surface, deadline: Instant) -> Result<SqliteExec> {
        let pool = self.pool(s)?;
        let permit = tokio::time::timeout_at(deadline, pool.permits.clone().acquire_owned())
            .await
            .map_err(|_| ForgeError::deadline())?
            .map_err(|_| ForgeError::unavailable("sqlite pool closed"))?;
        let worker = pool
            .free
            .lock()
            .map_err(|_| ForgeError::internal("sqlite pool poisoned"))?
            .pop()
            .ok_or_else(|| ForgeError::internal("sqlite pool has a permit but no connection"))?;
        Ok(SqliteExec {
            worker: Some(worker),
            pool,
            _permit: permit,
            open: false,
        })
    }

    pub(crate) async fn begin(
        &self,
        surface: Surface,
        deadline: Instant,
    ) -> Result<Box<dyn ForgeTx>> {
        let mut exec = self.checkout(surface, deadline).await?;
        // Marked open before BEGIN: if the wait times out, the BEGIN may still succeed later
        // on the connection thread, and the queued ROLLBACK must undo it.
        exec.open = true;
        within(deadline, exec.batch("BEGIN IMMEDIATE")).await?;
        Ok(Box::new(SqlTx {
            exec,
            dialect: Dialect::Sqlite,
            surface,
            deadline,
        }))
    }

    pub(crate) async fn migrate(&self, migrations: &[&Migration]) -> Result<MigrationReport> {
        let mut surfaces: Vec<Surface> = migrations.iter().flat_map(|m| m.surfaces()).collect();
        surfaces.sort();
        surfaces.dedup();
        let mut report = MigrationReport::default();
        for s in surfaces {
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut exec = self.checkout(s, deadline).await?;
            exec.batch("BEGIN IMMEDIATE").await?;
            exec.open = true;
            migrate_surface(
                &mut exec,
                Dialect::Sqlite,
                s,
                migrations,
                self.cfg.allow_contract,
                self.clock.now_ms(),
                &mut report,
            )
            .await?;
            exec.commit().await?;
        }
        Ok(report)
    }
}

struct SqliteExec {
    worker: Option<Worker>,
    pool: Arc<Pool>,
    _permit: OwnedSemaphorePermit,
    open: bool,
}

impl SqliteExec {
    fn worker(&self) -> Result<&Worker> {
        self.worker
            .as_ref()
            .ok_or_else(|| ForgeError::internal("sqlite connection already returned"))
    }
}

impl Drop for SqliteExec {
    fn drop(&mut self) {
        if let Some(w) = self.worker.take() {
            if self.open {
                w.fire(|c| {
                    let _ = c.execute_batch("ROLLBACK");
                });
            }
            self.pool.put(w);
        }
    }
}

fn map_err(e: rusqlite::Error) -> ForgeError {
    match &e {
        rusqlite::Error::SqliteFailure(f, _) => match f.code {
            ErrorCode::ConstraintViolation => {
                ForgeError::already_exists(format!("unique key ({e})"))
            }
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                ForgeError::unavailable(format!("sqlite busy: {e}"))
            }
            ErrorCode::CannotOpen | ErrorCode::SystemIoFailure | ErrorCode::DiskFull => {
                ForgeError::unavailable(format!("sqlite unavailable: {e}"))
            }
            _ => ForgeError::internal(format!("sqlite: {e}")),
        },
        _ => ForgeError::internal(format!("sqlite: {e}")),
    }
}

fn bind(v: Val) -> Value {
    match v {
        Val::Text(s) => Value::Text(s),
        Val::OptText(s) => s.map(Value::Text).unwrap_or(Value::Null),
        Val::Int(i) => Value::Integer(i),
        Val::OptInt(i) => i.map(Value::Integer).unwrap_or(Value::Null),
        Val::Blob(b) => Value::Blob(b),
    }
}

fn read(v: ValueRef<'_>, col: Col) -> Result<Val> {
    let text = |b: &[u8]| {
        String::from_utf8(b.to_vec()).map_err(|_| ForgeError::internal("non-UTF-8 text column"))
    };
    Ok(match (col, v) {
        (Col::Text, ValueRef::Text(b)) => Val::Text(text(b)?),
        (Col::OptText, ValueRef::Text(b)) => Val::OptText(Some(text(b)?)),
        (Col::OptText, ValueRef::Null) => Val::OptText(None),
        (Col::Int, ValueRef::Integer(i)) => Val::Int(i),
        (Col::OptInt, ValueRef::Integer(i)) => Val::OptInt(Some(i)),
        (Col::OptInt, ValueRef::Null) => Val::OptInt(None),
        (Col::Blob, ValueRef::Blob(b)) => Val::Blob(b.to_vec()),
        (Col::Blob, ValueRef::Null) => Val::Blob(Vec::new()),
        (col, other) => {
            return Err(ForgeError::internal(format!(
                "sqlite column type {:?} does not match {col:?}",
                other.data_type()
            )));
        }
    })
}

#[async_trait]
impl Exec for SqliteExec {
    async fn query(&mut self, sql: &str, params: Vec<Val>, cols: &[Col]) -> Result<Vec<Vec<Val>>> {
        let sql = sql.to_string();
        let cols = cols.to_vec();
        self.worker()?
            .call(move |c| -> Result<Vec<Vec<Val>>> {
                let mut stmt = c.prepare_cached(&sql).map_err(map_err)?;
                let values: Vec<Value> = params.into_iter().map(bind).collect();
                let mut rows = stmt
                    .query(rusqlite::params_from_iter(values))
                    .map_err(map_err)?;
                let mut out = Vec::new();
                while let Some(row) = rows.next().map_err(map_err)? {
                    let mut vals = Vec::with_capacity(cols.len());
                    for (i, col) in cols.iter().enumerate() {
                        vals.push(read(row.get_ref(i).map_err(map_err)?, *col)?);
                    }
                    out.push(vals);
                }
                Ok(out)
            })
            .await?
    }

    async fn execute(&mut self, sql: &str, params: Vec<Val>) -> Result<u64> {
        let sql = sql.to_string();
        self.worker()?
            .call(move |c| -> Result<u64> {
                let mut stmt = c.prepare_cached(&sql).map_err(map_err)?;
                let values: Vec<Value> = params.into_iter().map(bind).collect();
                let n = stmt
                    .execute(rusqlite::params_from_iter(values))
                    .map_err(map_err)?;
                Ok(n as u64)
            })
            .await?
    }

    async fn batch(&mut self, sql: &str) -> Result<()> {
        let sql = sql.to_string();
        self.worker()?
            .call(move |c| c.execute_batch(&sql).map_err(map_err))
            .await?
    }

    async fn commit(&mut self) -> Result<()> {
        self.batch("COMMIT").await?;
        self.open = false;
        Ok(())
    }
}
