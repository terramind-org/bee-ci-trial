//! Matrix coverage for rulesets that require change requests to enter the merge queue.

mod common;

use bee_proto::v1::ErrorDetail;
use common::*;
use serde_json::{Value, json};

async fn open_change(t: &TestServer, target: &str, file: &str, cid: u64) -> String {
    let (tip, _) = t.tip(target).await;
    let head = id_text(&t.commit(&tip, &[(file, b"queued\n")], cid, file).await["id"]);
    t.call(
        "ChangeService",
        "OpenChange",
        json!({ "repo": repo(), "targetRef": target, "head": id_json(&head), "title": file }),
    )
    .await
    .expect("open change")["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn land(t: &TestServer, id: &str) -> Result<Value, Box<ErrorDetail>> {
    t.call(
        "ChangeService",
        "LandChange",
        json!({ "repo": repo(), "id": id }),
    )
    .await
    .map_err(Box::new)
}

async fn queue_ruleset(t: &TestServer, owner: Value, refs: &[&str], name: &str) {
    let mut request = json!({
        "name": name,
        "target": "RULESET_TARGET_BRANCH",
        "enforcement": "RULESET_ENFORCEMENT_ACTIVE",
        "conditions": { "refNameIncludes": refs },
        "rules": [{ "mergeQueue": { "maxEntriesToMerge": 1, "minEntriesToMerge": 1 } }],
    });
    request
        .as_object_mut()
        .unwrap()
        .extend(owner.as_object().unwrap().clone());
    t.call("PolicyService", "CreateRuleset", request)
        .await
        .expect("create queue ruleset");
}

async fn assert_queue_required(t: &TestServer, target: &str, file: &str, cid: u64) {
    let cr = open_change(t, target, file, cid).await;
    let e = land(t, &cr).await.expect_err("direct land refused");
    assert_eq!(reason(&e), "INVALID_STATE", "{e:?}");
    assert_eq!(e.subcode, "merge_queue_required", "{e:?}");
}

async fn landed(t: &TestServer, id: &str) {
    eventually(30, "queued change to land", || async {
        let change = t
            .call(
                "ChangeService",
                "GetChange",
                json!({ "repo": repo(), "id": id }),
            )
            .await
            .ok()?;
        (change["state"] == "CHANGE_STATE_LANDED").then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_queue_requirement_ignores_ruleset_bypass_actors() {
    let t = start_default().await;
    let request = json!({
        "repo": repo(), "name": "queue", "target": "RULESET_TARGET_BRANCH",
        "enforcement": "RULESET_ENFORCEMENT_ACTIVE",
        "conditions": { "refNameIncludes": ["main"] },
        "rules": [{ "mergeQueue": { "maxEntriesToMerge": 1, "minEntriesToMerge": 1 } }],
        "bypassActors": [{ "kind": "RULESET_BYPASS_ACTOR_KIND_PRINCIPAL", "actorId": "dev", "mode": "RULESET_BYPASS_MODE_ALWAYS" }],
    });
    t.call("PolicyService", "CreateRuleset", request)
        .await
        .unwrap();
    assert_queue_required(&t, "main", "bypass.txt", 9101).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_and_org_ruleset_owners_cover_matching_repositories() {
    for (n, owner) in [
        ("repo", json!({ "repo": repo() })),
        ("org", json!({ "org": "dev" })),
    ] {
        let t = start_default().await;
        queue_ruleset(&t, owner, &["main"], n).await;
        assert_queue_required(&t, "main", &format!("{n}.txt"), 9200).await;
    }
    let t = start_default().await;
    queue_ruleset(&t, json!({ "repo": repo() }), &["main"], "repo").await;
    queue_ruleset(&t, json!({ "org": "dev" }), &["main"], "org").await;
    assert_queue_required(&t, "main", "both.txt", 9201).await;

    let t = start_default().await;
    let owner = json!({ "org": "dev" });
    let mut request = json!({
        "name": "other-repos", "target": "RULESET_TARGET_BRANCH",
        "enforcement": "RULESET_ENFORCEMENT_ACTIVE",
        "conditions": { "refNameIncludes": ["main"], "repoNameIncludes": ["other-*"] },
        "rules": [{ "mergeQueue": { "maxEntriesToMerge": 1, "minEntriesToMerge": 1 } }],
    });
    request
        .as_object_mut()
        .unwrap()
        .extend(owner.as_object().unwrap().clone());
    t.call("PolicyService", "CreateRuleset", request)
        .await
        .unwrap();
    let cr = open_change(&t, "main", "excluded.txt", 9202).await;
    land(&t, &cr)
        .await
        .expect("repository filter excludes playground");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_ruleset_ref_patterns_normalize_heads_and_are_case_sensitive() {
    for (n, pattern) in [("short", "main"), ("full", "refs/heads/main")] {
        let t = start_default().await;
        queue_ruleset(&t, json!({ "repo": repo() }), &[pattern], n).await;
        assert_queue_required(&t, "main", &format!("{n}.txt"), 9300).await;
    }
    let t = start_default().await;
    let (tip, _) = t.tip("main").await;
    t.call(
        "RefService",
        "CreateRef",
        json!({ "repo": repo(), "name": "release/1.0", "target": id_json(&tip) }),
    )
    .await
    .unwrap();
    t.call(
        "RefService",
        "CreateRef",
        json!({ "repo": repo(), "name": "Main", "target": id_json(&tip) }),
    )
    .await
    .unwrap();
    queue_ruleset(&t, json!({ "repo": repo() }), &["release/*"], "release").await;
    assert_queue_required(&t, "release/1.0", "release.txt", 9301).await;
    let main = open_change(&t, "main", "plain.txt", 9302).await;
    land(&t, &main)
        .await
        .expect("main does not match release/*");
    let upper = open_change(&t, "Main", "upper.txt", 9303).await;
    land(&t, &upper).await.expect("case differs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_parent_retargets_its_child_which_still_requires_the_queue() {
    let t = start_default().await;
    queue_ruleset(&t, json!({ "repo": repo() }), &["main"], "queue").await;
    let (tip, _) = t.tip("main").await;
    let parent_head = id_text(
        &t.commit(&tip, &[("parent.txt", b"parent\n")], 9401, "parent")
            .await["id"],
    );
    let parent = t
        .call(
            "ChangeService",
            "OpenChange",
            json!({ "repo": repo(), "head": id_json(&parent_head), "title": "parent" }),
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let child_head = id_text(
        &t.commit(&parent_head, &[("child.txt", b"child\n")], 9402, "child")
            .await["id"],
    );
    let child = t
        .call("ChangeService", "OpenChange", json!({ "repo": repo(), "head": id_json(&child_head), "title": "child", "dependsOn": parent }))
        .await
        .unwrap()["id"].as_str().unwrap().to_string();
    t.call(
        "ChangeService",
        "EnqueueChange",
        json!({ "repo": repo(), "id": parent }),
    )
    .await
    .expect("queue parent");
    landed(&t, &parent).await;
    let retargeted = t
        .call(
            "ChangeService",
            "GetChange",
            json!({ "repo": repo(), "id": child }),
        )
        .await
        .unwrap();
    assert_eq!(retargeted["targetRef"], "main", "{retargeted}");
    let e = land(&t, &child)
        .await
        .expect_err("child direct land refused");
    assert_eq!(reason(&e), "INVALID_STATE", "{e:?}");
    assert_eq!(e.subcode, "merge_queue_required", "{e:?}");
    t.call(
        "ChangeService",
        "EnqueueChange",
        json!({ "repo": repo(), "id": child }),
    )
    .await
    .expect("queue child");
    landed(&t, &child).await;
}
