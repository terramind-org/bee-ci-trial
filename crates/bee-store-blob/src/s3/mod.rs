//! [`S3BlobStore`]: the blob store on any S3-compatible service (SPEC §6.1, §6.5, §10.5).
//!
//! # Mapping
//!
//! - Keys follow [`bee_store::blob_key`] under an optional configured prefix, so a bucket
//!   can be shared: `<prefix>o/<org>/<kind>/<xx>/<digest>`. Quarantined bytes live under
//!   `<prefix>q/…`; `get`, `get_range` and `has` only ever look at `o/`.
//! - Bytes are stored raw (not compressed), so a presigned GET serves exactly the bytes the
//!   id hashes, which is what SPEC §10.5 needs: the client verifies them against the id.
//! - The hot and cold classes map to S3 storage classes ([`S3Config::hot_class`],
//!   [`S3Config::cold_class`]; `STANDARD` and `STANDARD_IA` by default). A class change is a
//!   server-side copy onto the same key. Use a class that serves reads synchronously
//!   (`STANDARD_IA`, `GLACIER_IR`), not an archive class.
//! - `written_at_ms` is the object's `LastModified` (the service's clock, one second
//!   resolution). A re-`put` refreshes it; class changes and quarantine copies do too, which
//!   can only delay GC.
//! - `put` is an `If-None-Match: *` conditional PUT (insert-or-ignore) where the service
//!   supports it: an existing object is not re-uploaded, only touched (a self-copy) to
//!   refresh its time and apply the requested class; replacing damaged bytes therefore takes
//!   a `delete` first (the disk store, which overwrites, needs none). Services that reject the
//!   header are detected once and get plain PUTs. Blobs of [`S3Config::multipart_threshold`] or more are
//!   uploaded in parts of [`S3Config::part_size`], four at a time.
//! - Requests are signed with SigV4 and retried (connection errors, timeouts, throttling,
//!   5xx) with exponential backoff and jitter within a per-attempt timeout and an overall
//!   deadline ([`RetryPolicy`]).
//! - `delete` with `written_before_ms` checks the time with a HEAD and then deletes; S3 has no
//!   conditional delete, so a re-put landing between the two is lost. GC's 24-hour orphan
//!   grace (SPEC §6.4) is what makes that window harmless.
//! - Signed GET URLs for SPEC §10.5: [`S3BlobStore::presign_get`].

mod client;
mod dates;
mod sigv4;
mod xml;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bee_core::{BeeError, ObjectId, Result, Subcode};
use bee_store::{AccessCounters, BlobInfo, BlobStore, OrgId, StorageClass, blob_key};
use bytes::Bytes;
use http::Method;
use tokio::task::JoinSet;

use self::client::{ClientConfig, Request, S3Client, Target};
use crate::common::{
    clamp_range, id_from_digest_hex, internal, listing_kinds, unavailable, verify_bytes,
};

/// S3's minimum size of every multipart part but the last.
const MIN_PART_SIZE: u64 = 5 * 1024 * 1024;
/// S3's maximum number of parts in one upload.
const MAX_PARTS: u64 = 10_000;
/// Longest a SigV4 presigned URL can live.
const MAX_PRESIGN_SECS: u64 = 7 * 24 * 60 * 60;
/// Parts uploaded at once.
const PART_CONCURRENCY: usize = 4;
/// `has` HEAD requests in flight at once.
const HAS_CONCURRENCY: usize = 16;

/// Access keys for SigV4.
#[derive(Clone, PartialEq, Eq)]
pub struct S3Credentials {
    /// The access key id.
    pub access_key_id: String,
    /// The secret access key.
    pub secret_access_key: String,
    /// The session token of temporary credentials.
    pub session_token: Option<String>,
}

impl S3Credentials {
    /// Long-lived credentials.
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        S3Credentials {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: None,
        }
    }

    /// Credentials from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and the optional
    /// `AWS_SESSION_TOKEN`; `None` when the first two are not both set.
    pub fn from_env() -> Option<S3Credentials> {
        Some(S3Credentials {
            access_key_id: std::env::var("AWS_ACCESS_KEY_ID").ok()?,
            secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY").ok()?,
            session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
        })
    }
}

