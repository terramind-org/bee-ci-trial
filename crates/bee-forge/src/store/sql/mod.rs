//! SQL backends: SQLite (dev mode, always available) and Postgres (production).
//!
//! Both share one statement builder and one transaction implementation ([`SqlTx`]); a
//! backend only supplies an [`Exec`] that runs a statement with typed parameters and
//! returns typed columns.
//!
//! # Separation from the VCS store
//!
//! The forge database is configured separately from the VCS metadata store
//! ([`SqlConfig`]): SQLite uses its own directory with one file per surface
//! (`forge-<surface>.db`), Postgres its own schema (default `bee_forge`) with one bounded
//! connection pool per surface. Nothing in the VCS data path opens these.
//!
//! # Deadlines
//!
//! Every statement runs under the transaction's deadline. Postgres additionally sets
//! `statement_timeout` for the transaction; SQLite bounds lock waits with `busy_timeout`.

mod postgres;
mod sqlite;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use prost::Message;
use tokio::time::Instant;

use super::{
    ForgeStore, ForgeTx, Index, Key, Migration, MigrationOp, MigrationReport, Order, OutboxRecord,
    Phase, Row, Scan, Surface, TableDef, check_surface, migrate, within,
};
use crate::error::{ForgeError, Result};

/// Default connections per surface.
pub const DEFAULT_POOL_SIZE: usize = 4;
/// Default Postgres schema for forge tables.
pub const DEFAULT_SCHEMA: &str = "bee_forge";

/// Which database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SqlBackend {
    /// A directory holding one SQLite file per surface.
    Sqlite {
        /// Directory (created if missing).
        dir: PathBuf,
    },
    /// A Postgres database and the schema the forge tables live in.
    Postgres {
        /// libpq-style connection string or `postgres://` URL.
        url: String,
        /// Schema name (created if missing).
        schema: String,
    },
}

/// Forge database configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlConfig {
    /// The database.
    pub backend: SqlBackend,
    /// Connections per surface unless overridden.
    pub default_pool_size: usize,
    /// Per-surface pool sizes.
    pub pool_sizes: BTreeMap<String, usize>,
    /// Whether contract migrations may run.
    pub allow_contract: bool,
    /// SQLite lock wait bound in milliseconds.
    pub busy_timeout_ms: u64,
}

impl SqlConfig {
    /// SQLite files under `dir`.
    pub fn sqlite(dir: impl Into<PathBuf>) -> SqlConfig {
        SqlConfig::with(SqlBackend::Sqlite { dir: dir.into() })
    }

    /// Postgres at `url`, tables in `schema`.
    pub fn postgres(url: impl Into<String>, schema: impl Into<String>) -> SqlConfig {
        SqlConfig::with(SqlBackend::Postgres {
            url: url.into(),
            schema: schema.into(),
        })
    }

    fn with(backend: SqlBackend) -> SqlConfig {
        SqlConfig {
            backend,
            default_pool_size: DEFAULT_POOL_SIZE,
            pool_sizes: BTreeMap::new(),
            allow_contract: false,
            busy_timeout_ms: 5_000,
        }
    }

    /// Parses `sqlite:<dir>` or a `postgres://` URL with an optional `schema=<name>` query
    /// parameter (removed before connecting).
    pub fn parse(url: &str) -> Result<SqlConfig> {
        if let Some(dir) = url.strip_prefix("sqlite:") {
            if dir.is_empty() {
                return Err(ForgeError::invalid(
                    "bad_config",
                    "sqlite: needs a directory",
                ));
            }
            return Ok(SqlConfig::sqlite(dir));
        }
        if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let (base, query) = url.split_once('?').unwrap_or((url, ""));
            let mut schema = DEFAULT_SCHEMA.to_string();
            let mut rest = Vec::new();
            for kv in query.split('&').filter(|s| !s.is_empty()) {
                match kv.split_once('=') {
                    Some(("schema", v)) => schema = v.to_string(),
                    _ => rest.push(kv),
                }
            }
            let url = if rest.is_empty() {
                base.to_string()
            } else {
                format!("{base}?{}", rest.join("&"))
            };
            return Ok(SqlConfig::postgres(url, schema));
        }
        Err(ForgeError::invalid(
            "bad_config",
            "forge database URL must start with sqlite: or postgres://",
        ))
    }

    /// Pool size of a surface.
    pub fn pool_size(&self, s: Surface) -> usize {
        self.pool_sizes
            .get(s.0)
            .copied()
            .unwrap_or(self.default_pool_size)
            .max(1)
    }
}

