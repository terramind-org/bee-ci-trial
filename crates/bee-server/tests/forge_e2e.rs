//! End to end through the real server (dev mode: SQLite + disk; Postgres when
//! `BEE_TEST_PG` is set): the forge wired to the VCS, over HTTP/JSON and gRPC.

mod common;

use base64::Engine as _;
use bee_proto::v1;
use common::*;
use serde_json::{Value, json};

fn b64(s: &str) -> String {
    B64.encode(s.as_bytes())
}

async fn org_member(t: &TestServer, principal: &str, role: &str) {
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": principal, "role": role }),
    )
    .await
    .expect("set member");
}

/// The full change-request path: open → review approve → check passes → auto-merge lands
/// through the coalescer → closing keyword closes the issue → the lease tied to the change
/// is released → the reviewer has an inbox thread → a release's generated notes list the CR.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn change_request_lands_through_the_coalescer_and_closes_the_loop() {
    let t = start_default().await;
    let (reviewer, rtok) = t.principal("human", "reviewer", "").await;
    org_member(&t, &reviewer, "ROLE_ADMIN").await;
    // main needs one approval and a passing `ci/test`.
    t.land_policy(
        "[[land]]\nref = \"main\"\nrequired_checks = [\"ci/test\"]\n[[land.approvals]]\ncount = 1\n",
        1,
    )
    .await;

    let issue = t
        .call(
            "IssueService",
            "CreateIssue",
            json!({ "repo": repo(), "title": "Login redirect is broken", "body": "steps" }),
        )
        .await
        .expect("issue");
    let issue_no: u64 = issue["number"].as_str().unwrap().parse().unwrap();

    // A lease on src/ tied to the change's ChangeId: released when the CR lands.
    let lease = t
        .call(
            "LeaseService",
            "AcquireLease",
            json!({
                "repo": repo(), "ref": "main", "paths": [b64("src")],
                "mode": "LEASE_MODE_EXCLUSIVE", "changeId": change_id(40), "note": "fixing",
            }),
        )
        .await
        .expect("lease");
    assert!(lease["lease"]["id"].is_string(), "{lease}");

    let (tip, v0) = t.tip("main").await;
    let head = id_text(
        &t.commit(
            &tip,
            &[("src/login.rs", b"fn redirect() {}\n")],
            40,
            &format!("Fix the login redirect\n\nFixes #{issue_no}"),
        )
        .await["id"],
    );
    let cr = t
        .call(
            "ChangeService",
            "OpenChange",
            json!({
                "repo": repo(), "head": id_json(&head), "title": "Fix the login redirect",
                "body": format!("Fixes #{issue_no}"), "reviewerIds": [reviewer],
            }),
        )
        .await
        .expect("open");
    let cr_id = cr["id"].as_str().unwrap().to_string();
    assert_eq!(cr["number"].as_str().unwrap(), (issue_no + 1).to_string());

    let eval = t
        .call(
            "ChangeService",
            "EvaluateLanding",
            json!({ "repo": repo(), "changeRequestId": cr_id }),
        )
        .await
        .expect("evaluate");
    assert_eq!(eval["allowed"], Value::Null, "{eval}");
    let missing: Vec<String> = eval["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap().to_string())
        .collect();
    assert!(
        missing.contains(&"check:ci/test".to_string()),
        "{missing:?}"
    );
    assert!(
        missing.iter().any(|m| m.starts_with("approval")),
        "{missing:?}"
    );

    // Landing now is refused by the server's own policy too.
    let e = t
        .call(
            "ChangeService",
            "LandChange",
            json!({ "repo": repo(), "id": cr_id }),
        )
        .await
        .expect_err("not yet");
    assert_eq!(reason(&e), "POLICY_DENIED");

    t.call(
        "ChangeService",
        "EnableAutoMerge",
        json!({ "repo": repo(), "id": cr_id, "method": "LAND_METHOD_REBASE" }),
    )
    .await
    .expect("auto-merge");

    // The review arrives over gRPC.
    let mut ch = v1::change_service_client::ChangeServiceClient::new(t.grpc_channel().await);
    ch.submit_review(t.bearer(
        &rtok,
        v1::SubmitReviewRequest {
            repo: prepo(),
            change_request_id: cr_id.clone(),
            revision: 1,
            verdict: v1::Verdict::Approve as i32,
            body: "looks right".into(),
            comments: Vec::new(),
        },
    ))
    .await
    .expect("review over gRPC");
    let still = t
        .call(
            "ChangeService",
            "GetChange",
            json!({ "repo": repo(), "id": cr_id }),
        )
        .await
        .unwrap();
    assert_eq!(
        still["state"], "CHANGE_STATE_OPEN",
        "the check is still missing"
    );

    // CI reports success on the head: auto-merge lands through the coalescer.
    t.call(
        "CheckService",
        "ReportCheck",
        json!({
            "repo": repo(), "changeset": id_json(&head), "name": "ci/test",
            "status": "CHECK_STATUS_PASSED", "summary": "green",
        }),
    )
    .await
    .expect("check");
    let landed = eventually(30, "auto-merge to land", || async {
        let c = t
            .call(
                "ChangeService",
                "GetChange",
                json!({ "repo": repo(), "id": cr_id }),
            )
            .await
            .ok()?;
        (c["state"] == "CHANGE_STATE_LANDED").then_some(c)
    })
    .await;
    assert_eq!(landed["landedRef"]["name"], "main");
    let (tip2, v2) = t.tip("main").await;
    assert_eq!(v2, v0 + 1, "one coalesced land");
    assert_eq!(
        t.read("src/login.rs", &tip2).await.unwrap(),
        b"fn redirect() {}\n"
    );
    assert!(
        t.srv
            .state
            .metrics
            .render()
            .contains("bee_land_batches_total"),
        "landed through the coalescer"
    );

    // The closing keyword closed the issue.
    eventually(30, "issue to close", || async {
        let i = t
            .call(
                "IssueService",
                "GetIssueByNumber",
                json!({ "repo": repo(), "number": issue_no }),
            )
            .await
            .ok()?;
        (i["state"] == "ISSUE_STATE_CLOSED").then_some(())
    })
    .await;

    // cr.landed released the lease.
    eventually(30, "lease to be released", || async {
        let l = t
            .call("LeaseService", "ListLeases", json!({ "repo": repo() }))
            .await
            .ok()?;
        l["leases"]
            .as_array()
            .is_none_or(|a| a.is_empty())
            .then_some(())
    })
    .await;

    // The reviewer's inbox has the review request.
    eventually(30, "a notification for the reviewer", || async {
        let n = t
            .call_as(
                &rtok,
                "NotificationService",
                "ListNotificationThreads",
                json!({ "filter": { "includeDone": true } }),
            )
            .await
            .ok()?;
        n["threads"]
            .as_array()?
            .iter()
            .any(|th| th["subject"]["id"] == cr_id.as_str())
            .then_some(())
    })
    .await;

    // A release with generated notes lists the change request.
    let rel = t
        .call(
            "ReleaseService",
            "CreateRelease",
            json!({
                "repo": repo(), "tagName": "v1.0.0", "name": "v1.0.0", "generateNotes": true,
            }),
        )
        .await
        .expect("release");
    let body = rel["body"].as_str().unwrap_or_default();
    assert!(
        body.contains("Fix the login redirect") && body.contains(&format!("#{}", issue_no + 1)),
        "notes: {body}"
    );
    // The tag is a VCS ref.
    let refs = t
        .call(
            "RefService",
            "ListRefs",
            json!({ "repo": repo(), "prefix": "tags/" }),
        )
        .await
        .unwrap();
    assert!(refs.to_string().contains("tags/v1.0.0"), "{refs}");
}

