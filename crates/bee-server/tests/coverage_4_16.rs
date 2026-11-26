//! Coverage for PRODUCT §4.16 rows that had no test, through the HTTP/JSON surface of an
//! in-process dev server: creating repos, template repos, branches (including rename),
//! topics and topic pages, and the user principal record.

mod common;

use base64::Engine as _;
use common::*;
use serde_json::{Value, json};

fn named(full: &str) -> Value {
    json!({ "fullName": full })
}

fn ids(log: &Value) -> Vec<String> {
    log["changesets"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|c| id_text(&c["id"]))
        .collect()
}

async fn log_of(t: &TestServer, full: &str) -> Value {
    t.call(
        "QueryService",
        "Log",
        json!({ "repo": named(full), "from": { "expr": "main" } }),
    )
    .await
    .expect("log")
}

fn u64_of(v: &Value) -> u64 {
    v.as_str()
        .map(|s| s.parse().unwrap())
        .or_else(|| v.as_u64())
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_repo_makes_an_empty_root_and_main_at_version_one() {
    let t = start_default().await;
    let made = t
        .call(
            "RepoService",
            "CreateRepo",
            json!({ "org": "dev", "name": "fresh", "description": "new one" }),
        )
        .await
        .expect("create");
    assert_eq!(made["fullName"], "dev/fresh");
    assert_eq!(made["defaultRef"], "main");
    let got = t
        .call(
            "RepoService",
            "GetRepo",
            json!({ "repo": named("dev/fresh") }),
        )
        .await
        .expect("get");
    assert_eq!(got["id"], made["id"]);
    let main = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": named("dev/fresh"), "name": "main" }),
        )
        .await
        .expect("main exists");
    assert_eq!(u64_of(&main["ref"]["version"]), 1);
    let log = log_of(&t, "dev/fresh").await;
    let cs = log["changesets"].as_array().unwrap();
    assert_eq!(cs.len(), 1, "only the root: {log}");
    assert!(cs[0]["parents"].as_array().is_none_or(|p| p.is_empty()));
    assert_eq!(id_text(&cs[0]["id"]), id_text(&main["ref"]["target"]));
    let tree = t
        .call(
            "QueryService",
            "ListTree",
            json!({ "repo": named("dev/fresh"), "at": { "expr": "main" } }),
        )
        .await
        .expect("tree");
    assert!(
        tree["entries"].as_array().is_none_or(|e| e.is_empty()),
        "the root is empty: {tree}"
    );
    // The name is taken now; bad names are refused.
    let e = t
        .call(
            "RepoService",
            "CreateRepo",
            json!({ "org": "dev", "name": "fresh" }),
        )
        .await
        .expect_err("duplicate");
    assert_eq!(reason(&e), "ALREADY_EXISTS");
    let e = t
        .call(
            "RepoService",
            "CreateRepo",
            json!({ "org": "dev", "name": "no spaces" }),
        )
        .await
        .expect_err("bad name");
    assert_eq!(reason(&e), "INVALID_ARGUMENT");
    let listed = t
        .call("RepoService", "ListRepos", json!({ "org": "dev" }))
        .await
        .expect("list");
    assert!(
        listed["repos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["fullName"] == "dev/fresh")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_template_repo_seeds_new_repos_with_fresh_history() {
    let t = start_default().await;
    let (tip, _) = t.tip("main").await;
    let c = id_text(
        &t.commit(
            &tip,
            &[
                ("README.md", b"# starter\n"),
                ("src/lib.rs", b"pub fn x() {}\n"),
            ],
            1,
            "starter files",
        )
        .await["id"],
    );
    t.land(&c).await.expect("land");
    let template_ids = ids(&log_of(&t, "dev/playground").await);

    // Not a template yet.
    let e = t
        .call(
            "RepoService",
            "CreateRepoFromTemplate",
            json!({ "template": repo(), "org": "dev", "name": "app" }),
        )
        .await
        .expect_err("not a template");
    assert_eq!(reason(&e), "INVALID_STATE");
    let marked = t
        .call(
            "RepoService",
            "UpdateRepo",
            json!({ "repo": repo(), "isTemplate": true }),
        )
        .await
        .expect("mark");
    assert_eq!(marked["isTemplate"], true);
    let made = t
        .call(
            "RepoService",
            "CreateRepoFromTemplate",
            json!({ "template": repo(), "org": "dev", "name": "app", "description": "from the starter" }),
        )
        .await
        .expect("from template");
    assert_eq!(made["templateRepoId"], marked["id"]);
    assert_ne!(made["id"], marked["id"]);

    // Same files at the tip ...
    let read = |path: &'static str| {
        let t = &t;
        async move {
            let r = t
                .call(
                    "QueryService",
                    "ReadPath",
                    json!({ "repo": named("dev/app"), "at": { "expr": "main" }, "path": B64.encode(path) }),
                )
                .await
                .expect("read");
            let mut out = Vec::new();
            for item in r["items"].as_array().cloned().unwrap_or_default() {
                if let Some(d) = item["data"].as_str() {
                    out.extend(B64.decode(d).unwrap());
                }
            }
            out
        }
    };
    assert_eq!(read("README.md").await, b"# starter\n");
    assert_eq!(read("src/lib.rs").await, b"pub fn x() {}\n");
    // ... on a history of its own: the template's changesets are not copied; the only one
    // both share is the canonical empty root every repo starts from (content-addressed).
    let log = log_of(&t, "dev/app").await;
    let new_ids = ids(&log);
    assert_eq!(new_ids.len(), 2, "{log}");
    let root = template_ids.last().unwrap().clone();
    assert_eq!(new_ids[1], root, "starts at an empty root");
    assert!(!template_ids.contains(&new_ids[0]), "{log}");
    assert!(!new_ids.contains(&c), "the template's commit is not copied");
    let first = &log["changesets"][0];
    assert_eq!(first["parents"].as_array().unwrap().len(), 1);
    assert_eq!(id_text(&first["parents"][0]), root);
    // A repo made from a template is not itself a template.
    let e = t
        .call(
            "RepoService",
            "CreateRepoFromTemplate",
            json!({ "template": named("dev/app"), "org": "dev", "name": "app2" }),
        )
        .await
        .expect_err("copy is no template");
    assert_eq!(reason(&e), "INVALID_STATE");
}

async fn ref_names(t: &TestServer) -> Vec<String> {
    let r = t
        .call("RefService", "ListRefs", json!({ "repo": repo() }))
        .await
        .expect("list refs");
    let mut v: Vec<String> = r["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branches_are_listed_created_renamed_and_deleted() {
    let t = start_default().await;
    let (tip, _) = t.tip("main").await;
    assert_eq!(ref_names(&t).await, vec!["main"]);
    let made = t
        .call(
            "RefService",
            "CreateRef",
            json!({ "repo": repo(), "name": "feature", "target": id_json(&tip) }),
        )
        .await
        .expect("create");
    assert_eq!(u64_of(&made["version"]), 1);
    let e = t
        .call(
            "RefService",
            "CreateRef",
            json!({ "repo": repo(), "name": "feature", "target": id_json(&tip) }),
        )
        .await
        .expect_err("exists");
    assert_eq!(reason(&e), "ALREADY_EXISTS");
    assert_eq!(ref_names(&t).await, vec!["feature", "main"]);

    // Rename: stale version refused, then the branch moves to its new name.
    let e = t
        .call(
            "RefService",
            "RenameRef",
            json!({ "repo": repo(), "name": "feature", "newName": "topic", "expectedVersion": "7" }),
        )
        .await
        .expect_err("stale");
    assert_eq!(reason(&e), "VERSION_MISMATCH");
    let e = t
        .call(
            "RefService",
            "RenameRef",
            json!({ "repo": repo(), "name": "feature", "newName": "main" }),
        )
        .await
        .expect_err("taken");
    assert_eq!(reason(&e), "ALREADY_EXISTS");
    let renamed = t
        .call(
            "RefService",
            "RenameRef",
            json!({ "repo": repo(), "name": "feature", "newName": "topic", "expectedVersion": "1" }),
        )
        .await
        .expect("rename");
    assert_eq!(renamed["name"], "topic");
    assert_eq!(id_text(&renamed["target"]), tip);
    assert_eq!(ref_names(&t).await, vec!["main", "topic"]);
    assert_eq!(t.tip("topic").await.0, tip);
    // The old name redirects to the renamed ref (RenameRef's contract).
    let moved = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": repo(), "name": "feature" }),
        )
        .await
        .expect("old name redirects");
    assert_eq!(moved["ref"]["name"], "topic");
    assert_eq!(moved["renamedTo"], "topic");
    let e = t
        .call(
            "RefService",
            "RenameRef",
            json!({ "repo": repo(), "name": "feature", "newName": "x" }),
        )
        .await
        .expect_err("missing");
    assert_eq!(reason(&e), "NOT_FOUND");

    // Delete.
    let (_, v) = t.tip("topic").await;
    t.call(
        "RefService",
        "DeleteRef",
        json!({ "repo": repo(), "name": "topic", "expectedVersion": v.to_string() }),
    )
    .await
    .expect("delete");
    assert_eq!(ref_names(&t).await, vec!["main"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn topics_tag_repos_and_topic_pages_list_them() {
    let t = start_with(|c| c.search.poll_ms = 50).await;
    t.call(
        "RepoService",
        "CreateRepo",
        json!({ "org": "dev", "name": "tools", "visibility": "VISIBILITY_PUBLIC" }),
    )
    .await
    .expect("public repo");
    t.call(
        "RepoService",
        "CreateRepo",
        json!({ "org": "dev", "name": "vault", "visibility": "VISIBILITY_PRIVATE" }),
    )
    .await
    .expect("private repo");
    let set = t
        .call(
            "RepoService",
            "SetRepoTopics",
            json!({ "repo": named("dev/tools"), "topics": ["Rust", "cli", "rust"] }),
        )
        .await
        .expect("topics");
    assert_eq!(set["topics"], json!(["cli", "rust"]));
    t.call(
        "RepoService",
        "SetRepoTopics",
        json!({ "repo": named("dev/vault"), "topics": ["rust"] }),
    )
    .await
    .expect("topics");
    let e = t
        .call(
            "RepoService",
            "SetRepoTopics",
            json!({ "repo": named("dev/tools"), "topics": ["Not A Topic"] }),
        )
        .await
        .expect_err("bad topic");
    assert_eq!(reason(&e), "INVALID_ARGUMENT");
    let got = t
        .call(
            "RepoService",
            "GetRepo",
            json!({ "repo": named("dev/tools") }),
        )
        .await
        .expect("get");
    assert_eq!(got["topics"], json!(["cli", "rust"]));

    // The topic page lists the repos tagged with it; topic search finds them.
    let page = t
        .call("SearchService", "GetTopic", json!({ "name": "Rust" }))
        .await
        .expect("topic page");
    assert_eq!(page["name"], "rust");
    assert_eq!(u64_of(&page["repoCount"]), 2, "{page}");
    let mut names: Vec<String> = page["topRepos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["fullName"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["dev/tools", "dev/vault"]);
    let found = t
        .call(
            "SearchService",
            "SearchRepos",
            json!({ "query": "topic:cli" }),
        )
        .await
        .expect("topic search");
    assert_eq!(found["hits"][0]["fullName"], "dev/tools", "{found}");
    let cli = t
        .call("SearchService", "GetTopic", json!({ "name": "cli" }))
        .await
        .expect("cli page");
    assert_eq!(cli["topRepos"][0]["fullName"], "dev/tools");
    let e = t
        .call("SearchService", "GetTopic", json!({ "name": "cobol" }))
        .await
        .expect_err("unused topic");
    assert_eq!(reason(&e), "NOT_FOUND");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_principals_carry_display_names_and_external_ids() {
    let t = start_default().await;
    let state = &t.srv.state;
    let id = bee_server::util::new_ulid(state.now_ms())
        .unwrap()
        .to_text();
    let rec = bee_server::auth::PrincipalRecord {
        id: id.clone(),
        kind: "human".into(),
        display: "Ada Lovelace".into(),
        org: t.srv.dev.as_ref().unwrap().org.0.to_text(),
        owner: String::new(),
        runtime: String::new(),
        purpose: String::new(),
        external_ids: vec!["github:1815".into()],
        groups: Vec::new(),
        deactivated: false,
    };
    state
        .kv
        .put(
            bee_server::auth::NS_PRINCIPAL,
            id.as_bytes(),
            serde_json::to_vec(&rec).unwrap(),
        )
        .await
        .unwrap();
    let token = t.token_for(&rec.to_auth(), &[]);
    let me = t
        .call_as(&token, "AuthService", "WhoAmI", json!({}))
        .await
        .expect("whoami");
    assert_eq!(me["principal"]["id"], id);
    assert_eq!(me["principal"]["kind"], "PRINCIPAL_KIND_HUMAN");
    assert_eq!(me["principal"]["display"], "Ada Lovelace");
    assert_eq!(me["principal"]["externalIds"], json!(["github:1815"]));
    let p = t
        .call("AuthService", "GetPrincipal", json!({ "principalId": id }))
        .await
        .expect("get principal");
    assert_eq!(p["display"], "Ada Lovelace");
    assert_eq!(p["externalIds"], json!(["github:1815"]));
    let e = t
        .call(
            "AuthService",
            "GetPrincipal",
            json!({ "principalId": "01HZZZZZZZZZZZZZZZZZZZZZZZ" }),
        )
        .await
        .expect_err("unknown");
    assert_eq!(reason(&e), "NOT_FOUND");
    // External ids must be `provider:id`.
    let mut bad = bee_auth::Principal::human("x", "X");
    bad.external_ids.push("ada".into());
    assert!(bad.validate().is_err());
}