/// Whether `s` is a safe SQL identifier.
pub(crate) fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !s.as_bytes()[0].is_ascii_digit()
}

/// SQL flavour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dialect {
    Sqlite,
    Postgres,
}

impl Dialect {
    fn p(self, i: usize) -> String {
        match self {
            Dialect::Sqlite => format!("?{i}"),
            Dialect::Postgres => format!("${i}"),
        }
    }

    fn blob(self) -> &'static str {
        match self {
            Dialect::Sqlite => "BLOB",
            Dialect::Postgres => "BYTEA",
        }
    }
}

/// A typed parameter or column value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Val {
    Text(String),
    OptText(Option<String>),
    Int(i64),
    OptInt(Option<i64>),
    Blob(Vec<u8>),
}

/// Expected column type of a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Col {
    Text,
    OptText,
    Int,
    OptInt,
    Blob,
}

const ROW_COLS: &[Col] = &[
    Col::Text,
    Col::Text,
    Col::OptText,
    Col::OptText,
    Col::OptInt,
    Col::Int,
    Col::Blob,
];
const ROW_SELECT: &str = "scope, id, k1, k2, n1, version, body";

fn take_text(v: &mut std::vec::IntoIter<Val>) -> Result<String> {
    match v.next() {
        Some(Val::Text(s)) => Ok(s),
        other => Err(ForgeError::internal(format!(
            "expected text, got {other:?}"
        ))),
    }
}

fn take_opt_text(v: &mut std::vec::IntoIter<Val>) -> Result<Option<String>> {
    match v.next() {
        Some(Val::OptText(s)) => Ok(s),
        other => Err(ForgeError::internal(format!(
            "expected nullable text, got {other:?}"
        ))),
    }
}

fn take_int(v: &mut std::vec::IntoIter<Val>) -> Result<i64> {
    match v.next() {
        Some(Val::Int(i)) => Ok(i),
        other => Err(ForgeError::internal(format!(
            "expected integer, got {other:?}"
        ))),
    }
}

fn take_opt_int(v: &mut std::vec::IntoIter<Val>) -> Result<Option<i64>> {
    match v.next() {
        Some(Val::OptInt(i)) => Ok(i),
        other => Err(ForgeError::internal(format!(
            "expected nullable integer, got {other:?}"
        ))),
    }
}

fn take_blob(v: &mut std::vec::IntoIter<Val>) -> Result<Vec<u8>> {
    match v.next() {
        Some(Val::Blob(b)) => Ok(b),
        other => Err(ForgeError::internal(format!(
            "expected bytes, got {other:?}"
        ))),
    }
}

fn to_row(vals: Vec<Val>) -> Result<Row> {
    let mut it = vals.into_iter();
    Ok(Row {
        scope: take_text(&mut it)?,
        id: take_text(&mut it)?,
        k1: take_opt_text(&mut it)?,
        k2: take_opt_text(&mut it)?,
        n1: take_opt_int(&mut it)?,
        version: u64::try_from(take_int(&mut it)?).unwrap_or(0),
        body: take_blob(&mut it)?,
    })
}

fn version_param(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| ForgeError::invalid("bad_version", "version out of range"))
}

/// Runs statements for one open transaction (or, during migration, one connection).
#[async_trait]
pub(crate) trait Exec: Send {
    /// Runs a query and returns its rows, typed as `cols`.
    async fn query(&mut self, sql: &str, params: Vec<Val>, cols: &[Col]) -> Result<Vec<Vec<Val>>>;
    /// Runs a statement and returns the affected row count.
    async fn execute(&mut self, sql: &str, params: Vec<Val>) -> Result<u64>;
    /// Runs statements without parameters (DDL, `BEGIN`, ...).
    async fn batch(&mut self, sql: &str) -> Result<()>;
    /// Commits the open transaction.
    async fn commit(&mut self) -> Result<()>;
}

