//! The `bee-github` binary: `--json` output and error envelopes.

use crate::common;
use common::*;

fn run(args: &[&str]) -> (bool, serde_json::Value) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bee-github"))
        .args(args)
        .env_remove("BEE_ENDPOINT")
        .output()
        .unwrap();
    let v = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}): {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.success(), v)
}

#[tokio::test(flavor = "multi_thread")]
async fn as_mig_cli_dry_run_and_errors_are_json() {
    let w = World::new(false).await;
    let state = w.state.path().display().to_string();
    let base = w.base.clone();
    let st = state.clone();
    let (ok, v) = tokio::task::spawn_blocking(move || {
        run(&[
            "import",
            FULL,
            "--dry-run",
            "--api-url",
            &base,
            "--github-token",
            "t",
            "--state",
            &st,
            "--json",
        ])
    })
    .await
    .unwrap();
    assert!(ok, "{v}");
    assert_eq!(v["pull_requests"], 3);
    assert_eq!(v["issues"], 2);
    let (ok, v) = tokio::task::spawn_blocking(move || {
        run(&["import", "not-a-repo", "--github-token", "t", "--json"])
    })
    .await
    .unwrap();
    assert!(!ok);
    assert_eq!(v["error"]["reason"], "INVALID_ARGUMENT");
    let (ok, v) = tokio::task::spawn_blocking(move || {
        run(&[
            "claim-token",
            FULL,
            "1001",
            "--github-token",
            "t",
            "--state",
            &state,
            "--json",
        ])
    })
    .await
    .unwrap();
    assert!(ok, "{v}");
    assert_eq!(v["principal"], bee_github::principals::principal_id(1001));
    assert_eq!(v["claim_token"].as_str().unwrap().len(), 32);
}
