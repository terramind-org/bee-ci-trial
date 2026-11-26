//! Postgres backend: forge tables in their own schema, one bounded connection pool per
//! surface.
//!
//! Connections are opened lazily up to the pool size and carry a per-connection prepared
//! statement cache. A transaction sets `statement_timeout` from its deadline. A transaction
//! dropped without commit (every read-only one) is rolled back on a background task that then
//! returns the connection to the pool; its pool slot stays taken until that finishes, so a
//! half-finished transaction never leaks into the next user and the pool never exceeds its
//! size. Only a connection whose rollback fails or times out is discarded.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, NoTls, Statement};

use super::{Col, Dialect, Exec, SqlBackend, SqlConfig, SqlTx, Val, migrate_surface};
use crate::error::{ForgeError, Result};
use crate::ids::Clock;
use crate::store::{ForgeTx, Migration, MigrationReport, PublicationLock, Surface, within};

/// Advisory lock key serializing concurrent migration runs.
const MIGRATION_LOCK: i64 = 0x4245_4548_4956; // "BEEHIV"

/// How long the background rollback of an abandoned transaction may take before its
/// connection is discarded instead of reused.
const ROLLBACK_TIMEOUT: Duration = Duration::from_secs(5);

struct Conn {
    client: Client,
    cache: HashMap<String, Statement>,
}

struct Pool {
    free: Mutex<Vec<Conn>>,
    permits: Arc<Semaphore>,
}

pub(crate) struct PgBackend {
    cfg: SqlConfig,
    clock: Arc<dyn Clock>,
    pools: Mutex<HashMap<&'static str, Arc<Pool>>>,
    connects: AtomicU64,
}

impl PgBackend {
    pub(crate) fn new(cfg: SqlConfig, clock: Arc<dyn Clock>) -> PgBackend {
        PgBackend {
            cfg,
            clock,
            pools: Mutex::new(HashMap::new()),
            connects: AtomicU64::new(0),
        }
    }

    /// Connections opened since this backend was created, over every surface's pool.
    pub(crate) fn connections_opened(&self) -> u64 {
        self.connects.load(Ordering::Relaxed)
    }

    fn url_schema(&self) -> Result<(&str, &str)> {
        match &self.cfg.backend {
            SqlBackend::Postgres { url, schema } => Ok((url, schema)),
            SqlBackend::Sqlite { .. } => Err(ForgeError::internal("not a postgres config")),
        }
    }

    fn pool(&self, s: Surface) -> Result<Arc<Pool>> {
        let mut pools = self
            .pools
            .lock()
            .map_err(|_| ForgeError::internal("postgres pool map poisoned"))?;
        Ok(pools
            .entry(s.0)
            .or_insert_with(|| {
                Arc::new(Pool {
                    free: Mutex::new(Vec::new()),
                    permits: Arc::new(Semaphore::new(self.cfg.pool_size(s))),
                })
            })
            .clone())
    }

    async fn connect(&self) -> Result<Conn> {
        let (url, schema) = self.url_schema()?;
        self.connects.fetch_add(1, Ordering::Relaxed);
        let (client, connection) = tokio_postgres::connect(url, NoTls)
            .await
            .map_err(|e| ForgeError::unavailable(format!("postgres connect: {e}")))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
            .batch_execute(&format!("SET search_path TO {schema}"))
            .await
            .map_err(map_err)?;
        Ok(Conn {
            client,
            cache: HashMap::new(),
        })
    }

    async fn checkout(&self, s: Surface, deadline: Instant) -> Result<PgExec> {
        let pool = self.pool(s)?;
        let permit = tokio::time::timeout_at(deadline, pool.permits.clone().acquire_owned())
            .await
            .map_err(|_| ForgeError::deadline())?
            .map_err(|_| ForgeError::unavailable("postgres pool closed"))?;
        let reused = loop {
            let next = pool
                .free
                .lock()
                .map_err(|_| ForgeError::internal("postgres pool poisoned"))?
                .pop();
            match next {
                Some(c) if c.client.is_closed() => continue,
                other => break other,
            }
        };
        let conn = match reused {
            Some(c) => c,
            None => within(deadline, self.connect()).await?,
        };
        Ok(PgExec {
            conn: Some(conn),
            pool,
            permit: Some(permit),
            open: false,
        })
    }