/// DDL of a table in the standard shape.
pub(crate) fn create_table_sql(d: Dialect, t: &TableDef) -> String {
    let mut s = format!(
        "CREATE TABLE IF NOT EXISTS {name} (scope TEXT NOT NULL, id TEXT NOT NULL, k1 TEXT, \
         k2 TEXT, n1 BIGINT, version BIGINT NOT NULL, body {blob} NOT NULL, \
         PRIMARY KEY (scope, id));\n",
        name = t.name,
        blob = d.blob()
    );
    if t.unique_k1 {
        s.push_str(&format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS {n}_uk1 ON {n} (scope, k1);\n",
            n = t.name
        ));
    }
    if t.unique_k2 {
        s.push_str(&format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS {n}_uk2 ON {n} (scope, k2);\n",
            n = t.name
        ));
    }
    for i in t.indexes {
        s.push_str(&create_index_sql(t, *i));
        s.push('\n');
    }
    s
}

pub(crate) fn create_index_sql(t: &TableDef, i: Index) -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS {n}_{sfx} ON {n} ({cols});",
        n = t.name,
        sfx = i.suffix(),
        cols = i.columns()
    )
}

pub(crate) fn outbox_table(s: Surface) -> String {
    format!("forge_outbox_{}", s.0)
}

fn migrations_ddl(d: Dialect) -> String {
    let _ = d;
    "CREATE TABLE IF NOT EXISTS forge_migrations (id TEXT PRIMARY KEY, module TEXT NOT NULL, \
     version BIGINT NOT NULL, name TEXT NOT NULL, surface TEXT NOT NULL, checksum TEXT NOT NULL, \
     applied_at_ms BIGINT NOT NULL);"
        .to_string()
}

fn outbox_ddl(d: Dialect, s: Surface) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {} (id TEXT PRIMARY KEY, body {} NOT NULL);",
        outbox_table(s),
        d.blob()
    )
}

/// Applies the migrations touching one surface over an exec that already holds that
/// surface's migration lock and an open transaction.
pub(crate) async fn migrate_surface(
    exec: &mut dyn Exec,
    d: Dialect,
    surface: Surface,
    migrations: &[&Migration],
    allow_contract: bool,
    now_ms: u64,
    report: &mut MigrationReport,
) -> Result<()> {
    exec.batch(&migrations_ddl(d)).await?;
    exec.batch(&outbox_ddl(d, surface)).await?;
    let applied = exec
        .query(
            &format!(
                "SELECT id, checksum FROM forge_migrations WHERE surface = {}",
                d.p(1)
            ),
            vec![Val::Text(surface.0.to_string())],
            &[Col::Text, Col::Text],
        )
        .await?;
    let applied: BTreeMap<String, String> = applied
        .into_iter()
        .filter_map(|r| {
            let mut it = r.into_iter();
            match (it.next(), it.next()) {
                (Some(Val::Text(a)), Some(Val::Text(b))) => Some((a, b)),
                _ => None,
            }
        })
        .collect();
    for m in migrations {
        if !m.surfaces().contains(&surface) {
            continue;
        }
        let key = format!("{}@{}", m.id(), surface);
        let sum = m.checksum();
        if let Some(prev) = applied.get(&key) {
            if *prev != sum {
                return Err(ForgeError::internal(format!(
                    "migration {key} was applied with a different checksum; applied migrations \
                     are immutable"
                )));
            }
            report.already_applied.push(key);
            continue;
        }
        if m.phase == Phase::Contract && !allow_contract {
            report.pending_contract.push(key);
            continue;
        }
        for op in m.ops.iter().filter(|o| o.surface() == surface) {
            let sql = match op {
                MigrationOp::CreateTable(t) => create_table_sql(d, t),
                MigrationOp::CreateIndex(t, i) => create_index_sql(t, *i),
                MigrationOp::Sql {
                    sqlite, postgres, ..
                } => match d {
                    Dialect::Sqlite => sqlite.to_string(),
                    Dialect::Postgres => postgres.to_string(),
                },
            };
            exec.batch(&sql).await?;
        }
        exec.execute(
            &format!(
                "INSERT INTO forge_migrations (id, module, version, name, surface, checksum, \
                 applied_at_ms) VALUES ({}, {}, {}, {}, {}, {}, {})",
                d.p(1),
                d.p(2),
                d.p(3),
                d.p(4),
                d.p(5),
                d.p(6),
                d.p(7)
            ),
            vec![
                Val::Text(key.clone()),
                Val::Text(m.module.to_string()),
                Val::Int(i64::from(m.version)),
                Val::Text(m.name.to_string()),
                Val::Text(surface.0.to_string()),
                Val::Text(sum),
                Val::Int(i64::try_from(now_ms).unwrap_or(i64::MAX)),
            ],
        )
        .await?;
        report.applied.push(key);
    }
    Ok(())
}

