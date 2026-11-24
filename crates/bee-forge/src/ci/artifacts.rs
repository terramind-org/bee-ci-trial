//! Artifacts and caches: metadata rows on the ci surface, bytes in the blob store.
//!
//! Both follow reserve → upload (through a signed URL the server's HTTP surface serves, which
//! calls [`Ci::upload_artifact`] / [`Ci::upload_cache`]) → finalize/commit, which checks the
//! declared size and SHA-256 digest against what was uploaded (`INTEGRITY` otherwise).
//! Artifacts expire after their retention; caches are scoped by ref (a job reads its own ref,
//! then its base ref, then the default ref), evicted least-recently-used beyond the repo's
//! limit and dropped after the retention since last access.

use bee_proto::v1::{
    ErrorReason, Repo, WorkflowArtifact, WorkflowCache, WorkflowCacheSort, WorkflowCacheUsage,
    WorkflowStatus,
};

use super::convert::git_ref;
use super::engine::{Ci, job_of, repo_ref_of, run_of, status_of};
use super::model::{ArtifactRecord, CacheRecord};
use super::util::sha256_digest;
use super::{ARTIFACTS, CACHES, SURFACE};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::store::{Key, Order, Row, Scan};

/// Largest artifact or cache entry accepted.
pub const MAX_BLOB_BYTES: u64 = 10 * 1024 * 1024 * 1024;

fn artifact_row(repo_id: &str, r: &ArtifactRecord) -> Row {
    let a = r.artifact.clone().unwrap_or_default();
    Row::new(repo_id, a.id.clone())
        .k1(a.run_id.clone())
        .k2(a.name.clone())
        .n1(a.expires_at.as_ref().map(ts_ms).unwrap_or(0) as i64)
        .msg(r)
}

fn cache_row(repo_id: &str, r: &CacheRecord) -> Row {
    let c = r.cache.clone().unwrap_or_default();
    Row::new(repo_id, c.id.clone())
        .k1(c.r#ref.clone())
        .k2(c.key.clone())
        .n1(c.last_accessed_at.as_ref().map(ts_ms).unwrap_or(0) as i64)
        .msg(r)
}

fn valid_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 256
        || name.contains(['"', ':', '<', '>', '|', '*', '?', '\\', '/', '\r', '\n'])
    {
        return Err(ForgeError::invalid(
            "invalid_name",
            format!("bad artifact name {name:?}"),
        ));
    }
    Ok(())
}

impl Ci {
    async fn running_job(
        &self,
        cx: &Cx,
        repo: &Repo,
        job_id: &str,
    ) -> Result<super::model::JobRecord> {
        let j = self.job_record(cx, &repo.id, job_id).await?;
        if status_of(job_of(&j)) != WorkflowStatus::InProgress {
            return Err(ForgeError::invalid_state("the job is not running"));
        }
        Ok(j)
    }

