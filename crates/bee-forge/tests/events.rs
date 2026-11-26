//! Event log: ordering, block allocation, dedupe, org mirroring, subscribe with resume,
//! retention, the outbox relay and the VCS bridge.

use crate::common;
use std::sync::Arc;
use std::time::Duration;

use bee_core::Ulid;
use bee_forge::Cx;
use bee_forge::events::bridge::VcsBridge;
use bee_forge::events::{EventLog, org_log};
use bee_forge::ids::{IdGen, ManualClock};
use bee_forge::module::Registry;
use bee_forge::store::Db;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_proto::v1::{ErrorReason, Event, Ref, event};
use bee_store::{MemoryMetaStore, MetaStore, NewEvent, OrgId, OrgRow, RepoId, RepoRow};
use prost::Message;

const R: &str = "01HZZZZZZZZZZZZZZZZZZZZZZR";
const O: &str = "01HZZZZZZZZZZZZZZZZZZZZZZG";

async fn db_with(clock: Arc<ManualClock>) -> Db {
    let store = Arc::new(MemoryStore::new());
    store
        .migrate(&Registry::core().unwrap().migrations())
        .await
        .unwrap();
    Db::new(store, Arc::new(IdGen::new(clock)))
}

fn ev(kind: &str) -> Event {
    Event {
        kind: kind.into(),
        repo_id: R.into(),
        org_id: O.into(),
        principal: "p".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn sequences_increase_across_blocks_and_restarts() {
    let clock = Arc::new(ManualClock::new(1_000_000));
    let db = db_with(clock.clone()).await;
    let cx = Cx::system();
    let log = EventLog::new(db.clone()).with_block_size(4);
    let mut seqs = Vec::new();
    for i in 0..6 {
        let out = log
            .append(&cx, ev("lease.acquired"), &format!("k{i}"))
            .await
            .unwrap();
        seqs.push(out.iter().find(|(k, _)| k == R).unwrap().1);
    }
    assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6]);
    // A new process gets a fresh block: numbers keep increasing, leaving a gap.
    let log2 = EventLog::new(db.clone()).with_block_size(4);
    let out = log2.append(&cx, ev("lease.acquired"), "k6").await.unwrap();
    let seq = out.iter().find(|(k, _)| k == R).unwrap().1;
    assert_eq!(seq, 9);
    let (events, next) = log2.read(&cx, R, 0, &[], 100).await.unwrap();
    let got: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(got, vec![1, 2, 3, 4, 5, 6, 9]);
    assert_eq!(next, 10);
}

#[tokio::test]
async fn appends_are_idempotent_and_mirrored_to_the_org_log() {
    let db = db_with(Arc::new(ManualClock::new(1_000_000))).await;
    let cx = Cx::system();
    let log = EventLog::new(db);
    let a = log.append(&cx, ev("cr.opened"), "same").await.unwrap();
    let b = log.append(&cx, ev("cr.opened"), "same").await.unwrap();
    assert_eq!(a, b);
    let (repo_events, _) = log.read(&cx, R, 0, &[], 10).await.unwrap();
    assert_eq!(repo_events.len(), 1);
    let (org_events, _) = log.read(&cx, &org_log(O), 0, &[], 10).await.unwrap();
    assert_eq!(org_events.len(), 1);
    assert_eq!(org_events[0].kind, "cr.opened");
}

#[tokio::test]
async fn kind_filters_and_paging() {
    let db = db_with(Arc::new(ManualClock::new(1_000_000))).await;
    let cx = Cx::system();
    let log = EventLog::new(db);
    for i in 0..10 {
        let kind = if i % 2 == 0 {
            "cr.opened"
        } else {
            "lease.acquired"
        };
        log.append(&cx, ev(kind), &format!("k{i}")).await.unwrap();
    }
    let (crs, next) = log.read(&cx, R, 0, &["cr.*".into()], 3).await.unwrap();
    assert_eq!(crs.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 3, 5]);
    let (more, _) = log.read(&cx, R, next, &["cr.*".into()], 10).await.unwrap();
    assert_eq!(more.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![7, 9]);
    let info = log.info(&cx, R).await.unwrap();
    assert_eq!(
        (info.oldest_seq, info.latest_seq, info.retention_days),
        (1, 10, 30)
    );
}

