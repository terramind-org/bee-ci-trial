//! The API gaps the forge domains reported, closed over HTTP/JSON on the real server: each
//! test drives RPCs that used to exist only as Rust domain methods (so agents, the CLI and
//! MCP could not reach them).

mod common;

use base64::Engine as _;
use common::*;
use serde_json::{Value, json};

async fn org_member(t: &TestServer, principal: &str, role: &str) {
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": principal, "role": role }),
    )
    .await
    .expect("set member");
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// Agent definitions, owner budgets, run provenance fields, sessions by change, the activity
/// stream by delegation root and the watcher's agent field (docs/spec/agents.md §9).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_definitions_budgets_sessions_and_activity_are_rpcs() {
    let t = start_with(|c| c.forge.agent_runner = "fake".into()).await;
    let dev = t.dev_user();
    let (bob, bob_tok) = t.principal("human", "bob", "").await;
    org_member(&t, &bob, "ROLE_MEMBER").await;
    let repo_row = t
        .call("RepoService", "GetRepo", json!({ "repo": repo() }))
        .await
        .expect("repo");
    let repo_id = s(&repo_row["id"]).to_string();
    let agent = t
        .call(
            "AuthService",
            "CreateAgent",
            json!({ "display": "helper", "runtime": "claude-code", "org": "dev" }),
        )
        .await
        .expect("agent");
    let agent_id = s(&agent["id"]).to_string();
    org_member(&t, &agent_id, "ROLE_MEMBER").await;

    let def = t
        .call(
            "AgentService",
            "RegisterAgent",
            json!({
                "org": "dev", "agentPrincipalId": agent_id, "name": "helper",
                "runtime": "claude-code", "model": "model-a",
                "instructionsPath": "docs/AGENT.md",
                "toolsets": ["read", "commit", "lease"],
                "defaultBudget": { "maxUsdMicros": "5000000" },
                "schedules": [{ "repoId": repo_id, "cron": "0 3 * * *", "prompt": "nightly" }],
                "purpose": "chores"
            }),
        )
        .await
        .expect("register agent");
    assert_eq!(def["name"], "helper", "{def}");
    assert_eq!(def["enabled"], true);
    assert_eq!(def["owners"][0], dev.as_str());
    assert_eq!(def["schedules"][0]["cron"], "0 3 * * *");
    assert!(!s(&def["schedules"][0]["id"]).is_empty());
    let again = t
        .call(
            "AgentService",
            "RegisterAgent",
            json!({ "org": "dev", "agentPrincipalId": agent_id, "name": "other" }),
        )
        .await
        .expect_err("defined twice");
    assert_eq!(again.reason, "ALREADY_EXISTS", "{again:?}");
    let got = t
        .call(
            "AgentService",
            "GetAgent",
            json!({ "org": "dev", "agentPrincipalId": agent_id }),
        )
        .await
        .expect("get agent");
    assert_eq!(got["model"], "model-a");
    let mine = t
        .call(
            "AgentService",
            "ListAgents",
            json!({ "org": "dev", "ownerId": dev }),
        )
        .await
        .expect("list agents");
    assert_eq!(mine["agents"].as_array().map(Vec::len), Some(1), "{mine}");
    let none = t
        .call(
            "AgentService",
            "ListAgents",
            json!({ "org": "dev", "ownerId": bob }),
        )
        .await
        .expect("list agents");
    assert!(none["agents"].as_array().is_none_or(|a| a.is_empty()));
    let updated = t
        .call(
            "AgentService",
            "UpdateAgent",
            json!({ "org": "dev", "agentPrincipalId": agent_id, "model": "model-b",
                    "replaceSchedules": true }),
        )
        .await
        .expect("update agent");
    assert_eq!(updated["model"], "model-b");
    assert!(updated["schedules"].as_array().is_none_or(|a| a.is_empty()));
    assert_eq!(updated["toolsets"].as_array().map(Vec::len), Some(3));
    let denied = t
        .call_as(
            &bob_tok,
            "AgentService",
            "UpdateAgent",
            json!({ "org": "dev", "agentPrincipalId": agent_id, "model": "x" }),
        )
        .await
        .expect_err("bob does not own the agent");
    assert_eq!(denied.reason, "PERMISSION_DENIED", "{denied:?}");

    // Owner budgets: org admins set them; owners read their own.
    let budget = t
        .call(
            "AgentService",
            "SetAgentBudget",
            json!({ "org": "dev", "ownerId": dev, "maxRuns": "10" }),
        )
        .await
        .expect("set budget");
    assert_eq!(budget["maxRuns"], "10", "{budget}");
    let denied = t
        .call_as(
            &bob_tok,
            "AgentService",
            "SetAgentBudget",
            json!({ "org": "dev", "ownerId": bob, "maxRuns": "1000" }),
        )
        .await
        .expect_err("members cannot set budgets");
    assert_eq!(denied.reason, "PERMISSION_DENIED");
    let denied = t
        .call_as(
            &bob_tok,
            "AgentService",
            "GetAgentBudget",
            json!({ "org": "dev", "ownerId": dev }),
        )
        .await
        .expect_err("another owner's budget");
    assert_eq!(denied.reason, "PERMISSION_DENIED");

    // A run carries its trigger and, once finished, its transcript and log references.
    let run = t
        .call(
            "AgentService",
            "StartAgentRun",
            json!({ "repo": repo(), "agentPrincipalId": agent_id,
                    "task": { "kind": "AGENT_TASK_KIND_FREEFORM", "prompt": "tidy up" } }),
        )
        .await
        .expect("start run");
    assert_eq!(run["trigger"], "manual", "{run}");
    let done = eventually(60, "the run to finish", || async {
        let r = t
            .call("AgentService", "GetAgentRun", json!({ "id": run["id"] }))
            .await
            .ok()?;
        (!s(&r["transcriptRef"]).is_empty()).then_some(r)
    })
    .await;
    assert!(!s(&done["finishedAt"]).is_empty(), "{done}");
    let spent = t
        .call("AgentService", "GetAgentBudget", json!({ "org": "dev" }))
        .await
        .expect("own budget");
    assert_eq!(spent["runsStarted"], "1", "{spent}");

    // Which session produced the run's changeset.
    let cs = done["changesets"][0].clone();
    assert!(cs.is_object(), "the fake runner commits: {done}");
    let sessions = t
        .call(
            "AgentService",
            "GetSessionsForChange",
            json!({ "repo": repo(), "changeset": cs }),
        )
        .await
        .expect("sessions");
    assert_eq!(sessions["sessions"][0]["runId"], run["id"], "{sessions}");
    assert_eq!(
        sessions["sessions"][0]["agentPrincipalId"],
        agent_id.as_str()
    );
    let chain = sessions["sessions"][0]["chain"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(chain.last().and_then(Value::as_str), Some(dev.as_str()));
    let missing = t
        .call(
            "AgentService",
            "GetSessionsForChange",
            json!({ "repo": repo() }),
        )
        .await
        .expect_err("needs a subject");
    assert_eq!(missing.reason, "INVALID_ARGUMENT");

    // The activity stream, filtered by delegation root.
    let mine = eventually(30, "agent activity", || async {
        let a = t
            .call(
                "AgentService",
                "ListAgentActivity",
                json!({ "org": "dev", "agentPrincipalId": agent_id, "delegationRoot": dev }),
            )
            .await
            .ok()?;
        a["events"].as_array().filter(|e| !e.is_empty()).cloned()
    })
    .await;
    assert!(
        mine.iter().all(|e| e["principal"] == agent_id.as_str()),
        "{mine:?}"
    );
    let bobs = t
        .call(
            "AgentService",
            "ListAgentActivity",
            json!({ "org": "dev", "agentPrincipalId": agent_id, "delegationRoot": bob }),
        )
        .await
        .expect("activity");
    assert!(
        bobs["events"].as_array().is_none_or(|e| e.is_empty()),
        "{bobs}"
    );

    // A watcher names its agent.
    let w = t
        .call(
            "AgentService",
            "CreateComponentWatcher",
            json!({ "repo": repo(), "name": "docs", "component": "docs",
                    "paths": [B64.encode("docs/")], "agentPrincipalId": agent_id }),
        )
        .await
        .expect("watcher");
    assert_eq!(w["agent"]["id"], agent_id.as_str(), "{w}");
    let unknown = t
        .call(
            "AgentService",
            "CreateComponentWatcher",
            json!({ "repo": repo(), "name": "x", "component": "x",
                    "paths": [B64.encode("x/")], "agentPrincipalId": bob }),
        )
        .await
        .expect_err("bob is not an agent here");
    assert_eq!(unknown.reason, "INVALID_ARGUMENT");

    t.call(
        "AgentService",
        "DeleteAgent",
        json!({ "org": "dev", "agentPrincipalId": agent_id }),
    )
    .await
    .expect("delete agent");
    let gone = t
        .call(
            "AgentService",
            "GetAgent",
            json!({ "org": "dev", "agentPrincipalId": agent_id }),
        )
        .await
        .expect_err("deleted");
    assert_eq!(gone.reason, "NOT_FOUND");
}

