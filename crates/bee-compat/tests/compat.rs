//! Contract tests for the GitHub compatibility gateway: JSON shapes per endpoint group
//! (written from GitHub's public REST and GraphQL references), pagination, ETags, rate limits,
//! API versions and error bodies, over a real forge on SQLite.

use crate::common;
use bee_compat::CompatConfig;
use common::{ORG, has_keys, world, world_with};
use serde_json::{Value, json};

const REPO_KEYS: &[&str] = &[
    "id",
    "node_id",
    "name",
    "full_name",
    "private",
    "owner",
    "html_url",
    "description",
    "fork",
    "url",
    "clone_url",
    "default_branch",
    "visibility",
    "permissions",
    "created_at",
    "pushed_at",
    "open_issues_count",
    "topics",
    "has_issues",
    "archived",
    "allow_squash_merge",
];

const PULL_KEYS: &[&str] = &[
    "url",
    "id",
    "node_id",
    "html_url",
    "number",
    "state",
    "locked",
    "title",
    "user",
    "body",
    "labels",
    "milestone",
    "created_at",
    "updated_at",
    "closed_at",
    "merged_at",
    "assignees",
    "requested_reviewers",
    "head",
    "base",
    "_links",
    "draft",
    "merged",
    "mergeable",
    "mergeable_state",
    "commits",
    "additions",
    "deletions",
    "changed_files",
];

const ISSUE_KEYS: &[&str] = &[
    "id",
    "node_id",
    "url",
    "repository_url",
    "html_url",
    "number",
    "state",
    "title",
    "body",
    "user",
    "labels",
    "assignee",
    "assignees",
    "milestone",
    "locked",
    "comments",
    "created_at",
    "updated_at",
    "closed_at",
    "author_association",
    "reactions",
];

// ----- edge: auth, versions, errors, rate limits, ETags, pagination ------------------------

#[tokio::test]
async fn errors_have_githubs_shape_and_the_bee_reason() {
    let w = world().await;
    let r = w.call("GET", "/api/v3/user", None, None).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json["message"], "Requires authentication");
    has_keys(
        &r.json,
        &["message", "documentation_url", "status", "bee_reason"],
    );
    let r = w
        .call("GET", "/api/v3/user", Some("not-a-token"), None)
        .await;
    assert_eq!(
        (r.status, r.json["message"].as_str()),
        (401, Some("Bad credentials"))
    );
    let r = w.get(&format!("/api/v3/repos/{ORG}/nope"), "owner").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json["bee_reason"], "NOT_FOUND");
    let r = w.get("/api/v3/definitely/not/an/endpoint", "owner").await;
    assert_eq!(r.status, 404);
    let t = w.token("owner");
    let r = w
        .call(
            "POST",
            &format!("/api/v3/orgs/{ORG}/repos"),
            Some(&t),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 422);
    assert_eq!(r.json["message"], "Validation Failed");
    assert_eq!(r.json["errors"][0]["code"], "missing_field");
    let r = w
        .send(
            "GET",
            "/api/v3/user",
            Some(&t),
            None,
            &[("x-github-api-version", "2099-01-01")],
        )
        .await;
    assert_eq!(r.status, 400);
    let r = w
        .send(
            "GET",
            "/api/v3/user",
            Some(&t),
            None,
            &[("x-github-api-version", "2022-11-28")],
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("x-github-api-version-selected"),
        Some("2022-11-28")
    );
    assert!(r.header("x-github-request-id").is_some());
    assert!(r.header("x-oauth-scopes").is_some());
}

#[tokio::test]
async fn bearer_basic_and_token_schemes_authenticate() {
    use base64::Engine as _;
    let w = world().await;
    let t = w.token("writer");
    for auth in [
        format!("token {t}"),
        format!("Bearer {t}"),
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{t}"))
        ),
    ] {
        let r = w
            .send(
                "GET",
                "/api/v3/user",
                None,
                None,
                &[("authorization", &auth)],
            )
            .await;
        assert_eq!(r.status, 200, "{auth}: {:?}", r.json);
        assert_eq!(r.json["login"], "writer");
    }
}

#[tokio::test]
async fn rate_limits_count_down_refund_304s_and_answer_403_when_spent() {
    let w = world_with(CompatConfig {
        core_limit: 3,
        ..CompatConfig::default()
    })
    .await;
    let t = w.token("owner");
    let r = w.call("GET", "/api/v3/user", Some(&t), None).await;
    assert_eq!(r.header("x-ratelimit-limit"), Some("3"));
    assert_eq!(r.header("x-ratelimit-remaining"), Some("2"));
    assert_eq!(r.header("x-ratelimit-used"), Some("1"));
    assert_eq!(r.header("x-ratelimit-resource"), Some("core"));
    let etag = r.header("etag").expect("etag").to_string();
    let r = w
        .send(
            "GET",
            "/api/v3/user",
            Some(&t),
            None,
            &[("if-none-match", &etag)],
        )
        .await;
    assert_eq!(r.status, 304);
    assert!(r.body.is_empty());
    assert_eq!(
        r.header("x-ratelimit-remaining"),
        Some("2"),
        "a 304 is free"
    );
    w.call("GET", "/api/v3/user", Some(&t), None).await;
    w.call("GET", "/api/v3/user", Some(&t), None).await;
    let r = w.call("GET", "/api/v3/user", Some(&t), None).await;
    assert_eq!(r.status, 403);
    assert_eq!(r.header("x-ratelimit-remaining"), Some("0"));
    assert!(
        r.json["message"]
            .as_str()
            .unwrap()
            .contains("rate limit exceeded")
    );
    assert!(r.header("retry-after").is_some());
    let r = w.call("GET", "/api/v3/rate_limit", Some(&t), None).await;
    assert_eq!(r.status, 200, "rate_limit is free");
    assert_eq!(r.json["resources"]["core"]["remaining"], 0);
    has_keys(&r.json["resources"], &["core", "search", "graphql"]);
    // Another principal has its own window.
    let r = w.get("/api/v3/user", "writer").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn link_headers_paginate_lists() {
    let w = world().await;
    w.repo("app").await;
    let t = w.token("owner");
    for i in 0..5 {
        let r = w
            .call(
                "POST",
                &format!("/api/v3/repos/{ORG}/app/labels"),
                Some(&t),
                Some(json!({"name": format!("l{i}"), "color": "ff0000"})),
            )
            .await;
        assert_eq!(r.status, 201, "{:?}", r.json);
    }
    let r = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/labels?per_page=2"),
            "owner",
        )
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 2);
    let link = r.header("link").expect("link header");
    assert!(
        link.contains("rel=\"next\"") && link.contains("rel=\"last\""),
        "{link}"
    );
    assert!(link.contains("page=2") && link.contains("per_page=2"));
    let last: usize = link
        .split(", ")
        .find(|p| p.ends_with("rel=\"last\""))
        .and_then(|p| p.split("page=").last())
        .and_then(|p| p.split(['>', '&']).next())
        .and_then(|n| n.parse().ok())
        .expect("last page");
    assert!(last >= 3, "five labels or more over pages of two");
    let r = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/labels?per_page=2&page={last}"),
            "owner",
        )
        .await;
    let link = r.header("link").expect("link header");
    assert!(
        link.contains("rel=\"prev\"")
            && link.contains("rel=\"first\"")
            && !link.contains("rel=\"next\"")
    );
}

#[tokio::test]
async fn root_aliases_and_meta() {
    let w = world().await;
    let r = w.get("/user", "owner").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json["login"], "owner");
    let r = w.get("/api/v3/meta", "owner").await;
    has_keys(
        &r.json,
        &["verifiable_password_authentication", "installed_version"],
    );
    let r = w.get("/api/v3/", "owner").await;
    assert!(
        r.json["repository_url"]
            .as_str()
            .unwrap()
            .ends_with("/repos/{owner}/{repo}")
    );
    let r = w.get("/zen", "owner").await;
    assert_eq!(r.status, 200);
}

// ----- users and orgs -----------------------------------------------------------------------

#[tokio::test]
async fn users_and_orgs() {
    let w = world().await;
    let me = w.get("/api/v3/user", "writer").await;
    has_keys(
        &me.json,
        &[
            "login",
            "id",
            "node_id",
            "avatar_url",
            "html_url",
            "type",
            "site_admin",
            "name",
            "public_repos",
            "followers",
            "created_at",
        ],
    );
    assert_eq!(me.json["type"], "User");
    let u = w.get("/api/v3/users/reader", "writer").await;
    assert_eq!(u.json["login"], "reader");
    let agent = w.get("/api/v3/users/agent", "writer").await;
    assert_eq!(agent.json["type"], "Bot");
    let o = w.get(&format!("/api/v3/users/{ORG}"), "writer").await;
    assert_eq!(o.json["type"], "Organization");
    let o = w.get(&format!("/api/v3/orgs/{ORG}"), "writer").await;
    has_keys(
        &o.json,
        &[
            "login",
            "id",
            "node_id",
            "url",
            "repos_url",
            "members_url",
            "html_url",
            "type",
        ],
    );
    let orgs = w.get("/api/v3/user/orgs", "writer").await;
    assert_eq!(orgs.json[0]["login"], ORG);
    let members = w
        .get(&format!("/api/v3/orgs/{ORG}/members"), "writer")
        .await;
    let logins: Vec<&str> = members
        .json
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["login"].as_str().unwrap())
        .collect();
    assert!(logins.contains(&"owner") && logins.contains(&"reader"));
    let r = w
        .get(&format!("/api/v3/orgs/{ORG}/members/reader"), "writer")
        .await;
    assert_eq!(r.status, 204);
    let r = w
        .get(&format!("/api/v3/orgs/{ORG}/members/outsider"), "writer")
        .await;
    assert_eq!(r.status, 404);
    let r = w
        .get(&format!("/api/v3/orgs/{ORG}/memberships/owner"), "owner")
        .await;
    assert_eq!(r.json["role"], "admin");
}

