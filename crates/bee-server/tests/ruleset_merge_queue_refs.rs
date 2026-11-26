//! Merge-queue rulesets protect direct ref writes and route git gateway writes through Land.

mod common;

use common::*;
use serde_json::json;

async fn create_queue_ruleset(
    t: &TestServer,
    enforcement: &str,
    ref_name: &str,
) -> serde_json::Value {
    t.call(
        "PolicyService",
        "CreateRuleset",
        json!({
            "repo": repo(),
            "name": format!("queue-{enforcement}"),
            "target": "RULESET_TARGET_BRANCH",
            "enforcement": enforcement,
            "conditions": { "refNameIncludes": [ref_name] },
            "rules": [{ "mergeQueue": {} }],
        }),
    )
    .await
    .expect("create merge-queue ruleset")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_merge_queue_ruleset_refuses_direct_ref_writes() {
    let t = start_default().await;
    let (base, _) = t.tip("main").await;
    let initial = id_text(
        &t.commit(
            &base,
            &[("queue/initial.txt", b"initial\n")],
            919,
            "initial",
        )
        .await["id"],
    );
    t.call(
        "RefService",
        "CreateRef",
        json!({ "repo": repo(), "name": "queue-target", "target": id_json(&initial) }),
    )
    .await
    .expect("create queue target");
    create_queue_ruleset(&t, "RULESET_ENFORCEMENT_ACTIVE", "queue-target").await;
    let (tip, version) = t.tip("queue-target").await;
    let head = id_text(
        &t.commit(&tip, &[("queue/direct.txt", b"blocked\n")], 920, "blocked")
            .await["id"],
    );

    let update = t
        .call(
            "RefService",
            "UpdateRef",
            json!({ "repo": repo(), "name": "queue-target", "target": id_json(&head), "expectedVersion": version }),
        )
        .await
        .expect_err("direct update is refused");
    assert_eq!(reason(&update), "POLICY_DENIED", "{update:?}");
    assert_eq!(t.tip("queue-target").await, (tip.clone(), version));

    let delete = t
        .call(
            "RefService",
            "DeleteRef",
            json!({ "repo": repo(), "name": "queue-target", "expectedVersion": version }),
        )
        .await
        .expect_err("direct delete is refused");
    assert_eq!(reason(&delete), "POLICY_DENIED", "{delete:?}");
    assert_eq!(t.tip("queue-target").await, (tip, version));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evaluate_merge_queue_ruleset_leaves_direct_writes_and_lands_unaffected() {
    let t = start_default().await;
    let (base, _) = t.tip("main").await;
    let initial = id_text(
        &t.commit(
            &base,
            &[("queue/evaluate-initial.txt", b"initial\n")],
            920,
            "initial",
        )
        .await["id"],
    );
    t.call(
        "RefService",
        "CreateRef",
        json!({ "repo": repo(), "name": "evaluate-target", "target": id_json(&initial) }),
    )
    .await
    .expect("create evaluate target");
    let created = create_queue_ruleset(&t, "RULESET_ENFORCEMENT_ACTIVE", "evaluate-target").await;
    t.call(
        "PolicyService",
        "UpdateRuleset",
        json!({
            "rulesetId": created["id"],
            "expectedVersion": created["version"],
            "enforcement": "RULESET_ENFORCEMENT_EVALUATE",
        }),
    )
    .await
    .expect("switch ruleset to evaluate");
    let (tip, version) = t.tip("evaluate-target").await;
    let update_head = id_text(
        &t.commit(
            &tip,
            &[("queue/evaluate-ref.txt", b"allowed\n")],
            921,
            "update",
        )
        .await["id"],
    );
    t.call(
        "RefService",
        "UpdateRef",
        json!({ "repo": repo(), "name": "evaluate-target", "target": id_json(&update_head), "expectedVersion": version }),
    )
    .await
    .expect("evaluate ruleset does not block update");
    let land_head = id_text(
        &t.commit(
            &update_head,
            &[("queue/evaluate-land.txt", b"allowed\n")],
            922,
            "land",
        )
        .await["id"],
    );
    t.call(
        "WriteService",
        "Land",
        json!({ "repo": repo(), "ref": "evaluate-target", "head": id_json(&land_head) }),
    )
    .await
    .expect("evaluate ruleset does not block land");
    assert_eq!(t.tip("evaluate-target").await.0, land_head);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_queue_ruleset_does_not_protect_other_branches() {
    let t = start_default().await;
    create_queue_ruleset(&t, "RULESET_ENFORCEMENT_ACTIVE", "main").await;
    let (tip, _) = t.tip("main").await;
    let first = id_text(
        &t.commit(&tip, &[("queue/topic.txt", b"first\n")], 923, "topic")
            .await["id"],
    );
    t.call(
        "RefService",
        "CreateRef",
        json!({ "repo": repo(), "name": "topic", "target": id_json(&first) }),
    )
    .await
    .expect("unmatched branch can be created");
    let second = id_text(
        &t.commit(&first, &[("queue/topic.txt", b"second\n")], 924, "topic")
            .await["id"],
    );
    t.call(
        "RefService",
        "UpdateRef",
        json!({ "repo": repo(), "name": "topic", "target": id_json(&second) }),
    )
    .await
    .expect("unmatched branch can be updated");
    assert_eq!(t.tip("topic").await.0, second);
}
