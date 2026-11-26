//! CI follow-ups on the real server: `SecretService`, `DeploymentService` and the bisect RPCs
//! answer, and CI's `/_ci/*` transfer URLs are mounted and refuse unsigned requests.

mod common;

use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_deployment_and_bisect_services_and_transfer_urls_are_served() {
    let t = start_default().await;
    let key = t
        .call(
            "SecretService",
            "GetSecretPublicKey",
            json!({ "scope": { "kind": "SECRET_SCOPE_KIND_REPO", "repo": repo() } }),
        )
        .await
        .expect("public key");
    assert!(!key["keyId"].as_str().unwrap_or("").is_empty(), "{key}");
    let env = t
        .call(
            "DeploymentService",
            "CreateDeploymentEnvironment",
            json!({ "repo": repo(), "name": "production" }),
        )
        .await
        .expect("environment");
    assert_eq!(env["name"], "production");
    let bisects = t
        .call("CheckService", "ListBisects", json!({ "repo": repo() }))
        .await
        .expect("bisects");
    assert!(bisects["bisects"].as_array().is_none_or(|b| b.is_empty()));
    let unsigned = format!("{}/_ci/repos/x/jobs/y/logs", t.url());
    let (status, _, _) = http("GET", &unsigned, &[], Vec::new()).await;
    assert_eq!(status, 403);
    let forged = format!("{unsigned}?expires=99999999999999&signature=00");
    let (status, _, _) = http("GET", &forged, &[], Vec::new()).await;
    assert_eq!(status, 403);
}
