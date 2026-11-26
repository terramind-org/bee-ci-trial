//! Complexity gates through the real server (SPEC §14.6, §16): the storage work of an
//! operation must not grow with history length or repository size.
//!
//! One repository grows in three stages, each 10× the previous in both history (changesets on
//! `main`) and size (files): gate sizes 2 / 20 / 200 changesets and 50 / 500 / 5,000
//! files; `BEE_SCALE_FULL=1` uses 20 / 200 / 2,000 and 1,000 / 10,000 / 100,000. After each
//! stage the same operations run once and their storage access is counted with the metadata
//! store's and blob store's access counters (rows read and written, transactions, tree and
//! blob fetches), never wall-clock time, so the gate is stable on a loaded machine:
//!
//! - one-file CreateChangeset (fixed depth): sub-linear, reading only its own new blob;
//! - uncontended land of a one-change stack: a constant number of transactions and rows;
//! - land over an unrelated tip (a rebase): zero blob reads, constant work;
//! - a 10-entry log page, and a log page filtered to one file: constant;
//! - blame of a file with a fixed number of edits while unrelated history grows: constant;
//! - diff with classification (SPEC §11.3) of a one-file change: constant;
//! - sparse fetch of a fixed 50-file prefix (ListTree + GetObjects): the same fetch count.
//!
//! Status against tree size is a working-copy property (no server storage involved); its
//! gate is `bee-daemon`'s `watcher_status_rescans_only_changed_paths_on_a_large_tree`.

use crate::scale_support;

use std::collections::BTreeMap;

use bee_conformance::load::client::{Client, change_id};
use bee_core::ObjectId;
use bee_store::{AccessSnapshot, FetchKind};
use scale_support::*;

/// The counted work of one operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Work {
    transactions: u64,
    reads: u64,
    writes: u64,
    tree_fetches: u64,
    blob_fetches: u64,
    blob_gets: u64,
}

impl Work {
    fn of(meta: &AccessSnapshot, blob: &AccessSnapshot) -> Work {
        Work {
            transactions: meta.transactions,
            reads: meta.total_reads(),
            writes: meta.total_writes(),
            tree_fetches: meta.fetches_of(FetchKind::Tree) + blob.fetches_of(FetchKind::Tree),
            blob_fetches: meta.fetches_of(FetchKind::Blob) + blob.fetches_of(FetchKind::Blob),
            blob_gets: blob.blob.gets,
        }
    }

    fn fields(&self) -> [(&'static str, u64); 6] {
        [
            ("transactions", self.transactions),
            ("rows read", self.reads),
            ("rows written", self.writes),
            ("tree fetches", self.tree_fetches),
            ("blob fetches", self.blob_fetches),
            ("blob-store gets", self.blob_gets),
        ]
    }
}

struct Counted<'a> {
    s: &'a Srv,
}

impl Counted<'_> {
    fn snap(&self) -> (AccessSnapshot, AccessSnapshot) {
        let m = self
            .s
            .srv
            .state
            .meta
            .access_counters()
            .expect("the metadata store counts accesses")
            .snapshot();
        let b = self
            .s
            .srv
            .state
            .blobs
            .access_counters()
            .map(|c| c.snapshot())
            .unwrap_or_default();
        (m, b)
    }

    async fn measure<T, F: std::future::Future<Output = T>>(&self, f: F) -> (T, Work) {
        let (m0, b0) = self.snap();
        let out = f.await;
        let (m1, b1) = self.snap();
        (out, Work::of(&m1.since(&m0), &b1.since(&b0)))
    }
}

