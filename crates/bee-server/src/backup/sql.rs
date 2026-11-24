//! Consistent online snapshots of the server's databases, without external tools.
//!
//! - **SQLite**: `VACUUM INTO` on a separate connection writes a transactionally consistent
//!   copy of a live WAL database; restore places the file.
//! - **Postgres**: one `REPEATABLE READ` read-only transaction exports every table of the
//!   metadata schema (metadata tables and the server store) and the forge schema with
//!   `COPY … TO STDOUT` (text format), so all of them reflect one snapshot. Restore lets
//!   the stores create their schemas (migrations), empties every table, loads the rows with
//!   `COPY … FROM STDIN`, and moves identity sequences past the loaded keys.

use std::path::{Path, PathBuf};

use bee_proto::v1::ErrorReason;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};

use crate::errors::{ApiError, ApiResult};

fn unavailable(what: &str, e: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorReason::Unavailable, format!("{what}: {e}"))
}

/// A consistent copy of the SQLite database at `path`.
pub async fn sqlite_snapshot(path: PathBuf) -> ApiResult<Vec<u8>> {
    tokio::task::spawn_blocking(move || -> ApiResult<Vec<u8>> {
        let tmp_dir = tempdir_near(&path)?;
        let out = tmp_dir.join("snapshot.db");
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| unavailable("open for snapshot", e))?;
        conn.busy_timeout(std::time::Duration::from_secs(30))
            .map_err(|e| unavailable("busy timeout", e))?;
        conn.execute("VACUUM INTO ?1", [out.to_string_lossy().as_ref()])
            .map_err(|e| unavailable("VACUUM INTO", e))?;
        let bytes = std::fs::read(&out).map_err(|e| unavailable("read snapshot", e))?;
        let _ = std::fs::remove_dir_all(&tmp_dir);
        Ok(bytes)
    })
    .await
    .map_err(|e| unavailable("snapshot task", e))?
}

fn tempdir_near(path: &Path) -> ApiResult<PathBuf> {
    let base = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!(".backup-{}", crate::util::random_hex(6)));
    std::fs::create_dir_all(&dir).map_err(|e| unavailable("temp dir", e))?;
    Ok(dir)
}

/// Writes `bytes` to `path` durably (temporary file, sync, rename).
pub fn write_durable(path: &Path, bytes: &[u8]) -> ApiResult<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| unavailable("create dir", e))?;
    }
    let tmp = path.with_extension("restore-tmp");
    std::fs::write(&tmp, bytes).map_err(|e| unavailable("write", e))?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| unavailable("sync", e))?;
    std::fs::rename(&tmp, path).map_err(|e| unavailable("rename", e))?;
    Ok(())
}

/// The forge's SQLite files (`forge-<surface>.db`) in `dir`, by name.
pub fn forge_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("forge-") && name.ends_with(".db") {
                out.push((name, e.path()));
            }
        }
    }
    out.sort();
    out
}

/// Quotes an identifier.
pub fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Connects one client.
pub async fn pg_connect(url: &str) -> ApiResult<tokio_postgres::Client> {
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .map_err(|e| unavailable("postgres connect", e))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(client)
}

/// Base tables of a schema, by name.
async fn tables(
    client: &impl tokio_postgres::GenericClient,
    schema: &str,
) -> ApiResult<Vec<String>> {
    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables
             WHERE table_schema = $1 AND table_type = 'BASE TABLE' ORDER BY table_name",
            &[&schema],
        )
        .await
        .map_err(|e| unavailable("list tables", e))?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

/// One table's rows in `COPY` text format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableDump {
    /// Schema role: `meta` or `forge`.
    pub role: String,
    /// Table name.
    pub table: String,
    /// `COPY TO STDOUT` bytes.
    pub data: Vec<u8>,
}

/// Exports every table of `schemas` (role, schema name) in one snapshot.
pub async fn pg_dump(url: &str, schemas: &[(String, String)]) -> ApiResult<Vec<TableDump>> {
    let mut client = pg_connect(url).await?;
    let tx = client
        .build_transaction()
        .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .map_err(|e| unavailable("begin snapshot", e))?;
    let mut out = Vec::new();
    for (role, schema) in schemas {
        for t in tables(&tx, schema).await? {
            let stmt = format!("COPY {}.{} TO STDOUT", ident(schema), ident(&t));
            let stream = tx
                .copy_out(&stmt)
                .await
                .map_err(|e| unavailable("copy out", e))?;
            let mut stream = Box::pin(stream);
            let mut data = Vec::new();
            while let Some(chunk) = stream.next().await {
                data.extend_from_slice(&chunk.map_err(|e| unavailable("copy out", e))?);
            }
            out.push(TableDump {
                role: role.clone(),
                table: t,
                data,
            });
        }
    }
    tx.commit()
        .await
        .map_err(|e| unavailable("end snapshot", e))?;
    Ok(out)
}

