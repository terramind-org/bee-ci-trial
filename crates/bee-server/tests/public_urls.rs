//! Client-visible URLs use the configured public URL rather than placeholder hosts.

mod common;

use common::{http, start_with};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cargo_registry_advertises_the_configured_public_url() {
    let mut t = start_with(|cfg| cfg.server.public_url = "https://bee.example".into()).await;
    let server = format!("http://{}", t.srv.addr);
    // Keep the test client's transport on the bound listener; `server.public_url` is what the
    // forge advertises to clients, not the address used by this in-process test harness.
    t.srv.url = server.clone();

    let (status, headers, body) = http(
        "GET",
        &format!("{server}/cargo/dev/index/1/a"),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 401);
    let challenge = headers
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .expect("Cargo authentication challenge");
    assert!(
        challenge.contains("login_url=\"https://bee.example/settings/tokens\""),
        "unexpected challenge: {challenge}"
    );
    assert!(!challenge.contains("beehive.invalid"));
    assert!(!String::from_utf8_lossy(&body).contains("beehive.invalid"));

    let (tip, _) = t.tip("main").await;
    let head = common::id_text(
        &t.commit(
            &tip,
            &[(".github/FUNDING.yml", b"beehive: bee\n")],
            1,
            "funding links",
        )
        .await["id"],
    );
    t.land(&head).await.expect("land funding file");
    let funding = t
        .call(
            "SponsorService",
            "GetSponsorFundingFile",
            serde_json::json!({ "repo": common::repo() }),
        )
        .await
        .expect("funding links");
    let sponsor_url = funding["links"][0]["urls"][0]
        .as_str()
        .expect("Bee sponsor URL");
    assert_eq!(sponsor_url, "https://bee.example/sponsors/bee");
    assert!(!sponsor_url.contains("beehive.invalid"));
    assert!(!funding.to_string().contains("beehive.invalid"));
}
