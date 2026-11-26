//! The land coalescer through the real server path (SPEC §7.4): many concurrent landers on
//! one protected ref, and owner failover with fencing across two server processes sharing
//! one store.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bee_core::RefName;
use bee_proto::v1;
use bee_server::kv::KvOp;
use common::*;
use tonic::transport::Channel;

fn landers() -> usize {
    std::env::var("BEE_TEST_LANDERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
}

/// Uploads one blob per file name over gRPC and creates one changeset per file on `parent`.
async fn prepare(
    t: &TestServer,
    ch: &Channel,
    token: &str,
    parent: &str,
    n: usize,
    tag: u64,
) -> Vec<String> {
    let mut items = Vec::new();
    let mut blobs = Vec::new();
    for i in 0..n {
        let data = format!("lander {tag}-{i}\n").into_bytes();
        let id = bee_object::blob_id(&data).to_text();
        items.push(v1::UploadItem {
            repo: if i == 0 { prepo() } else { None },
            id: Some(pid(&id)),
            offset: 0,
            data: data.clone(),
            last: true,
        });
        blobs.push((id, data.len() as u64));
    }
    let mut wc = v1::write_service_client::WriteServiceClient::new(ch.clone());
    wc.upload_objects(t.bearer(token, futures_util::stream::iter(items)))
        .await
        .expect("upload");
    let mut handles = Vec::new();
    for (i, (blob, size)) in blobs.into_iter().enumerate() {
        let mut wc = v1::write_service_client::WriteServiceClient::new(ch.clone());
        let req = t.bearer(
            token,
            v1::CreateChangesetRequest {
                repo: prepo(),
                parents: vec![pid(parent)],
                delta: vec![v1::DeltaEntry {
                    path: format!("landers/{tag}/{i}.txt").into_bytes(),
                    op: Some(v1::delta_entry::Op::Put(v1::Put {
                        mode: 1,
                        blob: Some(pid(&blob)),
                        size,
                    })),
                }],
                change_id: pchange(tag * 1_000_000 + i as u64),
                message: format!("lander {i}"),
                author_time: Some(v1::Signature {
                    name: "l".into(),
                    time: Some(pbjson_types::Timestamp {
                        seconds: 1_790_000_000,
                        nanos: 0,
                    }),
                    tz_offset_minutes: 0,
                }),
                ..Default::default()
            },
        );
        handles.push(tokio::spawn(async move {
            let cs = wc
                .create_changeset(req)
                .await
                .expect("create")
                .into_inner()
                .changeset
                .unwrap();
            bee_core::ObjectId::from_bytes(&cs.id.unwrap().raw)
                .unwrap()
                .to_text()
        }));
    }
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}

struct Landed {
    version: u64,
    latency: Duration,
}

async fn land_all(
    t: &TestServer,
    ch: &Channel,
    token: &str,
    heads: Vec<String>,
) -> Vec<Result<Landed, tonic::Status>> {
    let start = Arc::new(tokio::sync::Barrier::new(heads.len()));
    let mut handles = Vec::new();
    for head in heads {
        let mut wc = v1::write_service_client::WriteServiceClient::new(ch.clone());
        let req = t.bearer(
            token,
            v1::LandRequest {
                repo: prepo(),
                r#ref: "main".into(),
                head: Some(pid(&head)),
                ..Default::default()
            },
        );
        let start = start.clone();
        handles.push(tokio::spawn(async move {
            start.wait().await;
            let t0 = Instant::now();
            let r = wc.land(req).await?;
            let r = r.into_inner();
            assert_eq!(r.landed.len(), 1);
            assert!(!r.landed[0].skipped);
            Ok(Landed {
                version: r.r#ref.unwrap().version,
                latency: t0.elapsed(),
            })
        }));
    }
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}

fn pct(sorted: &[Duration], p: f64) -> Duration {
    let i = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[i]
}

async fn assert_ref_log_continuous(t: &TestServer, expect_last: u64) {
    let repo = t.srv.dev.as_ref().unwrap().repo;
    let main = RefName::new("main").unwrap();
    let log = t
        .srv
        .state
        .meta
        .ref_log(repo, &main, 1, 1_000_000)
        .await
        .unwrap();
    let versions: Vec<u64> = log.iter().map(|e| e.version).collect();
    assert_eq!(
        versions,
        (1..=expect_last).collect::<Vec<_>>(),
        "ref log has no gaps"
    );
    for w in log.windows(2) {
        assert_eq!(
            w[0].new, w[1].old,
            "each entry starts where the previous ended"
        );
    }
    let landed = t
        .srv
        .state
        .meta
        .list_landed(repo, Some(&main))
        .await
        .unwrap();
    let total: usize = log.iter().map(|e| e.landed.len()).sum();
    assert_eq!(
        landed.len(),
        total,
        "every landed change is indexed exactly once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn thousand_concurrent_landers_on_one_ref_land_exactly_once() {
    let n = landers();
    let t = start_with(|c| {
        c.limits.default_deadline_ms = 120_000;
    })
    .await;
    let ch = t.grpc_channel().await;
    let (root, _) = t.tip("main").await;
    let heads = prepare(&t, &ch, &t.token, &root, n, 1).await;
    let wall = Instant::now();
    let results = land_all(&t, &ch, &t.token, heads).await;
    let elapsed = wall.elapsed();
    let mut versions = Vec::new();
    let mut lat = Vec::new();
    for r in results {
        let l = r.unwrap_or_else(|s| panic!("land failed: {s:?}"));
        versions.push(l.version);
        lat.push(l.latency);
    }
    versions.sort();
    assert_eq!(
        versions,
        (2..=n as u64 + 1).collect::<Vec<_>>(),
        "every lander got its own version"
    );
    assert_ref_log_continuous(&t, n as u64 + 1).await;
    let batches = t
        .srv
        .state
        .metrics
        .land_batches
        .load(std::sync::atomic::Ordering::Relaxed);
    lat.sort();
    eprintln!(
        "coalescer: {n} landers in {:.2}s = {:.0} lands/s; {batches} CAS batches (avg {:.1} per batch); time-to-land p50 {:?} p99 {:?} max {:?}",
        elapsed.as_secs_f64(),
        n as f64 / elapsed.as_secs_f64(),
        n as f64 / batches.max(1) as f64,
        pct(&lat, 0.50),
        pct(&lat, 0.99),
        lat.last().unwrap()
    );
    if n >= 50 {
        assert!(
            batches < n as u64,
            "lands were coalesced ({batches} batches for {n} lands)"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn owner_failover_is_safe_and_fenced() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(dir.path());
    cfg.land.owner_lease_ms = 600;
    cfg.logging.level = "debug".into();
    let a = start_cfg(tempfile::tempdir().unwrap(), cfg.clone()).await;
    let b = start_cfg(tempfile::tempdir().unwrap(), cfg.clone()).await;
    let repo = a.srv.dev.as_ref().unwrap().repo;
    assert_eq!(
        repo,
        b.srv.dev.as_ref().unwrap().repo,
        "both processes serve one store"
    );
    let ch_a = a.grpc_channel().await;
    let ch_b = b.grpc_channel().await;
    let (root, _) = a.tip("main").await;

    // Phase 1: A owns main.
    let heads = prepare(&a, &ch_a, &a.token, &root, 30, 2).await;
    for r in land_all(&a, &ch_a, &a.token, heads).await {
        r.expect("land via A");
    }
    let lease = bee_server::coalescer::read_lease(&a.srv.state, repo, "main")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lease.owner, a.srv.state.instance_id);
    let first_fence = lease.fence;

    // Phase 2: A crashes holding the lease; lands through B still succeed (forwarding to
    // the dead owner fails, so B lands directly), then B takes over with a higher fence.
    a.srv.crash().await;
    let heads = prepare(&b, &ch_b, &b.token, &root, 30, 3).await;
    for r in land_all(&b, &ch_b, &b.token, heads).await {
        r.expect("land via B while A's lease is live");
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    let heads = prepare(&b, &ch_b, &b.token, &root, 30, 4).await;
    for r in land_all(&b, &ch_b, &b.token, heads).await {
        r.expect("land via B as owner");
    }
    let lease = bee_server::coalescer::read_lease(&b.srv.state, repo, "main")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lease.owner, b.srv.state.instance_id);
    assert!(
        lease.fence > first_fence,
        "takeover increments the fencing token"
    );

    // Phase 3: B is deposed by another owner (whose endpoint is unreachable): B must not
    // publish as owner, and lands still land exactly once.
    let intruder = serde_json::json!({ "owner": "intruder", "fence": lease.fence + 1, "expires_ms": b.srv.state.now_ms() + 60_000, "endpoint": "http://127.0.0.1:9" });
    let key = format!("{}/main", repo.0.to_text()).into_bytes();
    assert!(
        b.srv
            .state
            .kv
            .apply(vec![KvOp::put(
                "lease",
                key.clone(),
                serde_json::to_vec(&intruder).unwrap()
            )])
            .await
            .unwrap()
    );
    let heads = prepare(&b, &ch_b, &b.token, &root, 20, 5).await;
    for r in land_all(&b, &ch_b, &b.token, heads).await {
        r.expect("land via deposed B");
    }
    let lease = bee_server::coalescer::read_lease(&b.srv.state, repo, "main")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        lease.owner, "intruder",
        "a deposed owner never takes a live lease back"
    );
    assert_ref_log_continuous(&b, 1 + 30 + 30 + 30 + 20).await;
    let f = b
        .call(
            "AdminService",
            "RunFsck",
            serde_json::json!({ "org": "dev" }),
        )
        .await
        .unwrap();
    assert_eq!(f["clean"], true, "{f}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_retries_of_one_land_land_it_once() {
    let t = start_default().await;
    let ch = t.grpc_channel().await;
    let (root, _) = t.tip("main").await;
    let heads = prepare(&t, &ch, &t.token, &root, 20, 6).await;
    let mut handles = Vec::new();
    for head in heads.iter().chain(heads.iter()) {
        let mut wc = v1::write_service_client::WriteServiceClient::new(ch.clone());
        let req = t.bearer(
            &t.token,
            v1::LandRequest {
                repo: prepo(),
                r#ref: "main".into(),
                head: Some(pid(head)),
                ..Default::default()
            },
        );
        let head = head.clone();
        handles.push(tokio::spawn(async move {
            let r = wc.land(req).await.expect("land").into_inner();
            (head, r.r#ref.unwrap().version, r.landed[0].skipped)
        }));
    }
    let mut by_head: std::collections::BTreeMap<String, Vec<(u64, bool)>> = Default::default();
    for h in handles {
        let (head, version, skipped) = h.await.unwrap();
        by_head.entry(head).or_default().push((version, skipped));
    }
    for (head, mut answers) in by_head {
        answers.sort();
        assert_eq!(answers.len(), 2, "{head}");
        assert_eq!(
            answers[0].0, answers[1].0,
            "both answers name the same version: {answers:?}"
        );
        assert_eq!(
            answers.iter().filter(|a| !a.1).count(),
            1,
            "landed once, the retry is skipped: {answers:?}"
        );
    }
    assert_ref_log_continuous(&t, 21).await;
}

/// A forward to the owner that fails below the API (the owner died mid-request, or a proxy
/// answered for it) carries no Bee error detail. The land then lands directly, exactly
/// once, instead of failing with INTERNAL: before this, killing the owner of a three-replica
/// deployment under load failed 16 of 640 lands that way (docs/scaling.md).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_forward_that_fails_below_the_api_lands_directly() {
    let t = start_default().await;
    let repo = t.srv.dev.as_ref().unwrap().repo;
    // An owner endpoint that completes the HTTP/2 handshake (so it counts as reachable) and
    // answers every request with a bare HTTP 500 and no gRPC status, as a proxy in front of a
    // dead process does; tonic maps it to INTERNAL, without an ErrorDetail.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app =
            axum::Router::new().fallback(|| async { http::StatusCode::INTERNAL_SERVER_ERROR });
        let _ = axum::serve(listener, app).await;
    });
    let owner = serde_json::json!({
        "owner": "dying",
        "fence": 7,
        "expires_ms": t.srv.state.now_ms() + 600_000,
        "endpoint": format!("http://{addr}"),
    });
    let key = format!("{}/main", repo.0.to_text()).into_bytes();
    assert!(
        t.srv
            .state
            .kv
            .apply(vec![KvOp::put(
                "lease",
                key,
                serde_json::to_vec(&owner).unwrap()
            )])
            .await
            .unwrap()
    );
    let ch = t.grpc_channel().await;
    let (root, _) = t.tip("main").await;
    let heads = prepare(&t, &ch, &t.token, &root, 10, 7).await;
    for r in land_all(&t, &ch, &t.token, heads).await {
        r.expect("the land falls back to landing directly");
    }
    assert_ref_log_continuous(&t, 11).await;
    let lease = bee_server::coalescer::read_lease(&t.srv.state, repo, "main")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lease.owner, "dying", "a live lease is not taken over");
}
