//! Agents create their own sub-agents (SPEC §9.1, PRODUCT §4.9, AS-AUTH-01): an agent's
//! `CreateAgent` makes an agent it owns, in its own org, within the delegation depth; it then
//! mints the sub-agent a token limited to `docs/` with a budget of five changesets, attenuated
//! from its own.

// The helpers return the wire `ErrorDetail` as the shared client does.
#![allow(clippy::result_large_err)]

mod common;

use base64::Engine as _;
use bee_proto::v1::ErrorDetail;
use common::*;
use serde_json::{Value, json};

const REPO: &str = "dev/handbook";

fn rref() -> Value {
    json!({ "fullName": REPO })
}

/// Commits `files` on `parent` in [`REPO`] with `token`.
async fn commit(
    t: &TestServer,
    token: &str,
    parent: &str,
    files: &[(&str, &[u8])],
    cid: u64,
) -> Result<Value, ErrorDetail> {
    let mut messages = Vec::new();
    let mut delta = Vec::new();
    for (i, (path, bytes)) in files.iter().enumerate() {
        let id = bee_object::blob_id(bytes).to_text();
        let mut m =
            json!({ "id": id_json(&id), "offset": "0", "data": B64.encode(bytes), "last": true });
        if i == 0 {
            m["repo"] = rref();
        }
        messages.push(m);
        delta.push(json!({ "path": B64.encode(path.as_bytes()), "put": { "mode": "MODE_FILE", "blob": id_json(&id), "size": bytes.len().to_string() } }));
    }
    t.call_as(
        token,
        "WriteService",
        "UploadObjects",
        json!({ "messages": messages }),
    )
    .await?;
    let resp = t
        .call_as(
            token,
            "WriteService",
            "CreateChangeset",
            json!({
                "repo": rref(),
                "parents": [id_json(parent)],
                "delta": delta,
                "changeId": change_id(cid),
                "author": "tester",
                "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                "message": format!("change {cid}"),
            }),
        )
        .await?;
    Ok(resp["changeset"].clone())
}

/// Lets `id` into the `dev` org and makes it a write collaborator on [`REPO`] (as the dev
/// user, the org's admin), accepting an invitation with `token` if one is sent.
async fn join(t: &TestServer, id: &str, token: &str) {
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": id, "role": "ROLE_MEMBER" }),
    )
    .await
    .expect("org member");
    let r = t
        .call(
            "RepoService",
            "AddCollaborator",
            json!({ "repo": rref(), "principalId": id, "role": "REPO_ROLE_WRITE" }),
        )
        .await
        .expect("add collaborator");
    if let Some(inv) = r["invitation"]["id"].as_str() {
        t.call_as(
            token,
            "RepoService",
            "AcceptRepoInvitation",
            json!({ "invitationId": inv }),
        )
        .await
        .expect("accept invitation");
    }
}

/// `CreateAgent` as `token`, returning the new agent's id.
async fn create_agent(
    t: &TestServer,
    token: &str,
    display: &str,
    org: &str,
) -> Result<String, ErrorDetail> {
    let a = t
        .call_as(
            token,
            "AuthService",
            "CreateAgent",
            json!({ "display": display, "runtime": "claude-code", "purpose": "docs", "org": org }),
        )
        .await?;
    Ok(a["id"].as_str().expect("id").to_string())
}

/// `MintToken` as `token` for `principal` with `scope`.
async fn mint(
    t: &TestServer,
    token: &str,
    principal: &str,
    scope: Value,
) -> Result<String, ErrorDetail> {
    let m = t
        .call_as(
            token,
            "AuthService",
            "MintToken",
            json!({ "principalId": principal, "scope": scope }),
        )
        .await?;
    Ok(m["token"].as_str().expect("token").to_string())
}

