//! A lead and an independent run both deliver a pull request through a running Beehive server.

use std::time::Duration;

use bee_conformance::load::client::Client;
use bee_proto::v1;
use bee_proto::v1::agent_service_client::AgentServiceClient;
use bee_proto::v1::change_service_client::ChangeServiceClient;
use bee_proto::v1::event_service_client::EventServiceClient;
use bee_proto::v1::issue_service_client::IssueServiceClient;
use bee_proto::v1::repo_work_item::Work;
use bee_proto::v1::swarm_service_client::SwarmServiceClient;

use crate::scale_support::start_fresh;

pub(super) async fn register_agent(c: &Client, name: &str) -> String {
    let agent = c.create_agent(name).await.expect("create agent");
    c.add_member("dev", &agent).await.expect("add agent to org");
    c.add_writer(&agent).await.expect("allow agent to write");
    AgentServiceClient::new(c.channel().clone())
        .register_agent(c.req(v1::RegisterAgentRequest {
            org: "dev".into(),
            agent_principal_id: agent.clone(),
            name: name.into(),
            runtime: "claude-code".into(),
            toolsets: vec!["read".into(), "commit".into()],
            ..Default::default()
        }))
        .await
        .expect("register agent");
    agent
}

pub(super) async fn issue_run(c: &Client, name: &str) -> (v1::Issue, v1::AgentRun) {
    let agent = register_agent(c, name).await;
    let issue = IssueServiceClient::new(c.channel().clone())
        .create_issue(c.req(v1::CreateIssueRequest {
            repo: c.repo(),
            title: format!("Update the {name} example"),
            ..Default::default()
        }))
        .await
        .expect("open issue")
        .into_inner();
    let run = AgentServiceClient::new(c.channel().clone())
        .start_agent_run(c.req(v1::StartAgentRunRequest {
            repo: c.repo(),
            agent_principal_id: agent,
            base_ref: "main".into(),
            task: Some(v1::AgentTask {
                kind: v1::AgentTaskKind::Issue as i32,
                issue: issue.number.to_string(),
                ..Default::default()
            }),
            budget: Some(v1::AgentRunBudget {
                max_usd_micros: 1_000_000,
                ..Default::default()
            }),
            ..Default::default()
        }))
        .await
        .expect("start run")
        .into_inner();
    (issue, run)
}

pub(super) async fn wait_for_state(c: &Client, run: &str, expected: v1::AgentRunState) {
    let mut events = AgentServiceClient::new(c.channel().clone())
        .stream_agent_run_events(c.req(v1::StreamAgentRunEventsRequest {
            run_id: run.into(),
            kinds: vec![v1::AgentRunEventKind::StateChanged as i32],
            ..Default::default()
        }))
        .await
        .expect("open session event stream")
        .into_inner();
    while let Some(event) = events.message().await.expect("session event") {
        if event.state() == expected {
            return;
        }
    }
    panic!("run {run} ended without entering {expected:?}");
}