/// A transaction over any [`Exec`].
pub(crate) struct SqlTx<E: Exec> {
    pub(crate) exec: E,
    pub(crate) dialect: Dialect,
    pub(crate) surface: Surface,
    pub(crate) deadline: Instant,
}

impl<E: Exec> SqlTx<E> {
    fn p(&self, i: usize) -> String {
        self.dialect.p(i)
    }

    async fn rows(&mut self, sql: String, params: Vec<Val>) -> Result<Vec<Row>> {
        let deadline = self.deadline;
        let raw = within(deadline, self.exec.query(&sql, params, ROW_COLS)).await?;
        raw.into_iter().map(to_row).collect()
    }

    async fn exec(&mut self, sql: String, params: Vec<Val>) -> Result<u64> {
        let deadline = self.deadline;
        within(deadline, self.exec.execute(&sql, params)).await
    }

    async fn current_version(
        &mut self,
        t: &TableDef,
        scope: &str,
        id: &str,
    ) -> Result<Option<u64>> {
        let sql = format!(
            "SELECT {ROW_SELECT} FROM {} WHERE scope = {} AND id = {}",
            t.name,
            self.p(1),
            self.p(2)
        );
        let rows = self
            .rows(sql, vec![Val::Text(scope.into()), Val::Text(id.into())])
            .await?;
        Ok(rows.first().map(|r| r.version))
    }

    fn row_params(row: &Row) -> Vec<Val> {
        vec![
            Val::Text(row.scope.clone()),
            Val::Text(row.id.clone()),
            Val::OptText(row.k1.clone()),
            Val::OptText(row.k2.clone()),
            Val::OptInt(row.n1),
            Val::Blob(row.body.clone()),
        ]
    }
}

#[async_trait]
impl<E: Exec> ForgeTx for SqlTx<E> {
    fn surface(&self) -> Surface {
        self.surface
    }

    async fn get(&mut self, t: &TableDef, scope: &str, id: &str) -> Result<Option<Row>> {
        check_surface(self.surface, t)?;
        let sql = format!(
            "SELECT {ROW_SELECT} FROM {} WHERE scope = {} AND id = {}",
            t.name,
            self.p(1),
            self.p(2)
        );
        Ok(self
            .rows(sql, vec![Val::Text(scope.into()), Val::Text(id.into())])
            .await?
            .into_iter()
            .next())
    }

    async fn get_for_update(&mut self, t: &TableDef, scope: &str, id: &str) -> Result<Option<Row>> {
        check_surface(self.surface, t)?;
        let lock = if self.dialect == Dialect::Postgres {
            " FOR UPDATE"
        } else {
            ""
        };
        let sql = format!(
            "SELECT {ROW_SELECT} FROM {} WHERE scope = {} AND id = {}{lock}",
            t.name,
            self.p(1),
            self.p(2)
        );
        Ok(self
            .rows(sql, vec![Val::Text(scope.into()), Val::Text(id.into())])
            .await?
            .into_iter()
            .next())
    }