impl fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Retry, timeout and deadline rules for S3 requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt.
    pub max_retries: u32,
    /// Backoff ceiling of the first retry; it doubles each retry.
    pub base_backoff: Duration,
    /// Largest backoff ceiling.
    pub max_backoff: Duration,
    /// Longest one attempt may take.
    pub request_timeout: Duration,
    /// Longest one request may take in total, retries and sleeps included.
    pub deadline: Duration,
}

impl Default for RetryPolicy {
    fn default() -> RetryPolicy {
        RetryPolicy {
            max_retries: 4,
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
            deadline: Duration::from_secs(300),
        }
    }
}

/// Settings of an [`S3BlobStore`].
#[derive(Clone, Debug)]
pub struct S3Config {
    /// Service endpoint, `http(s)://host[:port]`; `None` for AWS in [`S3Config::region`].
    pub endpoint: Option<String>,
    /// Signing region (`us-east-1` for MinIO, `auto` for R2).
    pub region: String,
    /// Bucket name.
    pub bucket: String,
    /// Key prefix inside the bucket (empty, or a path such as `bee/prod`).
    pub prefix: String,
    /// Address the bucket in the path (`endpoint/bucket/key`) instead of the host
    /// (`bucket.endpoint/key`). Required by MinIO; defaults to true when an endpoint is set.
    pub path_style: bool,
    /// Signing credentials.
    pub credentials: S3Credentials,
    /// Storage class of the hot class.
    pub hot_class: String,
    /// Storage class of the cold class; must differ from `hot_class`.
    pub cold_class: String,
    /// Use `If-None-Match: *` for puts where the service accepts it.
    pub conditional_put: bool,
    /// Blobs at least this large are uploaded as multipart; at least [`MIN_PART_SIZE`]
    /// (5 MiB).
    pub multipart_threshold: u64,
    /// Multipart part size; at least 5 MiB, raised automatically for blobs that would need
    /// more than 10,000 parts.
    pub part_size: u64,
    /// Retries, timeouts and deadline.
    pub retry: RetryPolicy,
}

impl S3Config {
    /// AWS defaults for `bucket` in `region`: virtual-host addressing, `STANDARD` hot and
    /// `STANDARD_IA` cold, conditional puts, multipart from 64 MiB in 16 MiB parts.
    pub fn new(
        bucket: impl Into<String>,
        region: impl Into<String>,
        credentials: S3Credentials,
    ) -> S3Config {
        S3Config {
            endpoint: None,
            region: region.into(),
            bucket: bucket.into(),
            prefix: String::new(),
            path_style: false,
            credentials,
            hot_class: "STANDARD".to_string(),
            cold_class: "STANDARD_IA".to_string(),
            conditional_put: true,
            multipart_threshold: 64 * 1024 * 1024,
            part_size: 16 * 1024 * 1024,
            retry: RetryPolicy::default(),
        }
    }

    /// Points the store at a custom endpoint (MinIO, R2, a local stub) with path-style
    /// addressing.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> S3Config {
        self.endpoint = Some(endpoint.into());
        self.path_style = true;
        self
    }

    fn invalid(why: impl Into<String>) -> BeeError {
        BeeError::invalid(Subcode::InvalidField, format!("S3 config: {}", why.into()))
    }
}

/// A blob store on S3-compatible object storage. See the [module docs](self).
#[derive(Clone)]
pub struct S3BlobStore {
    client: S3Client,
    hot_class: String,
    cold_class: String,
    prefix: String,
    multipart_threshold: u64,
    part_size: u64,
    conditional_put: Arc<AtomicBool>,
    counters: Arc<AccessCounters>,
}

impl fmt::Debug for S3BlobStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3BlobStore")
            .field("client", &self.client)
            .field("prefix", &self.prefix)
            .finish()
    }
}

/// What a HEAD or list entry tells about an object.
#[derive(Clone, Debug)]
struct Head {
    len: u64,
    storage_class: String,
    last_modified_ms: u64,
}

