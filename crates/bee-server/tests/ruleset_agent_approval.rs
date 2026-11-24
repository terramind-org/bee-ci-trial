//! Rulesets created through `PolicyService` add no implicit human approval (SPEC §9.4): agents
//! land under them on their own, and humans are required only by a required-approvals rule's
//! `humanCount`. Mirrors the setups of scenarios AS-CR-01 and AS-GIT-01.

#![allow(clippy::result_large_err)]

mod common;

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

/// An org member of `kind` with write on the repo, and a token for it. Agents are created and their tokens minted
/// through `AuthService`, as the scenarios do, so each acts on behalf of the dev user.
async fn member(t: &TestServer, kind: &str, display: &str) -> (String, String) {
    let (id, token) = if kind == "agent" {
        let agent = t
            .call(
                "AuthService",
                "CreateAgent",
                json!({ "display": display, "runtime": "claude-code", "purpose": "tests", "org": "dev" }),
            )
            .await
            .expect("agent");
        let id = agent["id"].as_str().unwrap().to_string();
        let minted = t
            .call("AuthService", "MintToken", json!({ "principalId": id }))
            .await
            .expect("agent token");
        (id, minted["token"].as_str().unwrap().to_string())
    } else {
        t.principal(kind, display, "").await
    };
    org_member(t, &id).await;
    t.call(
        "RepoService",
        "AddCollaborator",
        json!({ "repo": repo(), "principalId": id, "role": "REPO_ROLE_WRITE" }),
    )
    .await
    .expect("collaborator");
    (id, token)
}

/// An active ruleset on the default branch with `rules`.
async fn ruleset(t: &TestServer, name: &str, rules: Value) {
    t.call(
        "PolicyService",
        "CreateRuleset",
        json!({
            "repo": repo(), "name": name,
            "target": "RULESET_TARGET_BRANCH",
            "enforcement": "RULESET_ENFORCEMENT_ACTIVE",
            "conditions": { "refNameIncludes": ["~DEFAULT_BRANCH"] },
            "rules": rules,
        }),
    )
    .await
    .expect("create ruleset");
}

/// A changeset by `token` putting `files` on main's tip, opened as a change request asking
/// `reviewer` for review; returns the CR id.
async fn open_cr(
    t: &TestServer,
    token: &str,
    files: &[(&str, &[u8])],
    cid: u64,
    reviewer: &str,
) -> String {
    let (tip, _) = t.tip("main").await;
    let head = id_text(
        &t.commit_as(token, &tip, files, cid, "change")
            .await
            .expect("commit")["id"],
    );
    let cr = t
        .call_as(
            token,
            "ChangeService",
            "OpenChange",
            json!({ "repo": repo(), "head": id_json(&head), "title": format!("change {cid}"),
                    "reviewerIds": [reviewer] }),
        )
        .await
        .expect("open");
    cr["id"].as_str().unwrap().to_string()
}

async fn approve(t: &TestServer, token: &str, cr: &str) {
    t.call_as(
        token,
        "ChangeService",
        "SubmitReview",
        json!({ "repo": repo(), "changeRequestId": cr, "revision": 1,
                "verdict": "VERDICT_APPROVE", "body": "ship it" }),
    )
    .await
    .expect("approve");
}

async fn missing(t: &TestServer, cr: &str) -> Vec<String> {
    let eval = t
        .call(
            "ChangeService",
            "EvaluateLanding",
            json!({ "repo": repo(), "changeRequestId": cr }),
        )
        .await
        .expect("evaluate");
    eval["missing"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|m| m.as_str().unwrap().to_string())
        .collect()
}

async fn land_cr(
    t: &TestServer,
    token: &str,
    cr: &str,
) -> Result<Value, bee_proto::v1::ErrorDetail> {
    t.call_as(
        token,
        "ChangeService",
        "LandChange",
        json!({ "repo": repo(), "id": cr }),
    )
    .await
}

