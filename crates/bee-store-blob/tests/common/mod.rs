//! The behavioural suite every `BlobStore` passes: the in-memory reference, the disk store
//! and (when configured) S3. Each function takes a store and a fresh org.

#![allow(dead_code)]

use std::any::Any;

use bee_core::{ObjectId, ObjectKind, Reason, Ulid};
use bee_object::{blob_id, hash_id};
use bee_store::{BlobStore, OrgId, StorageClass};

/// A store under test, the org to use, and anything that must outlive it (a temp dir).
pub struct Fixture {
    pub store: Box<dyn BlobStore>,
    pub org: OrgId,
    pub keep: Box<dyn Any + Send>,
}

/// A random org, so tests sharing one bucket never see each other's objects.
pub fn fresh_org() -> OrgId {
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).expect("entropy");
    OrgId(Ulid::from_u128(u128::from_le_bytes(raw)))
}

/// Deterministic incompressible bytes.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

pub fn blob(data: &[u8]) -> (ObjectId, Vec<u8>) {
    (blob_id(data), data.to_vec())
}

pub async fn put_get_has_stat(s: &dyn BlobStore, org: OrgId) {
    let (id, bytes) = blob(b"hello blob store");
    assert_eq!(s.get(org, &id).await.unwrap(), None);
    assert_eq!(s.stat(org, &id).await.unwrap(), None);
    s.put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    assert_eq!(s.get(org, &id).await.unwrap(), Some(bytes.clone()));
    let absent = blob_id(b"never stored");
    assert_eq!(
        s.has(org, &[id, absent, id]).await.unwrap(),
        vec![true, false, true]
    );
    let info = s.stat(org, &id).await.unwrap().unwrap();
    assert_eq!(info.len, bytes.len() as u64);
    assert_eq!(info.class, StorageClass::Hot);
    assert!(!info.quarantined);
    assert!(info.written_at_ms > 0);

    // A repeated put is harmless.
    s.put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    assert_eq!(s.get(org, &id).await.unwrap(), Some(bytes));
    assert_eq!(s.list(org, None, 10).await.unwrap().len(), 1);

    // The empty blob, and trees and metas by their kinds.
    let (empty, bytes) = blob(b"");
    s.put(org, &empty, bytes, StorageClass::Hot).await.unwrap();
    assert_eq!(s.get(org, &empty).await.unwrap(), Some(Vec::new()));
    assert_eq!(s.stat(org, &empty).await.unwrap().unwrap().len, 0);
    for kind in [ObjectKind::Tree, ObjectKind::Meta] {
        let data = format!("{kind:?} bytes").into_bytes();
        let id = hash_id(kind, &data);
        s.put(org, &id, data.clone(), StorageClass::Cold)
            .await
            .unwrap();
        assert_eq!(s.get(org, &id).await.unwrap(), Some(data));
    }
}

pub async fn range_reads(s: &dyn BlobStore, org: OrgId) {
    let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    let (id, bytes) = blob(&data);
    assert_eq!(s.get_range(org, &id, 0, 10).await.unwrap(), None);
    s.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
    let r = |off, len| s.get_range(org, &id, off, len);
    assert_eq!(r(0, 10).await.unwrap(), Some(data[..10].to_vec()));
    assert_eq!(r(500, 100).await.unwrap(), Some(data[500..600].to_vec()));
    assert_eq!(r(990, 100).await.unwrap(), Some(data[990..].to_vec()));
    assert_eq!(r(0, u64::MAX).await.unwrap(), Some(data.clone()));
    assert_eq!(r(1000, 5).await.unwrap(), Some(Vec::new()));
    assert_eq!(r(5000, 5).await.unwrap(), Some(Vec::new()));
    assert_eq!(r(10, 0).await.unwrap(), Some(Vec::new()));
    s.set_class(org, &id, StorageClass::Cold).await.unwrap();
    assert_eq!(r(250, 3).await.unwrap(), Some(data[250..253].to_vec()));
    s.quarantine(org, &id).await.unwrap();
    assert_eq!(r(0, 10).await.unwrap(), None);
}

pub async fn delete_before(s: &dyn BlobStore, org: OrgId) {
    let (id, bytes) = blob(b"to be collected");
    assert!(!s.delete(org, &id, None).await.unwrap());
    s.put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    let written = s.stat(org, &id).await.unwrap().unwrap().written_at_ms;

    // Not written strictly before its own write time, nor before the epoch: it survives.
    assert!(!s.delete(org, &id, Some(written)).await.unwrap());
    assert!(!s.delete(org, &id, Some(0)).await.unwrap());
    assert!(s.has(org, &[id]).await.unwrap()[0]);

    // Written before a later cut-off: collected.
    assert!(s.delete(org, &id, Some(written + 3_600_000)).await.unwrap());
    assert!(!s.has(org, &[id]).await.unwrap()[0]);
    assert_eq!(s.stat(org, &id).await.unwrap(), None);

    // Unconditional delete, quarantined bytes included.
    s.put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    assert!(s.delete(org, &id, None).await.unwrap());
    s.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
    s.quarantine(org, &id).await.unwrap();
    assert!(s.delete(org, &id, None).await.unwrap());
    assert_eq!(s.stat(org, &id).await.unwrap(), None);
    assert!(!s.delete(org, &id, None).await.unwrap());
}

