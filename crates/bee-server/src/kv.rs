//! Server-owned state behind one small transactional key-value trait.
//!
//! The metadata store ([`bee_store::MetaStore`]) owns everything SPEC §6 defines. The server
//! keeps a few tables of its own: principals, service credentials, revocations, budget usage,
//! the hash-chained audit log, coalescer owner leases, imports and redaction lifecycle rows.
//! They all reduce to namespaced keys and one atomic primitive, [`Kv::apply`]: check every
//! expectation and, only if all hold, apply every write in one transaction.
//!
//! [`SqliteKv`] keeps the table in its own SQLite file (dev mode and single-node servers);
//! [`PgKv`] keeps it in the Postgres database next to the metadata tables.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bee_core::{BeeError, Reason, Result};

/// One step of an atomic [`Kv::apply`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvOp {
    /// The key must currently hold `value` (`None` = absent).
    Expect {
        /// Namespace.
        ns: String,
        /// Key.
        key: Vec<u8>,
        /// Expected value.
        value: Option<Vec<u8>>,
    },
    /// Write `value` under the key.
    Put {
        /// Namespace.
        ns: String,
        /// Key.
        key: Vec<u8>,
        /// New value.
        value: Vec<u8>,
    },
    /// Remove the key.
    Delete {
        /// Namespace.
        ns: String,
        /// Key.
        key: Vec<u8>,
    },
}

impl KvOp {
    /// An expectation.
    pub fn expect(ns: &str, key: impl Into<Vec<u8>>, value: Option<Vec<u8>>) -> KvOp {
        KvOp::Expect {
            ns: ns.into(),
            key: key.into(),
            value,
        }
    }

    /// A write.
    pub fn put(ns: &str, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> KvOp {
        KvOp::Put {
            ns: ns.into(),
            key: key.into(),
            value: value.into(),
        }
    }

    /// A delete.
    pub fn delete(ns: &str, key: impl Into<Vec<u8>>) -> KvOp {
        KvOp::Delete {
            ns: ns.into(),
            key: key.into(),
        }
    }
}

/// Transactional key-value storage for server-owned tables.
#[async_trait]
pub trait Kv: Send + Sync {
    /// Reads one key.
    async fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Keys of `ns` starting with `prefix`, ordered bytewise, after `after` (exclusive), at
    /// most `limit`.
    async fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;

    /// Applies `ops` atomically: returns `false` (and writes nothing) when an expectation
    /// does not hold.
    async fn apply(&self, ops: Vec<KvOp>) -> Result<bool>;

    /// Writes one key unconditionally.
    async fn put(&self, ns: &str, key: &[u8], value: Vec<u8>) -> Result<()> {
        self.apply(vec![KvOp::put(ns, key, value)])
            .await
            .map(|_| ())
    }
}

fn unavailable(what: impl std::fmt::Display) -> BeeError {
    BeeError::new(
        Reason::Unavailable,
        format!("server store unavailable: {what}"),
    )
}

// ----- SQLite ---------------------------------------------------------------------------

/// [`Kv`] on a SQLite file (WAL, `synchronous = FULL`, one writer connection).
pub struct SqliteKv {
    conn: Arc<Mutex<rusqlite::Connection>>,
    /// Reads use their own connection so they never wait behind a commit's fsync (WAL).
    reader: Arc<Mutex<rusqlite::Connection>>,
}

impl SqliteKv {
    /// Opens (creating) the database file.
    pub fn open(path: &Path) -> Result<SqliteKv> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(unavailable)?;
        }
        let conn = rusqlite::Connection::open(path).map_err(unavailable)?;
        conn.busy_timeout(Duration::from_secs(10))
            .map_err(unavailable)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS bee_server_kv (
                 ns TEXT NOT NULL, k BLOB NOT NULL, v BLOB NOT NULL, PRIMARY KEY (ns, k));",
        )
        .map_err(unavailable)?;
        let reader = rusqlite::Connection::open(path).map_err(unavailable)?;
        reader
            .busy_timeout(Duration::from_secs(10))
            .map_err(unavailable)?;
        Ok(SqliteKv {
            conn: Arc::new(Mutex::new(conn)),
            reader: Arc::new(Mutex::new(reader)),
        })
    }

    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T> {
        Self::on(self.conn.clone(), f).await
    }

    async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T> {
        Self::on(self.reader.clone(), f).await
    }

    async fn on<T: Send + 'static>(
        conn: Arc<Mutex<rusqlite::Connection>>,
        f: impl FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T> {
        tokio::task::spawn_blocking(move || {
            let mut g = conn.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut g)
        })
        .await
        .map_err(unavailable)?
        .map_err(unavailable)
    }
}

