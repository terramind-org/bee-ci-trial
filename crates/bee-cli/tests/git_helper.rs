//! `bee git setup` and `bee git credential` with the real `git` against an in-process
//! `bee-server --dev`: after `bee login`, a plain repository is pointed at Beehive, pushes
//! and clones with no token in any URL (git asks the helper), and the generated preflight and
//! credential commands answer. Every `--json` document is validated against its schema.

use crate::support;

use std::path::Path;
use std::process::Command;

use bee_server::config::Config;
use bee_server::server::{ServerOptions, start};
use serde_json::Value;
use support::{Bee, validate};

/// Runs `bee <args> --json` with `config` (the `bee login` credentials) and returns `data`.
fn bee_json(config: &Path, dir: &Path, args: &[&str]) -> Value {
    let mut all = args.to_vec();
    all.push("--json");
    let run = Bee::new(&all)
        .env("BEE_CONFIG_DIR", config.to_str().unwrap())
        .current_dir(dir)
        .run()
        .0;
    assert_eq!(run.code, 0, "bee {args:?}: {run:?}");
    let doc = run.json();
    let cmd: Vec<&str> = args.iter().take(2).copied().collect();
    validate(
        &format!("schemas/v1/cli/{}.schema.json", cmd.join(".")),
        &doc,
    );
    doc["data"].clone()
}

/// `git <args>` in `dir` with an isolated HOME; the helper finds `bee login`'s credentials
/// through `BEE_CONFIG_DIR`.
fn git(home: &Path, config: &Path, dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Dev")
        .env("GIT_AUTHOR_EMAIL", "dev@example.com")
        .env("GIT_COMMITTER_NAME", "Dev")
        .env("GIT_COMMITTER_EMAIL", "dev@example.com")
        .env("BEE_CONFIG_DIR", config)
        .env_remove("BEE_TOKEN")
        .env_remove("BEE_ENDPOINT")
        .output()
        .expect("git runs")
}

fn ok(out: std::process::Output) -> String {
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn git_setup_installs_the_credential_helper_and_git_pushes_and_clones_without_a_token_in_urls() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut opts = ServerOptions::new(Config::dev(
        tmp.path().join("server"),
        Some("127.0.0.1:0".into()),
    ));
    opts.dev = true;
    let srv = runtime.block_on(start(opts)).unwrap();
    let url = srv.url.clone();
    let token = srv.dev.as_ref().unwrap().token.clone();
    let home = tmp.path().join("home");
    let config = tmp.path().join("config");
    let work = tmp.path().join("work");
    for d in [&home, &config, &work] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(home.join(".gitconfig"), "[init]\n\tdefaultBranch = main\n").unwrap();
    // bee login stores the token and endpoint.
    let login = Bee::new(&["login", "--token", &token, "--endpoint", &url, "--json"])
        .env("BEE_CONFIG_DIR", config.to_str().unwrap())
        .run()
        .0;
    assert_eq!(login.code, 0, "{login:?}");

    // A plain git repository, pointed at Beehive.
    ok(git(&home, &config, &work, &["init", "-q"]));
    std::fs::write(work.join("hello.txt"), "hello over git\n").unwrap();
    ok(git(&home, &config, &work, &["add", "-A"]));
    ok(git(&home, &config, &work, &["commit", "-q", "-m", "hello"]));
    let setup = bee_json(
        &config,
        &work,
        &["git", "setup", "--repo", "dev/playground"],
    );
    let remote_url = format!("{url}/dev/playground.git");
    assert_eq!(setup["url"], remote_url.as_str());
    assert_eq!(setup["remote"], "origin");
    assert_eq!(setup["configScope"], "local");
    assert_eq!(setup["protocolVersion"], 2);
    let problems = setup["preflight"]["problems"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !problems
            .iter()
            .any(|p| p["code"] == "no_credential_helper" || p["code"] == "no_read_access"),
        "{problems:?}"
    );
    assert_eq!(
        ok(git(&home, &config, &work, &["remote", "get-url", "origin"])),
        remote_url,
        "no credential in the remote URL"
    );
    assert_eq!(
        ok(git(&home, &config, &work, &["config", "protocol.version"])),
        "2"
    );
    let helpers = ok(git(
        &home,
        &config,
        &work,
        &["config", "--get-all", &format!("credential.{url}.helper")],
    ));
    assert!(helpers.ends_with("git credential"), "{helpers}");
    // git asks the helper for the token for the push.
    ok(git(
        &home,
        &config,
        &work,
        &["push", "-q", "origin", "HEAD:refs/heads/topic"],
    ));
    let clone = tmp.path().join("clone");
    // A fresh clone with only the helper (from the global config) and no token anywhere.
    let helper = setup["credentialHelper"].as_str().unwrap();
    ok(git(
        &home,
        &config,
        tmp.path(),
        &[
            "config",
            "--global",
            &format!("credential.{url}.helper"),
            helper,
        ],
    ));
    ok(git(
        &home,
        &config,
        tmp.path(),
        &["clone", "-q", "-b", "topic", &remote_url, "clone"],
    ));
    assert_eq!(
        std::fs::read_to_string(clone.join("hello.txt")).unwrap(),
        "hello over git\n"
    );
    // Without the helper the same clone is refused: the credential came from it.
    std::fs::write(home.join(".gitconfig"), "[init]\n\tdefaultBranch = main\n").unwrap();
    let refused = git(
        &home,
        &config,
        tmp.path(),
        &["clone", "-q", &remote_url, "nohelper"],
    );
    assert!(!refused.status.success());

    // The helper protocol itself.
    let host = url.trim_start_matches("http://");
    let answer = Bee::new(&["git", "credential", "get"])
        .env("BEE_CONFIG_DIR", config.to_str().unwrap())
        .stdin(&format!(
            "protocol=http\nhost={host}\npath=dev/playground.git\n\n"
        ))
        .run()
        .0;
    assert_eq!(answer.code, 0, "{answer:?}");
    assert_eq!(
        answer.stdout.trim(),
        format!("username=x-bee-token\npassword={token}")
    );
    let other = Bee::new(&["git", "credential", "get"])
        .env("BEE_CONFIG_DIR", config.to_str().unwrap())
        .stdin("protocol=https\nhost=github.com\n\n")
        .run()
        .0;
    assert_eq!(
        (other.code, other.stdout.trim()),
        (0, ""),
        "other hosts get nothing"
    );
    let stored = bee_json(&config, tmp.path(), &["git", "credential", "store"]);
    assert_eq!(stored["answered"], false);

    // The generated preflight and credential commands.
    let pf = bee_json(
        &config,
        tmp.path(),
        &[
            "git",
            "git-preflight",
            "--repo",
            "dev/playground",
            "--client-git-version",
            "2.20.0",
            "--transport",
            "http",
        ],
    );
    assert!(
        pf["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["code"] == "git_too_old"),
        "{pf}"
    );
    let cred = bee_json(
        &config,
        tmp.path(),
        &["git", "create-git-credential", "--repos", "dev/playground"],
    );
    assert!(
        !cred["password"].as_str().unwrap_or_default().is_empty(),
        "{cred}"
    );
    assert_eq!(cred["username"], "x-bee-token");
    runtime.block_on(srv.shutdown());
}
