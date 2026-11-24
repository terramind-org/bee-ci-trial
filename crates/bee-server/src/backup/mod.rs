//! Backups and restores (`AdminService.CreateBackup`, `ListBackups`, `StartRestore`,
//! `GetRestore`; `docs/spec/operations.md` §4).
//!
//! A **backup** is a manifest plus files on the backup [`target::Target`]:
//!
//! - the metadata database, the server store and the forge database: SQLite files
//!   snapshotted with `VACUUM INTO`, or every table of the Postgres metadata and forge schemas
//!   exported in one `REPEATABLE READ` snapshot ([`sql`]);
//! - the signing keys and the signed-URL key of the data directory (needed for tokens and
//!   URLs to keep verifying after a restore; backups must be protected accordingly);
//! - a **blob manifest** per org: every object id the blob store holds. With the disk blob
//!   store the objects themselves are copied to the target (incrementally: an object already
//!   there is not copied again); with S3 the bucket is the durable copy and the manifest lets
//!   a restore verify it.
//!
//! A **restore** rebuilds a complete data directory at `<backup.restore_root>/<target_cell>`
//! (which must be absent or empty): SQLite files (or, for Postgres, the schemas
//! `<target_cell>` and `<target_cell>_forge`, which must be empty), keys and blobs. It then
//! opens the restored metadata and blob stores and runs fsck over every org; a **drill** also
//! checks that every ref's tip and root tree read back and that the restored audit chain
//! verifies. Start a server on the restored directory to serve it.
//!
//! Point-in-time restore between backups needs WAL archiving, which this server does not
//! keep: `point_in_time` must be unset or equal to the backup's time.

pub mod sql;
pub mod target;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bee_core::ObjectId;
use bee_forge::store::ForgeStore;
use bee_proto::v1::{self, ErrorReason};
use bee_store::{BlobStore, FsckOptions, MetaStore, OrgId, StorageClass};
use bee_store_blob::DiskBlobStore;
use bee_store_sql::{PgMetaStore, PgOptions, SqliteMetaStore, SqliteOptions};
use serde::{Deserialize, Serialize};

use crate::errors::{ApiError, ApiResult};
use crate::logging::{self, Level};
use crate::services::support::{get_json, put_json, scan_json};
use crate::state::AppState;
use crate::util::timestamp_ms;

use self::target::{FileRef, MANIFEST_MAGIC, Target};

/// Backup rows of this server (`<id>`).
pub const NS_BACKUP: &str = "backup";
/// Restore rows (`<id>`).
pub const NS_RESTORE: &str = "restore";

/// What a backup holds.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    /// Backup id.
    pub id: String,
    /// Cell.
    pub cell: String,
    /// Operator note.
    pub note: String,
    /// `sqlite` or `postgres`.
    pub metadata_backend: String,
    /// Started.
    pub started_ms: u64,
    /// Finished.
    pub finished_ms: u64,
    /// Database snapshots and data-directory files.
    pub files: Vec<FileRef>,
    /// Org id → blob manifest file (newline-separated object ids).
    pub blob_manifests: Vec<(String, FileRef)>,
    /// Objects were copied to the target.
    pub copied_blobs: bool,
    /// Objects listed across orgs.
    pub objects: u64,
    /// Total bytes stored for this backup (files and copied objects, before dedup).
    pub size_bytes: u64,
}

/// A backup row.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct BackupRec {
    /// Id.
    pub id: String,
    /// Cell.
    pub cell: String,
    /// `BackupState`.
    pub state: i32,
    /// Started.
    pub started_ms: u64,
    /// Finished.
    pub finished_ms: u64,
    /// Bytes.
    pub size_bytes: u64,
    /// Note.
    pub note: String,
    /// Manifest blob id on the target.
    pub manifest: String,
    /// Failure.
    pub error: String,
    /// Target location.
    pub location: String,
    /// Restored and verified by a drill.
    pub verified: bool,
}

