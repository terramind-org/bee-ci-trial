//! `DiskBlobStore`: the conformance suite, plus everything specific to files on disk:
//! compression at rest, tiers as subtrees, verify-on-read, durability ordering and crashes.

use crate::common;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bee_core::{ObjectId, Reason};
use bee_object::blob_id;
use bee_store::faults::{Fault, FaultInjector, FaultRule, is_injected_crash};
use bee_store::{BlobStore, FaultyBlob, ManualClock, OrgId, StorageClass, SystemClock, blob_key};
use bee_store_blob::disk::points;
use bee_store_blob::{DiskBlobConfig, DiskBlobStore};
use bee_testkit::fixtures::quiet_injected_crashes;
use common::{Fixture, blob, fresh_org, noise};

async fn make() -> Option<Fixture> {
    let dir = tempfile::tempdir().unwrap();
    let store = DiskBlobStore::open_with(
        dir.path(),
        DiskBlobConfig::default(),
        Arc::new(ManualClock::new(1_000)),
    )
    .unwrap();
    Some(Fixture {
        store: Box::new(store),
        org: fresh_org(),
        keep: Box::new(dir),
    })
}

conformance_suite!(make);

#[tokio::test(flavor = "multi_thread")]
async fn verify_on_read_suite() {
    let fx = make().await.unwrap();
    common::verify_on_read(&*fx.store, fx.org).await;
}

fn open(dir: &Path) -> DiskBlobStore {
    DiskBlobStore::open_with(
        dir,
        DiskBlobConfig::default(),
        Arc::new(ManualClock::new(5_000)),
    )
    .unwrap()
}

/// Every regular file under `dir`.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn stored_files(root: &Path) -> Vec<PathBuf> {
    let mut all = files(&root.join("hot"));
    all.extend(files(&root.join("cold")));
    all
}

#[tokio::test(flavor = "multi_thread")]
async fn bytes_are_zstd_compressed_at_rest() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let org = fresh_org();
    let data = vec![b'a'; 1 << 20];
    let (id, bytes) = blob(&data);
    store.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
    let path = store.path_of(org, &id).unwrap();
    let on_disk = fs::read(&path).unwrap();
    assert!(
        on_disk.len() < 4096,
        "1 MiB of one byte took {}",
        on_disk.len()
    );
    assert_eq!(&on_disk[..4], b"SWBL");
    // The zstd frame follows the 24-byte header.
    assert_eq!(&on_disk[24..28], &[0x28, 0xB5, 0x2F, 0xFD]);
    // stat reports the uncompressed length, and reads return the original bytes.
    assert_eq!(
        store.stat(org, &id).await.unwrap().unwrap().len,
        data.len() as u64
    );
    assert_eq!(store.get(org, &id).await.unwrap().unwrap(), data);
}

