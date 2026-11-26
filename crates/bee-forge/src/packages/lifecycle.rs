//! Upload sessions, native publish, OCI blob rows, retention and cleanup, and the purge of
//! deleted packages and versions after the restore window.

use std::collections::{BTreeMap, BTreeSet};

use bee_proto::v1::{PackageOwnerKind, SubjectKind};
use bytes::Bytes;
use serde_json::json;

use super::hash::{md5_hex, sha1_hex, sha256_digest};
use super::manage::{NewFile, NewVersion, Owner, Packages, UploadTarget};
use super::model::{
    BlobRec, Eco, FileRec, NativeFile, OwnerSettingsRec, PackageRec, RetentionRec, UploadRec,
    VersionRec,
};
use super::ports::ProvenanceClaim;
use super::{
    BLOBREFS, BLOBS, COUNTS, DAILY, FILES, GRANTS, LINKS, OWNER, PACKAGES, PROVENANCE, SURFACE,
    TAGS, UPLOADS, VERSIONS, names,
};
use crate::community::common::{draft, emit};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::store::{Cursor, Row, Scan, Tx};

/// A file announced by a native publish.
#[derive(Clone, Debug)]
pub struct UploadFileSpec {
    /// Name within the version.
    pub name: String,
    /// Declared size.
    pub size: u64,
    /// Declared `sha256:<hex>` digest.
    pub digest: String,
    /// Media type.
    pub media_type: String,
}

async fn scan_all(tx: &mut Tx, t: &crate::store::TableDef, scope: &str) -> Result<Vec<Row>> {
    let mut out = Vec::new();
    let mut after: Option<Cursor> = None;
    loop {
        let rows = tx
            .scan(t, &Scan::scope(scope, 1000).after(after.clone()))
            .await?;
        let n = rows.len();
        if let Some(l) = rows.last() {
            after = Some(Cursor::after(l));
        }
        out.extend(rows);
        if n < 1000 {
            return Ok(out);
        }
    }
}

fn blob_key(owner_id: &str, digest: &str) -> String {
    format!("pkg/{owner_id}/{}", digest.replace(':', "/"))
}

impl Packages {
    // ---------- OCI blobs and upload sessions ----------

    /// The blob store key of a content-addressed blob of an owner.
    pub fn content_key(owner_id: &str, digest: &str) -> String {
        blob_key(owner_id, digest)
    }