    pub(crate) async fn begin(
        &self,
        surface: Surface,
        deadline: Instant,
    ) -> Result<Box<dyn ForgeTx>> {
        let mut exec = self.checkout(surface, deadline).await?;
        let ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .max(1);
        // Marked open first: a BEGIN that times out is rolled back on drop.
        exec.open = true;
        within(
            deadline,
            exec.batch(&format!(
                "BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL statement_timeout = {ms}"
            )),
        )
        .await?;
        Ok(Box::new(SqlTx {
            exec,
            dialect: Dialect::Postgres,
            surface,
            deadline,
        }))
    }

    pub(crate) async fn publication_lock(
        &self,
        org: &str,
        shared: bool,
        deadline: Instant,
    ) -> Result<PublicationLock> {
        let (_, schema) = self.url_schema()?;
        let hash = blake3::derive_key(
            "bee forge safety publication lock v1",
            format!("{schema}/{org}").as_bytes(),
        );
        let mut key = [0; 8];
        key.copy_from_slice(&hash[..8]);
        let key = i64::from_le_bytes(key);
        let lock = if shared {
            "pg_advisory_xact_lock_shared"
        } else {
            "pg_advisory_xact_lock"
        };
        // These connections hold advisory locks only. In particular, an in-flight
        // publication must never consume an agents-store connection through its CAS.
        let mut exec = self
            .checkout(Surface("safety_publication"), deadline)
            .await?;
        let ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .max(1);
        exec.open = true;
        within(deadline, exec.batch(&format!(
            "BEGIN READ ONLY; SET LOCAL statement_timeout = {ms}; SELECT {lock}('{key}'::bigint)"
        ))).await?;
        Ok(Box::new(exec))
    }