    async fn insert(&mut self, t: &TableDef, row: &Row) -> Result<()> {
        check_surface(self.surface, t)?;
        let sql = format!(
            "INSERT INTO {} (scope, id, k1, k2, n1, body, version) VALUES ({}, {}, {}, {}, {}, {}, 1)",
            t.name,
            self.p(1),
            self.p(2),
            self.p(3),
            self.p(4),
            self.p(5),
            self.p(6)
        );
        self.exec(sql, Self::row_params(row)).await.map(|_| ())
    }

    async fn update(&mut self, t: &TableDef, row: &Row) -> Result<u64> {
        check_surface(self.surface, t)?;
        let sql = format!(
            "UPDATE {} SET k1 = {}, k2 = {}, n1 = {}, body = {}, version = version + 1 \
             WHERE scope = {} AND id = {} AND version = {}",
            t.name,
            self.p(3),
            self.p(4),
            self.p(5),
            self.p(6),
            self.p(1),
            self.p(2),
            self.p(7)
        );
        let mut params = Self::row_params(row);
        params.push(Val::Int(version_param(row.version)?));
        let n = self.exec(sql, params).await?;
        if n == 1 {
            return Ok(row.version + 1);
        }
        match self.current_version(t, &row.scope, &row.id).await? {
            None => Err(ForgeError::not_found(format!("{} row {}", t.name, row.id))),
            Some(cur) => Err(ForgeError::version_mismatch(row.version, cur)),
        }
    }

    async fn put(&mut self, t: &TableDef, row: &Row) -> Result<u64> {
        check_surface(self.surface, t)?;
        let sql = format!(
            "INSERT INTO {n} (scope, id, k1, k2, n1, body, version) VALUES ({}, {}, {}, {}, {}, {}, 1) \
             ON CONFLICT (scope, id) DO UPDATE SET k1 = excluded.k1, k2 = excluded.k2, \
             n1 = excluded.n1, body = excluded.body, version = {n}.version + 1 RETURNING version",
            self.p(1),
            self.p(2),
            self.p(3),
            self.p(4),
            self.p(5),
            self.p(6),
            n = t.name,
        );
        let deadline = self.deadline;
        let out = within(
            deadline,
            self.exec.query(&sql, Self::row_params(row), &[Col::Int]),
        )
        .await?;
        match out.into_iter().next().and_then(|r| r.into_iter().next()) {
            Some(Val::Int(v)) => Ok(u64::try_from(v).unwrap_or(0)),
            _ => Err(ForgeError::internal("upsert returned no version")),
        }
    }

    async fn delete(&mut self, t: &TableDef, scope: &str, id: &str) -> Result<bool> {
        check_surface(self.surface, t)?;
        let sql = format!(
            "DELETE FROM {} WHERE scope = {} AND id = {}",
            t.name,
            self.p(1),
            self.p(2)
        );
        Ok(self
            .exec(sql, vec![Val::Text(scope.into()), Val::Text(id.into())])
            .await?
            > 0)
    }