/// Assigning an issue to an agent emits `issue.assigned_to_agent` into the repo's event log,
/// which also carries the VCS `ref.updated` events (one per-repo log for forge and VCS).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_assignment_and_vcs_events_share_one_repo_log() {
    let t = start_default().await;
    let agent = t
        .call(
            "AuthService",
            "CreateAgent",
            json!({ "display": "fixer", "runtime": "claude-code", "purpose": "fix bugs", "org": "dev" }),
        )
        .await
        .expect("agent");
    let agent_id = agent["id"].as_str().unwrap().to_string();
    // Agents take work in the org like members.
    org_member(&t, &agent_id, "ROLE_MEMBER").await;
    let issue = t
        .call(
            "IssueService",
            "CreateIssue",
            json!({ "repo": repo(), "title": "Flaky test", "body": "fails 1 in 10" }),
        )
        .await
        .unwrap();
    let n: u64 = issue["number"].as_str().unwrap().parse().unwrap();
    t.call(
        "IssueService",
        "AddIssueAssignees",
        json!({ "repo": repo(), "number": n, "assignees": [agent_id], "agentInstructions": "fix it" }),
    )
    .await
    .expect("assign");
    // A VCS land in the same repo.
    let (tip, _) = t.tip("main").await;
    let c = id_text(&t.commit(&tip, &[("a.txt", b"a\n")], 7, "a").await["id"]);
    t.land(&c).await.unwrap();

    // Subscribe over gRPC from the start of the log.
    let mut ev = v1::event_service_client::EventServiceClient::new(t.grpc_channel().await);
    let (mut assigned, mut ref_updated) = (None, false);
    for _ in 0..100 {
        let mut stream = ev
            .subscribe(t.bearer(
                &t.token,
                v1::SubscribeRequest {
                    repo: prepo(),
                    from_seq: 0,
                    kinds: vec!["issue.*".into(), "ref.updated".into()],
                    org: String::new(),
                },
            ))
            .await
            .expect("subscribe")
            .into_inner();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        while let Ok(Ok(Some(e))) = tokio::time::timeout_at(deadline, stream.message()).await {
            if e.kind == "issue.assigned_to_agent" {
                assigned = Some(e.clone());
            }
            if e.kind == "ref.updated"
                && matches!(&e.body, Some(v1::event::Body::RefUpdated(r)) if r.name == "main")
            {
                ref_updated = true;
            }
        }
        if assigned.is_some() && ref_updated {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let e = assigned.expect("issue.assigned_to_agent in the repo log");
    let p: Value = serde_json::from_str(&e.payload_json).unwrap();
    assert_eq!(
        p["agent"]["id"].as_str().or(p["agent"].as_str()),
        Some(agent_id.as_str()),
        "{p}"
    );
    assert!(ref_updated, "VCS ref.updated bridged into the same log");
}

/// Assigning an issue to an agent over HTTP/JSON starts a run executed by the configured
/// command runner, whose only credential is a run token whose chain ends at the human.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn assigning_an_issue_to_an_agent_runs_it_with_a_token_ending_at_the_human() {
    let out = tempfile::tempdir().unwrap();
    let token_file = out.path().join("token");
    let cmd = format!(
        "printf %s \"$BEE_TOKEN\" > '{}'; cat > /dev/null; echo done",
        token_file.display()
    );
    let t = start_with(move |c| {
        c.forge.agent_runner = "command".into();
        c.forge.agent_runner_command = cmd;
    })
    .await;
    let dev = t.dev_user();
    let agent = t
        .call(
            "AuthService",
            "CreateAgent",
            json!({ "display": "fixer", "runtime": "claude-code", "purpose": "fix bugs", "org": "dev" }),
        )
        .await
        .expect("agent");
    let agent_id = agent["id"].as_str().unwrap().to_string();
    org_member(&t, &agent_id, "ROLE_MEMBER").await;
    // Agent definitions have no RPC yet (docs/spec/agents.md §9): define it in process.
    let w = t.srv.forge.as_ref().expect("forge");
    w.agents
        .define_agent(
            &bee_forge::Cx::new(dev.clone(), v1::PrincipalKind::Human),
            bee_forge::agents::AgentDefinition {
                org_id: t.srv.dev.as_ref().unwrap().org.0.to_text(),
                agent: v1::Principal {
                    id: agent_id.clone(),
                    kind: v1::PrincipalKind::Agent as i32,
                    display: "fixer".into(),
                    ..Default::default()
                },
                name: "fixer".into(),
                runtime: "claude-code".into(),
                toolsets: ["read", "commit", "lease", "land"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                owners: vec![dev.clone()],
                purpose: "fix bugs".into(),
                enabled: true,
                ..Default::default()
            },
        )
        .await
        .expect("define agent");
    let issue = t
        .call(
            "IssueService",
            "CreateIssue",
            json!({ "repo": repo(), "title": "Typo in README", "body": "fix it" }),
        )
        .await
        .unwrap();
    let n: u64 = issue["number"].as_str().unwrap().parse().unwrap();
    t.call(
        "IssueService",
        "AddIssueAssignees",
        json!({ "repo": repo(), "number": n, "assignees": [agent_id] }),
    )
    .await
    .expect("assign to the agent");
    let run = eventually(60, "the agent run", || async {
        let l = t
            .call(
                "AgentService",
                "ListAgentRuns",
                json!({ "repo": repo(), "agentPrincipalId": agent_id }),
            )
            .await
            .ok()?;
        l["runs"].as_array()?.first().cloned()
    })
    .await;
    assert_eq!(run["agent"]["id"], agent_id.as_str(), "{run}");
    let token = eventually(60, "the runner to receive its token", || async {
        std::fs::read_to_string(&token_file)
            .ok()
            .filter(|s| !s.is_empty())
    })
    .await;
    let v = bee_auth::Verifier::new(
        &t.srv.state.ring.read().unwrap().verifying_keys(),
        &bee_auth::InMemoryRevocations::new(),
    )
    .verify(&token, t.srv.state.now_ms() / 1000)
    .expect("a verifiable run token");
    let chain: Vec<String> = v.chain().into_iter().map(|p| p.id).collect();
    assert_eq!(
        chain.first(),
        Some(&agent_id),
        "acts as the agent: {chain:?}"
    );
    assert_eq!(chain.last(), Some(&dev), "ends at the human: {chain:?}");
    // The run finishes once the command exits; its token is then revoked.
    eventually(60, "the run to finish", || async {
        let r = t
            .call("AgentService", "GetAgentRun", json!({ "id": run["id"] }))
            .await
            .ok()?;
        matches!(
            r["state"].as_str(),
            Some("AGENT_RUN_STATE_SUCCEEDED") | Some("AGENT_RUN_STATE_WAITING_FOR_REVIEW")
        )
        .then_some(())
    })
    .await;
    eventually(30, "the run token to be revoked", || async {
        t.call_as(&token, "RefService", "ListRefs", json!({ "repo": repo() }))
            .await
            .err()
            .filter(|e| e.reason == "TOKEN_REVOKED")
            .map(|_| ())
    })
    .await;
}