    /// Reserves an artifact for a running job; returns it with a signed upload URL.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_artifact(
        &self,
        cx: &Cx,
        repo: &Repo,
        job_id: &str,
        name: &str,
        archive: bool,
        content_type: &str,
        retention_days: u32,
    ) -> Result<(WorkflowArtifact, String, u64)> {
        valid_name(name)?;
        let j = self.running_job(cx, repo, job_id).await?;
        let policy = self.effective_policy(cx, repo).await?;
        let max_days = policy.artifact_retention_days.max(1);
        let days = if retention_days == 0 {
            max_days
        } else {
            retention_days.min(max_days)
        };
        let run_id = job_of(&j).run_id.clone();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let taken = tx
            .scan_msgs::<ArtifactRecord>(
                &ARTIFACTS,
                &Scan::scope(repo.id.clone(), 1000).key(Key::K1(run_id.clone())),
            )
            .await?
            .into_iter()
            .any(|(a, _)| a.artifact.is_some_and(|a| a.name == name && !a.expired));
        if taken {
            return Err(ForgeError::already_exists(format!(
                "artifact {name} in this run"
            )));
        }
        let now = tx.now_ms();
        let art = WorkflowArtifact {
            id: tx.new_id()?,
            repo: repo_ref_of(&repo.id),
            name: name.to_string(),
            archive,
            content_type: if content_type.is_empty() {
                if archive {
                    "application/zip"
                } else {
                    "application/octet-stream"
                }
                .to_string()
            } else {
                content_type.to_string()
            },
            run_id,
            job_id: job_id.to_string(),
            head_ref: job_of(&j).head_ref.clone(),
            head_changeset: job_of(&j).head_changeset.clone(),
            retention_days: days,
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            expires_at: Some(ts(now + u64::from(days) * 86_400_000)),
            ..Default::default()
        };
        let rec = ArtifactRecord {
            artifact: Some(art.clone()),
            org_id: repo.org_id.clone(),
            ..Default::default()
        };
        tx.insert(&ARTIFACTS, &artifact_row(&repo.id, &rec)).await?;
        tx.audit(
            AuditDraft::new("artifact.create", &repo.org_id)
                .repo_id(&repo.id)
                .target_id(&art.id)
                .after(&serde_json::json!({ "name": name, "job": job_id }).to_string()),
        )
        .await?;
        tx.commit().await?;
        let exp = now + self.config.signed_url_ttl_ms;
        let url = self.urls.sign(
            &format!("/_ci/repos/{}/artifacts/{}/upload", repo.id, art.id),
            exp,
        );
        Ok((art, url, exp))
    }

    /// Stores uploaded artifact bytes (the upload URL's handler).
    pub async fn upload_artifact(
        &self,
        cx: &Cx,
        repo_id: &str,
        id: &str,
        bytes: Vec<u8>,
    ) -> Result<()> {
        if bytes.len() as u64 > MAX_BLOB_BYTES {
            return Err(ForgeError::too_large(
                "artifact_bytes",
                MAX_BLOB_BYTES,
                bytes.len() as u64,
            ));
        }
        let org = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((r, _)) = tx
                .get_msg::<ArtifactRecord>(&ARTIFACTS, repo_id, id)
                .await?
            else {
                return Err(ForgeError::not_found(format!("artifact {id}")));
            };
            if r.artifact.as_ref().is_some_and(|a| a.finalized) {
                return Err(ForgeError::invalid_state(
                    "the artifact is already finalized",
                ));
            }
            r.org_id
        };
        let digest = sha256_digest(&bytes);
        let size = bytes.len() as u64;
        let blob = self.blobs.put(&org, bytes).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut r, row)) = tx
            .get_msg::<ArtifactRecord>(&ARTIFACTS, repo_id, id)
            .await?
        else {
            return Err(ForgeError::not_found(format!("artifact {id}")));
        };
        let old = std::mem::replace(&mut r.blob_id, blob);
        r.uploaded_digest = digest;
        r.uploaded_size = size;
        tx.update(&ARTIFACTS, &artifact_row(repo_id, &r).version(row.version))
            .await?;
        tx.commit().await?;
        if !old.is_empty() {
            self.blobs.delete(&org, &old).await?;
        }
        Ok(())
    }

    /// Seals an uploaded artifact after checking its size and digest.
    pub async fn finalize_artifact(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        size: u64,
        digest: &str,
    ) -> Result<WorkflowArtifact> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut r, row)) = tx
            .get_msg::<ArtifactRecord>(&ARTIFACTS, &repo.id, id)
            .await?
        else {
            return Err(ForgeError::not_found(format!("artifact {id}")));
        };
        let mut a = r.artifact.clone().unwrap_or_default();
        if a.finalized {
            return Ok(a);
        }
        if r.blob_id.is_empty()
            || r.uploaded_size != size
            || (!digest.is_empty() && r.uploaded_digest != digest)
        {
            return Err(ForgeError::new(
                ErrorReason::Integrity,
                format!(
                    "uploaded {} bytes with digest {}, declared {size} bytes with digest {digest}",
                    r.uploaded_size, r.uploaded_digest
                ),
            ));
        }
        a.size_bytes = size;
        a.digest = r.uploaded_digest.clone();
        a.finalized = true;
        a.updated_at = Some(ts(tx.now_ms()));
        r.artifact = Some(a.clone());
        tx.update(&ARTIFACTS, &artifact_row(&repo.id, &r).version(row.version))
            .await?;
        tx.audit(
            AuditDraft::new("artifact.finalize", &repo.org_id)
                .repo_id(&repo.id)
                .target_id(id),
        )
        .await?;
        tx.commit().await?;
        Ok(a)
    }

    /// Artifacts of a repo or one run, optionally by name, newest first.
    pub async fn list_artifacts(
        &self,
        cx: &Cx,
        repo: &Repo,
        run_id: &str,
        name: &str,
    ) -> Result<Vec<WorkflowArtifact>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut scan = Scan::scope(repo.id.clone(), 10_000);
        if !run_id.is_empty() {
            scan = scan.key(Key::K1(run_id.to_string()));
        }
        let mut v: Vec<WorkflowArtifact> = tx
            .scan_msgs::<ArtifactRecord>(&ARTIFACTS, &scan)
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.artifact)
            .filter(|a| name.is_empty() || a.name == name)
            .collect();
        v.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(v)
    }

    /// One artifact.
    pub async fn get_artifact(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<WorkflowArtifact> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<ArtifactRecord>(&ARTIFACTS, &repo.id, id)
            .await?
            .and_then(|(r, _)| r.artifact)
            .ok_or_else(|| ForgeError::not_found(format!("artifact {id}")))
    }

    /// Deletes an artifact and its bytes.
    pub async fn delete_artifact(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((r, _)) = tx
            .get_msg::<ArtifactRecord>(&ARTIFACTS, &repo.id, id)
            .await?
        else {
            return Err(ForgeError::not_found(format!("artifact {id}")));
        };
        tx.delete(&ARTIFACTS, &repo.id, id).await?;
        tx.audit(
            AuditDraft::new("artifact.delete", &repo.org_id)
                .repo_id(&repo.id)
                .target_id(id),
        )
        .await?;
        tx.commit().await?;
        if !r.blob_id.is_empty() {
            self.blobs.delete(&r.org_id, &r.blob_id).await?;
        }
        Ok(())
    }

    /// A signed download URL; `INVALID_STATE` for expired or unfinalized artifacts.
    pub async fn artifact_download_url(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
    ) -> Result<(String, u64)> {
        let a = self.get_artifact(cx, repo, id).await?;
        if a.expired {
            return Err(ForgeError::invalid_state("the artifact expired"));
        }
        if !a.finalized {
            return Err(ForgeError::invalid_state("the artifact is not finalized"));
        }
        let exp = self.now() + self.config.signed_url_ttl_ms;
        Ok((
            self.urls.sign(
                &format!("/_ci/repos/{}/artifacts/{id}/download", repo.id),
                exp,
            ),
            exp,
        ))
    }

    /// The bytes of a finalized artifact (the download URL's handler).
    pub async fn artifact_bytes(&self, cx: &Cx, repo_id: &str, id: &str) -> Result<Vec<u8>> {
        let r = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.get_msg::<ArtifactRecord>(&ARTIFACTS, repo_id, id)
                .await?
                .map(|(r, _)| r)
                .ok_or_else(|| ForgeError::not_found(format!("artifact {id}")))?
        };
        let a = r.artifact.clone().unwrap_or_default();
        if a.expired || !a.finalized {
            return Err(ForgeError::invalid_state(
                "the artifact is expired or not finalized",
            ));
        }
        self.blobs
            .get(&r.org_id, &r.blob_id)
            .await?
            .ok_or_else(|| ForgeError::new(ErrorReason::MissingObject, "artifact bytes missing"))
    }

    /// Expires artifacts past their retention.
    pub(crate) async fn expire_artifacts(&self, cx: &Cx) -> Result<()> {
        let now = self.now();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan_msgs::<ArtifactRecord>(
                &ARTIFACTS,
                &Scan {
                    order: Order::N1,
                    n1_min: Some(1),
                    n1_max: Some(now as i64),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?;
        let mut blobs = Vec::new();
        for (mut r, row) in rows {
            let mut a = r.artifact.clone().unwrap_or_default();
            if a.expired {
                continue;
            }
            a.expired = true;
            a.expires_at = Some(ts(now));
            r.artifact = Some(a.clone());
            if !r.blob_id.is_empty() {
                blobs.push((r.org_id.clone(), std::mem::take(&mut r.blob_id)));
            }
            let mut nr = artifact_row(&row.scope, &r).version(row.version);
            nr.n1 = Some(0);
            tx.update(&ARTIFACTS, &nr).await?;
        }
        tx.commit().await?;
        for (org, b) in blobs {
            self.blobs.delete(&org, &b).await?;
        }
        Ok(())
    }

    // ----- caches -----------------------------------------------------------------------------

    async fn cache_refs(&self, cx: &Cx, repo: &Repo, job_id: &str) -> Result<Vec<String>> {
        let j = self.job_record(cx, &repo.id, job_id).await?;
        let rec = self.run_record(cx, &repo.id, &job_of(&j).run_id).await?;
        let event_ref = rec
            .event
            .as_ref()
            .map(|e| e.git_ref.clone())
            .unwrap_or_default();
        let default = git_ref(&rec.repo_info.clone().unwrap_or_default().default_branch);
        let mut refs = vec![event_ref];
        if !rec.base_ref.is_empty() {
            refs.push(git_ref(&rec.base_ref));
        }
        refs.push(default);
        refs.dedup();
        let _ = run_of(&rec);
        Ok(refs)
    }

    /// Finds the best cache entry for a job: exact key and version on the job's ref, base ref,
    /// default ref; then restore-key prefixes in order, newest first. `NOT_FOUND` on a miss.
    pub async fn lookup_cache(
        &self,
        cx: &Cx,
        repo: &Repo,
        job_id: &str,
        key: &str,
        version: &str,
        restore_keys: &[String],
    ) -> Result<(WorkflowCache, bool, String, u64)> {
        self.running_job(cx, repo, job_id).await?;
        let refs = self.cache_refs(cx, repo, job_id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let all: Vec<(CacheRecord, Row)> = tx
            .scan_msgs::<CacheRecord>(&CACHES, &Scan::scope(repo.id.clone(), 10_000))
            .await?
            .into_iter()
            .filter(|(c, _)| {
                c.cache
                    .as_ref()
                    .is_some_and(|c| c.committed && c.version == version)
            })
            .collect();
        let mut hit: Option<(CacheRecord, Row, bool)> = None;
        'outer: for r in &refs {
            let on_ref: Vec<&(CacheRecord, Row)> = all
                .iter()
                .filter(|(c, _)| c.cache.as_ref().is_some_and(|c| c.r#ref == *r))
                .collect();
            if let Some((c, row)) = on_ref
                .iter()
                .find(|(c, _)| c.cache.as_ref().is_some_and(|c| c.key == key))
            {
                hit = Some((c.clone(), row.clone(), true));
                break;
            }
            for rk in restore_keys {
                let best = on_ref
                    .iter()
                    .filter(|(c, _)| {
                        c.cache
                            .as_ref()
                            .is_some_and(|c| c.key.starts_with(rk.as_str()))
                    })
                    .max_by_key(|(c, _)| c.cache.as_ref().map(|c| c.id.clone()));
                if let Some((c, row)) = best {
                    hit = Some((c.clone(), row.clone(), false));
                    break 'outer;
                }
            }
        }
        let Some((mut c, row, exact)) = hit else {
            return Err(ForgeError::not_found(format!("cache miss for {key}")));
        };
        let now = tx.now_ms();
        let mut cache = c.cache.clone().unwrap_or_default();
        cache.last_accessed_at = Some(ts(now));
        c.cache = Some(cache.clone());
        tx.update(&CACHES, &cache_row(&repo.id, &c).version(row.version))
            .await?;
        tx.commit().await?;
        let exp = now + self.config.signed_url_ttl_ms;
        let url = self.urls.sign(
            &format!("/_ci/repos/{}/caches/{}/download", repo.id, cache.id),
            exp,
        );
        Ok((cache, exact, url, exp))
    }

    /// Reserves a key and version on the job's ref.
    pub async fn reserve_cache(
        &self,
        cx: &Cx,
        repo: &Repo,
        job_id: &str,
        key: &str,
        version: &str,
        size: u64,
    ) -> Result<(WorkflowCache, String, u64)> {
        if key.is_empty() || key.len() > 512 || key.contains(',') {
            return Err(ForgeError::invalid(
                "invalid_key",
                "cache keys are 1-512 characters without commas",
            ));
        }
        self.running_job(cx, repo, job_id).await?;
        let policy = self.effective_policy(cx, repo).await?;
        if size > policy.cache_max_size_bytes {
            return Err(ForgeError::new(
                ErrorReason::QuotaExceeded,
                format!(
                    "cache entry of {size} bytes exceeds the repo limit of {}",
                    policy.cache_max_size_bytes
                ),
            ));
        }
        let r = self
            .cache_refs(cx, repo, job_id)
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let exists = tx
            .scan_msgs::<CacheRecord>(
                &CACHES,
                &Scan::scope(repo.id.clone(), 10_000).key(Key::K2(key.to_string())),
            )
            .await?
            .into_iter()
            .any(|(c, _)| {
                c.cache
                    .is_some_and(|c| c.version == version && c.r#ref == r)
            });
        if exists {
            return Err(ForgeError::already_exists(format!("cache {key} on {r}")));
        }
        let now = tx.now_ms();
        let cache = WorkflowCache {
            id: tx.new_id()?,
            key: key.to_string(),
            version: version.to_string(),
            r#ref: r,
            size_bytes: size,
            committed: false,
            created_at: Some(ts(now)),
            last_accessed_at: Some(ts(now)),
        };
        tx.insert(
            &CACHES,
            &cache_row(
                &repo.id,
                &CacheRecord {
                    cache: Some(cache.clone()),
                    org_id: repo.org_id.clone(),
                    ..Default::default()
                },
            ),
        )
        .await?;
        tx.audit(
            AuditDraft::new("cache.reserve", &repo.org_id)
                .repo_id(&repo.id)
                .target_id(&cache.id)
                .after(&serde_json::json!({ "key": key, "ref": cache.r#ref }).to_string()),
        )
        .await?;
        tx.commit().await?;
        let exp = now + self.config.signed_url_ttl_ms;
        let url = self.urls.sign(
            &format!("/_ci/repos/{}/caches/{}/upload", repo.id, cache.id),
            exp,
        );
        Ok((cache, url, exp))
    }

    /// Stores uploaded cache bytes (the upload URL's handler).
    pub async fn upload_cache(
        &self,
        cx: &Cx,
        repo_id: &str,
        id: &str,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let org = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((r, _)) = tx.get_msg::<CacheRecord>(&CACHES, repo_id, id).await? else {
                return Err(ForgeError::not_found(format!("cache {id}")));
            };
            if r.cache.as_ref().is_some_and(|c| c.committed) {
                return Err(ForgeError::invalid_state(
                    "the cache entry is already committed",
                ));
            }
            r.org_id
        };
        let digest = sha256_digest(&bytes);
        let size = bytes.len() as u64;
        let blob = self.blobs.put(&org, bytes).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut r, row)) = tx.get_msg::<CacheRecord>(&CACHES, repo_id, id).await? else {
            return Err(ForgeError::not_found(format!("cache {id}")));
        };
        let old = std::mem::replace(&mut r.blob_id, blob);
        r.uploaded_digest = digest;
        r.uploaded_size = size;
        tx.update(&CACHES, &cache_row(repo_id, &r).version(row.version))
            .await?;
        tx.commit().await?;
        if !old.is_empty() {
            self.blobs.delete(&org, &old).await?;
        }
        Ok(())
    }

    /// Commits an uploaded cache entry, evicting least-recently-used entries over the limit.
    pub async fn commit_cache(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        size: u64,
        digest: &str,
    ) -> Result<WorkflowCache> {
        let policy = self.effective_policy(cx, repo).await?;
        let mut evicted = Vec::new();
        let cache;
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((mut r, row)) = tx.get_msg::<CacheRecord>(&CACHES, &repo.id, id).await? else {
                return Err(ForgeError::not_found(format!("cache {id}")));
            };
            let mut c = r.cache.clone().unwrap_or_default();
            if c.committed {
                return Err(ForgeError::invalid_state(
                    "the cache entry is already committed",
                ));
            }
            if r.blob_id.is_empty()
                || r.uploaded_size != size
                || (!digest.is_empty() && r.uploaded_digest != digest)
            {
                return Err(ForgeError::new(
                    ErrorReason::Integrity,
                    "uploaded cache bytes do not match the declared size or digest",
                ));
            }
            c.committed = true;
            c.size_bytes = size;
            r.cache = Some(c.clone());
            tx.update(&CACHES, &cache_row(&repo.id, &r).version(row.version))
                .await?;
            let mut all: Vec<(CacheRecord, Row)> = tx
                .scan_msgs::<CacheRecord>(&CACHES, &Scan::scope(repo.id.clone(), 10_000).by_n1())
                .await?
                .into_iter()
                .filter(|(c, _)| c.cache.as_ref().is_some_and(|c| c.committed))
                .collect();
            let mut total: u64 = all
                .iter()
                .map(|(c, _)| c.cache.as_ref().map(|c| c.size_bytes).unwrap_or(0))
                .sum();
            all.retain(|(c, _)| c.cache.as_ref().is_some_and(|x| x.id != id));
            for (c, row) in all {
                if total <= policy.cache_max_size_bytes {
                    break;
                }
                total -= c.cache.as_ref().map(|c| c.size_bytes).unwrap_or(0);
                tx.delete(&CACHES, &repo.id, &row.id).await?;
                evicted.push((c.org_id, c.blob_id));
            }
            tx.audit(
                AuditDraft::new("cache.commit", &repo.org_id)
                    .repo_id(&repo.id)
                    .target_id(id),
            )
            .await?;
            tx.commit().await?;
            cache = c;
        }
        for (org, b) in evicted {
            if !b.is_empty() {
                self.blobs.delete(&org, &b).await?;
            }
        }
        Ok(cache)
    }

    /// The bytes of a committed cache entry (the download URL's handler).
    pub async fn cache_bytes(&self, cx: &Cx, repo_id: &str, id: &str) -> Result<Vec<u8>> {
        let r = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.get_msg::<CacheRecord>(&CACHES, repo_id, id)
                .await?
                .map(|(r, _)| r)
                .ok_or_else(|| ForgeError::not_found(format!("cache {id}")))?
        };
        self.blobs
            .get(&r.org_id, &r.blob_id)
            .await?
            .ok_or_else(|| ForgeError::new(ErrorReason::MissingObject, "cache bytes missing"))
    }

    /// Cache entries of a repo.
    pub async fn list_caches(
        &self,
        cx: &Cx,
        repo: &Repo,
        key_prefix: &str,
        r: &str,
        sort: WorkflowCacheSort,
    ) -> Result<Vec<WorkflowCache>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut v: Vec<WorkflowCache> = tx
            .scan_msgs::<CacheRecord>(&CACHES, &Scan::scope(repo.id.clone(), 10_000))
            .await?
            .into_iter()
            .filter_map(|(c, _)| c.cache)
            .filter(|c| {
                c.key.starts_with(key_prefix)
                    && (r.is_empty() || c.r#ref == r || c.r#ref == git_ref(r))
            })
            .collect();
        match sort {
            WorkflowCacheSort::Created => v.sort_by(|a, b| b.id.cmp(&a.id)),
            WorkflowCacheSort::Size => v.sort_by_key(|c| std::cmp::Reverse(c.size_bytes)),
            _ => v.sort_by(|a, b| {
                b.last_accessed_at
                    .as_ref()
                    .map(ts_ms)
                    .cmp(&a.last_accessed_at.as_ref().map(ts_ms))
            }),
        }
        Ok(v)
    }

    /// Deletes one entry by id, or every entry with `key` (optionally on one ref).
    pub async fn delete_caches(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        key: &str,
        r: &str,
    ) -> Result<u32> {
        if id.is_empty() && key.is_empty() {
            return Err(ForgeError::missing("cache_id or key"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan_msgs::<CacheRecord>(&CACHES, &Scan::scope(repo.id.clone(), 10_000))
            .await?;
        let mut blobs = Vec::new();
        for (c, row) in rows {
            let x = c.cache.clone().unwrap_or_default();
            let hit = if !id.is_empty() {
                x.id == id
            } else {
                x.key == key && (r.is_empty() || x.r#ref == r || x.r#ref == git_ref(r))
            };
            if hit {
                tx.delete(&CACHES, &repo.id, &row.id).await?;
                blobs.push((c.org_id, c.blob_id));
            }
        }
        if blobs.is_empty() && !id.is_empty() {
            return Err(ForgeError::not_found(format!("cache {id}")));
        }
        tx.audit(
            AuditDraft::new("cache.delete", &repo.org_id)
                .repo_id(&repo.id)
                .after(
                    &serde_json::json!({ "id": id, "key": key, "ref": r, "count": blobs.len() })
                        .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        let n = blobs.len() as u32;
        for (org, b) in blobs {
            if !b.is_empty() {
                self.blobs.delete(&org, &b).await?;
            }
        }
        Ok(n)
    }

    /// Cache storage used by a repo.
    pub async fn cache_usage(&self, cx: &Cx, repo: &Repo) -> Result<WorkflowCacheUsage> {
        let policy = self.effective_policy(cx, repo).await?;
        let list = self
            .list_caches(cx, repo, "", "", WorkflowCacheSort::Unspecified)
            .await?;
        let active: Vec<&WorkflowCache> = list.iter().filter(|c| c.committed).collect();
        Ok(WorkflowCacheUsage {
            repo: repo_ref_of(&repo.id),
            active_size_bytes: active.iter().map(|c| c.size_bytes).sum(),
            active_count: active.len() as u64,
            limit_bytes: policy.cache_max_size_bytes,
        })
    }

    /// Drops cache entries not accessed within the retention.
    pub(crate) async fn expire_caches(&self, cx: &Cx) -> Result<()> {
        let cutoff = self
            .now()
            .saturating_sub(u64::from(self.config.cache_retention_days) * 86_400_000);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan_msgs::<CacheRecord>(
                &CACHES,
                &Scan {
                    order: Order::N1,
                    n1_max: Some(cutoff as i64),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut blobs = Vec::new();
        for (c, row) in rows {
            tx.delete(&CACHES, &row.scope, &row.id).await?;
            blobs.push((c.org_id, c.blob_id));
        }
        tx.commit().await?;
        for (org, b) in blobs {
            if !b.is_empty() {
                self.blobs.delete(&org, &b).await?;
            }
        }
        Ok(())
    }
}