pub async fn class_moves(s: &dyn BlobStore, org: OrgId) {
    let (id, bytes) = blob(&noise(5000, 7));
    assert!(
        !s.set_class(org, &id, StorageClass::Cold).await.unwrap(),
        "absent bytes cannot move"
    );
    s.put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    assert!(s.set_class(org, &id, StorageClass::Cold).await.unwrap());
    let info = s.stat(org, &id).await.unwrap().unwrap();
    assert_eq!(info.class, StorageClass::Cold);
    // Reads are transparent, and the id is unchanged.
    assert_eq!(s.get(org, &id).await.unwrap(), Some(bytes.clone()));
    assert!(s.has(org, &[id]).await.unwrap()[0]);
    // Moving to the class it is already in is fine.
    assert!(s.set_class(org, &id, StorageClass::Cold).await.unwrap());
    assert!(s.set_class(org, &id, StorageClass::Hot).await.unwrap());
    assert_eq!(
        s.stat(org, &id).await.unwrap().unwrap().class,
        StorageClass::Hot
    );
    assert_eq!(s.get(org, &id).await.unwrap(), Some(bytes.clone()));
    // A put states its class; a re-put in another class moves the bytes.
    s.put(org, &id, bytes.clone(), StorageClass::Cold)
        .await
        .unwrap();
    assert_eq!(
        s.stat(org, &id).await.unwrap().unwrap().class,
        StorageClass::Cold
    );
    s.put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    assert_eq!(
        s.stat(org, &id).await.unwrap().unwrap().class,
        StorageClass::Hot
    );
    assert_eq!(s.list(org, None, 10).await.unwrap().len(), 1);
    // The class survives quarantine and release.
    s.set_class(org, &id, StorageClass::Cold).await.unwrap();
    s.quarantine(org, &id).await.unwrap();
    let q = s.stat(org, &id).await.unwrap().unwrap();
    assert_eq!((q.class, q.quarantined), (StorageClass::Cold, true));
    assert!(s.set_class(org, &id, StorageClass::Hot).await.unwrap());
    let q = s.stat(org, &id).await.unwrap().unwrap();
    assert_eq!((q.class, q.quarantined), (StorageClass::Hot, true));
    s.release(org, &id).await.unwrap();
    assert_eq!(s.get(org, &id).await.unwrap(), Some(bytes));
}

pub async fn quarantine_release(s: &dyn BlobStore, org: OrgId) {
    let (id, bytes) = blob(b"sensitive bytes");
    assert!(!s.quarantine(org, &id).await.unwrap());
    assert!(!s.release(org, &id).await.unwrap());
    s.put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    assert!(!s.release(org, &id).await.unwrap(), "not quarantined");

    assert!(s.quarantine(org, &id).await.unwrap());
    assert_eq!(s.get(org, &id).await.unwrap(), None);
    assert_eq!(s.get_range(org, &id, 0, 3).await.unwrap(), None);
    assert_eq!(s.has(org, &[id]).await.unwrap(), vec![false]);
    let info = s.stat(org, &id).await.unwrap().unwrap();
    assert!(info.quarantined);
    assert_eq!(info.len, bytes.len() as u64);
    let listed = s.list(org, None, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].1.quarantined);
    assert!(
        s.quarantine(org, &id).await.unwrap(),
        "quarantining twice is fine"
    );

    assert!(s.release(org, &id).await.unwrap());
    assert_eq!(s.get(org, &id).await.unwrap(), Some(bytes.clone()));
    assert!(!s.stat(org, &id).await.unwrap().unwrap().quarantined);
    assert!(!s.release(org, &id).await.unwrap());

    // Putting bytes again makes them servable, as in the reference store.
    s.quarantine(org, &id).await.unwrap();
    s.put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    assert_eq!(s.get(org, &id).await.unwrap(), Some(bytes));
    assert_eq!(s.list(org, None, 10).await.unwrap().len(), 1);
}

