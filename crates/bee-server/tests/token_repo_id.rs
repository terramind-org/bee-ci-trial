//! SPEC §9.2: a token's `repos` may name a repo by its full name or by its id. One rule holds
//! on every surface (core RPCs, forge RPCs, git smart HTTP): the full name is matched against
//! every pattern; the id is accepted only when the token lists that exact id, and no pattern is
//! ever matched against an id. An agent loop like the scenario harness's runs once with a token
//! naming the repo by full name and once with a token naming it by id, and every call must
//! behave the same; tokens naming another repo's id, or a pattern that matches only the id's
//! text, are refused on every call.

#![allow(clippy::result_large_err)]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use base64::Engine as _;
use bee_proto::v1::ErrorDetail;
use common::*;
use serde_json::{Value, json};

const ORG: &str = "acme";
const APP: &str = "acme/app";
const ACTIONS: [&str; 6] = ["read", "commit", "land", "review", "lease", "manage_refs"];

fn b64(s: &str) -> String {
    B64.encode(s.as_bytes())
}

fn by_name() -> Value {
    json!({ "fullName": APP })
}

/// A call's outcome as one comparable word: `OK` or the SPEC §12.2 reason.
fn outcome(r: &Result<Value, ErrorDetail>) -> String {
    match r {
        Ok(_) => "OK".into(),
        Err(e) => e.reason.clone(),
    }
}

/// An agent in `acme` with a forge role on `acme/app`, owned by the dev user.
async fn agent(t: &TestServer, display: &str, role: &str) -> String {
    let a = t
        .call(
            "AuthService",
            "CreateAgent",
            json!({ "display": display, "runtime": "claude-code", "purpose": "token scope", "org": ORG }),
        )
        .await
        .expect("create agent");
    let id = a["id"].as_str().expect("agent id").to_string();
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": ORG, "principalId": id, "role": "ROLE_MEMBER" }),
    )
    .await
    .expect("org member");
    let r = t
        .call(
            "RepoService",
            "AddCollaborator",
            json!({ "repo": by_name(), "principalId": id, "role": role }),
        )
        .await
        .expect("collaborator");
    if let Some(inv) = r["invitation"]["id"].as_str() {
        let tok = mint(t, &id, &[], &[]).await;
        t.call_as(
            &tok,
            "RepoService",
            "AcceptRepoInvitation",
            json!({ "invitationId": inv }),
        )
        .await
        .expect("accept invitation");
    }
    id
}

/// A token for `principal` minted by the dev user through `AuthService/MintToken`.
async fn mint(t: &TestServer, principal: &str, repos: &[&str], actions: &[&str]) -> String {
    let mut scope = json!({});
    if !repos.is_empty() {
        scope["repos"] = json!(repos);
    }
    if !actions.is_empty() {
        scope["actions"] = json!(actions);
    }
    let r = t
        .call(
            "AuthService",
            "MintToken",
            json!({ "principalId": principal, "scope": scope }),
        )
        .await
        .expect("mint");
    r["token"].as_str().expect("token").to_string()
}

async fn tip(t: &TestServer) -> (String, u64) {
    let r = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": by_name(), "name": "main" }),
        )
        .await
        .expect("resolve main");
    (
        id_text(&r["ref"]["target"]),
        r["ref"]["version"].as_str().unwrap_or("0").parse().unwrap(),
    )
}

/// Uploads `content` and creates a changeset putting it at `path` on `parent`.
async fn changeset(
    t: &TestServer,
    token: &str,
    parent: &str,
    path: &str,
    content: &[u8],
    cid: u64,
) -> Result<String, ErrorDetail> {
    let blob = bee_object::blob_id(content).to_text();
    t.call_as(
        token,
        "WriteService",
        "UploadObjects",
        json!({ "messages": [{
            "repo": by_name(), "id": id_json(&blob), "offset": "0",
            "data": B64.encode(content), "last": true,
        }] }),
    )
    .await?;
    let r = t
        .call_as(
            token,
            "WriteService",
            "CreateChangeset",
            json!({
                "repo": by_name(),
                "parents": [id_json(parent)],
                "delta": [{ "path": b64(path), "put": {
                    "mode": "MODE_FILE", "blob": id_json(&blob), "size": content.len().to_string(),
                } }],
                "changeId": change_id(cid),
                "author": "agent",
                "authorTime": { "name": "agent", "time": "2026-10-18T00:00:00Z" },
                "message": format!("write {path}"),
            }),
        )
        .await?;
    Ok(id_text(&r["changeset"]["id"]))
}