/// Grows `main` by `commits` changesets (a chain landed as one stack) and `files` files.
async fn grow(c: &Client, stage: usize, commits: usize, files: usize) {
    let (tip, _) = main_tip(c).await;
    // Files: one changeset spreading them over two directory levels.
    let blobs: Vec<Vec<u8>> = (0..files)
        .map(|i| format!("bulk {stage} {i}\n").into_bytes())
        .collect();
    for chunk in blobs.chunks(2_000) {
        c.upload(chunk).await.expect("upload bulk");
    }
    let puts: Vec<(String, ObjectId, u64)> = blobs
        .iter()
        .enumerate()
        .map(|(i, b)| {
            (
                format!(
                    "bulk{stage}/d{:02}/e{:02}/f{i:06}.txt",
                    i % 100,
                    (i / 100) % 100
                ),
                bee_object::blob_id(b),
                b.len() as u64,
            )
        })
        .collect();
    let mut head = c
        .create(
            &tip,
            &puts,
            change_id(9_000_000 + stage as u64),
            "bulk files",
        )
        .await
        .expect("bulk changeset");
    // History: a chain of unrelated one-file changesets.
    for i in 1..commits {
        let file = (
            format!("noise/{stage}/{i}.txt"),
            format!("noise {stage} {i}\n").into_bytes(),
        );
        head = c
            .commit(
                &head,
                &[file],
                change_id(1_000_000_000 + 1_000_000 * stage as u64 + i as u64),
                "noise",
            )
            .await
            .expect("noise commit");
    }
    c.land("main", &head).await.expect("land growth");
}