pub async fn list_ordering_and_pagination(s: &dyn BlobStore, org: OrgId) {
    let mut expected: Vec<ObjectId> = Vec::new();
    for i in 0..13u32 {
        let (id, bytes) = blob(format!("blob number {i}").as_bytes());
        s.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
        expected.push(id);
    }
    for i in 0..3u32 {
        let data = format!("tree number {i}").into_bytes();
        let id = hash_id(ObjectKind::Tree, &data);
        s.put(org, &id, data, StorageClass::Cold).await.unwrap();
        expected.push(id);
    }
    let data = b"a meta".to_vec();
    let id = hash_id(ObjectKind::Meta, &data);
    s.put(org, &id, data, StorageClass::Hot).await.unwrap();
    expected.push(id);
    // One quarantined entry stays listed, once.
    s.quarantine(org, &expected[2]).await.unwrap();
    expected.sort();

    let all = s.list(org, None, 100).await.unwrap();
    assert_eq!(all.iter().map(|(i, _)| *i).collect::<Vec<_>>(), expected);
    assert_eq!(all.iter().filter(|(_, i)| i.quarantined).count(), 1);
    assert!(all.iter().all(|(id, i)| {
        i.class
            == if id.kind() == ObjectKind::Tree {
                StorageClass::Cold
            } else {
                StorageClass::Hot
            }
    }));

    for page in [1usize, 4, 5, 16, 17] {
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let chunk = s.list(org, after, page).await.unwrap();
            assert!(chunk.len() <= page);
            if chunk.is_empty() {
                break;
            }
            after = chunk.last().map(|(i, _)| *i);
            seen.extend(chunk.into_iter().map(|(i, _)| i));
        }
        assert_eq!(seen, expected, "page size {page}");
    }
    assert!(s.list(org, None, 0).await.unwrap().is_empty());
    assert!(
        s.list(org, expected.last().copied(), 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(s.list(fresh_org(), None, 10).await.unwrap().is_empty());
}

pub async fn orgs_are_isolated(s: &dyn BlobStore, org: OrgId) {
    let other = fresh_org();
    let (id, bytes) = blob(b"shared content");
    s.put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    assert_eq!(s.get(other, &id).await.unwrap(), None);
    assert_eq!(s.has(other, &[id]).await.unwrap(), vec![false]);
    s.put(other, &id, bytes, StorageClass::Hot).await.unwrap();
    s.quarantine(org, &id).await.unwrap();
    assert!(s.get(other, &id).await.unwrap().is_some());
    s.delete(org, &id, None).await.unwrap();
    assert!(s.get(other, &id).await.unwrap().is_some());
    s.delete(other, &id, None).await.unwrap();
}

pub async fn large_blob(s: &dyn BlobStore, org: OrgId) {
    let data = noise(11 * 1024 * 1024 + 123, 99);
    let (id, bytes) = blob(&data);
    s.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
    let got = s.get(org, &id).await.unwrap().unwrap();
    assert_eq!(got.len(), data.len());
    assert!(got == data);
    let mid = 5 * 1024 * 1024 - 7;
    assert_eq!(
        s.get_range(org, &id, mid, 100).await.unwrap().unwrap(),
        data[mid as usize..mid as usize + 100]
    );
    assert_eq!(
        s.stat(org, &id).await.unwrap().unwrap().len,
        data.len() as u64
    );
    s.delete(org, &id, None).await.unwrap();
}

/// Bytes stored under an id they do not hash to are caught when read: the store re-hashes.
pub async fn verify_on_read(s: &dyn BlobStore, org: OrgId) {
    let id = blob_id(b"the real bytes");
    s.put(org, &id, b"not the real bytes".to_vec(), StorageClass::Hot)
        .await
        .unwrap();
    let err = s.get(org, &id).await.unwrap_err();
    assert_eq!(err.reason, Reason::Integrity);
    // Metadata calls do not read the bytes.
    assert!(s.has(org, &[id]).await.unwrap()[0]);
    // Delete-then-put repairs it (an insert-or-ignore put keeps what is already there).
    assert!(s.delete(org, &id, None).await.unwrap());
    s.put(org, &id, b"the real bytes".to_vec(), StorageClass::Hot)
        .await
        .unwrap();
    assert_eq!(
        s.get(org, &id).await.unwrap(),
        Some(b"the real bytes".to_vec())
    );
}

/// Declares the shared suite as tests; `$make` is an async fn returning `Option<Fixture>`
/// (`None` skips, for S3 without a service).
#[macro_export]
macro_rules! conformance_suite {
    ($make:path) => {
        mod conformance {
            use super::*;
            $crate::conformance_suite!(@one $make, put_get_has_stat);
            $crate::conformance_suite!(@one $make, range_reads);
            $crate::conformance_suite!(@one $make, delete_before);
            $crate::conformance_suite!(@one $make, class_moves);
            $crate::conformance_suite!(@one $make, quarantine_release);
            $crate::conformance_suite!(@one $make, list_ordering_and_pagination);
            $crate::conformance_suite!(@one $make, orgs_are_isolated);
            $crate::conformance_suite!(@one $make, large_blob);
        }
    };
    (@one $make:path, $name:ident) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let Some(fx) = $make().await else { return };
            $crate::common::$name(&*fx.store, fx.org).await;
            drop(fx.keep);
        }
    };
}