impl S3BlobStore {
    /// Builds the store. Nothing is sent: the bucket must exist (see
    /// [`S3BlobStore::ensure_bucket`]).
    pub fn new(config: S3Config) -> Result<S3BlobStore> {
        if config.bucket.is_empty() || config.bucket.contains(['/', ' ']) {
            return Err(S3Config::invalid("the bucket name is empty or malformed"));
        }
        if config.region.is_empty() {
            return Err(S3Config::invalid("the region is empty"));
        }
        if config.hot_class == config.cold_class {
            return Err(S3Config::invalid(
                "hot_class and cold_class must differ, or the class of an object is unknowable",
            ));
        }
        if config.part_size < MIN_PART_SIZE || config.multipart_threshold < MIN_PART_SIZE {
            return Err(S3Config::invalid(
                "part_size and multipart_threshold must be at least 5 MiB",
            ));
        }
        let target = match &config.endpoint {
            Some(endpoint) => Target::parse(endpoint)?,
            None => Target {
                scheme: "https".to_string(),
                authority: format!("s3.{}.amazonaws.com", config.region),
            },
        };
        let trimmed = config.prefix.trim_matches('/');
        let prefix = if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}/")
        };
        Ok(S3BlobStore {
            client: S3Client::new(ClientConfig {
                target,
                region: config.region,
                bucket: config.bucket,
                path_style: config.path_style,
                credentials: config.credentials,
                retry: config.retry,
            }),
            hot_class: config.hot_class,
            cold_class: config.cold_class,
            prefix,
            multipart_threshold: config.multipart_threshold,
            part_size: config.part_size,
            conditional_put: Arc::new(AtomicBool::new(config.conditional_put)),
            counters: Arc::new(AccessCounters::new()),
        })
    }

    /// Counts into `counters` instead of the store's own.
    pub fn with_counters(mut self, counters: Arc<AccessCounters>) -> S3BlobStore {
        self.counters = counters;
        self
    }

    /// The counters.
    pub fn counters(&self) -> &Arc<AccessCounters> {
        &self.counters
    }

    /// Creates the bucket unless it already exists (for tests and bootstrap; production
    /// buckets are usually provisioned elsewhere).
    pub async fn ensure_bucket(&self) -> Result<()> {
        let mut req = Request::new(Method::PUT, None);
        let region = &self.client.config().region;
        if region != "us-east-1" && region != "auto" {
            req = req.body(format!(
                "<CreateBucketConfiguration><LocationConstraint>{region}</LocationConstraint></CreateBucketConfiguration>"
            ));
        }
        let response = self.client.send(&req).await?;
        match (response.status.as_u16(), response.error_code().as_deref()) {
            (200, _) | (409, Some("BucketAlreadyOwnedByYou")) => Ok(()),
            _ => Err(response.error_for("CreateBucket")),
        }
    }

    /// A presigned GET URL for the servable bytes of `id`, valid for `ttl` (whole seconds, at
    /// least one, at most seven days), signed at the current time. It does not check that the
    /// bytes exist or are servable; [`crate::S3UrlIssuer`] does.
    pub fn presign_get(&self, org: OrgId, id: &ObjectId, ttl: Duration) -> Result<String> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.presign_get_at(org, id, ttl, now_ms)
    }

    /// [`S3BlobStore::presign_get`] signed at `now_ms` (Unix milliseconds), for tests.
    pub fn presign_get_at(
        &self,
        org: OrgId,
        id: &ObjectId,
        ttl: Duration,
        now_ms: u64,
    ) -> Result<String> {
        let secs = ttl.as_secs();
        if secs == 0 || secs > MAX_PRESIGN_SECS {
            return Err(S3Config::invalid(
                "a presigned URL lives between 1 second and 7 days",
            ));
        }
        // Only the servable key is ever presigned, never the quarantine prefix.
        let key = self.key(org, id, false);
        Ok(self.client.presign_get(&key, secs, now_ms / 1000))
    }

    fn key(&self, org: OrgId, id: &ObjectId, quarantined: bool) -> String {
        format!("{}{}", self.prefix, blob_key(org, id, quarantined))
    }

    fn class_name(&self, class: StorageClass) -> &str {
        match class {
            StorageClass::Hot => &self.hot_class,
            StorageClass::Cold => &self.cold_class,
        }
    }

    fn class_of(&self, storage_class: &str) -> StorageClass {
        if storage_class == self.cold_class {
            StorageClass::Cold
        } else {
            StorageClass::Hot
        }
    }

    fn info(&self, head: &Head, quarantined: bool) -> BlobInfo {
        BlobInfo {
            len: head.len,
            class: self.class_of(&head.storage_class),
            quarantined,
            written_at_ms: head.last_modified_ms,
        }
    }

    fn storage_header(&self, req: Request, storage_class: &str) -> Request {
        if storage_class == "STANDARD" {
            req
        } else {
            req.header("x-amz-storage-class", storage_class)
        }
    }

    async fn head(&self, key: &str) -> Result<Option<Head>> {
        let response = self
            .client
            .send(&Request::new(Method::HEAD, Some(key)))
            .await?;
        match response.status.as_u16() {
            200 => {
                let len = response
                    .header("content-length")
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| internal("S3 HEAD answered without a content length"))?;
                Ok(Some(Head {
                    len,
                    storage_class: response
                        .header("x-amz-storage-class")
                        .unwrap_or("STANDARD")
                        .to_string(),
                    last_modified_ms: response
                        .header("last-modified")
                        .and_then(dates::parse_http_date_ms)
                        .unwrap_or(0),
                }))
            }
            404 => Ok(None),
            _ => Err(response.error_for("HEAD")),
        }
    }

    async fn delete_key(&self, key: &str) -> Result<()> {
        self.client
            .expect(
                "DELETE",
                &Request::new(Method::DELETE, Some(key)),
                &[200, 204],
            )
            .await
            .map(|_| ())
    }

    /// Server-side copy of `src` onto `dst` with `storage_class`. A copy onto itself replaces
    /// metadata with a fresh touch marker, which S3 requires for a self-copy and which
    /// refreshes `LastModified`.
    async fn copy(&self, src: &str, dst: &str, storage_class: &str) -> Result<()> {
        let source = format!(
            "/{}/{}",
            self.client.config().bucket,
            sigv4::uri_encode(src, true)
        );
        let mut req = Request::new(Method::PUT, Some(dst)).header("x-amz-copy-source", source);
        if src == dst {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            req = req
                .header("x-amz-metadata-directive", "REPLACE")
                .header("x-amz-meta-bee-touch", now.to_string());
        }
        // Copies default to STANDARD, so the class is always stated.
        let req = req.header("x-amz-storage-class", storage_class);
        self.client
            .expect("CopyObject", &req, &[200])
            .await
            .map(|_| ())
    }

    async fn put_single(&self, key: &str, bytes: Bytes, storage_class: &str) -> Result<()> {
        for _ in 0..4 {
            let conditional = self.conditional_put.load(Ordering::Relaxed);
            let mut req = self.storage_header(
                Request::new(Method::PUT, Some(key)).body(bytes.clone()),
                storage_class,
            );
            if conditional {
                req = req.header("if-none-match", "*");
            }
            let response = self.client.send(&req).await?;
            match response.status.as_u16() {
                200 | 201 => return Ok(()),
                // Already stored; the bytes are the same by construction. Refresh the time.
                412 => return self.copy(key, key, storage_class).await,
                // A concurrent put of the same key is in flight; look again.
                409 if conditional => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                400 | 501
                    if conditional
                        && matches!(
                            response.error_code().as_deref(),
                            Some("NotImplemented" | "InvalidArgument")
                        ) =>
                {
                    self.conditional_put.store(false, Ordering::Relaxed);
                }
                _ => return Err(response.error_for("PutObject")),
            }
        }
        Err(unavailable(
            "S3 PutObject kept conflicting with a concurrent put",
        ))
    }

    async fn put_multipart(&self, key: &str, bytes: Bytes, storage_class: &str) -> Result<()> {
        let created = self
            .client
            .expect(
                "CreateMultipartUpload",
                &self.storage_header(
                    Request::new(Method::POST, Some(key)).query("uploads", ""),
                    storage_class,
                ),
                &[200],
            )
            .await?;
        let upload_id = xml::first(&created.text(), "UploadId")
            .map(xml::unescape)
            .ok_or_else(|| internal("S3 CreateMultipartUpload answered without an UploadId"))?;
        let outcome = async {
            let etags = self.upload_parts(key, &upload_id, &bytes).await?;
            self.complete_multipart(key, &upload_id, &etags, bytes.len() as u64)
                .await
        }
        .await;
        if outcome.is_err() {
            // Best effort: parts left behind are billed until the upload is aborted.
            let abort = Request::new(Method::DELETE, Some(key)).query("uploadId", upload_id);
            let _ = self.client.send(&abort).await;
        }
        outcome
    }

    async fn upload_parts(&self, key: &str, upload_id: &str, bytes: &Bytes) -> Result<Vec<String>> {
        let len = bytes.len() as u64;
        let part_size = self.part_size.max(len.div_ceil(MAX_PARTS));
        let count = len.div_ceil(part_size);
        let mut etags: BTreeMap<u64, String> = BTreeMap::new();
        let mut running: JoinSet<Result<(u64, String)>> = JoinSet::new();
        let mut failure = None;
        for n in 1..=count {
            while running.len() >= PART_CONCURRENCY {
                collect_part(running.join_next().await, &mut etags, &mut failure);
            }
            if failure.is_some() {
                break;
            }
            let start = ((n - 1) * part_size) as usize;
            let end = (n * part_size).min(len) as usize;
            let part = bytes.slice(start..end);
            let client = self.client.clone();
            let (key, upload_id) = (key.to_string(), upload_id.to_string());
            running.spawn(async move {
                let req = Request::new(Method::PUT, Some(&key))
                    .query("partNumber", n.to_string())
                    .query("uploadId", upload_id)
                    .body(part);
                let response = client.expect("UploadPart", &req, &[200]).await?;
                let etag = response
                    .header("etag")
                    .map(str::to_string)
                    .ok_or_else(|| internal("S3 UploadPart answered without an ETag"))?;
                Ok((n, etag))
            });
        }
        while let Some(done) = running.join_next().await {
            collect_part(Some(done), &mut etags, &mut failure);
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(etags.into_values().collect()),
        }
    }

    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        etags: &[String],
        len: u64,
    ) -> Result<()> {
        let mut body = String::from("<CompleteMultipartUpload>");
        for (i, etag) in etags.iter().enumerate() {
            body.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{etag}</ETag></Part>",
                i + 1
            ));
        }
        body.push_str("</CompleteMultipartUpload>");
        let req = Request::new(Method::POST, Some(key))
            .query("uploadId", upload_id)
            .body(body);
        let response = self.client.send(&req).await?;
        if response.status.as_u16() == 200 && !response.is_error_in_body() {
            return Ok(());
        }
        // A retried complete after a lost answer finds the upload gone, and the object there.
        if response.error_code().as_deref() == Some("NoSuchUpload")
            && self.head(key).await?.is_some_and(|h| h.len == len)
        {
            return Ok(());
        }
        Err(internal(format!(
            "S3 CompleteMultipartUpload failed: HTTP {} {}",
            response.status.as_u16(),
            response.text()
        )))
    }

    /// Lists up to `limit` objects of one kind under `o/` or `q/`, in key order after
    /// `start_after`.
    async fn list_stream(
        &self,
        org: OrgId,
        kind: bee_core::ObjectKind,
        quarantined: bool,
        start_after: Option<&ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, Head)>> {
        let prefix = format!(
            "{}{}/{}/{}/",
            self.prefix,
            if quarantined { "q" } else { "o" },
            org.0,
            kind.text_tag()
        );
        let mut out: Vec<(ObjectId, Head)> = Vec::new();
        let mut token: Option<String> = None;
        let mut first_start = start_after.map(|id| self.key(org, id, quarantined));
        while out.len() < limit {
            let mut req = Request::new(Method::GET, None)
                .query("list-type", "2")
                .query("prefix", prefix.clone())
                .query("max-keys", (limit - out.len()).min(1000).to_string());
            if let Some(t) = &token {
                req = req.query("continuation-token", t.clone());
            } else if let Some(s) = first_start.take() {
                req = req.query("start-after", s);
            }
            let response = self.client.expect("ListObjectsV2", &req, &[200]).await?;
            let text = response.text();
            for entry in xml::blocks(&text, "Contents") {
                let key = xml::first(entry, "Key")
                    .map(xml::unescape)
                    .unwrap_or_default();
                let Some(id) = key
                    .rsplit('/')
                    .next()
                    .and_then(|digest| id_from_digest_hex(kind, digest))
                else {
                    continue;
                };
                out.push((
                    id,
                    Head {
                        len: xml::first(entry, "Size")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0),
                        storage_class: xml::first(entry, "StorageClass")
                            .unwrap_or("STANDARD")
                            .to_string(),
                        last_modified_ms: xml::first(entry, "LastModified")
                            .and_then(dates::parse_iso8601_ms)
                            .unwrap_or(0),
                    },
                ));
            }
            match (
                xml::first(&text, "IsTruncated"),
                xml::first(&text, "NextContinuationToken"),
            ) {
                (Some("true"), Some(next)) => token = Some(xml::unescape(next)),
                _ => break,
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    async fn locate(&self, org: OrgId, id: &ObjectId) -> Result<Option<(String, Head, bool)>> {
        for quarantined in [false, true] {
            let key = self.key(org, id, quarantined);
            if let Some(head) = self.head(&key).await? {
                return Ok(Some((key, head, quarantined)));
            }
        }
        Ok(None)
    }

    /// Copies `from` to `to` keeping the storage class, checks the copy, then deletes `from`.
    async fn move_key(&self, from: &str, to: &str, head: &Head) -> Result<()> {
        self.copy(from, to, &head.storage_class).await?;
        match self.head(to).await? {
            Some(h) if h.len == head.len => {}
            _ => {
                return Err(internal(format!(
                    "S3 copy of {from} to {to} is not readable back; the original is kept"
                )));
            }
        }
        self.delete_key(from).await
    }
}

fn collect_part(
    done: Option<std::result::Result<Result<(u64, String)>, tokio::task::JoinError>>,
    etags: &mut BTreeMap<u64, String>,
    failure: &mut Option<BeeError>,
) {
    match done {
        Some(Ok(Ok((n, etag)))) => {
            etags.insert(n, etag);
        }
        Some(Ok(Err(e))) => {
            failure.get_or_insert(e);
        }
        Some(Err(e)) => {
            failure.get_or_insert(internal(format!("part upload task failed: {e}")));
        }
        None => {}
    }
}

#[async_trait]
impl BlobStore for S3BlobStore {
    async fn put(
        &self,
        org: OrgId,
        id: &ObjectId,
        bytes: Vec<u8>,
        class: StorageClass,
    ) -> Result<()> {
        self.counters.blob_put();
        let key = self.key(org, id, false);
        let storage_class = self.class_name(class).to_string();
        let large = bytes.len() as u64 >= self.multipart_threshold;
        let bytes = Bytes::from(bytes);
        if large {
            self.put_multipart(&key, bytes, &storage_class).await?;
        } else {
            self.put_single(&key, bytes, &storage_class).await?;
        }
        // A put makes the bytes servable again, as in the reference store.
        self.delete_key(&self.key(org, id, true)).await
    }

    async fn get(&self, org: OrgId, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        let response = self
            .client
            .send(&Request::new(Method::GET, Some(&self.key(org, id, false))))
            .await?;
        match response.status.as_u16() {
            200 => {
                verify_bytes(id, &response.body)?;
                self.counters.blob_get(
                    id.kind(),
                    response.body.len() as u64,
                    false,
                    response.header("x-amz-storage-class") == Some(self.cold_class.as_str()),
                );
                Ok(Some(response.body.to_vec()))
            }
            404 => Ok(None),
            _ => Err(response.error_for("GetObject")),
        }
    }

    async fn get_range(
        &self,
        org: OrgId,
        id: &ObjectId,
        offset: u64,
        len: u64,
    ) -> Result<Option<Vec<u8>>> {
        let key = self.key(org, id, false);
        if len == 0 {
            return Ok(self.head(&key).await?.map(|_| Vec::new()));
        }
        let last = offset.saturating_add(len - 1);
        let req =
            Request::new(Method::GET, Some(&key)).header("range", format!("bytes={offset}-{last}"));
        let response = self.client.send(&req).await?;
        match response.status.as_u16() {
            200 | 206 => {
                // A 200 means the service ignored the range: slice it ourselves.
                let body = if response.status.as_u16() == 200 {
                    let (s, e) = clamp_range(response.body.len() as u64, offset, len);
                    response.body.slice(s as usize..e as usize)
                } else {
                    response.body.clone()
                };
                self.counters.blob_get(
                    id.kind(),
                    body.len() as u64,
                    true,
                    response.header("x-amz-storage-class") == Some(self.cold_class.as_str()),
                );
                Ok(Some(body.to_vec()))
            }
            // The range starts past the end: the object exists and the answer is empty.
            416 => Ok(self.head(&key).await?.map(|_| Vec::new())),
            404 => Ok(None),
            _ => Err(response.error_for("GetObject range")),
        }
    }

    async fn has(&self, org: OrgId, ids: &[ObjectId]) -> Result<Vec<bool>> {
        self.counters.blob_has(ids.len() as u64);
        let mut found = vec![false; ids.len()];
        let mut running: JoinSet<(usize, Result<bool>)> = JoinSet::new();
        let mut failure = None;
        let mut next = 0;
        while next < ids.len() || !running.is_empty() {
            while next < ids.len() && running.len() < HAS_CONCURRENCY {
                let (store, key, index) = (self.clone(), self.key(org, &ids[next], false), next);
                running.spawn(async move { (index, store.head(&key).await.map(|h| h.is_some())) });
                next += 1;
            }
            match running.join_next().await {
                Some(Ok((index, Ok(present)))) => found[index] = present,
                Some(Ok((_, Err(e)))) => {
                    failure.get_or_insert(e);
                }
                Some(Err(e)) => {
                    failure.get_or_insert(internal(format!("has task failed: {e}")));
                }
                None => {}
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(found),
        }
    }

    async fn stat(&self, org: OrgId, id: &ObjectId) -> Result<Option<BlobInfo>> {
        Ok(self
            .locate(org, id)
            .await?
            .map(|(_, head, quarantined)| self.info(&head, quarantined)))
    }

    async fn delete(
        &self,
        org: OrgId,
        id: &ObjectId,
        written_before_ms: Option<u64>,
    ) -> Result<bool> {
        let mut deleted = false;
        for quarantined in [false, true] {
            let key = self.key(org, id, quarantined);
            let Some(head) = self.head(&key).await? else {
                continue;
            };
            if written_before_ms.is_none_or(|t| head.last_modified_ms < t) {
                self.delete_key(&key).await?;
                deleted = true;
            }
        }
        if deleted {
            self.counters.blob_delete();
        }
        Ok(deleted)
    }

    async fn set_class(&self, org: OrgId, id: &ObjectId, class: StorageClass) -> Result<bool> {
        let Some((key, head, _)) = self.locate(org, id).await? else {
            return Ok(false);
        };
        if self.class_of(&head.storage_class) == class {
            return Ok(true);
        }
        // The copy is server-side and atomic: the key never goes missing.
        self.copy(&key, &key, self.class_name(class)).await?;
        match self.head(&key).await? {
            Some(h) if h.len == head.len => Ok(true),
            _ => Err(internal(format!(
                "S3 class change of {id} did not take effect"
            ))),
        }
    }

    async fn quarantine(&self, org: OrgId, id: &ObjectId) -> Result<bool> {
        let servable = self.key(org, id, false);
        if let Some(head) = self.head(&servable).await? {
            self.move_key(&servable, &self.key(org, id, true), &head)
                .await?;
            return Ok(true);
        }
        Ok(self.head(&self.key(org, id, true)).await?.is_some())
    }

    async fn release(&self, org: OrgId, id: &ObjectId) -> Result<bool> {
        let quarantined = self.key(org, id, true);
        let Some(head) = self.head(&quarantined).await? else {
            return Ok(false);
        };
        self.move_key(&quarantined, &self.key(org, id, false), &head)
            .await?;
        Ok(true)
    }

    async fn list(
        &self,
        org: OrgId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<(ObjectId, BlobInfo)>> {
        let mut out: Vec<(ObjectId, BlobInfo)> = Vec::new();
        for kind in listing_kinds() {
            if out.len() >= limit {
                break;
            }
            if after.is_some_and(|a| a.kind().as_byte() > kind.as_byte()) {
                continue;
            }
            let remaining = limit - out.len();
            let start = after.filter(|a| a.kind() == kind);
            let mut merged: BTreeMap<ObjectId, BlobInfo> = BTreeMap::new();
            // Quarantined first, so a servable copy of the same id overwrites it.
            for quarantined in [true, false] {
                for (id, head) in self
                    .list_stream(org, kind, quarantined, start.as_ref(), remaining)
                    .await?
                {
                    merged.insert(id, self.info(&head, quarantined));
                }
            }
            out.extend(merged.into_iter().take(remaining));
        }
        Ok(out)
    }

    fn access_counters(&self) -> Option<Arc<AccessCounters>> {
        Some(self.counters.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> S3Config {
        S3Config::new("bkt", "eu-west-1", S3Credentials::new("k", "s"))
    }

    #[tokio::test]
    async fn validates_config() {
        assert!(S3BlobStore::new(config()).is_ok());
        let mut c = config();
        c.cold_class = c.hot_class.clone();
        assert!(S3BlobStore::new(c).is_err());
        let mut c = config();
        c.part_size = 1024;
        assert!(S3BlobStore::new(c).is_err());
        let mut c = config();
        c.bucket = "a/b".into();
        assert!(S3BlobStore::new(c).is_err());
        assert!(S3BlobStore::new(config().with_endpoint("nope")).is_err());
    }

    #[tokio::test]
    async fn keys_carry_the_prefix_and_quarantine_marker() {
        let mut c = config();
        c.prefix = "/bee/prod/".into();
        let store = S3BlobStore::new(c).unwrap();
        let org = OrgId(bee_core::Ulid::from_u128(7));
        let id = bee_object::blob_id(b"x");
        assert_eq!(
            store.key(org, &id, false),
            format!("bee/prod/{}", blob_key(org, &id, false))
        );
        assert!(store.key(org, &id, true).starts_with("bee/prod/q/"));
        assert_eq!(store.class_of("STANDARD_IA"), StorageClass::Cold);
        assert_eq!(store.class_of("STANDARD"), StorageClass::Hot);
    }

    #[tokio::test]
    async fn presigned_urls_are_scoped_to_one_servable_key() {
        let store = S3BlobStore::new(config().with_endpoint("http://127.0.0.1:9000")).unwrap();
        let org = OrgId(bee_core::Ulid::from_u128(7));
        let id = bee_object::blob_id(b"x");
        let url = store
            .presign_get_at(org, &id, Duration::from_secs(60), 1_369_353_600_000)
            .unwrap();
        assert!(url.starts_with(&format!(
            "http://127.0.0.1:9000/bkt/{}?",
            blob_key(org, &id, false)
        )));
        assert!(url.contains("X-Amz-Expires=60"));
        assert!(url.contains("X-Amz-Date=20130524T000000Z"));
        assert!(!url.contains("/q/"));
        assert!(store.presign_get_at(org, &id, Duration::ZERO, 0).is_err());
        assert!(
            store
                .presign_get_at(org, &id, Duration::from_secs(8 * 24 * 3600), 0)
                .is_err()
        );
        assert!(!format!("{:?}", config().credentials).contains("\"s\""));
    }
}