fn paths_of(diff: &Value) -> Vec<String> {
    diff["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|i| String::from_utf8(B64.decode(s(&i["path"])).unwrap()).unwrap())
        .collect()
}

/// Change request reactions and locks, comment anchors, closing issues, revision pushers,
/// the landing rule and reason, revision diffs by path and whitespace, and the merge-queue
/// land method (docs/spec/forge-changes.md §10).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn change_reactions_locks_anchors_and_diff_filters_are_served() {
    let t = start_default().await;
    let dev = t.dev_user();
    let (bob, bob_tok) = t.principal("human", "bob", "").await;
    org_member(&t, &bob, "ROLE_MEMBER").await;
    let issue = t
        .call(
            "IssueService",
            "CreateIssue",
            json!({ "repo": repo(), "title": "Tidy", "body": "x" }),
        )
        .await
        .expect("issue");
    let n = s(&issue["number"]).to_string();
    let (tip, _) = t.tip("main").await;
    let r1 = id_text(
        &t.commit(
            &tip,
            &[("src/a.rs", b"fn a() {}\n"), ("docs/b.md", b"# B\n")],
            70,
            &format!("Tidy\n\nFixes #{n}"),
        )
        .await["id"],
    );
    let cr = t
        .call(
            "ChangeService",
            "OpenChange",
            json!({ "repo": repo(), "head": id_json(&r1), "title": "Tidy",
                    "body": format!("Fixes #{n}") }),
        )
        .await
        .expect("open");
    let cr_id = s(&cr["id"]).to_string();
    assert_eq!(cr["closingIssues"][0], n.as_str(), "{cr}");
    assert_eq!(cr["revisions"][0]["pusher"]["id"], dev.as_str(), "{cr}");

    // Reactions on the CR and on a comment.
    let r = t
        .call(
            "ChangeService",
            "AddChangeReaction",
            json!({ "repo": repo(), "changeRequestId": cr_id, "content": "REACTION_CONTENT_ROCKET" }),
        )
        .await
        .expect("react");
    assert_eq!(
        r["reactions"][0]["content"], "REACTION_CONTENT_ROCKET",
        "{r}"
    );
    let c = t
        .call(
            "ChangeService",
            "CreateChangeComment",
            json!({ "repo": repo(), "changeRequestId": cr_id, "body": "why?",
                    "path": B64.encode("src/a.rs"), "side": "new", "startLine": 1, "endLine": 1 }),
        )
        .await
        .expect("line comment");
    assert_eq!(c["anchor"]["path"], B64.encode("src/a.rs"), "{c}");
    assert_eq!(c["anchor"]["startLine"], 1);
    assert!(!s(&c["anchor"]["threadId"]).is_empty(), "{c}");
    let r = t
        .call_as(
            &bob_tok,
            "ChangeService",
            "AddChangeReaction",
            json!({ "repo": repo(), "changeRequestId": cr_id, "commentId": c["id"],
                    "content": "REACTION_CONTENT_EYES" }),
        )
        .await
        .expect("comment reaction");
    assert_eq!(r["reactions"][0]["content"], "REACTION_CONTENT_EYES", "{r}");
    let r = t
        .call(
            "ChangeService",
            "RemoveChangeReaction",
            json!({ "repo": repo(), "changeRequestId": cr_id, "content": "REACTION_CONTENT_ROCKET" }),
        )
        .await
        .expect("unreact");
    assert!(
        r["reactions"].as_array().is_none_or(|a| a.is_empty()),
        "{r}"
    );

    // Changeset comments return their anchor.
    let cc = t
        .call(
            "ChangeService",
            "CreateChangesetComment",
            json!({ "repo": repo(), "changeset": id_json(&r1), "body": "nice",
                    "path": B64.encode("docs/b.md"), "line": 1 }),
        )
        .await
        .expect("changeset comment");
    assert_eq!(cc["anchor"]["path"], B64.encode("docs/b.md"), "{cc}");
    let listed = t
        .call(
            "ChangeService",
            "ListChangesetComments",
            json!({ "repo": repo(), "changeset": id_json(&r1) }),
        )
        .await
        .expect("changeset comments");
    assert_eq!(listed["comments"][0]["anchor"]["startLine"], 1, "{listed}");

    // Locking: triage only; members then cannot comment.
    let denied = t
        .call_as(
            &bob_tok,
            "ChangeService",
            "SetChangeLocked",
            json!({ "repo": repo(), "changeRequestId": cr_id, "locked": true }),
        )
        .await
        .expect_err("members cannot lock");
    assert_eq!(denied.reason, "PERMISSION_DENIED");
    let locked = t
        .call(
            "ChangeService",
            "SetChangeLocked",
            json!({ "repo": repo(), "changeRequestId": cr_id, "locked": true }),
        )
        .await
        .expect("lock");
    assert_eq!(locked["locked"], true);
    let refused = t
        .call_as(
            &bob_tok,
            "ChangeService",
            "CreateChangeComment",
            json!({ "repo": repo(), "changeRequestId": cr_id, "body": "+1" }),
        )
        .await
        .expect_err("locked");
    assert_eq!(refused.reason, "LOCKED", "{refused:?}");
    t.call(
        "ChangeService",
        "SetChangeLocked",
        json!({ "repo": repo(), "changeRequestId": cr_id, "locked": false }),
    )
    .await
    .expect("unlock");

    // Revision 2: a whitespace-only change in src/a.rs, a real one in docs/b.md.
    let r2 = id_text(
        &t.commit(
            &r1,
            &[
                ("src/a.rs", b"fn  a()  {}\n"),
                ("docs/b.md", b"# B\n\nmore\n"),
            ],
            71,
            "More",
        )
        .await["id"],
    );
    t.call(
        "ChangeService",
        "ReviseChange",
        json!({ "repo": repo(), "id": cr_id, "head": id_json(&r2) }),
    )
    .await
    .expect("revise");
    let all = t
        .call(
            "ChangeService",
            "DiffChangeRevisions",
            json!({ "repo": repo(), "changeRequestId": cr_id, "fromRevision": 1, "toRevision": 2 }),
        )
        .await
        .expect("diff");
    assert_eq!(paths_of(&all).len(), 2, "{all}");
    let ws = t
        .call(
            "ChangeService",
            "DiffChangeRevisions",
            json!({ "repo": repo(), "changeRequestId": cr_id, "fromRevision": 1, "toRevision": 2,
                    "ignoreWhitespace": true, "includeHunks": true }),
        )
        .await
        .expect("diff ignoring whitespace");
    assert_eq!(paths_of(&ws), vec!["docs/b.md".to_string()], "{ws}");
    let src = t
        .call(
            "ChangeService",
            "DiffChangeRevisions",
            json!({ "repo": repo(), "changeRequestId": cr_id, "fromRevision": 1, "toRevision": 2,
                    "paths": [B64.encode("src")] }),
        )
        .await
        .expect("diff of src");
    assert_eq!(paths_of(&src), vec!["src/a.rs".to_string()], "{src}");

    // A draft names the reason it cannot land.
    let (tip, _) = t.tip("main").await;
    let d = id_text(&t.commit(&tip, &[("draft.txt", b"d\n")], 72, "Draft").await["id"]);
    let draft = t
        .call(
            "ChangeService",
            "OpenChange",
            json!({ "repo": repo(), "head": id_json(&d), "title": "Draft", "draft": true }),
        )
        .await
        .expect("draft");
    let eval = t
        .call(
            "ChangeService",
            "EvaluateLanding",
            json!({ "repo": repo(), "changeRequestId": draft["id"] }),
        )
        .await
        .expect("evaluate");
    assert_eq!(eval["reason"], "DRAFT", "{eval}");

    // The merge-queue entry carries its land method and delegation chain.
    let entry = t
        .call(
            "ChangeService",
            "EnqueueChange",
            json!({ "repo": repo(), "id": cr_id, "method": "LAND_METHOD_REBASE" }),
        )
        .await
        .expect("enqueue");
    assert_eq!(entry["landMethod"], "LAND_METHOD_REBASE", "{entry}");
    assert_eq!(entry["onBehalfOf"][0], dev.as_str(), "{entry}");
}

