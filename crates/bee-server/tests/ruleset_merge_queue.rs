//! The `merge_queue` ruleset rule (AS-CR-03): accepted with the parameters the queue honours,
//! round-tripped by GetRuleset and ListRulesets, and enforced by the forge while the ruleset
//! is active on the change request's target ref: a direct `LandChange` is refused with
//! `merge_queue_required`, `EnqueueChange` lands, auto-merge enqueues. Evaluate mode and refs
//! the ruleset does not match land directly.

mod common;

use bee_proto::v1::ErrorDetail;
use common::*;
use serde_json::{Value, json};

async fn org_member(t: &TestServer, principal: &str) {
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": principal, "role": "ROLE_MEMBER" }),
    )
    .await
    .expect("set member");
}

/// Opens a change request by the dev user putting `file` on main; returns its id.
async fn open_change(t: &TestServer, file: &str, cid: u64) -> String {
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[(file, b"queued\n")], cid, file).await["id"]);
    let cr = t
        .call(
            "ChangeService",
            "OpenChange",
            json!({ "repo": repo(), "head": id_json(&head), "title": file }),
        )
        .await
        .expect("open change");
    cr["id"].as_str().unwrap().to_string()
}

async fn approve(t: &TestServer, token: &str, id: &str) {
    t.call_as(
        token,
        "ChangeService",
        "SubmitReview",
        json!({ "repo": repo(), "changeRequestId": id, "revision": 1, "verdict": "VERDICT_APPROVE" }),
    )
    .await
    .expect("approve");
}

async fn landed(t: &TestServer, id: &str) {
    eventually(30, "the change to land", || async {
        let c = t
            .call(
                "ChangeService",
                "GetChange",
                json!({ "repo": repo(), "id": id }),
            )
            .await
            .ok()?;
        (c["state"] == "CHANGE_STATE_LANDED").then_some(())
    })
    .await;
}

async fn land_change(t: &TestServer, id: &str) -> Result<Value, Box<ErrorDetail>> {
    t.call(
        "ChangeService",
        "LandChange",
        json!({ "repo": repo(), "id": id }),
    )
    .await
    .map_err(Box::new)
}

async fn land_ref(
    t: &TestServer,
    head: &str,
    change_request_id: Option<&str>,
) -> Result<Value, Box<ErrorDetail>> {
    let mut request = json!({ "repo": repo(), "ref": "main", "head": id_json(head) });
    if let Some(id) = change_request_id {
        request["changeRequestId"] = json!(id);
    }
    t.call("WriteService", "Land", request)
        .await
        .map_err(Box::new)
}