#[tokio::test]
async fn subscribers_resume_without_gaps_or_duplicates() {
    let db = db_with(Arc::new(ManualClock::new(1_000_000))).await;
    let cx = Cx::system();
    let log = Arc::new(EventLog::new(db));
    for i in 0..3 {
        log.append(&cx, ev("lease.acquired"), &format!("a{i}"))
            .await
            .unwrap();
    }
    let mut rx = log.subscribe(&cx, R, 1, vec![]).await.unwrap();
    let mut last = 0;
    for _ in 0..3 {
        last = rx.recv().await.unwrap().unwrap().seq;
    }
    assert_eq!(last, 3);
    drop(rx);
    for i in 0..2 {
        log.append(&cx, ev("lease.released"), &format!("b{i}"))
            .await
            .unwrap();
    }
    let mut rx = log.subscribe(&cx, R, last + 1, vec![]).await.unwrap();
    let a = rx.recv().await.unwrap().unwrap();
    let b = rx.recv().await.unwrap().unwrap();
    assert_eq!((a.seq, b.seq), (4, 5));
    // Live follow: an append after subscribing arrives without polling by the client.
    log.append(&cx, ev("cr.opened"), "c0").await.unwrap();
    let c = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("live event")
        .unwrap()
        .unwrap();
    assert_eq!((c.seq, c.kind.as_str()), (6, "cr.opened"));
    // Filtered subscription.
    let mut only = log.subscribe(&cx, R, 1, vec!["cr.*".into()]).await.unwrap();
    assert_eq!(only.recv().await.unwrap().unwrap().seq, 6);
}

/// A delivery backlog must never make a subscription disappear.  This deliberately leaves
/// half the receivers idle while a burst larger than the stream buffer is appended; each idle
/// receiver must subsequently be able to resume from its last cursor without a gap.
#[tokio::test]
async fn slow_subscribers_resume_a_burst_without_gaps_or_duplicates() {
    const SUBSCRIBERS: usize = 32;
    const BURST: usize = 600;

    let db = db_with(Arc::new(ManualClock::new(1_000_000))).await;
    let cx = Cx::system();
    let log = Arc::new(EventLog::new(db));
    let mut receivers = Vec::new();
    for _ in 0..SUBSCRIBERS {
        receivers.push(log.subscribe(&cx, R, 1, vec![]).await.unwrap());
    }
    for i in 0..BURST {
        log.append(&cx, ev("cr.opened"), &format!("burst-{i}"))
            .await
            .unwrap();
    }

    for mut rx in receivers {
        let mut seen = Vec::new();
        // Drain the bounded live queue, then resume from the durable cursor.  The sequence
        // assertion makes this a counted gate: no timeout or scheduler timing is involved.
        while let Ok(Ok(event)) = rx.try_recv() {
            seen.push(event.seq);
        }
        let from = seen.last().copied().unwrap_or(0) + 1;
        drop(rx);
        let mut resumed = log.subscribe(&cx, R, from, vec![]).await.unwrap();
        while seen.len() < BURST {
            seen.push(resumed.recv().await.unwrap().unwrap().seq);
        }
        assert_eq!(seen, (1..=BURST as u64).collect::<Vec<_>>());
    }
}

/// Live delivery reads durable storage once per log, not once per subscribed stream.
///
/// The two cases deliberately use different subscriber counts.  Each subscription has already
/// taken its initial snapshot before the counter is reset, so this measures only the read that
/// makes the subsequently appended event live to every subscriber.
#[tokio::test]
async fn shared_tail_durable_reads_do_not_grow_with_subscribers() {
    async fn delivery_reads(subscribers: usize) -> usize {
        let db = db_with(Arc::new(ManualClock::new(1_000_000))).await;
        let cx = Cx::system();
        let log = Arc::new(EventLog::new(db));
        let mut receivers = Vec::new();
        for _ in 0..subscribers {
            receivers.push(log.subscribe(&cx, R, 1, vec![]).await.unwrap());
        }

        // The shared reader has performed its startup scan before this counter begins.
        while log.tail_durable_reads(R).unwrap() == 0 {
            tokio::task::yield_now().await;
        }
        log.reset_tail_durable_reads(R).unwrap();
        log.append(&cx, ev("cr.opened"), "one-live-event")
            .await
            .unwrap();
        for rx in &mut receivers {
            assert_eq!(rx.recv().await.unwrap().unwrap().seq, 1);
        }
        log.tail_durable_reads(R).unwrap()
    }

    let reads_for_four = delivery_reads(4).await;
    let reads_for_sixty_four = delivery_reads(64).await;
    assert_eq!(reads_for_four, 1);
    assert_eq!(reads_for_sixty_four, reads_for_four);
}