#[async_trait]
impl Kv for SqliteKv {
    async fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (ns, key) = (ns.to_string(), key.to_vec());
        self.read(move |c| {
            let mut st =
                c.prepare_cached("SELECT v FROM bee_server_kv WHERE ns = ?1 AND k = ?2")?;
            let mut rows = st.query(rusqlite::params![ns, key])?;
            match rows.next()? {
                Some(r) => Ok(Some(r.get(0)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let (ns, prefix) = (ns.to_string(), prefix.to_vec());
        let after = after.map(<[u8]>::to_vec);
        self.read(move |c| {
            let lower = match &after {
                Some(a) if a.as_slice() >= prefix.as_slice() => a.clone(),
                _ => prefix.clone(),
            };
            let strict = after
                .as_ref()
                .is_some_and(|a| a.as_slice() >= prefix.as_slice());
            let sql = if strict {
                "SELECT k, v FROM bee_server_kv WHERE ns = ?1 AND k > ?2 ORDER BY k LIMIT ?3"
            } else {
                "SELECT k, v FROM bee_server_kv WHERE ns = ?1 AND k >= ?2 ORDER BY k LIMIT ?3"
            };
            let mut st = c.prepare_cached(sql)?;
            let mut out = Vec::new();
            let mut rows = st.query(rusqlite::params![ns, lower, limit as i64 + 1])?;
            while let Some(r) = rows.next()? {
                let k: Vec<u8> = r.get(0)?;
                if !k.starts_with(&prefix) {
                    break;
                }
                out.push((k, r.get(1)?));
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }

    async fn apply(&self, ops: Vec<KvOp>) -> Result<bool> {
        self.run(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            for op in &ops {
                if let KvOp::Expect { ns, key, value } = op {
                    let cur: Option<Vec<u8>> = {
                        let mut st = tx.prepare_cached(
                            "SELECT v FROM bee_server_kv WHERE ns = ?1 AND k = ?2",
                        )?;
                        let mut rows = st.query(rusqlite::params![ns, key])?;
                        match rows.next()? {
                            Some(r) => Some(r.get(0)?),
                            None => None,
                        }
                    };
                    if &cur != value {
                        return Ok(false);
                    }
                }
            }
            for op in &ops {
                match op {
                    KvOp::Expect { .. } => {}
                    KvOp::Put { ns, key, value } => {
                        tx.prepare_cached(
                            "INSERT INTO bee_server_kv (ns, k, v) VALUES (?1, ?2, ?3)
                             ON CONFLICT (ns, k) DO UPDATE SET v = excluded.v",
                        )?
                        .execute(rusqlite::params![ns, key, value])?;
                    }
                    KvOp::Delete { ns, key } => {
                        tx.prepare_cached("DELETE FROM bee_server_kv WHERE ns = ?1 AND k = ?2")?
                            .execute(rusqlite::params![ns, key])?;
                    }
                }
            }
            tx.commit()?;
            Ok(true)
        })
        .await
    }
}

// ----- Postgres -------------------------------------------------------------------------

/// Advisory lock key of `bee_server_kv` creation ("trellkv1").
const KV_CREATE_LOCK: i64 = 0x7472_656c_6c6b_7631;

/// [`Kv`] on Postgres: table `bee_server_kv` in the configured schema, SERIALIZABLE
/// transactions retried on serialization failures.
pub struct PgKv {
    clients: Vec<tokio::sync::Mutex<tokio_postgres::Client>>,
    next: std::sync::atomic::AtomicUsize,
}

impl PgKv {
    /// Connects `connections` clients and creates the table if needed.
    pub async fn connect(url: &str, schema: Option<&str>, connections: usize) -> Result<PgKv> {
        let ident = schema.map(|s| format!("\"{}\"", s.replace('"', "\"\"")));
        let mut clients = Vec::new();
        for _ in 0..connections.max(1) {
            let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
                .await
                .map_err(unavailable)?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            if let Some(ident) = &ident {
                // A schema that does not exist yet may be on the search path; it is created
                // below, before the first query that needs it.
                client
                    .batch_execute(&format!("SET search_path TO {ident}"))
                    .await
                    .map_err(unavailable)?;
            }
            clients.push(tokio::sync::Mutex::new(client));
        }
        // Several server replicas starting at once on one database would race here:
        // concurrent `CREATE ... IF NOT EXISTS` of the same schema or table can fail in
        // Postgres's catalogs with a unique violation (seen with three replicas). One creator
        // at a time, under a transaction-scoped advisory lock, as the metadata and forge
        // migrators do.
        let create_schema = ident
            .as_ref()
            .map(|i| format!("CREATE SCHEMA IF NOT EXISTS {i}; "))
            .unwrap_or_default();
        let first = clients[0].lock().await;
        let created = first
            .batch_execute(&format!(
                "BEGIN; SELECT pg_advisory_xact_lock({KV_CREATE_LOCK}); {create_schema}\
                 CREATE TABLE IF NOT EXISTS bee_server_kv (
                     ns TEXT NOT NULL, k BYTEA NOT NULL, v BYTEA NOT NULL, PRIMARY KEY (ns, k)); \
                 COMMIT"
            ))
            .await;
        if let Err(e) = created {
            let _ = first.batch_execute("ROLLBACK").await;
            return Err(unavailable(e));
        }
        drop(first);
        Ok(PgKv {
            clients,
            next: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn client(&self) -> &tokio::sync::Mutex<tokio_postgres::Client> {
        let i = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        &self.clients[i % self.clients.len()]
    }
}

fn is_retry(e: &tokio_postgres::Error) -> bool {
    e.code().is_some_and(|c| {
        *c == tokio_postgres::error::SqlState::T_R_SERIALIZATION_FAILURE
            || *c == tokio_postgres::error::SqlState::T_R_DEADLOCK_DETECTED
            || *c == tokio_postgres::error::SqlState::UNIQUE_VIOLATION
    })
}

#[async_trait]
impl Kv for PgKv {
    async fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let c = self.client().lock().await;
        let row = c
            .query_opt(
                "SELECT v FROM bee_server_kv WHERE ns = $1 AND k = $2",
                &[&ns, &key],
            )
            .await
            .map_err(unavailable)?;
        Ok(row.map(|r| r.get(0)))
    }

    async fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let c = self.client().lock().await;
        let (lower, strict) = match after {
            Some(a) if a >= prefix => (a.to_vec(), true),
            _ => (prefix.to_vec(), false),
        };
        let sql = if strict {
            "SELECT k, v FROM bee_server_kv WHERE ns = $1 AND k > $2 ORDER BY k LIMIT $3"
        } else {
            "SELECT k, v FROM bee_server_kv WHERE ns = $1 AND k >= $2 ORDER BY k LIMIT $3"
        };
        let rows = c
            .query(sql, &[&ns, &lower, &((limit as i64) + 1)])
            .await
            .map_err(unavailable)?;
        let mut out = Vec::new();
        for r in rows {
            let k: Vec<u8> = r.get(0);
            if !k.starts_with(prefix) || out.len() >= limit {
                break;
            }
            out.push((k, r.get(1)));
        }
        Ok(out)
    }

    async fn apply(&self, ops: Vec<KvOp>) -> Result<bool> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let mut c = self.client().lock().await;
            let res: std::result::Result<bool, tokio_postgres::Error> = async {
                let tx = c
                    .build_transaction()
                    .isolation_level(tokio_postgres::IsolationLevel::Serializable)
                    .start()
                    .await?;
                for op in &ops {
                    if let KvOp::Expect { ns, key, value } = op {
                        let cur: Option<Vec<u8>> = tx
                            .query_opt(
                                "SELECT v FROM bee_server_kv WHERE ns = $1 AND k = $2",
                                &[ns, key],
                            )
                            .await?
                            .map(|r| r.get(0));
                        if &cur != value {
                            tx.rollback().await?;
                            return Ok(false);
                        }
                    }
                }
                for op in &ops {
                    match op {
                        KvOp::Expect { .. } => {}
                        KvOp::Put { ns, key, value } => {
                            tx.execute(
                                "INSERT INTO bee_server_kv (ns, k, v) VALUES ($1, $2, $3)
                                 ON CONFLICT (ns, k) DO UPDATE SET v = excluded.v",
                                &[ns, key, value],
                            )
                            .await?;
                        }
                        KvOp::Delete { ns, key } => {
                            tx.execute(
                                "DELETE FROM bee_server_kv WHERE ns = $1 AND k = $2",
                                &[ns, key],
                            )
                            .await?;
                        }
                    }
                }
                tx.commit().await?;
                Ok(true)
            }
            .await;
            match res {
                Ok(v) => return Ok(v),
                Err(e) if is_retry(&e) && attempt < 10 => {
                    drop(c);
                    tokio::time::sleep(Duration::from_millis(2u64 << attempt.min(6))).await;
                }
                Err(e) => return Err(unavailable(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replicas starting at once on an empty database all open the store (the schema and
    /// table are created once, under the advisory lock). Runs when `BEE_TEST_PG` names a
    /// Postgres database.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn pg_replicas_starting_at_once_all_open_the_store() {
        let Ok(url) = std::env::var("BEE_TEST_PG") else {
            return;
        };
        for round in 0..5 {
            let schema = format!("kvrace_{}_{round}", crate::util::random_hex(6));
            let opens: Vec<_> = (0..12)
                .map(|_| {
                    let (url, schema) = (url.clone(), schema.clone());
                    tokio::spawn(async move { PgKv::connect(&url, Some(&schema), 2).await })
                })
                .collect();
            let mut stores = Vec::new();
            for o in opens {
                stores.push(o.await.unwrap().expect("every replica opens the store"));
            }
            assert!(
                stores[0]
                    .apply(vec![KvOp::put("a", b"k".to_vec(), b"v".to_vec())])
                    .await
                    .unwrap()
            );
            for kv in &stores {
                assert_eq!(kv.get("a", b"k").await.unwrap(), Some(b"v".to_vec()));
            }
            let c = stores[0].client().lock().await;
            c.batch_execute(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn sqlite_apply_is_atomic_and_scan_orders() {
        let dir = tempfile::tempdir().unwrap();
        let kv = SqliteKv::open(&dir.path().join("kv.db")).unwrap();
        assert!(
            kv.apply(vec![KvOp::put("a", b"k1".to_vec(), b"v1".to_vec())])
                .await
                .unwrap()
        );
        // A failed expectation writes nothing.
        let ok = kv
            .apply(vec![
                KvOp::expect("a", b"k1".to_vec(), Some(b"other".to_vec())),
                KvOp::put("a", b"k2".to_vec(), b"v2".to_vec()),
            ])
            .await
            .unwrap();
        assert!(!ok);
        assert_eq!(kv.get("a", b"k2").await.unwrap(), None);
        assert!(
            kv.apply(vec![
                KvOp::expect("a", b"k1".to_vec(), Some(b"v1".to_vec())),
                KvOp::expect("a", b"k2".to_vec(), None),
                KvOp::put("a", b"k2".to_vec(), b"v2".to_vec()),
                KvOp::put("b", b"k2".to_vec(), b"other-ns".to_vec()),
            ])
            .await
            .unwrap()
        );
        let all = kv.scan("a", b"k", None, 10).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, b"k1");
        let after = kv.scan("a", b"k", Some(b"k1"), 10).await.unwrap();
        assert_eq!(after.len(), 1);
        assert!(
            kv.apply(vec![KvOp::delete("a", b"k1".to_vec())])
                .await
                .unwrap()
        );
        assert_eq!(kv.get("a", b"k1").await.unwrap(), None);
    }
}
