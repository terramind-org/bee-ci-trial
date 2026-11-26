//! App-token revocation is durable in the shared store before suspension reports success.

mod common;

use bee_auth::{RevocationList, TokenId};
use bee_server::server::{ServerOptions, start};
use common::{reason, repo, start_default};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn suspension_persists_revocation_for_a_second_server_handle_before_returning() {
    let first = start_default().await;
    let mut options = ServerOptions::new(first.cfg.clone());
    options.dev = true;
    let second = start(options).await.expect("second server starts");

    let manifest = json!({
        "name": "Revocation Bot",
        "url": "https://example.com/revocation",
        "public": false,
        "default_permissions": { "contents": "read" },
        "default_events": [],
        "callback_urls": ["https://example.com/callback"]
    });
    let app = first
        .call(
            "AppService",
            "CreateAppFromManifest",
            json!({ "org": "dev", "manifestJson": manifest.to_string() }),
        )
        .await
        .expect("app created");
    let install = first
        .call(
            "AppService",
            "CreateAppInstallation",
            json!({
                "app": app["app"]["slug"],
                "org": "dev",
                "repositorySelection": "APP_REPOSITORY_SELECTION_SELECTED",
                "repos": [repo()]
            }),
        )
        .await
        .expect("installation created");
    let token = first
        .call(
            "AppService",
            "CreateInstallationToken",
            json!({ "installationId": install["id"], "ttlSeconds": 600 }),
        )
        .await
        .expect("installation token minted");
    let token_id = token["tokenId"].as_str().expect("token id").to_owned();

    let suspended = first
        .call(
            "AppService",
            "SuspendAppInstallation",
            json!({ "installationId": install["id"], "reason": "investigating" }),
        )
        .await
        .expect("suspension completes only after revocation is written");
    assert_eq!(suspended["suspended"], true);

    second
        .state
        .revocations
        .refresh(second.state.kv.as_ref())
        .await
        .expect("second handle refreshes from the shared store");
    assert!(
        second.state.revocations.is_revoked(&TokenId(token_id)),
        "the second server must read the persisted revocation immediately"
    );

    let still_usable = first
        .call_as(
            token["token"].as_str().expect("token value"),
            "AuthService",
            "WhoAmI",
            json!({}),
        )
        .await;
    assert_eq!(
        reason(&still_usable.expect_err("suspended token is revoked")),
        "TOKEN_REVOKED"
    );
}
