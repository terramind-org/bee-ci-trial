//! The bench/db hot-ref pattern through `PgMetaStore` (SPEC §7.4 land coalescer): many
//! concurrent landers on one ref, every land routed to one owner task that commits whatever
//! is queued as one multi-step `update_ref` (K versions, K ref-log rows, K landed rows).
//!
//! Ignored by default; run against a Postgres with
//! `BEE_TEST_PG=… cargo test -p bee-store-sql --test bench_hot_ref -- --ignored --nocapture`
//! (`BEE_BENCH_LANDERS`, `BEE_BENCH_SECS`, `BEE_BENCH_BATCH` override the defaults).

use std::time::{Duration, Instant};

use bee_core::{ChangeId, ObjectId};
use bee_store::{Expected, LandedEntry, MetaStore, RefStep, RefUpdate};
use bee_store_sql::PgMetaStore;
use bee_testkit::fixtures::{Change, ref_name};
use tokio::sync::{mpsc, oneshot};

fn env_num(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Land {
    change_id: ChangeId,
    done: oneshot::Sender<u64>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; needs BEE_TEST_PG"]
async fn coalesced_hot_ref_lands_per_second() {
    let landers = env_num("BEE_BENCH_LANDERS", 1000);
    let secs = env_num("BEE_BENCH_SECS", 10) as u64;
    let max_batch = env_num("BEE_BENCH_BATCH", 1000);
    let env = env_or_skip!(PgMetaStore);
    let r = env.create_repo("acme", "hot").await.unwrap();
    let main = ref_name("main");
    let target: ObjectId = env
        .commit_on(&r, &main, &[Change::put("f", "x")])
        .await
        .unwrap();
    let meta = env.meta.clone();
    let (tx, mut rx) = mpsc::channel::<Land>(landers * 2);

    // The owner: drains the queue and publishes each batch with one CAS.
    let owner = tokio::spawn(async move {
        let mut version = 1u64;
        let mut commits = 0u64;
        let mut batch = Vec::with_capacity(max_batch);
        while rx.recv_many(&mut batch, max_batch).await > 0 {
            let steps: Vec<RefStep> = batch
                .iter()
                .map(|l| RefStep {
                    target: Some(target),
                    landed: vec![LandedEntry {
                        change_id: l.change_id,
                        source: target,
                        result: target,
                    }],
                })
                .collect();
            let up = meta
                .update_ref(RefUpdate {
                    repo: r.repo,
                    name: ref_name("main"),
                    expected: Expected::Version(version),
                    steps,
                    principal: "coalescer".into(),
                    at_ms: 1,
                    events: vec![],
                })
                .await
                .expect("coalesced land");
            commits += 1;
            for (i, l) in batch.drain(..).enumerate() {
                let _ = l.done.send(version + 1 + i as u64);
            }
            version = up.new_version;
        }
        (version, commits)
    });

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut tasks = Vec::new();
    for w in 0..landers {
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            let mut n = 0u64;
            let mut latencies = Vec::new();
            while Instant::now() < deadline {
                let mut cid = [0u8; 16];
                cid[..8].copy_from_slice(&(w as u64).to_be_bytes());
                cid[8..].copy_from_slice(&n.to_be_bytes());
                let (done, wait) = oneshot::channel();
                let t0 = Instant::now();
                if tx
                    .send(Land {
                        change_id: ChangeId::new(cid),
                        done,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                if wait.await.is_err() {
                    break;
                }
                latencies.push(t0.elapsed());
                n += 1;
            }
            latencies
        }));
    }
    drop(tx);
    let started = Instant::now();
    let mut all = Vec::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    let elapsed = started.elapsed().as_secs_f64();
    let (final_version, commits) = owner.await.unwrap();
    all.sort_unstable();
    let lands = all.len();
    let p99 = all
        .get(lands.saturating_sub(1).min(lands * 99 / 100))
        .copied()
        .unwrap_or_default();
    println!(
        "hot-ref coalesced: {lands} lands in {elapsed:.1}s = {:.0} lands/s; {landers} landers, \
         {commits} commits (mean batch {:.0}), p99 {:.1} ms",
        lands as f64 / elapsed,
        lands as f64 / commits.max(1) as f64,
        p99.as_secs_f64() * 1000.0
    );
    // Every land is one version and one ref-log row; nothing lost.
    assert_eq!(final_version, 1 + lands as u64);
    let rf = env.meta.get_ref(r.repo, &main).await.unwrap().unwrap();
    assert_eq!(rf.version, final_version);
    assert_eq!(
        env.meta
            .list_landed(r.repo, Some(&main))
            .await
            .unwrap()
            .len(),
        lands
    );
}
