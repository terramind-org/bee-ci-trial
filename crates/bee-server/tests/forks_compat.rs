//! Forks and the GitHub compatibility gateway on the real server (dev mode):
//!
//! - an outside contributor forks through `POST /repos/{o}/{r}/forks`, pushes a branch to
//!   the fork through the git data and contents APIs, opens a pull request `contrib:feature`
//!   upstream; its CI run waits for approval, a maintainer approves it and merges, and the
//!   change lands upstream through the coalescer; the fork syncs afterwards;
//! - a ref moved through the gateway's git data API goes through `RefService`, so CI starts a
//!   push run for it;
//! - a release asset uploaded through `/api/uploads` is stored in the blob store and served
//!   at the signed download URL;
//! - the gateway's state and code search are the server's;
//! - opt-in (`BEE_TEST_GH=1`): `gh repo fork` and `gh pr create` from the fork.

mod common;

use std::process::Command;

use base64::Engine as _;
use common::*;
use serde_json::{Value, json};

/// A gateway call: status and JSON body.
async fn api(t: &TestServer, token: &str, method: &str, path: &str, body: Value) -> (u16, Value) {
    let auth = format!("token {token}");
    let (status, _, bytes) = http(
        method,
        &format!("{}{path}", t.url()),
        &[
            ("authorization", auth.as_str()),
            ("accept", "application/vnd.github+json"),
            ("content-type", "application/json"),
        ],
        if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        },
    )
    .await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Makes `dev/playground` public with a README and the workflows on `main`.
async fn seed(t: &TestServer) {
    t.call(
        "RepoService",
        "UpdateRepo",
        json!({ "repo": repo(), "visibility": "VISIBILITY_PUBLIC" }),
    )
    .await
    .expect("public");
    let (tip, _) = t.tip("main").await;
    let pr = "on: pull_request\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo pr\n";
    let push = "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo push\n";
    let head = t
        .commit(
            &tip,
            &[
                ("README.md", b"# playground\n"),
                ("src/lib.rs", b"pub fn needle_in_haystack() {}\n"),
                (".github/workflows/pr.yml", pr.as_bytes()),
                (".github/workflows/push.yml", push.as_bytes()),
            ],
            1,
            "seed",
        )
        .await;
    t.land(&id_text(&head["id"])).await.expect("seed lands");
}

/// A contributor with a personal namespace (an org named after its login).
async fn contributor(t: &TestServer, name: &str) -> String {
    let (_, token) = t.principal("human", name, "").await;
    t.call_as(&token, "OrgService", "CreateOrg", json!({ "name": name }))
        .await
        .expect("personal org");
    token
}