/// Tokens report their scope and budget limits (revoked ones on request), VCS denials reach
/// the audit log, and `denied_only` keeps only denials.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_show_scope_and_vcs_denials_are_audited() {
    let t = start_default().await;
    let minted = t
        .call(
            "AuthService",
            "MintToken",
            json!({ "scope": { "repos": ["dev/playground"], "actions": ["read"],
                               "readPaths": [B64.encode("docs")],
                               "budget": { "maxChangesets": "3" } } }),
        )
        .await
        .expect("mint");
    let id = s(&minted["tokenId"]).to_string();
    let reader = s(&minted["token"]).to_string();
    let find = |list: &Value| {
        list["tokens"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .find(|tk| tk["tokenId"] == id.as_str())
    };
    let list = t
        .call("AuthService", "ListTokens", json!({}))
        .await
        .expect("tokens");
    let info = find(&list).expect("listed");
    assert_eq!(info["scope"]["actions"][0], "read", "{info}");
    assert_eq!(info["scope"]["repos"][0], "dev/playground");
    assert_eq!(info["scope"]["readPaths"][0], B64.encode("docs"));
    assert_eq!(info["scope"]["budget"]["maxChangesets"], "3");
    assert!(!s(&info["scope"]["expiresAt"]).is_empty());

    // A read-only token cannot commit: denied, and the denial is audited.
    let (tip, _) = t.tip("main").await;
    let blob = t.upload(&[b"x\n"]).await.remove(0);
    let denied = t
        .call_as(
            &reader,
            "WriteService",
            "CreateChangeset",
            json!({
                "repo": repo(), "parents": [id_json(&tip)],
                "delta": [{ "path": B64.encode("docs/x.txt"),
                            "put": { "mode": "MODE_FILE", "blob": id_json(&blob), "size": "2" } }],
                "changeId": change_id(90), "author": "tester",
                "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                "message": "nope",
            }),
        )
        .await
        .expect_err("read-only token");
    assert!(
        matches!(
            denied.reason.as_str(),
            "PERMISSION_DENIED" | "POLICY_DENIED"
        ),
        "{denied:?}"
    );
    let denials = eventually(30, "the denial in the audit log", || async {
        let q = t
            .call(
                "AuditService",
                "QueryAuditLog",
                json!({ "org": "dev", "filter": { "deniedOnly": true } }),
            )
            .await
            .ok()?;
        let entries = q["entries"].as_array().cloned().unwrap_or_default();
        entries
            .iter()
            .any(|e| e["action"] == "vcs.commit")
            .then_some(entries)
    })
    .await;
    assert!(
        denials
            .iter()
            .all(|e| !s(&e["result"]).is_empty() && e["result"] != "ok"),
        "{denials:?}"
    );
    let commit = denials
        .iter()
        .find(|e| e["action"] == "vcs.commit")
        .unwrap();
    assert_eq!(commit["tokenId"], id.as_str(), "{commit}");

    // Revoked tokens are listed only on request.
    t.call("AuthService", "Revoke", json!({ "tokenId": id }))
        .await
        .expect("revoke");
    let list = t
        .call("AuthService", "ListTokens", json!({}))
        .await
        .expect("tokens");
    assert!(
        find(&list).is_none(),
        "revoked tokens are hidden by default"
    );
    let list = t
        .call(
            "AuthService",
            "ListTokens",
            json!({ "includeRevoked": true }),
        )
        .await
        .expect("tokens");
    assert_eq!(find(&list).expect("listed")["revoked"], true);
}