#[tokio::test]
async fn teams_and_collaborators() {
    let w = world().await;
    w.repo("app").await;
    let t = w.token("owner");
    let r = w
        .call(
            "POST",
            &format!("/api/v3/orgs/{ORG}/teams"),
            Some(&t),
            Some(json!({"name": "Core", "privacy": "closed"})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    has_keys(
        &r.json,
        &["id", "node_id", "name", "slug", "privacy", "members_url"],
    );
    let slug = r.json["slug"].as_str().unwrap().to_string();
    let r = w
        .call(
            "PUT",
            &format!("/api/v3/orgs/{ORG}/teams/{slug}/memberships/writer"),
            Some(&t),
            Some(json!({"role": "member"})),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    let r = w
        .get(&format!("/api/v3/orgs/{ORG}/teams/{slug}/members"), "owner")
        .await;
    assert!(
        r.json
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["login"] == "writer")
    );
    let r = w
        .call(
            "PUT",
            &format!("/api/v3/orgs/{ORG}/teams/{slug}/repos/{ORG}/app"),
            Some(&t),
            Some(json!({"permission": "push"})),
        )
        .await;
    assert_eq!(r.status, 204, "{:?}", r.json);
    let r = w
        .get(&format!("/api/v3/repos/{ORG}/app/teams"), "owner")
        .await;
    assert_eq!(r.json[0]["slug"], slug.as_str());
    let r = w
        .call(
            "PUT",
            &format!("/api/v3/repos/{ORG}/app/collaborators/outsider"),
            Some(&t),
            Some(json!({"permission": "triage"})),
        )
        .await;
    assert!(r.status == 201 || r.status == 204, "{:?}", r.json);
    let r = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/collaborators/owner/permission"),
            "owner",
        )
        .await;
    assert_eq!(r.json["permission"], "admin");
    has_keys(&r.json, &["permission", "role_name", "user"]);
    let r = w
        .get(&format!("/api/v3/repos/{ORG}/app/collaborators"), "owner")
        .await;
    assert!(
        r.json
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c.get("permissions").is_some())
    );
}

// ----- repositories, contents and git data --------------------------------------------------

#[tokio::test]
async fn repos_create_get_edit_list_delete() {
    let w = world().await;
    let created = w.repo("app").await;
    has_keys(&created, REPO_KEYS);
    assert_eq!(created["full_name"], format!("{ORG}/app"));
    assert_eq!(created["owner"]["type"], "Organization");
    let r = w.get(&format!("/api/v3/repos/{ORG}/app"), "owner").await;
    assert_eq!(r.json["permissions"]["admin"], true);
    assert_eq!(r.json["default_branch"], "main");
    let t = w.token("owner");
    let r = w
        .call(
            "PATCH",
            &format!("/api/v3/repos/{ORG}/app"),
            Some(&t),
            Some(json!({"description": "edited", "homepage": "https://x.test", "allow_rebase_merge": false, "has_wiki": true})),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["description"], "edited");
    assert_eq!(r.json["allow_rebase_merge"], false);
    assert_eq!(r.json["has_wiki"], true);
    let r = w
        .call(
            "PUT",
            &format!("/api/v3/repos/{ORG}/app/topics"),
            Some(&t),
            Some(json!({"names": ["rust", "vcs"]})),
        )
        .await;
    assert_eq!(r.json["names"], json!(["rust", "vcs"]));
    let list = w.get(&format!("/api/v3/orgs/{ORG}/repos"), "owner").await;
    assert_eq!(list.json.as_array().unwrap().len(), 1);
    let mine = w.get("/api/v3/user/repos", "owner").await;
    assert_eq!(mine.json[0]["name"], "app");
    let r = w.get(&format!("/api/v3/repos/{ORG}/app"), "outsider").await;
    assert_eq!(r.status, 200, "public repos are readable");
    assert_eq!(r.json["permissions"]["push"], false);
    let r = w
        .call(
            "POST",
            &format!("/api/v3/orgs/{ORG}/repos"),
            Some(&t),
            Some(json!({"name": "secret", "private": true})),
        )
        .await;
    assert_eq!(r.json["private"], true);
    assert_eq!(r.json["visibility"], "private");
    let r = w
        .get(&format!("/api/v3/repos/{ORG}/secret"), "outsider")
        .await;
    assert_eq!(r.status, 404, "private repos are invisible to outsiders");
    let r = w.get(&format!("/api/v3/orgs/{ORG}/repos"), "owner").await;
    assert_eq!(r.json.as_array().unwrap().len(), 2);
    let r = w
        .get(&format!("/api/v3/orgs/{ORG}/repos?type=private"), "owner")
        .await;
    assert_eq!(r.json[0]["name"], "secret");
    let r = w
        .call(
            "DELETE",
            &format!("/api/v3/repos/{ORG}/app"),
            Some(&t),
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{:?}", r.json);
    let r = w.get(&format!("/api/v3/repos/{ORG}/app"), "owner").await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn contents_and_git_data_use_real_git_ids() {
    let w = world().await;
    w.repo("app").await;
    let r = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/contents/README.md"),
            "owner",
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "type",
            "encoding",
            "size",
            "name",
            "path",
            "content",
            "sha",
            "url",
            "git_url",
            "html_url",
            "download_url",
            "_links",
        ],
    );
    // The blob sha is git's: sha1("blob 8\0# hello\n").
    let sha = r.json["sha"].as_str().unwrap().to_string();
    assert_eq!(sha, git_blob_sha(b"# hello\n"));
    let raw = w
        .send(
            "GET",
            &format!("/api/v3/repos/{ORG}/app/contents/README.md"),
            Some(&w.token("owner")),
            None,
            &[("accept", "application/vnd.github.raw")],
        )
        .await;
    assert_eq!(raw.body, b"# hello\n");
    let root = w
        .get(&format!("/api/v3/repos/{ORG}/app/contents"), "owner")
        .await;
    assert_eq!(root.json[0]["name"], "README.md");
    let readme = w
        .get(&format!("/api/v3/repos/{ORG}/app/readme"), "owner")
        .await;
    assert_eq!(readme.json["path"], "README.md");
    // Update needs the current sha; a stale one conflicts.
    let t = w.token("owner");
    let url = format!("/api/v3/repos/{ORG}/app/contents/README.md");
    let r = w
        .call(
            "PUT",
            &url,
            Some(&t),
            Some(json!({"message": "m", "content": "eA=="})),
        )
        .await;
    assert_eq!(r.status, 422, "{:?}", r.json);
    let r = w.call("PUT", &url, Some(&t), Some(json!({"message": "m", "content": "eA==", "sha": "0000000000000000000000000000000000000000"}))).await;
    assert_eq!(r.status, 409);
    let new_commit = w
        .put_file("app", "main", "README.md", "# hi\n", "Edit readme")
        .await;
    // Git data.
    let c = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/git/commits/{new_commit}"),
            "owner",
        )
        .await;
    has_keys(
        &c.json,
        &[
            "sha",
            "node_id",
            "url",
            "author",
            "committer",
            "message",
            "tree",
            "parents",
            "verification",
        ],
    );
    assert_eq!(c.json["parents"].as_array().unwrap().len(), 1);
    let tree = c.json["tree"]["sha"].as_str().unwrap().to_string();
    let tr = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/git/trees/{tree}?recursive=1"),
            "owner",
        )
        .await;
    assert_eq!(tr.json["tree"][0]["path"], "README.md");
    assert_eq!(tr.json["truncated"], false);
    let blob = tr.json["tree"][0]["sha"].as_str().unwrap().to_string();
    assert_eq!(blob, git_blob_sha(b"# hi\n"));
    let b = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/git/blobs/{blob}"),
            "owner",
        )
        .await;
    assert_eq!(b.json["encoding"], "base64");
    let refs = w
        .get(&format!("/api/v3/repos/{ORG}/app/git/refs/heads"), "owner")
        .await;
    assert_eq!(refs.json[0]["ref"], "refs/heads/main");
    assert_eq!(refs.json[0]["object"]["sha"], new_commit.as_str());
    // Build a commit through the git data API and move a new branch to it.
    let r = w
        .call(
            "POST",
            &format!("/api/v3/repos/{ORG}/app/git/blobs"),
            Some(&t),
            Some(json!({"content": "data\n", "encoding": "utf-8"})),
        )
        .await;
    assert_eq!(r.status, 201);
    let blob = r.json["sha"].as_str().unwrap().to_string();
    assert_eq!(blob, git_blob_sha(b"data\n"));
    let r = w
        .call("POST", &format!("/api/v3/repos/{ORG}/app/git/trees"), Some(&t), Some(json!({"base_tree": tree, "tree": [{"path": "dir/data.txt", "mode": "100644", "type": "blob", "sha": blob}]})))
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    let new_tree = r.json["sha"].as_str().unwrap().to_string();
    let r = w
        .call(
            "POST",
            &format!("/api/v3/repos/{ORG}/app/git/commits"),
            Some(&t),
            Some(json!({"message": "via git data", "tree": new_tree, "parents": [new_commit]})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    assert_eq!(
        r.json["tree"]["sha"],
        new_tree.as_str(),
        "the commit records the tree built"
    );
    let made = r.json["sha"].as_str().unwrap().to_string();
    let r = w
        .call(
            "POST",
            &format!("/api/v3/repos/{ORG}/app/git/refs"),
            Some(&t),
            Some(json!({"ref": "refs/heads/data", "sha": made})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    let r = w
        .call(
            "POST",
            &format!("/api/v3/repos/{ORG}/app/git/refs"),
            Some(&t),
            Some(json!({"ref": "refs/heads/data", "sha": made})),
        )
        .await;
    assert_eq!(r.status, 422, "Reference already exists");
    let f = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/contents/dir/data.txt?ref=data"),
            "owner",
        )
        .await;
    assert_eq!(f.json["sha"], blob.as_str());
    // Branches, commits and compare.
    let br = w
        .get(&format!("/api/v3/repos/{ORG}/app/branches"), "owner")
        .await;
    let names: Vec<&str> = br
        .json
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["data", "main"]);
    let one = w
        .get(&format!("/api/v3/repos/{ORG}/app/branches/data"), "owner")
        .await;
    has_keys(
        &one.json,
        &[
            "name",
            "commit",
            "_links",
            "protected",
            "protection",
            "protection_url",
        ],
    );
    let commits = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/commits?sha=data"),
            "owner",
        )
        .await;
    assert_eq!(commits.json.as_array().unwrap().len(), 3);
    has_keys(
        &commits.json[0],
        &[
            "sha",
            "node_id",
            "commit",
            "url",
            "html_url",
            "author",
            "committer",
            "parents",
        ],
    );
    let single = w
        .get(&format!("/api/v3/repos/{ORG}/app/commits/{made}"), "owner")
        .await;
    assert_eq!(single.json["files"][0]["filename"], "dir/data.txt");
    assert_eq!(single.json["stats"]["additions"], 1);
    let cmp = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/compare/main...data"),
            "owner",
        )
        .await;
    assert_eq!(cmp.json["status"], "ahead");
    assert_eq!(cmp.json["ahead_by"], 1);
    assert_eq!(cmp.json["files"][0]["status"], "added");
    has_keys(
        &cmp.json,
        &[
            "url",
            "html_url",
            "base_commit",
            "merge_base_commit",
            "status",
            "ahead_by",
            "behind_by",
            "total_commits",
            "commits",
            "files",
        ],
    );
    let diff = w
        .send(
            "GET",
            &format!("/api/v3/repos/{ORG}/app/compare/main...data"),
            Some(&t),
            None,
            &[("accept", "application/vnd.github.v3.diff")],
        )
        .await;
    assert!(String::from_utf8_lossy(&diff.body).contains("+++ b/dir/data.txt"));
    // Delete through the contents API.
    let f = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/contents/dir/data.txt?ref=data"),
            "owner",
        )
        .await;
    let r = w
        .call(
            "DELETE",
            &format!("/api/v3/repos/{ORG}/app/contents/dir/data.txt"),
            Some(&t),
            Some(json!({"message": "rm", "sha": f.json["sha"], "branch": "data"})),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["content"], Value::Null);
    let r = w
        .call(
            "DELETE",
            &format!("/api/v3/repos/{ORG}/app/git/refs/heads/data"),
            Some(&t),
            None,
        )
        .await;
    assert_eq!(r.status, 204);
    let tags = w
        .get(&format!("/api/v3/repos/{ORG}/app/tags"), "owner")
        .await;
    assert_eq!(tags.json, json!([]));
}

