//! Differential tests with the real `git` (and `git-lfs`) CLI against an in-process server in
//! dev mode (SQLite and disk blobs in a temporary directory): clone, fetch, shallow and partial
//! clones, and pushes that land under policy (PRODUCT §4.11, `AS-GIT-01`).

#![allow(missing_docs, clippy::result_large_err)]

use crate::common;
use std::path::Path;
use std::process::{Command, Stdio};

use base64::Engine as _;
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_git_01_clone_reproduces_imported_commit_ids_over_v2_and_v0() {
    let env = start_env().await;
    let src = imported(&env, "r").await;
    let want_refs = env
        .git(
            &src,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads",
                "refs/tags",
            ],
        )
        .await;
    let want_all = env.git(&src, &["rev-list", "--all"]).await;
    for (proto, name) in [("2", "c2"), ("0", "c0"), ("1", "c1")] {
        let dest = env.path(name);
        env.git(
            env.dir.path(),
            &[
                "-c",
                &format!("protocol.version={proto}"),
                "clone",
                "-q",
                &env.url("r"),
                dest.to_str().unwrap(),
            ],
        )
        .await;
        let remote = env
            .git(
                &dest,
                &[
                    "for-each-ref",
                    "--format=%(objectname) %(refname)",
                    "refs/remotes/origin/main",
                    "refs/remotes/origin/feature",
                    "refs/tags",
                ],
            )
            .await
            .replace("refs/remotes/origin/", "refs/heads/");
        let mut a: Vec<&str> = remote.lines().collect();
        let mut b: Vec<&str> = want_refs.lines().collect();
        a.sort();
        b.sort();
        assert_eq!(a, b, "protocol v{proto}: refs and ids");
        let mut got: Vec<String> = env
            .git(&dest, &["rev-list", "--all", "--remotes"])
            .await
            .lines()
            .map(str::to_string)
            .collect();
        let mut want: Vec<String> = want_all.lines().map(str::to_string).collect();
        got.sort();
        want.sort();
        assert_eq!(got, want, "protocol v{proto}: every commit id");
        env.git(&dest, &["fsck", "--strict"]).await;
        assert_eq!(std::fs::read(dest.join("dir/b.txt")).unwrap(), b"bravo 2\n");
        let link = std::fs::read_link(dest.join("link")).unwrap();
        assert_eq!(link, Path::new("a.txt"));
    }
    // ls-remote over v2 shows HEAD and the same ids.
    let ls = env
        .git(env.dir.path(), &["ls-remote", "--symref", &env.url("r")])
        .await;
    assert!(ls.contains("ref: refs/heads/main\tHEAD"), "{ls}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_is_incremental_and_serves_new_history_with_change_id_trailers() {
    let env = start_env().await;
    let src = imported(&env, "r").await;
    let dest = env.path("clone");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), dest.to_str().unwrap()],
    )
    .await;
    let before = env.git(&dest, &["rev-parse", "origin/main"]).await;
    env.api_commit_and_land("r", &[("api.txt", b"from the api\n")], "api change", 1)
        .await;
    let out = env.run(&dest, &["fetch", "origin"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = env.git(&dest, &["rev-parse", "origin/main"]).await;
    assert_ne!(before, after);
    assert_eq!(env.git(&dest, &["rev-parse", "origin/main^"]).await, before);
    let body = env
        .git(&dest, &["log", "-1", "--format=%B", "origin/main"])
        .await;
    assert!(body.starts_with("api change"), "{body}");
    assert!(body.contains("Change-Id: k_"), "{body}");
    assert_eq!(
        env.git(&dest, &["log", "-1", "--format=%an <%ae>", "origin/main"])
            .await,
        "API Author <api@example.com>"
    );
    assert_eq!(
        env.git(&dest, &["show", "origin/main:api.txt"]).await,
        "from the api"
    );
    // The derived git id is deterministic: a fresh clone (and the original repo, after
    // fetching) sees the same id.
    let fresh = env.path("fresh");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), fresh.to_str().unwrap()],
    )
    .await;
    assert_eq!(env.git(&fresh, &["rev-parse", "HEAD"]).await, after);
    env.git(&src, &["fetch", "-q", &env.url("r"), "main"]).await;
    assert_eq!(env.git(&src, &["rev-parse", "FETCH_HEAD"]).await, after);
    env.git(&fresh, &["fsck", "--strict"]).await;
    // A fetch with nothing new transfers nothing and succeeds.
    let out = env.run(&dest, &["fetch", "origin"]).await;
    assert!(out.status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shallow_clone_deepen_and_unshallow() {
    let env = start_env().await;
    let src = imported(&env, "r").await;
    let total: usize = env
        .git(&src, &["rev-list", "--count", "main"])
        .await
        .parse()
        .unwrap();
    let dest = env.path("shallow");
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &env.url("r"),
            dest.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(env.git(&dest, &["rev-list", "--count", "HEAD"]).await, "1");
    assert!(dest.join(".git/shallow").exists());
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"alpha 2\n");
    env.git(&dest, &["fetch", "-q", "--deepen", "1"]).await;
    assert_eq!(env.git(&dest, &["rev-list", "--count", "HEAD"]).await, "2");
    env.git(&dest, &["fetch", "-q", "--unshallow"]).await;
    assert_eq!(
        env.git(&dest, &["rev-list", "--count", "HEAD"]).await,
        total.to_string()
    );
    assert!(!dest.join(".git/shallow").exists());
    env.git(&dest, &["fsck"]).await;
    // Protocol v0 depth and deepen-not (`--shallow-exclude`).
    let v0 = env.path("v0");
    env.git(
        env.dir.path(),
        &[
            "-c",
            "protocol.version=0",
            "clone",
            "-q",
            "--depth",
            "2",
            &env.url("r"),
            v0.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(env.git(&v0, &["rev-list", "--count", "HEAD"]).await, "2");
    let ex = env.path("exclude");
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            "--shallow-exclude=v1",
            &env.url("r"),
            ex.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(env.git(&ex, &["rev-list", "--count", "HEAD"]).await, "1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_clone_blob_none_fetches_blobs_lazily() {
    let env = start_env().await;
    let src = imported(&env, "r").await;
    let first = env
        .git(&src, &["rev-list", "--max-parents=0", "main"])
        .await;
    let dest = env.path("partial");
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-checkout",
            &env.url("r"),
            dest.to_str().unwrap(),
        ],
    )
    .await;
    let missing = env
        .git(
            &dest,
            &["rev-list", "--objects", "--all", "--missing=print"],
        )
        .await;
    assert!(
        missing.lines().any(|l| l.starts_with('?')),
        "blobs are missing after a blob:none clone"
    );
    env.git(&dest, &["checkout", "-q", "main"]).await;
    assert_eq!(std::fs::read(dest.join("dir/b.txt")).unwrap(), b"bravo 2\n");
    // An old blob is fetched on demand.
    let old = env
        .git(&dest, &["cat-file", "-p", &format!("{first}:a.txt")])
        .await;
    assert_eq!(old, "alpha");
    // tree:0 too.
    let t0 = env.path("tree0");
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            "--filter=tree:0",
            &env.url("r"),
            t0.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(std::fs::read(t0.join("m.txt")).unwrap(), b"main work\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_git_01_push_to_main_lands_under_policy_and_shows_in_log() {
    let env = start_env().await;
    imported(&env, "r").await;
    let dest = env.path("clone");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), dest.to_str().unwrap()],
    )
    .await;
    env.git(&dest, &["checkout", "-q", "-b", "topic"]).await;
    write(&dest.join("new.txt"), b"pushed over git\n");
    env.git(&dest, &["add", "new.txt"]).await;
    env.git(&dest, &["commit", "-q", "-m", "add new.txt over git"])
        .await;
    let pushed = env.git(&dest, &["rev-parse", "HEAD"]).await;
    let (_, v_before) = env.tip("r", "main").await.unwrap();
    env.git(&dest, &["push", "-q", "origin", "HEAD:main"]).await;
    let (tip, v_after) = env.tip("r", "main").await.unwrap();
    assert_eq!(v_after, v_before + 1, "one land");
    assert_eq!(
        env.log_messages("r", "main").await[0],
        "add new.txt over git\n"
    );
    // A fast-forward land keeps the pushed commit and its git id.
    let ls = env
        .git(
            env.dir.path(),
            &["ls-remote", &env.url("r"), "refs/heads/main"],
        )
        .await;
    assert!(ls.starts_with(&pushed), "{ls} vs {pushed}");
    let read = env
        .rpc("QueryService", "ReadPath", json!({ "repo": { "fullName": "dev/r" }, "at": { "expr": tip }, "path": B64.encode("new.txt") }))
        .await
        .unwrap();
    let data: Vec<u8> = read["items"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|i| B64.decode(i["data"].as_str().unwrap_or("")).unwrap())
        .collect();
    assert_eq!(data, b"pushed over git\n");
    // A new branch is a plain ref; deleting it works.
    env.git(&dest, &["push", "-q", "origin", "HEAD:refs/heads/topic"])
        .await;
    assert!(env.tip("r", "topic").await.is_some());
    env.git(&dest, &["push", "-q", "origin", ":refs/heads/topic"])
        .await;
    assert!(env.tip("r", "topic").await.is_none());
    // Workspace pushes go to the pusher's workspace and come back as refs/heads/ws/*.
    env.git(&dest, &["push", "-q", "origin", "HEAD:refs/heads/ws/try"])
        .await;
    let ws = format!("ws/{}/try", env.user);
    assert!(env.tip("r", &ws).await.is_some(), "workspace ref {ws}");
    let ls = env
        .git(
            env.dir.path(),
            &["ls-remote", &env.url("r"), "refs/heads/ws/*"],
        )
        .await;
    assert!(ls.contains("refs/heads/ws/try"), "{ls}");
    // An annotated tag keeps its tag object: advertised with its peeled commit and cloned
    // as a tag.
    env.git(&dest, &["tag", "-a", "v2", "-m", "release two"])
        .await;
    let tag = env.git(&dest, &["rev-parse", "v2"]).await;
    env.git(&dest, &["push", "-q", "origin", "v2"]).await;
    assert!(env.tip("r", "tags/v2").await.is_some());
    let ls = env
        .git(
            env.dir.path(),
            &["ls-remote", &env.url("r"), "refs/tags/v2*"],
        )
        .await;
    assert!(ls.contains(&format!("{tag}\trefs/tags/v2")), "{ls}");
    assert!(ls.contains(&format!("{pushed}\trefs/tags/v2^{{}}")), "{ls}");
    let fresh = env.path("fresh");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), fresh.to_str().unwrap()],
    )
    .await;
    assert_eq!(env.git(&fresh, &["cat-file", "-t", "v2"]).await, "tag");
    assert_eq!(env.git(&fresh, &["rev-parse", "v2"]).await, tag);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sha256_repos_round_trip_exact_ids() {
    let env = start_env().await;
    let src = env.path("src256");
    std::fs::create_dir_all(&src).unwrap();
    env.git(&src, &["init", "-q", "--object-format=sha256"])
        .await;
    write(&src.join("a.txt"), b"alpha\n");
    env.git(&src, &["add", "-A"]).await;
    env.git(&src, &["commit", "-q", "-m", "first"]).await;
    write(&src.join("b/c.txt"), b"charlie\n");
    env.git(&src, &["add", "-A"]).await;
    env.git(&src, &["commit", "-q", "-m", "second"]).await;
    env.import(&src, "r256").await;
    let want = env.git(&src, &["rev-parse", "main"]).await;
    assert_eq!(want.len(), 64);
    let dest = env.path("c256");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r256"), dest.to_str().unwrap()],
    )
    .await;
    assert_eq!(env.git(&dest, &["rev-parse", "HEAD"]).await, want);
    assert_eq!(
        env.git(&dest, &["rev-parse", "--show-object-format"]).await,
        "sha256"
    );
    write(&dest.join("d.txt"), b"delta\n");
    env.git(&dest, &["add", "-A"]).await;
    env.git(&dest, &["commit", "-q", "-m", "pushed in sha256"])
        .await;
    let pushed = env.git(&dest, &["rev-parse", "HEAD"]).await;
    env.git(&dest, &["push", "-q", "origin", "HEAD:main"]).await;
    let ls = env
        .git(
            env.dir.path(),
            &["ls-remote", &env.url("r256"), "refs/heads/main"],
        )
        .await;
    assert!(ls.starts_with(&pushed), "{ls}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_of_a_secret_is_rejected_with_the_reason_and_a_bypass_is_audited() {
    let env = start_env().await;
    imported(&env, "r").await;
    let (rule, example) = bee_secrets::builtin_rules()
        .into_iter()
        .find(|r| r.id().starts_with("github") && r.confidence().blocks())
        .and_then(|r| {
            r.example()
                .map(|example| (r.id().to_string(), example.to_string()))
        })
        .expect("a github rule example");
    let dest = env.path("clone");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), dest.to_str().unwrap()],
    )
    .await;
    write(&dest.join("config.yml"), example.as_bytes());
    env.git(&dest, &["add", "config.yml"]).await;
    env.git(&dest, &["commit", "-q", "-m", "oops"]).await;
    let before = env.tip("r", "main").await.unwrap();
    let err = env.git_fails(&dest, &["push", "origin", "HEAD:main"]).await;
    assert!(err.contains("SECRET_DETECTED"), "{err}");
    assert!(
        !err.contains(&example),
        "the secret never appears in the reason"
    );
    let secret_refusal = env
        .rpc(
            "AuditService",
            "QueryAuditLog",
            json!({ "org": "dev", "filter": { "deniedOnly": true } }),
        )
        .await
        .expect("query audit log")["entries"]
        .as_array()
        .expect("audit entries")
        .iter()
        .find(|entry| entry["action"] == "vcs.write" && entry["result"] == "SECRET_DETECTED")
        .cloned()
        .expect("the refused git push is audited");
    assert_eq!(secret_refusal["actor"]["id"], env.user, "{secret_refusal}");
    assert!(
        secret_refusal["delegationChain"]
            .as_array()
            .is_some_and(|chain| chain.iter().any(|id| id == &env.user)),
        "{secret_refusal}"
    );
    assert!(
        secret_refusal["repo"]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{secret_refusal}"
    );
    assert!(
        secret_refusal["after"]
            .as_str()
            .is_some_and(|after| after.contains("config.yml")),
        "{secret_refusal}"
    );
    assert!(
        secret_refusal["after"]
            .as_str()
            .is_some_and(|after| after.contains(&rule)),
        "{secret_refusal}"
    );
    assert!(
        !serde_json::to_string(&secret_refusal)
            .expect("serialize audit entry")
            .contains(&example),
        "{secret_refusal}"
    );
    assert_eq!(
        env.tip("r", "main").await.unwrap(),
        before,
        "main did not move"
    );
    // The same push to a workspace is refused too (scanning happens at ingest).
    let err = env
        .git_fails(&dest, &["push", "origin", "HEAD:refs/heads/ws/x"])
        .await;
    assert!(err.contains("SECRET_DETECTED"), "{err}");
    // A bypass with a reason, as a push option.
    env.git(
        &dest,
        &[
            "push",
            "-q",
            "-o",
            "push-protection-bypass=used_in_tests",
            "origin",
            "HEAD:main",
        ],
    )
    .await;
    assert_ne!(env.tip("r", "main").await.unwrap(), before);
    let audit = env.srv.state.audit.read(1, 10_000).await.unwrap();
    let bypass = audit
        .iter()
        .find(|a| a.decision == "bypass")
        .expect("bypass audited");
    assert_eq!(bypass.detail["reason"], "used_in_tests");
    assert_eq!(bypass.action, "git-receive-pack");
    assert!(!bypass.detail.to_string().contains(&example));
    assert!(
        audit
            .iter()
            .any(|a| a.decision == "land" && a.action == "git-receive-pack:Land")
    );
    assert!(
        audit
            .iter()
            .any(|a| a.decision == "commit" && a.detail["via"] == "git")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_to_protected_ref_without_permission_is_rejected_by_policy() {
    let env = start_env().await;
    imported(&env, "r").await;
    let user = env.user.clone();
    let policy = format!(
        "version = 1\n[members]\nadmin = [\"{user}\"]\nwrite = [\"@humans\", \"@agents\"]\nread = [\"@everyone\"]\n\
         [[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n\
         [[land]]\nref = \"main\"\nagents_may_land_without_human = []\n"
    );
    env.api_commit_and_land(
        "r",
        &[(".beehive/policy.toml", policy.as_bytes())],
        "agents need a human to land on main",
        2,
    )
    .await;
    let agent = env.agent_token("pusher").await;
    let dest = env.path("agent");
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            &env.url_as(&agent, "r"),
            dest.to_str().unwrap(),
        ],
    )
    .await;
    write(&dest.join("agent.txt"), b"agent work\n");
    env.git(&dest, &["add", "agent.txt"]).await;
    env.git(&dest, &["commit", "-q", "-m", "agent change"])
        .await;
    let before = env.tip("r", "main").await.unwrap();
    let err = env.git_fails(&dest, &["push", "origin", "HEAD:main"]).await;
    assert!(err.contains("POLICY_DENIED"), "{err}");
    assert_eq!(env.tip("r", "main").await.unwrap(), before);
    // The agent can still park the work in its own workspace; a human fetches it from there
    // (it is the agent's workspace, so the human sees it under refs/workspaces/) and lands it.
    env.git(
        &dest,
        &["push", "-q", "origin", "HEAD:refs/heads/ws/agent-change"],
    )
    .await;
    let agent_head = env.git(&dest, &["rev-parse", "HEAD"]).await;
    let human = env.path("human");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), human.to_str().unwrap()],
    )
    .await;
    let ls = env
        .git(&human, &["ls-remote", "origin", "refs/workspaces/*"])
        .await;
    let line = ls
        .lines()
        .find(|l| l.ends_with("/agent-change"))
        .unwrap_or_else(|| panic!("{ls}"));
    assert!(line.starts_with(&agent_head), "{line}");
    let name = line.split('\t').nth(1).unwrap().to_string();
    env.git(&human, &["fetch", "-q", "origin", &name]).await;
    env.git(&human, &["push", "-q", "origin", "FETCH_HEAD:main"])
        .await;
    assert_eq!(env.log_messages("r", "main").await[0], "agent change\n");
    assert_ne!(env.tip("r", "main").await.unwrap(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_push_and_delete_are_denied_on_protected_refs_and_atomic_pushes_roll_back() {
    let env = start_env().await;
    imported(&env, "r").await;
    let dest = env.path("clone");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), dest.to_str().unwrap()],
    )
    .await;
    env.git(
        &dest,
        &["commit", "-q", "--amend", "-m", "rewritten history"],
    )
    .await;
    let before = env.tip("r", "main").await.unwrap();
    let err = env
        .git_fails(&dest, &["push", "--force", "origin", "HEAD:main"])
        .await;
    assert!(err.contains("NOT_FAST_FORWARD"), "{err}");
    let err = env.git_fails(&dest, &["push", "origin", ":main"]).await;
    assert!(err.contains("POLICY_DENIED"), "{err}");
    assert_eq!(env.tip("r", "main").await.unwrap(), before);
    // Atomic: the workspace update is not applied because main is refused.
    let err = env
        .git_fails(
            &dest,
            &[
                "push",
                "--atomic",
                "--force",
                "origin",
                "HEAD:refs/heads/ws/atomic",
                "HEAD:main",
            ],
        )
        .await;
    assert!(
        err.contains("atomic") || err.contains("NOT_FAST_FORWARD"),
        "{err}"
    );
    let ws = format!("ws/{}/atomic", env.user);
    assert!(
        env.tip("r", &ws).await.is_none(),
        "atomic push left {ws} behind"
    );
    // Force pushes to a non-protected ref are allowed for its owner.
    env.git(&dest, &["push", "-q", "origin", "HEAD:refs/heads/ws/mine"])
        .await;
    env.git(&dest, &["commit", "-q", "--amend", "-m", "rewritten again"])
        .await;
    env.git(
        &dest,
        &["push", "-q", "--force", "origin", "HEAD:refs/heads/ws/mine"],
    )
    .await;
    let head = env.git(&dest, &["rev-parse", "HEAD"]).await;
    let ls = env
        .git(
            env.dir.path(),
            &["ls-remote", &env.url("r"), "refs/heads/ws/mine"],
        )
        .await;
    assert!(ls.starts_with(&head));
}

