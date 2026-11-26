//! The Postgres backend: a small connection pool over `tokio-postgres`, per-connection
//! prepared-statement caches, deadlines on every statement and SQLSTATE classification.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::BytesMut;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{IsNull, ToSql, Type, to_sql_checked};
use tokio_postgres::{Client, Config, NoTls, Statement};

use crate::backend::{Backend, Dialect, Fail, Mode, Row, Tx, Value};
use crate::migrate;

/// How to reach Postgres.
#[derive(Clone, Debug)]
pub struct PgOptions {
    /// Connection string of the primary (`postgres://user@host:port/db` or key=value form).
    pub url: String,
    /// Connection string of a read replica, if any. Reads that may lag (`ReadFrom::Replica`)
    /// go there; everything else goes to the primary.
    pub replica_url: Option<String>,
    /// Schema to use (created if missing); `None` uses the connection's default.
    pub schema: Option<String>,
    /// Most connections open to each server at once.
    pub max_connections: usize,
    /// Deadline of every statement, enforced by the server (`statement_timeout`) and the
    /// client.
    pub query_timeout: Duration,
    /// Deadline for opening a connection or waiting for a free one.
    pub connect_timeout: Duration,
    /// How long a replica read with a `min_version` waits for the replica to catch up before
    /// it is forwarded to the primary (SPEC §6.3 `read_wait_ms`).
    pub read_wait: Duration,
}

impl PgOptions {
    /// Defaults for `url`: 16 connections, 5 s statements, 50 ms replica wait.
    pub fn new(url: impl Into<String>) -> PgOptions {
        PgOptions {
            url: url.into(),
            replica_url: None,
            schema: None,
            max_connections: 16,
            query_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
            read_wait: Duration::from_millis(50),
        }
    }
}