fn git_blob_sha(data: &[u8]) -> String {
    bee_git::ObjectFormat::Sha1
        .hash_object(bee_git::ObjectType::Blob, data)
        .unwrap()
        .to_hex()
}

#[tokio::test]
async fn branch_protection_maps_to_policy_land_rules() {
    let w = world().await;
    w.repo("app").await;
    let t = w.token("owner");
    let url = format!("/api/v3/repos/{ORG}/app/branches/main/protection");
    let r = w
        .call(
            "PUT",
            &url,
            Some(&t),
            Some(json!({
                "required_status_checks": {"strict": true, "contexts": ["ci/test"]},
                "enforce_admins": true,
                "required_pull_request_reviews": {"required_approving_review_count": 2, "dismiss_stale_reviews": true},
                "restrictions": null,
                "required_linear_history": true,
            })),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(
        r.json["required_status_checks"]["contexts"],
        json!(["ci/test"])
    );
    assert_eq!(
        r.json["required_pull_request_reviews"]["required_approving_review_count"],
        2
    );
    assert_eq!(r.json["required_linear_history"]["enabled"], true);
    let f = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/contents/.beehive/policy.toml"),
            "owner",
        )
        .await;
    assert_eq!(f.status, 200, "the land rule is written to the policy file");
    let raw = w
        .send(
            "GET",
            &format!("/api/v3/repos/{ORG}/app/contents/.beehive/policy.toml"),
            Some(&t),
            None,
            &[("accept", "application/vnd.github.raw")],
        )
        .await;
    let text = String::from_utf8(raw.body).unwrap();
    assert!(
        text.contains("required_checks = [\"ci/test\"]") && text.contains("count = 2"),
        "{text}"
    );
    bee_policy::Policy::parse(&text).expect("valid policy");
    let r = w.call("DELETE", &url, Some(&t), None).await;
    assert_eq!(r.status, 204);
    let raw = w
        .send(
            "GET",
            &format!("/api/v3/repos/{ORG}/app/contents/.beehive/policy.toml"),
            Some(&t),
            None,
            &[("accept", "application/vnd.github.raw")],
        )
        .await;
    assert!(!String::from_utf8(raw.body).unwrap().contains("[[land]]"));
}

// ----- pull requests ------------------------------------------------------------------------

#[tokio::test]
async fn pulls_create_list_get_update_files_commits() {
    let w = world().await;
    w.repo("app").await;
    let pr = w.pull("app", "feature", "Add feature").await;
    has_keys(&pr, PULL_KEYS);
    assert_eq!(pr["number"], 1);
    assert_eq!(pr["head"]["ref"], "feature");
    assert_eq!(pr["base"]["ref"], "main");
    assert_eq!(pr["head"]["label"], format!("{ORG}:feature"));
    assert_eq!(pr["state"], "open");
    assert_eq!(pr["commits"], 1);
    assert_eq!(pr["changed_files"], 1);
    let head_sha = pr["head"]["sha"].as_str().unwrap().to_string();
    assert_eq!(head_sha.len(), 40);
    let list = w
        .get(&format!("/api/v3/repos/{ORG}/app/pulls"), "owner")
        .await;
    assert_eq!(list.json.as_array().unwrap().len(), 1);
    let filtered = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/pulls?head={ORG}:feature"),
            "owner",
        )
        .await;
    assert_eq!(filtered.json.as_array().unwrap().len(), 1);
    let none = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/pulls?head={ORG}:other"),
            "owner",
        )
        .await;
    assert_eq!(none.json, json!([]));
    let t = w.token("owner");
    // Duplicate head is refused like GitHub.
    let r = w
        .call(
            "POST",
            &format!("/api/v3/repos/{ORG}/app/pulls"),
            Some(&t),
            Some(json!({"title": "dup", "head": "feature", "base": "main"})),
        )
        .await;
    assert_eq!(r.status, 422);
    assert!(
        r.json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("already exists")
    );
    let files = w
        .get(&format!("/api/v3/repos/{ORG}/app/pulls/1/files"), "owner")
        .await;
    assert_eq!(files.json[0]["filename"], "feature.txt");
    has_keys(
        &files.json[0],
        &[
            "sha",
            "filename",
            "status",
            "additions",
            "deletions",
            "changes",
            "blob_url",
            "raw_url",
            "patch",
        ],
    );
    let commits = w
        .get(&format!("/api/v3/repos/{ORG}/app/pulls/1/commits"), "owner")
        .await;
    assert_eq!(commits.json[0]["sha"], head_sha.as_str());
    let diff = w
        .send(
            "GET",
            &format!("/api/v3/repos/{ORG}/app/pulls/1"),
            Some(&t),
            None,
            &[("accept", "application/vnd.github.v3.diff")],
        )
        .await;
    assert!(String::from_utf8_lossy(&diff.body).contains("feature.txt"));
    let r = w
        .call(
            "PATCH",
            &format!("/api/v3/repos/{ORG}/app/pulls/1"),
            Some(&t),
            Some(json!({"title": "Better title", "draft": true})),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["title"], "Better title");
    assert_eq!(r.json["draft"], true);
    let r = w
        .call(
            "PATCH",
            &format!("/api/v3/repos/{ORG}/app/pulls/1"),
            Some(&t),
            Some(json!({"state": "closed"})),
        )
        .await;
    assert_eq!(r.json["state"], "closed");
    assert_eq!(r.json["merged"], false);
    let r = w
        .call(
            "PATCH",
            &format!("/api/v3/repos/{ORG}/app/pulls/1"),
            Some(&t),
            Some(json!({"state": "open"})),
        )
        .await;
    assert_eq!(r.json["state"], "open");
    // The issues API sees the PR with a pull_request key, under the same number.
    let issue = w
        .get(&format!("/api/v3/repos/{ORG}/app/issues/1"), "owner")
        .await;
    assert!(
        issue.json["pull_request"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/pulls/1")
    );
    let issues = w
        .get(&format!("/api/v3/repos/{ORG}/app/issues"), "owner")
        .await;
    assert_eq!(issues.json[0]["number"], 1);
}