    async fn scan(&mut self, t: &TableDef, q: &Scan) -> Result<Vec<Row>> {
        check_surface(self.surface, t)?;
        let mut wh: Vec<String> = Vec::new();
        let mut params: Vec<Val> = Vec::new();
        let dialect = self.dialect;
        let next = |params: &mut Vec<Val>, v: Val| {
            params.push(v);
            dialect.p(params.len())
        };
        if let Some(s) = &q.scope {
            let p = next(&mut params, Val::Text(s.clone()));
            wh.push(format!("scope = {p}"));
        }
        match &q.key {
            Some(Key::K1(v)) => {
                let p = next(&mut params, Val::Text(v.clone()));
                wh.push(format!("k1 = {p}"));
            }
            Some(Key::K2(v)) => {
                let p = next(&mut params, Val::Text(v.clone()));
                wh.push(format!("k2 = {p}"));
            }
            None => {}
        }
        if q.order == Order::N1 {
            wh.push("n1 IS NOT NULL".into());
        }
        if let Some(m) = q.n1_min {
            let p = next(&mut params, Val::Int(m));
            wh.push(format!("n1 >= {p}"));
        }
        if let Some(m) = q.n1_max {
            let p = next(&mut params, Val::Int(m));
            wh.push(format!("n1 <= {p}"));
        }
        let cmp = if q.descending { "<" } else { ">" };
        let dir = if q.descending { "DESC" } else { "ASC" };
        if let Some(c) = &q.after {
            match q.order {
                Order::Id => {
                    let a = next(&mut params, Val::Text(c.scope.clone()));
                    let b = next(&mut params, Val::Text(c.id.clone()));
                    wh.push(format!("(scope, id) {cmp} ({a}, {b})"));
                }
                Order::N1 => {
                    let a = next(&mut params, Val::Int(c.n1));
                    let b = next(&mut params, Val::Text(c.scope.clone()));
                    let d = next(&mut params, Val::Text(c.id.clone()));
                    wh.push(format!("(n1, scope, id) {cmp} ({a}, {b}, {d})"));
                }
            }
        }
        let order = match q.order {
            Order::Id => format!("scope {dir}, id {dir}"),
            Order::N1 => format!("n1 {dir}, scope {dir}, id {dir}"),
        };
        let where_ = if wh.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", wh.join(" AND "))
        };
        let sql = format!(
            "SELECT {ROW_SELECT} FROM {}{where_} ORDER BY {order} LIMIT {}",
            t.name,
            q.effective_limit()
        );
        self.rows(sql, params).await
    }

    async fn count(&mut self, t: &TableDef, scope: &str, key: Option<&Key>) -> Result<u64> {
        check_surface(self.surface, t)?;
        let mut params = vec![Val::Text(scope.into())];
        let mut sql = format!(
            "SELECT COUNT(*) FROM {} WHERE scope = {}",
            t.name,
            self.p(1)
        );
        match key {
            Some(Key::K1(v)) => {
                params.push(Val::Text(v.clone()));
                sql.push_str(&format!(" AND k1 = {}", self.p(2)));
            }
            Some(Key::K2(v)) => {
                params.push(Val::Text(v.clone()));
                sql.push_str(&format!(" AND k2 = {}", self.p(2)));
            }
            None => {}
        }
        let deadline = self.deadline;
        let out = within(deadline, self.exec.query(&sql, params, &[Col::Int])).await?;
        match out.into_iter().next().and_then(|r| r.into_iter().next()) {
            Some(Val::Int(v)) => Ok(u64::try_from(v).unwrap_or(0)),
            _ => Ok(0),
        }
    }

    async fn add_n1(&mut self, t: &TableDef, scope: &str, id: &str, delta: i64) -> Result<i64> {
        check_surface(self.surface, t)?;
        let sql = format!(
            "INSERT INTO {n} (scope, id, k1, k2, n1, body, version) VALUES ({}, {}, NULL, NULL, {}, {}, 1) \
             ON CONFLICT (scope, id) DO UPDATE SET n1 = COALESCE({n}.n1, 0) + excluded.n1, \
             version = {n}.version + 1 RETURNING n1",
            self.p(1),
            self.p(2),
            self.p(3),
            self.p(4),
            n = t.name,
        );
        let deadline = self.deadline;
        let out = within(
            deadline,
            self.exec.query(
                &sql,
                vec![
                    Val::Text(scope.into()),
                    Val::Text(id.into()),
                    Val::Int(delta),
                    Val::Blob(Vec::new()),
                ],
                &[Col::OptInt],
            ),
        )
        .await?;
        match out.into_iter().next().and_then(|r| r.into_iter().next()) {
            Some(Val::OptInt(Some(v))) => Ok(v),
            _ => Err(ForgeError::internal("counter returned no value")),
        }
    }

    async fn outbox_push(&mut self, id: &str, record: &OutboxRecord) -> Result<()> {
        let sql = format!(
            "INSERT INTO {} (id, body) VALUES ({}, {})",
            outbox_table(self.surface),
            self.p(1),
            self.p(2)
        );
        self.exec(
            sql,
            vec![Val::Text(id.into()), Val::Blob(record.encode_to_vec())],
        )
        .await
        .map(|_| ())
    }

    async fn outbox_peek(&mut self, limit: usize) -> Result<Vec<(String, OutboxRecord)>> {
        let sql = format!(
            "SELECT id, body FROM {} ORDER BY id LIMIT {}",
            outbox_table(self.surface),
            limit.clamp(1, super::MAX_SCAN)
        );
        let deadline = self.deadline;
        let out = within(
            deadline,
            self.exec.query(&sql, Vec::new(), &[Col::Text, Col::Blob]),
        )
        .await?;
        out.into_iter()
            .map(|r| {
                let mut it = r.into_iter();
                let id = take_text(&mut it)?;
                let body = take_blob(&mut it)?;
                Ok((id, OutboxRecord::decode(body.as_slice())?))
            })
            .collect()
    }

    async fn outbox_remove(&mut self, ids: &[String]) -> Result<()> {
        let sql = format!(
            "DELETE FROM {} WHERE id = {}",
            outbox_table(self.surface),
            self.p(1)
        );
        for id in ids {
            self.exec(sql.clone(), vec![Val::Text(id.clone())]).await?;
        }
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<()> {
        let deadline = self.deadline;
        within(deadline, self.exec.commit()).await
    }
}