impl ToSql for Value {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        match self {
            Value::Null => Ok(IsNull::Yes),
            Value::I64(v) => match *ty {
                Type::INT4 => i32::try_from(*v)?.to_sql(ty, out),
                Type::INT2 => i16::try_from(*v)?.to_sql(ty, out),
                _ => v.to_sql(ty, out),
            },
            Value::Bytes(v) => v.to_sql(ty, out),
            Value::Text(v) => v.to_sql(ty, out),
            Value::I64s(v) => v.to_sql(ty, out),
            Value::Bytess(v) => v.to_sql(ty, out),
            Value::Texts(v) => v.to_sql(ty, out),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

fn convert_row(row: &tokio_postgres::Row) -> Result<Row, Fail> {
    let mut out = Vec::with_capacity(row.len());
    for (i, col) in row.columns().iter().enumerate() {
        let v = match *col.type_() {
            Type::INT8 => row.try_get::<_, Option<i64>>(i).map(|v| v.map(Value::I64)),
            Type::INT4 => row
                .try_get::<_, Option<i32>>(i)
                .map(|v| v.map(|x| Value::I64(x.into()))),
            Type::INT2 => row
                .try_get::<_, Option<i16>>(i)
                .map(|v| v.map(|x| Value::I64(x.into()))),
            Type::BOOL => row
                .try_get::<_, Option<bool>>(i)
                .map(|v| v.map(|x| Value::I64(x.into()))),
            Type::BYTEA => row
                .try_get::<_, Option<Vec<u8>>>(i)
                .map(|v| v.map(Value::Bytes)),
            Type::TEXT | Type::VARCHAR | Type::NAME | Type::BPCHAR => row
                .try_get::<_, Option<String>>(i)
                .map(|v| v.map(Value::Text)),
            ref other => return Err(Fail::internal(format!("unexpected column type {other}"))),
        }
        .map_err(Fail::internal)?;
        out.push(v.unwrap_or(Value::Null));
    }
    Ok(Row(out))
}

/// Classifies a driver error (SPEC §6.1: serialization failures are retried by the store).
pub(crate) fn classify(e: tokio_postgres::Error) -> Fail {
    if let Some(code) = e.code() {
        let c = code.clone();
        if c == SqlState::T_R_SERIALIZATION_FAILURE
            || c == SqlState::T_R_DEADLOCK_DETECTED
            || c == SqlState::UNIQUE_VIOLATION
            || c == SqlState::LOCK_NOT_AVAILABLE
        {
            return Fail::conflict(format!("{} ({})", e, c.code()));
        }
        if c == SqlState::QUERY_CANCELED {
            return Fail::deadline(e);
        }
        if c == SqlState::ADMIN_SHUTDOWN
            || c == SqlState::CANNOT_CONNECT_NOW
            || c == SqlState::TOO_MANY_CONNECTIONS
            || c == SqlState::READ_ONLY_SQL_TRANSACTION
        {
            return Fail::unavailable(e);
        }
        return Fail::internal(format!("{} ({})", e, c.code()));
    }
    if e.is_closed() {
        return Fail::unavailable(e);
    }
    Fail::unavailable(e)
}

/// One open connection with its statement cache.
pub(crate) struct PgConn {
    client: Client,
    stmts: HashMap<String, Statement>,
}

struct Pool {
    config: Config,
    schema: Option<String>,
    query_timeout: Duration,
    connect_timeout: Duration,
    read_only: bool,
    idle: Mutex<Vec<PgConn>>,
    slots: Arc<Semaphore>,
}

/// A connection borrowed from a pool; returned on [`Lease::release`], closed when dropped.
pub(crate) struct Lease {
    conn: Option<PgConn>,
    pool: Arc<Pool>,
    _permit: OwnedSemaphorePermit,
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Pool {
    fn new(url: &str, opts: &PgOptions, read_only: bool) -> Result<Arc<Pool>, Fail> {
        let mut config = Config::from_str(url).map_err(|e| {
            Fail::Err(bee_core::BeeError::invalid(
                bee_core::Subcode::InvalidField,
                format!("bad Postgres connection string: {e}"),
            ))
        })?;
        config.connect_timeout(opts.connect_timeout);
        config.application_name("bee-store-sql");
        Ok(Arc::new(Pool {
            config,
            schema: opts.schema.clone(),
            query_timeout: opts.query_timeout,
            connect_timeout: opts.connect_timeout,
            read_only,
            idle: Mutex::new(Vec::new()),
            slots: Arc::new(Semaphore::new(opts.max_connections.max(1))),
        }))
    }

    async fn connect(&self) -> Result<PgConn, Fail> {
        let (client, connection) =
            tokio::time::timeout(self.connect_timeout, self.config.connect(NoTls))
                .await
                .map_err(|_| Fail::unavailable("timed out connecting to Postgres"))?
                .map_err(classify)?;
        tokio::spawn(async move {
            // The connection ends when the client is dropped or the server goes away; either
            // way every pending query on it fails, which classify() reports.
            let _ = connection.await;
        });
        let mut setup = format!(
            "SET statement_timeout = {}; SET idle_in_transaction_session_timeout = {};",
            self.query_timeout.as_millis(),
            (self.query_timeout * 4).as_millis()
        );
        if self.read_only {
            setup.push_str(" SET default_transaction_read_only = on;");
        }
        if let Some(s) = &self.schema {
            // The schema itself is created by the migrator, under its lock.
            setup.push_str(&format!(" SET search_path = {};", quote_ident(s)));
        }
        tokio::time::timeout(self.query_timeout, client.batch_execute(&setup))
            .await
            .map_err(|_| Fail::deadline("connection setup"))?
            .map_err(classify)?;
        Ok(PgConn {
            client,
            stmts: HashMap::new(),
        })
    }

    async fn get(self: &Arc<Self>) -> Result<Lease, Fail> {
        let permit = tokio::time::timeout(self.connect_timeout, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| Fail::unavailable("no free Postgres connection"))?
            .map_err(|_| Fail::unavailable("pool closed"))?;
        let reused = loop {
            let c = self
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop();
            match c {
                Some(c) if c.client.is_closed() => continue,
                other => break other,
            }
        };
        let conn = match reused {
            Some(c) => c,
            None => self.connect().await?,
        };
        Ok(Lease {
            conn: Some(conn),
            pool: self.clone(),
            _permit: permit,
        })
    }
}

impl Lease {
    fn conn(&mut self) -> Result<&mut PgConn, Fail> {
        self.conn
            .as_mut()
            .ok_or_else(|| Fail::internal("connection already released"))
    }

    /// Returns the connection to the pool.
    fn release(mut self) {
        if let Some(c) = self.conn.take()
            && !c.client.is_closed()
        {
            self.pool
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(c);
        }
    }

    /// Drops the connection (after a client-side timeout its state is unknown).
    fn discard(&mut self) {
        self.conn = None;
    }

    async fn statement(&mut self, sql: &str) -> Result<Statement, Fail> {
        let timeout = self.pool.query_timeout;
        let conn = self.conn()?;
        if let Some(s) = conn.stmts.get(sql) {
            return Ok(s.clone());
        }
        let prepared = tokio::time::timeout(timeout, conn.client.prepare(sql)).await;
        match prepared {
            Ok(Ok(s)) => {
                conn.stmts.insert(sql.to_string(), s.clone());
                Ok(s)
            }
            Ok(Err(e)) => Err(classify(e)),
            Err(_) => {
                self.discard();
                Err(Fail::deadline("prepare"))
            }
        }
    }

    async fn query(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Row>, Fail> {
        let stmt = self.statement(sql).await?;
        let timeout = self.pool.query_timeout + Duration::from_secs(1);
        let refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let conn = self.conn()?;
        match tokio::time::timeout(timeout, conn.client.query(&stmt, &refs)).await {
            Ok(Ok(rows)) => rows.iter().map(convert_row).collect(),
            Ok(Err(e)) => Err(classify(e)),
            Err(_) => {
                self.discard();
                Err(Fail::deadline(sql))
            }
        }
    }

    async fn exec(&mut self, sql: &str, params: &[Value]) -> Result<u64, Fail> {
        let stmt = self.statement(sql).await?;
        let timeout = self.pool.query_timeout + Duration::from_secs(1);
        let refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let conn = self.conn()?;
        match tokio::time::timeout(timeout, conn.client.execute(&stmt, &refs)).await {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(e)) => Err(classify(e)),
            Err(_) => {
                self.discard();
                Err(Fail::deadline(sql))
            }
        }
    }

