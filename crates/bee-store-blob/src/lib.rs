//! Blob store on S3-compatible object storage and local disk (SPEC §6.1, §6.5, §10.5).
//!
//! - [`DiskBlobStore`]: content-addressed files under a root directory, zstd-compressed at
//!   rest, written temp file, fsync, atomic rename; hot and cold classes as two subtrees,
//!   quarantine under the `q/` prefix.
//! - [`S3BlobStore`]: the same contract on any S3-compatible service (AWS, MinIO, R2) through
//!   a small SigV4 client over hyper: path or virtual-host addressing, storage-class mapping
//!   for hot and cold, conditional puts, multipart for large blobs, retries with jitter and
//!   deadlines. Bytes are stored raw so a signed URL serves exactly the bytes the id hashes.
//! - [`signed_url`]: the [`SignedUrlIssuer`] trait with S3 presigning and a local
//!   HMAC-signed URL for servers without a CDN, plus the verify function the server mounts.
//!
//! Both stores implement [`bee_store::BlobStore`] with the same key layout
//! ([`bee_store::blob_key`]); the conformance tests run one behavioural suite against
//! the in-memory reference, the disk store and (when `BEE_TEST_S3` is set) S3.
//!
//! Reads verify bytes against the id: [`bee_store::BlobStore::get`] re-hashes and reports
//! corruption as `INTEGRITY`.

mod common;
pub mod disk;
pub mod s3;
pub mod signed_url;

pub use disk::{DiskBlobConfig, DiskBlobStore};
pub use s3::{S3BlobStore, S3Config, S3Credentials};
pub use signed_url::{
    DEFAULT_SIGNED_URL_TTL, LocalUrlIssuer, S3UrlIssuer, SignedUrl, SignedUrlIssuer,
    bytes_match_id, verify_local_url,
};
