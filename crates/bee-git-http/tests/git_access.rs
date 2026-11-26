//! The real `git` CLI against repo access (SPEC §9.6): private repos clone only for those
//! who may read them, archived repos refuse pushes with `ARCHIVED`, deleted repos are not
//! found until restored, and a transferred repo clones under its new name.

#![allow(missing_docs, clippy::result_large_err)]

use crate::common;
use common::*;
use serde_json::json;

/// A stored human principal and a token for it.
async fn human(env: &Env, display: &str) -> (String, String) {
    let state = &env.srv.state;
    let id = bee_server::util::new_ulid(state.now_ms())
        .unwrap()
        .to_text();
    let rec = bee_server::auth::PrincipalRecord {
        id: id.clone(),
        kind: "human".into(),
        display: display.into(),
        org: String::new(),
        owner: String::new(),
        runtime: String::new(),
        purpose: String::new(),
        external_ids: Vec::new(),
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
    let now = state.now_ms() / 1000;
    let token = state
        .ring
        .read()
        .unwrap()
        .mint(
            &bee_auth::MintRequest {
                principal: rec.to_auth(),
                on_behalf_of: Vec::new(),
                scope: bee_auth::Scope::new().expires_at(now + 1800),
            },
            now,
        )
        .unwrap()
        .text;
    (id, token)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_clone_and_push_follow_repo_visibility_and_lifecycle() {
    let env = start_env().await;
    env.rpc(
        "OrgService",
        "UpdateOrgSettings",
        json!({ "org": "dev", "baseRepoRole": "REPO_ROLE_READ" }),
    )
    .await
    .expect("base role read");
    let created = env
        .rpc(
            "RepoService",
            "CreateRepo",
            json!({ "org": "dev", "name": "secret", "visibility": "VISIBILITY_PRIVATE", "autoInit": true }),
        )
        .await
        .expect("repo");
    let repo_id = created["id"].as_str().unwrap().to_string();
    let (member_id, member) = human(&env, "member").await;
    env.rpc(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": member_id, "role": "ROLE_MEMBER" }),
    )
    .await
    .expect("member");
    let (_, stranger) = human(&env, "stranger").await;

    // A stranger cannot even learn the repo exists; a member (base role read) clones.
    let err = env
        .git_fails(
            env.dir.path(),
            &[
                "clone",
                "-q",
                &env.url_as(&stranger, "secret"),
                env.path("s").to_str().unwrap(),
            ],
        )
        .await;
    assert!(err.contains("404") || err.contains("not found"), "{err}");
    let mine = env.path("m");
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            &env.url_as(&member, "secret"),
            mine.to_str().unwrap(),
        ],
    )
    .await;
    // Read is not write: the member's push is refused.
    write(&mine.join("m.txt"), b"member\n");
    env.git(&mine, &["add", "."]).await;
    env.git(&mine, &["commit", "-q", "-m", "member"]).await;
    let err = env
        .git_fails(&mine, &["push", "-q", "origin", "HEAD:refs/heads/member"])
        .await;
    assert!(
        err.contains("403") || err.contains("PERMISSION_DENIED"),
        "{err}"
    );

    // The owner pushes; once archived, the push is refused with ARCHIVED and clones still work.
    let own = env.path("o");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("secret"), own.to_str().unwrap()],
    )
    .await;
    write(&own.join("a.txt"), b"a\n");
    env.git(&own, &["add", "."]).await;
    env.git(&own, &["commit", "-q", "-m", "a"]).await;
    env.git(&own, &["push", "-q", "origin", "HEAD:refs/heads/topic"])
        .await;
    env.rpc(
        "RepoService",
        "ArchiveRepo",
        json!({ "repo": { "fullName": "dev/secret" } }),
    )
    .await
    .expect("archive");
    write(&own.join("b.txt"), b"b\n");
    env.git(&own, &["add", "."]).await;
    env.git(&own, &["commit", "-q", "-m", "b"]).await;
    let err = env
        .git_fails(&own, &["push", "-q", "origin", "HEAD:refs/heads/topic"])
        .await;
    assert!(err.contains("ARCHIVED"), "{err}");
    env.git(&own, &["fetch", "-q", "origin"]).await;
    env.rpc(
        "RepoService",
        "UnarchiveRepo",
        json!({ "repo": { "fullName": "dev/secret" } }),
    )
    .await
    .expect("unarchive");
    env.git(&own, &["push", "-q", "origin", "HEAD:refs/heads/topic"])
        .await;

    // Deleted: not found for the owner too; restored: back.
    env.rpc(
        "RepoService",
        "DeleteRepo",
        json!({ "repo": { "fullName": "dev/secret" }, "confirmFullName": "dev/secret" }),
    )
    .await
    .expect("delete");
    let err = env.git_fails(&own, &["fetch", "-q", "origin"]).await;
    assert!(err.contains("404") || err.contains("not found"), "{err}");
    env.rpc("RepoService", "RestoreRepo", json!({ "repoId": repo_id }))
        .await
        .expect("restore");
    env.git(&own, &["fetch", "-q", "origin"]).await;
}