fn pkt(line: &str) -> Vec<u8> {
    let mut v = format!("{:04x}", line.len() + 4).into_bytes();
    v.extend_from_slice(line.as_bytes());
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pushes_both_land_through_the_coalescer_rebased() {
    let env = start_env().await;
    imported(&env, "r").await;
    let a = env.path("a");
    let b = env.path("b");
    for d in [&a, &b] {
        env.git(
            env.dir.path(),
            &["clone", "-q", &env.url("r"), d.to_str().unwrap()],
        )
        .await;
    }
    let t = env.git(&a, &["rev-parse", "HEAD"]).await;
    write(&a.join("from-a.txt"), b"a\n");
    env.git(&a, &["add", "-A"]).await;
    env.git(&a, &["commit", "-q", "-m", "change from a"]).await;
    write(&b.join("from-b.txt"), b"b\n");
    env.git(&b, &["add", "-A"]).await;
    env.git(&b, &["commit", "-q", "-m", "change from b"]).await;
    let b1 = env.git(&b, &["rev-parse", "HEAD"]).await;
    env.git(&a, &["push", "-q", "origin", "HEAD:main"]).await;
    let a1 = env.git(&a, &["rev-parse", "HEAD"]).await;
    // B pushes with the old tip it saw, as a push racing A's would: the land rebases it.
    let mut c = env.git_cmd(&b, &["pack-objects", "--stdout", "--revs", "-q"]);
    c.stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = c.spawn().unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{b1}\n^{t}").unwrap();
    }
    let pack = child.wait_with_output().unwrap().stdout;
    let mut body = pkt(&format!(
        "{t} {b1} refs/heads/main\0report-status-v2 side-band-64k agent=test\n"
    ));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);
    let body_path = env.path("push.body");
    std::fs::write(&body_path, &body).unwrap();
    let out = Command::new("curl")
        .args([
            "-sS",
            "-u",
            &format!("git:{}", env.token),
            "-H",
            "Content-Type: application/x-git-receive-pack-request",
            "--data-binary",
            &format!("@{}", body_path.display()),
            &format!("http://{}/dev/r.git/git-receive-pack", env.addr()),
        ])
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("unpack ok"), "{text}");
    assert!(text.contains("ok refs/heads/main"), "{text}");
    let new = text
        .split("option new-oid ")
        .nth(1)
        .map(|s| s[..40].to_string())
        .expect("report names the rebased id");
    env.git(&b, &["fetch", "-q", "origin"]).await;
    assert_eq!(env.git(&b, &["rev-parse", "origin/main"]).await, new);
    assert_eq!(
        env.git(&b, &["rev-parse", "origin/main^"]).await,
        a1,
        "rebased onto A's land"
    );
    assert_eq!(env.git(&b, &["show", "origin/main:from-a.txt"]).await, "a");
    assert_eq!(env.git(&b, &["show", "origin/main:from-b.txt"]).await, "b");
    let msg = env
        .git(&b, &["log", "-1", "--format=%B", "origin/main"])
        .await;
    assert!(
        msg.starts_with("change from b") && msg.contains("Change-Id: k_"),
        "{msg}"
    );
    // Two real pushes at once: each lands (the coalescer rebases the later one) or is told to
    // fetch first; a retry after a rebase lands.
    let c1 = env.path("c1");
    let c2 = env.path("c2");
    for (i, d) in [&c1, &c2].iter().enumerate() {
        env.git(
            env.dir.path(),
            &["clone", "-q", &env.url("r"), d.to_str().unwrap()],
        )
        .await;
        write(&d.join(format!("race-{i}.txt")), b"race\n");
        env.git(d, &["add", "-A"]).await;
        env.git(d, &["commit", "-q", "-m", &format!("race {i}")])
            .await;
    }
    let mut p1 = env.git_cmd(&c1, &["push", "-q", "origin", "HEAD:main"]);
    let mut p2 = env.git_cmd(&c2, &["push", "-q", "origin", "HEAD:main"]);
    let h1 = std::thread::spawn(move || p1.output().unwrap());
    let h2 = std::thread::spawn(move || p2.output().unwrap());
    let (o1, o2) = tokio::task::spawn_blocking(move || (h1.join().unwrap(), h2.join().unwrap()))
        .await
        .unwrap();
    for (o, d) in [(o1, &c1), (o2, &c2)] {
        if !o.status.success() {
            let err = String::from_utf8_lossy(&o.stderr);
            assert!(
                err.contains("fetch first")
                    || err.contains("VERSION_MISMATCH")
                    || err.contains("rejected"),
                "{err}"
            );
            env.git(d, &["pull", "-q", "--rebase", "origin", "main"])
                .await;
            env.git(d, &["push", "-q", "origin", "HEAD:main"]).await;
        }
    }
    let msgs = env.log_messages("r", "main").await;
    assert!(
        msgs[..2].contains(&"race 0\n".to_string()) && msgs[..2].contains(&"race 1\n".to_string()),
        "{msgs:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credentials_basic_bearer_and_refusals() {
    let env = start_env().await;
    imported(&env, "r").await;
    let plain = format!("http://{}/dev/r.git", env.addr());
    let err = env.git_fails(env.dir.path(), &["ls-remote", &plain]).await;
    assert!(
        err.contains("Authentication failed") || err.contains("could not read Username"),
        "{err}"
    );
    let bad = env.url_as("not-a-token", "r");
    let err = env.git_fails(env.dir.path(), &["ls-remote", &bad]).await;
    assert!(
        err.contains("Authentication failed")
            || err.contains("401")
            || err.contains("could not read"),
        "{err}"
    );
    let header = format!("http.extraHeader=Authorization: Bearer {}", env.token);
    let ls = env
        .git(env.dir.path(), &["-c", &header, "ls-remote", &plain])
        .await;
    assert!(ls.contains("refs/heads/main"), "{ls}");
    let missing = env.url("nope");
    let err = env
        .git_fails(env.dir.path(), &["ls-remote", &missing])
        .await;
    assert!(err.contains("404") || err.contains("not found"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lfs_push_and_pull_through_signed_urls() {
    let env = start_env().await;
    let probe = Command::new("git").args(["lfs", "version"]).output();
    if !probe.is_ok_and(|o| o.status.success()) {
        eprintln!("skipping lfs_push_and_pull_through_signed_urls: git-lfs is not installed");
        return;
    }
    imported(&env, "r").await;
    let dest = env.path("clone");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), dest.to_str().unwrap()],
    )
    .await;
    env.git(&dest, &["lfs", "install", "--local"]).await;
    env.git(&dest, &["lfs", "track", "*.bin"]).await;
    let big: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    write(&dest.join("data/big.bin"), &big);
    env.git(&dest, &["add", ".gitattributes", "data/big.bin"])
        .await;
    env.git(&dest, &["commit", "-q", "-m", "add an LFS object"])
        .await;
    let out = env.run(&dest, &["push", "origin", "HEAD:main"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The pointer landed; the bytes are behind it.
    let fresh = env.path("fresh");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), fresh.to_str().unwrap()],
    )
    .await;
    env.git(&fresh, &["lfs", "install", "--local"]).await;
    env.git(&fresh, &["lfs", "pull"]).await;
    assert_eq!(std::fs::read(fresh.join("data/big.bin")).unwrap(), big);
    let pointer = env.git(&fresh, &["show", "HEAD:data/big.bin"]).await;
    assert!(
        pointer.starts_with("version https://git-lfs.github.com/spec/v1"),
        "{pointer}"
    );
}

