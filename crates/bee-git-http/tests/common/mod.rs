//! Shared harness for the git gateway tests: an in-process dev server and a git client environment.

#![allow(dead_code, missing_docs, clippy::result_large_err)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use base64::Engine as _;
use bee_proto::v1::ErrorDetail;
use bee_server::config::Config;
use bee_server::server::{RunningServer, ServerOptions, start};
use serde_json::{Value, json};

pub const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

pub struct Env {
    pub srv: RunningServer,
    pub token: String,
    pub user: String,
    pub dir: tempfile::TempDir,
}

pub async fn start_env() -> Env {
    start_env_with(None).await
}

/// As [`start_env`], with the server's clock injected.
pub async fn start_env_with(clock: Option<std::sync::Arc<dyn bee_store::Clock>>) -> Env {
    start_env_cfg(clock, |_| {}).await
}

/// As [`start_env_with`], with the server's configuration adjusted by `tweak`.
pub async fn start_env_cfg(
    clock: Option<std::sync::Arc<dyn bee_store::Clock>>,
    tweak: impl FnOnce(&mut Config),
) -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mut cfg = Config::dev(data, Some("127.0.0.1:0".into()));
    cfg.redaction.purge_interval_secs = 3600;
    tweak(&mut cfg);
    let mut opts = ServerOptions::new(cfg);
    opts.dev = true;
    if let Some(c) = clock {
        opts.clock = c;
    }
    let srv = start(opts).await.expect("server starts");
    let dev = srv.dev.clone().expect("dev bootstrap");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // No background maintenance: after a fetch, git 2.56 starts `git maintenance run --auto
    // --detach`, which repacks the clone while the test's next command (`git fsck`) reads it.
    std::fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = Test User\n\temail = test@example.com\n[init]\n\tdefaultBranch = main\n\
         [advice]\n\tdetachedHead = false\n[core]\n\tautocrlf = false\n[lfs]\n\tlocksverify = false\n\
         [maintenance]\n\tauto = false\n[gc]\n\tauto = 0\n",
    )
    .unwrap();
    Env {
        srv,
        token: dev.token,
        user: dev.user,
        dir,
    }
}

impl Env {
    /// The gateway's pack cache `(hits, misses, cached bytes)`.
    pub fn pack_cache(&self) -> (u64, u64, usize) {
        self.srv
            .state
            .git_gateway
            .get()
            .expect("the gateway is mounted")
            .cache()
            .stats()
    }

