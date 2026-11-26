//! `GetServerInfo` reports the configured edition (SPEC server §2, `server.edition`).

mod common;

use bee_server::config::Edition;
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_info_reports_the_configured_edition() {
    // Unset: self-hosted.
    let t = start_default().await;
    let info = t
        .call("ObjectService", "GetServerInfo", json!({}))
        .await
        .expect("server info");
    assert_eq!(info["edition"], "enterprise");

    let t = start_with(|c| c.server.edition = Edition::Cloud).await;
    let info = t
        .call("ObjectService", "GetServerInfo", json!({}))
        .await
        .expect("server info");
    assert_eq!(info["edition"], "cloud");
}
