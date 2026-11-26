//! Signing-key changes propagate through the shared server metadata KV store.

mod common;

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bee_auth::Principal;
use bee_server::config::Config;
use bee_server::keys;
use bee_server::kv::{Kv, SqliteKv};
use bee_server::server::{ServerOptions, start};
use common::{TestServer, reason, start_cfg, test_config};
use serde_json::json;

static POSTGRES_REPLICA_TESTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

async fn postgres_replica_permit() -> Option<tokio::sync::OwnedSemaphorePermit> {
    std::env::var("BEE_TEST_PG").ok()?;
    POSTGRES_REPLICA_TESTS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .acquire_owned()
        .await
        .ok()
}

/// The test servers use one metadata database and one key-ring record, while listening on
/// separate ephemeral ports.  SQLite is deliberately shared too: single-node deployments keep
/// the same semantics, and setting BEE_TEST_PG switches the metadata database to Postgres.
async fn replicas() -> (tempfile::TempDir, TestServer, TestServer) {
    let shared = tempfile::tempdir().expect("shared data directory");
    let mut cfg = test_config(shared.path());
    cfg.forge.access_cache_ms = 25;
    let a_dir = tempfile::tempdir().expect("replica A directory");
    let b_dir = tempfile::tempdir().expect("replica B directory");
    let a = start_cfg(a_dir, cfg.clone()).await;
    // Bootstrap only one replica. It creates the shared dev fixtures; the second replica only
    // needs to serve the data A initialized, which also mirrors production startup.
    let mut opts = ServerOptions::new(cfg.clone());
    opts.dev = false;
    let srv = start(opts).await.expect("replica B starts");
    let b = TestServer {
        srv,
        token: String::new(),
        dir: b_dir,
        cfg,
    };
    (shared, a, b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rotated_token_minted_on_one_replica_verifies_on_the_other() {
    let _permit = postgres_replica_permit().await;
    let (_shared, a, b) = replicas().await;
    let old_id = a.srv.state.keys.read().unwrap()[0].id;
    a.call("AdminService", "RotateSigningKey", json!({}))
        .await
        .expect("rotate on replica A");

    let signing_token = a.token_for(&Principal::human(a.dev_user(), "dev"), &[]);
    let minted = a
        .call_as(
            &signing_token,
            "AuthService",
            "MintToken",
            json!({ "principalId": a.dev_user() }),
        )
        .await
        .expect("mint with A's rotated active key");
    let token = minted["token"].as_str().expect("minted token").to_owned();

    let deadline = Instant::now() + Duration::from_millis(b.cfg.forge.access_cache_ms * 8 + 200);
    loop {
        if b.call_as(&token, "AuthService", "WhoAmI", json!({}))
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the rotated token was not visible within the cache bound"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Keep the old id live for the paired retirement assertion below. The initial key is not
    // the active key after the rotation, so it is eligible for retirement.
    assert_eq!(old_id, 1);
    a.srv.shutdown().await;
    b.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retired_key_is_refused_by_the_other_replica_after_the_cache_bound() {
    let _permit = postgres_replica_permit().await;
    let (_shared, a, b) = replicas().await;
    let old_token = a.token.clone();
    let old_id = a.srv.state.keys.read().unwrap()[0].id;
    b.call_as(&old_token, "AuthService", "WhoAmI", json!({}))
        .await
        .expect("B verifies the initial token");
    a.call("AdminService", "RotateSigningKey", json!({}))
        .await
        .expect("rotate on A");
    a.call(
        "AdminService",
        "RetireSigningKey",
        json!({ "keyId": old_id.to_string() }),
    )
    .await
    .expect("retire the old key on A");

    let deadline = Instant::now() + Duration::from_millis(b.cfg.forge.access_cache_ms * 8 + 200);
    loop {
        match b
            .call_as(&old_token, "AuthService", "WhoAmI", json!({}))
            .await
        {
            Err(e) => {
                assert_eq!(reason(&e), "UNAUTHENTICATED");
                break;
            }
            Ok(_) => assert!(
                Instant::now() < deadline,
                "retired token remained valid past cache bound"
            ),
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    a.srv.shutdown().await;
    b.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_rotations_keep_every_key() {
    let _permit = postgres_replica_permit().await;
    let (_shared, a, b) = replicas().await;
    let (ra, rb) = tokio::join!(keys::rotate(&a.srv.state), keys::rotate(&b.srv.state));
    let ra = ra.expect("A rotation");
    let rb = rb.expect("B rotation");
    assert_ne!(ra.id, rb.id);

    keys::refresh(&a.srv.state).await.expect("refresh A");
    let ids: Vec<_> = a
        .srv
        .state
        .keys
        .read()
        .unwrap()
        .iter()
        .map(|k| k.id)
        .collect();
    assert!(ids.contains(&ra.id), "A's rotated key was lost: {ids:?}");
    assert!(ids.contains(&rb.id), "B's rotated key was lost: {ids:?}");
    a.srv.shutdown().await;
    b.srv.shutdown().await;
}

#[tokio::test]
async fn legacy_json_is_imported_once_then_never_read_again() {
    let dir = tempfile::tempdir().expect("data directory");
    let cfg = Config::dev(dir.path().to_owned(), None);
    let legacy = dir.path().join("signing-keys.json");
    std::fs::write(
        &legacy,
        r#"[{"id":7,"secret_hex":"1111111111111111111111111111111111111111111111111111111111111111","created_ms":9,"retired_ms":0}]"#,
    )
    .expect("legacy JSON");
    let kv = SqliteKv::open(&dir.path().join("server.db")).expect("KV");
    let (_, initial) = keys::load(&cfg, &kv, 10).await.expect("import legacy file");
    assert_eq!(initial[0].id, 7);

    // A changed legacy file must not affect a ring that was already imported into shared KV.
    std::fs::write(&legacy, "not JSON").expect("replace legacy JSON");
    let (_, after) = keys::load(&cfg, &kv, 11).await.expect("read shared ring");
    assert_eq!(after[0].id, 7);
    assert!(kv.get("signing_keys", b"ring").await.unwrap().is_some());
}