async fn state(t: &TestServer, cr: &str) -> String {
    let c = t
        .call(
            "ChangeService",
            "GetChange",
            json!({ "repo": repo(), "id": cr }),
        )
        .await
        .expect("get change");
    c["state"].as_str().unwrap_or("").to_string()
}

/// AS-CR-01's ruleset: one approval, agents may approve, the last push needs someone else's
/// approval. A second agent's approval lands an agent's CR, by hand and through auto-merge
/// enabled before the approval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_approval_satisfies_a_ruleset_that_lets_agents_approve() {
    let t = start_default().await;
    let (_, author) = member(&t, "agent", "author").await;
    let (reviewer_id, reviewer) = member(&t, "agent", "reviewer").await;
    ruleset(
        &t,
        "review-required",
        json!([{ "requiredApprovals": { "count": 1, "agentsMayApprove": true, "requireLastPushApproval": true } }]),
    )
    .await;

    // Landed by hand once the reviewer agent approves.
    let cr = open_cr(
        &t,
        &author,
        &[("src/units.py", b"def f():\n    pass\n")],
        1,
        &reviewer_id,
    )
    .await;
    let before = missing(&t, &cr).await;
    assert!(
        before.iter().any(|m| m.starts_with("approval")),
        "{before:?}"
    );
    assert!(
        !before.contains(&"approval:human".to_string()),
        "{before:?}"
    );
    let e = land_cr(&t, &author, &cr)
        .await
        .expect_err("needs the review");
    assert_eq!(reason(&e), "POLICY_DENIED");
    approve(&t, &reviewer, &cr).await;
    assert_eq!(missing(&t, &cr).await, Vec::<String>::new());
    land_cr(&t, &author, &cr)
        .await
        .expect("an agent approval lands it");
    assert_eq!(state(&t, &cr).await, "CHANGE_STATE_LANDED");

    // Landed by auto-merge, enabled before the approval.
    let cr = open_cr(
        &t,
        &author,
        &[("src/units.py", b"def g():\n    pass\n")],
        2,
        &reviewer_id,
    )
    .await;
    t.call_as(
        &author,
        "ChangeService",
        "EnableAutoMerge",
        json!({ "repo": repo(), "id": cr, "method": "LAND_METHOD_REBASE" }),
    )
    .await
    .expect("auto-merge");
    assert_eq!(state(&t, &cr).await, "CHANGE_STATE_OPEN");
    approve(&t, &reviewer, &cr).await;
    eventually(30, "auto-merge to land", || async {
        (state(&t, &cr).await == "CHANGE_STATE_LANDED").then_some(())
    })
    .await;
    let (tip, _) = t.tip("main").await;
    assert_eq!(
        t.read("src/units.py", &tip).await.unwrap(),
        b"def g():\n    pass\n"
    );
}

/// `humanCount` is the only way a ruleset asks for a human: agents fill the rest of `count`
/// but never the human part.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn human_count_requires_a_human_on_top_of_agent_approvals() {
    let t = start_default().await;
    let (_, author) = member(&t, "agent", "author").await;
    let (reviewer_id, reviewer) = member(&t, "agent", "reviewer").await;
    let (_, second) = member(&t, "agent", "second").await;
    let (_, human) = member(&t, "human", "maintainer").await;
    ruleset(
        &t,
        "one-human",
        json!([{ "requiredApprovals": { "count": 2, "humanCount": 1, "agentsMayApprove": true } }]),
    )
    .await;

    let cr = open_cr(
        &t,
        &author,
        &[("src/lib.rs", b"pub fn x() {}\n")],
        1,
        &reviewer_id,
    )
    .await;
    approve(&t, &reviewer, &cr).await;
    approve(&t, &second, &cr).await;
    let m = missing(&t, &cr).await;
    assert!(
        m.iter().any(|m| m.starts_with("approval")),
        "two agents are not a human: {m:?}"
    );
    let e = land_cr(&t, &author, &cr).await.expect_err("needs a human");
    assert_eq!(reason(&e), "POLICY_DENIED");
    approve(&t, &human, &cr).await;
    assert_eq!(missing(&t, &cr).await, Vec::<String>::new());
    land_cr(&t, &author, &cr)
        .await
        .expect("a human approval lands it");
    assert_eq!(state(&t, &cr).await, "CHANGE_STATE_LANDED");

    // One agent and one human make the two approvals.
    let cr = open_cr(
        &t,
        &author,
        &[("src/lib.rs", b"pub fn y() {}\n")],
        2,
        &reviewer_id,
    )
    .await;
    approve(&t, &reviewer, &cr).await;
    let e = land_cr(&t, &author, &cr)
        .await
        .expect_err("an agent alone is not enough");
    assert_eq!(reason(&e), "POLICY_DENIED");
    approve(&t, &human, &cr).await;
    land_cr(&t, &author, &cr)
        .await
        .expect("agent plus human lands it");
}

