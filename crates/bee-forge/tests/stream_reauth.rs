//! Open stream authorization must be re-decided after a caller loses access.

use crate::ci;
use crate::common;
use bee_proto::v1::workflow_service_server::WorkflowService;
use bee_proto::v1::{Role, TailWorkflowJobLogsRequest};
use std::time::Duration;
use tokio_stream::StreamExt;

const HELLO: &str =
    "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hello\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tail_workflow_logs_ends_when_reader_loses_access_while_idle() {
    let w = ci::world().await;
    let (_, runs) = w
        .push(
            "main",
            &[(".github/workflows/test.yml", Some(HELLO))],
            "start workflow",
        )
        .await;
    let job = w.jobs(&runs[0]).await.remove(0);
    let mut stream = w
        .svc
        .tail_workflow_job_logs(common::req(
            &w.token("reader"),
            TailWorkflowJobLogsRequest {
                repo: w.rr(),
                job_id: job.id,
                offset: 0,
                follow: true,
                step_number: 0,
            },
        ))
        .await
        .unwrap()
        .into_inner();

    let reader = w.forge.hooks.directory.get("reader").await.unwrap();
    w.forge
        .orgs
        .set_member(&w.cx("owner"), &w.org.id, reader, Role::Unspecified)
        .await
        .unwrap();

    let status = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("stream reauthorization within two seconds")
        .expect("stream error")
        .expect_err("access revocation ends the stream");
    let detail = bee_proto::errors::detail_from_status(&status).expect("typed status");
    // This is the same refusal as Subscribe: a removed member cannot resolve the private repo.
    assert_eq!(detail.reason, "NOT_FOUND");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tail_workflow_logs_does_not_deliver_chunks_after_reader_loses_access() {
    let w = ci::world().await;
    let (_, runs) = w
        .push(
            "main",
            &[(".github/workflows/test.yml", Some(HELLO))],
            "start workflow",
        )
        .await;
    let job = w.jobs(&runs[0]).await.remove(0);
    let (runner, _, _) = w.api("reauth").await;
    let lease = runner.acquire().await.unwrap().expect("queued job");
    let mut stream = w
        .svc
        .tail_workflow_job_logs(common::req(
            &w.token("reader"),
            TailWorkflowJobLogsRequest {
                repo: w.rr(),
                job_id: job.id,
                offset: 0,
                follow: true,
                step_number: 0,
            },
        ))
        .await
        .unwrap()
        .into_inner();

    let reader = w.forge.hooks.directory.get("reader").await.unwrap();
    w.forge
        .orgs
        .set_member(&w.cx("owner"), &w.org.id, reader, Role::Unspecified)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    runner
        .append_log(
            &lease.job_id,
            &lease.lease_id,
            0,
            b"private log\n".to_vec(),
            0,
        )
        .await
        .unwrap();

    let status = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("stream ends after authorization is revoked")
        .expect("stream error")
        .expect_err("the post-revocation chunk is not delivered");
    let detail = bee_proto::errors::detail_from_status(&status).expect("typed status");
    assert_eq!(detail.reason, "NOT_FOUND");
}