/// A restore row.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RestoreRec {
    /// Id.
    pub id: String,
    /// Backup id.
    pub backup_id: String,
    /// Target cell.
    pub target_cell: String,
    /// `BackupState`.
    pub state: i32,
    /// Started.
    pub started_ms: u64,
    /// Finished.
    pub finished_ms: u64,
    /// fsck found nothing.
    pub fsck_clean: bool,
    /// Drill checks passed.
    pub conformance_passed: bool,
    /// Progress and results.
    pub detail: String,
    /// A drill.
    pub drill: bool,
    /// Where the data directory was written.
    pub data_dir: String,
    /// Backup time (for data-loss measurement).
    pub backup_ms: u64,
}

fn retention_ms(state: &AppState) -> u64 {
    state.cfg.backup.retention_days.clamp(1, 30) * bee_store::DAY_MS
}

/// The wire form of a backup row.
pub fn backup_proto(state: &AppState, r: &BackupRec) -> v1::Backup {
    v1::Backup {
        id: r.id.clone(),
        cell: r.cell.clone(),
        state: r.state,
        started_at: Some(timestamp_ms(r.started_ms)),
        finished_at: (r.finished_ms > 0).then(|| timestamp_ms(r.finished_ms)),
        size_bytes: r.size_bytes,
        expires_at: Some(timestamp_ms(r.started_ms + retention_ms(state))),
        note: if r.error.is_empty() {
            r.note.clone()
        } else {
            format!("{} (failed: {})", r.note, r.error)
                .trim()
                .to_string()
        },
    }
}

/// The wire form of a restore row.
pub fn restore_proto(r: &RestoreRec) -> v1::Restore {
    v1::Restore {
        id: r.id.clone(),
        backup_id: r.backup_id.clone(),
        target_cell: r.target_cell.clone(),
        state: r.state,
        started_at: Some(timestamp_ms(r.started_ms)),
        finished_at: (r.finished_ms > 0).then(|| timestamp_ms(r.finished_ms)),
        fsck_clean: r.fsck_clean,
        conformance_passed: r.conformance_passed,
        detail: r.detail.clone(),
    }
}

/// The Postgres schemas of this server: `(role, schema)` for metadata and forge.
fn pg_schemas(state: &AppState) -> Vec<(String, String)> {
    let m = &state.cfg.metadata;
    let meta = if m.postgres_schema.is_empty() {
        "public".to_string()
    } else {
        m.postgres_schema.clone()
    };
    let forge = forge_schema(state);
    let mut out = vec![("meta".to_string(), meta)];
    if let Some(f) = forge {
        out.push(("forge".to_string(), f));
    }
    out
}

/// The forge's Postgres schema, when the forge lives in the metadata database.
fn forge_schema(state: &AppState) -> Option<String> {
    let cfg = &state.cfg;
    if !cfg.forge.enabled {
        return None;
    }
    if !cfg.forge.database.is_empty() {
        let d = &cfg.forge.database;
        if d.starts_with("postgres") {
            return Some(
                d.split_once("schema=")
                    .map(|(_, s)| s.split('&').next().unwrap_or("").to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "bee_forge".into()),
            );
        }
        return None;
    }
    if cfg.metadata.backend == "postgres" {
        Some(if cfg.metadata.postgres_schema.is_empty() {
            "bee_forge".into()
        } else {
            format!("{}_forge", cfg.metadata.postgres_schema)
        })
    } else {
        None
    }
}

/// The forge's SQLite directory, when it uses SQLite.
fn forge_dir(state: &AppState) -> Option<PathBuf> {
    let cfg = &state.cfg;
    if !cfg.forge.enabled {
        return None;
    }
    if let Some(d) = cfg.forge.database.strip_prefix("sqlite:") {
        return Some(PathBuf::from(d));
    }
    (cfg.forge.database.is_empty() && cfg.metadata.backend != "postgres")
        .then(|| cfg.server.data_dir.join("forge"))
}

/// Data-directory files a restore needs (keys).
const DATA_FILES: &[&str] = &["signing-keys.json", "url.key"];