/// `bee-server --dev` seeds a second human, so a change the dev user authored can be
/// approved by someone else and landed under a policy that needs an approval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dev_mode_seeds_a_reviewer_who_can_approve_the_dev_users_change() {
    let t = start_default().await;
    let dev = t.srv.dev.as_ref().expect("dev bootstrap");
    let (reviewer, rtok) = (dev.reviewer.clone(), dev.reviewer_token.clone());
    assert_ne!(reviewer, dev.user);
    let who = t
        .call_as(&rtok, "AuthService", "WhoAmI", json!({}))
        .await
        .expect("reviewer token");
    assert_eq!(who["principal"]["id"], reviewer.as_str(), "{who}");
    let creds = std::fs::read_to_string(t.dir.path().join("credentials")).unwrap_or_default();
    if !creds.is_empty() {
        assert!(creds.contains("reviewer_token = "), "{creds}");
    }
    t.land_policy(
        "[[land]]\nref = \"main\"\n[[land.approvals]]\ncount = 1\n",
        95,
    )
    .await;
    let (tip, _) = t.tip("main").await;
    let head = id_text(
        &t.commit(&tip, &[("feature.txt", b"f\n")], 96, "Feature")
            .await["id"],
    );
    let cr = t
        .call(
            "ChangeService",
            "OpenChange",
            json!({ "repo": repo(), "head": id_json(&head), "title": "Feature" }),
        )
        .await
        .expect("open");
    let blocked = t
        .call(
            "ChangeService",
            "EvaluateLanding",
            json!({ "repo": repo(), "changeRequestId": cr["id"] }),
        )
        .await
        .expect("evaluate");
    assert_eq!(blocked["reason"], "POLICY_DENIED", "{blocked}");
    t.call_as(
        &rtok,
        "ChangeService",
        "SubmitReview",
        json!({ "repo": repo(), "changeRequestId": cr["id"], "revision": 1,
                "verdict": "VERDICT_APPROVE", "body": "ship it" }),
    )
    .await
    .expect("the reviewer approves");
    let eval = t
        .call(
            "ChangeService",
            "EvaluateLanding",
            json!({ "repo": repo(), "changeRequestId": cr["id"] }),
        )
        .await
        .expect("evaluate");
    assert_eq!(eval["allowed"], true, "{eval}");
    t.call(
        "ChangeService",
        "LandChange",
        json!({ "repo": repo(), "id": cr["id"] }),
    )
    .await
    .expect("the dev user lands");
}

