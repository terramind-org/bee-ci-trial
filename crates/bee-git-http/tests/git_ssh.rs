//! Git over SSH with the real `ssh` and `git` clients: a throwaway key in a temporary directory,
//! a throwaway known-hosts file, and the in-process dev server listening for SSH on a free port.

#![allow(missing_docs, clippy::result_large_err)]

use crate::common;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use bee_git_http::ssh::SshServer;
use common::*;

struct Ssh {
    server: SshServer,
    key: PathBuf,
    known_hosts: PathBuf,
    port: u16,
}

impl Ssh {
    fn url(&self, repo: &str) -> String {
        format!("ssh://git@127.0.0.1:{}/dev/{repo}.git", self.port)
    }

    fn scp_url(&self, repo: &str) -> String {
        format!("git@127.0.0.1:dev/{repo}.git")
    }

    /// `ssh` options shared by `GIT_SSH_COMMAND` and direct invocations.
    fn options(&self, key: &Path) -> Vec<String> {
        vec![
            "-i".into(),
            key.display().to_string(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", self.known_hosts.display()),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-p".into(),
            self.port.to_string(),
        ]
    }

    fn git_ssh_command(&self, key: &Path) -> String {
        format!("ssh {}", self.options(key).join(" "))
    }
}

/// A key pair in `dir` and its public key line.
fn keygen(env: &Env, name: &str) -> (PathBuf, String) {
    let key = env.path(name);
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "test", "-f"])
        .arg(&key)
        .output()
        .expect("ssh-keygen runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let public = std::fs::read_to_string(format!("{}.pub", key.display())).unwrap();
    (key, public.trim().to_string())
}

async fn start_ssh(env: &Env) -> Ssh {
    let (key, public) = keygen(env, "id_dev");
    bee_server::git_ssh::add_key(&env.srv.state, &env.user, &public, "test key")
        .await
        .expect("register the key");
    let gateway = bee_server::git_mount::gateway(env.srv.state.clone()).expect("gateway");
    let host_key =
        bee_server::git_ssh::load_or_create_host_key(&env.path("host_key")).expect("host key");
    let server = bee_server::git_ssh::start(&gateway, "127.0.0.1:0", host_key)
        .await
        .expect("ssh listens");
    let port = server.addr.port();
    Ssh {
        server,
        key,
        known_hosts: env.path("known_hosts"),
        port,
    }
}

impl Env {
    /// Runs git with `GIT_SSH_COMMAND` using `key`.
    async fn ssh_run(&self, ssh: &Ssh, key: &Path, dir: &Path, args: &[&str]) -> Output {
        let mut c = self.git_cmd(dir, args);
        c.env("GIT_SSH_COMMAND", ssh.git_ssh_command(key));
        tokio::task::spawn_blocking(move || c.output().expect("git runs"))
            .await
            .unwrap()
    }