/// A SQL forge store.
pub struct SqlStore {
    inner: Inner,
}

enum Inner {
    Sqlite(Arc<sqlite::SqliteBackend>),
    Postgres(Arc<postgres::PgBackend>),
}

impl SqlStore {
    /// Opens (and for SQLite creates) the database. No migration runs here; call
    /// [`ForgeStore::migrate`].
    pub async fn open(cfg: SqlConfig, clock: Arc<dyn crate::ids::Clock>) -> Result<SqlStore> {
        let inner = match &cfg.backend {
            SqlBackend::Sqlite { dir } => {
                std::fs::create_dir_all(dir).map_err(|e| {
                    ForgeError::unavailable(format!("cannot create {}: {e}", dir.display()))
                })?;
                Inner::Sqlite(Arc::new(sqlite::SqliteBackend::new(cfg.clone(), clock)))
            }
            SqlBackend::Postgres { schema, .. } => {
                if !ident_ok(schema) {
                    return Err(ForgeError::invalid(
                        "bad_config",
                        format!("schema name {schema:?} is not a plain lower-case identifier"),
                    ));
                }
                Inner::Postgres(Arc::new(postgres::PgBackend::new(cfg.clone(), clock)))
            }
        };
        Ok(SqlStore { inner })
    }

    /// Database connections this store has opened so far, over every surface's pool. A
    /// healthy pool opens at most its size per surface and then reuses them; SQLite reports 0
    /// (it has no network connections).
    pub fn connections_opened(&self) -> u64 {
        match &self.inner {
            Inner::Sqlite(_) => 0,
            Inner::Postgres(b) => b.connections_opened(),
        }
    }
}

#[async_trait]
impl ForgeStore for SqlStore {
    fn backend(&self) -> &'static str {
        match self.inner {
            Inner::Sqlite(_) => "sqlite",
            Inner::Postgres(_) => "postgres",
        }
    }

    async fn migrate(&self, migrations: &[&Migration]) -> Result<MigrationReport> {
        migrate::validate(migrations)?;
        match &self.inner {
            Inner::Sqlite(b) => b.migrate(migrations).await,
            Inner::Postgres(b) => b.migrate(migrations).await,
        }
    }

    async fn begin(&self, surface: Surface, deadline: Instant) -> Result<Box<dyn ForgeTx>> {
        match &self.inner {
            Inner::Sqlite(b) => b.begin(surface, deadline).await,
            Inner::Postgres(b) => b.begin(surface, deadline).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_urls_parse() {
        let c = SqlConfig::parse("sqlite:/tmp/forge").unwrap();
        assert_eq!(
            c.backend,
            SqlBackend::Sqlite {
                dir: "/tmp/forge".into()
            }
        );
        let c = SqlConfig::parse("postgres://u@h:5/db?schema=forge_x&sslmode=disable").unwrap();
        assert_eq!(
            c.backend,
            SqlBackend::Postgres {
                url: "postgres://u@h:5/db?sslmode=disable".into(),
                schema: "forge_x".into()
            }
        );
        assert!(SqlConfig::parse("mysql://x").is_err());
        assert!(ident_ok("bee_forge"));
        assert!(!ident_ok("Robert'); DROP"));
    }
}
