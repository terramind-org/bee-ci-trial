//! Merge-queue ruleset parameters either constrain the queue or are explicitly fixed/refused.

mod common;

use common::*;
use serde_json::json;

async fn open_change(t: &TestServer, file: &str, cid: u64) -> String {
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[(file, b"queued\n")], cid, file).await["id"]);
    t.call(
        "ChangeService",
        "OpenChange",
        json!({ "repo": repo(), "head": id_json(&head), "title": file }),
    )
    .await
    .expect("open change")["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_merge_queue_values_are_accepted() {
    let t = start_default().await;
    t.call(
        "PolicyService",
        "CreateRuleset",
        json!({
            "repo": repo(), "name": "scenario-queue", "target": "RULESET_TARGET_BRANCH",
            "enforcement": "RULESET_ENFORCEMENT_ACTIVE",
            "conditions": { "refNameIncludes": ["~DEFAULT_BRANCH"] },
            "rules": [{ "mergeQueue": { "maxEntriesToMerge": 5, "minEntriesToMerge": 1 } }],
        }),
    )
    .await
    .expect("AS-CR-03 merge queue values are accepted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unhonoured_merge_queue_values_are_refused_by_name() {
    let t = start_default().await;
    for (rule, field) in [
        (
            json!({ "mergeMethod": "RULESET_MERGE_METHOD_SQUASH" }),
            "merge_method",
        ),
        (json!({ "maxEntriesToBuild": 2 }), "max_entries_to_build"),
        (json!({ "minEntriesToMerge": 2 }), "min_entries_to_merge"),
        (
            json!({ "minEntriesWaitSeconds": 1 }),
            "min_entries_wait_seconds",
        ),
        (json!({ "checkTimeoutSeconds": 1 }), "check_timeout_seconds"),
        (
            json!({ "allEntriesMustPass": true }),
            "all_entries_must_pass",
        ),
    ] {
        let e = t
            .call(
                "PolicyService",
                "CreateRuleset",
                json!({ "repo": repo(), "name": field, "rules": [{ "mergeQueue": rule }] }),
            )
            .await
            .expect_err("unhonoured parameter is refused");
        assert_eq!(reason(&e), "INVALID_ARGUMENT", "{e:?}");
        assert_eq!(e.subcode, "unsupported_rule");
        assert!(e.message.contains(field), "{field}: {}", e.message);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maximum_entries_to_merge_limits_each_target_queue() {
    let t = start_default().await;
    t.call(
        "PolicyService",
        "CreateRuleset",
        json!({
            "repo": repo(), "name": "bounded-queue", "target": "RULESET_TARGET_BRANCH",
            "enforcement": "RULESET_ENFORCEMENT_ACTIVE",
            "conditions": { "refNameIncludes": ["~DEFAULT_BRANCH"] },
            "rules": [
                { "requiredChecks": { "checks": [{ "name": "ci" }] } },
                { "mergeQueue": { "maxEntriesToMerge": 1 } },
            ],
        }),
    )
    .await
    .expect("create bounded queue ruleset");
    let first = open_change(&t, "queue/one.txt", 901).await;
    t.call(
        "ChangeService",
        "EnqueueChange",
        json!({ "repo": repo(), "id": first }),
    )
    .await
    .expect("first entry is queued while its check is pending");
    let second = open_change(&t, "queue/two.txt", 902).await;
    let e = t
        .call(
            "ChangeService",
            "EnqueueChange",
            json!({ "repo": repo(), "id": second }),
        )
        .await
        .expect_err("the per-ref maximum is enforced");
    assert_eq!(reason(&e), "RESOURCE_EXHAUSTED", "{e:?}");
    assert_eq!(e.subcode, "merge_queue_full");
}