    /// Waits until no post-land warming task runs (a repo stays queued while its task works).
    pub async fn warmed(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let idle = self
                .srv
                .state
                .warm_queue
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty();
            if idle {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "post-land warming did not finish"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn addr(&self) -> String {
        self.srv.addr.to_string()
    }

    /// A clone URL carrying `token` as the Basic password.
    pub fn url_as(&self, token: &str, repo: &str) -> String {
        let enc: String = token
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        format!("http://git:{enc}@{}/dev/{repo}.git", self.addr())
    }

    pub fn url(&self, repo: &str) -> String {
        self.url_as(&self.token, repo)
    }

    pub fn git_cmd(&self, dir: &Path, args: &[&str]) -> Command {
        let mut c = Command::new("git");
        c.args(args)
            .current_dir(dir)
            .env("HOME", self.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home().join(".gitconfig"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_DATE", "2026-10-18T12:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-10-18T12:00:00Z")
            .env("LC_ALL", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(Stdio::null());
        c
    }

    pub async fn run(&self, dir: &Path, args: &[&str]) -> Output {
        let mut c = self.git_cmd(dir, args);
        tokio::task::spawn_blocking(move || c.output().expect("git runs"))
            .await
            .unwrap()
    }

    /// Runs git and returns stdout; panics with stderr on failure.
    pub async fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.run(dir, args).await;
        assert!(
            out.status.success(),
            "git {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Runs git expecting failure; returns stderr.
    pub async fn git_fails(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.run(dir, args).await;
        assert!(
            !out.status.success(),
            "git {args:?} should fail:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8_lossy(&out.stderr).to_string()
    }

    pub fn client(&self, token: &str) -> bee_mcp::RpcClient {
        let mut c = bee_mcp::ClientConfig::new(self.srv.url.clone());
        c.token = Some(token.to_string());
        c.request_timeout = Duration::from_secs(120);
        bee_mcp::RpcClient::new(&c)
    }

    pub async fn rpc_as(
        &self,
        token: &str,
        service: &str,
        method: &str,
        req: Value,
    ) -> Result<Value, ErrorDetail> {
        self.client(token)
            .call_rpc_json(service, method, &req)
            .await
    }

    pub async fn rpc(&self, service: &str, method: &str, req: Value) -> Result<Value, ErrorDetail> {
        self.rpc_as(&self.token, service, method, req).await
    }

    /// Imports a local git repository as `dev/<name>` and waits for it.
    pub async fn import(&self, src: &Path, name: &str) {
        let started = self
            .rpc(
                "ImportService",
                "StartImport",
                json!({ "source": format!("dev/{name}"), "sourceKind": "IMPORT_SOURCE_KIND_GIT",
                        "sourceUrl": src.display().to_string() }),
            )
            .await
            .expect("start import");
        let id = started["id"].as_str().unwrap().to_string();
        for _ in 0..600 {
            let g = self
                .rpc("ImportService", "GetImport", json!({ "id": id }))
                .await
                .unwrap();
            match g["state"].as_str().unwrap_or("") {
                "IMPORT_STATE_LIVE" => return,
                "IMPORT_STATE_FAILED" => panic!("import failed: {g}"),
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        panic!("import did not finish");
    }

    /// The target changeset id (text) and version of a ref.
    pub async fn tip(&self, repo: &str, name: &str) -> Option<(String, u64)> {
        let r = self
            .rpc(
                "ObjectService",
                "ResolveRef",
                json!({ "repo": { "fullName": format!("dev/{repo}") }, "name": name }),
            )
            .await
            .ok()?;
        Some((
            id_text(&r["ref"]["target"]),
            r["ref"]["version"].as_str().unwrap_or("0").parse().unwrap(),
        ))
    }

    /// Commits `files` on `parent` through the API and lands it on `main`.
    pub async fn api_commit_and_land(
        &self,
        repo: &str,
        files: &[(&str, &[u8])],
        message: &str,
        cid: u8,
    ) {
        let full = json!({ "fullName": format!("dev/{repo}") });
        let (parent, _) = self.tip(repo, "main").await.expect("main");
        let mut messages = Vec::new();
        let mut delta = Vec::new();
        for (i, (path, bytes)) in files.iter().enumerate() {
            let id = bee_object::blob_id(bytes).to_text();
            let mut m = json!({ "id": id_json(&id), "offset": "0", "data": B64.encode(bytes), "last": true });
            if i == 0 {
                m["repo"] = full.clone();
            }
            messages.push(m);
            delta.push(json!({ "path": B64.encode(path.as_bytes()),
                "put": { "mode": "MODE_FILE", "blob": id_json(&id), "size": bytes.len().to_string() } }));
        }
        self.rpc(
            "WriteService",
            "UploadObjects",
            json!({ "messages": messages }),
        )
        .await
        .expect("upload");
        let mut change = [cid; 16];
        change[0] = 0x42;
        let cs = self
            .rpc(
                "WriteService",
                "CreateChangeset",
                json!({ "repo": full, "parents": [id_json(&parent)], "delta": delta,
                        "changeId": { "raw": B64.encode(change) }, "author": "API Author <api@example.com>",
                        "authorTime": { "name": "x", "time": "2026-10-18T13:00:00Z" }, "message": message }),
            )
            .await
            .expect("create changeset");
        self.rpc(
            "WriteService",
            "Land",
            json!({ "repo": full, "ref": "main", "head": cs["changeset"]["id"] }),
        )
        .await
        .expect("land");
    }

    /// Commit messages on a ref, newest first (API `Log`).
    pub async fn log_messages(&self, repo: &str, from: &str) -> Vec<String> {
        let log = self
            .rpc(
                "QueryService",
                "Log",
                json!({ "repo": { "fullName": format!("dev/{repo}") }, "from": { "expr": from }, "includeMeta": true }),
            )
            .await
            .expect("log");
        log["changesets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["meta"]["message"].as_str().unwrap_or("").to_string())
            .collect()
    }

    /// A new agent principal and a token for it.
    pub async fn agent_token(&self, name: &str) -> String {
        let agent = self
            .rpc("AuthService", "CreateAgent", json!({ "display": name }))
            .await
            .unwrap();
        let minted = self
            .rpc(
                "AuthService",
                "MintToken",
                json!({ "principalId": agent["id"] }),
            )
            .await
            .unwrap();
        minted["token"].as_str().unwrap().to_string()
    }
}

pub fn id_json(text: &str) -> Value {
    let id = bee_core::ObjectId::parse(text).expect("id");
    json!({ "raw": B64.encode(id.as_bytes()) })
}

pub fn id_text(v: &Value) -> String {
    let raw = B64.decode(v["raw"].as_str().expect("raw")).expect("base64");
    bee_core::ObjectId::from_bytes(&raw).unwrap().to_text()
}

pub fn write(path: &Path, bytes: &[u8]) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

/// A source repository with files, an executable, a symlink, nested directories, a merge and
/// a tag.
pub async fn make_source(env: &Env) -> PathBuf {
    let src = env.path("src");
    std::fs::create_dir_all(&src).unwrap();
    env.git(&src, &["init", "-q"]).await;
    write(&src.join("a.txt"), b"alpha\n");
    write(&src.join("dir/b.txt"), b"bravo\n");
    write(&src.join("script.sh"), b"#!/bin/sh\necho hi\n");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            src.join("script.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();
    }
    env.git(&src, &["add", "-A"]).await;
    env.git(&src, &["commit", "-q", "-m", "first"]).await;
    write(&src.join("a.txt"), b"alpha 2\n");
    write(&src.join("dir/sub/c.txt"), b"charlie\n");
    env.git(&src, &["add", "-A"]).await;
    env.git(&src, &["commit", "-q", "-m", "second\n\nwith a body"])
        .await;
    env.git(&src, &["checkout", "-q", "-b", "feature"]).await;
    write(&src.join("f.txt"), b"feature\n");
    env.git(&src, &["add", "-A"]).await;
    env.git(&src, &["commit", "-q", "-m", "feature work"]).await;
    env.git(&src, &["checkout", "-q", "main"]).await;
    write(&src.join("m.txt"), b"main work\n");
    env.git(&src, &["add", "-A"]).await;
    env.git(&src, &["commit", "-q", "-m", "main work"]).await;
    env.git(
        &src,
        &["merge", "-q", "--no-ff", "feature", "-m", "merge feature"],
    )
    .await;
    env.git(&src, &["tag", "v1"]).await;
    write(&src.join("dir/b.txt"), b"bravo 2\n");
    env.git(&src, &["commit", "-q", "-am", "last"]).await;
    src
}

pub async fn imported(env: &Env, name: &str) -> PathBuf {
    let src = make_source(env).await;
    env.import(&src, name).await;
    src
}
