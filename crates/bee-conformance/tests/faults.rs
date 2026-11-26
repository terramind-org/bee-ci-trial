//! Fault runs against the real server (SPEC §15.1, §15.3, §7.4; PRODUCT §4.25), each
//! asserting the documented behavior:
//!
//! - the coalescer owner is killed in the middle of a batch: clients retry through another
//!   process and every change lands exactly once (retries of committed lands are answered
//!   from the landed index), the ref log has no gaps and a restarted process lands again;
//! - another server takes a crashed owner's port: forwarded lands carry the owner's instance
//!   id and fence, so that server refuses them, the forwarder lands on its own store and takes
//!   the coalescer over;
//! - the metadata store is unavailable for a window: ref reads return the last known version
//!   marked `stale` (within `metadata.stale_read_max_ms`, then `UNAVAILABLE`), every other
//!   call fails with a retryable `UNAVAILABLE` (never a wrong answer), calls carry deadlines,
//!   and after the window retried lands complete exactly once with fsck clean;
//! - the forge database is down while the VCS lands: lands, reads and commits keep working,
//!   forge RPCs answer `UNAVAILABLE`, and the outage's lands reach the forge event log after
//!   recovery;
//! - the code-search index is down: code search answers `UNAVAILABLE` while change request,
//!   lease and check lists and lands keep working.

use crate::scale_support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bee_conformance::load::client::{Client, Landed, RpcError, change_id, ref_updated};
use bee_core::ObjectId;
use bee_forge::store::unavailable::{ALL_SURFACES, SwitchableStore};
use bee_forge::store::{ForgeStore, Surface};
use bee_store::{Fault, FaultInjector, FaultRule};
use scale_support::*;

/// `n` changesets on `parent`, one file each.
async fn heads(c: &Client, parent: &ObjectId, n: usize, tag: u64) -> Vec<ObjectId> {
    let mut out = Vec::new();
    for i in 0..n {
        let file = (
            format!("faults/{tag}/{i}.txt"),
            format!("{tag} {i}\n").into_bytes(),
        );
        out.push(
            c.commit(
                parent,
                &[file],
                change_id(tag * 1_000_000 + i as u64),
                "fault run",
            )
            .await
            .expect("commit"),
        );
    }
    out
}

async fn land_all(c: &Client, heads: &[ObjectId]) -> Vec<Result<Landed, RpcError>> {
    let mut hs = Vec::new();
    for h in heads {
        let (c, h) = (c.clone(), *h);
        hs.push(tokio::spawn(async move { c.land("main", &h).await }));
    }
    let mut out = Vec::new();
    for h in hs {
        out.push(h.await.expect("land task"));
    }
    out
}