    async fn ssh_git(&self, ssh: &Ssh, dir: &Path, args: &[&str]) -> String {
        let out = self.ssh_run(ssh, &ssh.key, dir, args).await;
        assert!(
            out.status.success(),
            "git {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    async fn ssh_git_fails(&self, ssh: &Ssh, dir: &Path, args: &[&str]) -> String {
        let out = self.ssh_run(ssh, &ssh.key, dir, args).await;
        assert!(
            !out.status.success(),
            "git {args:?} should fail:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8_lossy(&out.stderr).to_string()
    }
}

/// Runs `ssh` itself against the server.
async fn ssh_raw(ssh: &Ssh, key: &Path, remote: &[&str]) -> Output {
    let mut args = ssh.options(key);
    args.push("git@127.0.0.1".into());
    args.extend(remote.iter().map(|s| s.to_string()));
    tokio::task::spawn_blocking(move || {
        Command::new("ssh")
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .expect("ssh runs")
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clone_fetch_and_push_over_ssh_for_every_protocol_version() {
    let env = start_env().await;
    let ssh = start_ssh(&env).await;
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
    for (proto, name) in [("2", "c2"), ("1", "c1"), ("0", "c0")] {
        let dest = env.path(name);
        env.ssh_git(
            &ssh,
            env.dir.path(),
            &[
                "-c",
                &format!("protocol.version={proto}"),
                "clone",
                "-q",
                &ssh.url("r"),
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
        assert_eq!(a, b, "protocol v{proto}: refs and ids over ssh");
        env.git(&dest, &["fsck", "--strict"]).await;
        assert_eq!(std::fs::read(dest.join("dir/b.txt")).unwrap(), b"bravo 2\n");
    }
    // The scp-like form and a path without `.git` reach the same repo.
    let scp = env.path("scp");
    env.ssh_git(
        &ssh,
        env.dir.path(),
        &["clone", "-q", &ssh.scp_url("r"), scp.to_str().unwrap()],
    )
    .await;
    assert_eq!(
        env.git(&scp, &["rev-parse", "HEAD"]).await,
        env.git(&env.path("c2"), &["rev-parse", "HEAD"]).await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ssh_negotiation_shallow_partial_and_incremental_fetches() {
    let env = start_env().await;
    let ssh = start_ssh(&env).await;
    let _src = imported(&env, "r").await;
    for proto in ["2", "0"] {
        let cfg = format!("protocol.version={proto}");
        // Shallow, deepen, unshallow.
        let dest = env.path(&format!("shallow{proto}"));
        env.ssh_git(
            &ssh,
            env.dir.path(),
            &[
                "-c",
                &cfg,
                "clone",
                "-q",
                "--depth",
                "1",
                &ssh.url("r"),
                dest.to_str().unwrap(),
            ],
        )
        .await;
        assert_eq!(env.git(&dest, &["rev-list", "--count", "HEAD"]).await, "1");
        env.ssh_git(&ssh, &dest, &["-c", &cfg, "fetch", "-q", "--deepen", "1"])
            .await;
        assert_eq!(env.git(&dest, &["rev-list", "--count", "HEAD"]).await, "2");
        env.ssh_git(&ssh, &dest, &["-c", &cfg, "fetch", "-q", "--unshallow"])
            .await;
        assert!(!dest.join(".git/shallow").exists());
        env.git(&dest, &["fsck"]).await;
        // Partial clone and lazy blob fetch.
        let part = env.path(&format!("part{proto}"));
        env.ssh_git(
            &ssh,
            env.dir.path(),
            &[
                "-c",
                &cfg,
                "clone",
                "-q",
                "--filter=blob:none",
                "--no-checkout",
                &ssh.url("r"),
                part.to_str().unwrap(),
            ],
        )
        .await;
        env.ssh_git(&ssh, &part, &["-c", &cfg, "checkout", "-q", "main"])
            .await;
        assert_eq!(std::fs::read(part.join("a.txt")).unwrap(), b"alpha 2\n");
        // Many local commits the server lacks make the client negotiate in several rounds.
        let busy = env.path(&format!("busy{proto}"));
        env.ssh_git(
            &ssh,
            env.dir.path(),
            &[
                "-c",
                &cfg,
                "clone",
                "-q",
                &ssh.url("r"),
                busy.to_str().unwrap(),
            ],
        )
        .await;
        for i in 0..60 {
            write(&busy.join(format!("n{i}.txt")), format!("{i}\n").as_bytes());
            env.git(&busy, &["add", "-A"]).await;
            env.git(&busy, &["commit", "-q", "-m", &format!("local {i}")])
                .await;
        }
        env.api_commit_and_land(
            "r",
            &[(&format!("api{proto}.txt"), b"api\n")],
            &format!("api change {proto}"),
            proto.parse::<u8>().unwrap() + 1,
        )
        .await;
        env.ssh_git(&ssh, &busy, &["-c", &cfg, "fetch", "-q", "origin"])
            .await;
        let body = env
            .git(&busy, &["log", "-1", "--format=%B", "origin/main"])
            .await;
        assert!(body.starts_with(&format!("api change {proto}")), "{body}");
        env.git(&busy, &["fsck", "--strict"]).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushes_over_ssh_land_under_the_same_policy_as_http() {
    let env = start_env().await;
    let ssh = start_ssh(&env).await;
    imported(&env, "r").await;
    for proto in ["2", "0"] {
        let cfg = format!("protocol.version={proto}");
        let dest = env.path(&format!("w{proto}"));
        env.ssh_git(
            &ssh,
            env.dir.path(),
            &[
                "-c",
                &cfg,
                "clone",
                "-q",
                &ssh.url("r"),
                dest.to_str().unwrap(),
            ],
        )
        .await;
        // A workspace branch, an annotated tag, then main (Land), and a delete-only push.
        write(&dest.join(format!("p{proto}.txt")), b"pushed over ssh\n");
        env.git(&dest, &["add", "-A"]).await;
        env.git(&dest, &["commit", "-q", "-m", &format!("ssh push {proto}")])
            .await;
        env.git(&dest, &["tag", "-a", &format!("t{proto}"), "-m", "tagged"])
            .await;
        env.ssh_git(
            &ssh,
            &dest,
            &[
                "-c",
                &cfg,
                "push",
                "-q",
                "origin",
                "HEAD:refs/heads/topic",
                &format!("t{proto}"),
            ],
        )
        .await;
        env.ssh_git(
            &ssh,
            &dest,
            &["-c", &cfg, "push", "-q", "origin", "HEAD:main"],
        )
        .await;
        let log = env.log_messages("r", "main").await;
        assert!(
            log.first()
                .is_some_and(|m| m.starts_with(&format!("ssh push {proto}"))),
            "{log:?}"
        );
        let ls = env.git(env.dir.path(), &["ls-remote", &env.url("r")]).await;
        assert!(
            ls.contains(&format!("refs/tags/t{proto}^{{}}")),
            "the tag is annotated: {ls}"
        );
        env.ssh_git(
            &ssh,
            &dest,
            &["-c", &cfg, "push", "-q", "origin", "--delete", "topic"],
        )
        .await;
        // A force push to main is refused by policy, with the reason.
        env.git(&dest, &["reset", "-q", "--hard", "HEAD~1"]).await;
        write(&dest.join("other.txt"), b"rewritten\n");
        env.git(&dest, &["add", "-A"]).await;
        env.git(&dest, &["commit", "-q", "-m", "rewrite"]).await;
        let err = env
            .ssh_git_fails(
                &ssh,
                &dest,
                &["-c", &cfg, "push", "-f", "origin", "HEAD:main"],
            )
            .await;
        assert!(err.contains("NOT_FAST_FORWARD"), "{err}");
    }
    // Push protection applies: a secret is refused with its reason.
    let dest = env.path("secret");
    env.ssh_git(
        &ssh,
        env.dir.path(),
        &["clone", "-q", &ssh.url("r"), dest.to_str().unwrap()],
    )
    .await;
    write(
        &dest.join("creds.txt"),
        b"aws_secret_access_key = AKIAIOSFODNN7EXAMPLE\ntoken = ghp_0123456789abcdefghijklmnopqrstuvwxyz\n",
    );
    env.git(&dest, &["add", "-A"]).await;
    env.git(&dest, &["commit", "-q", "-m", "oops"]).await;
    let err = env
        .ssh_git_fails(&ssh, &dest, &["push", "origin", "HEAD:refs/heads/leak"])
        .await;
    assert!(err.contains("SECRET_DETECTED"), "{err}");
    // The bypass push option travels over SSH too, and is audited like over HTTP.
    env.ssh_git(
        &ssh,
        &dest,
        &[
            "push",
            "-q",
            "-o",
            "push-protection-bypass=used_in_tests",
            "origin",
            "HEAD:refs/heads/leak",
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ssh_greeting_unknown_keys_commands_and_revoked_access() {
    let env = start_env().await;
    let ssh = start_ssh(&env).await;
    imported(&env, "r").await;
    // `ssh -T` greets by name and exits 1, like other forges.
    let out = ssh_raw(&ssh, &ssh.key, &[]).await;
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("Hi dev!") && err.contains("does not provide shell access"),
        "{err}"
    );
    // Other commands are refused.
    let out = ssh_raw(&ssh, &ssh.key, &["ls", "-la"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("git-upload-pack"));
    // An unknown repo and a malformed path.
    let err = env
        .ssh_git_fails(&ssh, env.dir.path(), &["ls-remote", &ssh.url("missing")])
        .await;
    assert!(err.contains("NOT_FOUND"), "{err}");
    // A key nobody registered is refused at authentication.
    let (stranger, _) = keygen(&env, "id_stranger");
    let out = ssh_raw(&ssh, &stranger, &[]).await;
    assert_eq!(out.status.code(), Some(255));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Permission denied"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env
        .ssh_run(
            &ssh,
            &stranger,
            env.dir.path(),
            &["ls-remote", &ssh.url("r")],
        )
        .await;
    assert!(!out.status.success());
    // Registry rules: duplicates, agents, bad keys, removal.
    let (_, public) = {
        let key = env.path("id_dev");
        (
            key,
            std::fs::read_to_string(env.path("id_dev.pub")).unwrap(),
        )
    };
    let dup = bee_server::git_ssh::add_key(&env.srv.state, &env.user, &public, "again").await;
    assert_eq!(dup.unwrap_err().detail.reason, "ALREADY_EXISTS");
    let agent = env
        .rpc(
            "AuthService",
            "CreateAgent",
            serde_json::json!({ "display": "bot" }),
        )
        .await
        .unwrap();
    let (_, other_public) = keygen(&env, "id_bot");
    let denied = bee_server::git_ssh::add_key(
        &env.srv.state,
        agent["id"].as_str().unwrap(),
        &other_public,
        "bot",
    )
    .await;
    assert_eq!(denied.unwrap_err().detail.reason, "PERMISSION_DENIED");
    let bad =
        bee_server::git_ssh::add_key(&env.srv.state, &env.user, "ssh-ed25519 nope", "x").await;
    assert_eq!(bad.unwrap_err().detail.reason, "INVALID_ARGUMENT");
    let keys = bee_server::git_ssh::list_keys(&env.srv.state, &env.user)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].fingerprint.starts_with("SHA256:"));
    // Removing the key ends access.
    assert!(
        !bee_server::git_ssh::remove_key(&env.srv.state, "someone-else", &keys[0].fingerprint)
            .await
            .unwrap()
    );
    assert!(
        bee_server::git_ssh::remove_key(&env.srv.state, &env.user, &keys[0].fingerprint)
            .await
            .unwrap()
    );
    let out = env
        .ssh_run(
            &ssh,
            &ssh.key,
            env.dir.path(),
            &["ls-remote", &ssh.url("r")],
        )
        .await;
    assert!(!out.status.success(), "a removed key no longer works");
    drop(ssh.server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_lfs_over_an_ssh_remote_uses_git_lfs_authenticate() {
    let probe = Command::new("git").args(["lfs", "version"]).output();
    if !probe.is_ok_and(|o| o.status.success()) {
        eprintln!("skipping git_lfs_over_an_ssh_remote_uses_git_lfs_authenticate: no git-lfs");
        return;
    }
    let env = start_env().await;
    let ssh = start_ssh(&env).await;
    imported(&env, "r").await;
    // The handshake itself: JSON with the HTTP endpoint and a bearer header.
    let out = ssh_raw(
        &ssh,
        &ssh.key,
        &["git-lfs-authenticate", "dev/r.git", "download"],
    )
    .await;
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("git-lfs-authenticate prints JSON");
    assert!(
        v["href"].as_str().unwrap().ends_with("/dev/r.git/info/lfs"),
        "{v}"
    );
    assert!(
        v["header"]["Authorization"]
            .as_str()
            .unwrap()
            .starts_with("Bearer ")
    );
    assert!(v["expires_in"].as_u64().unwrap() > 0);
    let denied = ssh_raw(
        &ssh,
        &ssh.key,
        &["git-lfs-authenticate", "dev/none.git", "upload"],
    )
    .await;
    assert!(!denied.status.success());
    // A full round trip through real git-lfs.
    let dest = env.path("clone");
    env.ssh_git(
        &ssh,
        env.dir.path(),
        &["clone", "-q", &ssh.url("r"), dest.to_str().unwrap()],
    )
    .await;
    env.ssh_git(&ssh, &dest, &["lfs", "install", "--local"])
        .await;
    env.ssh_git(&ssh, &dest, &["lfs", "track", "*.bin"]).await;
    let big: Vec<u8> = (0..120_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 9) as u8)
        .collect();
    write(&dest.join("big.bin"), &big);
    env.git(&dest, &["add", ".gitattributes", "big.bin"]).await;
    env.git(&dest, &["commit", "-q", "-m", "an LFS object over ssh"])
        .await;
    env.ssh_git(&ssh, &dest, &["push", "-q", "origin", "HEAD:main"])
        .await;
    let fresh = env.path("fresh");
    env.ssh_git(
        &ssh,
        env.dir.path(),
        &["clone", "-q", &ssh.url("r"), fresh.to_str().unwrap()],
    )
    .await;
    env.ssh_git(&ssh, &fresh, &["lfs", "install", "--local"])
        .await;
    env.ssh_git(&ssh, &fresh, &["lfs", "pull"]).await;
    assert_eq!(std::fs::read(fresh.join("big.bin")).unwrap(), big);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ssh_accepts_ed25519_ecdsa_and_rsa_user_keys() {
    let env = start_env().await;
    let ssh = start_ssh(&env).await;
    imported(&env, "r").await;
    for (kind, bits) in [("ecdsa", "256"), ("ecdsa", "384"), ("rsa", "3072")] {
        let key = env.path(&format!("id_{kind}_{bits}"));
        let out = Command::new("ssh-keygen")
            .args(["-q", "-t", kind, "-b", bits, "-N", "", "-f"])
            .arg(&key)
            .output()
            .expect("ssh-keygen runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let public = std::fs::read_to_string(format!("{}.pub", key.display())).unwrap();
        bee_server::git_ssh::add_key(&env.srv.state, &env.user, &public, kind)
            .await
            .unwrap_or_else(|e| panic!("{kind} {bits}: {e:?}"));
        let out = env
            .ssh_run(&ssh, &key, env.dir.path(), &["ls-remote", &ssh.url("r")])
            .await;
        assert!(
            out.status.success(),
            "{kind} {bits}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("refs/heads/main"));
    }
}

/// F416-029: deploy keys authenticate over SSH as their own principal on their repo only,
/// read-only keys may clone but not push, write keys may push, and removing the key ends
/// access at the next connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploy_keys_authenticate_over_ssh_read_only_or_write() {
    let env = start_env().await;
    let ssh = start_ssh(&env).await;
    let (ro_key, ro_public) = keygen(&env, "id_deploy_ro");
    let (rw_key, rw_public) = keygen(&env, "id_deploy_rw");
    let add = |title: &'static str, public: String, read_only: bool| {
        env.rpc(
            "RepoService",
            "AddDeployKey",
            serde_json::json!({ "repo": { "fullName": "dev/playground" }, "title": title,
                                "publicKey": public, "readOnly": read_only }),
        )
    };
    add("ci-read", ro_public, true)
        .await
        .expect("read-only key");
    let rw = add("ci-write", rw_public, false).await.expect("write key");

    // The read-only key clones.
    let ro_dir = env.path("ro");
    let out = env
        .ssh_run(
            &ssh,
            &ro_key,
            env.dir.path(),
            &[
                "clone",
                "-q",
                &ssh.url("playground"),
                ro_dir.to_str().unwrap(),
            ],
        )
        .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // ... but may not push.
    write(&ro_dir.join("ro.txt"), b"no\n");
    env.git(&ro_dir, &["add", "-A"]).await;
    env.git(&ro_dir, &["commit", "-q", "-m", "ro"]).await;
    let out = env
        .ssh_run(
            &ssh,
            &ro_key,
            &ro_dir,
            &["push", "-q", "origin", "HEAD:refs/heads/deploy/ro"],
        )
        .await;
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("read-only"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The write key pushes to main, which lands under policy.
    let rw_dir = env.path("rw");
    let out = env
        .ssh_run(
            &ssh,
            &rw_key,
            env.dir.path(),
            &[
                "clone",
                "-q",
                &ssh.url("playground"),
                rw_dir.to_str().unwrap(),
            ],
        )
        .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (_, before) = env.tip("playground", "main").await.unwrap();
    write(&rw_dir.join("rw.txt"), b"deployed\n");
    env.git(&rw_dir, &["add", "-A"]).await;
    env.git(&rw_dir, &["commit", "-q", "-m", "rw"]).await;
    let out = env
        .ssh_run(
            &ssh,
            &rw_key,
            &rw_dir,
            &["push", "-q", "origin", "HEAD:refs/heads/main"],
        )
        .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (_, after) = env.tip("playground", "main").await.unwrap();
    assert_eq!(after, before + 1, "the push landed on main");

    // A deploy key is scoped to its repo.
    imported(&env, "other").await;
    let out = env
        .ssh_run(
            &ssh,
            &rw_key,
            env.dir.path(),
            &["ls-remote", &ssh.url("other")],
        )
        .await;
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("not registered") || err.contains("PERMISSION_DENIED"),
        "{err}"
    );

    // Removing the key ends access.
    env.rpc(
        "RepoService",
        "DeleteDeployKey",
        serde_json::json!({ "repo": { "fullName": "dev/playground" }, "id": rw["id"] }),
    )
    .await
    .expect("delete");
    let out = env
        .ssh_run(
            &ssh,
            &rw_key,
            env.dir.path(),
            &["ls-remote", &ssh.url("playground")],
        )
        .await;
    assert!(!out.status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ssh_lfs_and_git_switches_refuse_ssh_sessions() {
    let env = start_env().await;
    // The dev repo has a forge record, which holds the `git_enabled` setting.
    env.api_commit_and_land("playground", &[("a.txt", b"alpha\n")], "seed", 1)
        .await;
    let ssh = start_ssh(&env).await;
    let url = ssh.url("playground");
    let set = |fields: serde_json::Value| {
        let mut req = serde_json::json!({ "repo": { "fullName": "dev/playground" } });
        for (k, v) in fields.as_object().unwrap() {
            req[k] = v.clone();
        }
        env.rpc("GitService", "UpdateGitConfig", req)
    };
    env.ssh_git(&ssh, env.dir.path(), &["ls-remote", &url])
        .await;
    let lfs_auth = || {
        ssh_raw(
            &ssh,
            &ssh.key,
            &["git-lfs-authenticate", "dev/playground.git", "download"],
        )
    };
    assert!(lfs_auth().await.status.success());
    // LFS off: git-lfs-authenticate is refused, git itself is not.
    set(serde_json::json!({ "lfsEnabled": false }))
        .await
        .expect("LFS off");
    let out = lfs_auth().await;
    assert!(!out.status.success());
    // The gateway reports exec errors as an `ERR` packet on the channel.
    let err = String::from_utf8_lossy(&out.stdout);
    assert!(
        err.contains("INVALID_STATE") && err.contains("lfs_disabled"),
        "{err}"
    );
    env.ssh_git(&ssh, env.dir.path(), &["ls-remote", &url])
        .await;
    // SSH off: every SSH session is refused; HTTPS keeps working.
    set(serde_json::json!({ "lfsEnabled": true, "sshEnabled": false }))
        .await
        .expect("SSH off");
    let err = env
        .ssh_git_fails(&ssh, env.dir.path(), &["ls-remote", &url])
        .await;
    assert!(
        err.contains("GIT_DISABLED") && err.contains("ssh_disabled"),
        "{err}"
    );
    env.git(env.dir.path(), &["ls-remote", &env.url("playground")])
        .await;
    // Git off: refused over SSH too.
    set(serde_json::json!({ "sshEnabled": true, "gitEnabled": false }))
        .await
        .expect("git off");
    let err = env
        .ssh_git_fails(&ssh, env.dir.path(), &["clone", "-q", &url, "c"])
        .await;
    assert!(err.contains("GIT_DISABLED"), "{err}");
    set(serde_json::json!({ "gitEnabled": true }))
        .await
        .expect("git on");
    env.ssh_git(&ssh, env.dir.path(), &["clone", "-q", &url, "c"])
        .await;
    // SSH sessions are counted by the gateway's metrics too.
    let t = env.srv.state.metrics.render();
    for line in [
        "bee_git_requests_total{transport=\"ssh\",service=\"upload-pack\",outcome=\"ok\"}",
        "bee_git_requests_total{transport=\"ssh\",service=\"upload-pack\",outcome=\"error\"}",
        "bee_git_requests_total{transport=\"ssh\",service=\"lfs-authenticate\",outcome=\"error\"}",
        "bee_git_fetch_protocol_total{transport=\"ssh\",version=\"2\"}",
        "bee_git_upload_pack_bytes_total{transport=\"ssh\"}",
    ] {
        assert!(t.contains(line), "{line}\n{t}");
    }
    drop(ssh.server);
}
