//! A repo's queue requirement survives a forge-only outage on another server replica.

mod common;

use std::sync::Arc;

use bee_forge::store::ForgeStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::store::unavailable::{ALL_SURFACES, SwitchableStore};
use bee_server::server::{ServerOptions, start};
use common::{TestServer, id_text, reason, repo, test_config};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remembered_queue_requirement_keeps_a_branch_gated_while_the_forge_is_down() {
    let shared = tempfile::tempdir().unwrap();
    let cfg = test_config(shared.path());
    let sql = SqlStore::open(
        SqlConfig::sqlite(shared.path().join("forge")),
        Arc::new(bee_forge::ids::SystemClock),
    )
    .await
    .unwrap();
    let switch = Arc::new(SwitchableStore::new(Arc::new(sql)));

    let mut options_a = ServerOptions::new(cfg.clone());
    options_a.dev = true;
    options_a.forge_store = Some(switch.clone() as Arc<dyn ForgeStore>);
    let srv_a = start(options_a).await.unwrap();
    let token = srv_a.dev.as_ref().unwrap().token.clone();
    let a = TestServer {
        srv: srv_a,
        token: token.clone(),
        dir: tempfile::tempdir().unwrap(),
        cfg: cfg.clone(),
    };

    let mut options_b = ServerOptions::new(cfg.clone());
    options_b.forge_store = Some(switch.clone() as Arc<dyn ForgeStore>);
    let srv_b = start(options_b).await.unwrap();
    let b = TestServer {
        srv: srv_b,
        // The signing key and metadata KV are shared; A's dev principal can call B.
        token,
        dir: tempfile::tempdir().unwrap(),
        cfg,
    };

    a.call("IssueService", "ListIssues", json!({ "repo": repo() }))
        .await
        .expect("default forge access is available");

    // B's successful ordinary land reads and remembers the repository setting as false.
    let (initial_tip, _) = b.tip("main").await;
    let ordinary = id_text(
        &b.commit(
            &initial_tip,
            &[("replica/ordinary.txt", b"ordinary land\n")],
            1701,
            "warm false queue setting on B",
        )
        .await["id"],
    );
    b.land(&ordinary)
        .await
        .expect("B lands while the queue is not required");

    // Prepare the attempted direct update before A changes the setting. No subsequent forge
    // read through B can refresh its intentionally stale process-local cache.
    let (tip_before_blocked, version_before_blocked) = b.tip("main").await;
    let blocked = id_text(
        &b.commit(
            &tip_before_blocked,
            &[("replica/blocked.txt", b"must use the queue\n")],
            1702,
            "direct update after queue requirement",
        )
        .await["id"],
    );

    a.call(
        "RepoService",
        "UpdateRepoSettings",
        json!({ "repo": repo(), "mergeQueueRequired": true }),
    )
    .await
    .expect("A commits the queue requirement and observes its shared KV write");

    switch.take_down(bee_forge::store::Surface(ALL_SURFACES));
    let error = b
        .land(&blocked)
        .await
        .expect_err("B must not publish a direct update after A requires the queue");
    assert_eq!(reason(&error), "UNAVAILABLE", "{error:?}");
    assert!(error.retryable, "{error:?}");
    assert_eq!(
        error.subcode, "merge_queue_required",
        "the unavailable forge prevents queue-entry verification, but the shared setting still identifies the gate: {error:?}"
    );
    assert_eq!(
        b.tip("main").await,
        (tip_before_blocked, version_before_blocked),
        "the blocked direct update leaves main unchanged"
    );

    a.srv.shutdown().await;
    b.srv.shutdown().await;
}