#[tokio::test]
async fn pulls_reviews_comments_requested_reviewers_and_merge() {
    let w = world().await;
    w.repo("app").await;
    w.pull("app", "feature", "Add feature").await;
    let t = w.token("owner");
    let base = format!("/api/v3/repos/{ORG}/app/pulls/1");
    let r = w
        .call(
            "POST",
            &format!("{base}/requested_reviewers"),
            Some(&t),
            Some(json!({"reviewers": ["writer"]})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    assert_eq!(r.json["requested_reviewers"][0]["login"], "writer");
    let r = w.get(&format!("{base}/requested_reviewers"), "owner").await;
    assert_eq!(r.json["users"][0]["login"], "writer");
    let r = w
        .call(
            "POST",
            &format!("{base}/reviews"),
            Some(&t),
            Some(json!({"body": "x"})),
        )
        .await;
    assert_eq!(r.status, 422, "pending reviews are not supported");
    let wt = w.token("writer");
    let r = w
        .call(
            "POST",
            &format!("{base}/reviews"),
            Some(&wt),
            Some(json!({"event": "APPROVE", "body": "LGTM"})),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "id",
            "node_id",
            "user",
            "body",
            "state",
            "html_url",
            "pull_request_url",
            "submitted_at",
            "commit_id",
        ],
    );
    assert_eq!(r.json["state"], "APPROVED");
    let review_id = r.json["id"].as_u64().unwrap();
    let r = w.get(&format!("{base}/reviews/{review_id}"), "owner").await;
    assert_eq!(r.json["user"]["login"], "writer");
    let r = w
        .call(
            "POST",
            &format!("{base}/comments"),
            Some(&t),
            Some(json!({"body": "nit", "path": "feature.txt", "line": 1, "side": "RIGHT"})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "id",
            "path",
            "line",
            "side",
            "body",
            "user",
            "commit_id",
            "html_url",
            "pull_request_url",
        ],
    );
    let cid = r.json["id"].as_u64().unwrap();
    let r = w
        .call(
            "POST",
            &format!("{base}/comments/{cid}/replies"),
            Some(&wt),
            Some(json!({"body": "fixed"})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    let r = w.get(&format!("{base}/comments"), "owner").await;
    assert_eq!(r.json.as_array().unwrap().len(), 2);
    // Conversation comments go through the issues API.
    let r = w
        .call(
            "POST",
            &format!("/api/v3/repos/{ORG}/app/issues/1/comments"),
            Some(&t),
            Some(json!({"body": "thanks"})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    let r = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/issues/1/comments"),
            "owner",
        )
        .await;
    assert_eq!(r.json[0]["body"], "thanks");
    // Merge with a stale sha conflicts; then merge for real.
    let r = w
        .call(
            "PUT",
            &format!("{base}/merge"),
            Some(&t),
            Some(json!({"sha": "0".repeat(40)})),
        )
        .await;
    assert_eq!(r.status, 409, "{:?}", r.json);
    let r = w.get(&format!("{base}/merge"), "owner").await;
    assert_eq!(r.status, 404);
    let r = w
        .call(
            "PUT",
            &format!("{base}/merge"),
            Some(&t),
            Some(json!({"merge_method": "squash", "commit_title": "Add feature (#1)"})),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["merged"], true);
    let merged_sha = r.json["sha"].as_str().unwrap().to_string();
    assert_eq!(merged_sha.len(), 40);
    let r = w.get(&format!("{base}/merge"), "owner").await;
    assert_eq!(r.status, 204);
    let pr = w.get(&base, "owner").await;
    assert_eq!(pr.json["merged"], true);
    assert_eq!(pr.json["state"], "closed");
    let main = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/git/ref/heads/main"),
            "owner",
        )
        .await;
    assert_eq!(main.json["object"]["sha"], merged_sha.as_str());
    let f = w
        .get(
            &format!("/api/v3/repos/{ORG}/app/contents/feature.txt"),
            "owner",
        )
        .await;
    assert_eq!(f.status, 200, "the change landed on main");
    let r = w
        .call("PUT", &format!("{base}/merge"), Some(&t), Some(json!({})))
        .await;
    assert_eq!(r.status, 405, "already merged");
}

// ----- issues -------------------------------------------------------------------------------

#[tokio::test]
async fn issues_labels_milestones_assignees_comments_events() {
    let w = world().await;
    w.repo("app").await;
    let t = w.token("owner");
    let base = format!("/api/v3/repos/{ORG}/app");
    let r = w
        .call(
            "POST",
            &format!("{base}/labels"),
            Some(&t),
            Some(
                json!({"name": "crash", "color": "#d73a4a", "description": "Something is broken"}),
            ),
        )
        .await;
    assert_eq!(r.status, 201);
    has_keys(
        &r.json,
        &[
            "id",
            "node_id",
            "url",
            "name",
            "color",
            "default",
            "description",
        ],
    );
    assert_eq!(r.json["color"], "d73a4a");
    let r = w
        .call(
            "POST",
            &format!("{base}/milestones"),
            Some(&t),
            Some(json!({"title": "v1", "due_on": "2030-01-01T00:00:00Z"})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "url",
            "html_url",
            "id",
            "node_id",
            "number",
            "state",
            "title",
            "open_issues",
            "closed_issues",
            "due_on",
        ],
    );
    let ms = r.json["number"].as_u64().unwrap();
    let r = w
        .call("POST", &format!("{base}/issues"), Some(&t), Some(json!({"title": "Crash", "body": "boom", "labels": ["crash"], "assignees": ["writer"], "milestone": ms})))
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    has_keys(&r.json, ISSUE_KEYS);
    assert_eq!(r.json["labels"][0]["name"], "crash");
    assert_eq!(r.json["assignee"]["login"], "writer");
    assert_eq!(r.json["milestone"]["title"], "v1");
    let n = r.json["number"].as_u64().unwrap();
    let r = w.get(&format!("{base}/issues?labels=crash"), "owner").await;
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    let r = w
        .get(&format!("{base}/issues?assignee=reader"), "owner")
        .await;
    assert_eq!(r.json, json!([]));
    let r = w
        .call(
            "POST",
            &format!("{base}/issues/{n}/comments"),
            Some(&t),
            Some(json!({"body": "repro?"})),
        )
        .await;
    assert_eq!(r.status, 201);
    has_keys(
        &r.json,
        &[
            "id",
            "node_id",
            "url",
            "html_url",
            "issue_url",
            "body",
            "user",
            "created_at",
            "updated_at",
            "author_association",
            "reactions",
        ],
    );
    let cid = r.json["id"].as_u64().unwrap();
    let r = w
        .call(
            "PATCH",
            &format!("{base}/issues/comments/{cid}"),
            Some(&t),
            Some(json!({"body": "repro please"})),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["body"], "repro please");
    let r = w
        .get(&format!("{base}/issues/comments/{cid}"), "owner")
        .await;
    assert_eq!(r.json["body"], "repro please");
    let r = w
        .call(
            "POST",
            &format!("{base}/issues/{n}/reactions"),
            Some(&t),
            Some(json!({"content": "+1"})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    let r = w.get(&format!("{base}/issues/{n}"), "owner").await;
    assert_eq!(r.json["reactions"]["+1"], 1);
    assert_eq!(r.json["comments"], 1);
    let r = w
        .call(
            "POST",
            &format!("{base}/issues/{n}/labels"),
            Some(&t),
            Some(json!({"labels": ["needs-triage"]})),
        )
        .await;
    assert_eq!(r.status, 200, "unknown labels are created: {:?}", r.json);
    let r = w
        .call(
            "POST",
            &format!("{base}/issues/{n}/labels"),
            Some(&t),
            Some(json!({"labels": ["needs-triage"]})),
        )
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 2, "{:?}", r.json);
    let r = w
        .call(
            "DELETE",
            &format!("{base}/issues/{n}/labels/crash"),
            Some(&t),
            None,
        )
        .await;
    assert_eq!(r.json[0]["name"], "needs-triage");
    let r = w
        .call(
            "POST",
            &format!("{base}/issues/{n}/assignees"),
            Some(&t),
            Some(json!({"assignees": ["reader"]})),
        )
        .await;
    assert_eq!(r.json["assignees"].as_array().unwrap().len(), 2);
    let r = w
        .call(
            "DELETE",
            &format!("{base}/issues/{n}/assignees"),
            Some(&t),
            Some(json!({"assignees": ["writer"]})),
        )
        .await;
    assert_eq!(r.json["assignees"][0]["login"], "reader");
    let r = w
        .call(
            "PATCH",
            &format!("{base}/issues/{n}"),
            Some(&t),
            Some(json!({"state": "closed", "state_reason": "not_planned"})),
        )
        .await;
    assert_eq!(r.json["state"], "closed");
    assert_eq!(r.json["state_reason"], "not_planned");
    let r = w.get(&format!("{base}/issues"), "owner").await;
    assert_eq!(r.json, json!([]), "closed issues are hidden by default");
    let r = w.get(&format!("{base}/issues?state=closed"), "owner").await;
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    let ev = w.get(&format!("{base}/issues/{n}/events"), "owner").await;
    let kinds: Vec<&str> = ev
        .json
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"closed"), "{kinds:?}");
    let tl = w.get(&format!("{base}/issues/{n}/timeline"), "owner").await;
    assert!(
        tl.json
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "commented")
    );
    let r = w
        .call(
            "PUT",
            &format!("{base}/issues/{n}/lock"),
            Some(&t),
            Some(json!({"lock_reason": "resolved"})),
        )
        .await;
    assert_eq!(r.status, 204);
    let r = w.get(&format!("{base}/issues/{n}"), "owner").await;
    assert_eq!(r.json["locked"], true);
    let r = w
        .get(&format!("{base}/milestones?state=all"), "owner")
        .await;
    assert_eq!(r.json[0]["closed_issues"], 1);
    let r = w.get(&format!("{base}/assignees"), "owner").await;
    assert!(
        r.json
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["login"] == "writer")
    );
    let r = w
        .call(
            "DELETE",
            &format!("{base}/issues/comments/{cid}"),
            Some(&t),
            None,
        )
        .await;
    assert_eq!(r.status, 204);
}

// ----- search -------------------------------------------------------------------------------

