//! `S3BlobStore` against a real S3-compatible service (MinIO in development).
//!
//! These tests run only when `BEE_TEST_S3` names an endpoint, e.g. `http://127.0.0.1:9000`.
//! Optional: `BEE_TEST_S3_BUCKET` (default `bee-test`, created if missing),
//! `BEE_TEST_S3_ACCESS_KEY` / `BEE_TEST_S3_SECRET_KEY` (default `minioadmin`),
//! `BEE_TEST_S3_REGION` (default `us-east-1`) and `BEE_TEST_S3_COLD_CLASS` (default
//! `REDUCED_REDUNDANCY`, the one non-STANDARD class MinIO accepts out of the box). Without
//! `BEE_TEST_S3` every test here returns immediately.

use crate::common;
use std::sync::Arc;
use std::time::Duration;

use bee_core::Reason;
use bee_store::{BlobStore, OrgId, StorageClass};
use bee_store_blob::{
    S3BlobStore, S3Config, S3Credentials, S3UrlIssuer, SignedUrlIssuer, bytes_match_id,
};
use bytes::Bytes;
use common::{Fixture, blob, fresh_org, noise};
use http::HeaderMap;
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

struct Env {
    endpoint: String,
    bucket: String,
    credentials: S3Credentials,
    region: String,
    cold_class: String,
}

fn env() -> Option<Env> {
    let endpoint = std::env::var("BEE_TEST_S3").ok()?;
    let var = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.into());
    Some(Env {
        endpoint,
        bucket: var("BEE_TEST_S3_BUCKET", "bee-test"),
        credentials: S3Credentials::new(
            var("BEE_TEST_S3_ACCESS_KEY", "minioadmin"),
            var("BEE_TEST_S3_SECRET_KEY", "minioadmin"),
        ),
        region: var("BEE_TEST_S3_REGION", "us-east-1"),
        cold_class: var("BEE_TEST_S3_COLD_CLASS", "REDUCED_REDUNDANCY"),
    })
}

async fn store_with(prefix: &str, tweak: impl FnOnce(&mut S3Config)) -> Option<S3BlobStore> {
    let env = env()?;
    let mut config = S3Config::new(&env.bucket, &env.region, env.credentials.clone())
        .with_endpoint(&env.endpoint);
    config.prefix = prefix.to_string();
    config.cold_class = env.cold_class.clone();
    // Small thresholds so a modest blob exercises the multipart path.
    config.multipart_threshold = 5 * 1024 * 1024;
    config.part_size = 5 * 1024 * 1024;
    tweak(&mut config);
    let store = S3BlobStore::new(config).unwrap();
    store.ensure_bucket().await.unwrap();
    Some(store)
}

/// Deletes everything an org stored when the test ends.
struct Cleanup {
    store: S3BlobStore,
    org: OrgId,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let (store, org) = (self.store.clone(), self.org);
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                while let Ok(batch) = store.list(org, None, 500).await {
                    if batch.is_empty() {
                        break;
                    }
                    for (id, _) in batch {
                        let _ = store.delete(org, &id, None).await;
                    }
                }
            })
        });
    }
}

async fn make() -> Option<Fixture> {
    let store = store_with("", |_| {}).await?;
    let org = fresh_org();
    Some(Fixture {
        store: Box::new(store.clone()),
        org,
        keep: Box::new(Cleanup { store, org }),
    })
}

conformance_suite!(make);

#[tokio::test(flavor = "multi_thread")]
async fn verify_on_read_suite() {
    let Some(fx) = make().await else { return };
    common::verify_on_read(&*fx.store, fx.org).await;
}