/// `issues_write` gates opening issues and commenting (a token can be limited to it),
/// `include_html` renders sanitized HTML, and mentions name the mentioned user
/// (docs/spec/forge-work.md §9).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issues_write_action_html_bodies_and_mentioned_users() {
    let t = start_default().await;
    let (bob, _) = t.principal("human", "bob", "").await;
    org_member(&t, &bob, "ROLE_MEMBER").await;
    let mint = |actions: Value| {
        let t = &t;
        async move {
            let m = t
                .call(
                    "AuthService",
                    "MintToken",
                    json!({ "scope": { "actions": actions } }),
                )
                .await
                .expect("mint");
            s(&m["token"]).to_string()
        }
    };
    let reader = mint(json!(["read"])).await;
    let commenter = mint(json!(["read", "issues_write"])).await;
    let issue = t
        .call(
            "IssueService",
            "CreateIssue",
            json!({ "repo": repo(), "title": "Docs",
                    "body": "# Steps\n\n- [x] **bold**\n\n<script>x()</script> [a](javascript:x)" }),
        )
        .await
        .expect("issue");
    let n = s(&issue["number"]).to_string();
    let denied = t
        .call_as(
            &reader,
            "IssueService",
            "CreateIssueComment",
            json!({ "repo": repo(), "number": n, "body": "me too" }),
        )
        .await
        .expect_err("a read-only token cannot comment");
    assert!(
        matches!(
            denied.reason.as_str(),
            "PERMISSION_DENIED" | "POLICY_DENIED"
        ),
        "{denied:?}"
    );
    let denied = t
        .call_as(
            &reader,
            "IssueService",
            "CreateIssue",
            json!({ "repo": repo(), "title": "nope" }),
        )
        .await
        .expect_err("nor open issues");
    assert!(matches!(
        denied.reason.as_str(),
        "PERMISSION_DENIED" | "POLICY_DENIED"
    ));
    t.call_as(
        &commenter,
        "IssueService",
        "CreateIssueComment",
        json!({ "repo": repo(), "number": n, "body": "cc @bob, see *this*" }),
    )
    .await
    .expect("issues_write comments");
    let (tip, _) = t.tip("main").await;
    let blob = t.upload(&[b"x\n"]).await.remove(0);
    let no_commit = t
        .call_as(
            &commenter,
            "WriteService",
            "CreateChangeset",
            json!({
                "repo": repo(), "parents": [id_json(&tip)],
                "delta": [{ "path": B64.encode("x.txt"),
                            "put": { "mode": "MODE_FILE", "blob": id_json(&blob), "size": "2" } }],
                "changeId": change_id(80), "author": "tester",
                "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                "message": "nope",
            }),
        )
        .await
        .expect_err("issues_write does not allow commit");
    assert!(matches!(
        no_commit.reason.as_str(),
        "PERMISSION_DENIED" | "POLICY_DENIED"
    ));

    let got = t
        .call(
            "IssueService",
            "GetIssueByNumber",
            json!({ "repo": repo(), "number": n, "includeHtml": true }),
        )
        .await
        .expect("issue");
    let html = s(&got["bodyHtml"]);
    assert!(html.contains("<h1>Steps</h1>"), "{html}");
    assert!(html.contains("<strong>bold</strong>"), "{html}");
    assert!(
        !html.contains("<script") && !html.contains("href=\"javascript"),
        "{html}"
    );
    let plain = t
        .call(
            "IssueService",
            "GetIssueByNumber",
            json!({ "repo": repo(), "number": n }),
        )
        .await
        .expect("issue");
    assert!(s(&plain["bodyHtml"]).is_empty());
    let comments = t
        .call(
            "IssueService",
            "ListIssueComments",
            json!({ "repo": repo(), "number": n, "includeHtml": true }),
        )
        .await
        .expect("comments");
    assert!(
        s(&comments["comments"][0]["bodyHtml"]).contains("<em>this</em>"),
        "{comments}"
    );
    let timeline = t
        .call(
            "IssueService",
            "ListIssueTimeline",
            json!({ "repo": repo(), "number": n,
                    "kinds": ["ISSUE_TIMELINE_EVENT_KIND_MENTIONED"] }),
        )
        .await
        .expect("timeline");
    assert_eq!(
        timeline["events"][0]["mentionedUser"]["id"],
        bob.as_str(),
        "{timeline}"
    );
}

