//! The unmodified `gh` CLI against a real in-process `bee-server` with the gateway mounted
//! (PRODUCT real-world check 3). Opt-in: set `BEE_TEST_GH=1` (needs `gh` on `PATH`).
//!
//! `gh` is pointed at `GH_HOST=github.localhost`, for which it uses plain HTTP at
//! `http://api.github.localhost/` (REST) and `/graphql`; an `HTTP_PROXY` naming the test
//! server delivers those requests to it, so no TLS or DNS setup is needed.

use std::process::Command;

use base64::Engine as _;
use bee_server::config::Config;
use bee_server::server::{ServerOptions, start};
use serde_json::Value;

struct Gh {
    proxy: String,
    token: String,
    home: tempfile::TempDir,
}

impl Gh {
    fn run(&self, args: &[&str]) -> String {
        let (ok, stdout, stderr) = self.exec(args);
        assert!(ok, "gh {args:?} failed\nstdout: {stdout}\nstderr: {stderr}");
        stdout
    }

    /// Runs a command that must fail; returns its stderr.
    fn fails(&self, args: &[&str]) -> String {
        let (ok, stdout, stderr) = self.exec(args);
        assert!(!ok, "gh {args:?} should fail\nstdout: {stdout}");
        stderr
    }

    fn exec(&self, args: &[&str]) -> (bool, String, String) {
        let out = Command::new("gh")
            .args(args)
            .env("GH_HOST", "github.localhost")
            .env("GH_TOKEN", &self.token)
            .env("GH_ENTERPRISE_TOKEN", &self.token)
            .env("HTTP_PROXY", &self.proxy)
            .env("HTTPS_PROXY", &self.proxy)
            .env("http_proxy", &self.proxy)
            .env("https_proxy", &self.proxy)
            .env("NO_PROXY", "")
            .env("no_proxy", "")
            .env("GH_CONFIG_DIR", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path())
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_SPINNER_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env("GH_PAGER", "cat")
            .current_dir(self.home.path())
            .output()
            .expect("run gh");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("gh {args:?}: {e}: {out}"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gh_cli_drives_the_gateway() {
    if std::env::var("BEE_TEST_GH").ok().as_deref() != Some("1") {
        eprintln!("skipped: set BEE_TEST_GH=1 to run the gh CLI against the gateway");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::dev(dir.path().to_path_buf(), Some("127.0.0.1:0".into()));
    cfg.forge.relay_poll_ms = 50;
    cfg.forge.webhook_poll_ms = 50;
    cfg.forge.bridge_interval_ms = 50;
    let mut opts = ServerOptions::new(cfg);
    opts.dev = true;
    let srv = start(opts).await.expect("server starts");
    let token = srv.dev.as_ref().expect("dev bootstrap").token.clone();
    let gh = Gh {
        proxy: srv.url.clone(),
        token,
        home: tempfile::tempdir().expect("gh home"),
    };
    let r = "dev/playground";
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);

    // gh api: REST through the root aliases.
    let me = gh.json(&["api", "user"]);
    assert_eq!(me["login"], "dev");
    let repo = gh.json(&["api", &format!("repos/{r}")]);
    assert_eq!(repo["full_name"], r);
    let main = gh.json(&["api", &format!("repos/{r}/git/ref/heads/main")]);
    let sha = main["object"]["sha"].as_str().expect("sha").to_string();
    gh.run(&[
        "api",
        &format!("repos/{r}/git/refs"),
        "-f",
        "ref=refs/heads/feature",
        "-f",
        &format!("sha={sha}"),
    ]);
    // `main` is protected in dev mode: it moves only through a land (a PR merge).
    let refused = gh.fails(&[
        "api",
        "-X",
        "PUT",
        &format!("repos/{r}/contents/README.md"),
        "-f",
        "message=Direct",
        "-f",
        &format!("content={}", b64("x\n")),
        "-f",
        "branch=main",
    ]);
    assert!(refused.contains("Protected branch"), "{refused}");
    gh.run(&[
        "api",
        "-X",
        "PUT",
        &format!("repos/{r}/contents/README.md"),
        "-f",
        "message=Add readme",
        "-f",
        &format!("content={}", b64("# playground\n")),
        "-f",
        "branch=feature",
    ]);
    gh.run(&[
        "api",
        "-X",
        "PUT",
        &format!("repos/{r}/contents/feature.txt"),
        "-f",
        "message=Add feature",
        "-f",
        &format!("content={}", b64("feature\n")),
        "-f",
        "branch=feature",
    ]);
    let paged = gh.run(&[
        "api",
        "--paginate",
        &format!("repos/{r}/labels?per_page=2"),
        "--jq",
        ".[].name",
    ]);
    assert!(
        paged.lines().count() >= 3,
        "--paginate follows Link headers: {paged}"
    );

    // gh repo view (GraphQL repository query and REST readme).
    let rv = gh.json(&[
        "repo",
        "view",
        r,
        "--json",
        "name,nameWithOwner,defaultBranchRef,viewerPermission",
    ]);
    assert_eq!(rv["nameWithOwner"], r);
    assert_eq!(rv["defaultBranchRef"]["name"], "main");

    // gh issue create / list / view / comment / close.
    let url = gh.run(&[
        "issue",
        "create",
        "-R",
        r,
        "--title",
        "First issue",
        "--body",
        "from gh",
    ]);
    assert!(url.trim().ends_with("/dev/playground/issues/1"), "{url}");
    let issues = gh.json(&[
        "issue",
        "list",
        "-R",
        r,
        "--json",
        "number,title,state,author",
    ]);
    assert_eq!(issues[0]["title"], "First issue");
    assert_eq!(issues[0]["author"]["login"], "dev");
    gh.run(&["issue", "comment", "1", "-R", r, "--body", "a comment"]);
    let iv = gh.json(&[
        "issue",
        "view",
        "1",
        "-R",
        r,
        "--json",
        "title,body,comments,state",
    ]);
    assert_eq!(iv["comments"].as_array().map(Vec::len), Some(1));
    gh.run(&["issue", "close", "1", "-R", r]);
    let closed = gh.json(&[
        "issue", "list", "-R", r, "--state", "closed", "--json", "number",
    ]);
    assert_eq!(closed[0]["number"], 1);

    // gh pr create / list / view / status / merge.
    let url = gh.run(&[
        "pr",
        "create",
        "-R",
        r,
        "--head",
        "feature",
        "--base",
        "main",
        "--title",
        "Add feature",
        "--body",
        "via gh",
    ]);
    assert!(
        url.trim().ends_with("/dev/playground/pull/2"),
        "numbers are shared with issues: {url}"
    );
    let prs = gh.json(&[
        "pr",
        "list",
        "-R",
        r,
        "--json",
        "number,title,headRefName,state,author,isDraft",
    ]);
    assert_eq!(prs[0]["number"], 2);
    assert_eq!(prs[0]["headRefName"], "feature");
    assert_eq!(prs[0]["state"], "OPEN");
    let pv = gh.json(&[
        "pr",
        "view",
        "2",
        "-R",
        r,
        "--json",
        "number,title,state,mergeable,files,commits,additions",
    ]);
    let paths: Vec<&str> = pv["files"]
        .as_array()
        .expect("files")
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();
    assert!(
        paths.contains(&"feature.txt") && paths.contains(&"README.md"),
        "{paths:?}"
    );
    gh.run(&["pr", "view", "2", "-R", r]);
    gh.run(&["pr", "status", "-R", r]);
    gh.run(&[
        "pr",
        "review",
        "2",
        "-R",
        r,
        "--comment",
        "--body",
        "Reviewed with gh",
    ]);
    let reviews = gh.json(&["pr", "view", "2", "-R", r, "--json", "reviews"]);
    assert_eq!(reviews["reviews"][0]["body"], "Reviewed with gh");
    let head = gh.json(&["pr", "view", "2", "-R", r, "--json", "headRefOid"]);
    let head = head["headRefOid"].as_str().expect("head oid").to_string();
    gh.run(&[
        "api",
        &format!("repos/{r}/statuses/{head}"),
        "-f",
        "state=success",
        "-f",
        "context=ci/lint",
    ]);
    let checks = gh.run(&["pr", "checks", "2", "-R", r]);
    assert!(checks.contains("ci/lint"), "{checks}");
    let q = gh.json(&["api", "graphql", "-f", "query=query { viewer { login } }"]);
    assert_eq!(q["data"]["viewer"]["login"], "dev");
    gh.run(&["pr", "merge", "2", "-R", r, "--squash"]);
    let merged = gh.json(&["pr", "view", "2", "-R", r, "--json", "state,mergedAt"]);
    assert_eq!(merged["state"], "MERGED");
    let readme = gh.run(&["repo", "view", r]);
    assert!(
        readme.contains("playground"),
        "the README landed on main: {readme}"
    );

    // gh release create / list / view.
    let rel = gh.run(&[
        "release",
        "create",
        "v1.0.0",
        "-R",
        r,
        "--title",
        "One",
        "--notes",
        "first release",
        "--target",
        "main",
    ]);
    assert!(rel.contains("v1.0.0"), "{rel}");
    let rels = gh.run(&["release", "list", "-R", r]);
    assert!(rels.contains("v1.0.0"), "{rels}");
    let rv = gh.json(&[
        "release",
        "view",
        "v1.0.0",
        "-R",
        r,
        "--json",
        "tagName,name,body",
    ]);
    assert_eq!(rv["name"], "One");

    // gh run list (Actions), gh workflow list.
    let runs = gh.json(&[
        "run",
        "list",
        "-R",
        r,
        "--json",
        "databaseId,status,workflowName",
    ]);
    assert!(runs.is_array());
    gh.run(&["workflow", "list", "-R", r]);

    // gh secret set: Go's sealed box opened by the gateway.
    gh.run(&["secret", "set", "API_TOKEN", "-R", r, "--body", "hunter2"]);
    let secrets = gh.run(&["secret", "list", "-R", r]);
    assert!(secrets.contains("API_TOKEN"), "{secrets}");

    srv.shutdown().await;
}