/// Starts a backup in the background; returns its row (RUNNING).
pub async fn create(state: &Arc<AppState>, cell: &str, note: &str) -> ApiResult<BackupRec> {
    if !cell.is_empty() && cell != state.cfg.server.cell {
        return Err(ApiError::invalid(
            "unknown_cell",
            format!(
                "this server serves cell {:?}; back up other cells on their own servers",
                state.cfg.server.cell
            ),
        ));
    }
    let target = Target::open(&state.cfg)?;
    let now = state.now_ms();
    let rec = BackupRec {
        id: crate::util::new_ulid(now)?.to_text(),
        cell: state.cfg.server.cell.clone(),
        state: v1::BackupState::Running as i32,
        started_ms: now,
        note: note.chars().take(500).collect(),
        location: target.location.clone(),
        ..BackupRec::default()
    };
    put_json(state.kv.as_ref(), NS_BACKUP, &rec.id, &rec).await?;
    let s = state.clone();
    let job = rec.clone();
    tokio::spawn(async move {
        let fin = match run_backup(&s, &target, job.clone()).await {
            Ok(r) => r,
            Err(e) => {
                logging::log(
                    Level::Error,
                    "backup",
                    "backup failed",
                    &[("error", e.detail.message.clone().into())],
                );
                BackupRec {
                    state: v1::BackupState::Failed as i32,
                    finished_ms: s.now_ms(),
                    error: e.detail.message.clone(),
                    ..job
                }
            }
        };
        let _ = put_json(s.kv.as_ref(), NS_BACKUP, &fin.id, &fin).await;
    });
    Ok(rec)
}

async fn run_backup(state: &AppState, target: &Target, mut rec: BackupRec) -> ApiResult<BackupRec> {
    let cfg = &state.cfg;
    let mut m = Manifest {
        id: rec.id.clone(),
        cell: rec.cell.clone(),
        note: rec.note.clone(),
        metadata_backend: cfg.metadata.backend.clone(),
        started_ms: rec.started_ms,
        ..Manifest::default()
    };
    let orgs = if cfg.metadata.backend == "postgres" {
        let dumps = sql::pg_dump(&cfg.metadata.postgres_url, &pg_schemas(state)).await?;
        for d in &dumps {
            let f = target
                .put_file(&format!("pg/{}/{}.copy", d.role, d.table), &d.data)
                .await?;
            m.size_bytes += f.size;
            m.files.push(f);
        }
        let meta_schema = pg_schemas(state)[0].1.clone();
        sql::pg_org_ids(&cfg.metadata.postgres_url, &meta_schema).await?
    } else {
        let meta_path = cfg.data_path(&cfg.metadata.sqlite_path, "meta.db");
        let snap = sql::sqlite_snapshot(meta_path).await?;
        let f = target.put_file("meta.db", &snap).await?;
        m.size_bytes += f.size;
        m.files.push(f);
        let kv_path = cfg.data_path(&cfg.metadata.server_sqlite_path, "server.db");
        let kv = sql::sqlite_snapshot(kv_path).await?;
        let f = target.put_file("server.db", &kv).await?;
        m.size_bytes += f.size;
        m.files.push(f);
        if let Some(dir) = forge_dir(state) {
            for (name, path) in sql::forge_files(&dir) {
                let snap = sql::sqlite_snapshot(path).await?;
                let f = target.put_file(&format!("forge/{name}"), &snap).await?;
                m.size_bytes += f.size;
                m.files.push(f);
            }
        }
        // The org list of the snapshot itself, so the blob manifest covers what it names.
        let tmp = cfg.server.data_dir.join(format!(".orgs-{}.db", rec.id));
        sql::write_durable(&tmp, &snap)?;
        let ids = sql::sqlite_org_ids(tmp.clone()).await;
        let _ = std::fs::remove_file(&tmp);
        ids?
    };
    for name in DATA_FILES {
        if let Ok(b) = std::fs::read(cfg.server.data_dir.join(name)) {
            let f = target.put_file(&format!("data/{name}"), &b).await?;
            m.files.push(f);
        }
    }
    // Blob manifests (and copies with the disk blob store).
    let copy = cfg.blobs.backend == "disk";
    m.copied_blobs = copy;
    for org in orgs {
        let org_id = OrgId(org);
        let mut ids: Vec<ObjectId> = Vec::new();
        let mut after = None;
        loop {
            let page = state.blobs.list(org_id, after, 1000).await?;
            let done = page.len() < 1000;
            for (id, info) in &page {
                if info.quarantined {
                    continue;
                }
                ids.push(*id);
            }
            match page.last() {
                Some((id, _)) if !done => after = Some(*id),
                _ => break,
            }
        }
        if copy {
            for chunk in ids.chunks(256) {
                let have = target.store.has(org_id, chunk).await?;
                for (id, present) in chunk.iter().zip(have) {
                    if present {
                        continue;
                    }
                    if let Some(bytes) = state.blobs.get(org_id, id).await? {
                        m.size_bytes += bytes.len() as u64;
                        target
                            .store
                            .put(org_id, id, bytes, StorageClass::Hot)
                            .await?;
                    }
                }
            }
        }
        m.objects += ids.len() as u64;
        let list = ids
            .iter()
            .map(ObjectId::to_text)
            .collect::<Vec<_>>()
            .join("\n");
        let f = target
            .put_file(&format!("blobs/{}.ids", org.to_text()), list.as_bytes())
            .await?;
        m.blob_manifests.push((org.to_text(), f));
    }
    m.finished_ms = state.now_ms();
    let mut body = MANIFEST_MAGIC.as_bytes().to_vec();
    body.extend(
        serde_json::to_vec(&m)
            .map_err(|e| ApiError::new(ErrorReason::Internal, format!("encode manifest: {e}")))?,
    );
    let id = bee_object::blob_id(&body);
    target
        .store
        .put(target::backup_org(), &id, body, StorageClass::Hot)
        .await?;
    rec.manifest = id.to_text();
    rec.size_bytes = m.size_bytes;
    rec.finished_ms = m.finished_ms;
    rec.state = v1::BackupState::Succeeded as i32;
    logging::log(
        Level::Info,
        "backup",
        "backup finished",
        &[
            ("backup", rec.id.clone().into()),
            ("objects", m.objects.into()),
            ("bytes", m.size_bytes.into()),
        ],
    );
    Ok(rec)
}