async fn land_outcome(c: &Client, run: &v1::AgentRun) -> String {
    wait_for_state(c, &run.id, v1::AgentRunState::WaitingForReview).await;
    let agent = &run.agent.as_ref().expect("run agent").id;
    let mut changes = ChangeServiceClient::new(c.channel().clone());
    let pulls = changes
        .list_changes(c.req(v1::ListChangesRequest {
            repo: c.repo(),
            author_id: agent.clone(),
            ..Default::default()
        }))
        .await
        .expect("list agent's pull requests")
        .into_inner()
        .changes;
    assert_eq!(pulls.len(), 1);
    let id = pulls[0].id.clone();
    // WaitingForReview is emitted before the runner links its session to the pull request.
    // Landing is observed through that link, so wait for it before making the transition.
    let mut agents = AgentServiceClient::new(c.channel().clone());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sessions = agents
                .get_sessions_for_change(c.req(v1::GetSessionsForChangeRequest {
                    repo: c.repo(),
                    change_request: id.clone(),
                    ..Default::default()
                }))
                .await
                .expect("get sessions for pull request")
                .into_inner()
                .sessions;
            if sessions.iter().any(|session| session.run_id == run.id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("pull request links to the run");
    // The fake runner holds an exclusive lease while its pull request waits for review.
    // Release it as the holder so the human can merge the resulting change.
    let completed_work = agents
        .get_agent_run(c.req(v1::GetAgentRunRequest { id: run.id.clone() }))
        .await
        .expect("read the run's leases")
        .into_inner();
    assert!(!completed_work.lease_ids.is_empty());
    let token = c
        .mint_token(agent, &[], 0, 0)
        .await
        .expect("mint a token for the lease holder");
    let holder = c.with_token(token);
    for lease in completed_work.lease_ids {
        holder.release_lease(&lease).await.expect("release lease");
    }
    c.land_change(&id).await.expect("merge outcome");
    wait_for_state(c, &run.id, v1::AgentRunState::Succeeded).await;
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swarm_and_independent_run_record_their_pull_requests() {
    let (s, _dir) = start_fresh(|cfg| cfg.forge.agent_runner = "fake".into(), None).await;
    let c = s.client.with_timeout(Duration::from_secs(30));
    let mut swarms = SwarmServiceClient::new(c.channel().clone());
    let (issue, lead) = issue_run(&c, "docs-lead").await;
    wait_for_state(&c, &lead.id, v1::AgentRunState::WaitingForReview).await;
    let mut stream = EventServiceClient::new(c.channel().clone())
        .subscribe(c.req(v1::SubscribeRequest {
            repo: c.repo(),
            kinds: vec!["swarm.*".into()],
            ..Default::default()
        }))
        .await
        .expect("subscribe to swarm events")
        .into_inner();
    let swarm = swarms
        .create_swarm(c.req(v1::CreateSwarmRequest {
            repo: c.repo(),
            name: "Refresh the examples".into(),
            goal: "Make the examples match the current commands.".into(),
            issue: issue.number.to_string(),
            branch: "main".into(),
            lead_run_id: lead.id.clone(),
        }))
        .await
        .expect("create swarm")
        .into_inner();
    assert_eq!(swarm.state(), v1::SwarmState::Planning);
    let created = stream.message().await.expect("first event").expect("event");
    assert_eq!(created.subject.expect("swarm event subject").id, swarm.id);
    assert_eq!(created.kind, "swarm.created");
    drop(stream);
    assert_eq!(swarm.owner.expect("person accountable").id, s.user());
    let reference = Some(v1::SwarmRef {
        repo: c.repo(),
        id: swarm.id.clone(),
    });
    swarms
        .set_swarm_state(c.req(v1::SetSwarmStateRequest {
            swarm: reference.clone(),
            state: v1::SwarmState::Working as i32,
        }))
        .await
        .expect("start the swarm");
    let task = swarms
        .add_swarm_task(c.req(v1::AddSwarmTaskRequest {
            swarm: reference.clone(),
            title: "Check the installation commands".into(),
            paths: vec![b"docs/install.md".to_vec()],
        }))
        .await
        .expect("add task")
        .into_inner();
    swarms
        .start_swarm_task(c.req(v1::StartSwarmTaskRequest {
            swarm: reference.clone(),
            task_id: task.id.clone(),
            run_id: lead.id.clone(),
        }))
        .await
        .expect("start attempt");
    let worker = register_agent(&c, "docs-worker").await;
    let request = swarms
        .request_swarm_join(c.req(v1::RequestSwarmJoinRequest {
            swarm: reference.clone(),
            agent_principal_id: worker,
            budget: Some(v1::AgentRunBudget {
                max_usd_micros: 1_000_000,
                ..Default::default()
            }),
        }))
        .await
        .expect("offer another agent")
        .into_inner();
    let join = swarms
        .decide_swarm_join(c.req(v1::DecideSwarmJoinRequest {
            swarm: reference.clone(),
            join_id: request.id,
            accept: true,
            reason: "Check the examples alongside the lead.".into(),
        }))
        .await
        .expect("accept the worker")
        .into_inner();
    assert_eq!(join.state(), v1::SwarmJoinState::Accepted);
    let message = swarms
        .send_swarm_message(c.req(v1::SendSwarmMessageRequest {
            swarm: reference.clone(),
            body: "Please confirm the docs commands.".into(),
        }))
        .await
        .expect("send message to lead")
        .into_inner();
    assert_eq!(message.run_id, lead.id);
    assert_eq!(message.kind(), v1::AgentRunEventKind::SteeringMessage);
    wait_for_state(&c, &join.run_id, v1::AgentRunState::Succeeded).await;
    let worker_messages = AgentServiceClient::new(c.channel().clone())
        .list_agent_run_events(c.req(v1::ListAgentRunEventsRequest {
            run_id: join.run_id,
            kinds: vec![v1::AgentRunEventKind::SteeringMessage as i32],
            ..Default::default()
        }))
        .await
        .expect("read worker session")
        .into_inner()
        .events;
    assert!(worker_messages.is_empty());
    let finished = swarms
        .finish_swarm_task(c.req(v1::FinishSwarmTaskRequest {
            swarm: reference.clone(),
            task_id: task.id,
            run_id: lead.id.clone(),
            outcome: v1::SwarmAttemptOutcome::Finished as i32,
            reason: "The commands match the docs.".into(),
            hand_back: Some(v1::SwarmHandBack {
                done: vec!["Verified the installation commands.".into()],
                ..Default::default()
            }),
            attempt_number: 1,
        }))
        .await
        .expect("finish attempt")
        .into_inner();
    assert_eq!(finished.state(), v1::SwarmTaskState::Finished);

    let swarm_pull = land_outcome(&c, &lead).await;
    let completed = swarms
        .set_swarm_state(c.req(v1::SetSwarmStateRequest {
            swarm: reference,
            state: v1::SwarmState::Finished as i32,
        }))
        .await
        .expect("finish swarm")
        .into_inner();
    assert_eq!(
        completed.change_request_ids.as_slice(),
        std::slice::from_ref(&swarm_pull)
    );

    let (_, independent) = issue_run(&c, "independent-docs").await;
    let independent_pull = land_outcome(&c, &independent).await;
    let rows = swarms
        .list_repo_work(c.req(v1::ListRepoWorkRequest {
            repo: c.repo(),
            ..Default::default()
        }))
        .await
        .expect("list repo work")
        .into_inner()
        .items;
    assert_eq!(rows.len(), 2, "swarm members are not standalone work");
    assert_eq!(rows[0].change_request_ids, [independent_pull]);
    assert!(matches!(&rows[0].work, Some(Work::Run(r)) if r.id == independent.id));
    assert_eq!(rows[1].change_request_ids, [swarm_pull]);
    assert!(matches!(&rows[1].work, Some(Work::Swarm(s)) if s.id == swarm.id));

    s.srv.shutdown().await;
}