async fn runs(t: &TestServer) -> Vec<Value> {
    let (s, v) = api(
        t,
        &t.token,
        "GET",
        "/repos/dev/playground/actions/runs",
        Value::Null,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    v["workflow_runs"].as_array().cloned().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_and_pull_request_from_a_fork_through_the_gateway() {
    let t = start_default().await;
    seed(&t).await;
    let ctok = contributor(&t, "contrib").await;

    // Fork into the caller's namespace.
    let (s, fork) = api(&t, &ctok, "POST", "/repos/dev/playground/forks", json!({})).await;
    assert_eq!(s, 202, "{fork}");
    assert_eq!(fork["full_name"], "contrib/playground");
    assert_eq!(fork["fork"], true);
    assert_eq!(fork["parent"]["full_name"], "dev/playground");
    let (s, list) = api(&t, &ctok, "GET", "/repos/dev/playground/forks", Value::Null).await;
    assert_eq!(s, 200);
    assert_eq!(list[0]["full_name"], "contrib/playground", "{list}");
    // Native clients resolve the fork by its name too.
    t.call_as(
        &ctok,
        "ObjectService",
        "ResolveRef",
        json!({ "repo": { "fullName": "contrib/playground" }, "name": "main" }),
    )
    .await
    .expect("the fork resolves by name");

    // A branch and a commit in the fork through the git data and contents APIs.
    let (s, main) = api(
        &t,
        &ctok,
        "GET",
        "/repos/contrib/playground/git/ref/heads/main",
        Value::Null,
    )
    .await;
    assert_eq!(s, 200, "{main}");
    let sha = main["object"]["sha"].as_str().unwrap().to_string();
    let (s, v) = api(
        &t,
        &ctok,
        "POST",
        "/repos/contrib/playground/git/refs",
        json!({ "ref": "refs/heads/feature", "sha": sha }),
    )
    .await;
    assert_eq!(s, 201, "{v}");
    let (s, v) = api(
        &t,
        &ctok,
        "PUT",
        "/repos/contrib/playground/contents/feature.txt",
        json!({
            "message": "Add feature",
            "content": B64.encode("from the fork\n"),
            "branch": "feature",
        }),
    )
    .await;
    assert_eq!(s, 201, "{v}");

    // The pull request from the fork.
    let (s, pr) = api(
        &t,
        &ctok,
        "POST",
        "/repos/dev/playground/pulls",
        json!({ "head": "contrib:feature", "base": "main", "title": "From the fork", "maintainer_can_modify": true }),
    )
    .await;
    assert_eq!(s, 201, "{pr}");
    assert_eq!(pr["head"]["repo"]["full_name"], "contrib/playground");
    assert_eq!(pr["head"]["ref"], "feature");
    assert_eq!(pr["maintainer_can_modify"], true);
    let number = pr["number"].as_u64().unwrap();

    // CI holds the first-time contributor's run for approval. A run is created `requested`
    // and moves on once CI has looked at it, so wait for that before checking the state.
    let run = eventually(20, "the pull request run", || async {
        runs(&t)
            .await
            .into_iter()
            .find(|r| r["event"] == "pull_request" && r["status"] != "requested")
    })
    .await;
    assert_eq!(run["status"], "action_required", "{run}");
    let (s, v) = api(
        &t,
        &ctok,
        "POST",
        &format!("/repos/dev/playground/actions/runs/{}/approve", run["id"]),
        Value::Null,
    )
    .await;
    assert_eq!(s, 403, "the contributor cannot approve: {v}");
    let (s, v) = api(
        &t,
        &t.token,
        "POST",
        &format!("/repos/dev/playground/actions/runs/{}/approve", run["id"]),
        Value::Null,
    )
    .await;
    assert!(s == 201 || s == 200 || s == 204, "{s} {v}");
    let after = eventually(20, "the approved run", || async {
        runs(&t)
            .await
            .into_iter()
            .find(|r| r["id"] == run["id"] && r["status"] != "action_required")
    })
    .await;
    assert_ne!(after["status"], "action_required");

    // A maintainer merges: the fork's stack lands upstream through the coalescer.
    let (s, v) = api(
        &t,
        &t.token,
        "PUT",
        &format!("/repos/dev/playground/pulls/{number}/merge"),
        json!({ "merge_method": "squash" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (tip, _) = t.tip("main").await;
    assert_eq!(
        t.read("feature.txt", &tip).await.unwrap(),
        b"from the fork\n"
    );

    // The fork catches up with the upstream.
    let synced = t
        .call_as(
            &ctok,
            "RepoService",
            "SyncFork",
            json!({ "repo": { "fullName": "contrib/playground" } }),
        )
        .await
        .expect("sync");
    assert_eq!(id_text(&synced["ref"]["target"]), tip, "{synced}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_data_pushes_start_ci_and_release_assets_round_trip() {
    let t = start_default().await;
    seed(&t).await;

    // A branch created through the git data API is a real ref move: CI runs `on: push`.
    let (_, main) = api(
        &t,
        &t.token,
        "GET",
        "/repos/dev/playground/git/ref/heads/main",
        Value::Null,
    )
    .await;
    let sha = main["object"]["sha"].as_str().unwrap().to_string();
    let (s, v) = api(
        &t,
        &t.token,
        "POST",
        "/repos/dev/playground/git/refs",
        json!({ "ref": "refs/heads/topic", "sha": sha }),
    )
    .await;
    assert_eq!(s, 201, "{v}");
    let run = eventually(20, "the push run", || async {
        runs(&t)
            .await
            .into_iter()
            .find(|r| r["event"] == "push" && r["head_branch"] == "topic")
    })
    .await;
    assert_eq!(run["name"], ".github/workflows/push.yml", "{run}");
    // The ref event went through the server's write path.
    let events = t
        .call("EventService", "ListEvents", json!({ "repo": repo() }))
        .await
        .expect("events");
    assert!(events.to_string().contains("topic"), "{events}");

    // Release assets: upload through /api/uploads, download at the signed URL.
    let (s, rel) = api(
        &t,
        &t.token,
        "POST",
        "/repos/dev/playground/releases",
        json!({ "tag_name": "v1.0.0", "target_commitish": "main", "name": "One" }),
    )
    .await;
    assert_eq!(s, 201, "{rel}");
    let bytes = b"\x00asset bytes\xff".to_vec();
    let auth = format!("token {}", t.token);
    let (s, _, body) = http(
        "POST",
        &format!(
            "{}/api/uploads/repos/dev/playground/releases/{}/assets?name=tool.bin",
            t.url(),
            rel["id"]
        ),
        &[
            ("authorization", auth.as_str()),
            ("content-type", "application/octet-stream"),
        ],
        bytes.clone(),
    )
    .await;
    assert_eq!(s, 201, "{}", String::from_utf8_lossy(&body));
    let asset: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(asset["state"], "uploaded");
    let (s, headers, _) = http(
        "GET",
        &format!(
            "{}/repos/dev/playground/releases/assets/{}",
            t.url(),
            asset["id"]
        ),
        &[
            ("authorization", auth.as_str()),
            ("accept", "application/octet-stream"),
        ],
        Vec::new(),
    )
    .await;
    assert_eq!(s, 302);
    let location = headers["location"].to_str().unwrap().to_string();
    let (s, _, got) = http("GET", &location, &[], Vec::new()).await;
    assert_eq!(s, 200);
    assert_eq!(got, bytes);
    // A tampered signature is refused.
    let (s, _, _) = http(
        "GET",
        &location.replace("signature=", "signature=0"),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(s, 404);

    // Code search runs on the server's trigram index.
    let (s, hits) = api(
        &t,
        &t.token,
        "GET",
        "/search/code?q=needle_in_haystack+repo:dev/playground",
        Value::Null,
    )
    .await;
    assert_eq!(s, 200, "{hits}");
    assert_eq!(hits["items"][0]["path"], "src/lib.rs", "{hits}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_state_survives_a_restart() {
    let t = start_default().await;
    let cfg = t.cfg.clone();
    let state = t.srv.state.clone();
    state
        .kv
        .put(
            bee_server::forge::compat::NS_COMPAT_STATE,
            b"probe",
            b"kept".to_vec(),
        )
        .await
        .unwrap();
    // The gateway's config derives its sealed-box key from the server's own key material.
    let c = bee_server::forge::compat::config(&state).unwrap();
    assert_ne!(
        c.secret_box_key,
        bee_compat::CompatConfig::default().secret_box_key
    );
    assert_eq!(c.public_url, None, "derived from each request's host");
    drop(state);
    let TestServer { srv, dir, .. } = t;
    srv.shutdown().await;
    let t2 = start_cfg(dir, cfg).await;
    assert_eq!(
        t2.srv
            .state
            .kv
            .get(bee_server::forge::compat::NS_COMPAT_STATE, b"probe")
            .await
            .unwrap(),
        Some(b"kept".to_vec())
    );
}

/// `gh repo fork` and `gh pr create` from the fork, against the gateway (opt-in).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gh_forks_and_opens_a_pull_request_from_the_fork() {
    if std::env::var("BEE_TEST_GH").ok().as_deref() != Some("1") {
        eprintln!("skipped: set BEE_TEST_GH=1 to run the gh CLI against the gateway");
        return;
    }
    let t = start_default().await;
    seed(&t).await;
    let ctok = contributor(&t, "contrib").await;
    let home = tempfile::tempdir().unwrap();
    let gh = |token: &str, args: &[&str]| {
        let out = Command::new("gh")
            .args(args)
            .env("GH_HOST", "github.localhost")
            .env("GH_TOKEN", token)
            .env("GH_ENTERPRISE_TOKEN", token)
            .env("HTTP_PROXY", t.url())
            .env("HTTPS_PROXY", t.url())
            .env("http_proxy", t.url())
            .env("https_proxy", t.url())
            .env("NO_PROXY", "")
            .env("no_proxy", "")
            .env("GH_CONFIG_DIR", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1")
            .env("GH_PAGER", "cat")
            .current_dir(home.path())
            .output()
            .expect("run gh");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "gh {args:?}\n{stdout}\n{stderr}");
        stdout
    };
    gh(&ctok, &["repo", "fork", "dev/playground", "--clone=false"]);
    let forks = gh(&ctok, &["api", "repos/dev/playground/forks"]);
    assert!(forks.contains("contrib/playground"), "{forks}");
    let main = gh(
        &ctok,
        &[
            "api",
            "repos/contrib/playground/git/ref/heads/main",
            "--jq",
            ".object.sha",
        ],
    );
    gh(
        &ctok,
        &[
            "api",
            "repos/contrib/playground/git/refs",
            "-f",
            "ref=refs/heads/feature",
            "-f",
            &format!("sha={}", main.trim()),
        ],
    );
    gh(
        &ctok,
        &[
            "api",
            "-X",
            "PUT",
            "repos/contrib/playground/contents/gh.txt",
            "-f",
            "message=From gh",
            "-f",
            &format!("content={}", B64.encode("gh\n")),
            "-f",
            "branch=feature",
        ],
    );
    let url = gh(
        &ctok,
        &[
            "pr",
            "create",
            "--repo",
            "dev/playground",
            "--head",
            "contrib:feature",
            "--base",
            "main",
            "--title",
            "From a fork via gh",
            "--body",
            "",
        ],
    );
    assert!(url.contains("/pull/"), "{url}");
    let view = gh(
        &t.token,
        &[
            "pr",
            "view",
            "--repo",
            "dev/playground",
            url.trim(),
            "--json",
            "headRepositoryOwner,isCrossRepository",
        ],
    );
    let v: Value = serde_json::from_str(&view).unwrap();
    assert_eq!(v["isCrossRepository"], true, "{v}");
    assert_eq!(v["headRepositoryOwner"]["login"], "contrib", "{v}");
}
