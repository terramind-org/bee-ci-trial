//! Rebase conflicts tell API clients exactly how to preserve and land the change.

mod common;

use base64::Engine as _;
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflicting_rebase_names_the_resolution_parent_and_change_id() {
    let t = start_default().await;
    let (base, _) = t.tip("main").await;
    let source = t
        .commit(
            &base,
            &[("pricing.txt", b"discount = 0.15\n")],
            41,
            "raise discount",
        )
        .await;
    let source_id = id_text(&source["id"]);
    let source_change_id = bee_core::ChangeId::from_bytes(
        &B64.decode(source["changeId"]["raw"].as_str().expect("change id"))
            .unwrap(),
    )
    .unwrap()
    .to_text();
    let moved_main = t
        .commit(
            &base,
            &[("pricing.txt", b"discount = 0.12 # Q3\n")],
            42,
            "Q3 promo",
        )
        .await;
    t.land(&id_text(&moved_main["id"]))
        .await
        .expect("move main");
    let (onto, _) = t.tip("main").await;

    let err = t
        .call(
            "WriteService",
            "Rebase",
            json!({
                "repo": repo(),
                "changeset": id_json(&source_id),
                "onto": { "expr": "main" },
            }),
        )
        .await
        .expect_err("conflicting rebase");
    assert_eq!(err.reason, "MERGE_CONFLICT", "{err:?}");
    assert!(
        err.message.contains("1 conflicting path(s): pricing.txt"),
        "{err:?}"
    );
    assert!(err.message.contains(&onto), "{err:?}");
    assert!(err.message.contains(&source_change_id), "{err:?}");
    assert!(
        err.message.contains("Create a resolved changeset with"),
        "{err:?}"
    );
    assert!(err.message.contains("then land it"), "{err:?}");

    // Follow the guidance: parent the resolution on the rebase target and reuse the ChangeId.
    let resolved = t
        .commit(
            &onto,
            &[("pricing.txt", b"discount = 0.15 # Q3\n")],
            41,
            "resolve rebase",
        )
        .await;
    let landed = t
        .land(&id_text(&resolved["id"]))
        .await
        .expect("land resolution");
    let landed_change_id = B64
        .decode(
            landed["landed"][0]["changeId"]["raw"]
                .as_str()
                .expect("landed change id"),
        )
        .unwrap();
    assert_eq!(
        bee_core::ChangeId::from_bytes(&landed_change_id)
            .unwrap()
            .to_text(),
        source_change_id
    );
    assert_eq!(
        t.read("pricing.txt", "main").await.unwrap(),
        b"discount = 0.15 # Q3\n"
    );
}