/// A history that returns to an earlier tree (A, B, A) shares objects between trees that each
/// skip them against the other. The walk must send them anyway: found importing a real
/// repository whose clone failed with "did not receive expected object".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_history_that_returns_to_an_earlier_tree_clones_in_full_and_shallow() {
    let env = start_env().await;
    let src = env.path("revert-src");
    std::fs::create_dir_all(&src).unwrap();
    env.git(&src, &["init", "-q", "-b", "main"]).await;
    write(&src.join("dir/shared.txt"), b"shared by every state\n");
    write(&src.join("dir/x.txt"), b"one\n");
    env.git(&src, &["add", "-A"]).await;
    env.git(&src, &["commit", "-q", "-m", "A"]).await;
    write(&src.join("dir/x.txt"), b"two\n");
    env.git(&src, &["commit", "-q", "-am", "B"]).await;
    write(&src.join("dir/x.txt"), b"one\n");
    env.git(&src, &["commit", "-q", "-am", "A again"]).await;
    env.import(&src, "revert").await;
    // Reopen the durable store without the importer's in-memory git mappings or pack cache.
    let Env { srv, dir, .. } = env;
    srv.shutdown().await;
    let mut opts = bee_server::server::ServerOptions::new(bee_server::config::Config::dev(
        dir.path().join("data"),
        Some("127.0.0.1:0".into()),
    ));
    opts.dev = true;
    let srv = bee_server::server::start(opts).await.unwrap();
    let dev = srv.dev.clone().unwrap();
    let env = Env {
        srv,
        token: dev.token,
        user: dev.user,
        dir,
    };
    let read = env
        .rpc("QueryService", "ReadPath", json!({ "repo": { "fullName": "dev/revert" }, "at": { "expr": "main" }, "path": B64.encode("dir/shared.txt") }))
        .await
        .unwrap();
    let data: Vec<u8> = read["items"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|i| B64.decode(i["data"].as_str().unwrap_or("")).unwrap())
        .collect();
    assert_eq!(data, b"shared by every state\n");
    for (name, depth) in [("full", None), ("shallow", Some("2"))] {
        let dest = env.path(name);
        let mut args = vec!["clone", "-q"];
        if let Some(d) = depth {
            args.extend(["--depth", d]);
        }
        let url = format!("http://{}/dev/revert.git", env.addr());
        args.extend([url.as_str(), dest.to_str().unwrap()]);
        let mut cmd = env.git_cmd(env.dir.path(), &args);
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: Bearer {}", env.token),
            );
        let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "clone failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        env.git(&dest, &["fsck", "--connectivity-only"]).await;
        assert_eq!(
            std::fs::read(dest.join("dir/shared.txt")).unwrap(),
            b"shared by every state\n"
        );
    }
}