    /// An OCI blob of a package.
    pub async fn blob(&self, cx: &Cx, p: &PackageRec, digest: &str) -> Result<Option<BlobRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        match tx.get(&BLOBS, &p.id, digest).await? {
            Some(r) => Ok(Some(r.decode()?)),
            None => Ok(None),
        }
    }

    /// Stores a blob: content first, then the row (idempotent per digest).
    pub async fn put_blob(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        digest: &str,
        data: Bytes,
    ) -> Result<BlobRec> {
        let key = blob_key(&owner.id, digest);
        self.ports.blobs.put(&key, data.clone()).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if let Some(r) = tx.get(&BLOBS, &p.id, digest).await? {
            return r.decode();
        }
        let rec = BlobRec {
            digest: digest.to_string(),
            size: data.len() as u64,
            blob_key: key.clone(),
            created_ms: tx.now_ms(),
        };
        tx.insert(&BLOBS, &Row::new(&p.id, digest).msg(&rec))
            .await?;
        tx.add_n1(&BLOBREFS, &owner.id, &key, 1).await?;
        tx.commit().await?;
        Ok(rec)
    }

    /// Makes a blob of `from` visible in `to` (cross-repo mount); `false` when `from` has no
    /// such blob.
    pub async fn mount_blob(
        &self,
        cx: &Cx,
        owner: &Owner,
        from: &PackageRec,
        to: &PackageRec,
        digest: &str,
    ) -> Result<bool> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some(src) = tx.get(&BLOBS, &from.id, digest).await? else {
            return Ok(false);
        };
        if tx.get(&BLOBS, &to.id, digest).await?.is_some() {
            return Ok(true);
        }
        let mut rec: BlobRec = src.decode()?;
        rec.created_ms = tx.now_ms();
        if from.owner_id != to.owner_id {
            return Ok(false);
        }
        tx.insert(&BLOBS, &Row::new(&to.id, digest).msg(&rec))
            .await?;
        tx.add_n1(&BLOBREFS, &owner.id, &rec.blob_key, 1).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Deletes a blob row; the content goes when nothing else references it. `false` when the
    /// package has no such blob.
    pub async fn delete_blob(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        digest: &str,
    ) -> Result<bool> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some(r) = tx.get(&BLOBS, &p.id, digest).await? else {
            return Ok(false);
        };
        let rec: BlobRec = r.decode()?;
        tx.delete(&BLOBS, &p.id, digest).await?;
        let dropped = if tx.add_n1(&BLOBREFS, &owner.id, &rec.blob_key, -1).await? <= 0 {
            vec![rec.blob_key]
        } else {
            vec![]
        };
        tx.commit().await?;
        self.drop_blobs(&dropped).await;
        Ok(true)
    }

    /// Starts an OCI blob upload session.
    pub async fn upload_start(&self, cx: &Cx, owner: &Owner, p: &PackageRec) -> Result<UploadRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let rec = UploadRec {
            id: tx.new_id()?,
            package_id: p.id.clone(),
            owner_id: owner.id.clone(),
            started_ms: now,
            expires_ms: now + self.upload_ms(),
            principal: cx.actor_id.clone(),
            ..Default::default()
        };
        tx.insert(
            &UPLOADS,
            &Row::new(&owner.id, &rec.id)
                .n1(rec.expires_ms as i64)
                .msg(&rec),
        )
        .await?;
        tx.commit().await?;
        Ok(rec)
    }

    /// An upload session of the package; `NOT_FOUND` when unknown or expired.
    pub async fn upload(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        id: &str,
    ) -> Result<UploadRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        match tx.get(&UPLOADS, &owner.id, id).await? {
            Some(r) => {
                let u: UploadRec = r.decode()?;
                if u.package_id != p.id || u.native || u.expires_ms <= now {
                    Err(ForgeError::not_found("upload session"))
                } else {
                    Ok(u)
                }
            }
            None => Err(ForgeError::not_found("upload session")),
        }
    }

    fn chunk_key(owner_id: &str, id: &str, n: u32) -> String {
        format!("pkg/{owner_id}/up/{id}/{n:08}")
    }

    /// Appends a chunk at `offset` (which must equal the bytes received so far).
    pub async fn upload_append(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        id: &str,
        offset: Option<u64>,
        data: Bytes,
    ) -> Result<UploadRec> {
        let u = self.upload(cx, owner, p, id).await?;
        if offset.is_some_and(|o| o != u.size) {
            return Err(ForgeError::invalid(
                "range_not_satisfiable",
                format!(
                    "the upload has {} bytes; the chunk starts at {}",
                    u.size,
                    offset.unwrap_or(0)
                ),
            ));
        }
        if u.size + data.len() as u64 > self.config.max_file_bytes {
            return Err(ForgeError::too_large(
                "blob size",
                self.config.max_file_bytes,
                u.size + data.len() as u64,
            ));
        }
        self.ports
            .blobs
            .put(&Self::chunk_key(&owner.id, id, u.chunks), data.clone())
            .await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some(row) = tx.get(&UPLOADS, &owner.id, id).await? else {
            return Err(ForgeError::not_found("upload session"));
        };
        let mut cur: UploadRec = row.decode()?;
        if cur.size != u.size {
            return Err(ForgeError::invalid(
                "range_not_satisfiable",
                "another chunk was appended concurrently",
            ));
        }
        cur.size += data.len() as u64;
        cur.chunks += 1;
        tx.update(
            &UPLOADS,
            &Row::new(&owner.id, id)
                .n1(cur.expires_ms as i64)
                .msg(&cur)
                .version(row.version),
        )
        .await?;
        tx.commit().await?;
        Ok(cur)
    }

    async fn assemble(&self, owner: &Owner, u: &UploadRec) -> Result<Vec<u8>> {
        let mut all = Vec::with_capacity(u.size as usize);
        for n in 0..u.chunks {
            match self
                .ports
                .blobs
                .get(&Self::chunk_key(&owner.id, &u.id, n))
                .await?
            {
                Some(c) => all.extend_from_slice(&c),
                None => {
                    return Err(ForgeError::new(
                        bee_proto::v1::ErrorReason::Integrity,
                        "an upload chunk is missing from the store",
                    ));
                }
            }
        }
        Ok(all)
    }

    /// Finishes a session: appends the final chunk, verifies `digest` and stores the blob.
    /// A wrong digest is `INVALID_ARGUMENT` / `digest_invalid` and the session stays open.
    pub async fn upload_finish(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        id: &str,
        last: Option<Bytes>,
        digest: &str,
    ) -> Result<BlobRec> {
        let mut u = self.upload(cx, owner, p, id).await?;
        if let Some(c) = last.filter(|c| !c.is_empty()) {
            u = self.upload_append(cx, owner, p, id, None, c).await?;
        }
        let all = self.assemble(owner, &u).await?;
        let actual = sha256_digest(&all);
        if actual != digest {
            return Err(ForgeError::invalid(
                "digest_invalid",
                format!("the uploaded content has digest {actual}, not {digest}"),
            ));
        }
        let rec = self
            .put_blob(cx, owner, p, digest, Bytes::from(all))
            .await?;
        self.upload_cancel(cx, owner, p, id).await?;
        Ok(rec)
    }

    /// Drops a session and its chunks.
    pub async fn upload_cancel(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        id: &str,
    ) -> Result<()> {
        let u = self.upload(cx, owner, p, id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.delete(&UPLOADS, &owner.id, id).await?;
        tx.commit().await?;
        for n in 0..u.chunks {
            let _ = self
                .ports
                .blobs
                .delete(&Self::chunk_key(&owner.id, id, n))
                .await;
        }
        Ok(())
    }

    // ---------- native publish ----------

    /// Starts a native publish: validates, reserves the upload and returns signed URLs.
    #[allow(clippy::too_many_arguments)]
    pub async fn begin_upload(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        version: &str,
        files: &[UploadFileSpec],
        metadata_json: &str,
    ) -> Result<(UploadRec, Vec<UploadTarget>)> {
        let eco = p.eco()?;
        if eco == Eco::Container {
            return Err(ForgeError::invalid(
                "unsupported_ecosystem",
                "container images are published with the OCI protocol",
            ));
        }
        names::validate_version(eco, version)?;
        if files.is_empty() {
            return Err(ForgeError::missing("files"));
        }
        if files.len() > self.config.max_files_per_version {
            return Err(ForgeError::too_large(
                "files per version",
                self.config.max_files_per_version as u64,
                files.len() as u64,
            ));
        }
        if !metadata_json.is_empty()
            && serde_json::from_str::<serde_json::Value>(metadata_json).is_err()
        {
            return Err(ForgeError::invalid(
                "bad_metadata",
                "`metadata_json` is not valid JSON",
            ));
        }
        let mut seen = BTreeSet::new();
        let mut total = 0u64;
        for f in files {
            if f.name.is_empty()
                || f.name.contains('/')
                || f.name.contains('\\')
                || f.name.starts_with('.')
            {
                return Err(ForgeError::invalid(
                    "bad_file_name",
                    format!("`{}` is not a valid file name", f.name),
                ));
            }
            if !names::is_digest(&f.digest) {
                return Err(ForgeError::invalid(
                    "bad_digest",
                    format!("`{}` is not a sha256 digest", f.digest),
                ));
            }
            if f.size > self.config.max_file_bytes {
                return Err(ForgeError::too_large(
                    "file size",
                    self.config.max_file_bytes,
                    f.size,
                ));
            }
            if !seen.insert(f.name.clone()) {
                return Err(ForgeError::invalid(
                    "duplicate_file",
                    format!("`{}` is listed twice", f.name),
                ));
            }
            total += f.size;
        }
        let key = names::version_key(eco, version);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        if Self::find_version(&mut tx, &p.id, &key).await?.is_some() {
            return Err(
                ForgeError::already_exists(format!("version {version} of {}", p.name))
                    .with_subcode("version_exists"),
            );
        }
        let settings = Self::owner_settings_in(&mut tx, &owner.id).await?;
        if settings.storage_quota_bytes > 0 {
            let used = Self::counter(&mut tx, &owner.id, "storage").await?;
            if used + total > settings.storage_quota_bytes {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::QuotaExceeded,
                    format!(
                        "{} would exceed its package storage quota of {} bytes",
                        owner.name, settings.storage_quota_bytes
                    ),
                ));
            }
        }
        let id = tx.new_id()?;
        let mut native = Vec::new();
        let mut targets = Vec::new();
        for (i, f) in files.iter().enumerate() {
            let k = format!("pkg/{}/up/{id}/{i:04}-{}", owner.id, f.name);
            let url = self.ports.signer.upload(&k, f.size, &f.media_type, now);
            targets.push(UploadTarget {
                name: f.name.clone(),
                url: url.url,
                headers: url.headers,
            });
            native.push(NativeFile {
                name: f.name.clone(),
                size: f.size,
                digest: f.digest.clone(),
                media_type: f.media_type.clone(),
                key: k,
            });
        }
        let rec = UploadRec {
            id: id.clone(),
            package_id: p.id.clone(),
            owner_id: owner.id.clone(),
            started_ms: now,
            expires_ms: now + 3_600_000,
            principal: cx.actor_id.clone(),
            native: true,
            version: version.to_string(),
            files: native,
            metadata_json: metadata_json.to_string(),
            ..Default::default()
        };
        tx.insert(
            &UPLOADS,
            &Row::new(&owner.id, &id).n1(rec.expires_ms as i64).msg(&rec),
        )
        .await?;
        tx.commit().await?;
        Ok((rec, targets))
    }

    /// The native upload session `id` of an owner.
    pub async fn native_upload(&self, cx: &Cx, owner: &Owner, id: &str) -> Result<UploadRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        match tx.get(&UPLOADS, &owner.id, id).await? {
            Some(r) => {
                let u: UploadRec = r.decode()?;
                if !u.native || u.expires_ms <= now {
                    Err(ForgeError::not_found("upload"))
                } else {
                    Ok(u)
                }
            }
            None => Err(ForgeError::not_found("upload")),
        }
    }

    /// Finishes a native publish: verifies every file against its declared size and digest and
    /// makes the version visible. `MISSING_OBJECT` when a file was not uploaded, `INTEGRITY` on
    /// a size or digest mismatch.
    pub async fn finalize_upload(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        u: &UploadRec,
        tags: &[String],
        provenance: Option<ProvenanceClaim>,
    ) -> Result<VersionRec> {
        let mut files = Vec::new();
        for f in &u.files {
            let Some(data) = self.ports.blobs.get(&f.key).await? else {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::MissingObject,
                    format!("`{}` was not uploaded", f.name),
                )
                .with_subcode("file_not_uploaded"));
            };
            if data.len() as u64 != f.size {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::Integrity,
                    format!("`{}` is {} bytes, declared {}", f.name, data.len(), f.size),
                )
                .with_subcode("size_mismatch"));
            }
            if sha256_digest(&data) != f.digest {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::Integrity,
                    format!("`{}` does not match its declared digest", f.name),
                )
                .with_subcode("digest_mismatch"));
            }
            files.push(NewFile {
                name: f.name.clone(),
                size: f.size,
                digest: f.digest.clone(),
                media_type: f.media_type.clone(),
                blob_key: f.key.clone(),
                sha1: sha1_hex(&data),
                md5: md5_hex(&data),
                sha512_b64: super::hash::sha512_b64(&data),
            });
        }
        let mut nv = NewVersion::new(&u.version);
        nv.digest = files.first().map(|f| f.digest.clone()).unwrap_or_default();
        nv.files = files;
        nv.tags = tags.to_vec();
        nv.metadata_json = u.metadata_json.clone();
        nv.provenance = provenance;
        let out = self.publish(cx, owner, p, nv).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.delete(&UPLOADS, &owner.id, &u.id).await?;
        tx.commit().await?;
        Ok(out.version)
    }

    // ---------- settings and retention ----------

    /// The owner's package settings.
    pub async fn owner_settings(&self, cx: &Cx, owner: &Owner) -> Result<OwnerSettingsRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::owner_settings_in(&mut tx, &owner.id).await
    }

    /// Updates the owner's package settings.
    pub async fn set_owner_settings(
        &self,
        cx: &Cx,
        owner: &Owner,
        forbid_public: Option<bool>,
        storage_quota_bytes: Option<u64>,
    ) -> Result<OwnerSettingsRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let row = tx.get(&OWNER, &owner.id, "settings").await?;
        let mut s: OwnerSettingsRec = match &row {
            Some(r) => r.decode()?,
            None => Default::default(),
        };
        if let Some(f) = forbid_public {
            s.forbid_public = f;
        }
        if let Some(q) = storage_quota_bytes {
            s.storage_quota_bytes = q;
        }
        tx.put(&OWNER, &Row::new(&owner.id, "settings").msg(&s))
            .await?;
        tx.audit(draft(
            "package.settings.update",
            owner.org_id(),
            "",
            SubjectKind::Package,
            &owner.id,
            &json!({"forbid_public": s.forbid_public, "storage_quota_bytes": s.storage_quota_bytes}),
        ))
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    fn retention_id(p: Option<&PackageRec>) -> String {
        match p {
            Some(p) => format!("retention:{}", p.id),
            None => "retention:*".to_string(),
        }
    }

    /// Sets the retention policy of a package, or the owner's default when `p` is `None`. An
    /// all-zero policy removes it.
    pub async fn set_retention(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: Option<&PackageRec>,
        policy: RetentionRec,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let id = Self::retention_id(p);
        if policy == RetentionRec::default() {
            tx.delete(&OWNER, &owner.id, &id).await?;
        } else {
            tx.put(&OWNER, &Row::new(&owner.id, &id).msg(&policy))
                .await?;
        }
        tx.audit(draft(
            "package.retention.set",
            owner.org_id(),
            p.map(|p| p.repo_id.as_str()).unwrap_or(""),
            SubjectKind::Package,
            p.map(|p| p.id.as_str()).unwrap_or(&owner.id),
            &json!({"keep_last": policy.keep_last, "untagged_days": policy.untagged_days, "older_than_days": policy.older_than_days}),
        ))
        .await?;
        tx.commit().await
    }

    /// Exactly the policy stored for `p` (the owner's default when `None`), without falling
    /// back.
    pub async fn retention_of(
        &self,
        cx: &Cx,
        owner_id: &str,
        p: Option<&PackageRec>,
    ) -> Result<Option<RetentionRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        match tx.get(&OWNER, owner_id, &Self::retention_id(p)).await? {
            Some(r) => Ok(Some(r.decode()?)),
            None => Ok(None),
        }
    }

    /// The policy that applies to `p`: its own, else the owner's default.
    pub async fn retention(
        &self,
        cx: &Cx,
        owner_id: &str,
        p: &PackageRec,
    ) -> Result<Option<RetentionRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for id in [Self::retention_id(Some(p)), Self::retention_id(None)] {
            if let Some(r) = tx.get(&OWNER, owner_id, &id).await? {
                return Ok(Some(r.decode()?));
            }
        }
        Ok(None)
    }

    /// Applies the retention policy of every live package of `owner`; returns how many
    /// versions were deleted (they stay restorable inside the window). Container blobs that no
    /// manifest references and that are older than a day are removed too.
    pub async fn apply_retention(&self, cx: &Cx, owner: &Owner) -> Result<usize> {
        let packages = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            scan_all(&mut tx, &PACKAGES, &owner.id).await?
        };
        let mut deleted = 0;
        for r in packages {
            let p: PackageRec = r.decode()?;
            if p.deleted_ms != 0 {
                continue;
            }
            if let Some(policy) = self.retention(cx, &owner.id, &p).await? {
                deleted += self.apply_policy(cx, owner, &p, &policy).await?;
            }
            if p.eco()? == Eco::Container {
                self.gc_blobs(cx, owner, &p).await?;
            }
        }
        Ok(deleted)
    }

    async fn apply_policy(
        &self,
        cx: &Cx,
        owner: &Owner,
        p: &PackageRec,
        policy: &RetentionRec,
    ) -> Result<usize> {
        let versions = self.all_versions(cx, p).await?;
        let tagged: BTreeSet<String> = self
            .tags(cx, p)
            .await?
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        let now = self.db.now_ms();
        let day = 86_400_000u64;
        let mut doomed = Vec::new();
        for (i, v) in versions.iter().enumerate() {
            let is_tagged = tagged.contains(&v.id);
            let age = now.saturating_sub(v.created_ms);
            let over_keep = policy.keep_last > 0 && i >= policy.keep_last as usize && !is_tagged;
            let stale_untagged = policy.untagged_days > 0
                && !is_tagged
                && age > u64::from(policy.untagged_days) * day;
            let too_old =
                policy.older_than_days > 0 && age > u64::from(policy.older_than_days) * day;
            if over_keep || stale_untagged || too_old {
                doomed.push(v.id.clone());
            }
        }
        let mut n = 0;
        for id in doomed {
            if self.delete_version(cx, owner, p, &id).await.is_ok() {
                n += 1;
            }
        }
        if n > 0 {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let payload = json!({"package": p.name, "versions_deleted": n});
            let subject = bee_proto::v1::SubjectRef {
                kind: SubjectKind::Package as i32,
                repo: crate::community::common::repo_ref_id(&p.repo_id),
                id: p.id.clone(),
            };
            emit(
                &mut tx,
                "package.retention_applied",
                owner.org_id(),
                &p.repo_id,
                subject,
                payload.clone(),
            )
            .await?;
            tx.audit(draft(
                "package.retention.apply",
                owner.org_id(),
                &p.repo_id,
                SubjectKind::Package,
                &p.id,
                &payload,
            ))
            .await?;
            tx.commit().await?;
        }
        Ok(n)
    }

    async fn gc_blobs(&self, cx: &Cx, owner: &Owner, p: &PackageRec) -> Result<usize> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let mut referenced = BTreeSet::new();
        for v in scan_all(&mut tx, &VERSIONS, &p.id).await? {
            for f in scan_all(&mut tx, &FILES, &v.id).await? {
                referenced.insert(f.decode::<FileRec>()?.digest);
            }
        }
        let mut dropped = Vec::new();
        let mut n = 0;
        for b in scan_all(&mut tx, &BLOBS, &p.id).await? {
            let rec: BlobRec = b.decode()?;
            if !referenced.contains(&rec.digest) && now.saturating_sub(rec.created_ms) > 86_400_000
            {
                tx.delete(&BLOBS, &p.id, &rec.digest).await?;
                if tx.add_n1(&BLOBREFS, &owner.id, &rec.blob_key, -1).await? <= 0 {
                    dropped.push(rec.blob_key);
                }
                n += 1;
            }
        }
        tx.commit().await?;
        self.drop_blobs(&dropped).await;
        Ok(n)
    }

    // ---------- purge ----------

    /// Removes the rows and content of one version inside `tx`; returns keys to delete.
    async fn purge_version_in(
        &self,
        tx: &mut Tx,
        owner_id: &str,
        v: &VersionRec,
    ) -> Result<Vec<String>> {
        let mut dropped = Vec::new();
        let mut size = 0u64;
        for f in scan_all(tx, &FILES, &v.id).await? {
            let rec: FileRec = f.decode()?;
            size += rec.size;
            tx.delete(&FILES, &v.id, &f.id).await?;
            if tx.add_n1(&BLOBREFS, owner_id, &rec.blob_key, -1).await? <= 0 {
                dropped.push(rec.blob_key);
            }
        }
        tx.delete(&PROVENANCE, &v.package_id, &v.id).await?;
        tx.delete(&COUNTS, &v.package_id, &v.id).await?;
        for d in scan_all(tx, &DAILY, &v.package_id).await? {
            if d.id.ends_with(&format!(":{}", v.id)) {
                tx.delete(&DAILY, &v.package_id, &d.id).await?;
            }
        }
        if size > 0 {
            tx.add_n1(&COUNTS, owner_id, "storage", -(size as i64))
                .await?;
        }
        tx.delete(&VERSIONS, &v.package_id, &v.id).await?;
        Ok(dropped)
    }

    async fn purge_package(&self, cx: &Cx, p: &PackageRec) -> Result<()> {
        loop {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let rows = tx.scan(&VERSIONS, &Scan::scope(&p.id, 100)).await?;
            if rows.is_empty() {
                break;
            }
            let mut dropped = Vec::new();
            for r in rows {
                let v: VersionRec = r.decode()?;
                dropped.extend(self.purge_version_in(&mut tx, &p.owner_id, &v).await?);
            }
            tx.commit().await?;
            self.drop_blobs(&dropped).await;
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut dropped = Vec::new();
        for b in scan_all(&mut tx, &BLOBS, &p.id).await? {
            let rec: BlobRec = b.decode()?;
            tx.delete(&BLOBS, &p.id, &b.id).await?;
            if tx.add_n1(&BLOBREFS, &p.owner_id, &rec.blob_key, -1).await? <= 0 {
                dropped.push(rec.blob_key);
            }
        }
        for t in [&TAGS, &GRANTS, &COUNTS, &DAILY] {
            for r in scan_all(&mut tx, t, &p.id).await? {
                tx.delete(t, &p.id, &r.id).await?;
            }
        }
        tx.delete(&OWNER, &p.owner_id, &Self::retention_id(Some(p)))
            .await?;
        if !p.repo_id.is_empty() {
            tx.delete(&LINKS, &p.repo_id, &p.id).await?;
        }
        tx.delete(&PACKAGES, &p.owner_id, &p.id).await?;
        tx.commit().await?;
        self.drop_blobs(&dropped).await;
        Ok(())
    }

    /// Removes deleted packages and versions past the restore window, and expired upload
    /// sessions with their staged content. Returns how many packages, versions and sessions
    /// were purged.
    pub async fn purge_expired(&self, cx: &Cx) -> Result<(usize, usize, usize)> {
        let now = self.db.now_ms();
        let cutoff =
            now.saturating_sub(u64::from(self.config.restore_window_days) * 86_400_000) as i64;
        let (mut pk, mut vs, mut up) = (0, 0, 0);
        let packages = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan(
                &PACKAGES,
                &Scan {
                    n1_min: Some(1),
                    n1_max: Some(cutoff),
                    limit: 200,
                    ..Scan::default()
                }
                .by_n1(),
            )
            .await?
        };
        for r in packages {
            let p: PackageRec = r.decode()?;
            if p.deleted_ms != 0 {
                self.purge_package(cx, &p).await?;
                pk += 1;
            }
        }
        let versions = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan(
                &VERSIONS,
                &Scan {
                    n1_min: Some(1),
                    n1_max: Some(cutoff),
                    limit: 500,
                    ..Scan::default()
                }
                .by_n1(),
            )
            .await?
        };
        for r in versions {
            let v: VersionRec = r.decode()?;
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((ver, _)) = Self::load_version(&mut tx, &v.package_id, &v.id).await? else {
                continue;
            };
            if ver.deleted_ms == 0 {
                continue;
            }
            let dropped = self.purge_version_in(&mut tx, &ver.owner_id, &ver).await?;
            tx.commit().await?;
            self.drop_blobs(&dropped).await;
            vs += 1;
        }
        let sessions = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan(
                &UPLOADS,
                &Scan {
                    n1_min: Some(1),
                    n1_max: Some(now as i64),
                    limit: 200,
                    ..Scan::default()
                }
                .by_n1(),
            )
            .await?
        };
        for r in sessions {
            let u: UploadRec = r.decode()?;
            let mut keys: Vec<String> = u.files.iter().map(|f| f.key.clone()).collect();
            for n in 0..u.chunks {
                keys.push(Self::chunk_key(&u.owner_id, &u.id, n));
            }
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.delete(&UPLOADS, &u.owner_id, &u.id).await?;
            tx.commit().await?;
            self.drop_blobs(&keys).await;
            up += 1;
        }
        Ok((pk, vs, up))
    }

    /// One maintenance pass: purge, then retention for every owner that has a policy.
    pub async fn maintain(&self, cx: &Cx) -> Result<()> {
        self.purge_expired(cx).await?;
        let owners: BTreeMap<String, ()> = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan(
                &OWNER,
                &Scan {
                    limit: 10_000,
                    ..Scan::default()
                },
            )
            .await?
            .into_iter()
            .filter(|r| r.id.starts_with("retention:"))
            .map(|r| (r.scope, ()))
            .collect()
        };
        for id in owners.into_keys() {
            let owner = match self.orgs.org_any(cx, &id).await {
                Ok(o) => Owner {
                    id: o.id,
                    kind: PackageOwnerKind::Org,
                    name: o.name,
                },
                Err(_) => Owner {
                    name: id.clone(),
                    id,
                    kind: PackageOwnerKind::User,
                },
            };
            self.apply_retention(cx, &owner).await?;
        }
        Ok(())
    }
}

impl Packages {
    /// Live versions whose OCI `subject` is `digest` (the referrers of a manifest).
    pub async fn referrers(
        &self,
        cx: &Cx,
        p: &PackageRec,
        digest: &str,
    ) -> Result<Vec<VersionRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(
                &VERSIONS,
                &Scan::scope(&p.id, 1000).key(crate::store::Key::K2(digest.to_string())),
            )
            .await?;
        let mut out = Vec::new();
        for r in rows {
            let v: VersionRec = r.decode()?;
            if v.deleted_ms == 0 {
                out.push(v);
            }
        }
        Ok(out)
    }
}