fn parse_manifest(bytes: &[u8]) -> ApiResult<Manifest> {
    let body = bytes
        .strip_prefix(MANIFEST_MAGIC.as_bytes())
        .ok_or_else(|| ApiError::new(ErrorReason::Integrity, "not a backup manifest"))?;
    serde_json::from_slice(body)
        .map_err(|e| ApiError::new(ErrorReason::Integrity, format!("bad manifest: {e}")))
}

/// Every backup: rows of this server plus manifests found on the target (backups taken by
/// another server writing to the same target), newest first.
pub async fn list(state: &AppState) -> ApiResult<Vec<BackupRec>> {
    let mut rows: Vec<BackupRec> = scan_json::<BackupRec>(state.kv.as_ref(), NS_BACKUP, "")
        .await?
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    if let Ok(target) = Target::open(&state.cfg) {
        for (blob, bytes) in target.manifests().await.unwrap_or_default() {
            let Ok(m) = parse_manifest(&bytes) else {
                continue;
            };
            if rows.iter().any(|r| r.id == m.id) {
                continue;
            }
            rows.push(BackupRec {
                id: m.id,
                cell: m.cell,
                state: v1::BackupState::Succeeded as i32,
                started_ms: m.started_ms,
                finished_ms: m.finished_ms,
                size_bytes: m.size_bytes,
                note: m.note,
                manifest: blob,
                location: target.location.clone(),
                ..BackupRec::default()
            });
        }
    }
    let horizon = state.now_ms().saturating_sub(retention_ms(state));
    rows.retain(|r| r.started_ms >= horizon);
    rows.sort_by(|a, b| b.started_ms.cmp(&a.started_ms).then(b.id.cmp(&a.id)));
    Ok(rows)
}

async fn find_backup(state: &AppState, id: &str) -> ApiResult<(BackupRec, Manifest, Target)> {
    let target = Target::open(&state.cfg)?;
    let rec = list(state)
        .await?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| ApiError::not_found(format!("backup {id}")))?;
    if rec.state != v1::BackupState::Succeeded as i32 {
        return Err(ApiError::new(
            ErrorReason::InvalidState,
            format!("backup {id} did not succeed"),
        ));
    }
    let mid = ObjectId::parse(&rec.manifest).map_err(ApiError::from)?;
    let bytes = target
        .store
        .get(target::backup_org(), &mid)
        .await?
        .ok_or_else(|| ApiError::new(ErrorReason::Integrity, "the backup manifest is missing"))?;
    let m = parse_manifest(&bytes)?;
    Ok((rec, m, target))
}