/// Loads `dumps` of `role` into `schema` (whose tables the stores' migrations created):
/// every table of the schema is emptied first, so the result is exactly the backup.
pub async fn pg_load(url: &str, schema: &str, role: &str, dumps: &[TableDump]) -> ApiResult<u64> {
    let mut client = pg_connect(url).await?;
    let tx = client
        .transaction()
        .await
        .map_err(|e| unavailable("begin restore", e))?;
    let present = tables(&tx, schema).await?;
    if !present.is_empty() {
        let list = present
            .iter()
            .map(|t| format!("{}.{}", ident(schema), ident(t)))
            .collect::<Vec<_>>()
            .join(", ");
        tx.batch_execute(&format!("TRUNCATE {list}"))
            .await
            .map_err(|e| unavailable("truncate", e))?;
    }
    let mut rows = 0u64;
    for d in dumps.iter().filter(|d| d.role == role) {
        if !present.contains(&d.table) {
            return Err(ApiError::new(
                ErrorReason::InvalidState,
                format!(
                    "table {} of the backup does not exist after migrations; restore with a server version that knows it",
                    d.table
                ),
            ));
        }
        let stmt = format!("COPY {}.{} FROM STDIN", ident(schema), ident(&d.table));
        let sink = tx
            .copy_in::<_, Bytes>(&stmt)
            .await
            .map_err(|e| unavailable("copy in", e))?;
        let mut sink = Box::pin(sink);
        sink.send(Bytes::from(d.data.clone()))
            .await
            .map_err(|e| unavailable("copy in", e))?;
        rows += sink
            .as_mut()
            .finish()
            .await
            .map_err(|e| unavailable("copy in", e))?;
    }
    // Identity and serial columns continue after the loaded keys.
    let seqs = tx
        .query(
            "SELECT table_name, column_name FROM information_schema.columns
             WHERE table_schema = $1 AND (is_identity = 'YES' OR column_default LIKE 'nextval(%')",
            &[&schema],
        )
        .await
        .map_err(|e| unavailable("list sequences", e))?;
    for r in seqs {
        let (t, c): (String, String) = (r.get(0), r.get(1));
        let qualified = format!("{}.{}", ident(schema), ident(&t));
        let sql = format!(
            "SELECT setval(pg_get_serial_sequence($1, $2), COALESCE((SELECT MAX({col}) FROM {qualified}), 0) + 1, false)",
            col = ident(&c),
        );
        tx.query(&sql, &[&qualified, &c])
            .await
            .map_err(|e| unavailable("reset sequence", e))?;
    }
    tx.commit()
        .await
        .map_err(|e| unavailable("commit restore", e))?;
    Ok(rows)
}

/// Whether `schema` holds any table with rows (a restore target must be empty).
pub async fn pg_schema_has_rows(url: &str, schema: &str) -> ApiResult<bool> {
    let client = pg_connect(url).await?;
    for t in tables(&client, schema).await? {
        if t == "bee_schema" || t.ends_with("_migrations") || t == "forge_migrations" {
            continue;
        }
        let n: i64 = client
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM (SELECT 1 FROM {}.{} LIMIT 1) x",
                    ident(schema),
                    ident(&t)
                ),
                &[],
            )
            .await
            .map_err(|e| unavailable("count rows", e))?
            .get(0);
        if n > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Org ids in a metadata database (SQLite file), for blob manifests and fsck.
pub async fn sqlite_org_ids(path: PathBuf) -> ApiResult<Vec<bee_core::Ulid>> {
    tokio::task::spawn_blocking(move || -> ApiResult<Vec<bee_core::Ulid>> {
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| unavailable("open metadata", e))?;
        let mut st = conn
            .prepare("SELECT id FROM orgs ORDER BY id")
            .map_err(|e| unavailable("list orgs", e))?;
        let mut rows = st.query([]).map_err(|e| unavailable("list orgs", e))?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(|e| unavailable("list orgs", e))? {
            let b: Vec<u8> = r.get(0).map_err(|e| unavailable("list orgs", e))?;
            if let Ok(a) = <[u8; 16]>::try_from(b.as_slice()) {
                out.push(bee_core::Ulid::from_bytes(a));
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| unavailable("list orgs task", e))?
}

/// Org ids in a Postgres metadata schema.
pub async fn pg_org_ids(url: &str, schema: &str) -> ApiResult<Vec<bee_core::Ulid>> {
    let client = pg_connect(url).await?;
    let rows = client
        .query(
            &format!("SELECT id FROM {}.orgs ORDER BY id", ident(schema)),
            &[],
        )
        .await
        .map_err(|e| unavailable("list orgs", e))?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let b: Vec<u8> = r.get(0);
            <[u8; 16]>::try_from(b.as_slice())
                .ok()
                .map(bee_core::Ulid::from_bytes)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_snapshot_is_a_readable_copy() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("live.db");
        let c = rusqlite::Connection::open(&p).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode = WAL; CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1), (2);",
        )
        .unwrap();
        let bytes = sqlite_snapshot(p.clone()).await.unwrap();
        let q = dir.path().join("copy.db");
        write_durable(&q, &bytes).unwrap();
        let c2 = rusqlite::Connection::open(&q).unwrap();
        let n: i64 = c2
            .query_row("SELECT SUM(x) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(ident("a\"b"), "\"a\"\"b\"");
    }
}