#[tokio::test]
async fn search_issues_repositories_and_code() {
    let w = world().await;
    w.repo("app").await;
    w.put_file("app", "main", "src/lib.rs", "fn needle() {}\n", "Add lib")
        .await;
    w.pull("app", "feature", "Feature PR").await;
    let t = w.token("owner");
    w.call(
        "POST",
        &format!("/api/v3/repos/{ORG}/app/issues"),
        Some(&t),
        Some(json!({"title": "Needle issue"})),
    )
    .await;
    let r = w
        .get(
            &format!("/api/v3/search/issues?q=repo:{ORG}/app+is:pr"),
            "owner",
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    has_keys(&r.json, &["total_count", "incomplete_results", "items"]);
    assert_eq!(r.json["total_count"], 1);
    assert!(r.json["items"][0]["pull_request"].is_object());
    let r = w
        .get(
            &format!("/api/v3/search/issues?q=repo:{ORG}/app+is:issue"),
            "owner",
        )
        .await;
    assert_eq!(r.json["total_count"], 1);
    assert_eq!(r.json["items"][0]["title"], "Needle issue");
    assert_eq!(r.header("x-ratelimit-resource"), Some("search"));
    let r = w
        .get("/api/v3/search/repositories?q=test+org:acme", "owner")
        .await;
    assert_eq!(r.json["items"][0]["full_name"], format!("{ORG}/app"));
    let r = w
        .get(
            &format!("/api/v3/search/code?q=needle+repo:{ORG}/app"),
            "owner",
        )
        .await;
    assert_eq!(r.json["total_count"], 1, "{:?}", r.json);
    assert_eq!(r.json["items"][0]["path"], "src/lib.rs");
    has_keys(
        &r.json["items"][0],
        &[
            "name",
            "path",
            "sha",
            "url",
            "git_url",
            "html_url",
            "repository",
            "score",
        ],
    );
    let r = w.get("/api/v3/search/issues", "owner").await;
    assert_eq!(r.status, 422);
}

// ----- releases -----------------------------------------------------------------------------

#[tokio::test]
async fn releases_and_assets_through_the_uploads_path() {
    let w = world().await;
    w.repo("app").await;
    let t = w.token("owner");
    let base = format!("/api/v3/repos/{ORG}/app");
    let r = w
        .call("POST", &format!("{base}/releases"), Some(&t), Some(json!({"tag_name": "v1.0.0", "name": "One", "body": "notes", "target_commitish": "main"})))
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "url",
            "html_url",
            "assets_url",
            "upload_url",
            "tarball_url",
            "zipball_url",
            "id",
            "node_id",
            "tag_name",
            "target_commitish",
            "name",
            "body",
            "draft",
            "prerelease",
            "created_at",
            "published_at",
            "author",
            "assets",
        ],
    );
    let id = r.json["id"].as_u64().unwrap();
    let upload = r.json["upload_url"]
        .as_str()
        .unwrap()
        .replace("{?name,label}", "");
    assert!(
        upload.contains("/api/uploads/repos/acme/app/releases/"),
        "{upload}"
    );
    let path = upload.split_once("ghe.example.test").unwrap().1.to_string();
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("{path}?name=tool.tar.gz&label=Tool"))
        .header("authorization", format!("token {t}"))
        .header("content-type", "application/gzip")
        .header("host", "ghe.example.test")
        .body(axum::body::Body::from(&b"binary-bytes"[..]))
        .unwrap();
    let r = w.raw(req).await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "url",
            "browser_download_url",
            "id",
            "node_id",
            "name",
            "label",
            "state",
            "content_type",
            "size",
            "download_count",
            "uploader",
            "digest",
        ],
    );
    assert_eq!(r.json["size"], 12);
    assert_eq!(r.json["state"], "uploaded");
    let asset = r.json["id"].as_u64().unwrap();
    let r = w.get(&format!("{base}/releases/{id}"), "owner").await;
    assert_eq!(r.json["assets"][0]["name"], "tool.tar.gz");
    let r = w
        .get(&format!("{base}/releases/tags/v1.0.0"), "owner")
        .await;
    assert_eq!(r.json["id"], id);
    let r = w.get(&format!("{base}/releases/latest"), "owner").await;
    assert_eq!(r.json["tag_name"], "v1.0.0");
    let r = w
        .send(
            "GET",
            &format!("{base}/releases/assets/{asset}"),
            Some(&t),
            None,
            &[("accept", "application/octet-stream")],
        )
        .await;
    assert_eq!(r.status, 302);
    let loc = r.header("location").unwrap().to_string();
    assert_eq!(
        w.assets.get(&loc).as_deref(),
        Some(&b"binary-bytes"[..]),
        "the bytes are where the signed URL points"
    );
    let tags = w.get(&format!("{base}/tags"), "owner").await;
    assert_eq!(tags.json[0]["name"], "v1.0.0");
    let r = w
        .call(
            "PATCH",
            &format!("{base}/releases/{id}"),
            Some(&t),
            Some(json!({"name": "Uno", "prerelease": true})),
        )
        .await;
    assert_eq!(r.json["name"], "Uno");
    assert_eq!(r.json["prerelease"], true);
    let r = w.get(&format!("{base}/releases"), "owner").await;
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    let r = w
        .call("DELETE", &format!("{base}/releases/{id}"), Some(&t), None)
        .await;
    assert_eq!(r.status, 204);
}

// ----- actions ------------------------------------------------------------------------------

const WORKFLOW: &str = "name: CI\non:\n  push:\n  workflow_dispatch:\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";