#[tokio::test]
async fn retention_removes_old_events_and_refuses_expired_resume() {
    let clock = Arc::new(ManualClock::new(1_000_000_000));
    let db = db_with(clock.clone()).await;
    let cx = Cx::system();
    let log = EventLog::new(db).with_retention_days(30);
    log.append(&cx, ev("lease.acquired"), "old").await.unwrap();
    clock.advance(31 * 86_400_000);
    log.append(&cx, ev("lease.acquired"), "new").await.unwrap();
    let removed = log.prune(&cx).await.unwrap();
    assert!(removed >= 1);
    let err = log.read(&cx, R, 1, &[], 10).await.unwrap_err();
    assert_eq!(err.reason, ErrorReason::InvalidArgument);
    assert_eq!(err.subcode, "seq_not_retained");
    let (events, _) = log.read(&cx, R, 0, &[], 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_workers_relay_without_explicit_flushes() {
    let env = common::env().await;
    let workers = env.forge.spawn_workers();
    let cx = Cx::new("owner", bee_proto::v1::PrincipalKind::Human);
    let p = env.forge.hooks.directory.get("owner").await.unwrap();
    let org = env.forge.orgs.create_org(&cx, "acme", "", p).await.unwrap();
    let mut rx = env
        .forge
        .events
        .subscribe(&cx, &org_log(&org.id), 0, vec!["org.*".into()])
        .await
        .unwrap();
    let e = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("relayed")
        .unwrap()
        .unwrap();
    assert_eq!(e.kind, "org.created");
    workers.stop().await;
}

#[tokio::test]
async fn vcs_events_are_bridged_once() {
    let db = db_with(Arc::new(ManualClock::new(1_000_000))).await;
    let cx = Cx::system();
    let log = Arc::new(EventLog::new(db.clone()));
    let meta = Arc::new(MemoryMetaStore::new());
    let org = OrgId(Ulid::parse(O).unwrap());
    let repo = RepoId(Ulid::parse(R).unwrap());
    meta.create_org(OrgRow {
        id: org,
        name: "o".into(),
        created_at_ms: 1,
    })
    .await
    .unwrap();
    meta.create_repo(RepoRow {
        id: repo,
        org,
        name: "r".into(),
        created_at_ms: 1,
        draft_retention_days: 30,
    })
    .await
    .unwrap();
    let payload = Event {
        kind: "ref.updated".into(),
        body: Some(event::Body::RefUpdated(Ref {
            name: "main".into(),
            target: None,
            version: 2,
        })),
        ..Default::default()
    }
    .encode_to_vec();
    meta.append_events(
        repo,
        vec![NewEvent {
            kind: "ref.updated".into(),
            payload,
            at_ms: 5,
        }],
    )
    .await
    .unwrap();
    let bridge = VcsBridge::new(meta.clone(), log.clone(), db);
    assert_eq!(bridge.sync_repo(&cx, R, O).await.unwrap(), 1);
    assert_eq!(bridge.sync_repo(&cx, R, O).await.unwrap(), 0);
    let (events, _) = log.read(&cx, R, 0, &[], 10).await.unwrap();
    assert_eq!(events.len(), 1);
    match &events[0].body {
        Some(event::Body::RefUpdated(r)) => assert_eq!(r.version, 2),
        other => panic!("unexpected body {other:?}"),
    }
}

/// Counts events and takes `delay` over each.
struct SlowListener {
    seen: std::sync::atomic::AtomicUsize,
    delay: Duration,
}

#[async_trait::async_trait]
impl bee_forge::events::EventListener for SlowListener {
    async fn on_event(&self, _: &Event) {
        tokio::time::sleep(self.delay).await;
        self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A VCS store holding repo `R` of org `O` with `events` appended.
async fn vcs_with(events: Vec<NewEvent>) -> Arc<MemoryMetaStore> {
    let meta = Arc::new(MemoryMetaStore::new());
    let org = OrgId(Ulid::parse(O).unwrap());
    let repo = RepoId(Ulid::parse(R).unwrap());
    meta.create_org(OrgRow {
        id: org,
        name: "o".into(),
        created_at_ms: 1,
    })
    .await
    .unwrap();
    meta.create_repo(RepoRow {
        id: repo,
        org,
        name: "r".into(),
        created_at_ms: 1,
        draft_retention_days: 30,
    })
    .await
    .unwrap();
    meta.append_events(repo, events).await.unwrap();
    meta
}

/// Every event of log `R`, in sequence order.
async fn all_events(log: &EventLog) -> Vec<Event> {
    let cx = Cx::system();
    let mut out = Vec::new();
    let mut from = 0;
    loop {
        let (page, next) = log.read(&cx, R, from, &[], 1000).await.unwrap();
        if page.is_empty() {
            return out;
        }
        out.extend(page);
        from = next;
    }
}

/// The W14 load run's forge-log stall: the bridge appended VCS events one at a time and ran
/// every listener inline, so one slow listener held the log back by its delay per event.
/// Now a bridge batch commits in one transaction and listeners catch up on their own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_listener_does_not_hold_the_bridged_log_back() {
    let db = db_with(Arc::new(ManualClock::new(1_000_000))).await;
    let cx = Cx::system();
    let log = Arc::new(EventLog::new(db.clone()));
    let slow = Arc::new(SlowListener {
        seen: Default::default(),
        delay: Duration::from_millis(100),
    });
    log.add_listener(slow.clone());
    let n = 600u64;
    let meta = vcs_with(
        (1..=n)
            .map(|v| NewEvent {
                kind: "ref.updated".into(),
                payload: Event {
                    kind: "ref.updated".into(),
                    body: Some(event::Body::RefUpdated(Ref {
                        name: "main".into(),
                        target: None,
                        version: v,
                    })),
                    ..Default::default()
                }
                .encode_to_vec(),
                at_ms: v,
            })
            .collect(),
    )
    .await;
    let bridge = VcsBridge::new(meta, log.clone(), db);

    // 600 events at 100 ms each would take a minute inline; bridged, two batches commit at
    // once and the log holds every event.
    let t0 = std::time::Instant::now();
    let mut bridged = 0;
    loop {
        let k = bridge.sync_repo(&cx, R, O).await.unwrap();
        if k == 0 {
            break;
        }
        bridged += k;
    }
    assert_eq!(bridged, n as usize);
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "bridging waited on the listener: {:?}",
        t0.elapsed()
    );
    let versions: Vec<u64> = all_events(&log)
        .await
        .iter()
        .filter_map(|e| match &e.body {
            Some(event::Body::RefUpdated(r)) => Some(r.version),
            _ => None,
        })
        .collect();
    assert_eq!(
        versions,
        (1..=n).collect::<Vec<_>>(),
        "every event, in order"
    );
    // The listener is still behind; it catches up on its own task.
    assert!(slow.seen.load(std::sync::atomic::Ordering::SeqCst) < n as usize);
    assert_eq!(
        bridge.sync_repo(&cx, R, O).await.unwrap(),
        0,
        "cursor at the end"
    );
}

/// A listener of batched appends sees every event once, in order, and `settle` waits for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_listeners_see_every_batched_event_in_order() {
    let db = db_with(Arc::new(ManualClock::new(1_000_000))).await;
    let cx = Cx::system();
    let log = Arc::new(EventLog::new(db));
    let slow = Arc::new(SlowListener {
        seen: Default::default(),
        delay: Duration::from_millis(1),
    });
    log.add_listener(slow.clone());
    let batch: Vec<(Event, String)> = (0..50)
        .map(|i| (ev("lease.acquired"), format!("b{i}")))
        .collect();
    let seqs = log.append_batch(&cx, batch).await.unwrap();
    assert_eq!(seqs.len(), 50);
    log.settle().await;
    assert_eq!(slow.seen.load(std::sync::atomic::Ordering::SeqCst), 50);
    // Appending the same dedupe keys again adds nothing to the log.
    let again: Vec<(Event, String)> = (0..50)
        .map(|i| (ev("lease.acquired"), format!("b{i}")))
        .collect();
    assert_eq!(log.append_batch(&cx, again).await.unwrap(), seqs);
    assert_eq!(all_events(&log).await.len(), 50);
}

/// The bridge's cursor and its events commit together, one sync per repo at a time:
/// concurrent syncs never skip or double-count events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_bridge_syncs_bridge_every_event_once() {
    let db = db_with(Arc::new(ManualClock::new(1_000_000))).await;
    let cx = Cx::system();
    let log = Arc::new(EventLog::new(db.clone()));
    let n = 1_200u64;
    let meta = vcs_with(
        (1..=n)
            .map(|v| NewEvent {
                kind: "policy.changed".into(),
                payload: Vec::new(),
                at_ms: v,
            })
            .collect(),
    )
    .await;
    let bridge = Arc::new(VcsBridge::new(meta, log.clone(), db));
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let (bridge, cx) = (bridge.clone(), cx.clone());
        tasks.push(tokio::spawn(async move {
            let mut total = 0;
            loop {
                let k = bridge.sync_repo(&cx, R, O).await.unwrap();
                if k == 0 {
                    return total;
                }
                total += k;
            }
        }));
    }
    let mut total = 0;
    for t in tasks {
        total += t.await.unwrap();
    }
    assert_eq!(total, n as usize, "each event bridged by exactly one sync");
    assert_eq!(all_events(&log).await.len(), n as usize);
}
