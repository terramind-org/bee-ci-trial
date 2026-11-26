//! Cancelling through another replica stops the command that owns the process.

mod common;

use bee_mcp::{ClientConfig, RpcClient};
use bee_proto::v1;
use bee_server::server::{ServerOptions, start};
use common::*;
use serde_json::{Value, json};
use std::time::Duration;

fn rpc_client(url: &str, token: &str) -> RpcClient {
    let mut config = ClientConfig::new(url.to_string());
    config.token = Some(token.to_string());
    config.request_timeout = Duration::from_secs(30);
    RpcClient::new(&config)
}

fn process_is_running(pid: u32) -> bool {
    let stat_path = format!("/proc/{pid}/stat");
    let Ok(stat) = std::fs::read_to_string(stat_path) else {
        let pid = pid.to_string();
        return std::process::Command::new("/bin/kill")
            .args(["-0", pid.as_str()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
    };
    let Some((_, fields)) = stat.rsplit_once(") ") else {
        return false;
    };
    fields.split_whitespace().next() != Some("Z")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_through_another_replica_stops_the_running_command() {
    let command =
        r#"sleep 60 & child=$!; printf '%s\n' "$child" > child.pid; wait "$child"; touch finished"#;
    let runner = start_with(|cfg| {
        cfg.forge.agent_runner = "command".into();
        cfg.forge.agent_runner_command = command.into();
    })
    .await;
    let dev = runner.dev_user();
    let agent = runner
        .call(
            "AuthService",
            "CreateAgent",
            json!({ "display": "cancel-agent", "runtime": "shell", "org": "dev" }),
        )
        .await
        .expect("create agent");
    let agent_id = agent["id"].as_str().expect("agent id").to_string();
    runner
        .call(
            "OrgService",
            "SetMember",
            json!({ "org": "dev", "principalId": agent_id, "role": "ROLE_MEMBER" }),
        )
        .await
        .expect("add agent to org");
    let org_id = runner
        .srv
        .dev
        .as_ref()
        .expect("dev bootstrap")
        .org
        .0
        .to_text();
    runner
        .srv
        .forge
        .as_ref()
        .expect("forge")
        .agents
        .define_agent(
            &bee_forge::Cx::new(dev.clone(), v1::PrincipalKind::Human),
            bee_forge::agents::AgentDefinition {
                org_id,
                agent: v1::Principal {
                    id: agent_id.clone(),
                    kind: v1::PrincipalKind::Agent as i32,
                    display: "cancel-agent".into(),
                    ..Default::default()
                },
                name: "cancel-agent".into(),
                runtime: "shell".into(),
                toolsets: ["read", "commit"].into_iter().map(str::to_string).collect(),
                owners: vec![dev],
                purpose: "cancellation regression".into(),
                enabled: true,
                ..Default::default()
            },
        )
        .await
        .expect("define agent");

    let run = runner
        .call(
            "AgentService",
            "StartAgentRun",
            json!({
                "repo": repo(),
                "agentPrincipalId": agent_id,
                "task": { "kind": "AGENT_TASK_KIND_FREEFORM", "prompt": "wait" }
            }),
        )
        .await
        .expect("start run");
    let run_id = run["id"].as_str().expect("run id").to_string();
    let run_dir = runner.cfg.server.data_dir.join("agent-runs").join(&run_id);
    let pid_file = run_dir.join("child.pid");
    let finished_file = run_dir.join("finished");
    let child_pid = eventually(20, "command child to start", || async {
        std::fs::read_to_string(&pid_file)
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()
    })
    .await;
    assert!(process_is_running(child_pid), "sleep process did not start");

    let mut replica_options = ServerOptions::new(runner.cfg.clone());
    replica_options.dev = true;
    let replica_b = start(replica_options).await.expect("start replica B");
    let client_b = rpc_client(
        &replica_b.url,
        &replica_b
            .dev
            .as_ref()
            .expect("replica B dev bootstrap")
            .token,
    );
    let cancelled: Value = client_b
        .call_rpc_json(
            "AgentService",
            "CancelAgentRun",
            &json!({ "id": run_id, "reason": "replica cancellation test" }),
        )
        .await
        .expect("cancel through replica B");
    assert_eq!(
        cancelled["state"], "AGENT_RUN_STATE_CANCELLED",
        "{cancelled}"
    );

    eventually(5, "command child to stop", || async {
        (!process_is_running(child_pid)).then_some(())
    })
    .await;
    assert!(
        !finished_file.exists(),
        "command continued through its sleep"
    );
    let final_run = eventually(5, "cancelled run status", || async {
        let run = client_b
            .call_rpc_json("AgentService", "GetAgentRun", &json!({ "id": run_id }))
            .await
            .ok()?;
        (run["state"] == "AGENT_RUN_STATE_CANCELLED").then_some(run)
    })
    .await;
    assert_eq!(
        final_run["state"], "AGENT_RUN_STATE_CANCELLED",
        "{final_run}"
    );

    replica_b.shutdown().await;
    runner.srv.shutdown().await;
}