#[tokio::test]
async fn actions_workflows_runs_jobs_dispatch_cancel_rerun() {
    let w = world().await;
    w.repo("app").await;
    w.put_file(
        "app",
        "main",
        ".github/workflows/ci.yml",
        WORKFLOW,
        "Add CI",
    )
    .await;
    let t = w.token("owner");
    let base = format!("/api/v3/repos/{ORG}/app/actions");
    let r = w.get(&format!("{base}/workflows"), "owner").await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["total_count"], 1);
    has_keys(
        &r.json["workflows"][0],
        &[
            "id",
            "node_id",
            "name",
            "path",
            "state",
            "created_at",
            "updated_at",
            "url",
            "html_url",
            "badge_url",
        ],
    );
    assert_eq!(r.json["workflows"][0]["path"], ".github/workflows/ci.yml");
    let wid = r.json["workflows"][0]["id"].as_u64().unwrap();
    let r = w.get(&format!("{base}/workflows/ci.yml"), "owner").await;
    assert_eq!(r.json["id"], wid);
    let r = w
        .call(
            "POST",
            &format!("{base}/workflows/{wid}/dispatches"),
            Some(&t),
            Some(json!({"ref": "main"})),
        )
        .await;
    assert_eq!(r.status, 204, "{:?}", r.json);
    let r = w.get(&format!("{base}/runs"), "owner").await;
    assert!(r.json["total_count"].as_u64().unwrap() >= 1, "{:?}", r.json);
    let run = r.json["workflow_runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["event"] == "workflow_dispatch")
        .expect("dispatched run")
        .clone();
    has_keys(
        &run,
        &[
            "id",
            "name",
            "node_id",
            "head_branch",
            "head_sha",
            "path",
            "display_title",
            "run_number",
            "event",
            "status",
            "conclusion",
            "workflow_id",
            "url",
            "html_url",
            "created_at",
            "updated_at",
            "actor",
            "run_attempt",
            "jobs_url",
            "logs_url",
            "head_commit",
            "repository",
        ],
    );
    assert_eq!(run["workflow_id"], wid);
    assert_eq!(run["head_branch"], "main");
    let rid = run["id"].as_u64().unwrap();
    let r = w
        .get(
            &format!("{base}/workflows/ci.yml/runs?event=workflow_dispatch"),
            "owner",
        )
        .await;
    assert!(
        r.json["workflow_runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == rid)
    );
    let r = w.get(&format!("{base}/runs/{rid}"), "owner").await;
    assert_eq!(r.json["id"], rid);
    let r = w.get(&format!("{base}/runs/{rid}/jobs"), "owner").await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    has_keys(&r.json, &["total_count", "jobs"]);
    if let Some(job) = r.json["jobs"].as_array().unwrap().first() {
        has_keys(
            job,
            &[
                "id",
                "run_id",
                "status",
                "conclusion",
                "name",
                "steps",
                "labels",
                "html_url",
            ],
        );
        let jid = job["id"].as_u64().unwrap();
        let r = w.get(&format!("{base}/jobs/{jid}"), "owner").await;
        assert_eq!(r.json["run_id"], rid);
    }
    let r = w
        .call("POST", &format!("{base}/runs/{rid}/cancel"), Some(&t), None)
        .await;
    assert_eq!(r.status, 202, "{:?}", r.json);
    let r = w
        .call("POST", &format!("{base}/runs/{rid}/rerun"), Some(&t), None)
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    let r = w.get(&format!("{base}/artifacts"), "owner").await;
    assert_eq!(r.json["total_count"], 0);
}

#[tokio::test]
async fn actions_secrets_are_sealed_boxes_and_variables_round_trip() {
    use base64::Engine as _;
    let w = world().await;
    w.repo("app").await;
    let t = w.token("owner");
    let base = format!("/api/v3/repos/{ORG}/app/actions");
    let r = w.get(&format!("{base}/secrets/public-key"), "owner").await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    has_keys(&r.json, &["key_id", "key"]);
    let pk: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(r.json["key"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let sealed = bee_compat::sealed::seal(&pk, &[42u8; 32], b"s3cret").unwrap();
    let enc = base64::engine::general_purpose::STANDARD.encode(sealed);
    let r = w
        .call(
            "PUT",
            &format!("{base}/secrets/API_TOKEN"),
            Some(&t),
            Some(json!({"encrypted_value": enc, "key_id": r.json["key_id"]})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    let r = w
        .call(
            "PUT",
            &format!("{base}/secrets/API_TOKEN"),
            Some(&t),
            Some(json!({"encrypted_value": enc})),
        )
        .await;
    assert_eq!(r.status, 204, "an update answers 204");
    let r = w
        .call(
            "PUT",
            &format!("{base}/secrets/BAD"),
            Some(&t),
            Some(json!({"encrypted_value": "AAAA"})),
        )
        .await;
    assert_eq!(r.status, 422, "garbage does not decrypt");
    let r = w.get(&format!("{base}/secrets"), "owner").await;
    assert_eq!(r.json["total_count"], 1);
    assert_eq!(r.json["secrets"][0]["name"], "API_TOKEN");
    assert!(r.json["secrets"][0].get("value").is_none());
    let r = w.get(&format!("{base}/secrets/API_TOKEN"), "owner").await;
    has_keys(&r.json, &["name", "created_at", "updated_at"]);
    let r = w.get(&format!("{base}/secrets"), "reader").await;
    assert!(
        r.status == 403 || r.status == 404,
        "readers cannot list secrets: {}",
        r.status
    );
    let r = w
        .call(
            "DELETE",
            &format!("{base}/secrets/API_TOKEN"),
            Some(&t),
            None,
        )
        .await;
    assert_eq!(r.status, 204);
    let r = w
        .call(
            "POST",
            &format!("{base}/variables"),
            Some(&t),
            Some(json!({"name": "REGION", "value": "eu"})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    let r = w
        .call(
            "PATCH",
            &format!("{base}/variables/REGION"),
            Some(&t),
            Some(json!({"value": "us"})),
        )
        .await;
    assert_eq!(r.status, 204, "{:?}", r.json);
    let r = w.get(&format!("{base}/variables/REGION"), "owner").await;
    assert_eq!(r.json["value"], "us");
    let r = w.get(&format!("{base}/variables"), "owner").await;
    assert_eq!(r.json["total_count"], 1);
    let r = w
        .get(
            &format!("/api/v3/orgs/{ORG}/actions/secrets/public-key"),
            "owner",
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
}

#[tokio::test]
async fn environments_and_deployments() {
    let w = world().await;
    w.repo("app").await;
    let t = w.token("owner");
    let base = format!("/api/v3/repos/{ORG}/app");
    let r = w
        .call("PUT", &format!("{base}/environments/production"), Some(&t), Some(json!({"wait_timer": 5, "deployment_branch_policy": {"protected_branches": true, "custom_branch_policies": false}})))
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "id",
            "node_id",
            "name",
            "url",
            "html_url",
            "created_at",
            "updated_at",
            "protection_rules",
            "deployment_branch_policy",
        ],
    );
    assert!(
        r.json["protection_rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["type"] == "wait_timer" && p["wait_timer"] == 5)
    );
    let r = w.get(&format!("{base}/environments"), "owner").await;
    assert_eq!(r.json["total_count"], 1);
    let r = w.get(&format!("{base}/deployments"), "owner").await;
    assert_eq!(r.json, json!([]));
    let r = w
        .call("POST", &format!("{base}/deployments"), Some(&t), Some(json!({"ref": "main", "environment": "staging", "required_contexts": [], "payload": {"k": 1}})))
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "url",
            "id",
            "node_id",
            "sha",
            "ref",
            "task",
            "payload",
            "environment",
            "creator",
            "created_at",
            "statuses_url",
            "repository_url",
        ],
    );
    assert_eq!(r.json["payload"]["k"], 1);
    assert_eq!(r.json["sha"].as_str().unwrap().len(), 40);
    let id = r.json["id"].as_u64().unwrap();
    let r = w
        .call(
            "POST",
            &format!("{base}/deployments/{id}/statuses"),
            Some(&t),
            Some(json!({"state": "success", "environment_url": "https://staging.test"})),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    assert_eq!(r.json["state"], "success");
    let sid = r.json["id"].as_u64().unwrap();
    let r = w
        .get(&format!("{base}/deployments/{id}/statuses/{sid}"), "owner")
        .await;
    assert_eq!(r.json["environment_url"], "https://staging.test");
    let r = w
        .get(&format!("{base}/deployments?environment=staging"), "owner")
        .await;
    assert_eq!(r.json[0]["id"], id);
    let r = w
        .call(
            "DELETE",
            &format!("{base}/environments/production"),
            Some(&t),
            None,
        )
        .await;
    assert_eq!(r.status, 204);
}

// ----- checks and statuses ------------------------------------------------------------------

#[tokio::test]
async fn statuses_and_check_runs() {
    let w = world().await;
    w.repo("app").await;
    let t = w.token("owner");
    let base = format!("/api/v3/repos/{ORG}/app");
    let sha = w
        .get(&format!("{base}/git/ref/heads/main"), "owner")
        .await
        .json["object"]["sha"]
        .as_str()
        .unwrap()
        .to_string();
    let r = w
        .call("POST", &format!("{base}/statuses/{sha}"), Some(&t), Some(json!({"state": "pending", "context": "ci/lint", "target_url": "https://ci.test/1"})))
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "url",
            "id",
            "node_id",
            "state",
            "description",
            "target_url",
            "context",
            "created_at",
            "updated_at",
            "creator",
        ],
    );
    w.call(
        "POST",
        &format!("{base}/statuses/{sha}"),
        Some(&t),
        Some(json!({"state": "success", "context": "ci/lint"})),
    )
    .await;
    let r = w.get(&format!("{base}/commits/main/status"), "owner").await;
    assert_eq!(r.json["state"], "success", "{:?}", r.json);
    assert_eq!(r.json["total_count"], 1);
    let r = w
        .get(&format!("{base}/commits/{sha}/statuses"), "owner")
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 2);
    let r = w
        .call("POST", &format!("{base}/check-runs"), Some(&t), Some(json!({"name": "test", "head_sha": sha, "status": "in_progress", "output": {"title": "Tests", "summary": "running"}})))
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "id",
            "head_sha",
            "node_id",
            "external_id",
            "url",
            "html_url",
            "details_url",
            "status",
            "conclusion",
            "started_at",
            "completed_at",
            "output",
            "name",
            "check_suite",
            "app",
            "pull_requests",
        ],
    );
    assert_eq!(r.json["status"], "in_progress");
    let id = r.json["id"].as_u64().unwrap();
    let r = w
        .call("PATCH", &format!("{base}/check-runs/{id}"), Some(&t), Some(json!({"conclusion": "failure", "output": {"title": "Tests", "summary": "1 failed", "annotations": [{"path": "README.md", "start_line": 1, "end_line": 1, "annotation_level": "failure", "message": "bad"}]}})))
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["status"], "completed");
    assert_eq!(r.json["conclusion"], "failure");
    let r = w
        .get(&format!("{base}/check-runs/{id}/annotations"), "owner")
        .await;
    assert_eq!(r.json[0]["annotation_level"], "failure");
    let r = w
        .get(&format!("{base}/commits/{sha}/check-runs"), "owner")
        .await;
    assert_eq!(r.json["total_count"], 1);
    assert_eq!(r.json["check_runs"][0]["id"], id);
}

// ----- webhooks -----------------------------------------------------------------------------

