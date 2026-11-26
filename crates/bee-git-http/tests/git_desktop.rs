//! A git desktop client pointed at Beehive (PRODUCT §4.24: "git compatibility makes existing
//! git desktop clients work"). The test drives the real `git` the way GitHub Desktop does: it
//! signs in through the GitHub-compatible API (`/user`, `/user/repos`, `/repos/{o}/{r}`),
//! clones the API's `clone_url` with credentials from an askpass program and no credential
//! helper, then branches, commits, publishes the branch with upstream tracking, fetches with
//! prune, pulls, and lists the branch and pull requests through the API again.

#![allow(missing_docs, clippy::result_large_err)]

use crate::common;
use std::path::Path;
use std::process::Command;

use common::*;
use serde_json::Value;

/// `GET <path>` on the GitHub-compatible API with a token, as Desktop's API client sends it.
async fn api(env: &Env, path: &str) -> Value {
    let url = format!("{}{path}", env.srv.url);
    let auth = format!("Authorization: token {}", env.token);
    let out = tokio::task::spawn_blocking(move || {
        Command::new("curl")
            .args([
                "-sf",
                "-H",
                &auth,
                "-H",
                "Accept: application/vnd.github.v3+json",
                "-H",
                "User-Agent: GitHubDesktop/3.4.0 (macOS)",
                &url,
            ])
            .output()
            .expect("curl runs")
    })
    .await
    .unwrap();
    assert!(out.status.success(), "GET {path} failed");
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Runs git like Desktop: no credential helper, credentials from `GIT_ASKPASS`, progress on.
async fn desktop_git(env: &Env, askpass: &Path, dir: &Path, args: &[&str]) -> String {
    let mut all = vec!["-c", "credential.helper=", "-c", "protocol.version=2"];
    all.extend_from_slice(args);
    let mut c = env.git_cmd(dir, &all);
    c.env("GIT_ASKPASS", askpass)
        .env("DESKTOP_TOKEN", &env.token)
        .env("GIT_TERMINAL_PROMPT", "0");
    let out = tokio::task::spawn_blocking(move || c.output().expect("git runs"))
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "git {all:?}:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_desktop_client_signs_in_clones_branches_publishes_fetches_and_pulls() {
    let env = start_env().await;
    env.api_commit_and_land(
        "playground",
        &[("a.txt", b"alpha\n"), ("dir/b.txt", b"bravo\n")],
        "seed",
        8,
    )
    .await;
    // Sign in: who am I, and which repositories can I clone.
    let me = api(&env, "/user").await;
    assert!(me["login"].as_str().is_some_and(|l| !l.is_empty()), "{me}");
    let repos = api(&env, "/user/repos?per_page=100").await;
    assert!(
        repos
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["full_name"] == "dev/playground"),
        "{repos}"
    );
    let repo = api(&env, "/repos/dev/playground").await;
    let clone_url = repo["clone_url"].as_str().expect("clone_url").to_string();
    assert!(clone_url.ends_with("/dev/playground.git"), "{clone_url}");
    assert!(!clone_url.contains(&env.token), "no credential in the URL");
    // The askpass program Desktop installs: user name and token on request.
    let askpass = env.path("askpass.sh");
    std::fs::write(
        &askpass,
        "#!/bin/sh\ncase \"$1\" in\n  Username*) echo x-access-token ;;\n  *) echo \"$DESKTOP_TOKEN\" ;;\nesac\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let dest = env.path("desktop");
    desktop_git(
        &env,
        &askpass,
        env.dir.path(),
        &[
            "clone",
            "--recursive",
            "--progress",
            "--",
            &clone_url,
            dest.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"alpha\n");
    // Status the way Desktop reads it.
    let status = desktop_git(
        &env,
        &askpass,
        &dest,
        &[
            "status",
            "--untracked-files=all",
            "--branch",
            "--porcelain=2",
            "-z",
        ],
    )
    .await;
    assert!(status.contains("# branch.head main"), "{status}");
    // A new branch with a commit, published with upstream tracking.
    desktop_git(&env, &askpass, &dest, &["checkout", "-b", "desktop-change"]).await;
    write(
        &dest.join("from-desktop.txt"),
        b"made in a desktop client\n",
    );
    desktop_git(&env, &askpass, &dest, &["add", "-A"]).await;
    desktop_git(
        &env,
        &askpass,
        &dest,
        &["commit", "-m", "Change from desktop"],
    )
    .await;
    desktop_git(
        &env,
        &askpass,
        &dest,
        &[
            "push",
            "origin",
            "desktop-change:desktop-change",
            "--progress",
            "--set-upstream",
        ],
    )
    .await;
    let upstream = desktop_git(
        &env,
        &askpass,
        &dest,
        &["rev-parse", "--abbrev-ref", "desktop-change@{upstream}"],
    )
    .await;
    assert_eq!(upstream, "origin/desktop-change");
    // Someone else lands on main; Desktop fetches (with prune) and pulls.
    env.api_commit_and_land(
        "playground",
        &[("remote.txt", b"from the api\n")],
        "remote work",
        9,
    )
    .await;
    desktop_git(&env, &askpass, &dest, &["checkout", "main"]).await;
    desktop_git(
        &env,
        &askpass,
        &dest,
        &[
            "fetch",
            "--progress",
            "--prune",
            "--recurse-submodules=on-demand",
            "origin",
        ],
    )
    .await;
    desktop_git(
        &env,
        &askpass,
        &dest,
        &[
            "pull",
            "--ff",
            "--recurse-submodules",
            "--progress",
            "origin",
            "main",
        ],
    )
    .await;
    assert_eq!(
        std::fs::read(dest.join("remote.txt")).unwrap(),
        b"from the api\n"
    );
    // The published branch and the pull request list, through the API.
    let branches = api(&env, "/repos/dev/playground/branches").await;
    assert!(
        branches
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["name"] == "desktop-change"),
        "{branches}"
    );
    let pulls = api(&env, "/repos/dev/playground/pulls?state=open").await;
    assert!(pulls.is_array(), "{pulls}");
}
