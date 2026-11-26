//! A repo's gateway settings (`docs/spec/operations.md` §9) enforced on the smart HTTP and LFS
//! routes with the real `git` and `curl`: the per-repo git switch, the LFS switch, the oldest
//! accepted git version and the change-id trailer requirement.

#![allow(missing_docs, clippy::result_large_err)]

use crate::common;
use std::process::Command;

use common::*;
use serde_json::{Value, json};

async fn set(env: &Env, repo: &str, fields: Value) -> Result<Value, bee_proto::v1::ErrorDetail> {
    let mut req = json!({ "repo": { "fullName": format!("dev/{repo}") } });
    for (k, v) in fields.as_object().unwrap() {
        req[k] = v.clone();
    }
    env.rpc("GitService", "UpdateGitConfig", req).await
}

/// `POST /<repo>.git/info/lfs/objects/batch` with `curl`: status and body.
async fn lfs_batch(env: &Env, repo: &str) -> (u16, String) {
    let url = format!(
        "http://{}/dev/{repo}.git/info/lfs/objects/batch",
        env.addr()
    );
    let auth = format!("Authorization: Bearer {}", env.token);
    let body = json!({
        "operation": "download",
        "transfers": ["basic"],
        "objects": [{ "oid": "0".repeat(64), "size": 1 }],
    })
    .to_string();
    let out = tokio::task::spawn_blocking(move || {
        Command::new("curl")
            .args([
                "-s",
                "-o",
                "-",
                "-w",
                "\n%{http_code}",
                "-H",
                "Accept: application/vnd.git-lfs+json",
                "-H",
                "Content-Type: application/vnd.git-lfs+json",
                "-H",
                &auth,
                "--data-binary",
                &body,
                &url,
            ])
            .output()
            .expect("curl runs")
    })
    .await
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (code.trim().parse().expect("status"), body.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_per_repo_git_switch_refuses_clone_fetch_push_and_lfs() {
    // The dev repo has a forge record, which holds the `git_enabled` setting.
    let env = start_env().await;
    env.api_commit_and_land("playground", &[("a.txt", b"alpha\n")], "seed", 1)
        .await;
    let clone = env.path("clone");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("playground"), "clone"],
    )
    .await;
    write(&clone.join("new.txt"), b"new\n");
    env.git(&clone, &["add", "-A"]).await;
    env.git(&clone, &["commit", "-q", "-m", "a side branch"])
        .await;
    env.git(&clone, &["push", "-q", "origin", "HEAD:refs/heads/side"])
        .await;
    let (status, _) = lfs_batch(&env, "playground").await;
    assert_eq!(status, 200, "LFS is on by default");
    let cfg = set(&env, "playground", json!({ "gitEnabled": false }))
        .await
        .expect("turn git off");
    assert_ne!(cfg["gitEnabled"], true, "false is omitted in proto3 JSON");
    // Every git transport answers GIT_DISABLED, for reads and writes alike.
    let err = env
        .git_fails(env.dir.path(), &["ls-remote", &env.url("playground")])
        .await;
    assert!(err.contains("GIT_DISABLED"), "{err}");
    let err = env
        .git_fails(
            env.dir.path(),
            &["clone", "-q", &env.url("playground"), "again"],
        )
        .await;
    assert!(err.contains("GIT_DISABLED"), "{err}");
    let err = env.git_fails(&clone, &["fetch", "-q", "origin"]).await;
    assert!(err.contains("GIT_DISABLED"), "{err}");
    write(&clone.join("more.txt"), b"more\n");
    env.git(&clone, &["add", "-A"]).await;
    env.git(&clone, &["commit", "-q", "-m", "while disabled"])
        .await;
    let err = env
        .git_fails(&clone, &["push", "-q", "origin", "HEAD:refs/heads/side2"])
        .await;
    assert!(err.contains("GIT_DISABLED"), "{err}");
    assert!(
        env.tip("playground", "side2").await.is_none(),
        "nothing was written"
    );
    let (status, body) = lfs_batch(&env, "playground").await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("GIT_DISABLED"), "{body}");
    // The RPC form of the gateway answers the same.
    let e = env
        .rpc(
            "GitService",
            "GitListRefs",
            json!({ "repo": { "fullName": "dev/playground" } }),
        )
        .await
        .expect_err("disabled");
    assert_eq!(e.reason, "GIT_DISABLED");
    // The API is unaffected: git is one surface among several.
    assert!(env.tip("playground", "side").await.is_some());
    // Back on: the same push goes through.
    set(&env, "playground", json!({ "gitEnabled": true }))
        .await
        .expect("turn git on");
    env.git(&clone, &["push", "-q", "origin", "HEAD:refs/heads/side2"])
        .await;
    assert!(env.tip("playground", "side2").await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lfs_switch_refuses_the_lfs_api_but_not_git() {
    let env = start_env().await;
    imported(&env, "r").await;
    set(&env, "r", json!({ "lfsEnabled": false }))
        .await
        .expect("turn LFS off");
    let (status, body) = lfs_batch(&env, "r").await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains("INVALID_STATE") && body.contains("lfs_disabled"),
        "{body}"
    );
    env.git(env.dir.path(), &["ls-remote", &env.url("r")]).await;
    set(&env, "r", json!({ "lfsEnabled": true }))
        .await
        .expect("turn LFS on");
    assert_eq!(lfs_batch(&env, "r").await.0, 200);
}