fn git(home: &Path, dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git did not run: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn encode(token: &str) -> String {
    token
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Clones `acme/app` over smart HTTP with `token`, commits a file on `branch` and pushes it.
/// Returns which step failed, or `OK`.
async fn git_push(t: &TestServer, token: &str, branch: &str) -> String {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".gitconfig"),
        "[user]\n\tname = Agent\n\temail = agent@example.com\n[init]\n\tdefaultBranch = main\n[protocol]\n\tversion = 2\n",
    )
    .unwrap();
    let url = format!("http://git:{}@{}/{APP}.git", encode(token), t.srv.addr);
    let h = home.path().to_path_buf();
    let branch = branch.to_string();
    tokio::task::spawn_blocking(move || {
        let w = h.join("w");
        if let Err(e) = git(&h, &h, &["clone", "-q", &url, w.to_str().unwrap()]) {
            return format!("clone failed: {e}");
        }
        let steps: [&[&str]; 3] = [
            &["checkout", "-q", "-b", &branch],
            &["add", "pushed.txt"],
            &["commit", "-q", "-m", "Pushed over git"],
        ];
        std::fs::write(w.join("pushed.txt"), format!("{branch}\n")).unwrap();
        for s in steps {
            if let Err(e) = git(&h, &w, s) {
                return format!("local step failed: {e}");
            }
        }
        match git(&h, &w, &["push", "-q", "origin", &branch]) {
            Ok(_) => "OK".into(),
            Err(e) => format!("push failed: {e}"),
        }
    })
    .await
    .unwrap()
}