/// Runs every measured operation once against the current repo.
async fn measure_all(s: &Srv, stage: usize, sparse: &[ObjectId]) -> BTreeMap<&'static str, Work> {
    let c = &s.client;
    let k = Counted { s };
    let mut out = BTreeMap::new();
    let (tip, _) = main_tip(c).await;

    // One-file CreateChangeset (the blob is uploaded first, as a client does).
    let data = format!("hot {stage}\n").into_bytes();
    c.upload(std::slice::from_ref(&data)).await.expect("upload");
    let put = vec![(
        "hot/file.txt".to_string(),
        bee_object::blob_id(&data),
        data.len() as u64,
    )];
    let (head, w) = k
        .measure(c.create(&tip, &put, change_id(20_000_000 + stage as u64), "hot"))
        .await;
    out.insert("create_one_file", w);
    let head = head.expect("create");

    // Uncontended land of that one-change stack.
    let (r, w) = k.measure(c.land("main", &head)).await;
    r.expect("land");
    out.insert("land_uncontended", w);

    // Land over an unrelated tip: prepare a change, move main with another, then land.
    let (tip, _) = main_tip(c).await;
    let mine = c
        .commit(
            &tip,
            &[(
                "rebase/mine.txt".into(),
                format!("m {stage}\n").into_bytes(),
            )],
            change_id(30_000_000 + stage as u64),
            "mine",
        )
        .await
        .expect("mine");
    let other = c
        .commit(
            &tip,
            &[(
                "rebase/other.txt".into(),
                format!("o {stage}\n").into_bytes(),
            )],
            change_id(40_000_000 + stage as u64),
            "other",
        )
        .await
        .expect("other");
    c.land("main", &other).await.expect("land other");
    let (r, w) = k.measure(c.land("main", &mine)).await;
    r.expect("land over unrelated tip");
    out.insert("land_rebase_unrelated", w);

    let (r, w) = k.measure(c.log("main", 10)).await;
    assert_eq!(r.expect("log"), 10);
    out.insert("log_page_10", w);

    let (r, w) = k.measure(c.log_path("main", "blamed.txt", 10)).await;
    assert_eq!(r.expect("log path"), 5);
    out.insert("log_path_page", w);

    let (r, w) = k.measure(c.blame("main", "blamed.txt")).await;
    assert!(r.expect("blame") > 0);
    out.insert("blame_fixed_file", w);

    let (r, w) = k.measure(c.diff("main~1", "main", true)).await;
    let items = r.expect("diff");
    assert_eq!(items.len(), 1, "{items:?}");
    assert!(!items[0].1.is_empty(), "the diff is classified: {items:?}");
    out.insert("diff_classified", w);

    let (r, w) = k
        .measure(async {
            let files = c.list_tree("main", "sparse").await.expect("list tree");
            let ids: Vec<ObjectId> = files.iter().map(|f| f.1).collect();
            c.get_objects(&ids).await.map(|bytes| (files.len(), bytes))
        })
        .await;
    let (n, bytes) = r.expect("sparse fetch");
    assert_eq!(n, sparse.len());
    assert!(bytes > 0);
    out.insert("sparse_fetch_50", w);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_work_does_not_grow_with_history_or_repo_size() {
    // The forge, the search indexer, the ops sampler and post-land warming are off:
    // their background workers read the event log and the trees (and warming writes derived
    // rows) and would add noise to the counters. Rows are not shared between requests, so
    // each count is one request's own work. Everything measured here is VCS-only
    // (SPEC §15.1).
    let (s, _dir) = start_fresh(
        |c| {
            c.forge.enabled = false;
            c.search.enabled = false;
            c.ops.metrics_sample_ms = 3_600_000;
            c.land.warm_blame = false;
            c.land.warm_git = false;
            c.land.warm_pack = false;
            c.server.share_history_rows = false;
        },
        None,
    )
    .await;
    let c = &s.client;
    let (tip, _) = main_tip(c).await;
    // A file with exactly five edits, and a fixed 50-file sparse prefix.
    let sparse_files: Vec<(String, Vec<u8>)> = (0..50)
        .map(|i| {
            (
                format!("sparse/p{i:02}.txt"),
                format!("sparse {i}\n").into_bytes(),
            )
        })
        .collect();
    let mut head = c
        .commit(&tip, &sparse_files, change_id(1), "sparse prefix")
        .await
        .expect("sparse");
    for e in 0..5u64 {
        let text: String = (0..200)
            .map(|l| {
                if l % 40 == e as usize {
                    format!("edit {e} line {l}\n")
                } else {
                    format!("line {l}\n")
                }
            })
            .collect();
        head = c
            .commit(
                &head,
                &[("blamed.txt".into(), text.into_bytes())],
                change_id(100 + e),
                "edit",
            )
            .await
            .expect("edit");
    }
    c.land("main", &head).await.expect("land base");
    let sparse: Vec<ObjectId> = sparse_files
        .iter()
        .map(|(_, d)| bee_object::blob_id(d))
        .collect();

    let commits = [size(2, 20), size(20, 200), size(200, 2_000)];
    let files = [size(50, 1_000), size(500, 10_000), size(5_000, 100_000)];
    let mut stages = Vec::new();
    for (stage, (h, f)) in commits.iter().zip(files).enumerate() {
        grow(c, stage, *h, f).await;
        let m = measure_all(&s, stage, &sparse).await;
        stages.push(m);
    }
    // Determinism: measuring again at the last size gives the same work.
    let again = measure_all(&s, 9, &sparse).await;

    let mut report = String::new();
    for op in stages[0].keys() {
        report.push_str(&format!("{op}:\n"));
        for (i, st) in stages.iter().enumerate() {
            report.push_str(&format!(
                "  {}× ({} changesets, {} files): {:?}\n",
                10usize.pow(i as u32),
                commits[i],
                files[i],
                st[op]
            ));
        }
    }
    eprintln!("{report}");
    // Every operation: no linear term. Over a 100× growth of history and files the work may
    // at most double (plus 2); a cost proportional to history or size would grow 10× / 100×.
    for op in stages[0].keys() {
        let base = stages[0][op];
        for (i, st) in stages.iter().enumerate().skip(1) {
            for ((field, b), (_, v)) in base.fields().into_iter().zip(st[op].fields()) {
                assert!(
                    v <= 2 * b + 2,
                    "{op}: {field} grew from {b} at 1× to {v} at {}×\n{report}",
                    10usize.pow(i as u32)
                );
            }
        }
    }
    // Operations whose work is exactly constant: identical counters at 1×, 10× and 100×
    // (and on a repeat at the largest size).
    for op in ["land_uncontended", "log_path_page", "sparse_fetch_50"] {
        for st in stages.iter().chain([&again]) {
            assert_eq!(st[op], stages[0][op], "{op} is constant\n{report}");
        }
    }
    // A log page reads its metas in one batch, and two commits with identical metas share
    // one row: a constant number of transactions, and never more rows than at 1×.
    for st in stages.iter().chain([&again]) {
        let (base, now) = (stages[0]["log_page_10"], st["log_page_10"]);
        assert_eq!(
            now.transactions, base.transactions,
            "log_page_10 transactions are constant\n{report}"
        );
        for ((field, b), (_, v)) in base.fields().into_iter().zip(now.fields()) {
            assert!(v <= b, "log_page_10: {field} {v} above {b} at 1×\n{report}");
        }
    }
    // Blame and classified diff derive their rows once; afterwards they are constant.
    for op in ["blame_fixed_file", "diff_classified"] {
        assert_eq!(stages[2][op], stages[1][op], "{op} is constant\n{report}");
        assert_eq!(again[op], stages[1][op], "{op} is constant\n{report}");
    }
    // SPEC §14.6: a rebase over an unrelated tip reads zero blobs; a one-file commit reads
    // only its own new blob (push protection scans added files) and depth-bounded trees.
    for st in stages.iter().chain([&again]) {
        assert_eq!(st["land_rebase_unrelated"].blob_fetches, 0, "{report}");
        assert_eq!(st["land_rebase_unrelated"].blob_gets, 0, "{report}");
        assert_eq!(st["create_one_file"].blob_gets, 1, "{report}");
        assert!(st["create_one_file"].tree_fetches <= 3, "{report}");
        assert_eq!(
            st["land_uncontended"].writes,
            stages[0]["land_uncontended"].writes
        );
        // The sparse fetch reads exactly its 50 blobs, whatever the repo size.
        assert_eq!(
            st["sparse_fetch_50"].blob_gets,
            sparse.len() as u64,
            "{report}"
        );
    }
}

