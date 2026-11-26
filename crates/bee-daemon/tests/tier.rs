//! Tier behaviour against an in-memory upstream: coalescing, per-credential decisions, fail
//! closed, epochs and redaction, signed URLs, integrity, refs, snapshots and the socket API.

use crate::common;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bee_core::{Mode, ObjectId};
use bee_daemon::snapshot::{LinkMode, SnapshotRequest};
use bee_daemon::upstream::Auth;
use bee_daemon::{Tier, TierConfig, TierKind, Upstream};
use bee_proto::v1;
use common::*;
use serde_json::json;

fn auth(token: &str) -> Auth {
    Auth(Some(format!("Bearer {token}")))
}

fn tier_over(mock: &Arc<MockUpstream>, cfg: TierConfig) -> (Arc<Tier>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tmp");
    let store = disk_store(&dir.path().join("cache"), 4 << 30);
    let up: Arc<dyn Upstream> = mock.clone();
    (Arc::new(Tier::new(cfg, store, up)), dir)
}

fn get_req(ids: &[ObjectId]) -> v1::GetObjectsRequest {
    v1::GetObjectsRequest {
        repo: Some(bee_daemon::snapshot::repo_ref(REPO)),
        ids: ids
            .iter()
            .map(|i| v1::ObjectId {
                raw: i.as_bytes().to_vec(),
            })
            .collect(),
        ..Default::default()
    }
}