/// Every call of the scenarios' agent loop with `worker`'s token (and `reviewer`'s for the
/// review), as `(call, outcome)`. `n` keeps change ids and paths apart between runs.
async fn agent_loop(
    t: &TestServer,
    worker_id: &str,
    worker: &str,
    reviewer: &str,
    n: u64,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let (base, _) = tip(t).await;
    let tag = format!("run{n}");

    // Upload and changeset.
    let c1 = changeset(
        t,
        worker,
        &base,
        &format!("{tag}/a.txt"),
        b"one\n",
        n * 10 + 1,
    )
    .await;
    out.push(("CreateChangeset", outcome(&c1.clone().map(|_| Value::Null))));
    // From here on the admin supplies changesets the worker could not create, so a refusal
    // later in the loop is that call's own and not a knock-on of the first one.
    let c1 = match c1 {
        Ok(c) => c,
        Err(_) => changeset(
            t,
            &t.token,
            &base,
            &format!("{tag}/a.txt"),
            b"one\n",
            n * 10 + 1,
        )
        .await
        .expect("admin changeset"),
    };
    let c2 = changeset(
        t,
        &t.token,
        &c1,
        &format!("{tag}/b.txt"),
        b"two\n",
        n * 10 + 2,
    )
    .await
    .expect("admin changeset");

    // A workspace ref, created and then moved.
    let ws = format!("ws/{worker_id}/{tag}");
    let created = t
        .call_as(
            worker,
            "RefService",
            "CreateRef",
            json!({ "repo": by_name(), "name": ws, "target": id_json(&c1) }),
        )
        .await;
    out.push(("CreateRef", outcome(&created)));
    let version = match &created {
        Ok(r) => r["version"].as_str().unwrap_or("1").to_string(),
        Err(_) => {
            let r = t
                .call(
                    "RefService",
                    "CreateRef",
                    json!({ "repo": by_name(), "name": ws, "target": id_json(&c1) }),
                )
                .await
                .expect("admin creates the ref");
            r["version"].as_str().unwrap_or("1").to_string()
        }
    };
    let updated = t
        .call_as(
            worker,
            "RefService",
            "UpdateRef",
            json!({ "repo": by_name(), "name": ws, "target": id_json(&c2), "expectedVersion": version }),
        )
        .await;
    out.push(("UpdateRef", outcome(&updated)));

    // Land the stack on main.
    let landed = t
        .call_as(
            worker,
            "WriteService",
            "Land",
            json!({ "repo": by_name(), "ref": "main", "head": id_json(&c2) }),
        )
        .await;
    out.push(("Land", outcome(&landed)));
    if landed.is_err() {
        t.call(
            "WriteService",
            "Land",
            json!({ "repo": by_name(), "ref": "main", "head": id_json(&c2) }),
        )
        .await
        .expect("admin lands");
    }

    // Reads, by full name and by id in the request's RepoRef.
    let read = t
        .call_as(
            worker,
            "QueryService",
            "ReadPath",
            json!({ "repo": by_name(), "at": { "expr": "main" }, "path": b64(&format!("{tag}/a.txt")) }),
        )
        .await;
    if let Ok(r) = &read {
        let data = r["items"][0]["data"].as_str().unwrap_or_default();
        assert_eq!(B64.decode(data).unwrap(), b"one\n", "{r}");
    }
    out.push(("ReadPath", outcome(&read)));
    let log = t
        .call_as(
            worker,
            "QueryService",
            "Log",
            json!({ "repo": by_name(), "from": { "expr": "main" } }),
        )
        .await;
    out.push(("Log", outcome(&log)));

    // A change request: opened by the worker, reviewed by another agent.
    let (main_tip, _) = tip(t).await;
    let c3 = changeset(
        t,
        &t.token,
        &main_tip,
        &format!("{tag}/c.txt"),
        b"three\n",
        n * 10 + 3,
    )
    .await
    .expect("admin changeset");
    let opened = t
        .call_as(
            worker,
            "ChangeService",
            "OpenChange",
            json!({ "repo": by_name(), "head": id_json(&c3), "title": format!("Change {tag}") }),
        )
        .await;
    out.push(("OpenChange", outcome(&opened)));
    let cr = match opened {
        Ok(c) => c,
        Err(_) => t
            .call(
                "ChangeService",
                "OpenChange",
                json!({ "repo": by_name(), "head": id_json(&c3), "title": format!("Change {tag}") }),
            )
            .await
            .expect("admin opens the change"),
    };
    let review = t
        .call_as(
            reviewer,
            "ChangeService",
            "SubmitReview",
            json!({
                "repo": by_name(), "changeRequestId": cr["id"], "revision": 1,
                "verdict": "VERDICT_APPROVE", "body": "looks right",
            }),
        )
        .await;
    out.push(("SubmitReview", outcome(&review)));

    // A lease on the run's directory.
    let lease = t
        .call_as(
            worker,
            "LeaseService",
            "AcquireLease",
            json!({
                "repo": by_name(), "ref": "main", "paths": [b64(&tag)],
                "mode": "LEASE_MODE_EXCLUSIVE", "changeId": change_id(n * 10 + 3), "note": "editing",
            }),
        )
        .await;
    out.push(("AcquireLease", outcome(&lease)));

    // Git smart HTTP: clone, commit, push a branch in the worker's own workspace namespace
    // (a new plain branch needs `manage_refs` from the role, which WRITE does not grant).
    out.push((
        "git push",
        git_push(t, worker, &format!("ws/{worker_id}/git-{tag}")).await,
    ));
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repo_named_by_id_in_token_scope_works_like_its_full_name_on_every_surface() {
    let t = start_default().await;
    t.call(
        "OrgService",
        "CreateOrg",
        json!({ "name": ORG, "display": "Acme" }),
    )
    .await
    .expect("create org");
    let mut ids = Vec::new();
    for name in ["app", "other"] {
        let r = t
            .call(
                "RepoService",
                "CreateRepo",
                json!({ "org": ORG, "name": name, "visibility": "VISIBILITY_PRIVATE", "autoInit": true }),
            )
            .await
            .expect("create repo");
        assert_eq!(r["fullName"], format!("{ORG}/{name}"), "{r}");
        ids.push(r["id"].as_str().expect("repo id").to_string());
    }
    let (app_id, other_id) = (ids[0].clone(), ids[1].clone());
    let worker = agent(&t, "worker", "REPO_ROLE_WRITE").await;
    let reviewer = agent(&t, "reviewer", "REPO_ROLE_WRITE").await;

    // MintToken keeps the id as given, and WhoAmI reports it.
    let by_id = mint(&t, &worker, &[&app_id], &ACTIONS).await;
    let who = t
        .call_as(&by_id, "AuthService", "WhoAmI", json!({}))
        .await
        .expect("whoami");
    assert_eq!(who["effectiveScope"]["repos"], json!([app_id]), "{who}");
    let l = t
        .call(
            "AuthService",
            "ListTokens",
            json!({ "principalId": worker }),
        )
        .await
        .expect("list tokens");
    let scopes: Vec<&Value> = l["tokens"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| &t["scope"]["repos"])
        .collect();
    assert!(scopes.contains(&&json!([app_id])), "{l}");

    // The same loop with the repo named by full name and by id: identical outcomes, and the
    // full-name run succeeds on every call.
    let name_worker = mint(&t, &worker, &[APP], &ACTIONS).await;
    let name_reviewer = mint(&t, &reviewer, &[APP], &ACTIONS).await;
    let id_reviewer = mint(&t, &reviewer, &[&app_id], &ACTIONS).await;
    let named = agent_loop(&t, &worker, &name_worker, &name_reviewer, 1).await;
    let id_run = agent_loop(&t, &worker, &by_id, &id_reviewer, 2).await;
    for (call, o) in &named {
        assert_eq!(o, "OK", "full-name token on {call}: {named:?}");
    }
    assert_eq!(named, id_run, "full name vs id");

    // Refused on every call: another repo's id, a prefix pattern that matches this repo's id
    // text but not its full name, and another repo's full name.
    let id_prefix = format!("{}*", &app_id[..2]);
    for (n, repos) in [other_id.as_str(), id_prefix.as_str(), "acme/other"]
        .into_iter()
        .enumerate()
    {
        let w = mint(&t, &worker, &[repos], &ACTIONS).await;
        let r = mint(&t, &reviewer, &[repos], &ACTIONS).await;
        let run = agent_loop(&t, &worker, &w, &r, 3 + n as u64).await;
        for (call, o) in &run {
            assert_ne!(
                o, "OK",
                "token for {repos:?} must be refused on {call}: {run:?}"
            );
        }
    }
}