fn safe_cell(name: &str) -> ApiResult<String> {
    let ok = !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && name.as_bytes()[0].is_ascii_lowercase();
    if ok {
        Ok(name.to_string())
    } else {
        Err(ApiError::invalid(
            "bad_target_cell",
            "target_cell is 1-48 lowercase letters, digits, `_` or `-`, starting with a letter",
        ))
    }
}

/// Where restores write.
pub fn restore_root(state: &AppState) -> PathBuf {
    if state.cfg.backup.restore_root.is_empty() {
        state.cfg.server.data_dir.join("restores")
    } else {
        PathBuf::from(&state.cfg.backup.restore_root)
    }
}

/// Starts a restore in the background; returns its row (RUNNING).
pub async fn start_restore(
    state: &Arc<AppState>,
    req: v1::StartRestoreRequest,
) -> ApiResult<RestoreRec> {
    if req.backup_id.is_empty() {
        return Err(ApiError::invalid("missing_field", "backup_id is required"));
    }
    let (backup, manifest, target) = find_backup(state, &req.backup_id).await?;
    if let Some(t) = &req.point_in_time {
        let at = crate::util::ts_secs(t) * 1000;
        if at / 1000 != manifest.finished_ms / 1000 && at / 1000 != manifest.started_ms / 1000 {
            return Err(ApiError::invalid(
                "point_in_time_unavailable",
                "this server keeps no WAL archive: restore to the backup's own time",
            ));
        }
    }
    let now = state.now_ms();
    let id = crate::util::new_ulid(now)?.to_text();
    let cell = if req.target_cell.is_empty() {
        format!("restore-{}", id.to_ascii_lowercase())
    } else {
        safe_cell(&req.target_cell)?
    };
    let dir = restore_root(state).join(&cell);
    if dir.exists()
        && std::fs::read_dir(&dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(true)
    {
        return Err(ApiError::new(
            ErrorReason::AlreadyExists,
            format!(
                "{} is not empty; restores go into an empty data directory",
                dir.display()
            ),
        ));
    }
    if manifest.metadata_backend == "postgres" {
        if state.cfg.metadata.backend != "postgres" {
            return Err(ApiError::new(
                ErrorReason::InvalidState,
                "a Postgres backup restores through a server configured for Postgres",
            ));
        }
        let url = &state.cfg.metadata.postgres_url;
        let schema = pg_target_schema(&cell);
        if sql::pg_schema_has_rows(url, &schema).await?
            || sql::pg_schema_has_rows(url, &format!("{schema}_forge")).await?
        {
            return Err(ApiError::new(
                ErrorReason::AlreadyExists,
                format!("schema {schema} is not empty"),
            ));
        }
    }
    let rec = RestoreRec {
        id,
        backup_id: backup.id.clone(),
        target_cell: cell,
        state: v1::BackupState::Running as i32,
        started_ms: now,
        drill: req.drill,
        data_dir: dir.display().to_string(),
        backup_ms: manifest.finished_ms,
        ..RestoreRec::default()
    };
    put_json(state.kv.as_ref(), NS_RESTORE, &rec.id, &rec).await?;
    let s = state.clone();
    let job = rec.clone();
    tokio::spawn(async move {
        let fin = match run_restore(&s, &target, &manifest, job.clone()).await {
            Ok(r) => r,
            Err(e) => RestoreRec {
                state: v1::BackupState::Failed as i32,
                finished_ms: s.now_ms(),
                detail: e.detail.message.clone(),
                ..job
            },
        };
        let _ = put_json(s.kv.as_ref(), NS_RESTORE, &fin.id, &fin).await;
        if fin.drill
            && fin.state == v1::BackupState::Succeeded as i32
            && fin.fsck_clean
            && fin.conformance_passed
            && let Ok(Some(mut b)) =
                get_json::<BackupRec>(s.kv.as_ref(), NS_BACKUP, &fin.backup_id).await
        {
            b.verified = true;
            let _ = put_json(s.kv.as_ref(), NS_BACKUP, &b.id, &b).await;
        }
    });
    Ok(rec)
}

/// The Postgres schema a restore into `cell` writes the metadata to.
pub fn pg_target_schema(cell: &str) -> String {
    format!("restore_{}", cell.replace('-', "_"))
}

/// Creates every forge table in the Postgres schema `schema`, as the running server's forge has
/// them, so the backup's forge tables can be loaded into it.
///
/// `SqlStore::open` connects but creates no table; the forge's tables come from its module
/// migrations, which `Forge::open` runs on every start. A restore never opens a `Forge`, so it
/// runs the same migrations itself. Without them the schema stays empty and `pg_load` refuses
/// the first forge table of the backup ("table forge_activity of the backup does not exist after
/// migrations"), which failed every restore of a Postgres-backed server.
pub(crate) async fn restore_forge_schema(
    url: &str,
    schema: &str,
    clock: Arc<dyn bee_forge::ids::Clock>,
) -> ApiResult<()> {
    let store = bee_forge::store::sql::SqlStore::open(
        bee_forge::store::sql::SqlConfig::postgres(url.to_string(), schema.to_string()),
        clock,
    )
    .await
    .map_err(crate::forge::forge_to_api)?;
    let registry = bee_forge::module::Registry::core().map_err(crate::forge::forge_to_api)?;
    store
        .migrate(&registry.migrations())
        .await
        .map_err(crate::forge::forge_to_api)?;
    Ok(())
}

async fn run_restore(
    state: &AppState,
    target: &Target,
    m: &Manifest,
    mut rec: RestoreRec,
) -> ApiResult<RestoreRec> {
    let dir = PathBuf::from(&rec.data_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApiError::new(ErrorReason::Unavailable, format!("create data dir: {e}")))?;
    let mut notes: Vec<String> = Vec::new();
    // 1. Databases and data-directory files.
    let pg = m.metadata_backend == "postgres";
    let mut dumps: Vec<sql::TableDump> = Vec::new();
    for f in &m.files {
        let bytes = target.get_file(f).await?;
        if let Some(rest) = f.name.strip_prefix("pg/") {
            let (role, file) = rest.split_once('/').unwrap_or(("meta", rest));
            dumps.push(sql::TableDump {
                role: role.to_string(),
                table: file.trim_end_matches(".copy").to_string(),
                data: bytes,
            });
        } else if let Some(name) = f.name.strip_prefix("data/") {
            sql::write_durable(&dir.join(name), &bytes)?;
        } else {
            sql::write_durable(&dir.join(&f.name), &bytes)?;
        }
    }
    let schema = pg_target_schema(&rec.target_cell);
    let url = state.cfg.metadata.postgres_url.clone();
    let meta: Arc<dyn MetaStore> = if pg {
        let mut o = PgOptions::new(url.clone());
        o.schema = Some(schema.clone());
        let store = PgMetaStore::connect(o).await?;
        // The server store and the forge create their tables, then every table is loaded.
        crate::kv::PgKv::connect(&url, Some(&schema), 1).await?;
        let rows = sql::pg_load(&url, &schema, "meta", &dumps).await?;
        notes.push(format!("{rows} metadata rows"));
        if dumps.iter().any(|d| d.role == "forge") {
            let fs = format!("{schema}_forge");
            restore_forge_schema(&url, &fs, state.clock.clone()).await?;
            let rows = sql::pg_load(&url, &fs, "forge", &dumps).await?;
            notes.push(format!("{rows} forge rows"));
        }
        Arc::new(store)
    } else {
        Arc::new(SqliteMetaStore::open(SqliteOptions::new(dir.join("meta.db"))).await?)
    };
    // 2. Blobs.
    let restored_blobs: Arc<dyn BlobStore> = if m.copied_blobs {
        Arc::new(DiskBlobStore::open(dir.join("blobs"))?)
    } else {
        state.blobs.clone()
    };
    let mut copied = 0u64;
    let mut missing = 0u64;
    for (org_text, f) in &m.blob_manifests {
        let org = OrgId(bee_core::Ulid::parse(org_text).map_err(ApiError::from)?);
        let list = target.get_file(f).await?;
        let ids: Vec<ObjectId> = String::from_utf8_lossy(&list)
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| ObjectId::parse(l).ok())
            .collect();
        for chunk in ids.chunks(256) {
            if m.copied_blobs {
                for id in chunk {
                    match target.store.get(org, id).await? {
                        Some(bytes) => {
                            restored_blobs
                                .put(org, id, bytes, StorageClass::Hot)
                                .await?;
                            copied += 1;
                        }
                        None => missing += 1,
                    }
                }
            } else {
                missing += restored_blobs
                    .has(org, chunk)
                    .await?
                    .iter()
                    .filter(|h| !**h)
                    .count() as u64;
            }
        }
    }
    notes.push(format!("{copied} objects copied, {missing} missing"));
    // 3. Verification.
    let orgs = if pg {
        sql::pg_org_ids(&url, &schema).await?
    } else {
        sql::sqlite_org_ids(dir.join("meta.db")).await?
    };
    let opts = FsckOptions {
        eager: state.registry.eager_types(),
        ..FsckOptions::default()
    };
    let mut violations = 0usize;
    for org in &orgs {
        let report =
            bee_store::fsck(meta.as_ref(), restored_blobs.as_ref(), OrgId(*org), &opts).await?;
        violations += report.violations.len();
    }
    rec.fsck_clean = violations == 0 && missing == 0;
    notes.push(format!(
        "fsck: {violations} violations over {} orgs",
        orgs.len()
    ));
    if rec.drill {
        let (ok, why) = drill_checks(
            state,
            &dir,
            pg,
            &url,
            &schema,
            &meta,
            &restored_blobs,
            &orgs,
        )
        .await?;
        rec.conformance_passed = ok;
        notes.push(why);
    }
    let note = serde_json::json!({
        "backup": rec.backup_id,
        "metadata_backend": m.metadata_backend,
        "postgres_schema": if pg { schema.clone() } else { String::new() },
        "blobs": if m.copied_blobs { "disk:blobs" } else { "shared" },
    });
    sql::write_durable(
        &dir.join("restore.json"),
        serde_json::to_string_pretty(&note)
            .unwrap_or_default()
            .as_bytes(),
    )?;
    rec.detail = notes.join("; ");
    rec.finished_ms = state.now_ms();
    rec.state = v1::BackupState::Succeeded as i32;
    Ok(rec)
}

