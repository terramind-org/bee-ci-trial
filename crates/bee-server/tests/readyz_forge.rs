//! `/readyz` and the forge: a process whose forge is enabled but did not open serves the VCS
//! only, so it must not be Ready; `/healthz` is unaffected.

mod common;

use common::{http, start_with};

async fn get(url: &str) -> (u16, String) {
    let (status, _, body) = http("GET", url, &[], Vec::new()).await;
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readyz_is_503_when_the_forge_is_enabled_but_could_not_open() {
    // A forge database under a path that is not a directory cannot be created.
    let t = start_with(|cfg| cfg.forge.database = "sqlite:/dev/null/forge".into()).await;
    assert!(t.srv.forge.is_none(), "the forge must not have opened");
    let (status, body) = get(&format!("{}/readyz", t.url())).await;
    assert_eq!(status, 503, "readyz: {body}");
    assert!(body.contains("forge unavailable"), "readyz: {body}");
    let (status, body) = get(&format!("{}/healthz", t.url())).await;
    assert_eq!(status, 200, "healthz stays up: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readyz_is_200_when_the_forge_is_open() {
    let t = start_with(|_| {}).await;
    assert!(t.srv.forge.is_some(), "the forge runs");
    let (status, body) = get(&format!("{}/readyz", t.url())).await;
    assert_eq!(status, 200, "readyz: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readyz_is_200_when_the_forge_is_disabled() {
    let t = start_with(|cfg| cfg.forge.enabled = false).await;
    assert!(t.srv.forge.is_none());
    let (status, body) = get(&format!("{}/readyz", t.url())).await;
    assert_eq!(status, 200, "readyz: {body}");
}