    async fn batch(&mut self, sql: &str) -> Result<(), Fail> {
        let timeout = self.pool.query_timeout + Duration::from_secs(1);
        let conn = self.conn()?;
        match tokio::time::timeout(timeout, conn.client.batch_execute(sql)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(classify(e)),
            Err(_) => {
                self.discard();
                Err(Fail::deadline(sql))
            }
        }
    }
}

/// The Postgres backend.
pub(crate) struct Pg {
    primary: Arc<Pool>,
    replica: Option<Arc<Pool>>,
}

impl Pg {
    /// Connects, checks the server and applies pending migrations.
    pub(crate) async fn open(opts: &PgOptions) -> Result<Pg, Fail> {
        let primary = Pool::new(&opts.url, opts, false)?;
        let replica = match &opts.replica_url {
            Some(u) => Some(Pool::new(u, opts, true)?),
            None => None,
        };
        let pg = Pg { primary, replica };
        pg.migrate().await?;
        Ok(pg)
    }

    async fn migrate(&self) -> Result<(), Fail> {
        let mut lease = self.primary.get().await?;
        let version = lease
            .query("SELECT current_setting('server_version_num')::bigint", &[])
            .await?;
        let num = version.first().map(|r| r.i64(0)).transpose()?.unwrap_or(0);
        if num < 150_000 {
            return Err(Fail::Err(
                bee_core::BeeError::new(
                    bee_core::Reason::Unavailable,
                    format!("Postgres 15 or newer is required (server_version_num {num})"),
                )
                .with_retryable(false),
            ));
        }
        let d = Dialect::Postgres;
        // One migrator at a time per schema: a transaction-scoped advisory lock.
        lease.batch("BEGIN").await?;
        let res = async {
            lease
                .batch("SELECT pg_advisory_xact_lock(7106511853217318913)")
                .await?;
            if let Some(s) = &self.primary.schema {
                lease
                    .batch(&format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(s)))
                    .await?;
            }
            lease.batch(&migrate::schema_table_sql(d)).await?;
            let rows = lease
                .query(
                    "SELECT version, digest FROM bee_schema ORDER BY version",
                    &[],
                )
                .await?;
            let applied = rows
                .iter()
                .map(|r| Ok((r.i64(0)?, r.text(1)?)))
                .collect::<Result<Vec<_>, bee_core::BeeError>>()?;
            for (m, rendered) in migrate::pending(&applied, d)? {
                lease.batch(&rendered).await?;
                lease
                    .exec(
                        "INSERT INTO bee_schema (version, name, digest) VALUES ($1, $2, $3)",
                        &[
                            Value::I64(m.version),
                            Value::from(m.name),
                            Value::Text(migrate::digest(&rendered)),
                        ],
                    )
                    .await?;
            }
            Ok::<(), Fail>(())
        }
        .await;
        match res {
            Ok(()) => {
                lease.batch("COMMIT").await?;
                lease.release();
                Ok(())
            }
            Err(e) => {
                let _ = lease.batch("ROLLBACK").await;
                Err(e)
            }
        }
    }
}

/// An open Postgres transaction (or, for reads, a connection running autocommit statements).
pub(crate) struct PgTx {
    lease: Lease,
    in_txn: bool,
}

impl Backend for Pg {
    type Tx = PgTx;
    const DIALECT: Dialect = Dialect::Postgres;

    async fn begin(&self, mode: Mode) -> Result<PgTx, Fail> {
        let pool = match (mode, &self.replica) {
            (Mode::Replica, Some(r)) => r,
            _ => &self.primary,
        };
        let mut lease = pool.get().await?;
        let begin = match mode {
            Mode::Read | Mode::Replica => None,
            Mode::Write => Some("BEGIN ISOLATION LEVEL READ COMMITTED"),
            Mode::Serializable => Some("BEGIN ISOLATION LEVEL SERIALIZABLE"),
        };
        if let Some(sql) = begin {
            lease.batch(sql).await?;
        }
        Ok(PgTx {
            lease,
            in_txn: begin.is_some(),
        })
    }

    fn has_replica(&self) -> bool {
        self.replica.is_some()
    }
}

impl Tx for PgTx {
    async fn query(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Row>, Fail> {
        self.lease.query(sql, params).await
    }

    async fn exec(&mut self, sql: &str, params: &[Value]) -> Result<u64, Fail> {
        self.lease.exec(sql, params).await
    }

    async fn commit(mut self) -> Result<(), Fail> {
        if self.in_txn {
            self.lease.batch("COMMIT").await?;
        }
        self.lease.release();
        Ok(())
    }

    async fn rollback(mut self) {
        if self.in_txn {
            if self.lease.batch("ROLLBACK").await.is_ok() {
                self.lease.release();
            }
        } else {
            self.lease.release();
        }
    }
}