#[tokio::test]
async fn webhooks_crud_ping_and_deliveries() {
    let w = world().await;
    w.repo("app").await;
    let t = w.token("owner");
    let base = format!("/api/v3/repos/{ORG}/app/hooks");
    let r = w
        .call("POST", &base, Some(&t), Some(json!({"name": "web", "active": true, "events": ["push", "pull_request"], "config": {"url": "http://127.0.0.1:9/hook", "content_type": "json", "secret": "s"}})))
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "type",
            "id",
            "name",
            "active",
            "events",
            "config",
            "updated_at",
            "created_at",
            "url",
            "test_url",
            "ping_url",
            "deliveries_url",
            "last_response",
        ],
    );
    let mut events: Vec<&str> = r.json["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect();
    events.sort();
    assert_eq!(events, vec!["pull_request", "push"]);
    assert!(
        r.json["config"].get("secret").is_none(),
        "secrets are write-only"
    );
    let id = r.json["id"].as_u64().unwrap();
    let r = w.get(&format!("{base}/{id}"), "owner").await;
    assert_eq!(r.json["config"]["url"], "http://127.0.0.1:9/hook");
    let r = w
        .call(
            "PATCH",
            &format!("{base}/{id}"),
            Some(&t),
            Some(json!({"active": false, "add_events": ["issues"]})),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["active"], false);
    assert!(
        r.json["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "issues")
    );
    let r = w
        .call(
            "PATCH",
            &format!("{base}/{id}/config"),
            Some(&t),
            Some(json!({"url": "http://127.0.0.1:9/other"})),
        )
        .await;
    assert_eq!(r.json["url"], "http://127.0.0.1:9/other");
    let r = w
        .call("POST", &format!("{base}/{id}/pings"), Some(&t), None)
        .await;
    assert_eq!(r.status, 204, "{:?}", r.json);
    let r = w.get(&format!("{base}/{id}/deliveries"), "owner").await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    if let Some(d) = r.json.as_array().unwrap().first() {
        has_keys(
            d,
            &[
                "id",
                "guid",
                "delivered_at",
                "redelivery",
                "duration",
                "status",
                "status_code",
                "event",
                "action",
            ],
        );
        assert_eq!(d["event"], "ping");
    }
    let r = w.get(&base, "owner").await;
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    let r = w
        .call("DELETE", &format!("{base}/{id}"), Some(&t), None)
        .await;
    assert_eq!(r.status, 204);
    let r = w.get(&format!("{base}/{id}"), "owner").await;
    assert_eq!(r.status, 404);
}

// ----- activity: stars, watching, notifications, gists --------------------------------------

#[tokio::test]
async fn stars_watching_and_gists() {
    let w = world().await;
    w.repo("app").await;
    let t = w.token("writer");
    let r = w
        .call(
            "PUT",
            &format!("/api/v3/user/starred/{ORG}/app"),
            Some(&t),
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{:?}", r.json);
    let r = w
        .call(
            "GET",
            &format!("/api/v3/user/starred/{ORG}/app"),
            Some(&t),
            None,
        )
        .await;
    assert_eq!(r.status, 204);
    let r = w.call("GET", "/api/v3/user/starred", Some(&t), None).await;
    assert_eq!(r.json[0]["full_name"], format!("{ORG}/app"));
    let r = w
        .get(&format!("/api/v3/repos/{ORG}/app/stargazers"), "owner")
        .await;
    assert_eq!(r.json[0]["login"], "writer");
    let r = w
        .call(
            "DELETE",
            &format!("/api/v3/user/starred/{ORG}/app"),
            Some(&t),
            None,
        )
        .await;
    assert_eq!(r.status, 204);
    let r = w
        .call(
            "PUT",
            &format!("/api/v3/repos/{ORG}/app/subscription"),
            Some(&t),
            Some(json!({"subscribed": true})),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["subscribed"], true);
    let r = w
        .call(
            "GET",
            &format!("/api/v3/repos/{ORG}/app/subscription"),
            Some(&t),
            None,
        )
        .await;
    assert_eq!(r.json["subscribed"], true);
    let r = w.call("GET", "/api/v3/notifications", Some(&t), None).await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    let r = w
        .call("POST", "/api/v3/gists", Some(&t), Some(json!({"description": "snippet", "public": false, "files": {"a.txt": {"content": "hello"}}})))
        .await;
    assert_eq!(r.status, 201, "{:?}", r.json);
    has_keys(
        &r.json,
        &[
            "url",
            "forks_url",
            "commits_url",
            "id",
            "node_id",
            "git_pull_url",
            "html_url",
            "files",
            "public",
            "created_at",
            "updated_at",
            "description",
            "comments",
            "owner",
        ],
    );
    assert_eq!(r.json["files"]["a.txt"]["content"], "hello");
    let id = r.json["id"].as_str().unwrap().to_string();
    let r = w
        .call(
            "PATCH",
            &format!("/api/v3/gists/{id}"),
            Some(&t),
            Some(json!({"files": {"a.txt": {"content": "bye"}}})),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["files"]["a.txt"]["content"], "bye");
    let r = w.call("GET", "/api/v3/gists", Some(&t), None).await;
    assert_eq!(r.json[0]["id"], id.as_str());
    let r = w
        .call(
            "GET",
            &format!("/api/v3/gists/{id}/commits"),
            Some(&t),
            None,
        )
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 2, "{:?}", r.json);
    let r = w
        .call("DELETE", &format!("/api/v3/gists/{id}"), Some(&t), None)
        .await;
    assert_eq!(r.status, 204);
}

// ----- GraphQL ------------------------------------------------------------------------------

/// The shape of `gh pr list --json number,title,state,headRefName,author,isDraft,labels`.
const PR_LIST: &str = r#"
query PullRequestList($owner: String!, $repo: String!, $limit: Int!, $endCursor: String, $baseBranch: String, $state: [PullRequestState!] = OPEN) {
  repository(owner: $owner, name: $repo) {
    pullRequests(states: $state, baseRefName: $baseBranch, first: $limit, after: $endCursor, orderBy: {field: CREATED_AT, direction: DESC}) {
      totalCount
      nodes { ...pr }
      pageInfo { hasNextPage endCursor }
    }
  }
}
fragment pr on PullRequest {
  number title state url headRefName baseRefName isDraft createdAt
  author { login ...on User { id name } }
  labels(first: 100) { nodes { id name color } totalCount }
}"#;

#[tokio::test]
async fn graphql_pr_list_view_status_checks_and_merge() {
    let w = world().await;
    w.repo("app").await;
    w.pull("app", "feature", "Add feature").await;
    let vars = json!({"owner": ORG, "repo": "app", "limit": 30});
    let r = w.gql("owner", PR_LIST, vars).await;
    assert!(r.get("errors").is_none(), "{r}");
    let prs = &r["data"]["repository"]["pullRequests"];
    assert_eq!(prs["totalCount"], 1);
    let pr = &prs["nodes"][0];
    assert_eq!(pr["number"], 1);
    assert_eq!(pr["state"], "OPEN");
    assert_eq!(pr["headRefName"], "feature");
    assert_eq!(pr["author"]["login"], "owner");
    assert_eq!(prs["pageInfo"]["hasNextPage"], false);
    // `gh pr view`.
    let view = r#"query PullRequestByNumber($owner: String!, $repo: String!, $pr_number: Int!) {
      repository(owner: $owner, name: $repo) {
        pullRequest(number: $pr_number) {
          id url number title state body isDraft closed merged mergeable mergeStateStatus reviewDecision
          additions deletions changedFiles
          author { login }
          headRefName headRefOid baseRefName headRepositoryOwner { id login } headRepository { id name } isCrossRepository maintainerCanModify
          commits(last: 1) { totalCount nodes { commit { oid messageHeadline statusCheckRollup { state } } } }
          files(first: 100) { nodes { path additions deletions } }
          reviewRequests(first: 100) { nodes { requestedReviewer { __typename ...on User { login } ...on Team { slug } } } totalCount }
          reviews(first: 100) { nodes { author { login } state body } totalCount }
          assignees(first: 100) { nodes { login } totalCount }
          milestone { title }
          comments(first: 100) { nodes { author { login } body createdAt } totalCount }
          reactionGroups { content users { totalCount } }
          projectCards(first: 100) { nodes { project { name } } totalCount }
          projectItems(first: 100) { nodes { id } totalCount }
          autoMergeRequest { mergeMethod }
        }
      }
    }"#;
    let r = w
        .gql(
            "owner",
            view,
            json!({"owner": ORG, "repo": "app", "pr_number": 1}),
        )
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    let pr = &r["data"]["repository"]["pullRequest"];
    assert_eq!(pr["mergeable"], "MERGEABLE");
    assert_eq!(pr["changedFiles"], 1);
    assert_eq!(pr["commits"]["totalCount"], 1);
    assert_eq!(pr["headRefOid"].as_str().unwrap().len(), 40);
    assert_eq!(pr["files"]["nodes"][0]["path"], "feature.txt");
    assert_eq!(pr["reactionGroups"].as_array().unwrap().len(), 8);
    // `gh pr checks`: a status context and a check run on the head.
    let head = pr["headRefOid"].as_str().unwrap().to_string();
    let t = w.token("owner");
    w.call(
        "POST",
        &format!("/api/v3/repos/{ORG}/app/statuses/{head}"),
        Some(&t),
        Some(json!({"state": "success", "context": "lint"})),
    )
    .await;
    w.call("POST", &format!("/api/v3/repos/{ORG}/app/check-runs"), Some(&t), Some(json!({"name": "test", "head_sha": head, "status": "completed", "conclusion": "success"}))).await;
    let checks = r#"query PullRequestStatusChecks($owner: String!, $repo: String!, $number: Int!, $endCursor: String) {
      repository(owner: $owner, name: $repo) {
        pullRequest(number: $number) {
          statusCheckRollup: commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 100, after: $endCursor) {
            totalCount checkRunCount checkRunCountsByState { state count } statusContextCount statusContextCountsByState { state count }
            pageInfo { hasNextPage endCursor }
            nodes {
              __typename
              ...on StatusContext { context state targetUrl createdAt description isRequired(pullRequestNumber: $number) }
              ...on CheckRun { name checkSuite { workflowRun { event workflow { name } } } status conclusion startedAt completedAt detailsUrl isRequired(pullRequestNumber: $number) }
            }
          } } } } }
        }
      }
    }"#;
    let r = w
        .gql(
            "owner",
            checks,
            json!({"owner": ORG, "repo": "app", "number": 1}),
        )
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    let ctx = &r["data"]["repository"]["pullRequest"]["statusCheckRollup"]["nodes"][0]["commit"]["statusCheckRollup"]
        ["contexts"];
    assert_eq!(ctx["totalCount"], 2);
    assert_eq!(ctx["checkRunCount"], 1);
    let kinds: Vec<&str> = ctx["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["__typename"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["CheckRun", "StatusContext"]);
    assert_eq!(ctx["nodes"][0]["conclusion"], "SUCCESS");
    assert_eq!(ctx["nodes"][1]["state"], "SUCCESS");
    // `gh pr status`: viewer and search.
    let status = r#"query PullRequestStatus($owner: String!, $repo: String!, $headRefName: String!, $viewerQuery: String!, $reviewerQuery: String!, $per_page: Int = 10) {
      repository(owner: $owner, name: $repo) {
        defaultBranchRef { name }
        pullRequests(headRefName: $headRefName, first: $per_page, orderBy: {field: CREATED_AT, direction: DESC}) { totalCount edges { node { number title state } } }
      }
      viewerCreated: search(query: $viewerQuery, type: ISSUE, first: $per_page) { totalCount: issueCount edges { node { ...on PullRequest { number title } } } }
      reviewRequested: search(query: $reviewerQuery, type: ISSUE, first: $per_page) { totalCount: issueCount edges { node { ...on PullRequest { number } } } }
      viewer { login }
    }"#;
    let r = w
        .gql(
            "owner",
            status,
            json!({"owner": ORG, "repo": "app", "headRefName": "feature", "viewerQuery": format!("repo:{ORG}/app state:open is:pr author:@me"), "reviewerQuery": format!("repo:{ORG}/app state:open review-requested:@me")}),
        )
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    assert_eq!(r["data"]["viewer"]["login"], "owner");
    assert_eq!(r["data"]["repository"]["defaultBranchRef"]["name"], "main");
    assert_eq!(r["data"]["repository"]["pullRequests"]["totalCount"], 1);
    assert_eq!(r["data"]["viewerCreated"]["totalCount"], 1);
    assert_eq!(r["data"]["viewerCreated"]["edges"][0]["node"]["number"], 1);
    // `gh pr merge`.
    let id = pr["id"].as_str().unwrap().to_string();
    let m = r#"mutation PullRequestMerge($input: MergePullRequestInput!) { mergePullRequest(input: $input) { clientMutationId pullRequest { state merged } } }"#;
    let r = w.gql("owner", m, json!({"input": {"pullRequestId": id, "mergeMethod": "MERGE", "clientMutationId": "x"}})).await;
    assert!(r.get("errors").is_none(), "{r}");
    assert_eq!(
        r["data"]["mergePullRequest"]["pullRequest"]["state"],
        "MERGED"
    );
    assert_eq!(r["data"]["mergePullRequest"]["clientMutationId"], "x");
}