/// Retries a land until it answers (retryable failures only), up to `tries`.
async fn land_retrying(c: &Client, head: &ObjectId, tries: usize) -> Landed {
    let mut last = None;
    // Each attempt has a short deadline so a stuck attempt is retried, never waited on.
    let c = c.with_timeout(Duration::from_secs(10));
    for _ in 0..tries {
        match c.land("main", head).await {
            Ok(l) => return l,
            Err(e) => {
                assert!(e.retryable(), "only retryable failures: {e}");
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    panic!("land never answered: {last:?}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn owner_killed_mid_batch_loses_and_duplicates_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = config(dir.path());
    cfg.land.owner_lease_ms = 600;
    let slow = FaultInjector::new();
    let a = start_on(cfg.clone(), Some(slow.clone())).await;
    let b = start_on(cfg.clone(), None).await;
    let (tip, v0) = main_tip(&a.client).await;
    let hs = heads(&a.client, &tip, size(40, 400), 1).await;
    // A publishes slowly, so the kill lands while batches are being written.
    slow.add(FaultRule::always(
        "meta.update_ref",
        Fault::Delay(Duration::from_millis(150)),
    ));
    let ca = a.client.clone();
    let first = {
        let hs = hs.clone();
        tokio::spawn(async move { land_all(&ca, &hs).await })
    };
    tokio::time::sleep(Duration::from_millis(250)).await;
    a.srv.crash().await;
    let first = first.await.expect("first wave");
    let answered = first.iter().filter(|r| r.is_ok()).count();
    eprintln!(
        "owner kill: {answered} of {} lands answered before the crash",
        hs.len()
    );

    // Clients retry every land through B: each lands exactly once.
    let mut versions = BTreeSet::new();
    let mut skipped = 0;
    for h in &hs {
        let l = land_retrying(&b.client, h, 100).await;
        assert_eq!(l.members.len(), 1);
        if l.members[0].skipped {
            skipped += 1;
        }
        assert!(versions.insert(l.version), "one version per change");
    }
    eprintln!("owner kill: {skipped} retries answered from the landed index");
    let last = b.assert_ref_log_continuous("main").await;
    assert_eq!(last, v0 + hs.len() as u64, "every change landed once");

    // A restarted process joins the same store and lands through the new owner.
    let a2 = start_on(cfg.clone(), None).await;
    let (tip, _) = main_tip(&a2.client).await;
    let more = heads(&a2.client, &tip, 5, 2).await;
    for r in land_all(&a2.client, &more).await {
        r.expect("land after restart");
    }
    assert_eq!(b.assert_ref_log_continuous("main").await, last + 5);
    let (clean, v) = b.fsck_clean().await;
    assert!(clean, "fsck: {v:?}");
    a2.srv.shutdown().await;
    b.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_server_on_a_crashed_owners_port_receives_no_forwarded_lands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = config(dir.path());
    // A long lease: the crashed owner's row is still valid while B lands, so B must notice
    // that another process answers at the owner's endpoint instead of waiting for expiry.
    cfg.land.owner_lease_ms = 60_000;
    let a = start_on(cfg.clone(), None).await;
    let b = start_on(cfg.clone(), None).await;
    let repo = a.repo();
    let (tip, v0) = main_tip(&a.client).await;
    let hs = heads(&a.client, &tip, 4, 6).await;
    // A lands first and so owns main's coalescer.
    a.client.land("main", &hs[0]).await.expect("land through A");
    let lease = bee_server::coalescer::read_lease(&a.srv.state, repo, "main")
        .await
        .expect("lease")
        .expect("A holds the lease");
    assert_eq!(lease.owner, a.srv.state.instance_id);
    let addr = a.srv.addr;
    a.srv.crash().await;

    // An unrelated server (its own store and keys) takes the dead owner's port.
    let zdir = tempfile::tempdir().expect("tempdir");
    let mut zcfg = config(zdir.path());
    zcfg.server.listen = addr.to_string();
    let z = start_on(zcfg, None).await;
    let (_, z0) = main_tip(&z.client).await;

    // B's lands are refused there, land on B's store, and B takes the coalescer over.
    for h in &hs[1..] {
        let l = b
            .client
            .with_timeout(Duration::from_secs(20))
            .land("main", h)
            .await
            .expect("land through B");
        assert_eq!(l.members.len(), 1);
    }
    assert_eq!(
        b.assert_ref_log_continuous("main").await,
        v0 + hs.len() as u64
    );
    assert_eq!(
        main_tip(&z.client).await.1,
        z0,
        "the server on the dead owner's port landed nothing"
    );
    let lease = bee_server::coalescer::read_lease(&b.srv.state, repo, "main")
        .await
        .expect("lease")
        .expect("a lease");
    assert_eq!(
        lease.owner, b.srv.state.instance_id,
        "B re-elected itself owner"
    );
    assert!(lease.fence > 1, "the takeover moved the fence");
    z.srv.shutdown().await;
    b.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_unavailable_window_fails_retryably_then_recovers() {
    let faults = FaultInjector::new();
    let (s, _dir) = start_fresh(|_| {}, Some(faults.clone())).await;
    let c = &s.client;
    let (tip, v0) = main_tip(c).await;
    let hs = heads(c, &tip, 10, 3).await;

    // The window: every metadata call fails.
    faults.add(FaultRule::always("meta.*", Fault::unavailable()));
    for r in land_all(c, &hs).await {
        let e = r.expect_err("no land during the outage");
        assert_eq!(e.reason(), "UNAVAILABLE", "{e}");
        assert!(e.retryable());
    }
    // Reads of refs survive (SPEC §15.1): the last known version, marked stale.
    let (target, version, stale) = c
        .resolve_allow_stale("main")
        .await
        .expect("a stale ref read during the outage")
        .expect("main exists");
    assert!(stale, "served while the primary is down");
    assert_eq!((target, version), (tip, v0), "the last known version");
    let e = c
        .resolve("main")
        .await
        .expect_err("a strong read refuses the stale answer");
    assert!(e.retryable(), "{e}");
    let e = c
        .log("main", 10)
        .await
        .expect_err("no log during the outage");
    assert!(e.retryable(), "{e}");
    faults.clear_rules();

    // Every call has a deadline: a store call hanging longer than the client's deadline
    // ends the call at the deadline, not when the store answers.
    faults.add(FaultRule::always(
        "meta.get_ref",
        Fault::Delay(Duration::from_secs(5)),
    ));
    let t0 = Instant::now();
    let e = c
        .with_timeout(Duration::from_millis(500))
        .resolve("main")
        .await
        .expect_err("deadline");
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    assert!(e.retryable(), "{e}");
    faults.clear_rules();

    // Lost responses: the ref CAS commits and then reports failure; clients retry.
    faults.add(FaultRule::nth("meta.update_ref", 0, Fault::response_lost()));
    faults.add(FaultRule::nth("meta.update_ref", 2, Fault::response_lost()));
    let mut versions = BTreeSet::new();
    for h in &hs {
        let l = land_retrying(c, h, 20).await;
        versions.insert(l.version);
    }
    assert_eq!(versions.len(), hs.len());
    assert_eq!(
        s.assert_ref_log_continuous("main").await,
        v0 + hs.len() as u64
    );
    let (clean, v) = s.fsck_clean().await;
    assert!(clean, "fsck: {v:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_ref_reads_end_at_the_staleness_bound() {
    let faults = FaultInjector::new();
    let (s, _dir) = start_fresh(
        |c| c.metadata.stale_read_max_ms = 1_000,
        Some(faults.clone()),
    )
    .await;
    let c = &s.client;
    let (tip, v0) = main_tip(c).await;
    faults.add(FaultRule::always("meta.*", Fault::unavailable()));
    let read = c
        .resolve_allow_stale("main")
        .await
        .expect("stale read")
        .expect("main");
    assert_eq!(read, (tip, v0, true));
    // A version newer than the last known one cannot be answered stale.
    let mut objects =
        bee_proto::v1::object_service_client::ObjectServiceClient::new(c.channel().clone());
    let e = RpcError::from(
        objects
            .resolve_ref(c.req(bee_proto::v1::ResolveRefRequest {
                repo: c.repo(),
                name: "main".into(),
                min_version: v0 + 1,
                ..Default::default()
            }))
            .await
            .expect_err("min_version past the last known version"),
    );
    assert_eq!(e.reason(), "UNAVAILABLE");
    // Past the bound the read fails like every other call.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let e = c
        .resolve_allow_stale("main")
        .await
        .expect_err("older than the staleness bound");
    assert_eq!(e.reason(), "UNAVAILABLE");
    assert!(e.retryable());
    faults.clear_rules();
    assert_eq!(main_tip(c).await, (tip, v0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_vcs_lands_while_the_forge_database_is_down() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config(dir.path());
    let sql = bee_forge::store::sql::SqlStore::open(
        bee_forge::store::sql::SqlConfig::sqlite(dir.path().join("forge")),
        Arc::new(bee_forge::ids::SystemClock),
    )
    .await
    .expect("forge store");
    let switch = Arc::new(SwitchableStore::new(Arc::new(sql)));
    let sw = switch.clone();
    let s = start_opts(cfg, move |o| {
        o.forge_store = Some(sw as Arc<dyn ForgeStore>)
    })
    .await;
    let c = &s.client;
    let (tip, v0) = main_tip(c).await;
    let hs = heads(c, &tip, size(20, 200), 4).await;

    switch.take_down(Surface(ALL_SURFACES));
    let e = c.list_changes().await.expect_err("forge down");
    assert_eq!(e.reason(), "UNAVAILABLE");
    assert!(e.retryable());
    // Concurrent lands on the protected ref, commits and reads all work.
    for r in land_all(c, &hs).await {
        r.expect("land during the forge outage");
    }
    let (tip, v) = main_tip(c).await;
    assert_eq!(v, v0 + hs.len() as u64);
    c.log("main", 10).await.expect("log during the outage");
    c.commit(
        &tip,
        &[("faults/outage.txt".into(), b"x\n".to_vec())],
        change_id(4_999_999),
        "outage",
    )
    .await
    .expect("commit during the outage");

    // Recovery: the outage's lands reach the forge event log.
    switch.bring_up(Surface(ALL_SURFACES));
    c.list_changes().await.expect("forge back");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let mut seen = BTreeSet::new();
        let mut from = 0;
        loop {
            let (events, next) = c.list_events(&["ref.updated"], from).await.expect("events");
            for e in &events {
                if let Some((name, _, ver)) = ref_updated(e)
                    && name == "main"
                {
                    seen.insert(ver);
                }
            }
            if events.is_empty() || next <= from {
                break;
            }
            from = next;
        }
        if (v0 + 1..=v).all(|x| seen.contains(&x)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "outage lands never reached the forge log: {seen:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lists_and_lands_work_while_code_search_is_down() {
    let blocker = tempfile::tempdir().expect("tempdir");
    let file = blocker.path().join("not-a-dir");
    std::fs::write(&file, b"x").expect("write");
    let (s, _dir) = start_fresh(
        |c| c.search.index_dir = file.join("search").display().to_string(),
        None,
    )
    .await;
    let c = &s.client;
    let mut search =
        bee_proto::v1::search_service_client::SearchServiceClient::new(c.channel().clone());
    let e = RpcError::from(
        search
            .search_code(c.req(bee_proto::v1::SearchCodeRequest {
                query: "anything".into(),
                ..Default::default()
            }))
            .await
            .expect_err("index down"),
    );
    assert_eq!(e.reason(), "UNAVAILABLE");
    assert!(e.retryable());
    // Lists are served from their own tables (SPEC §15.1).
    let (tip, _) = main_tip(c).await;
    let head = heads(c, &tip, 1, 5).await[0];
    let cr = c
        .open_change("main", &head, "listed while search is down")
        .await
        .expect("open");
    let lease = c.acquire_lease("main", &["faults/5"]).await.expect("lease");
    let changes = c.list_changes().await.expect("change list");
    assert!(changes.iter().any(|(id, _)| *id == cr));
    assert!(c.list_leases().await.expect("lease list").contains(&lease));
    c.list_checks(&head).await.expect("check list");
    c.release_lease(&lease).await.expect("release");
    c.land_change(&cr).await.expect("land while search is down");
}