/// A native runner fetches its job's secrets by RPC (once) and reports the evaluated
/// environment URL; runs carry their numeric id; logs are searchable; badges are served
/// (docs/spec/workflows.md §15).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_runner_secrets_environment_url_log_search_and_badges() {
    let t = start_default().await;
    t.call(
        "DeploymentService",
        "CreateDeploymentEnvironment",
        json!({ "repo": repo(), "name": "production" }),
    )
    .await
    .expect("environment");
    let reg = t
        .call(
            "RunnerService",
            "CreateRunnerRegistrationToken",
            json!({ "scope": { "kind": "RUNNER_SCOPE_KIND_REPO", "repo": repo() } }),
        )
        .await
        .expect("registration token");
    let runner = t
        .call(
            "RunnerService",
            "RegisterRunner",
            json!({ "registrationToken": reg["token"], "name": "native-1",
                    "labels": ["gpu-box"] }),
        )
        .await
        .expect("register");
    let runner_id = s(&runner["runner"]["id"]).to_string();
    let cred = s(&runner["credential"]).to_string();
    let workflow = "name: CI\non: push\njobs:\n  deploy:\n    runs-on: gpu-box\n    environment:\n      name: production\n      url: https://example.com/app\n    steps:\n      - run: echo deploying\n";
    let (tip, _) = t.tip("main").await;
    let head = id_text(
        &t.commit(
            &tip,
            &[(".github/workflows/ci.yml", workflow.as_bytes())],
            60,
            "CI",
        )
        .await["id"],
    );
    t.land(&head).await.expect("land the workflow");

    let acquire =
        bee_proto::methods::find_method("/bee.v1.RunnerService/AcquireRunnerJob").expect("method");
    let job = eventually(60, "a job assignment", || async {
        let out = t
            .client(&cred)
            .call_method(
                acquire,
                &json!({ "runnerId": runner_id }),
                bee_mcp::StreamLimits {
                    max_items: 1,
                    timeout: std::time::Duration::from_secs(2),
                },
            )
            .await
            .ok()?
            .to_json();
        out["items"]
            .as_array()?
            .iter()
            .find(|i| !s(&i["jobId"]).is_empty())
            .cloned()
    })
    .await;
    let (job_id, lease) = (s(&job["jobId"]).to_string(), s(&job["leaseId"]).to_string());
    let run_id = s(&job["runId"]).to_string();
    let lease_req = json!({ "runnerId": runner_id, "jobId": job_id, "leaseId": lease });
    let secrets = t
        .call_as(
            &cred,
            "RunnerService",
            "FetchRunnerJobSecrets",
            lease_req.clone(),
        )
        .await
        .expect("secrets");
    assert!(secrets.is_object(), "{secrets}");
    let again = t
        .call_as(
            &cred,
            "RunnerService",
            "FetchRunnerJobSecrets",
            lease_req.clone(),
        )
        .await
        .expect_err("secrets are handed out once");
    assert_eq!(again.reason, "PERMISSION_DENIED", "{again:?}");
    t.call_as(
        &cred,
        "RunnerService",
        "AppendRunnerJobLog",
        json!({ "runnerId": runner_id, "jobId": job_id, "leaseId": lease, "offset": "0",
                "data": B64.encode("deploying\nthe NEEDLE is here\ndone\n") }),
    )
    .await
    .expect("log");
    t.call_as(
        &cred,
        "RunnerService",
        "CompleteRunnerJob",
        json!({ "runnerId": runner_id, "jobId": job_id, "leaseId": lease,
                "conclusion": "WORKFLOW_CONCLUSION_SUCCESS",
                "environmentUrl": "https://example.com/app/42" }),
    )
    .await
    .expect("complete");

    let run = eventually(30, "the run to complete", || async {
        let r = t
            .call(
                "WorkflowService",
                "GetWorkflowRun",
                json!({ "repo": repo(), "runId": run_id }),
            )
            .await
            .ok()?;
        (r["status"] == "WORKFLOW_STATUS_COMPLETED").then_some(r)
    })
    .await;
    assert!(
        s(&run["numericId"]).parse::<u64>().unwrap_or(0) > 0,
        "{run}"
    );
    let found = t
        .call(
            "WorkflowService",
            "SearchWorkflowRunLogs",
            json!({ "repo": repo(), "runId": run_id, "query": "needle" }),
        )
        .await
        .expect("search");
    assert_eq!(found["matches"][0]["line"], "2", "{found}");
    assert_eq!(found["matches"][0]["jobId"], job_id.as_str());
    let badge = t
        .call(
            "WorkflowService",
            "GetWorkflowBadge",
            json!({ "repo": repo(), "workflow": "ci.yml" }),
        )
        .await
        .expect("badge");
    assert_eq!(badge["message"], "passing", "{badge}");
    assert!(s(&badge["svg"]).starts_with("<svg"), "{badge}");

    let deployments = t
        .call(
            "DeploymentService",
            "ListDeployments",
            json!({ "repo": repo(), "environment": "production" }),
        )
        .await
        .expect("deployments");
    let dep = deployments["deployments"][0]["id"].clone();
    let statuses = t
        .call(
            "DeploymentService",
            "ListDeploymentStatuses",
            json!({ "repo": repo(), "deploymentId": dep }),
        )
        .await
        .expect("statuses");
    let urls: Vec<&str> = statuses["statuses"]
        .as_array()
        .map(|v| v.iter().map(|st| s(&st["environmentUrl"])).collect())
        .unwrap_or_default();
    assert!(urls.contains(&"https://example.com/app/42"), "{statuses}");
}