fn bytes_of(chunks: &[v1::ObjectChunk], id: &ObjectId) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for c in chunks {
        if c.id.as_ref().map(|i| i.raw.as_slice()) != Some(id.as_bytes().as_slice()) {
            continue;
        }
        if c.status != v1::ItemStatus::Ok as i32 {
            return Err(c.error_reason.clone());
        }
        out.extend_from_slice(&c.data);
    }
    Ok(out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn fetch_coalescing_1000_concurrent_requests_one_upstream_fetch() {
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("a");
    mock.delay_ms.store(200, Ordering::SeqCst);
    let ids: Vec<ObjectId> = (0..10)
        .map(|n| {
            mock.add(
                &format!("f{n}"),
                format!("content {n}").as_bytes(),
                Mode::File,
            )
        })
        .collect();
    let (tier, _dir) = tier_over(&mock, TierConfig::new(TierKind::Daemon));
    let started = Instant::now();
    let mut tasks = Vec::new();
    for n in 0..1000 {
        let tier = tier.clone();
        let ids = ids.clone();
        tasks.push(tokio::spawn(async move {
            // Every request asks for all ten objects, in a rotated order.
            let mut mine = ids.clone();
            mine.rotate_left(n % 10);
            tier.get_objects(&auth("a"), get_req(&mine)).await
        }));
    }
    for t in tasks {
        let (chunks, _) = t.await.expect("join").expect("served");
        for (n, id) in ids.iter().enumerate() {
            assert_eq!(
                bytes_of(&chunks, id).expect("ok"),
                format!("content {n}").as_bytes()
            );
        }
    }
    let elapsed = started.elapsed();
    assert_eq!(
        mock.get_calls.load(Ordering::SeqCst),
        1,
        "one upstream fetch"
    );
    let s = tier.stats_view();
    assert_eq!(s.upstream_objects, 10);
    eprintln!(
        "coalescing: 1000 concurrent requests x 10 objects -> {} upstream GetObjects in {:?}",
        mock.get_calls.load(Ordering::SeqCst),
        elapsed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decisions_are_per_credential_and_never_leak_cached_bytes() {
    let mock = Arc::new(MockUpstream::new());
    let secret = mock.add("secret/key", b"hidden", Mode::File);
    let open = mock.add("open.txt", b"open", Mode::File);
    mock.allow_all("alice");
    mock.allow("bob", &[open]);
    mock.allow_all("carol");
    let (tier, _dir) = tier_over(&mock, TierConfig::new(TierKind::Daemon));
    let (c, _) = tier
        .get_objects(&auth("alice"), get_req(&[secret, open]))
        .await
        .expect("alice");
    assert_eq!(bytes_of(&c, &secret).expect("alice reads"), b"hidden");
    assert!(tier.store().contains(&secret));
    // Bob may read `open` but not `secret`: the cached secret is not served to him.
    let (c, _) = tier
        .get_objects(&auth("bob"), get_req(&[secret, open]))
        .await
        .expect("bob");
    assert_eq!(bytes_of(&c, &secret), Err("NOT_FOUND".to_owned()));
    assert_eq!(bytes_of(&c, &open).expect("open"), b"open");
    // No credential at all: the tier above's refusal comes back unchanged.
    let e = tier
        .get_objects(&Auth::default(), get_req(&[open]))
        .await
        .expect_err("anonymous");
    assert_eq!(e.reason, "UNAUTHENTICATED");
    // Carol is served from the cache after one decision check, without fetching bytes.
    let gets = mock.get_calls.load(Ordering::SeqCst);
    let (c, _) = tier
        .get_objects(&auth("carol"), get_req(&[secret, open]))
        .await
        .expect("carol");
    assert_eq!(bytes_of(&c, &secret).expect("carol reads"), b"hidden");
    assert_eq!(
        mock.get_calls.load(Ordering::SeqCst),
        gets,
        "no byte fetch for carol"
    );
    // And again with no upstream call at all while her decisions are confirmed.
    let has = mock.has_calls.load(Ordering::SeqCst);
    tier.get_objects(&auth("carol"), get_req(&[secret]))
        .await
        .expect("carol again");
    assert_eq!(mock.has_calls.load(Ordering::SeqCst), has);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fails_closed_when_decisions_cannot_be_confirmed() {
    let mock = Arc::new(MockUpstream::new());
    let id = mock.add("a", b"aaa", Mode::File);
    mock.allow_all("a");
    let mut cfg = TierConfig::new(TierKind::Daemon);
    cfg.confirm_ms = 100;
    cfg.max_retries = 0;
    let (tier, _dir) = tier_over(&mock, cfg);
    tier.get_objects(&auth("a"), get_req(&[id]))
        .await
        .expect("warm");
    mock.down.store(true, Ordering::SeqCst);
    // Inside the confirmation window the cache serves.
    let (c, _) = tier
        .get_objects(&auth("a"), get_req(&[id]))
        .await
        .expect("within the window");
    assert_eq!(bytes_of(&c, &id).expect("ok"), b"aaa");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let e = tier
        .get_objects(&auth("a"), get_req(&[id]))
        .await
        .expect_err("fail closed");
    assert_eq!(e.reason, "UNAVAILABLE");
    assert!(tier.stats_view().fail_closed >= 1);
    mock.down.store(false, Ordering::SeqCst);
    tier.get_objects(&auth("a"), get_req(&[id]))
        .await
        .expect("recovers");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn epoch_changes_drop_decisions_and_redaction_evicts() {
    let mock = Arc::new(MockUpstream::new());
    let a = mock.add("a", b"first", Mode::File);
    let b = mock.add("b", b"second", Mode::File);
    mock.allow_all("t");
    let mut cfg = TierConfig::new(TierKind::Daemon);
    cfg.confirm_ms = 60_000;
    let (tier, _dir) = tier_over(&mock, cfg);
    tier.get_objects(&auth("t"), get_req(&[a]))
        .await
        .expect("a");
    // A policy change seen on any response drops the repo's decisions.
    mock.epochs.lock().expect("lock").policy = "2".into();
    tier.get_objects(&auth("t"), get_req(&[b]))
        .await
        .expect("b");
    let has = mock.has_calls.load(Ordering::SeqCst);
    tier.get_objects(&auth("t"), get_req(&[a]))
        .await
        .expect("a again");
    assert_eq!(
        mock.has_calls.load(Ordering::SeqCst),
        has + 1,
        "a is re-authorized after the epoch change"
    );
    // Redaction: the next upstream answer carries the new redaction epoch, every decision is
    // re-made, the tier above says REDACTED, and the bytes leave the cache.
    let c = mock.add("c", b"third", Mode::File);
    mock.redacted.lock().expect("lock").insert(a);
    mock.epochs.lock().expect("lock").redaction = "2".into();
    tier.get_objects(&auth("t"), get_req(&[c]))
        .await
        .expect("observe");
    let (c, _) = tier
        .get_objects(&auth("t"), get_req(&[a]))
        .await
        .expect("a redacted");
    assert_eq!(bytes_of(&c, &a), Err("REDACTED".to_owned()));
    assert!(!tier.store().contains(&a), "redacted bytes are evicted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_urls_are_fetched_and_fall_back_to_the_api() {
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("t");
    mock.signed_min.store(64 * 1024, Ordering::SeqCst);
    let big1 = mock.add("big1", &vec![1u8; 100 * 1024], Mode::File);
    let big2 = mock.add("big2", &vec![2u8; 100 * 1024], Mode::File);
    let small = mock.add("small", b"tiny", Mode::File);
    let (tier, _dir) = tier_over(&mock, TierConfig::new(TierKind::Daemon));
    let (c, _) = tier
        .get_objects(&auth("t"), get_req(&[big1, small]))
        .await
        .expect("get");
    assert_eq!(bytes_of(&c, &big1).expect("big1").len(), 100 * 1024);
    let s = tier.stats_view();
    assert_eq!(s.signed_url_fetches, 1);
    assert_eq!(s.signed_url_fallbacks, 0);
    // The CDN is down: the object still arrives, through the API.
    mock.cdn_down.store(true, Ordering::SeqCst);
    let (c, _) = tier
        .get_objects(&auth("t"), get_req(&[big2]))
        .await
        .expect("get");
    assert_eq!(bytes_of(&c, &big2).expect("big2"), vec![2u8; 100 * 1024]);
    let s = tier.stats_view();
    assert_eq!(s.signed_url_fallbacks, 1);
    // A client never receives a URL from the tier: bytes are served from the cache.
    assert!(c.iter().all(|ch| ch.signed_url.is_empty()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_upstream_bytes_are_refetched_never_cached() {
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("t");
    let id = mock.add("x", b"precious bytes", Mode::File);
    mock.corrupt_once.lock().expect("lock").insert(id);
    let (tier, _dir) = tier_over(&mock, TierConfig::new(TierKind::Daemon));
    let (c, _) = tier
        .get_objects(&auth("t"), get_req(&[id]))
        .await
        .expect("get");
    assert_eq!(bytes_of(&c, &id).expect("ok"), b"precious bytes");
    assert_eq!(mock.get_calls.load(Ordering::SeqCst), 2, "one retry");
    assert_eq!(
        tier.store().get(&id).as_deref(),
        Some(&b"precious bytes"[..])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refs_honour_min_version_and_never_go_backwards() {
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("t");
    let mut cfg = TierConfig::new(TierKind::Daemon);
    cfg.ref_ttl_ms = 300;
    let (tier, _dir) = tier_over(&mock, cfg);
    let req = |min: u64| v1::ResolveRefRequest {
        repo: Some(bee_daemon::snapshot::repo_ref(REPO)),
        name: "main".into(),
        min_version: min,
        at_version: 0,
    };
    let version = |r: &v1::ResolveRefResponse| r.r#ref.as_ref().expect("ref").version;
    let (r, _) = tier.resolve_ref(&auth("t"), req(0)).await.expect("v1");
    assert_eq!(version(&r), 1);
    mock.ref_version.store(5, Ordering::SeqCst);
    let (r, _) = tier.resolve_ref(&auth("t"), req(0)).await.expect("cached");
    assert_eq!(version(&r), 1, "served from cache within the TTL");
    let (r, _) = tier.resolve_ref(&auth("t"), req(5)).await.expect("ryw");
    assert_eq!(version(&r), 5, "min_version forces a fresh read");
    // After the TTL a lagging answer (a replica at 3) must not move the ref backwards.
    mock.ref_version.store(3, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(350)).await;
    let (r, _) = tier.resolve_ref(&auth("t"), req(0)).await.expect("refresh");
    assert_eq!(version(&r), 5, "never older than what was already served");
    let s = tier.stats_view();
    assert_eq!(s.ref_hits, 1);
    assert_eq!(s.ref_misses, 3);
}

fn synthetic_tree(mock: &MockUpstream, files: usize) -> u64 {
    let mut bytes = 0u64;
    for n in 0..files {
        let path = format!("dir{:02}/sub{:03}/file{n:05}.txt", (n / 400) % 25, n / 20);
        let content = format!("file {n}\n{}\n", "x".repeat(n % 400));
        bytes += content.len() as u64;
        let mode = if n % 97 == 0 {
            Mode::Executable
        } else {
            Mode::File
        };
        mock.add(&path, content.as_bytes(), mode);
    }
    mock.add(
        "links/readme",
        b"../dir00/sub000/file00000.txt",
        Mode::Symlink,
    );
    bytes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn snapshot_of_a_10k_file_tree() {
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("t");
    let total = synthetic_tree(&mock, 10_000);
    let (tier, dir) = tier_over(&mock, TierConfig::new(TierKind::Daemon));
    let req = |dest: &str, mode: LinkMode| SnapshotRequest {
        repo: REPO.into(),
        revision: "main".into(),
        dest: dir.path().join(dest),
        mode,
        ..Default::default()
    };
    let t = Instant::now();
    let cold =
        bee_daemon::snapshot::snapshot(tier.clone(), &auth("t"), req("cold", LinkMode::Clone))
            .await
            .expect("cold snapshot");
    let cold_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(cold.files, 10_000);
    assert_eq!(cold.symlinks, 1);
    assert_eq!(cold.bytes, total);
    assert_eq!(cold.fetched, 10_001);
    let mut warm = Vec::new();
    for (name, mode) in [
        ("clone", LinkMode::Clone),
        ("hardlink", LinkMode::Hardlink),
        ("copy", LinkMode::Copy),
    ] {
        let t = Instant::now();
        let r = bee_daemon::snapshot::snapshot(tier.clone(), &auth("t"), req(name, mode))
            .await
            .expect("warm snapshot");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(r.files, 10_000);
        assert_eq!(r.fetched, 0, "warm snapshots fetch nothing");
        assert!(r.skipped.is_empty(), "{:?}", r.skipped);
        warm.push((name, ms, r));
    }
    // Contents are right in every mode, and modes are kept.
    let a = tree_files(&dir.path().join("cold"));
    for (name, _, _) in &warm {
        assert_eq!(tree_files(&dir.path().join(name)), a, "{name}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let exe = dir.path().join("clone/dir00/sub000/file00000.txt");
        assert_eq!(
            std::fs::metadata(&exe).expect("exe").permissions().mode() & 0o777,
            0o755
        );
        let linked = dir.path().join("hardlink/dir00/sub000/file00001.txt");
        let meta = std::fs::metadata(&linked).expect("linked");
        assert!(meta.permissions().readonly(), "hard links are read-only");
        assert!(meta.nlink() >= 2, "a hard link shares the cache inode");
        // Editing a cloned file never touches the cache.
        let cloned = dir.path().join("clone/dir00/sub000/file00001.txt");
        std::fs::write(&cloned, b"edited").expect("cloned files are writable");
        let id = bee_object::blob_id(&a["dir00/sub000/file00001.txt"]);
        assert!(
            tier.store().get(&id).is_some(),
            "the cache object still verifies"
        );
    }
    let hard = &warm[1];
    eprintln!(
        "snapshot 10k files ({total} bytes): cold clone {cold_ms:.0} ms (list {:.0}, fetch {:.0}, materialize {:.0}); warm clone {:.0} ms, hardlink {:.0} ms, copy {:.0} ms",
        cold.timings.list_ms,
        cold.timings.fetch_ms,
        cold.timings.materialize_ms,
        warm[0].1,
        hard.1,
        warm[2].1
    );
    // Generous ceilings for a loaded CI machine; the spec records measured numbers.
    assert!(hard.1 < 30_000.0, "hardlink snapshot took {} ms", hard.1);
    assert!(warm[0].1 < 60_000.0, "clone snapshot took {} ms", warm[0].1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn hundred_agents_snapshot_the_same_revision_with_one_fetch_per_object() {
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("t");
    mock.delay_ms.store(20, Ordering::SeqCst);
    synthetic_tree(&mock, 300);
    let (tier, dir) = tier_over(&mock, TierConfig::new(TierKind::Daemon));
    let t = Instant::now();
    let mut tasks = Vec::new();
    for n in 0..100 {
        let tier = tier.clone();
        let dest = dir.path().join(format!("agent{n:03}"));
        tasks.push(tokio::spawn(async move {
            bee_daemon::snapshot::snapshot(
                tier,
                &auth("t"),
                SnapshotRequest {
                    repo: REPO.into(),
                    revision: "main".into(),
                    dest,
                    mode: LinkMode::Hardlink,
                    ..Default::default()
                },
            )
            .await
        }));
    }
    let mut phases = [0f64; 3];
    for task in tasks {
        let r = task.await.expect("join").expect("snapshot");
        assert_eq!(r.files, 300);
        phases[0] += r.timings.list_ms;
        phases[1] += r.timings.fetch_ms;
        phases[2] += r.timings.materialize_ms;
    }
    eprintln!(
        "per-agent mean: list {:.0} ms, fetch {:.0} ms, materialize {:.0} ms",
        phases[0] / 100.0,
        phases[1] / 100.0,
        phases[2] / 100.0
    );
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(
        tier.stats_view().upstream_objects,
        301,
        "each object fetched once"
    );
    eprintln!("100 agents x 300-file snapshot (hardlink): {ms:.0} ms total");
}

#[test]
fn daemon_socket_serves_objects_and_local_operations() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("rt");
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("t");
    let id = mock.add("hello.txt", b"hello world", Mode::File);
    mock.add("docs/a.md", b"# a", Mode::File);
    let up: Arc<dyn Upstream> = mock.clone();
    let mut d = start_daemon(&rt, up, TierConfig::new(TierKind::Daemon));
    let c = d.client(Some("t"));
    let items = c
        .server_stream(
            "/bee.v1.ObjectService/GetObjects",
            &json!({ "repo": { "fullName": REPO }, "ids": [id_json(&id)] }),
        )
        .expect("get objects");
    assert_eq!(objects_of(&items)[&id], Ok(b"hello world".to_vec()));
    let info = c
        .unary("/bee.v1.ObjectService/GetServerInfo", &json!({}))
        .expect("info");
    assert_eq!(info["tier"], "daemon");
    let stats = c
        .unary("/bee.v1.ObjectService/GetTierStats", &json!({}))
        .expect("tier stats");
    assert_eq!(stats["tier"], "daemon");
    // Daemon-local operations.
    let tree = c
        .local(
            "Tree",
            &json!({ "repo": REPO, "revision": "main", "prefix": "docs" }),
        )
        .expect("tree");
    assert_eq!(tree["entries"].as_array().expect("entries").len(), 1);
    let dest = d.dir.path().join("snap");
    let snap = c
        .local(
            "Snapshot",
            &json!({ "repo": REPO, "revision": "main", "dest": dest, "mode": "hardlink" }),
        )
        .expect("snapshot");
    assert_eq!(snap["files"], 2);
    assert_eq!(
        std::fs::read(dest.join("hello.txt")).expect("file"),
        b"hello world"
    );
    let pin = c
        .local(
            "Pin",
            &json!({ "name": "release", "repo": REPO, "revision": "main" }),
        )
        .expect("pin");
    assert_eq!(pin["objects"], 2);
    let pins = c.local("Pins", &json!({})).expect("pins");
    assert_eq!(pins["pins"][0]["name"], "release");
    assert_eq!(
        c.local("Unpin", &json!({ "name": "release" }))
            .expect("unpin")["removed"],
        true
    );
    let e = c.local("Nope", &json!({})).expect_err("unknown op");
    assert_eq!(e.reason, "UNIMPLEMENTED");
    let s = c.local("Stats", &json!({})).expect("stats");
    assert!(s["hits"].as_u64().expect("hits") >= 1);
    // A non-object RPC is forwarded to the tier above, which this mock cannot answer.
    let e = c
        .unary(
            "/bee.v1.QueryService/Log",
            &json!({ "repo": { "fullName": REPO } }),
        )
        .expect_err("forwarded");
    assert_eq!(e.reason, "UNIMPLEMENTED");
    d.stop(&rt);
}

#[test]
fn regional_cache_refuses_local_file_system_operations() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("rt");
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("t");
    let up: Arc<dyn Upstream> = mock.clone();
    let mut r = start_regional(&rt, up, TierConfig::new(TierKind::Regional));
    let c = r.client(Some("t"));
    let e = c
        .local(
            "Snapshot",
            &json!({ "repo": REPO, "revision": "main", "dest": "/tmp/x" }),
        )
        .expect_err("refused");
    assert_eq!(e.reason, "PERMISSION_DENIED");
    let info = c
        .unary("/bee.v1.ObjectService/GetServerInfo", &json!({}))
        .expect("info");
    assert_eq!(info["tier"], "cache");
    r.stop(&rt);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eviction_under_pressure_keeps_pinned_objects() {
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("t");
    let ids: Vec<ObjectId> = (0..20)
        .map(|n| mock.add(&format!("f{n:02}"), &vec![n as u8; 1000], Mode::File))
        .collect();
    let dir = tempfile::tempdir().expect("tmp");
    let store = disk_store(&dir.path().join("cache"), 10_000);
    let up: Arc<dyn Upstream> = mock.clone();
    let tier = Arc::new(Tier::new(TierConfig::new(TierKind::Daemon), store, up));
    tier.store().pin_set("keep", &ids[..3]).expect("pin");
    for chunk in ids.chunks(2) {
        tier.get_objects(&auth("t"), get_req(chunk))
            .await
            .expect("get");
    }
    let st = tier.store().stats();
    assert!(st.bytes <= 10_000, "{st:?}");
    assert!(st.evictions > 0);
    for id in &ids[..3] {
        assert!(tier.store().contains(id), "pinned objects stay");
    }
    // Evicted objects come back transparently.
    let (c, _) = tier
        .get_objects(&auth("t"), get_req(&ids[4..5]))
        .await
        .expect("refetch");
    assert_eq!(bytes_of(&c, &ids[4]).expect("ok"), vec![4u8; 1000]);
}
