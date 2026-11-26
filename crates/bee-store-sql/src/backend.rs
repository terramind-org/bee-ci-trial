//! The narrow interface the shared store logic needs from a database: begin a transaction,
//! run statements with [`Value`] parameters, commit or roll back.
//!
//! SQL text uses Postgres-style `$n` placeholders on both backends (SQLite binds them by
//! name). Statements written with `= ANY($n)` take an array parameter on Postgres; on SQLite
//! [`Dialect::per_element`] rewrites them to `= $n` and the caller runs them once per element.

use std::future::Future;

use bee_core::{BeeError, Reason};

/// A parameter or result value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    /// SQL NULL.
    Null,
    /// A 64-bit integer.
    I64(i64),
    /// Bytes.
    Bytes(Vec<u8>),
    /// Text.
    Text(String),
    /// An array of nullable integers (Postgres only).
    I64s(Vec<Option<i64>>),
    /// An array of nullable byte strings (Postgres only).
    Bytess(Vec<Option<Vec<u8>>>),
    /// An array of nullable strings (Postgres only).
    Texts(Vec<Option<String>>),
}

impl From<i64> for Value {
    fn from(v: i64) -> Value {
        Value::I64(v)
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Value {
        Value::Bytes(v)
    }
}

impl From<&[u8]> for Value {
    fn from(v: &[u8]) -> Value {
        Value::Bytes(v.to_vec())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::Text(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::Text(v.to_string())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        v.map(Into::into).unwrap_or(Value::Null)
    }
}

/// One result row.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Row(pub(crate) Vec<Value>);

fn bad_column(i: usize, want: &str, got: Option<&Value>) -> BeeError {
    BeeError::new(
        Reason::Internal,
        format!("column {i}: expected {want}, got {got:?}"),
    )
}

impl Row {
    /// An integer column.
    pub(crate) fn i64(&self, i: usize) -> Result<i64, BeeError> {
        match self.0.get(i) {
            Some(Value::I64(v)) => Ok(*v),
            other => Err(bad_column(i, "integer", other)),
        }
    }

    /// A non-negative integer column.
    pub(crate) fn u64(&self, i: usize) -> Result<u64, BeeError> {
        let v = self.i64(i)?;
        u64::try_from(v).map_err(|_| bad_column(i, "non-negative integer", self.0.get(i)))
    }

    /// A bytes column.
    pub(crate) fn bytes(&self, i: usize) -> Result<Vec<u8>, BeeError> {
        match self.0.get(i) {
            Some(Value::Bytes(v)) => Ok(v.clone()),
            other => Err(bad_column(i, "bytes", other)),
        }
    }

    /// A nullable bytes column.
    pub(crate) fn opt_bytes(&self, i: usize) -> Result<Option<Vec<u8>>, BeeError> {
        match self.0.get(i) {
            Some(Value::Null) => Ok(None),
            Some(Value::Bytes(v)) => Ok(Some(v.clone())),
            other => Err(bad_column(i, "bytes or null", other)),
        }
    }

    /// A text column.
    pub(crate) fn text(&self, i: usize) -> Result<String, BeeError> {
        match self.0.get(i) {
            Some(Value::Text(v)) => Ok(v.clone()),
            other => Err(bad_column(i, "text", other)),
        }
    }
}

/// Why a statement or transaction failed.
#[derive(Debug)]
pub(crate) enum Fail {
    /// A serialization failure, deadlock or busy database: the store retries the whole
    /// transaction (SPEC §6.1). Carries the `UNAVAILABLE` error returned when the attempts
    /// run out.
    Retry(BeeError),
    /// Any other error, returned to the caller as is.
    Err(BeeError),
}

impl From<BeeError> for Fail {
    fn from(e: BeeError) -> Fail {
        Fail::Err(e)
    }
}

impl Fail {
    /// A retryable conflict.
    pub(crate) fn conflict(what: impl std::fmt::Display) -> Fail {
        Fail::Retry(BeeError::new(
            Reason::Unavailable,
            format!("metadata transaction conflict: {what}"),
        ))
    }

    /// An internal error.
    pub(crate) fn internal(what: impl std::fmt::Display) -> Fail {
        Fail::Err(BeeError::new(
            Reason::Internal,
            format!("metadata store: {what}"),
        ))
    }

    /// The query ran past its deadline.
    pub(crate) fn deadline(what: impl std::fmt::Display) -> Fail {
        Fail::Err(BeeError::new(
            Reason::DeadlineExceeded,
            format!("metadata query deadline exceeded: {what}"),
        ))
    }