/// Manual bisect sessions are driven over the API: next candidate, marks, the full session,
/// and cancel (docs/spec/workflows.md §15).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_bisect_marks_next_candidate_session_and_cancel_are_rpcs() {
    let t = start_default().await;
    let (mut tip, _) = t.tip("main").await;
    let mut chain = Vec::new();
    for i in 0..=6u64 {
        let file = format!("f{i}");
        let mut files: Vec<(&str, &[u8])> = vec![(file.as_str(), b"x")];
        match i {
            0 => files.push(("value.txt", b"good")),
            3 => files.push(("value.txt", b"bad")),
            _ => {}
        }
        let head = id_text(&t.commit(&tip, &files, 200 + i, "step").await["id"]);
        t.land(&head).await.expect("land");
        tip = t.tip("main").await.0;
        chain.push(tip.clone());
    }
    let start = |manual: bool| {
        let t = &t;
        let good = chain[0].clone();
        async move {
            t.call(
                "CheckService",
                "StartBisect",
                json!({ "repo": repo(), "ref": "main", "checkName": "manual-check",
                        "good": id_json(&good), "manual": manual }),
            )
            .await
        }
    };
    let b = start(true).await.expect("start a manual bisect");
    let id = b["id"].clone();
    let mut marks = 0;
    loop {
        let next = t
            .call(
                "CheckService",
                "GetBisectNextCandidate",
                json!({ "repo": repo(), "id": id }),
            )
            .await
            .expect("next");
        let Some(cs) = next.get("changeset").filter(|c| c.is_object()).cloned() else {
            break;
        };
        let value = t.read("value.txt", &id_text(&cs)).await.expect("read");
        let outcome = if value == b"good" {
            "BISECT_OUTCOME_GOOD"
        } else {
            "BISECT_OUTCOME_BAD"
        };
        t.call(
            "CheckService",
            "MarkBisect",
            json!({ "repo": repo(), "id": id, "changeset": cs, "outcome": outcome }),
        )
        .await
        .expect("mark");
        marks += 1;
        assert!(marks <= 4, "a binary search over six candidates");
    }
    let session = t
        .call(
            "CheckService",
            "GetBisectSession",
            json!({ "repo": repo(), "id": id }),
        )
        .await
        .expect("session");
    assert_eq!(session["mode"], "manual", "{session}");
    assert_eq!(
        session["bisect"]["state"], "BISECT_STATE_FOUND",
        "{session}"
    );
    assert_eq!(id_text(&session["bisect"]["firstBad"]), chain[3]);
    assert_eq!(session["probes"].as_array().map(Vec::len), Some(marks));
    assert_eq!(session["candidates"].as_array().map(Vec::len), Some(6));

    let again = start(true).await.expect("a second session");
    let cancelled = t
        .call(
            "CheckService",
            "CancelBisect",
            json!({ "repo": repo(), "id": again["id"] }),
        )
        .await
        .expect("cancel");
    assert_eq!(cancelled["state"], "BISECT_STATE_CANCELLED", "{cancelled}");
    let finished = t
        .call(
            "CheckService",
            "CancelBisect",
            json!({ "repo": repo(), "id": again["id"] }),
        )
        .await
        .expect_err("already finished");
    assert_eq!(finished.reason, "INVALID_STATE");
}

/// The push-protection bypass policy is part of the security configuration, bypasses name
/// their audit entry, secrets can be rescanned, and SECURITY.md is served
/// (docs/spec/security.md §14).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_policy_rescans_audit_ids_and_security_policy_are_served() {
    let t = start_default().await;
    let (bob, _) = t.principal("human", "bob", "").await;
    let missing = t
        .call(
            "SecurityService",
            "GetSecurityPolicy",
            json!({ "repo": repo() }),
        )
        .await
        .expect_err("no SECURITY.md yet");
    assert_eq!(missing.reason, "NOT_FOUND");
    let (tip, _) = t.tip("main").await;
    let head = id_text(
        &t.commit(
            &tip,
            &[(
                "SECURITY.md",
                b"# Reporting\n\nMail **security@example.com**.\n",
            )],
            300,
            "policy",
        )
        .await["id"],
    );
    t.land(&head).await.expect("land");
    let policy = t
        .call(
            "SecurityService",
            "GetSecurityPolicy",
            json!({ "repo": repo() }),
        )
        .await
        .expect("security policy");
    assert_eq!(policy["path"], B64.encode("SECURITY.md"), "{policy}");
    assert!(s(&policy["content"]).starts_with("# Reporting"));
    assert!(s(&policy["contentHtml"]).contains("<strong>security@example.com</strong>"));

    let bad = t
        .call(
            "SecurityService",
            "UpdateSecurityConfiguration",
            json!({ "repo": repo(), "pushProtectionBypassMode": "sometimes" }),
        )
        .await
        .expect_err("unknown mode");
    assert_eq!(bad.reason, "INVALID_ARGUMENT");
    let cfg = t
        .call(
            "SecurityService",
            "UpdateSecurityConfiguration",
            json!({ "repo": repo(), "secretScanning": true, "secretScanningPushProtection": true,
                    "pushProtectionBypassMode": "allow", "bypassReviewerIds": [bob],
                    "bypassTtlHours": 5 }),
        )
        .await
        .expect("configure");
    assert_eq!(cfg["pushProtectionBypassMode"], "allow", "{cfg}");
    assert_eq!(cfg["bypassReviewerIds"][0], bob.as_str());
    assert_eq!(cfg["bypassTtlHours"], 5);
    let got = t
        .call(
            "SecurityService",
            "GetSecurityConfiguration",
            json!({ "repo": repo() }),
        )
        .await
        .expect("config");
    assert_eq!(got["bypassTtlHours"], 5, "{got}");

    t.call(
        "SecurityService",
        "CreateSecretScanningPattern",
        json!({ "repo": repo(), "name": "acme key", "regex": "ACME_KEY_[0-9]{8}",
                "pushProtection": true, "published": true }),
    )
    .await
    .expect("pattern");
    let (tip, _) = t.tip("main").await;
    let blocked = t
        .commit_as(
            &t.token,
            &tip,
            &[("conf.txt", b"key = ACME_KEY_12345678\n")],
            301,
            "leak",
        )
        .await
        .expect_err("push protection");
    assert_eq!(blocked.reason, "SECRET_DETECTED", "{blocked:?}");
    let secret = "ACME_KEY_12345678";
    let refused = eventually(30, "the secret refusal in the audit log", || async {
        let log = t
            .call(
                "AuditService",
                "QueryAuditLog",
                json!({ "org": "dev", "filter": { "deniedOnly": true } }),
            )
            .await
            .ok()?;
        log["entries"]
            .as_array()?
            .iter()
            .find(|e| e["result"] == "SECRET_DETECTED")
            .cloned()
    })
    .await;
    assert!(
        refused["repo"]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{refused}"
    );
    assert!(
        refused["after"].to_string().contains("conf.txt"),
        "{refused}"
    );
    assert!(refused["after"].to_string().contains("rule"), "{refused}");
    assert!(
        !serde_json::to_string(&refused).unwrap().contains(secret),
        "{refused}"
    );
    let Some(bee_proto::v1::error_detail::Payload::PushProtection(block)) = blocked.payload else {
        panic!("a push-protection block: {blocked:?}");
    };
    let block_id = block
        .bypass_request_url
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    let bypass = t
        .call(
            "SecurityService",
            "RequestPushProtectionBypass",
            json!({ "repo": repo(), "blockId": block_id,
                    "reason": "PUSH_PROTECTION_BYPASS_REASON_USED_IN_TESTS" }),
        )
        .await
        .expect("bypass");
    let audit_id = s(&bypass["auditEntryId"]).to_string();
    assert!(!audit_id.is_empty(), "{bypass}");
    let entry = eventually(30, "the bypass audit entry", || async {
        t.call(
            "AuditService",
            "GetAuditEntry",
            json!({ "org": "dev", "id": audit_id }),
        )
        .await
        .ok()
    })
    .await;
    assert_eq!(entry["action"], "push_protection.bypass_request", "{entry}");

    let rescan = t
        .call(
            "SecurityService",
            "RescanSecrets",
            json!({ "repo": repo() }),
        )
        .await
        .expect("rescan");
    assert!(rescan.is_object(), "{rescan}");
    t.call(
        "SecurityService",
        "UpdateSecurityConfiguration",
        json!({ "repo": repo(), "secretScanning": false, "secretScanningPushProtection": false }),
    )
    .await
    .expect("scanning off");
    let off = t
        .call(
            "SecurityService",
            "RescanSecrets",
            json!({ "repo": repo() }),
        )
        .await
        .expect_err("scanning is off");
    assert_eq!(off.reason, "INVALID_STATE");
}

