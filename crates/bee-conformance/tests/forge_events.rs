//! The forge event log under a land burst on a large tree (SPEC §15.1, §16 event delivery).
//!
//! The W14 load run found the forge log stuck for 17 minutes after a 10,000-file land
//! followed by 1,000 concurrent lands: the VCS→forge bridge appended events one at a time
//! and ran every listener inline, and the dependency-graph listener listed the whole tree on
//! each `main` move. This test repeats that shape at gate size and requires the log to hold
//! the burst's last land within a bound.

use crate::scale_support;

use std::time::{Duration, Instant};

use bee_conformance::load::client::{Client, change_id, ref_updated};
use bee_core::ObjectId;
use scale_support::*;

/// The newest `main` version in the forge log (paging from `from`), and where to continue.
async fn newest_main(c: &Client, mut from: u64, newest: &mut u64) -> u64 {
    loop {
        let (events, next) = c.list_events(&["ref.updated"], from).await.expect("events");
        for e in &events {
            if let Some((name, _, v)) = ref_updated(e)
                && name == "main"
            {
                *newest = (*newest).max(v);
            }
        }
        if events.is_empty() || next <= from {
            return from;
        }
        from = next;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn forge_event_log_catches_up_after_a_land_burst_on_a_large_tree() {
    let (s, _dir) = start_fresh(|_| {}, None).await;
    let c = &s.client;
    let (tip, _) = main_tip(c).await;

    // A large tree with a manifest, so the dependency-graph listener has a graph to keep.
    let files = size(3_000, 10_000);
    let mut bulk: Vec<(String, Vec<u8>)> = (0..files)
        .map(|i| {
            (
                format!("bulk/d{:02}/f{i:06}.txt", i % 50),
                format!("bulk file {i}\n").into_bytes(),
            )
        })
        .collect();
    bulk.push((
        "Cargo.toml".into(),
        b"[package]\nname = \"burst\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1.0.100\"\n"
            .to_vec(),
    ));
    for chunk in bulk.chunks(2_000) {
        let blobs: Vec<Vec<u8>> = chunk.iter().map(|f| f.1.clone()).collect();
        c.upload(&blobs).await.expect("upload");
    }
    let puts: Vec<(String, ObjectId, u64)> = bulk
        .iter()
        .map(|(p, d)| (p.clone(), bee_object::blob_id(d), d.len() as u64))
        .collect();
    let grown = c
        .create(&tip, &puts, change_id(7_000_000), "large tree")
        .await
        .expect("create");
    let tip = c
        .land("main", &grown)
        .await
        .expect("land large tree")
        .target;

    // The burst: concurrent landers, each with its own one-file change on the same tip.
    let landers = size(200, 1_000);
    let mut heads = Vec::with_capacity(landers);
    for i in 0..landers {
        heads.push(
            c.commit(
                &tip,
                &[(format!("burst/{i}.txt"), format!("{i}\n").into_bytes())],
                change_id(7_000_001 + i as u64),
                "burst",
            )
            .await
            .expect("commit"),
        );
    }
    let mut tasks = Vec::with_capacity(landers);
    for h in heads {
        let c = c.clone();
        tasks.push(tokio::spawn(async move { c.land("main", &h).await }));
    }
    let mut last = 0;
    for t in tasks {
        last = last.max(t.await.expect("land task").expect("land").version);
    }

    // The forge log holds the burst's last land within the bound.
    let bound = Duration::from_secs(if full() { 60 } else { 10 });
    let t0 = Instant::now();
    let (mut newest, mut from) = (0, 0);
    while newest < last {
        assert!(
            t0.elapsed() < bound,
            "forge log held main up to version {newest} of {last} after {:?}",
            t0.elapsed()
        );
        from = newest_main(c, from, &mut newest).await;
        if newest < last {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    eprintln!(
        "forge log caught up to version {last} ({landers} landers, {files} files) in {:?}",
        t0.elapsed()
    );

    // And it keeps flowing: one more land reaches the log promptly.
    let head = c
        .commit(
            &tip,
            &[("burst/after.txt".into(), b"after\n".to_vec())],
            change_id(7_999_999),
            "after",
        )
        .await
        .expect("commit");
    let after = c.land("main", &head).await.expect("land after").version;
    let t1 = Instant::now();
    while newest < after {
        assert!(
            t1.elapsed() < Duration::from_secs(5),
            "a land after the burst did not reach the forge log"
        );
        from = newest_main(c, from, &mut newest).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    s.srv.shutdown().await;
}