    pub(crate) async fn migrate(&self, migrations: &[&Migration]) -> Result<MigrationReport> {
        let (_, schema) = self.url_schema()?;
        let schema = schema.to_string();
        let mut surfaces: Vec<Surface> = migrations.iter().flat_map(|m| m.surfaces()).collect();
        surfaces.sort();
        surfaces.dedup();
        let mut report = MigrationReport::default();
        for s in surfaces {
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut exec = self.checkout(s, deadline).await?;
            exec.batch(&format!(
                "BEGIN; SELECT pg_advisory_xact_lock({MIGRATION_LOCK}); \
                 CREATE SCHEMA IF NOT EXISTS {schema}"
            ))
            .await?;
            exec.open = true;
            migrate_surface(
                &mut exec,
                Dialect::Postgres,
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

struct PgExec {
    conn: Option<Conn>,
    pool: Arc<Pool>,
    /// Held until the connection is back in the pool (after a background rollback if needed).
    permit: Option<OwnedSemaphorePermit>,
    open: bool,
}

impl PgExec {
    fn conn(&mut self) -> Result<&mut Conn> {
        self.conn
            .as_mut()
            .ok_or_else(|| ForgeError::internal("postgres connection already returned"))
    }

    async fn stmt(&mut self, sql: &str) -> Result<Statement> {
        let conn = self.conn()?;
        if let Some(s) = conn.cache.get(sql) {
            return Ok(s.clone());
        }
        let s = conn.client.prepare(sql).await.map_err(map_err)?;
        conn.cache.insert(sql.to_string(), s.clone());
        Ok(s)
    }
}

impl Drop for PgExec {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        if conn.client.is_closed() {
            return;
        }
        if !self.open {
            if let Ok(mut free) = self.pool.free.lock() {
                free.push(conn);
            }
            return;
        }
        // An open transaction: roll it back off the dropping task, then hand the connection
        // and its pool slot back. Without a runtime the connection is discarded and the
        // server rolls back when the session ends.
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let pool = self.pool.clone();
        let permit = self.permit.take();
        rt.spawn(async move {
            let rolled_back =
                tokio::time::timeout(ROLLBACK_TIMEOUT, conn.client.batch_execute("ROLLBACK")).await;
            if matches!(rolled_back, Ok(Ok(())))
                && !conn.client.is_closed()
                && let Ok(mut free) = pool.free.lock()
            {
                free.push(conn);
            }
            drop(permit);
        });
    }
}

fn map_err(e: tokio_postgres::Error) -> ForgeError {
    if let Some(code) = e.code() {
        if *code == SqlState::UNIQUE_VIOLATION {
            return ForgeError::already_exists(format!("unique key ({e})"));
        }
        if *code == SqlState::T_R_SERIALIZATION_FAILURE
            || *code == SqlState::T_R_DEADLOCK_DETECTED
            || *code == SqlState::LOCK_NOT_AVAILABLE
        {
            return ForgeError::unavailable(format!("postgres contention: {e}"));
        }
        if *code == SqlState::QUERY_CANCELED {
            return ForgeError::deadline();
        }
        return ForgeError::internal(format!("postgres: {e}"));
    }
    ForgeError::unavailable(format!("postgres: {e}"))
}

fn to_sql(v: Val) -> Box<dyn ToSql + Sync + Send> {
    match v {
        Val::Text(s) => Box::new(s),
        Val::OptText(s) => Box::new(s),
        Val::Int(i) => Box::new(i),
        Val::OptInt(i) => Box::new(i),
        Val::Blob(b) => Box::new(b),
    }
}

fn read(row: &tokio_postgres::Row, i: usize, col: Col) -> Result<Val> {
    let r = match col {
        Col::Text => row.try_get::<_, String>(i).map(Val::Text),
        Col::OptText => row.try_get::<_, Option<String>>(i).map(Val::OptText),
        Col::Int => row.try_get::<_, i64>(i).map(Val::Int),
        Col::OptInt => row.try_get::<_, Option<i64>>(i).map(Val::OptInt),
        Col::Blob => row.try_get::<_, Vec<u8>>(i).map(Val::Blob),
    };
    r.map_err(|e| ForgeError::internal(format!("postgres column {i}: {e}")))
}

#[async_trait]
impl Exec for PgExec {
    async fn query(&mut self, sql: &str, params: Vec<Val>, cols: &[Col]) -> Result<Vec<Vec<Val>>> {
        let stmt = self.stmt(sql).await?;
        let boxed: Vec<Box<dyn ToSql + Sync + Send>> = params.into_iter().map(to_sql).collect();
        let refs: Vec<&(dyn ToSql + Sync)> = boxed
            .iter()
            .map(|b| b.as_ref() as &(dyn ToSql + Sync))
            .collect();
        let rows = self
            .conn()?
            .client
            .query(&stmt, &refs)
            .await
            .map_err(map_err)?;
        rows.iter()
            .map(|r| {
                cols.iter()
                    .enumerate()
                    .map(|(i, c)| read(r, i, *c))
                    .collect()
            })
            .collect()
    }

    async fn execute(&mut self, sql: &str, params: Vec<Val>) -> Result<u64> {
        let stmt = self.stmt(sql).await?;
        let boxed: Vec<Box<dyn ToSql + Sync + Send>> = params.into_iter().map(to_sql).collect();
        let refs: Vec<&(dyn ToSql + Sync)> = boxed
            .iter()
            .map(|b| b.as_ref() as &(dyn ToSql + Sync))
            .collect();
        self.conn()?
            .client
            .execute(&stmt, &refs)
            .await
            .map_err(map_err)
    }

    async fn batch(&mut self, sql: &str) -> Result<()> {
        self.conn()?
            .client
            .batch_execute(sql)
            .await
            .map_err(map_err)
    }

    async fn commit(&mut self) -> Result<()> {
        self.batch("COMMIT").await?;
        self.open = false;
        Ok(())
    }
}