#[tokio::test]
async fn graphql_issues_repo_view_create_and_errors() {
    let w = world().await;
    w.repo("app").await;
    let repo = r#"query RepositoryInfo($owner: String!, $name: String!) {
      repository(owner: $owner, name: $name) {
        id name nameWithOwner owner { login } description url homepageUrl isPrivate visibility isArchived isFork
        hasIssuesEnabled hasWikiEnabled viewerPermission defaultBranchRef { name target { oid } }
        mergeCommitAllowed rebaseMergeAllowed squashMergeAllowed stargazerCount forkCount
        repositoryTopics(first: 100) { nodes { topic { name } } }
        primaryLanguage { name } licenseInfo { name } latestRelease { tagName }
        watchers { totalCount } issues(states: OPEN) { totalCount } pullRequests(states: OPEN) { totalCount }
      }
    }"#;
    let r = w
        .gql("owner", repo, json!({"owner": ORG, "name": "app"}))
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    let d = &r["data"]["repository"];
    assert_eq!(d["nameWithOwner"], format!("{ORG}/app"));
    assert_eq!(d["viewerPermission"], "ADMIN");
    assert_eq!(d["visibility"], "PUBLIC");
    assert_eq!(d["defaultBranchRef"]["name"], "main");
    assert_eq!(
        d["defaultBranchRef"]["target"]["oid"]
            .as_str()
            .unwrap()
            .len(),
        40
    );
    // `gh issue create` then `gh issue list` / `view`.
    let create = r#"mutation IssueCreate($input: CreateIssueInput!) { createIssue(input: $input) { issue { id number url title } } }"#;
    let r = w
        .gql(
            "owner",
            create,
            json!({"input": {"repositoryId": d["id"], "title": "From GraphQL", "body": "b"}}),
        )
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    let num = r["data"]["createIssue"]["issue"]["number"]
        .as_u64()
        .unwrap();
    let issue_id = r["data"]["createIssue"]["issue"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let list = r#"query IssueList($owner: String!, $repo: String!, $limit: Int, $endCursor: String, $states: [IssueState!] = OPEN, $assignee: String, $author: String, $mention: String) {
      repository(owner: $owner, name: $repo) {
        hasIssuesEnabled
        issues(first: $limit, after: $endCursor, orderBy: {field: CREATED_AT, direction: DESC}, states: $states, filterBy: {assignee: $assignee, createdBy: $author, mentioned: $mention}) {
          totalCount nodes { number title state url createdAt updatedAt author { login } labels(first: 100) { nodes { name } } } pageInfo { hasNextPage endCursor }
        }
      }
    }"#;
    let r = w
        .gql(
            "owner",
            list,
            json!({"owner": ORG, "repo": "app", "limit": 30}),
        )
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    assert_eq!(r["data"]["repository"]["issues"]["totalCount"], 1);
    assert_eq!(
        r["data"]["repository"]["issues"]["nodes"][0]["title"],
        "From GraphQL"
    );
    let view = r#"query IssueByNumber($owner: String!, $repo: String!, $number: Int!) {
      repository(owner: $owner, name: $repo) {
        hasIssuesEnabled
        issue: issueOrPullRequest(number: $number) {
          __typename
          ...on Issue { id title state body author { login } comments(last: 1) { totalCount nodes { body } } assignees(first: 100) { nodes { login } } labels(first: 100) { nodes { name } } milestone { title } reactionGroups { content users { totalCount } } }
          ...on PullRequest { id }
        }
      }
    }"#;
    let add = r#"mutation CommentCreate($input: AddCommentInput!) { addComment(input: $input) { commentEdge { node { url body } } } }"#;
    let r = w
        .gql(
            "owner",
            add,
            json!({"input": {"subjectId": issue_id, "body": "first"}}),
        )
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    assert_eq!(
        r["data"]["addComment"]["commentEdge"]["node"]["body"],
        "first"
    );
    let r = w
        .gql(
            "owner",
            view,
            json!({"owner": ORG, "repo": "app", "number": num}),
        )
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    assert_eq!(r["data"]["repository"]["issue"]["__typename"], "Issue");
    assert_eq!(
        r["data"]["repository"]["issue"]["comments"]["totalCount"],
        1
    );
    let close = r#"mutation IssueClose($input: CloseIssueInput!) { closeIssue(input: $input) { issue { state stateReason } } }"#;
    let r = w
        .gql(
            "owner",
            close,
            json!({"input": {"issueId": issue_id, "stateReason": "NOT_PLANNED"}}),
        )
        .await;
    assert_eq!(r["data"]["closeIssue"]["issue"]["state"], "CLOSED", "{r}");
    // Errors: unknown repo, unknown field, parse error, introspection.
    let r = w
        .gql("owner", repo, json!({"owner": ORG, "name": "missing"}))
        .await;
    assert_eq!(r["data"]["repository"], Value::Null);
    assert_eq!(r["errors"][0]["type"], "NOT_FOUND");
    assert_eq!(r["errors"][0]["path"], json!(["repository"]));
    let r = w.gql("owner", "{ viewer { login nope } }", json!({})).await;
    assert_eq!(r["errors"][0]["type"], "UNDEFINED_FIELD");
    assert_eq!(r["data"]["viewer"]["login"], "owner");
    let r = w.gql("owner", "{ viewer { login ", json!({})).await;
    assert!(
        r["errors"][0]["message"]
            .as_str()
            .unwrap()
            .starts_with("Parse error")
    );
    let r = w
        .gql("owner", r#"query PullRequest_fields { PullRequest: __type(name: "PullRequest") { fields(includeDeprecated: true) { name } } }"#, json!({}))
        .await;
    let names: Vec<&str> = r["data"]["PullRequest"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"isInMergeQueue") && names.contains(&"statusCheckRollup"));
    let r = w
        .call(
            "POST",
            "/api/graphql",
            None,
            Some(json!({"query": "{ viewer { login } }"})),
        )
        .await;
    assert_eq!(r.status, 401);
    let t = w.token("owner");
    let r = w
        .call(
            "POST",
            "/graphql",
            Some(&t),
            Some(json!({"query": "{ viewer { login } }"})),
        )
        .await;
    assert_eq!(r.json["data"]["viewer"]["login"], "owner", "root alias");
    assert_eq!(r.header("x-ratelimit-resource"), Some("graphql"));
}

#[tokio::test]
async fn graphql_create_pull_request_and_review() {
    let w = world().await;
    w.repo("app").await;
    w.branch("app", "topic", "main").await;
    w.put_file("app", "topic", "t.txt", "t\n", "topic").await;
    let r = w
        .gql(
            "owner",
            r#"{ repository(owner: "acme", name: "app") { id } }"#,
            json!({}),
        )
        .await;
    let rid = r["data"]["repository"]["id"].as_str().unwrap().to_string();
    let create = r#"mutation PullRequestCreate($input: CreatePullRequestInput!) { createPullRequest(input: $input) { pullRequest { id url number headRefName } } }"#;
    let r = w
        .gql("owner", create, json!({"input": {"repositoryId": rid, "baseRefName": "main", "headRefName": "topic", "title": "Topic", "body": "b", "draft": false}}))
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    let pr = &r["data"]["createPullRequest"]["pullRequest"];
    assert_eq!(pr["headRefName"], "topic");
    assert!(pr["url"].as_str().unwrap().ends_with("/acme/app/pull/1"));
    let id = pr["id"].as_str().unwrap().to_string();
    let r = w.gql("writer", r#"{ viewer { id } }"#, json!({})).await;
    let writer_id = r["data"]["viewer"]["id"].as_str().unwrap().to_string();
    let req = r#"mutation PullRequestUpdateRequestReviews($input: RequestReviewsInput!) { requestReviews(input: $input) { clientMutationId } }"#;
    let r = w
        .gql(
            "owner",
            req,
            json!({"input": {"pullRequestId": id, "userIds": [writer_id], "union": true}}),
        )
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    let review = r#"mutation PullRequestReviewAdd($input: AddPullRequestReviewInput!) { addPullRequestReview(input: $input) { pullRequestReview { state author { login } } } }"#;
    let r = w
        .gql(
            "writer",
            review,
            json!({"input": {"pullRequestId": id, "event": "APPROVE", "body": "ok"}}),
        )
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    assert_eq!(
        r["data"]["addPullRequestReview"]["pullRequestReview"]["state"],
        "APPROVED"
    );
    let r = w
        .gql("owner", r#"query($id: ID!) { node(id: $id) { __typename ...on PullRequest { reviewDecision latestReviews(first: 10) { nodes { state } } } } }"#, json!({"id": id}))
        .await;
    assert!(r.get("errors").is_none(), "{r}");
    assert_eq!(r["data"]["node"]["reviewDecision"], "APPROVED");
}