async fn fetch(url: &str, range: Option<&str>) -> (u16, HeaderMap, Bytes) {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(connector);
    let mut req = http::Request::builder().uri(url);
    if let Some(r) = range {
        req = req.header("range", r);
    }
    let resp = client
        .request(req.body(Empty::new()).unwrap())
        .await
        .unwrap();
    let (parts, body) = resp.into_parts();
    (
        parts.status.as_u16(),
        parts.headers,
        body.collect().await.unwrap().to_bytes(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_urls_serve_exactly_the_bytes_of_the_id_and_expire() {
    let Some(fx) = make().await else { return };
    let store = store_with("", |_| {}).await.unwrap();
    let org = fx.org;
    let issuer = S3UrlIssuer::new(store.clone());
    let data = noise(300_000, 4);
    let (id, bytes) = blob(&data);
    store.put(org, &id, bytes, StorageClass::Hot).await.unwrap();

    let url = issuer
        .issue(org, &id, Duration::from_secs(60))
        .await
        .unwrap();
    let (status, _, body) = fetch(&url.url, None).await;
    assert_eq!(status, 200);
    assert_eq!(&body[..], &data[..]);
    assert!(
        bytes_match_id(&id, &body),
        "the client verifies against the id"
    );
    // Range requests work through the same URL.
    let (status, _, part) = fetch(&url.url, Some("bytes=10-19")).await;
    assert_eq!(status, 206);
    assert_eq!(&part[..], &data[10..20]);

    // The URL is bound to its signature: a different object, or a flipped signature, fails.
    let other = blob(b"another object").0;
    let other_url = issuer.issue(org, &other, Duration::from_secs(60)).await;
    assert_eq!(other_url.unwrap_err().reason, Reason::NotFound);
    let swapped = url.url.replace(
        &bee_store::blob_key(org, &id, false),
        &bee_store::blob_key(org, &other, false),
    );
    assert_eq!(fetch(&swapped, None).await.0, 403);
    let (head, sig) = url.url.rsplit_once("X-Amz-Signature=").unwrap();
    let flipped = format!(
        "{head}X-Amz-Signature={}{}",
        if sig.starts_with('0') { '1' } else { '0' },
        &sig[1..]
    );
    assert_eq!(fetch(&flipped, None).await.0, 403);

    // Quarantined (and so, once redacted, purged) bytes: no URL, and old URLs stop working.
    store.quarantine(org, &id).await.unwrap();
    assert_eq!(
        issuer
            .issue(org, &id, Duration::from_secs(60))
            .await
            .unwrap_err()
            .reason,
        Reason::NotFound
    );
    assert_eq!(fetch(&url.url, None).await.0, 404);
    store.release(org, &id).await.unwrap();
    assert_eq!(fetch(&url.url, None).await.0, 200);

    // Expiry is enforced by the service.
    let short = issuer
        .issue(org, &id, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(fetch(&short.url, None).await.0, 200);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(fetch(&short.url, None).await.0, 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn large_blobs_use_multipart_and_small_ones_do_not() {
    let Some(fx) = make().await else { return };
    let store = store_with("", |_| {}).await.unwrap();
    let org = fx.org;
    let issuer = S3UrlIssuer::new(store.clone());
    let big = noise(11 * 1024 * 1024, 17);
    let small = noise(1000, 18);
    for data in [&big, &small] {
        let (id, bytes) = blob(data);
        store.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
        let url = issuer
            .issue(org, &id, Duration::from_secs(60))
            .await
            .unwrap();
        let (status, headers, _) = fetch(&url.url, Some("bytes=0-0")).await;
        assert_eq!(status, 206);
        let etag = headers.get("etag").unwrap().to_str().unwrap().to_string();
        // A multipart ETag is `<md5 of part md5s>-<part count>`.
        assert_eq!(etag.contains('-'), data.len() > 5 * 1024 * 1024, "{etag}");
        if data.len() > 5 * 1024 * 1024 {
            assert!(etag.trim_matches('"').ends_with("-3"), "{etag}");
        }
        assert_eq!(store.get(org, &id).await.unwrap().unwrap(), *data);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hot_and_cold_map_to_storage_classes() {
    let Some(fx) = make().await else { return };
    let env = env().unwrap();
    let store = store_with("", |_| {}).await.unwrap();
    let org = fx.org;
    let issuer = S3UrlIssuer::new(store.clone());
    let (id, bytes) = blob(b"classy");
    store
        .put(org, &id, bytes, StorageClass::Cold)
        .await
        .unwrap();
    let url = issuer
        .issue(org, &id, Duration::from_secs(60))
        .await
        .unwrap();
    let class = |h: &HeaderMap| {
        h.get("x-amz-storage-class")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_else(|| "STANDARD".into())
    };
    assert_eq!(class(&fetch(&url.url, None).await.1), env.cold_class);
    store.set_class(org, &id, StorageClass::Hot).await.unwrap();
    assert_eq!(class(&fetch(&url.url, None).await.1), "STANDARD");
    assert_eq!(
        store.stat(org, &id).await.unwrap().unwrap().class,
        StorageClass::Hot
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn prefixes_partition_a_bucket() {
    let Some(fx) = make().await else { return };
    let a = store_with("tenant-a", |_| {}).await.unwrap();
    let b = store_with("/tenant-b/nested/", |_| {}).await.unwrap();
    let org = fx.org;
    let (id, bytes) = blob(b"partitioned");
    a.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
    assert!(a.get(org, &id).await.unwrap().is_some());
    assert_eq!(b.get(org, &id).await.unwrap(), None);
    assert!(b.list(org, None, 10).await.unwrap().is_empty());
    assert_eq!(a.list(org, None, 10).await.unwrap().len(), 1);
    a.delete(org, &id, None).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn conditional_put_does_not_reupload_but_refreshes_the_time() {
    for conditional in [true, false] {
        let Some(fx) = make().await else { return };
        let store = store_with("", |c| c.conditional_put = conditional)
            .await
            .unwrap();
        let org = fx.org;
        let (id, bytes) = blob(b"insert or ignore");
        store
            .put(org, &id, bytes.clone(), StorageClass::Hot)
            .await
            .unwrap();
        let first = store.stat(org, &id).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        store
            .put(org, &id, bytes.clone(), StorageClass::Hot)
            .await
            .unwrap();
        let second = store.stat(org, &id).await.unwrap().unwrap();
        assert!(
            second.written_at_ms > first.written_at_ms,
            "conditional={conditional}: {first:?} then {second:?}"
        );
        assert_eq!(store.get(org, &id).await.unwrap(), Some(bytes.clone()));
        // The re-put also applies the requested class.
        store
            .put(org, &id, bytes, StorageClass::Cold)
            .await
            .unwrap();
        assert_eq!(
            store.stat(org, &id).await.unwrap().unwrap().class,
            StorageClass::Cold
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_puts_of_one_id_all_succeed() {
    let Some(fx) = make().await else { return };
    let store = Arc::new(store_with("", |_| {}).await.unwrap());
    let org = fx.org;
    let (id, bytes) = blob(&noise(100_000, 33));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (s, b) = (store.clone(), bytes.clone());
        tasks.push(tokio::spawn(async move {
            s.put(org, &id, b, StorageClass::Hot).await
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(store.get(org, &id).await.unwrap(), Some(bytes));
    assert_eq!(store.list(org, None, 10).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn has_handles_many_ids_in_order() {
    let Some(fx) = make().await else { return };
    let store = store_with("", |_| {}).await.unwrap();
    let org = fx.org;
    let mut ids = Vec::new();
    let mut expected = Vec::new();
    for i in 0..40u32 {
        let (id, bytes) = blob(format!("has {i}").as_bytes());
        if i % 3 == 0 {
            store.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
        }
        ids.push(id);
        expected.push(i % 3 == 0);
    }
    assert_eq!(store.has(org, &ids).await.unwrap(), expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_credentials_are_a_clear_error_not_an_absent_object() {
    let Some(env) = env() else { return };
    let fx = make().await.unwrap();
    let config = S3Config::new(
        &env.bucket,
        &env.region,
        S3Credentials::new("not-a-key", "not-a-secret"),
    )
    .with_endpoint(&env.endpoint);
    let store = S3BlobStore::new(config).unwrap();
    let (id, _) = blob(b"x");
    let err = store.get(fx.org, &id).await.unwrap_err();
    assert_eq!(err.reason, Reason::Internal);
    assert!(err.message.contains("403") || err.message.contains("InvalidAccessKeyId"));
}