/// SPEC §7.4, §16 hot ref: a coalesced batch writes its rebased stacks in one transaction
/// before its one ref CAS. The W14 load run found every rebased stack committing twice (its
/// trees, then its changeset row), which bounded SQLite at 73 lands/s; durable commits per
/// batch are now constant, whatever the number of stacks in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_coalesced_batch_commits_once_not_twice_per_stack() {
    use std::sync::atomic::Ordering;
    let (s, _dir) = start_fresh(
        |c| {
            c.forge.enabled = false;
            c.search.enabled = false;
            c.ops.metrics_sample_ms = 3_600_000;
            c.land.warm_blame = false;
            c.land.warm_git = false;
            c.land.warm_pack = false;
            c.server.share_history_rows = false;
        },
        None,
    )
    .await;
    let c = &s.client;
    let (tip, v0) = main_tip(c).await;
    let landers = 40;
    let mut heads = Vec::with_capacity(landers);
    for i in 0..landers {
        heads.push(
            c.commit(
                &tip,
                &[(format!("batch/{i}.txt"), format!("{i}\n").into_bytes())],
                change_id(9_000_000 + i as u64),
                "batched",
            )
            .await
            .expect("commit"),
        );
    }
    let counters = s
        .srv
        .state
        .meta
        .access_counters()
        .expect("the metadata store counts accesses");
    let metrics = &s.srv.state.metrics;
    let m0 = counters.snapshot();
    let (b0, r0) = (
        metrics.land_batches.load(Ordering::Relaxed),
        metrics.land_cas_retries.load(Ordering::Relaxed),
    );
    let mut tasks = Vec::with_capacity(landers);
    for h in heads {
        let c = c.clone();
        tasks.push(tokio::spawn(async move { c.land("main", &h).await }));
    }
    for t in tasks {
        t.await.expect("land task").expect("land");
    }
    let commits = counters.snapshot().since(&m0).write_transactions;
    let batches = metrics.land_batches.load(Ordering::Relaxed) - b0;
    let retries = metrics.land_cas_retries.load(Ordering::Relaxed) - r0;
    assert_eq!(
        main_tip(c).await.1,
        v0 + landers as u64,
        "every stack landed"
    );
    eprintln!(
        "{landers} stacks: {batches} batches, {retries} CAS retries, {commits} write transactions"
    );
    // One flush and one CAS per attempt, plus a little slack for incidental writes.
    assert!(
        commits <= 2 * (batches + retries) + 4,
        "{commits} write transactions for {landers} stacks in {batches} batches \
         ({retries} retries): more than two per batch"
    );
}