/// Drill checks: every ref of every repo resolves to a changeset whose root tree reads back,
/// and the restored server store's audit chain verifies.
#[allow(clippy::too_many_arguments)]
async fn drill_checks(
    state: &AppState,
    dir: &Path,
    pg: bool,
    url: &str,
    schema: &str,
    meta: &Arc<dyn MetaStore>,
    blobs: &Arc<dyn BlobStore>,
    orgs: &[bee_core::Ulid],
) -> ApiResult<(bool, String)> {
    let mut refs = 0u64;
    let mut bad = Vec::new();
    for org in orgs {
        let org_id = OrgId(*org);
        for repo in meta.list_repos(org_id).await? {
            let writer = bee_store::Writer::new(meta.clone(), blobs.clone());
            let history = bee_history::History::new(writer, org_id, repo.id);
            for r in meta.list_refs(repo.id, "").await? {
                refs += 1;
                let Some(t) = r.target else { continue };
                match history.changeset(&t).await {
                    Ok(cs) => {
                        if let Err(e) = history.tree(cs.root()).await {
                            bad.push(format!("{}: root tree: {}", r.name, e.message));
                        }
                    }
                    Err(e) => bad.push(format!("{}: {}", r.name, e.message)),
                }
            }
        }
    }
    let kv: Arc<dyn crate::kv::Kv> = if pg {
        Arc::new(crate::kv::PgKv::connect(url, Some(schema), 1).await?)
    } else {
        Arc::new(crate::kv::SqliteKv::open(&dir.join("server.db"))?)
    };
    let audit = crate::audit::KvAudit::start(kv, state.metrics.clone());
    let chain = audit.verify().await?;
    let mut ok = bad.is_empty();
    let chain_note = match chain {
        Ok(n) => format!("audit chain verified ({n} entries)"),
        Err(seq) => {
            ok = false;
            format!("audit chain broken at {seq}")
        }
    };
    let detail = if bad.is_empty() {
        format!("drill: {refs} refs read back; {chain_note}")
    } else {
        format!(
            "drill: {} of {refs} refs failed ({}); {chain_note}",
            bad.len(),
            bad.join(", ")
        )
    };
    Ok((ok, detail))
}
