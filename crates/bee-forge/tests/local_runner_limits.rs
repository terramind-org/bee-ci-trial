//! Safety limits for the in-process workflow runner.

use std::time::{Duration, Instant};

use bee_forge::ci::runner::{LocalRunner, RunnerConfig};
use bee_proto::v1::WorkflowConclusion;

use crate::ci::world;
use bee_proto::v1::workflow_service_server::WorkflowService;

async fn run(
    workflow: &str,
    config: RunnerConfig,
) -> (
    crate::ci::World,
    Vec<String>,
    Vec<bee_forge::ci::runner::Ran>,
) {
    let w = world().await;
    let (_, runs) = w
        .push(
            "main",
            &[(".github/workflows/limits.yml", Some(workflow))],
            "limits",
        )
        .await;
    let runner = w.local_with("limits", config).await;
    let ran = w.drain(&runner).await;
    (w, runs, ran)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refuses_container_requests_before_steps() {
    let w = world().await;
    let marker = w.work.path().join("container-step-ran");
    let workflow = format!(
        "on: push\njobs:\n  container:\n    runs-on: ubuntu-latest\n    container: alpine:3\n    steps:\n      - run: echo should-not-run\n  service:\n    runs-on: ubuntu-latest\n    services:\n      db:\n        image: postgres:17\n    steps:\n      - run: echo should-not-run\n  docker_action:\n    runs-on: ubuntu-latest\n    steps:\n      - run: touch {}\n      - uses: docker://alpine:3\n",
        marker.display()
    );
    let (_, runs) = w
        .push(
            "main",
            &[(".github/workflows/limits.yml", Some(workflow.as_str()))],
            "limits",
        )
        .await;
    let runner = w.local_with("limits", RunnerConfig::default()).await;
    let ran = w.drain(&runner).await;
    assert_eq!(ran.len(), 3);
    for job in w.jobs(&runs[0]).await {
        assert_eq!(job.conclusion, WorkflowConclusion::Failure as i32);
        let log = w.log(&job.id).await;
        assert!(log.contains("this runner cannot run containers:"), "{log}");
        assert!(!log.contains("should-not-run"), "{log}");
    }
    assert!(!marker.exists(), "a step ran before docker:// was refused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refuses_nested_local_docker_actions_before_steps() {
    let w = world().await;
    let marker = w.work.path().join("nested-container-step-ran");
    let workflow = format!(
        "on: push\njobs:\n  nested:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: touch {}\n      - uses: ./.github/actions/wrapper\n",
        marker.display()
    );
    let wrapper =
        "name: wrapper\nruns:\n  using: composite\n  steps:\n    - uses: docker://alpine:3\n";
    let (_, runs) = w
        .push(
            "main",
            &[
                (".github/actions/wrapper/action.yml", Some(wrapper)),
                (".github/workflows/limits.yml", Some(workflow.as_str())),
            ],
            "nested docker",
        )
        .await;
    let runner = w.local_with("limits", RunnerConfig::default()).await;
    w.drain(&runner).await;
    let job = w.jobs(&runs[0]).await.pop().unwrap();
    let log = w.log(&job.id).await;
    assert_eq!(job.conclusion, WorkflowConclusion::Failure as i32);
    assert!(
        log.contains(
            "this runner cannot run containers: docker://alpine:3 in ./.github/actions/wrapper"
        ),
        "{log}"
    );
    assert!(
        !marker.exists(),
        "a step ran before nested docker:// was refused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refuses_local_docker_actions_before_steps() {
    let w = world().await;
    let marker = w.work.path().join("local-docker-step-ran");
    let workflow = format!(
        "on: push\njobs:\n  local_docker:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: touch {}\n      - uses: ./.github/actions/docker\n",
        marker.display()
    );
    let action = "name: docker\nruns:\n  using: docker\n  image: Dockerfile\n";
    let (_, runs) = w
        .push(
            "main",
            &[
                (".github/actions/docker/action.yml", Some(action)),
                (".github/workflows/limits.yml", Some(workflow.as_str())),
            ],
            "local docker",
        )
        .await;
    let runner = w.local_with("limits", RunnerConfig::default()).await;
    w.drain(&runner).await;
    let job = w.jobs(&runs[0]).await.pop().unwrap();
    let log = w.log(&job.id).await;
    assert_eq!(job.conclusion, WorkflowConclusion::Failure as i32);
    assert!(
        log.contains("this runner cannot run containers: Docker action ./.github/actions/docker"),
        "{log}"
    );
    assert!(
        !marker.exists(),
        "a step ran before local Docker action was refused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refuses_cycles_in_local_composite_actions_before_steps() {
    let w = world().await;
    let marker = w.work.path().join("local-cycle-step-ran");
    let workflow = format!(
        "on: push\njobs:\n  cycle:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: touch {}\n      - uses: ./.github/actions/cycle\n",
        marker.display()
    );
    let action =
        "name: cycle\nruns:\n  using: composite\n  steps:\n    - uses: ./.github/actions/cycle\n";
    let (_, runs) = w
        .push(
            "main",
            &[
                (".github/actions/cycle/action.yml", Some(action)),
                (".github/workflows/limits.yml", Some(workflow.as_str())),
            ],
            "local cycle",
        )
        .await;
    let runner = w.local_with("limits", RunnerConfig::default()).await;
    w.drain(&runner).await;
    let job = w.jobs(&runs[0]).await.pop().unwrap();
    let log = w.log(&job.id).await;
    assert_eq!(job.conclusion, WorkflowConclusion::Failure as i32);
    assert!(log.contains("local action cycle"), "{log}");
    assert!(
        !marker.exists(),
        "a step ran before a local action cycle was refused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_composite_run_steps_still_work_after_preflight() {
    let workflow = "on: push\njobs:\n  local:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - uses: ./.github/actions/hello\n";
    let action = "name: hello\nruns:\n  using: composite\n  steps:\n    - shell: bash\n      run: echo local composite ran\n";
    let w = world().await;
    let (_, runs) = w
        .push(
            "main",
            &[
                (".github/actions/hello/action.yml", Some(action)),
                (".github/workflows/limits.yml", Some(workflow)),
            ],
            "local composite",
        )
        .await;
    let runner = w.local_with("limits", RunnerConfig::default()).await;
    w.drain(&runner).await;
    let job = w.jobs(&runs[0]).await.pop().unwrap();
    assert_eq!(job.conclusion, WorkflowConclusion::Success as i32);
    assert!(w.log(&job.id).await.contains("local composite ran"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steps_start_with_a_scrubbed_environment() {
    let workflow = "on: push\njobs:\n  env:\n    runs-on: ubuntu-latest\n    env:\n      OWN: present\n    steps:\n      - run: test -d \"$HOME\" && env\n";
    let (w, runs, _) = run(workflow, RunnerConfig::default()).await;
    let job = w.jobs(&runs[0]).await.pop().unwrap();
    let log = w.log(&job.id).await;
    assert!(log.contains("OWN=present"), "{log}");
    assert!(log.contains("BEE_ACTIONS=true"), "{log}");
    assert!(!log.contains("CARGO_PKG_NAME="), "{log}");
    assert!(!log.contains("CARGO_MANIFEST_DIR="), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_timeout_minutes_below_runner_cap_is_honoured() {
    let workflow = "on: push\njobs:\n  slow:\n    runs-on: ubuntu-latest\n    timeout-minutes: 0.016\n    steps:\n      - run: sleep 30\n";
    let config = RunnerConfig {
        max_job_timeout: Duration::from_secs(4),
        ..Default::default()
    };
    let start = Instant::now();
    let (w, runs, ran) = run(workflow, config).await;
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(ran[0].conclusion, Some(WorkflowConclusion::TimedOut));
    let job = w.jobs(&runs[0]).await.pop().unwrap();
    assert_eq!(job.conclusion, WorkflowConclusion::TimedOut as i32);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_cap_times_out_job_and_removes_workspace() {
    let workflow =
        "on: push\njobs:\n  slow:\n    runs-on: ubuntu-latest\n    steps:\n      - run: sleep 30\n";
    let config = RunnerConfig {
        max_job_timeout: Duration::from_secs(1),
        ..Default::default()
    };
    let start = Instant::now();
    let (w, runs, ran) = run(workflow, config).await;
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(ran[0].conclusion, Some(WorkflowConclusion::TimedOut));
    let job = w.jobs(&runs[0]).await.pop().unwrap();
    assert_eq!(job.conclusion, WorkflowConclusion::TimedOut as i32);
    assert!(std::fs::read_dir(w.work.path()).unwrap().next().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn javascript_actions_obey_the_job_timeout() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping javascript_actions_obey_the_job_timeout: `node` is not on PATH");
        return;
    }
    let workflow = "on: push\njobs:\n  slow:\n    runs-on: ubuntu-latest\n    timeout-minutes: 0.033\n    steps:\n      - uses: actions/checkout@v4\n      - uses: ./slow\n";
    let action = "name: slow\nruns:\n  using: node20\n  main: main.js\n  post: post.js\n";
    let main = "console.log('node-pid=' + process.pid); while (true) {}\n";
    let post = "console.log('post ran after timeout');\n";
    let w = world().await;
    let (_, runs) = w
        .push(
            "main",
            &[
                (".github/workflows/limits.yml", Some(workflow)),
                ("slow/action.yml", Some(action)),
                ("slow/main.js", Some(main)),
                ("slow/post.js", Some(post)),
            ],
            "slow js action",
        )
        .await;
    let config = RunnerConfig {
        max_job_timeout: Duration::from_secs(2),
        ..Default::default()
    };
    let ran = tokio::time::timeout(Duration::from_secs(30), async {
        let runner = w.local_with("limits", config).await;
        w.drain(&runner).await
    })
    .await
    .expect("JavaScript action exceeded the test's 30 second guard");
    assert_eq!(ran[0].conclusion, Some(WorkflowConclusion::TimedOut));
    let job = w.jobs(&runs[0]).await.pop().unwrap();
    assert_eq!(job.conclusion, WorkflowConclusion::TimedOut as i32);
    let log = w.log(&job.id).await;
    assert!(log.contains("The action has timed out."), "{log}");
    assert!(
        log.contains("Post ./slow"),
        "post processing should still be attempted: {log}"
    );
    let pid = log
        .lines()
        .find_map(|line| line.split_once("node-pid=").map(|(_, pid)| pid.trim()))
        .expect("the JavaScript action should report its pid");
    let status = std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .expect("kill command should be available");
    assert!(
        !status.success(),
        "timed-out node process {pid} is still alive"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn artifact_upload_rejects_symlinks_outside_the_workspace() {
    let w = world().await;
    let outside = w.work.path().join("outside-secret.txt");
    let secret = "runner-outside-workspace-secret-6f3d";
    std::fs::write(&outside, secret).unwrap();
    let workflow = format!(
        "on: push\njobs:\n  leak:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: ln -s '{}' leak\n      - uses: actions/upload-artifact@v4\n        with:\n          name: leak\n          path: leak\n",
        outside.display()
    );
    let (_, runs) = w
        .push(
            "main",
            &[(".github/workflows/limits.yml", Some(workflow.as_str()))],
            "symlink artifact",
        )
        .await;
    let runner = w.local_with("limits", RunnerConfig::default()).await;
    w.drain(&runner).await;
    let job = w.jobs(&runs[0]).await.pop().unwrap();
    let log = w.log(&job.id).await;
    assert_eq!(job.conclusion, WorkflowConclusion::Failure as i32, "{log}");
    assert!(log.contains("outside the workspace"), "{log}");

    let artifacts = w
        .svc
        .list_workflow_artifacts(crate::common::req(
            &w.token("reader"),
            bee_proto::v1::ListWorkflowArtifactsRequest {
                repo: w.rr(),
                run_id: runs[0].clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .artifacts;
    assert!(
        artifacts.is_empty(),
        "an artifact was stored for the rejected path"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspaces_are_removed_after_success_and_failure() {
    let workflow = "on: push\njobs:\n  success:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n  failure:\n    runs-on: ubuntu-latest\n    steps:\n      - run: false\n";
    let (w, _, _) = run(workflow, RunnerConfig::default()).await;
    assert!(std::fs::read_dir(w.work.path()).unwrap().next().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_actions_remain_refused() {
    let workflow = "on: push\njobs:\n  action:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: example/action@v1\n";
    let (w, runs, ran) = run(workflow, RunnerConfig::default()).await;
    assert_eq!(ran[0].conclusion, Some(WorkflowConclusion::Failure));
    let job = w.jobs(&runs[0]).await.pop().unwrap();
    assert!(w.log(&job.id).await.contains("not supported"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn working_directories_stay_inside_the_workspace() {
    let workflow = "on: push\njobs:\n  absolute:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n        working-directory: /tmp\n  parent:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n        working-directory: ../outside\n  symlink:\n    runs-on: ubuntu-latest\n    steps:\n      - run: ln -s /tmp outside\n      - run: true\n        working-directory: outside\n  inside:\n    runs-on: ubuntu-latest\n    steps:\n      - run: mkdir subdir\n      - run: pwd\n        working-directory: subdir\n";
    let (w, runs, ran) = run(workflow, RunnerConfig::default()).await;
    assert_eq!(ran.len(), 4);
    for job in w.jobs(&runs[0]).await {
        let log = w.log(&job.id).await;
        if log.contains("workspace/subdir") {
            assert_eq!(job.conclusion, WorkflowConclusion::Success as i32, "{log}");
        } else {
            assert_eq!(job.conclusion, WorkflowConclusion::Failure as i32, "{log}");
            assert!(
                log.contains("working-directory must stay inside the workspace:"),
                "{log}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborting_a_job_removes_its_workspace() {
    let workflow =
        "on: push\njobs:\n  slow:\n    runs-on: ubuntu-latest\n    steps:\n      - run: sleep 30\n";
    let w = world().await;
    let (_, runs) = w
        .push(
            "main",
            &[(".github/workflows/limits.yml", Some(workflow))],
            "limits",
        )
        .await;
    let job = w.jobs(&runs[0]).await.pop().unwrap();
    let root = w.work.path().join(&job.id);
    let (api, _, _) = w.api("limits").await;
    let runner = LocalRunner::new(
        api,
        RunnerConfig {
            work_root: w.work.path().to_path_buf(),
            name: "limits".into(),
            heartbeat: Duration::from_millis(200),
            ..Default::default()
        },
    );
    let task = tokio::spawn(async move { runner.run_once().await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !root.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("job workspace was created");
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while root.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("aborting the job removed its workspace");
}
