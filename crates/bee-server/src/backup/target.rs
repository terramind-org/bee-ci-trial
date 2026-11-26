//! Where backups live: any [`BlobStore`] (a directory, or the S3-compatible bucket of
//! `[blobs]` under its own prefix). Files of a backup are split into content-addressed chunks
//! stored under a reserved org, so unchanged files and chunks are stored once across backups;
//! object copies keep their real org and id.

use std::sync::Arc;

use bee_core::{ObjectId, Ulid};
use bee_store::{BlobStore, OrgId, StorageClass};
use bee_store_blob::{DiskBlobStore, S3BlobStore, S3Config, S3Credentials};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::errors::{ApiError, ApiResult};
use bee_proto::v1::ErrorReason;

/// The reserved org holding backup files and manifests (no real org has id 0).
pub fn backup_org() -> OrgId {
    OrgId(Ulid::from_bytes([0; 16]))
}

/// Chunk size of backup files.
pub const CHUNK: usize = 32 << 20;

/// Magic first line of a manifest.
pub const MANIFEST_MAGIC: &str = "swarm-backup-manifest-v1\n";

/// A file stored as chunks.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRef {
    /// Logical name inside the backup (`meta.db`, `pg/meta/refs.copy`, ...).
    pub name: String,
    /// Total size.
    pub size: u64,
    /// Chunk blob ids (text), in order.
    pub chunks: Vec<String>,
}

/// A backup target.
#[derive(Clone)]
pub struct Target {
    /// The store.
    pub store: Arc<dyn BlobStore>,
    /// Human-readable location (no credentials).
    pub location: String,
}

fn unavailable(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorReason::Unavailable, format!("backup target: {e}"))
}

/// Targets opened by this process, by location: one store instance per location (opening a
/// directory store sweeps its temporary files, which must not race a running backup).
static OPEN: std::sync::Mutex<Vec<(String, Target)>> = std::sync::Mutex::new(Vec::new());

impl Target {
    /// The configured target (`backup.target`), shared by every caller in this process.
    pub fn open(cfg: &Config) -> ApiResult<Target> {
        let key = format!(
            "{}|{}|{}",
            cfg.backup.target,
            cfg.backup.s3_prefix,
            cfg.server.data_dir.display()
        );
        let mut open = OPEN.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, t)) = open.iter().find(|(k, _)| *k == key) {
            return Ok(t.clone());
        }
        let t = Self::open_new(cfg)?;
        open.push((key, t.clone()));
        Ok(t)
    }

    fn open_new(cfg: &Config) -> ApiResult<Target> {
        let t = cfg.backup.target.trim();
        if t == "s3" {
            let b = &cfg.blobs;
            if b.s3_bucket.is_empty() {
                return Err(ApiError::new(
                    ErrorReason::InvalidState,
                    "backup.target = \"s3\" needs blobs.s3_bucket",
                ));
            }
            let mut c = S3Config::new(
                b.s3_bucket.clone(),
                b.s3_region.clone(),
                S3Credentials::new(b.s3_access_key_id.clone(), b.s3_secret_access_key.clone()),
            );
            if !b.s3_endpoint.is_empty() {
                c = c.with_endpoint(b.s3_endpoint.clone());
            }
            c.prefix = cfg.backup.s3_prefix.clone();
            let location = format!("s3://{}/{}", b.s3_bucket, cfg.backup.s3_prefix);
            return Ok(Target {
                store: Arc::new(S3BlobStore::new(c)?),
                location,
            });
        }
        let dir = match t.strip_prefix("dir:") {
            Some(d) => std::path::PathBuf::from(d),
            None if t.is_empty() => cfg.server.data_dir.join("backups"),
            None => {
                return Err(ApiError::new(
                    ErrorReason::InvalidState,
                    "backup.target must be empty, `dir:<path>` or `s3`",
                ));
            }
        };
        let location = format!("dir:{}", dir.display());
        Ok(Target {
            store: Arc::new(DiskBlobStore::open(dir)?),
            location,
        })
    }

    /// Stores `bytes` as a file.
    pub async fn put_file(&self, name: &str, bytes: &[u8]) -> ApiResult<FileRef> {
        let org = backup_org();
        let mut chunks = Vec::new();
        for c in bytes.chunks(CHUNK.max(1)) {
            let id = bee_object::blob_id(c);
            if !self
                .store
                .has(org, &[id])
                .await?
                .first()
                .copied()
                .unwrap_or(false)
            {
                self.store
                    .put(org, &id, c.to_vec(), StorageClass::Hot)
                    .await
                    .map_err(unavailable)?;
            }
            chunks.push(id.to_text());
        }
        if bytes.is_empty() {
            let id = bee_object::blob_id(&[]);
            if !self
                .store
                .has(org, &[id])
                .await?
                .first()
                .copied()
                .unwrap_or(false)
            {
                self.store
                    .put(org, &id, Vec::new(), StorageClass::Hot)
                    .await?;
            }
            chunks.push(id.to_text());
        }
        Ok(FileRef {
            name: name.to_string(),
            size: bytes.len() as u64,
            chunks,
        })
    }

    /// Reads a file back, verifying every chunk against its id.
    pub async fn get_file(&self, f: &FileRef) -> ApiResult<Vec<u8>> {
        let org = backup_org();
        let mut out = Vec::with_capacity(f.size as usize);
        for c in &f.chunks {
            let id = ObjectId::parse(c).map_err(ApiError::from)?;
            let bytes = self.store.get(org, &id).await?.ok_or_else(|| {
                ApiError::new(
                    ErrorReason::Integrity,
                    format!("backup chunk {c} of {} is missing", f.name),
                )
            })?;
            if bee_object::blob_id(&bytes) != id {
                return Err(ApiError::new(
                    ErrorReason::Integrity,
                    format!("backup chunk {c} of {} is corrupt", f.name),
                ));
            }
            out.extend_from_slice(&bytes);
        }
        if out.len() as u64 != f.size {
            return Err(ApiError::new(
                ErrorReason::Integrity,
                format!("backup file {} has the wrong size", f.name),
            ));
        }
        Ok(out)
    }

    /// Every manifest stored on the target (by scanning the reserved org for the magic).
    pub async fn manifests(&self) -> ApiResult<Vec<(String, Vec<u8>)>> {
        let org = backup_org();
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self.store.list(org, after, 1000).await?;
            let done = page.len() < 1000;
            for (id, info) in &page {
                // Manifests are small; chunks are not worth reading.
                if info.len > 4 << 20 || info.quarantined {
                    continue;
                }
                if let Some(head) = self
                    .store
                    .get_range(org, id, 0, MANIFEST_MAGIC.len() as u64)
                    .await?
                    && head == MANIFEST_MAGIC.as_bytes()
                    && let Some(b) = self.store.get(org, id).await?
                {
                    out.push((id.to_text(), b));
                }
            }
            match page.last() {
                Some((id, _)) if !done => after = Some(*id),
                _ => return Ok(out),
            }
        }
    }
}