    /// The database could not be reached; the caller may retry.
    pub(crate) fn unavailable(what: impl std::fmt::Display) -> Fail {
        Fail::Err(BeeError::new(
            Reason::Unavailable,
            format!("metadata store unavailable: {what}"),
        ))
    }
}

/// The SQL differences between the two backends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// Postgres 15+.
    Postgres,
    /// SQLite 3.35+.
    Sqlite,
}

impl Dialect {
    /// True when array parameters (`= ANY($n)`, `unnest`) are available.
    pub(crate) fn arrays(self) -> bool {
        matches!(self, Dialect::Postgres)
    }

    /// The row-lock clause, or nothing on SQLite (whose single writer serializes everything).
    pub(crate) fn lock(self, clause: &'static str) -> &'static str {
        match self {
            Dialect::Postgres => clause,
            Dialect::Sqlite => "",
        }
    }

    /// The two-argument maximum function.
    pub(crate) fn greatest(self) -> &'static str {
        match self {
            Dialect::Postgres => "GREATEST",
            Dialect::Sqlite => "MAX",
        }
    }

    /// Rewrites `= ANY($n)` to `= $n` for per-element execution on SQLite.
    pub(crate) fn per_element(self, sql: &str) -> String {
        match self {
            Dialect::Postgres => sql.to_string(),
            Dialect::Sqlite => {
                let mut out = String::with_capacity(sql.len());
                let mut rest = sql;
                while let Some(i) = rest.find("ANY($") {
                    out.push_str(&rest[..i]);
                    let after = &rest[i + 4..];
                    let close = after.find(')').unwrap_or(after.len());
                    out.push_str(&after[..close]);
                    rest = after.get(close + 1..).unwrap_or("");
                }
                out.push_str(rest);
                out
            }
        }
    }
}

/// How a transaction runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Read-only statements on the primary, each its own snapshot (every read method is one
    /// statement over rows that are immutable or read strongly).
    Read,
    /// Read-only statements on the read replica, when one is configured.
    Replica,
    /// A read-write transaction: READ COMMITTED with explicit row locks on Postgres,
    /// `BEGIN IMMEDIATE` on SQLite.
    Write,
    /// A SERIALIZABLE read-write transaction on Postgres (ref CAS and the landed index);
    /// `BEGIN IMMEDIATE` on SQLite, which is serializable by construction.
    Serializable,
}

/// A database the store logic runs on.
pub(crate) trait Backend: Send + Sync + 'static {
    /// An open transaction.
    type Tx: Tx;

    /// The SQL dialect.
    const DIALECT: Dialect;

    /// Begins a transaction.
    fn begin(&self, mode: Mode) -> impl Future<Output = Result<Self::Tx, Fail>> + Send;

    /// True when a read replica is configured.
    fn has_replica(&self) -> bool;
}

/// An open transaction. Dropping it without [`Tx::commit`] rolls it back.
pub(crate) trait Tx: Send {
    /// Runs a statement and returns its rows.
    fn query(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> impl Future<Output = Result<Vec<Row>, Fail>> + Send;

    /// Runs a statement and returns the number of rows it changed.
    fn exec(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> impl Future<Output = Result<u64, Fail>> + Send;

    /// Commits.
    fn commit(self) -> impl Future<Output = Result<(), Fail>> + Send;

    /// Rolls back, ignoring errors (the connection is discarded if rollback fails).
    fn rollback(self) -> impl Future<Output = ()> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_element_rewrites_any() {
        let sql = "SELECT a FROM t WHERE k = $1 AND id = ANY($2) AND x = ANY($3) ORDER BY a";
        assert_eq!(
            Dialect::Sqlite.per_element(sql),
            "SELECT a FROM t WHERE k = $1 AND id = $2 AND x = $3 ORDER BY a"
        );
        assert_eq!(Dialect::Postgres.per_element(sql), sql);
    }

    #[test]
    fn row_accessors_check_types() {
        let r = Row(vec![
            Value::I64(5),
            Value::Null,
            Value::Bytes(vec![1]),
            Value::Text("t".into()),
        ]);
        assert_eq!(r.i64(0).unwrap(), 5);
        assert_eq!(r.u64(0).unwrap(), 5);
        assert_eq!(r.opt_bytes(1).unwrap(), None);
        assert_eq!(r.bytes(2).unwrap(), vec![1]);
        assert_eq!(r.text(3).unwrap(), "t");
        assert!(r.i64(2).is_err());
        assert!(r.text(9).is_err());
        assert!(Row(vec![Value::I64(-1)]).u64(0).is_err());
    }
}