/// SPEC §16 CreateChangeset (p50 ≤ 10 ms, p99 ≤ 50 ms independent of repo size), counted.
/// The load run of 2026-10-20 measured 10.8 ms at 400 files and 20.0 ms at 11,400 on
/// SQLite: building a changeset read and decoded its parent's whole row to learn the parent's
/// root, so a commit on top of a 10,000-file import decoded 10,000 delta entries; and the
/// meta and trees were committed apart from the changeset row (two durable commits).
///
/// Two repos with the same history shape (the bulk changeset landed, then a one-file land),
/// one of 400 files and one of 11,400. The same one-file change at depth 2 is created on the
/// bulk changeset itself (399 or 11,000 delta entries: an agent committing right after an
/// import) and on the one-file land. Like for like, the counts are identical at both sizes:
/// rows read and written, transactions, tree and blob fetches; one durable commit; and only
/// a 1 KiB prefix of the parent's row is read (the 11,000-entry row is about 900 KB).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_changeset_work_is_independent_of_repo_and_parent_size() {
    /// Counted work, durable commits and changeset bytes read of one CreateChangeset.
    type Spent = (Work, u64, u64);

    async fn repo_of(files: usize) -> (Spent, Spent) {
        let (s, _dir) = start_fresh(
            |c| {
                c.forge.enabled = false;
                c.search.enabled = false;
                c.ops.metrics_sample_ms = 3_600_000;
                c.land.warm_blame = false;
                c.land.warm_git = false;
                c.land.warm_pack = false;
                c.server.share_history_rows = false;
            },
            None,
        )
        .await;
        let c = &s.client;
        let counters = s
            .srv
            .state
            .meta
            .access_counters()
            .expect("the metadata store counts accesses");
        let k = Counted { s: &s };

        // The bulk changeset: `files` files over two directory levels, landed.
        let (tip, _) = main_tip(c).await;
        let blobs: Vec<Vec<u8>> = (0..files)
            .map(|i| format!("bulk {i}\n").into_bytes())
            .collect();
        for chunk in blobs.chunks(2_000) {
            c.upload(chunk).await.expect("upload bulk");
        }
        let puts: Vec<(String, ObjectId, u64)> = blobs
            .iter()
            .enumerate()
            .map(|(i, b)| {
                (
                    format!("bulk/d{:02}/e{:02}/f{i:06}.txt", i % 100, (i / 100) % 100),
                    bee_object::blob_id(b),
                    b.len() as u64,
                )
            })
            .collect();
        let head = c
            .create(&tip, &puts, change_id(60_000_000), "bulk")
            .await
            .expect("bulk changeset");
        let bulk = c.land("main", &head).await.expect("land bulk").target;
        // A one-file land on top of it.
        let head = c
            .commit(
                &bulk,
                &[("u.txt".into(), b"u\n".to_vec())],
                change_id(60_000_001),
                "u",
            )
            .await
            .expect("one-file change");
        let one_file = c.land("main", &head).await.expect("land").target;

        // The measured change: one new file at depth 2, same size and a meta of its own.
        let create = async |n: u64, parent: &ObjectId| -> Spent {
            let data = format!("hot {n}\n").into_bytes();
            c.upload(std::slice::from_ref(&data)).await.expect("upload");
            let put = vec![(
                "hot/file.txt".to_string(),
                bee_object::blob_id(&data),
                data.len() as u64,
            )];
            let m0 = counters.snapshot();
            let (head, w) = k
                .measure(c.create(parent, &put, change_id(50_000_000 + n), &format!("hot {n}")))
                .await;
            head.expect("create");
            let d = counters.snapshot().since(&m0);
            (w, d.write_transactions, d.changeset_bytes_read)
        };
        (create(1, &bulk).await, create(2, &one_file).await)
    }

    let (small_bulk, small_one) = repo_of(399).await;
    let (big_bulk, big_one) = repo_of(11_399).await;
    let report = format!(
        "(work, durable commits, changeset bytes read)\n\
         400 files, on the 399-entry bulk changeset: {small_bulk:?}\n\
         11,400 files, on the 11,399-entry bulk changeset: {big_bulk:?}\n\
         400 files, on a one-file land: {small_one:?}\n\
         11,400 files, on a one-file land: {big_one:?}"
    );
    eprintln!("{report}");
    // Like for like, identical work at 400 and 11,400 files.
    assert_eq!(big_bulk.0, small_bulk.0, "{report}");
    assert_eq!(big_one.0, small_one.0, "{report}");
    for (w, commits, bytes) in [small_bulk, big_bulk, small_one, big_one] {
        // The meta, the new trees and the changeset row commit together; the budget
        // charge and the audit entry are not in this store.
        assert_eq!(commits, 1, "{report}");
        // At most the 1 KiB prefix of the parent's row plus the new one-file changeset.
        assert!(bytes <= 1_024 + 512, "{report}");
        // SPEC §14.6: the new blob is the only blob read; trees only along the path.
        assert_eq!(w.blob_gets, 1, "{report}");
        assert!(w.tree_fetches <= 3, "{report}");
    }
}