fn queue_rule(rs: &Value) -> Vec<Value> {
    rs["rules"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r.get("mergeQueue").cloned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_land_requires_a_landing_queue_entry() {
    let t = start_default().await;
    t.call(
        "PolicyService",
        "CreateRuleset",
        json!({
            "repo": repo(),
            "name": "native-land-queue",
            "target": "RULESET_TARGET_BRANCH",
            "enforcement": "RULESET_ENFORCEMENT_ACTIVE",
            "conditions": { "refNameIncludes": ["~DEFAULT_BRANCH"] },
            "rules": [{ "mergeQueue": {} }],
        }),
    )
    .await
    .expect("create active queue rule");
    let (tip, version) = t.tip("main").await;
    let direct = id_text(
        &t.commit(&tip, &[("native/direct.txt", b"no queue\n")], 850, "direct")
            .await["id"],
    );
    let error = land_ref(&t, &direct, None)
        .await
        .expect_err("native land is refused");
    assert_eq!(reason(&error), "INVALID_STATE", "{error:?}");
    assert_eq!(error.subcode, "merge_queue_required");
    assert_eq!(
        t.tip("main").await,
        (tip.clone(), version),
        "main stays unmoved"
    );

    let change = open_change(&t, "native/change.txt", 851).await;
    let change_head = t
        .call(
            "ChangeService",
            "GetChange",
            json!({ "repo": repo(), "id": change }),
        )
        .await
        .expect("get open change")["revisions"][0]["head"]
        .clone();
    let error = t
        .call(
            "WriteService",
            "Land",
            json!({ "repo": repo(), "ref": "main", "head": change_head, "changeRequestId": change }),
        )
        .await
        .expect_err("an open but unqueued change cannot use native land");
    assert_eq!(reason(&error), "INVALID_STATE", "{error:?}");
    assert_eq!(error.subcode, "merge_queue_required");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_land_honours_the_repository_queue_setting() {
    let t = start_default().await;
    t.call(
        "RepoService",
        "UpdateRepoSettings",
        json!({ "repo": repo(), "mergeQueueRequired": true }),
    )
    .await
    .expect("require the queue");
    let (tip, version) = t.tip("main").await;
    let head = id_text(
        &t.commit(
            &tip,
            &[("native/setting.txt", b"setting\n")],
            852,
            "setting",
        )
        .await["id"],
    );
    let error = land_ref(&t, &head, None)
        .await
        .expect_err("native land is refused");
    assert_eq!(reason(&error), "INVALID_STATE", "{error:?}");
    assert_eq!(error.subcode, "merge_queue_required");
    assert_eq!(t.tip("main").await, (tip, version), "main stays unmoved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merge_queue_ruleset_forces_changes_through_the_queue() {
    let t = start_default().await;
    // The reviewer is another agent with write access, as in AS-CR-03.
    let agent = t
        .call(
            "AuthService",
            "CreateAgent",
            json!({ "display": "queue-reviewer", "runtime": "claude-code", "purpose": "code review", "org": "dev" }),
        )
        .await
        .expect("create agent");
    let reviewer = agent["id"].as_str().unwrap().to_string();
    org_member(&t, &reviewer).await;
    t.call(
        "RepoService",
        "AddCollaborator",
        json!({ "repo": repo(), "principalId": reviewer, "role": "REPO_ROLE_WRITE" }),
    )
    .await
    .expect("add collaborator");
    let minted = t
        .call(
            "AuthService",
            "MintToken",
            json!({
                "principalId": reviewer,
                "scope": { "repos": ["dev/playground"], "actions": ["read", "review"] },
            }),
        )
        .await
        .expect("mint reviewer token");
    let reviewer_token = minted["token"].as_str().unwrap().to_string();

    // AS-CR-03's setup, verbatim.
    let rs = t
        .call(
            "PolicyService",
            "CreateRuleset",
            json!({
                "repo": repo(),
                "name": "queue-and-review",
                "target": "RULESET_TARGET_BRANCH",
                "enforcement": "RULESET_ENFORCEMENT_ACTIVE",
                "conditions": { "refNameIncludes": ["~DEFAULT_BRANCH"] },
                "rules": [
                    { "requiredApprovals": { "count": 1, "agentsMayApprove": true } },
                    { "mergeQueue": { "maxEntriesToMerge": 5, "minEntriesToMerge": 1 } },
                ],
            }),
        )
        .await
        .expect("a merge-queue ruleset is accepted");
    let id = rs["id"].as_str().unwrap().to_string();
    let want = json!({ "maxEntriesToMerge": 5, "minEntriesToMerge": 1 });
    assert_eq!(queue_rule(&rs), vec![want.clone()], "{rs}");
    let got = t
        .call("PolicyService", "GetRuleset", json!({ "rulesetId": id }))
        .await
        .expect("get ruleset");
    assert_eq!(got["enforcement"], "RULESET_ENFORCEMENT_ACTIVE");
    assert_eq!(queue_rule(&got), vec![want.clone()], "{got}");
    assert_eq!(got["rules"], rs["rules"], "the rules round-trip unchanged");
    let list = t
        .call("PolicyService", "ListRulesets", json!({ "repo": repo() }))
        .await
        .expect("list rulesets");
    let listed = list["rulesets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id.as_str())
        .cloned()
        .expect("listed");
    assert_eq!(queue_rule(&listed), vec![want], "{listed}");

    // Approved by the other agent, a change still cannot land directly ...
    let cr1 = open_change(&t, "queue/one.txt", 801).await;
    approve(&t, &reviewer_token, &cr1).await;
    let e = land_change(&t, &cr1)
        .await
        .expect_err("direct land refused under the ruleset");
    assert_eq!(reason(&e), "INVALID_STATE", "{e:?}");
    assert_eq!(e.subcode, "merge_queue_required");
    // ... and lands through the queue.
    t.call(
        "ChangeService",
        "EnqueueChange",
        json!({ "repo": repo(), "id": cr1 }),
    )
    .await
    .expect("enqueue");
    landed(&t, &cr1).await;
    let (tip, _) = t.tip("main").await;
    assert_eq!(t.read("queue/one.txt", &tip).await.unwrap(), b"queued\n");

    // Auto-merge enqueues rather than landing directly.
    let cr2 = open_change(&t, "queue/two.txt", 802).await;
    t.call(
        "ChangeService",
        "EnableAutoMerge",
        json!({ "repo": repo(), "id": cr2, "method": "LAND_METHOD_REBASE" }),
    )
    .await
    .expect("auto-merge");
    approve(&t, &reviewer_token, &cr2).await;
    landed(&t, &cr2).await;
    let number2 = t
        .call(
            "ChangeService",
            "GetChange",
            json!({ "repo": repo(), "id": cr2 }),
        )
        .await
        .unwrap()["number"]
        .clone();
    eventually(30, "auto-merge to go through the queue", || async {
        let ev = t
            .call(
                "EventService",
                "ListEvents",
                json!({ "repo": repo(), "kinds": ["merge_queue.entered"] }),
            )
            .await
            .ok()?;
        ev["events"]
            .as_array()?
            .iter()
            .any(|e| e["change"]["id"] == cr2.as_str() || e["change"]["number"] == number2)
            .then_some(())
    })
    .await;

    // Complete the AS-CR-03 shape: five approved changes, in order, through the queue.
    // Each is opened after its predecessor lands, so its base is the queue's resulting tip.
    for (file, cid) in [
        ("queue/three.txt", 803),
        ("queue/four.txt", 804),
        ("queue/five.txt", 805),
    ] {
        let id = open_change(&t, file, cid).await;
        t.call(
            "ChangeService",
            "EnableAutoMerge",
            json!({ "repo": repo(), "id": id, "method": "LAND_METHOD_REBASE" }),
        )
        .await
        .expect("auto-merge");
        approve(&t, &reviewer_token, &id).await;
        landed(&t, &id).await;
    }

    // Evaluate mode does not force the queue.
    let v = got["version"].clone();
    let upd = t
        .call(
            "PolicyService",
            "UpdateRuleset",
            json!({ "rulesetId": id, "expectedVersion": v, "enforcement": "RULESET_ENFORCEMENT_EVALUATE" }),
        )
        .await
        .expect("evaluate mode");
    let cr6 = open_change(&t, "queue/six.txt", 806).await;
    approve(&t, &reviewer_token, &cr6).await;
    land_change(&t, &cr6)
        .await
        .expect("an evaluate-mode ruleset does not force the queue");

    // Active again, but on refs other than main: main lands directly.
    let narrowed = t
        .call(
            "PolicyService",
            "UpdateRuleset",
            json!({
                "rulesetId": id, "expectedVersion": upd["version"],
                "enforcement": "RULESET_ENFORCEMENT_ACTIVE",
                "conditions": { "refNameIncludes": ["release/*"] },
            }),
        )
        .await
        .expect("narrow to release branches");
    let cr7 = open_change(&t, "queue/seven.txt", 807).await;
    approve(&t, &reviewer_token, &cr7).await;
    land_change(&t, &cr7)
        .await
        .expect("a ruleset that does not match the target ref does not force the queue");

    // Disabled rulesets are ignored too.
    t.call(
        "PolicyService",
        "UpdateRuleset",
        json!({
            "rulesetId": id, "expectedVersion": narrowed["version"],
            "enforcement": "RULESET_ENFORCEMENT_DISABLED",
        }),
    )
    .await
    .expect("disable ruleset");
    let cr8 = open_change(&t, "queue/eight.txt", 808).await;
    approve(&t, &reviewer_token, &cr8).await;
    land_change(&t, &cr8)
        .await
        .expect("a disabled ruleset does not force the queue");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_queue_parameters_the_queue_cannot_honour_are_refused_by_name() {
    let t = start_default().await;
    for (rule, field) in [
        (json!({ "minEntriesToMerge": 2 }), "min_entries_to_merge"),
        (
            json!({ "checkTimeoutSeconds": 600 }),
            "check_timeout_seconds",
        ),
        (
            json!({ "mergeMethod": "RULESET_MERGE_METHOD_SQUASH" }),
            "merge_method",
        ),
    ] {
        let e = t
            .call(
                "PolicyService",
                "CreateRuleset",
                json!({ "repo": repo(), "name": "q", "rules": [{ "mergeQueue": rule }] }),
            )
            .await
            .expect_err("refused");
        assert_eq!(reason(&e), "INVALID_ARGUMENT", "{e:?}");
        assert_eq!(e.subcode, "unsupported_rule");
        assert!(e.message.contains(field), "{field}: {}", e.message);
    }
    // Nothing was stored.
    let list = t
        .call("PolicyService", "ListRulesets", json!({ "repo": repo() }))
        .await
        .expect("list");
    assert!(
        list["rulesets"].as_array().is_none_or(|r| r.is_empty()),
        "{list}"
    );
    for (rule, field) in [
        (json!({ "maxEntriesToBuild": 3 }), "max_entries_to_build"),
        (
            json!({ "minEntriesWaitSeconds": 60 }),
            "min_entries_wait_seconds",
        ),
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
            .expect_err("refused");
        assert_eq!(reason(&e), "INVALID_ARGUMENT", "{e:?}");
        assert_eq!(e.subcode, "unsupported_rule");
        assert!(e.message.contains(field), "{field}: {}", e.message);
    }
}
