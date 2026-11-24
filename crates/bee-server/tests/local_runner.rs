//! Regression coverage for the server-owned development workflow runner.

mod common;

use bee_mcp::{ClientConfig, RpcClient};
use bee_proto::v1::ErrorDetail;
use bee_server::server::{ServerOptions, start};
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_runner_is_opt_in_and_runs_workflows() {
    let disabled = start_default().await;
    let (tip, _) = disabled.tip("main").await;
    let queued = id_text(
        &disabled
            .commit(
                &tip,
                &[(".github/workflows/ci.yml", b"name: CI\non: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo queued\n")],
                1,
                "queued workflow",
            )
            .await["id"],
    );
    disabled.land(&queued).await.expect("land workflow");
    let queued_run = eventually(2, "queued workflow run", || async {
        let runs = disabled
            .call(
                "WorkflowService",
                "ListWorkflowRuns",
                json!({ "repo": repo() }),
            )
            .await
            .ok()?;
        runs["runs"]
            .as_array()?
            .iter()
            .find(|run| run["status"] == "WORKFLOW_STATUS_QUEUED")
            .cloned()
    })
    .await;
    assert_eq!(queued_run["status"], "WORKFLOW_STATUS_QUEUED");
    disabled.srv.shutdown().await;

    let enabled = start_with(|cfg| cfg.forge.local_runner.enabled = Some(true)).await;
    let (tip, _) = enabled.tip("main").await;
    let head = id_text(
        &enabled
            .commit(
                &tip,
                &[(".github/workflows/ci.yml", b"name: CI\non: push\njobs:\n  pass:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo PASS\n  fail:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo FAIL-STEP && exit 1\n")],
                2,
                "workflow outcomes",
            )
            .await["id"],
    );
    enabled.land(&head).await.expect("land workflow");
    let failed = eventually(60, "failed workflow", || async {
        let runs = enabled
            .call(
                "WorkflowService",
                "ListWorkflowRuns",
                json!({ "repo": repo() }),
            )
            .await
            .ok()?;
        runs["runs"]
            .as_array()?
            .iter()
            .find(|run| run["conclusion"] == "WORKFLOW_CONCLUSION_FAILURE")
            .cloned()
    })
    .await;
    assert_eq!(failed["status"], "WORKFLOW_STATUS_COMPLETED");
    let run_id = failed["id"].as_str().expect("run id");
    let logs = enabled
        .call(
            "WorkflowService",
            "SearchWorkflowRunLogs",
            json!({ "repo": repo(), "runId": run_id, "query": "FAIL-STEP" }),
        )
        .await
        .expect("failed step log");
    assert!(
        !logs["matches"].as_array().expect("matches").is_empty(),
        "{logs}"
    );

    let fixed = id_text(
        &enabled
            .commit(
                &head,
                &[(".github/workflows/ci.yml", b"name: CI\non: push\njobs:\n  pass:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo fixed\n")],
                3,
                "fixed workflow",
            )
            .await["id"],
    );
    enabled.land(&fixed).await.expect("land fixed workflow");
    let succeeded = eventually(60, "fixed workflow", || async {
        let runs = enabled
            .call(
                "WorkflowService",
                "ListWorkflowRuns",
                json!({ "repo": repo() }),
            )
            .await
            .ok()?;
        runs["runs"]
            .as_array()?
            .iter()
            .find(|run| run["conclusion"] == "WORKFLOW_CONCLUSION_SUCCESS")
            .cloned()
    })
    .await;
    assert_eq!(succeeded["status"], "WORKFLOW_STATUS_COMPLETED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_runner_preserves_same_named_operator_runner_on_restart() {
    let enabled = start_with(|cfg| cfg.forge.local_runner.enabled = Some(true)).await;
    let registration = enabled
        .call(
            "RunnerService",
            "CreateRunnerRegistrationToken",
            json!({ "scope": { "kind": "RUNNER_SCOPE_KIND_REPO", "repo": repo() } }),
        )
        .await
        .expect("registration token");
    let reserved = enabled
        .call(
            "RunnerService",
            "RegisterRunner",
            json!({
                "registrationToken": registration["token"],
                "name": "client-managed-runner",
                "labels": ["bee-local-runner-managed"],
            }),
        )
        .await
        .expect_err("reserved marker is rejected");
    assert_eq!(reserved.reason, "INVALID_ARGUMENT");
    enabled
        .call(
            "RunnerService",
            "RegisterRunner",
            json!({
                "registrationToken": registration["token"],
                "name": "bee-local-runner",
            }),
        )
        .await
        .expect("operator runner");

    let (tip, _) = enabled.tip("main").await;
    let queued = id_text(
        &enabled
            .commit(
                &tip,
                &[(".github/workflows/ci.yml", b"name: CI\non: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo queued\n")],
                4,
                "queue local runner",
            )
            .await["id"],
    );
    enabled.land(&queued).await.expect("land workflow");

    let runners = eventually(10, "managed local runner", || async {
        let runners = named_runners(&enabled).await.ok()?;
        (runners.len() == 2).then_some(runners)
    })
    .await;
    assert_eq!(managed_runner_count(&runners), 1);

    let mut options = ServerOptions::new(enabled.cfg.clone());
    options.dev = true;
    enabled.srv.shutdown().await;
    let restarted = start(options).await.expect("restart server");
    let mut client_cfg = ClientConfig::new(restarted.url.clone());
    client_cfg.token = restarted.dev.as_ref().map(|dev| dev.token.clone());
    let client = RpcClient::new(&client_cfg);
    let runners = client
        .call_rpc_json(
            "RunnerService",
            "ListRunners",
            &json!({
                "scope": { "kind": "RUNNER_SCOPE_KIND_REPO", "repo": repo() },
                "name": "bee-local-runner",
            }),
        )
        .await
        .expect("list runners after restart")["runners"]
        .as_array()
        .expect("runners array")
        .clone();
    assert_eq!(runners.len(), 2);
    assert_eq!(managed_runner_count(&runners), 1);
    restarted.shutdown().await;
}

async fn named_runners(server: &TestServer) -> Result<Vec<serde_json::Value>, Box<ErrorDetail>> {
    Ok(server
        .call(
            "RunnerService",
            "ListRunners",
            json!({
                "scope": { "kind": "RUNNER_SCOPE_KIND_REPO", "repo": repo() },
                "name": "bee-local-runner",
            }),
        )
        .await
        .map_err(Box::new)?["runners"]
        .as_array()
        .expect("runners array")
        .clone())
}

fn managed_runner_count(runners: &[serde_json::Value]) -> usize {
    runners
        .iter()
        .filter(|runner| {
            runner["labels"].as_array().is_some_and(|labels| {
                labels
                    .iter()
                    .any(|label| label["name"] == "bee-local-runner-managed")
            })
        })
        .count()
}
