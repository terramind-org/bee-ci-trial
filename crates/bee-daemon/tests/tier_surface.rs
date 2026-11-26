//! The tier's own surface (`docs/spec/cache-tiers.md` §3–§4): the upstream concurrency cap and
//! its queue, `HasObjects` always forwarded, the daemon socket's permissions and token
//! authorization, and the health and metrics listener of both tiers.

use crate::common;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bee_core::{Mode, ObjectId};
use bee_daemon::server::{App, Peer};
use bee_daemon::upstream::Auth;
use bee_daemon::watch::WatchRegistry;
use bee_daemon::{ConnectClient, Endpoint, Tier, TierConfig, TierKind, Upstream};
use bee_proto::v1;
use common::*;
use http_body_util::BodyExt as _;
use serde_json::json;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("rt")
}

fn auth(token: &str) -> Auth {
    Auth(Some(format!("Bearer {token}")))
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

/// `GET path` on a tier endpoint: status and body text.
fn get(rt: &tokio::runtime::Runtime, endpoint: &Endpoint, path: &str) -> (u16, String) {
    let client = ConnectClient::new(endpoint.clone(), Duration::from_secs(30));
    rt.block_on(async {
        let req = http::Request::builder()
            .method("GET")
            .uri(path)
            .body(axum::body::Body::empty())
            .expect("request");
        let resp = client.send(req).await.expect("response");
        let status = resp.status().as_u16();
        let body = resp.into_body().collect().await.expect("body").to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    })
}

/// The value of `name{tier="<tier>"}` in a Prometheus text exposition.
fn metric(text: &str, name: &str, tier: &str) -> Option<f64> {
    let prefix = format!("{name}{{tier=\"{tier}\"}} ");
    text.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .and_then(|v| v.trim().parse().ok())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn upstream_concurrency_is_capped_and_excess_requests_queue() {
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("a");
    mock.delay_ms.store(40, Ordering::SeqCst);
    let ids: Vec<ObjectId> = (0..64)
        .map(|n| {
            mock.add(
                &format!("f{n}"),
                format!("object {n}").as_bytes(),
                Mode::File,
            )
        })
        .collect();
    let dir = tempfile::tempdir().expect("tmp");
    let store = disk_store(&dir.path().join("cache"), 1 << 30);
    let mut cfg = TierConfig::new(TierKind::Daemon);
    assert_eq!(cfg.upstream_concurrency, 32, "the documented default");
    cfg.upstream_concurrency = 4;
    let up: Arc<dyn Upstream> = mock.clone();
    let tier = Arc::new(Tier::new(cfg, store, up));
    // 64 requests for 64 different objects: nothing coalesces, every one needs the tier above.
    let tasks: Vec<_> = ids
        .iter()
        .map(|id| {
            let tier = tier.clone();
            let id = *id;
            tokio::spawn(async move { tier.get_objects(&auth("a"), get_req(&[id])).await })
        })
        .collect();
    for (n, t) in tasks.into_iter().enumerate() {
        let (chunks, _) = t.await.expect("join").expect("queued requests are served");
        assert_eq!(chunks[0].status, v1::ItemStatus::Ok as i32);
        assert_eq!(chunks[0].data, format!("object {n}").as_bytes());
    }
    assert_eq!(mock.get_calls.load(Ordering::SeqCst), 64);
    let max = mock.max_inflight.load(Ordering::SeqCst);
    assert_eq!(
        max, 4,
        "at most upstream_concurrency requests are in flight, and the cap is reached"
    );
    assert_eq!(tier.stats_view().upstream_requests, 64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn has_objects_is_forwarded_even_when_every_object_is_cached() {
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("a");
    let ids: Vec<ObjectId> = (0..3)
        .map(|n| mock.add(&format!("h{n}"), format!("have {n}").as_bytes(), Mode::File))
        .collect();
    let dir = tempfile::tempdir().expect("tmp");
    let store = disk_store(&dir.path().join("cache"), 1 << 30);
    let up: Arc<dyn Upstream> = mock.clone();
    let tier = Arc::new(Tier::new(TierConfig::new(TierKind::Daemon), store, up));
    // Warm: every object is in the cache and readable by this credential.
    tier.get_objects(&auth("a"), get_req(&ids))
        .await
        .expect("warm");
    tier.get_objects(&auth("a"), get_req(&ids))
        .await
        .expect("served from the cache");
    assert_eq!(mock.get_calls.load(Ordering::SeqCst), 1);
    assert!(tier.stats_view().hits >= 3);
    let has = |token: &'static str| {
        let tier = tier.clone();
        let ids = ids.clone();
        async move {
            tier.has_objects(
                &auth(token),
                v1::HasObjectsRequest {
                    repo: Some(bee_daemon::snapshot::repo_ref(REPO)),
                    ids: ids
                        .iter()
                        .map(|i| v1::ObjectId {
                            raw: i.as_bytes().to_vec(),
                        })
                        .collect(),
                },
            )
            .await
        }
    };
    for round in 1..=3u64 {
        let (resp, _) = has("a").await.expect("has");
        assert_eq!(resp.present, vec![true, true, true]);
        assert_eq!(
            mock.has_calls.load(Ordering::SeqCst),
            round,
            "every HasObjects goes to the tier above"
        );
    }
    // The answer is the tier above's: a redaction there shows at once, cache or not.
    mock.redacted.lock().expect("lock").insert(ids[1]);
    let (resp, _) = has("a").await.expect("has");
    assert_eq!(resp.present, vec![true, false, true]);
    assert_eq!(mock.has_calls.load(Ordering::SeqCst), 4);
}

#[test]
fn daemon_socket_is_open_to_every_user_and_reads_need_a_token() {
    use std::os::unix::fs::PermissionsExt as _;
    let rt = rt();
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("good");
    let id = mock.add("hello.txt", b"hello", Mode::File);
    let up: Arc<dyn Upstream> = mock.clone();
    let mut d = start_daemon(&rt, up, TierConfig::new(TierKind::Daemon));
    let Endpoint::Unix(path) = d.endpoint.clone() else {
        panic!("the daemon listens on a unix socket");
    };
    let mode = std::fs::metadata(&path)
        .expect("socket")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o666, "any local user may connect");
    let read = |token: Option<&str>| {
        d.client(token).server_stream(
            "/bee.v1.ObjectService/GetObjects",
            &json!({ "repo": { "fullName": REPO }, "ids": [id_json(&id)] }),
        )
    };
    let items = read(Some("good")).expect("a valid token reads");
    assert_eq!(objects_of(&items)[&id], Ok(b"hello".to_vec()));
    // The cached bytes are not served to a caller without, or with a wrong, credential.
    assert_eq!(read(None).expect_err("no token").reason, "UNAUTHENTICATED");
    assert_eq!(
        read(Some("bad")).expect_err("bad token").reason,
        "UNAUTHENTICATED"
    );
    d.stop(&rt);
}

#[test]
fn local_file_system_operations_are_refused_to_other_users_on_the_socket() {
    use std::os::unix::fs::MetadataExt as _;
    let rt = rt();
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("t");
    let id = mock.add("a.txt", b"a", Mode::File);
    let dir = short_tempdir();
    let store = disk_store(&dir.path().join("cache"), 1 << 30);
    let up: Arc<dyn Upstream> = mock.clone();
    let tier = Arc::new(Tier::new(TierConfig::new(TierKind::Daemon), store, up));
    let socket = dir.path().join("o.sock");
    let listener = {
        let _g = rt.enter();
        tokio::net::UnixListener::bind(&socket).expect("bind")
    };
    // The daemon runs as some other user: this process is a different local user to it.
    let me = std::fs::metadata(&socket).expect("socket").uid();
    let app = Arc::new(App {
        tier,
        watches: WatchRegistry::new(),
        owner_uid: Some(me.wrapping_add(1)),
    });
    let svc = bee_daemon::server::router(app).into_make_service_with_connect_info::<Peer>();
    let server = rt.spawn(async move {
        let _ = axum::serve(listener, svc).await;
    });
    let c =
        bee_daemon::BlockingClient::new(Endpoint::Unix(socket.clone()), Some("t")).expect("client");
    // Object reads work for anyone with a token...
    let items = c
        .server_stream(
            "/bee.v1.ObjectService/GetObjects",
            &json!({ "repo": { "fullName": REPO }, "ids": [id_json(&id)] }),
        )
        .expect("read");
    assert_eq!(objects_of(&items)[&id], Ok(b"a".to_vec()));
    assert!(c.local("Stats", &json!({})).is_ok(), "stats are not local");
    // ...but nothing that touches the daemon's file system.
    let dest = dir.path().join("snap");
    for (op, body) in [
        (
            "Snapshot",
            json!({ "repo": REPO, "revision": "main", "dest": dest }),
        ),
        ("Watch", json!({ "root": dir.path() })),
        (
            "Pin",
            json!({ "name": "p", "repo": REPO, "revision": "main" }),
        ),
        ("Unpin", json!({ "name": "p" })),
        ("Flush", json!({})),
    ] {
        let e = c.local(op, &body).expect_err(op);
        assert_eq!(e.reason, "PERMISSION_DENIED", "{op}");
    }
    assert!(!dest.exists());
    server.abort();
}

#[test]
fn daemon_and_regional_serve_health_and_prometheus_metrics() {
    let rt = rt();
    let mock = Arc::new(MockUpstream::new());
    mock.allow_all("t");
    let id = mock.add("m.txt", b"metrics", Mode::File);
    let up: Arc<dyn Upstream> = mock.clone();
    let mut d = start_daemon(&rt, up.clone(), TierConfig::new(TierKind::Daemon));
    let mut r = start_regional(&rt, up, TierConfig::new(TierKind::Regional));
    for (tier, name) in [(&d, "daemon"), (&r, "regional")] {
        let (status, body) = get(&rt, &tier.endpoint, "/healthz");
        assert_eq!((status, body.trim()), (200, "ok"), "{name}");
        let c = tier.client(Some("t"));
        for _ in 0..3 {
            c.server_stream(
                "/bee.v1.ObjectService/GetObjects",
                &json!({ "repo": { "fullName": REPO }, "ids": [id_json(&id)] }),
            )
            .expect("read");
        }
        let (status, text) = get(&rt, &tier.endpoint, "/metrics");
        assert_eq!(status, 200, "{name}");
        for m in [
            "bee_cache_hits_total",
            "bee_cache_misses_total",
            "bee_cache_hit_ratio",
            "bee_cache_fail_closed_total",
            "bee_cache_bytes_served_total",
            "bee_cache_upstream_requests_total",
            "bee_cache_signed_url_fetches_total",
            "bee_cache_signed_url_fallbacks_total",
            "bee_cache_forwarded_total",
            "bee_cache_objects",
            "bee_cache_bytes",
            "bee_cache_evictions_total",
        ] {
            assert!(
                text.contains(&format!("# TYPE {m} ")),
                "{name}: {m} is declared"
            );
            assert!(metric(&text, m, name).is_some(), "{name}: {m}\n{text}");
        }
        assert_eq!(metric(&text, "bee_cache_misses_total", name), Some(1.0));
        assert_eq!(metric(&text, "bee_cache_hits_total", name), Some(2.0));
        assert_eq!(metric(&text, "bee_cache_objects", name), Some(1.0));
        assert_eq!(
            metric(&text, "bee_cache_bytes_served_total", name),
            Some(21.0)
        );
        let ratio = metric(&text, "bee_cache_hit_ratio", name).expect("ratio");
        assert!((ratio - 2.0 / 3.0).abs() < 1e-9, "{name}: {ratio}");
    }
    d.stop(&rt);
    r.stop(&rt);
}