/// `git ls-remote` of `dev/r` sending `agent` as the HTTP user agent.
async fn ls_remote_as(env: &Env, agent: &str) -> std::process::Output {
    let agent = format!("http.userAgent={agent}");
    env.run(env.dir.path(), &["-c", &agent, "ls-remote", &env.url("r")])
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_older_than_the_minimum_version_is_refused() {
    let env = start_env().await;
    imported(&env, "r").await;
    // The default minimum is git 2.30.0.
    let old = ls_remote_as(&env, "git/2.20.1").await;
    assert!(!old.status.success());
    let err = String::from_utf8_lossy(&old.stderr);
    assert!(
        err.contains("git_too_old") && err.contains("2.30.0"),
        "{err}"
    );
    assert!(ls_remote_as(&env, "git/2.30.0").await.status.success());
    assert!(
        ls_remote_as(&env, "git/2.45.2 (Apple Git-154)")
            .await
            .status
            .success(),
        "vendor suffixes parse"
    );
    // The repo's own minimum applies to the real client too.
    let v = env.git(env.dir.path(), &["--version"]).await;
    set(&env, "r", json!({ "minClientVersion": "99.0.0" }))
        .await
        .expect("raise the minimum");
    let err = env
        .git_fails(env.dir.path(), &["ls-remote", &env.url("r")])
        .await;
    assert!(
        err.contains("git_too_old") && err.contains("99.0.0"),
        "{v}: {err}"
    );
    set(&env, "r", json!({ "minClientVersion": "2.0.0" }))
        .await
        .expect("lower the minimum");
    assert!(ls_remote_as(&env, "git/2.20.1").await.status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_change_id_trailer_requirement_refuses_pushes_without_one() {
    let env = start_env().await;
    imported(&env, "r").await;
    let clone = env.path("clone");
    env.git(env.dir.path(), &["clone", "-q", &env.url("r"), "clone"])
        .await;
    set(&env, "r", json!({ "requireChangeIdTrailer": true }))
        .await
        .expect("require trailers");
    write(&clone.join("t.txt"), b"one\n");
    env.git(&clone, &["add", "-A"]).await;
    env.git(&clone, &["commit", "-q", "-m", "no trailer here"])
        .await;
    let err = env
        .git_fails(&clone, &["push", "-q", "origin", "HEAD:refs/heads/topic"])
        .await;
    assert!(
        err.contains("change_id_trailer_required") && err.contains("INVALID_ARGUMENT"),
        "{err}"
    );
    assert!(env.tip("r", "topic").await.is_none());
    env.git(
        &clone,
        &[
            "commit",
            "-q",
            "--amend",
            "-m",
            "with a trailer\n\nChange-Id: I0123456789abcdef0123456789abcdef01234567",
        ],
    )
    .await;
    env.git(&clone, &["push", "-q", "origin", "HEAD:refs/heads/topic"])
        .await;
    assert!(env.tip("r", "topic").await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seed_packs_are_refused_rather_than_stored_unenforced() {
    let env = start_env().await;
    imported(&env, "r").await;
    let e = set(&env, "r", json!({ "seedPacksEnabled": true }))
        .await
        .expect_err("not served");
    assert_eq!(
        (e.reason.as_str(), e.subcode.as_str()),
        ("INVALID_ARGUMENT", "unsupported_setting")
    );
    let cfg = env
        .rpc(
            "GitService",
            "GetGitConfig",
            json!({ "repo": { "fullName": "dev/r" } }),
        )
        .await
        .expect("config");
    assert_ne!(cfg["seedPacksEnabled"], true);
}
