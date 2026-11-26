//! Forge-issued tokens follow the shared signing-key ring across replicas.

mod common;

use std::time::{Duration, Instant};

use bee_auth::{Principal, Verifier};
use bee_server::server::{ServerOptions, start};
use common::{TestServer, start_cfg, test_config};
use serde_json::json;

async fn replicas() -> (tempfile::TempDir, TestServer, TestServer) {
    let shared = tempfile::tempdir().expect("shared data directory");
    let mut cfg = test_config(shared.path());
    cfg.forge.access_cache_ms = 25;
    let a_dir = tempfile::tempdir().expect("replica A directory");
    let a = start_cfg(a_dir, cfg.clone()).await;
    let mut opts = ServerOptions::new(cfg.clone());
    opts.dev = false;
    let b = TestServer {
        srv: start(opts).await.expect("replica B starts"),
        token: a.token.clone(),
        dir: tempfile::tempdir().expect("replica B directory"),
        cfg,
    };
    (shared, a, b)
}

fn active_id(t: &TestServer) -> u32 {
    t.srv.state.ring.read().expect("ring").signer().id()
}

fn token_key_id(t: &TestServer, token: &str) -> u32 {
    let keys = t.srv.state.ring.read().expect("ring").verifying_keys();
    Verifier::new(&keys, t.srv.state.revocations.as_ref())
        .verify(token, t.srv.state.now_ms() / 1000)
        .expect("token verifies")
        .key_id
}

async fn wait_for_active(t: &TestServer, id: u32) {
    let deadline = Instant::now() + Duration::from_millis(t.cfg.forge.access_cache_ms * 8 + 200);
    while active_id(t) != id {
        assert!(
            Instant::now() < deadline,
            "rotated key was not visible within cache bound"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn installation(t: &TestServer, admin: &str) -> String {
    let manifest = json!({
        "name": "Rotation Bot", "url": "https://example.com/rotation", "public": false,
        "default_permissions": { "contents": "read" }, "default_events": [],
        "callback_urls": ["https://example.com/callback"]
    });
    let app = t
        .call_as(
            admin,
            "AppService",
            "CreateAppFromManifest",
            json!({
                "org": "dev", "manifestJson": manifest.to_string()
            }),
        )
        .await
        .expect("create app");
    let install = t
        .call_as(
            admin,
            "AppService",
            "CreateAppInstallation",
            json!({
                "app": app["app"]["slug"], "org": "dev",
                "repositorySelection": "APP_REPOSITORY_SELECTION_ALL"
            }),
        )
        .await
        .expect("install app");
    install["id"].as_str().expect("installation id").to_owned()
}

async fn installation_token(t: &TestServer, admin: &str, installation_id: &str) -> String {
    t.call_as(
        admin,
        "AppService",
        "CreateInstallationToken",
        json!({
            "installationId": installation_id, "ttlSeconds": 600
        }),
    )
    .await
    .expect("mint installation token")["token"]
        .as_str()
        .expect("token")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn installation_tokens_use_the_refreshed_signing_key() {
    let (_shared, a, b) = replicas().await;
    let old_id = active_id(&a);
    let rotated = a
        .call("AdminService", "RotateSigningKey", json!({}))
        .await
        .expect("rotate");
    let new_id = rotated["key"]["keyId"]
        .as_str()
        .expect("key id")
        .parse()
        .expect("numeric key id");
    wait_for_active(&b, new_id).await;
    let b_admin = b.token_for(&Principal::human(a.dev_user(), "dev"), &[]);
    let installation_id = installation(&b, &b_admin).await;

    let token = installation_token(&b, &b_admin, &installation_id).await;
    assert_eq!(
        token_key_id(&a, &token),
        new_id,
        "B signs with the rotated key"
    );
    a.call_as(&token, "AuthService", "WhoAmI", json!({}))
        .await
        .expect("A accepts token from B");

    a.call(
        "AdminService",
        "RetireSigningKey",
        json!({ "keyId": old_id.to_string() }),
    )
    .await
    .expect("retire old key");
    wait_for_active(&b, new_id).await;
    let after_retire = installation_token(&b, &b_admin, &installation_id).await;
    assert_eq!(token_key_id(&a, &after_retire), new_id);
    a.call_as(&after_retire, "AuthService", "WhoAmI", json!({}))
        .await
        .expect("A accepts token after retirement");

    a.srv.shutdown().await;
    b.srv.shutdown().await;
}
