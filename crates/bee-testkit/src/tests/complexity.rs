//! Complexity gates via storage access counters (SPEC §14.6): a one-file commit reads
//! depth trees and no blobs, and its store traffic does not grow with repository size.

use bee_store::{AccessSnapshot, FetchKind, Table};

use crate::fixtures::{Change, TestEnv, ref_name};

/// Builds a repo of `n` files at `dXX/eYY/fZZZZ.txt` and measures one one-file commit.
async fn one_file_commit(n: usize) -> AccessSnapshot {
    let env = TestEnv::new();
    let r = env.create_repo("acme", "big").await.unwrap();
    let main = ref_name("main");
    let changes: Vec<Change> = (0..n)
        .map(|i| {
            Change::put(
                &format!("d{:02}/e{:02}/f{:05}.txt", i % 10, (i / 10) % 10, i),
                format!("{i}\n"),
            )
        })
        .collect();
    let tip = env.commit_on(&r, &main, &changes).await.unwrap();
    env.clock.advance_ms(1000);
    let before = env.counters.snapshot();
    env.commit(
        &r,
        Some(tip),
        &[Change::put("d03/e04/f00043.txt", "changed\n")],
    )
    .await
    .unwrap();
    env.counters.snapshot().since(&before)
}

#[tokio::test]
async fn one_file_commit_reads_depth_trees_at_every_size() {
    let small = one_file_commit(1_000).await;
    let large = one_file_commit(10_000).await;
    // Depth of the changed path: root, d03, d03/e04.
    let depth = 3;
    assert_eq!(small.fetches_of(FetchKind::Tree), depth);
    assert_eq!(large.fetches_of(FetchKind::Tree), depth);
    assert_eq!(large.fetches_of(FetchKind::Blob), 0);
    assert_eq!(large.blob.gets, 0);
    // Everything the store does is independent of repository size.
    assert_eq!(small.transactions, large.transactions);
    assert_eq!(small.total_reads(), large.total_reads());
    assert_eq!(small.total_writes(), large.total_writes());
    assert_eq!(large.writes_of(Table::Changesets), 1);
    // One new tree per level, one blob, one meta.
    assert_eq!(large.writes_of(Table::Objects), depth + 2);
}