#[tokio::test(flavor = "multi_thread")]
async fn compression_level_is_configurable_and_not_identity() {
    let (low_dir, high_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let clock = Arc::new(ManualClock::new(1));
    let mk = |dir: &Path, level| {
        DiskBlobStore::open_with(
            dir,
            DiskBlobConfig {
                zstd_level: level,
                ..DiskBlobConfig::default()
            },
            clock.clone(),
        )
        .unwrap()
    };
    let (low, high) = (mk(low_dir.path(), 1), mk(high_dir.path(), 19));
    let org = fresh_org();
    let text: Vec<u8> = (0..20_000)
        .flat_map(|i| format!("line {} of some fairly repetitive text\n", i % 97).into_bytes())
        .collect();
    let (id, bytes) = blob(&text);
    low.put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    high.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
    let low_len = fs::metadata(low.path_of(org, &id).unwrap()).unwrap().len();
    let high_len = fs::metadata(high.path_of(org, &id).unwrap()).unwrap().len();
    assert!(
        high_len < low_len,
        "level 19 ({high_len}) vs level 1 ({low_len})"
    );

    // The same id, and each store reads the other's file.
    let rel = PathBuf::from("hot").join(blob_key(org, &id, false));
    let target = low_dir.path().join(&rel);
    fs::copy(high.path_of(org, &id).unwrap(), &target).unwrap();
    assert_eq!(low.get(org, &id).await.unwrap().unwrap(), text);
}

#[tokio::test(flavor = "multi_thread")]
async fn reput_refreshes_the_written_time() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(1_000));
    let store =
        DiskBlobStore::open_with(dir.path(), DiskBlobConfig::default(), clock.clone()).unwrap();
    let org = fresh_org();
    let (id, bytes) = blob(b"gc me later");
    store
        .put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    assert_eq!(
        store.stat(org, &id).await.unwrap().unwrap().written_at_ms,
        1_000
    );

    // Moving between classes and through quarantine keeps the time.
    clock.set(2_000);
    store.set_class(org, &id, StorageClass::Cold).await.unwrap();
    store.quarantine(org, &id).await.unwrap();
    store.release(org, &id).await.unwrap();
    assert_eq!(
        store.stat(org, &id).await.unwrap().unwrap().written_at_ms,
        1_000
    );

    // A re-put refreshes it, so a GC that planned at 1_500 no longer deletes the bytes.
    clock.set(3_000);
    store
        .put(org, &id, bytes, StorageClass::Cold)
        .await
        .unwrap();
    assert_eq!(
        store.stat(org, &id).await.unwrap().unwrap().written_at_ms,
        3_000
    );
    assert!(!store.delete(org, &id, Some(1_500)).await.unwrap());
    assert!(store.delete(org, &id, Some(3_001)).await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn tiers_are_subtrees_and_quarantine_is_the_q_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let org = fresh_org();
    let (id, bytes) = blob(b"tiered");
    let key = blob_key(org, &id, false);
    let qkey = blob_key(org, &id, true);
    assert!(key.starts_with("o/") && qkey.starts_with("q/"));

    store.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
    assert_eq!(
        stored_files(dir.path()),
        vec![dir.path().join("hot").join(&key)]
    );
    store.set_class(org, &id, StorageClass::Cold).await.unwrap();
    assert_eq!(
        stored_files(dir.path()),
        vec![dir.path().join("cold").join(&key)]
    );
    store.quarantine(org, &id).await.unwrap();
    assert_eq!(
        stored_files(dir.path()),
        vec![dir.path().join("cold").join(&qkey)]
    );
    store.release(org, &id).await.unwrap();
    assert_eq!(
        stored_files(dir.path()),
        vec![dir.path().join("cold").join(&key)]
    );
    store.delete(org, &id, None).await.unwrap();
    assert!(stored_files(dir.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn data_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let org = fresh_org();
    let (id, bytes) = blob(&noise(10_000, 3));
    {
        let store = open(dir.path());
        store
            .put(org, &id, bytes.clone(), StorageClass::Cold)
            .await
            .unwrap();
        store.quarantine(org, &id).await.unwrap();
    }
    let store = open(dir.path());
    let info = store.stat(org, &id).await.unwrap().unwrap();
    assert!(info.quarantined);
    assert_eq!(info.class, StorageClass::Cold);
    store.release(org, &id).await.unwrap();
    assert_eq!(store.get(org, &id).await.unwrap(), Some(bytes));
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_removes_leftover_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    drop(open(dir.path()));
    fs::write(dir.path().join("tmp").join("999-0.tmp"), b"half a write").unwrap();
    let store = open(dir.path());
    assert!(files(&dir.path().join("tmp")).is_empty());
    assert!(store.list(fresh_org(), None, 10).await.unwrap().is_empty());
}

// ----- verify on read -------------------------------------------------------------------

async fn stored(
    dir: &Path,
    data: &[u8],
    config: DiskBlobConfig,
) -> (DiskBlobStore, OrgId, ObjectId, PathBuf) {
    let store = DiskBlobStore::open_with(dir, config, Arc::new(SystemClock)).unwrap();
    let org = fresh_org();
    let (id, bytes) = blob(data);
    store.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
    let path = store.path_of(org, &id).unwrap();
    (store, org, id, path)
}

#[tokio::test(flavor = "multi_thread")]
async fn bit_rot_in_the_payload_is_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let data = noise(50_000, 5);
    let (store, org, id, path) = stored(dir.path(), &data, DiskBlobConfig::default()).await;
    let mut raw = fs::read(&path).unwrap();
    let mid = raw.len() / 2;
    raw[mid] ^= 0x01;
    fs::write(&path, raw).unwrap();
    let err = store.get(org, &id).await.unwrap_err();
    assert_eq!(err.reason, Reason::Integrity);
    // The metadata calls that do not read the payload still answer.
    assert!(store.has(org, &[id]).await.unwrap()[0]);
    assert_eq!(store.stat(org, &id).await.unwrap().unwrap().len, 50_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn truncation_and_bad_headers_are_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let data = noise(20_000, 6);
    let (store, org, id, path) = stored(dir.path(), &data, DiskBlobConfig::default()).await;
    let good = fs::read(&path).unwrap();

    fs::write(&path, &good[..good.len() - 10]).unwrap();
    assert_eq!(
        store.get(org, &id).await.unwrap_err().reason,
        Reason::Integrity
    );
    assert_eq!(
        store
            .get_range(org, &id, 19_950, 50)
            .await
            .unwrap_err()
            .reason,
        Reason::Integrity
    );

    fs::write(&path, &good[..10]).unwrap();
    assert_eq!(
        store.get(org, &id).await.unwrap_err().reason,
        Reason::Integrity
    );
    assert_eq!(
        store.stat(org, &id).await.unwrap_err().reason,
        Reason::Integrity
    );

    let mut bad_magic = good.clone();
    bad_magic[0] = b'X';
    fs::write(&path, bad_magic).unwrap();
    assert_eq!(
        store.get(org, &id).await.unwrap_err().reason,
        Reason::Integrity
    );

    // A header that lies about the length.
    let mut lying = good.clone();
    lying[16..24].copy_from_slice(&(data.len() as u64 + 1).to_le_bytes());
    fs::write(&path, lying).unwrap();
    assert_eq!(
        store.get(org, &id).await.unwrap_err().reason,
        Reason::Integrity
    );

    // Restoring the file restores the object.
    fs::write(&path, good).unwrap();
    assert_eq!(store.get(org, &id).await.unwrap(), Some(data));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_valid_file_holding_other_bytes_is_integrity() {
    // The frame decodes and its checksum passes; only the id check can catch this.
    let dir = tempfile::tempdir().unwrap();
    let (store, org, id, path) = stored(dir.path(), b"original", DiskBlobConfig::default()).await;
    let other = tempfile::tempdir().unwrap();
    let (imposter, iorg, iid, ipath) =
        stored(other.path(), b"original", DiskBlobConfig::default()).await;
    // Replace the file's payload by an equally well-formed file of different content.
    let foreign = blob_id(b"different");
    imposter
        .put(iorg, &foreign, b"different".to_vec(), StorageClass::Hot)
        .await
        .unwrap();
    let _ = (iid, ipath);
    fs::copy(imposter.path_of(iorg, &foreign).unwrap(), &path).unwrap();
    assert_eq!(
        store.get(org, &id).await.unwrap_err().reason,
        Reason::Integrity
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn verifying_range_reads_catch_damage_outside_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let data = noise(200_000, 8);
    let config = DiskBlobConfig {
        verify_range_reads: true,
        ..DiskBlobConfig::default()
    };
    let (store, org, id, path) = stored(dir.path(), &data, config).await;
    assert_eq!(
        store.get_range(org, &id, 10, 10).await.unwrap().unwrap(),
        data[10..20]
    );
    let mut raw = fs::read(&path).unwrap();
    let tail = raw.len() - 20;
    raw[tail] ^= 0x40;
    fs::write(&path, raw).unwrap();
    assert_eq!(
        store.get_range(org, &id, 10, 10).await.unwrap_err().reason,
        Reason::Integrity
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn damaged_files_can_still_be_collected_and_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let (store, org, id, path) = stored(dir.path(), b"fragile", DiskBlobConfig::default()).await;
    fs::write(&path, b"garbage").unwrap();
    // GC's age rule falls back to the file's mtime when the header is unreadable.
    assert!(!store.delete(org, &id, Some(1)).await.unwrap());
    assert!(store.delete(org, &id, Some(u64::MAX)).await.unwrap());
    assert_eq!(store.stat(org, &id).await.unwrap(), None);
    // And a re-upload repairs a damaged object in place.
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"garbage").unwrap();
    store
        .put(org, &id, b"fragile".to_vec(), StorageClass::Hot)
        .await
        .unwrap();
    assert_eq!(
        store.get(org, &id).await.unwrap(),
        Some(b"fragile".to_vec())
    );
}

// ----- crashes --------------------------------------------------------------------------

fn faulty_store(dir: &Path) -> (DiskBlobStore, Arc<FaultInjector>) {
    let faults = FaultInjector::new();
    let store = open(dir).with_faults(faults.clone());
    (store, faults)
}

/// Runs `op` on a task and returns whether it panicked with an injected crash.
async fn crashes<F, Fut>(op: F) -> bool
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = bee_core::Result<()>> + Send + 'static,
{
    quiet_injected_crashes();
    match tokio::spawn(async move { op().await }).await {
        Err(e) if e.is_panic() => {
            let payload = e.into_panic();
            let msg = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            assert!(is_injected_crash(&msg), "unexpected panic: {msg}");
            true
        }
        Ok(_) => false,
        Err(e) => panic!("task failed: {e}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_before_the_rename_leaves_nothing_visible() {
    let dir = tempfile::tempdir().unwrap();
    let (store, faults) = faulty_store(dir.path());
    let org = fresh_org();
    let (id, bytes) = blob(b"never lands");
    faults.add(FaultRule::always(points::PUT_TEMP_WRITTEN, Fault::Crash));
    let store = Arc::new(store);
    let s2 = store.clone();
    assert!(crashes(move || async move { s2.put(org, &id, bytes, StorageClass::Hot).await }).await);
    // The temp file is on disk (durable, unreferenced) but not visible.
    assert_eq!(files(&dir.path().join("tmp")).len(), 1);
    assert_eq!(store.get(org, &id).await.unwrap(), None);
    assert!(store.list(org, None, 10).await.unwrap().is_empty());
    drop(store);
    // Reopening, as after a process restart, clears it.
    let reopened = open(dir.path());
    assert!(files(&dir.path().join("tmp")).is_empty());
    assert_eq!(reopened.get(org, &id).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_the_rename_keeps_the_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (store, faults) = faulty_store(dir.path());
    let org = fresh_org();
    let (id, bytes) = blob(b"landed before the crash");
    faults.add(FaultRule::always(points::PUT_INSTALLED, Fault::Crash));
    let store = Arc::new(store);
    let (s2, b2) = (store.clone(), bytes.clone());
    assert!(crashes(move || async move { s2.put(org, &id, b2, StorageClass::Hot).await }).await);
    drop(store);
    let reopened = open(dir.path());
    assert_eq!(reopened.get(org, &id).await.unwrap(), Some(bytes));
    assert_eq!(reopened.list(org, None, 10).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_during_a_class_move_never_loses_or_duplicates_the_object() {
    let dir = tempfile::tempdir().unwrap();
    let (store, faults) = faulty_store(dir.path());
    let org = fresh_org();
    let (id, bytes) = blob(&noise(30_000, 11));
    store
        .put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    faults.add(FaultRule::always(points::SET_CLASS_COPIED, Fault::Crash));
    let store = Arc::new(store);
    let s2 = store.clone();
    assert!(
        crashes(
            move || async move { s2.set_class(org, &id, StorageClass::Cold).await.map(|_| ()) }
        )
        .await
    );
    drop(store);

    // Both copies exist; the object reads, lists and stats once.
    assert_eq!(stored_files(dir.path()).len(), 2);
    let reopened = open(dir.path());
    assert_eq!(reopened.get(org, &id).await.unwrap(), Some(bytes.clone()));
    assert_eq!(reopened.list(org, None, 10).await.unwrap().len(), 1);
    assert!(reopened.stat(org, &id).await.unwrap().is_some());

    // Repeating the move finishes the job, either way.
    assert!(
        reopened
            .set_class(org, &id, StorageClass::Cold)
            .await
            .unwrap()
    );
    assert_eq!(stored_files(dir.path()).len(), 1);
    assert_eq!(
        reopened.stat(org, &id).await.unwrap().unwrap().class,
        StorageClass::Cold
    );
    assert_eq!(reopened.get(org, &id).await.unwrap(), Some(bytes));
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_during_a_reput_into_another_class_heals_on_the_next_put() {
    let dir = tempfile::tempdir().unwrap();
    let (store, faults) = faulty_store(dir.path());
    let org = fresh_org();
    let (id, bytes) = blob(b"moved by a put");
    store
        .put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    faults.add(FaultRule::always(points::PUT_INSTALLED, Fault::Crash));
    let store = Arc::new(store);
    let (s2, b2) = (store.clone(), bytes.clone());
    assert!(crashes(move || async move { s2.put(org, &id, b2, StorageClass::Cold).await }).await);
    drop(store);
    assert_eq!(stored_files(dir.path()).len(), 2);
    let reopened = open(dir.path());
    assert_eq!(reopened.list(org, None, 10).await.unwrap().len(), 1);
    reopened
        .put(org, &id, bytes.clone(), StorageClass::Cold)
        .await
        .unwrap();
    assert_eq!(stored_files(dir.path()).len(), 1);
    assert_eq!(reopened.get(org, &id).await.unwrap(), Some(bytes));
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_and_lost_responses_at_every_step_leave_a_consistent_store() {
    for point in [
        points::PUT_TEMP_WRITTEN,
        points::PUT_INSTALLED,
        points::SET_CLASS_COPIED,
    ] {
        for fault in [Fault::unavailable(), Fault::response_lost()] {
            let dir = tempfile::tempdir().unwrap();
            let (store, faults) = faulty_store(dir.path());
            let org = fresh_org();
            let (id, bytes) = blob(b"fault matrix");
            store
                .put(org, &id, bytes.clone(), StorageClass::Hot)
                .await
                .unwrap();
            faults.add(FaultRule::nth(point, 0, fault.clone()));
            let result = if point == points::SET_CLASS_COPIED {
                store
                    .set_class(org, &id, StorageClass::Cold)
                    .await
                    .map(|_| ())
            } else {
                store.put(org, &id, bytes.clone(), StorageClass::Hot).await
            };
            let err = result.unwrap_err();
            assert_eq!(err.reason, Reason::Unavailable, "{point}");
            assert_eq!(
                store.get(org, &id).await.unwrap(),
                Some(bytes.clone()),
                "{point}"
            );
            assert_eq!(store.list(org, None, 10).await.unwrap().len(), 1, "{point}");
            // A retry succeeds and leaves exactly one file.
            store.put(org, &id, bytes, StorageClass::Hot).await.unwrap();
            assert_eq!(stored_files(dir.path()).len(), 1, "{point}");
            assert!(
                files(&dir.path().join("tmp")).is_empty(),
                "{point}: temp files leaked"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn faulty_blob_wrapper_crashes_before_any_effect() {
    let dir = tempfile::tempdir().unwrap();
    let faults = FaultInjector::new();
    let store = Arc::new(FaultyBlob::new(open(dir.path()), faults.clone()));
    let org = fresh_org();
    let (id, bytes) = blob(b"through the wrapper");

    faults.add(FaultRule::nth("blob.put", 0, Fault::Crash));
    let (s2, b2) = (store.clone(), bytes.clone());
    assert!(crashes(move || async move { s2.put(org, &id, b2, StorageClass::Hot).await }).await);
    assert_eq!(store.get(org, &id).await.unwrap(), None);
    assert!(stored_files(dir.path()).is_empty());

    // "Committed, response lost": the caller sees an error, the bytes are durable.
    faults.add(FaultRule::nth("blob.put", 0, Fault::response_lost()));
    let err = store
        .put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap_err();
    assert_eq!(err.reason, Reason::Unavailable);
    assert_eq!(store.get(org, &id).await.unwrap(), Some(bytes.clone()));

    faults.add(FaultRule::nth("blob.get", 0, Fault::unavailable()));
    assert!(store.get(org, &id).await.is_err());
    assert_eq!(store.get(org, &id).await.unwrap(), Some(bytes));
}

// ----- concurrency and counters ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_puts_class_moves_and_reads_agree() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(open(dir.path()));
    let org = fresh_org();
    let (id, bytes) = blob(&noise(40_000, 21));
    store
        .put(org, &id, bytes.clone(), StorageClass::Hot)
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for n in 0..12u32 {
        let (s, b) = (store.clone(), bytes.clone());
        tasks.push(tokio::spawn(async move {
            for round in 0..10 {
                match (n + round) % 3 {
                    0 => s.put(org, &id, b.clone(), StorageClass::Hot).await.unwrap(),
                    1 => {
                        let class = if round % 2 == 0 {
                            StorageClass::Cold
                        } else {
                            StorageClass::Hot
                        };
                        assert!(s.set_class(org, &id, class).await.unwrap());
                    }
                    _ => {
                        // Reads never see the bytes missing or damaged mid-move.
                        assert_eq!(s.get(org, &id).await.unwrap().as_ref(), Some(&b));
                        assert!(s.has(org, &[id]).await.unwrap()[0]);
                    }
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(store.get(org, &id).await.unwrap(), Some(bytes));
    assert_eq!(stored_files(dir.path()).len(), 1);
    assert!(files(&dir.path().join("tmp")).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn operations_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let org = fresh_org();
    let (id, bytes) = blob(b"counted");
    let counters = store.access_counters().unwrap();
    let before = counters.snapshot();
    store
        .put(org, &id, bytes, StorageClass::Cold)
        .await
        .unwrap();
    store.get(org, &id).await.unwrap();
    store.get_range(org, &id, 0, 2).await.unwrap();
    store.has(org, &[id, id]).await.unwrap();
    store.delete(org, &id, None).await.unwrap();
    let delta = counters.snapshot().since(&before);
    assert_eq!(delta.blob.puts, 1);
    assert_eq!(delta.blob.deletes, 1);
    assert_eq!(delta.blob.has, 2);
    assert_eq!(delta.blob.gets, 2);
    assert_eq!(delta.blob.range_gets, 1);
    assert_eq!(delta.blob.cold_reads, 2);
    assert_eq!(delta.blob.bytes_read, 7 + 2);
}
