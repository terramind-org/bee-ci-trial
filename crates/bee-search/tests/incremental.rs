//! Incremental indexing: deltas touch only changed blobs, dedup across repos, fallback
//! listing, deterministic rebuilds, ref removal and garbage collection, on both stores.

use crate::common;
use std::sync::Arc;

use bee_search::store::IndexStore;
use bee_search::{DiskIndexStore, Indexer, MemoryIndexStore};
use bee_store::{BlobStore, MetaStore};
use bee_testkit::{Change, ref_name};
use common::{World, allow_all};

fn files(n: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| {
            (
                format!("src/mod{i}/file{i}.rs"),
                format!("pub fn func_{i}() -> u32 {{\n    {i}\n}}\n"),
            )
        })
        .collect()
}

async fn delta_touches_only_changed_blobs(store: Arc<dyn IndexStore>) {
    let mut w = World::with_store(store);
    let f = files(40);
    let refs: Vec<(&str, &str)> = f.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
    let r = w.repo("acme", "api", &refs).await;
    let org = r.org;
    assert_eq!(w.store.doc_count(org).unwrap(), 40);

    // One file changes, one is added, one is removed: exactly two new blobs are indexed.
    let stats = w
        .change(
            "acme/api",
            &[
                Change::put(
                    "src/mod3/file3.rs",
                    "pub fn func_3() -> u32 {\n    333\n}\n",
                ),
                Change::put("src/new.rs", "pub fn brand_new() {}\n"),
                Change::delete("src/mod7/file7.rs"),
            ],
        )
        .await;
    assert_eq!(stats.deltas_replayed, 1);
    assert!(!stats.full_listing);
    assert_eq!(stats.paths_changed, 3);
    assert_eq!(stats.blobs_indexed, 2);
    assert_eq!(stats.blobs_reused, 0);
    assert_eq!(w.store.doc_count(org).unwrap(), 42);

    let hits = w.search("333", &allow_all);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path.to_string(), "src/mod3/file3.rs");
    assert!(w.search("func_7", &allow_all).is_empty());
    assert_eq!(w.search("brand_new", &allow_all).len(), 1);

    // Re-running is a no-op.
    let again = w
        .indexer
        .sync_ref(org, r.repo, &ref_name("heads/main"))
        .await
        .unwrap();
    assert_eq!(again.paths_changed, 0);
    assert_eq!(again.blobs_indexed, 0);

    // Moving a file to a new path reuses its document (content-addressed).
    let stats = w
        .change(
            "acme/api",
            &[
                Change::delete("src/mod1/file1.rs"),
                Change::put("moved/file1.rs", "pub fn func_1() -> u32 {\n    1\n}\n"),
            ],
        )
        .await;
    assert_eq!(stats.blobs_indexed, 0);
    assert_eq!(stats.blobs_reused, 1);
    assert_eq!(stats.paths_changed, 2);

    // A second repo of the same org with the same content indexes nothing new.
    let before = w.store.doc_count(org).unwrap();
    let r2 = w.env.create_repo("acme", "fork").await.unwrap();
    let changes: Vec<Change> = f.iter().map(|(p, c)| Change::put(p, c.as_str())).collect();
    w.env
        .commit_on(&r2, &ref_name("heads/main"), &changes)
        .await
        .unwrap();
    let s2 = w
        .indexer
        .sync_ref(org, r2.repo, &ref_name("heads/main"))
        .await
        .unwrap();
    assert_eq!(s2.blobs_indexed, 0);
    assert_eq!(s2.blobs_reused, 40);
    assert_eq!(w.store.doc_count(org).unwrap(), before);

    // Several commits between index runs replay every delta in order.
    for i in 0..5 {
        w.env
            .commit_on(
                &r,
                &ref_name("heads/main"),
                &[Change::put(
                    "src/counter.rs",
                    format!("const N: u32 = {i};\n"),
                )],
            )
            .await
            .unwrap();
    }
    let s = w
        .indexer
        .sync_ref(org, r.repo, &ref_name("heads/main"))
        .await
        .unwrap();
    assert_eq!(s.deltas_replayed, 5);
    assert_eq!(s.blobs_indexed, 1);
    assert_eq!(s.paths_changed, 1);
}

#[tokio::test]
async fn incremental_reindex_touches_only_changed_blobs_memory() {
    delta_touches_only_changed_blobs(Arc::new(MemoryIndexStore::new())).await;
}

#[tokio::test]
async fn incremental_reindex_touches_only_changed_blobs_disk() {
    let dir = tempfile::tempdir().unwrap();
    delta_touches_only_changed_blobs(Arc::new(DiskIndexStore::open(dir.path()).unwrap())).await;
}