/// `humanCount` and the total count are separate approval requirements. Two agent approvals
/// leave both the one-human and three-total requirements missing, so clients must receive
/// distinct stable ids for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn human_and_total_approval_requirements_have_distinct_ids() {
    let t = start_default().await;
    let (_, author) = member(&t, "agent", "author").await;
    let (reviewer_id, reviewer) = member(&t, "agent", "reviewer").await;
    let (_, second) = member(&t, "agent", "second").await;
    ruleset(
        &t,
        "three-with-one-human",
        json!([{ "requiredApprovals": { "count": 3, "humanCount": 1, "agentsMayApprove": true } }]),
    )
    .await;

    let cr = open_cr(
        &t,
        &author,
        &[("src/lib.rs", b"pub fn x() {}\n")],
        1,
        &reviewer_id,
    )
    .await;
    approve(&t, &reviewer, &cr).await;
    approve(&t, &second, &cr).await;

    assert_eq!(
        missing(&t, &cr).await,
        vec!["approval:any:human", "approval:any"],
        "0 of 1 human approvals and 2 of 3 total approvals are independently missing"
    );
}

/// AS-GIT-01's ruleset: only `secrets/**` is restricted, so an agent lands anything else on
/// main without anyone's approval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restricted_paths_ruleset_lets_agents_land_elsewhere() {
    let t = start_default().await;
    let (tip, _) = t.tip("main").await;
    let seed = id_text(
        &t.commit(
            &tip,
            &[("secrets/README.md", b"never commit here\n")],
            1,
            "seed",
        )
        .await["id"],
    );
    t.land(&seed).await.expect("seed lands");
    ruleset(
        &t,
        "protect-secrets",
        json!([{ "restrictedPaths": { "pathGlobs": ["secrets/**"] } }]),
    )
    .await;
    let (_, agent) = member(&t, "agent", "contributor").await;

    let (tip, v) = t.tip("main").await;
    let ok = id_text(
        &t.commit_as(
            &agent,
            &tip,
            &[("CONTRIBUTING.md", b"Run the tests before pushing.\n")],
            2,
            "contributing",
        )
        .await
        .expect("commit")["id"],
    );
    t.land_as(&agent, &ok)
        .await
        .expect("the agent lands outside secrets/");
    let (tip, v2) = t.tip("main").await;
    assert_eq!(v2, v + 1);
    assert_eq!(
        t.read("CONTRIBUTING.md", &tip).await.unwrap(),
        b"Run the tests before pushing.\n"
    );

    let bad = id_text(
        &t.commit_as(
            &agent,
            &tip,
            &[("secrets/notes.txt", b"rotation schedule\n")],
            3,
            "key",
        )
        .await
        .expect("commit")["id"],
    );
    let e = t
        .land_as(&agent, &bad)
        .await
        .expect_err("secrets/ is restricted");
    assert_eq!(reason(&e), "POLICY_DENIED");
    assert_eq!(t.tip("main").await.0, tip);
}
