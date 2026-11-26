//! Plain `git push` under rulesets created through `PolicyService` (SPEC §9.4): a ruleset adds
//! no implicit human approval, so an agent pushes to main what the ruleset allows. Mirrors the
//! setup of scenario AS-GIT-01.

use crate::common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_pushes_to_main_under_a_restricted_paths_ruleset() {
    let env = start_env().await;
    imported(&env, "r").await;
    env.api_commit_and_land(
        "r",
        &[("secrets/README.md", b"never commit here\n")],
        "seed",
        3,
    )
    .await;
    env.rpc(
        "PolicyService",
        "CreateRuleset",
        json!({
            "repo": { "fullName": "dev/r" }, "name": "protect-secrets",
            "target": "RULESET_TARGET_BRANCH",
            "enforcement": "RULESET_ENFORCEMENT_ACTIVE",
            "conditions": { "refNameIncludes": ["~DEFAULT_BRANCH"] },
            "rules": [{ "restrictedPaths": { "pathGlobs": ["secrets/**"] } }],
        }),
    )
    .await
    .expect("create ruleset");
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
    env.git(&dest, &["checkout", "-q", "-b", "docs"]).await;

    // Under secrets/: refused, main unchanged.
    write(&dest.join("secrets/notes.txt"), b"rotation schedule\n");
    env.git(&dest, &["add", "secrets/notes.txt"]).await;
    env.git(&dest, &["commit", "-q", "-m", "notes"]).await;
    let before = env.tip("r", "main").await.unwrap();
    let err = env.git_fails(&dest, &["push", "origin", "HEAD:main"]).await;
    assert!(err.contains("POLICY_DENIED"), "{err}");
    assert!(!err.contains("approval:human"), "{err}");
    assert_eq!(env.tip("r", "main").await.unwrap(), before);

    // Anywhere else: the agent's push lands on main.
    env.git(&dest, &["reset", "-q", "--hard", "HEAD~1"]).await;
    write(
        &dest.join("CONTRIBUTING.md"),
        b"Run the tests before pushing.\n",
    );
    env.git(&dest, &["add", "CONTRIBUTING.md"]).await;
    env.git(&dest, &["commit", "-q", "-m", "Add CONTRIBUTING.md"])
        .await;
    env.git(&dest, &["push", "-q", "origin", "HEAD:main"]).await;
    assert_eq!(
        env.log_messages("r", "main").await[0],
        "Add CONTRIBUTING.md\n"
    );
    assert_ne!(env.tip("r", "main").await.unwrap(), before);
}