#[tokio::test]
async fn full_listing_fallback_matches_delta_replay_and_rebuilds_deterministically() {
    let mut w = World::new();
    let f = files(12);
    let refs: Vec<(&str, &str)> = f.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
    let r = w.repo("acme", "api", &refs).await;
    for i in 0..3 {
        w.change(
            "acme/api",
            &[
                Change::put(&format!("extra/e{i}.txt"), format!("extra {i}\n")),
                Change::delete(&format!("src/mod{i}/file{i}.rs")),
            ],
        )
        .await;
    }
    let delta_manifest = w
        .store
        .manifest(r.org, r.repo, "heads/main")
        .unwrap()
        .unwrap();

    // A fresh index built by listing the whole tree (no delta walk allowed).
    let meta: Arc<dyn MetaStore> = w.env.meta.clone();
    let blobs: Arc<dyn BlobStore> = w.env.blobs.clone();
    let fresh: Arc<dyn IndexStore> = Arc::new(MemoryIndexStore::new());
    let listing = Indexer::new(fresh.clone(), meta.clone(), blobs.clone()).with_max_delta_walk(0);
    let s = listing
        .sync_ref(r.org, r.repo, &ref_name("heads/main"))
        .await
        .unwrap();
    assert!(s.full_listing);
    let listed = fresh
        .manifest(r.org, r.repo, "heads/main")
        .unwrap()
        .unwrap();
    let paths = |m: &bee_search::store::Manifest| {
        m.files
            .iter()
            .map(|(p, f)| (p.clone(), f.blob, f.mode, f.language))
            .collect::<Vec<_>>()
    };
    assert_eq!(paths(&listed), paths(&delta_manifest));
    assert_eq!(listed.changeset, delta_manifest.changeset);

    // Rebuilding from scratch twice gives byte-identical manifests (doc numbers included).
    let rebuild = || async {
        let s: Arc<dyn IndexStore> = Arc::new(MemoryIndexStore::new());
        Indexer::new(s.clone(), meta.clone(), blobs.clone())
            .sync_ref(r.org, r.repo, &ref_name("heads/main"))
            .await
            .unwrap();
        s.manifest(r.org, r.repo, "heads/main").unwrap().unwrap()
    };
    let a = rebuild().await;
    let b = rebuild().await;
    assert_eq!(a, b);
    assert_eq!(
        bee_search::store::codec::encode_manifest(&a),
        bee_search::store::codec::encode_manifest(&b)
    );
}

#[tokio::test]
async fn ref_removal_and_garbage_collection() {
    let mut w = World::new();
    let r = w
        .repo(
            "acme",
            "api",
            &[("a.rs", "fn shared() {}\n"), ("b.rs", "fn only_b() {}\n")],
        )
        .await;
    // A second ref keeps a.rs alive after main moves on.
    w.env
        .advance(
            &r,
            &ref_name("heads/keep"),
            None,
            w.env
                .meta
                .get_ref(r.repo, &ref_name("heads/main"))
                .await
                .unwrap()
                .unwrap()
                .target
                .unwrap(),
        )
        .await
        .unwrap();
    let all = w.indexer.sync_repo(r.org, r.repo, "heads/").await.unwrap();
    assert_eq!(all.len(), 2);
    w.change(
        "acme/api",
        &[
            Change::put("a.rs", "fn renamed() {}\n"),
            Change::delete("b.rs"),
        ],
    )
    .await;
    assert_eq!(
        w.indexer.collect_garbage(r.org).unwrap(),
        0,
        "heads/keep still uses both"
    );

    // Deleting the ref removes its manifest; GC then drops the blobs nobody uses.
    let keep = w
        .env
        .meta
        .get_ref(r.repo, &ref_name("heads/keep"))
        .await
        .unwrap()
        .unwrap();
    w.env
        .writer
        .update_ref(bee_store::RefUpdate::set(
            r.repo,
            ref_name("heads/keep"),
            bee_store::Expected::Version(keep.version),
            None,
            "agent:test",
            w.env.now(),
        ))
        .await
        .unwrap();
    let out = w.indexer.sync_repo(r.org, r.repo, "heads/").await.unwrap();
    assert!(out.iter().any(|(n, _)| n == "heads/keep"));
    assert!(
        w.store
            .manifest(r.org, r.repo, "heads/keep")
            .unwrap()
            .is_none()
    );
    assert_eq!(w.indexer.collect_garbage(r.org).unwrap(), 2);
    assert_eq!(w.store.doc_count(r.org).unwrap(), 1);
    assert_eq!(w.search("renamed", &allow_all).len(), 1);
}

#[tokio::test]
async fn binary_large_and_symlink_files() {
    let mut w = World::new();
    let r = w.env.create_repo("acme", "assets").await.unwrap();
    let big = "needle ".repeat((bee_search::store::MAX_INDEXED_BYTES as usize / 7) + 10);
    w.env
        .commit_on(
            &r,
            &ref_name("heads/main"),
            &[
                Change::put("img.bin", b"needle\0\x01\x02".to_vec()),
                Change::put("big.txt", big),
                Change::Put {
                    path: "link".into(),
                    mode: bee_core::Mode::Symlink,
                    data: b"needle".to_vec(),
                },
                Change::put("small.txt", "a needle here\n"),
                Change::put("bin/tool", "#!/usr/bin/env python3\nprint('needle')\n"),
            ],
        )
        .await
        .unwrap();
    let s = w
        .indexer
        .sync_ref(r.org, r.repo, &ref_name("heads/main"))
        .await
        .unwrap();
    assert_eq!(s.blobs_indexed, 4, "the symlink has no document");
    w.targets.push(bee_search::RepoTarget {
        org: r.org,
        org_name: "acme".into(),
        repo: r.repo,
        repo_name: "assets".into(),
        ref_name: "heads/main".into(),
    });
    let mut paths: Vec<String> = w
        .search("needle", &allow_all)
        .iter()
        .map(|h| h.path.to_string())
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["bin/tool", "small.txt"]);
    let py = w.search("needle lang:python", &allow_all);
    assert_eq!(py.len(), 1, "shebang detection");
    // Metadata-only queries still list every file, symlink included.
    assert_eq!(w.search("path:link", &allow_all).len(), 1);
    assert!(w.search("path:link", &allow_all)[0].path_matched);
}