/// The dev server with a private [`REPO`] seeded with `docs/index.md` and `src/main.py`, and
/// the worker agent A: created by the dev user (org admin), an org member and a write
/// collaborator, holding a token scoped to the repo as the harness's worker token is.
async fn world() -> (TestServer, String, String, String) {
    let t = start_default().await;
    t.call(
        "RepoService",
        "CreateRepo",
        json!({ "org": "dev", "name": "handbook", "visibility": "VISIBILITY_PRIVATE", "autoInit": true }),
    )
    .await
    .expect("create repo");
    let tip = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": rref(), "name": "main" }),
        )
        .await
        .expect("main");
    let root = id_text(&tip["ref"]["target"]);
    let seed = commit(
        &t,
        &t.token,
        &root,
        &[
            ("docs/index.md", b"# Docs\n"),
            ("src/main.py", b"print('main')\n"),
        ],
        1,
    )
    .await
    .expect("seed");
    let seed = id_text(&seed["id"]);
    let a = create_agent(&t, &t.token, "worker", "dev")
        .await
        .expect("worker");
    let a_token = mint(
        &t,
        &t.token,
        &a,
        json!({
            "repos": [REPO],
            "actions": ["read", "commit", "land", "review", "lease", "manage_refs"],
        }),
    )
    .await
    .expect("worker token");
    join(&t, &a, &a_token).await;
    (t, a, a_token, seed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_creates_a_sub_agent_and_mints_it_a_docs_token_with_a_budget_of_five() {
    let (t, a, a_token, seed) = world().await;

    // The worker creates the sub-agent itself; the worker owns it.
    let created = t
        .call_as(
            &a_token,
            "AuthService",
            "CreateAgent",
            json!({ "display": "docs-writer", "runtime": "claude-code", "purpose": "documentation", "org": "dev" }),
        )
        .await
        .expect("an agent creates an agent");
    let s = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["ownerId"], a);
    assert_eq!(created["kind"], "PRINCIPAL_KIND_AGENT");
    let stored = t.srv.state.principal(&s).await.unwrap().expect("stored");
    assert_eq!(stored.owner, a);
    assert_eq!(stored.org, "dev", "takes the worker's org");
    let audit = t.srv.state.audit.read(1, 10_000).await.unwrap();
    let entry = audit
        .iter()
        .find(|e| e.decision == "create_agent" && e.target == s)
        .expect("create_agent audited");
    assert_eq!(entry.chain[0], a, "the worker is the actor");

    // The sub-agent joins the org and the repo (as the scenario's setup does).
    let s_token_for_invites = mint(&t, &a_token, &s, json!({}))
        .await
        .expect("plain token");
    join(&t, &s, &s_token_for_invites).await;

    // The worker mints the sub-agent's token: docs/ only, five changesets.
    let token = mint(
        &t,
        &a_token,
        &s,
        json!({ "writePaths": [B64.encode("docs/")], "budget": { "maxChangesets": "5" } }),
    )
    .await
    .expect("the owner mints for its sub-agent");
    let who = t
        .call_as(&token, "AuthService", "WhoAmI", json!({}))
        .await
        .unwrap();
    assert_eq!(who["principal"]["id"], s);
    assert_eq!(who["onBehalfOf"][0], a);
    assert_eq!(who["onBehalfOf"][1], t.dev_user());
    assert_eq!(who["remaining"]["maxChangesets"], "5");
    let writes = who["effectiveScope"]["writePaths"].as_array().unwrap();
    assert_eq!(writes.len(), 1);
    let w = String::from_utf8(B64.decode(writes[0].as_str().unwrap()).unwrap()).unwrap();
    assert!(
        ["docs", "docs/", "/docs", "/docs/"].contains(&w.as_str()),
        "write path {w:?}"
    );
    // Attenuated from the worker's token: its repo and actions carry over.
    assert_eq!(who["effectiveScope"]["repos"], json!([REPO]));
    assert!(
        !who["effectiveScope"]["actions"]
            .as_array()
            .unwrap()
            .contains(&json!("admin"))
    );

    // A write outside docs/ is refused by policy.
    let e = commit(
        &t,
        &token,
        &seed,
        &[("src/main.py", b"print('pwned')\n")],
        10,
    )
    .await
    .expect_err("outside docs/");
    assert_eq!(reason(&e), "POLICY_DENIED", "{e:?}");
    // Five docs changesets, then the sixth is refused for budget.
    for i in 0..5u64 {
        commit(
            &t,
            &token,
            &seed,
            &[(&format!("docs/p{i}.md"), format!("{i}\n").as_bytes())],
            20 + i,
        )
        .await
        .unwrap_or_else(|e| panic!("docs changeset {i}: {e:?}"));
    }
    let e = commit(&t, &token, &seed, &[("docs/six.md", b"6\n")], 30)
        .await
        .expect_err("sixth");
    assert_eq!(reason(&e), "BUDGET_EXHAUSTED", "{e:?}");
    let who = t
        .call_as(&token, "AuthService", "WhoAmI", json!({}))
        .await
        .unwrap();
    assert_eq!(
        who["remaining"]["maxChangesets"].as_str().unwrap_or("0"),
        "0"
    );

    // Revoking the worker's token takes the sub-agent's token with it.
    let a_id = t
        .call_as(&a_token, "AuthService", "WhoAmI", json!({}))
        .await
        .unwrap()["tokenId"]
        .clone();
    t.call("AuthService", "Revoke", json!({ "tokenId": a_id }))
        .await
        .expect("revoke");
    let e = t
        .call_as(&token, "AuthService", "WhoAmI", json!({}))
        .await
        .expect_err("derived token revoked");
    assert_eq!(reason(&e), "TOKEN_REVOKED", "{e:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sub_agent_takes_its_creators_org_and_no_other() {
    let (t, a, a_token, _) = world().await;
    // No org: the worker's own.
    let s = create_agent(&t, &a_token, "no-org", "")
        .await
        .expect("empty org");
    assert_eq!(t.srv.state.principal(&s).await.unwrap().unwrap().org, "dev");
    // The org by id is the same org.
    let dev_org = t.srv.dev.as_ref().unwrap().org.0.to_text();
    let s = create_agent(&t, &a_token, "by-id", &dev_org)
        .await
        .expect("org by id");
    let rec = t.srv.state.principal(&s).await.unwrap().unwrap();
    assert_eq!((rec.org.as_str(), rec.owner.as_str()), ("dev", a.as_str()));
    // Another org, even one the dev user owns, is refused.
    t.call(
        "OrgService",
        "CreateOrg",
        json!({ "name": "acme", "display": "Acme" }),
    )
    .await
    .expect("acme");
    let e = create_agent(&t, &a_token, "elsewhere", "acme")
        .await
        .expect_err("another org");
    assert_eq!(reason(&e), "PERMISSION_DENIED", "{e:?}");
    assert!(e.message.contains("own org"), "{}", e.message);
    let e = create_agent(&t, &a_token, "nowhere", "no-such-org")
        .await
        .expect_err("unknown org");
    assert_eq!(reason(&e), "PERMISSION_DENIED", "{e:?}");
    // Humans are unchanged: the dev user still creates agents in any org it names.
    let h = create_agent(&t, &t.token, "human-made", "acme")
        .await
        .expect("human creates in acme");
    let rec = t.srv.state.principal(&h).await.unwrap().unwrap();
    assert_eq!(
        (rec.org.as_str(), rec.owner.as_str()),
        ("acme", t.dev_user().as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_mints_only_for_agents_it_owns() {
    let (t, _a, a_token, _) = world().await;
    // An agent the dev user owns, not the worker.
    let b = create_agent(&t, &t.token, "not-yours", "dev")
        .await
        .unwrap();
    let e = mint(&t, &a_token, &b, json!({}))
        .await
        .expect_err("not its agent");
    assert_eq!(reason(&e), "PERMISSION_DENIED", "{e:?}");
    // Nor for the dev user (a human), nor for a sibling's sub-agent.
    let e = mint(&t, &a_token, &t.dev_user(), json!({}))
        .await
        .expect_err("a human");
    assert_eq!(reason(&e), "PERMISSION_DENIED", "{e:?}");
    let b_token = mint(&t, &t.token, &b, json!({})).await.unwrap();
    let bs = create_agent(&t, &b_token, "bs", "").await.unwrap();
    let e = mint(&t, &a_token, &bs, json!({}))
        .await
        .expect_err("a sibling's sub-agent");
    assert_eq!(reason(&e), "PERMISSION_DENIED", "{e:?}");
    // A sub-agent cannot mint for its creator either.
    let s = create_agent(&t, &a_token, "s", "").await.unwrap();
    let s_token = mint(&t, &a_token, &s, json!({})).await.unwrap();
    let a = t
        .call_as(&a_token, "AuthService", "WhoAmI", json!({}))
        .await
        .unwrap()["principal"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let e = mint(&t, &s_token, &a, json!({}))
        .await
        .expect_err("its owner");
    assert_eq!(reason(&e), "PERMISSION_DENIED", "{e:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sub_agents_nest_until_the_owner_chain_is_seven_long() {
    let (t, a, a_token, _) = world().await;
    // The worker's chain is [worker, dev user]: 2. Each level adds one.
    let (mut id, mut token) = (a, a_token);
    for depth in 3..=7usize {
        let next = create_agent(&t, &token, &format!("level-{depth}"), "")
            .await
            .unwrap_or_else(|e| panic!("depth {depth}: {e:?}"));
        let chain = t.srv.state.principal_chain(&next).await.unwrap();
        assert_eq!(chain.len(), depth);
        assert_eq!(chain[1].0, id, "owned by its creator");
        token = mint(&t, &token, &next, json!({})).await.unwrap();
        id = next;
    }
    let who = t
        .call_as(&token, "AuthService", "WhoAmI", json!({}))
        .await
        .unwrap();
    assert_eq!(who["onBehalfOf"].as_array().unwrap().len(), 6);
    // At seven the agent may not create another.
    let e = create_agent(&t, &token, "level-8", "")
        .await
        .expect_err("chain of seven");
    assert_eq!(reason(&e), "PERMISSION_DENIED", "{e:?}");
    assert!(e.message.contains("owner chain of 7"), "{}", e.message);
}