/// Cargo is a package ecosystem on the wire, retention policies and owner settings are RPCs,
/// and Pages previews and org settings are served (docs/spec/packages-pages.md §7).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn package_retention_owner_settings_and_pages_previews_are_rpcs() {
    let t = start_default().await;
    let (bob, bob_tok) = t.principal("human", "bob", "").await;
    org_member(&t, &bob, "ROLE_MEMBER").await;
    let crates = t
        .call(
            "PackageService",
            "ListPackages",
            json!({ "owner": "dev", "ecosystem": "PACKAGE_ECOSYSTEM_CARGO" }),
        )
        .await
        .expect("cargo is an ecosystem");
    assert!(crates["packages"].as_array().is_none_or(|p| p.is_empty()));

    let set = t
        .call(
            "PackageService",
            "SetPackageRetentionPolicy",
            json!({ "owner": "dev", "keepLast": 10, "untaggedDays": 30 }),
        )
        .await
        .expect("owner default retention");
    assert_eq!(set["keepLast"], 10, "{set}");
    let got = t
        .call(
            "PackageService",
            "GetPackageRetentionPolicy",
            json!({ "owner": "dev" }),
        )
        .await
        .expect("retention");
    assert_eq!(got["untaggedDays"], 30, "{got}");
    let denied = t
        .call_as(
            &bob_tok,
            "PackageService",
            "SetPackageRetentionPolicy",
            json!({ "owner": "dev", "keepLast": 1 }),
        )
        .await
        .expect_err("members do not administer the org's packages");
    assert_eq!(denied.reason, "PERMISSION_DENIED", "{denied:?}");
    let settings = t
        .call(
            "PackageService",
            "UpdatePackageOwnerSettings",
            json!({ "owner": "dev", "forbidPublic": true, "storageQuotaBytes": "1048576" }),
        )
        .await
        .expect("owner settings");
    assert_eq!(settings["forbidPublic"], true, "{settings}");
    let settings = t
        .call(
            "PackageService",
            "GetPackageOwnerSettings",
            json!({ "owner": "dev" }),
        )
        .await
        .expect("owner settings");
    assert_eq!(settings["storageQuotaBytes"], "1048576", "{settings}");

    t.call(
        "PagesService",
        "CreatePagesSite",
        json!({ "repo": repo(), "buildType": "PAGES_BUILD_TYPE_REF",
                "source": { "ref": "main", "path": "/" } }),
    )
    .await
    .expect("site");
    let site = t
        .call(
            "PagesService",
            "UpdatePagesSite",
            json!({ "repo": repo(), "previewsEnabled": false, "previewTtlDays": 3 }),
        )
        .await
        .expect("preview settings");
    assert_eq!(site["previewTtlDays"], 3, "{site}");
    assert!(site["previewsEnabled"] != true, "{site}");
    let previews = t
        .call(
            "PagesService",
            "ListPagesPreviews",
            json!({ "repo": repo() }),
        )
        .await
        .expect("previews");
    assert!(previews["previews"].as_array().is_none_or(|p| p.is_empty()));
    let org = t
        .call(
            "PagesService",
            "UpdatePagesOrgSettings",
            json!({ "org": "dev", "forbidPublic": true }),
        )
        .await
        .expect("org pages settings");
    assert_eq!(org["forbidPublic"], true, "{org}");
    let seen = t
        .call_as(
            &bob_tok,
            "PagesService",
            "GetPagesOrgSettings",
            json!({ "org": "dev" }),
        )
        .await
        .expect("members read them");
    assert_eq!(seen["forbidPublic"], true);
    let denied = t
        .call_as(
            &bob_tok,
            "PagesService",
            "UpdatePagesOrgSettings",
            json!({ "org": "dev", "forbidPublic": false }),
        )
        .await
        .expect_err("members cannot change them");
    assert_eq!(denied.reason, "PERMISSION_DENIED");
}
