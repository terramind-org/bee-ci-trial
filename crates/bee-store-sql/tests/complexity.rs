//! Complexity gates via the SQL stores' access counters (SPEC §14.6): a one-file commit reads
//! depth trees and no blobs, and its store traffic does not grow with repository size.

use crate::common;
use bee_store::{AccessSnapshot, FetchKind, MetaStore, Table};
use bee_testkit::fixtures::{Change, ref_name};
use common::{Env, Open};

async fn one_file_commit<S: Open>(n: usize) -> Option<AccessSnapshot> {
    let env = Env::<S>::new().await?;
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
    let d = env.counters.snapshot().since(&before);
    assert!(env.meta.access_counters().is_some());
    Some(d)
}

async fn one_file_commit_reads_depth_trees_at_every_size<S: Open>() {
    let Some(small) = one_file_commit::<S>(1_000).await else {
        return;
    };
    let large = one_file_commit::<S>(10_000).await.unwrap();
    let depth = 3;
    assert_eq!(small.fetches_of(FetchKind::Tree), depth);
    assert_eq!(large.fetches_of(FetchKind::Tree), depth);
    assert_eq!(large.fetches_of(FetchKind::Blob), 0);
    assert_eq!(large.blob.gets, 0);
    assert_eq!(small.transactions, large.transactions);
    assert_eq!(small.total_reads(), large.total_reads());
    assert_eq!(small.total_writes(), large.total_writes());
    assert_eq!(large.writes_of(Table::Changesets), 1);
    assert_eq!(large.writes_of(Table::Objects), depth + 2);
}
both!(one_file_commit_reads_depth_trees_at_every_size);
