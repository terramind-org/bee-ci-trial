//! The organization stop applies at native and pull-request publication and holds the queue.

use std::time::Duration;

use bee_conformance::load::client::{Client, RpcError};
use bee_core::{ChangeId, ObjectId};
use bee_forge::Cx;
use bee_forge::changes::QueueStep;
use bee_proto::errors::detail_from_status;
use bee_proto::v1::{
    self, change_service_client::ChangeServiceClient, event_service_client::EventServiceClient,
    repo_service_client::RepoServiceClient, safety_service_client::SafetyServiceClient,
};

use crate::scale_support::{main_tip, start_fresh};

async fn agent_writer(owner: &Client) -> Client {
    let id = owner
        .create_agent("merge worker")
        .await
        .expect("create agent");
    owner
        .add_member("dev", &id)
        .await
        .expect("add to organization");
    owner
        .add_writer(&id)
        .await
        .expect("grant repository write access");
    let token = owner
        .mint_token(&id, &[], 0, 0)
        .await
        .expect("mint agent credential");
    owner.with_token(token)
}

async fn commit(client: &Client, base: &ObjectId, file: &str, seed: u8) -> ObjectId {
    client
        .commit(
            base,
            &[(
                file.to_string(),
                b"Updated instructions for the example\n".to_vec(),
            )],
            ChangeId::new([seed; 16]),
            "Update example documentation",
        )
        .await
        .expect("create changeset")
}

fn stop_request(owner: &Client, reason: &str) -> tonic::Request<v1::SetSafetyStopRequest> {
    owner.req(v1::SetSafetyStopRequest {
        org: "dev".into(),
        reason: reason.into(),
    })
}

fn assert_frozen(error: &RpcError) {
    let detail = error.detail.as_ref().expect("structured denial");
    assert_eq!(detail.reason, "POLICY_DENIED");
    assert_eq!(detail.subcode, "safety_merge_frozen");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_stop_blocks_agent_native_and_pull_request_lands_until_resume() {
    let (server, _dir) = start_fresh(|_| {}, None).await;
    let owner = server.client.with_timeout(Duration::from_secs(30));
    let agent = agent_writer(&owner).await;
    let (base, version) = main_tip(&owner).await;
    let native = commit(&agent, &base, "docs/native.md", 21).await;
    let pull_head = commit(&agent, &base, "docs/pull-request.md", 22).await;
    let pull = agent
        .open_change("main", &pull_head, "Update pull request instructions")
        .await
        .expect("open pull request");
    let mut safety = SafetyServiceClient::new(owner.channel().clone());

    let denied = safety
        .stop_everything(stop_request(
            &agent,
            "The agent cannot stop the organization.",
        ))
        .await
        .expect_err("only people who own an organization can stop it");
    assert_eq!(
        detail_from_status(&denied)
            .expect("structured denial")
            .reason,
        "PERMISSION_DENIED"
    );

    let stop = safety
        .stop_everything(stop_request(&owner, "Pause to check a release."))
        .await
        .expect("freeze merges")
        .into_inner();
    assert!(stop.frozen);
    assert_frozen(
        &agent
            .land("main", &native)
            .await
            .expect_err("native agent land refused"),
    );
    assert_frozen(
        &agent
            .land_change(&pull)
            .await
            .expect_err("agent pull request refused"),
    );
    assert_eq!(main_tip(&owner).await, (base, version));

    let human_head = commit(&owner, &base, "docs/release.md", 23).await;
    let human_pull = owner
        .open_change("main", &human_head, "Update release instructions")
        .await
        .expect("open human pull request");
    owner
        .land_change(&human_pull)
        .await
        .expect("human can still merge");
    assert_eq!(main_tip(&owner).await.1, version + 1);

    let resume = safety
        .resume_everything(stop_request(&owner, "Release check finished."))
        .await
        .expect("unfreeze merges")
        .into_inner();
    assert!(!resume.frozen);
    agent
        .land_change(&pull)
        .await
        .expect("agent pull request can merge again");
    agent
        .land("main", &native)
        .await
        .expect("agent native land can publish again");
    assert_eq!(main_tip(&owner).await.1, version + 3);
    server.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_resume_wakes_a_held_queue_on_a_nondefault_ref() {
    let (server, _dir) = start_fresh(|_| {}, None).await;
    let owner = server.client.with_timeout(Duration::from_secs(30));
    let agent = agent_writer(&owner).await;
    let (base, _) = main_tip(&owner).await;
    owner
        .create_ref("release", &base)
        .await
        .expect("create release ref");
    RepoServiceClient::new(owner.channel().clone())
        .update_repo_settings(owner.req(v1::UpdateRepoSettingsRequest {
            repo: owner.repo(),
            merge_queue_required: Some(true),
            ..Default::default()
        }))
        .await
        .expect("require queue");
    let mut safety = SafetyServiceClient::new(owner.channel().clone());
    safety
        .stop_everything(stop_request(&owner, "Pause to check the release."))
        .await
        .expect("freeze merges");

    let head = commit(&agent, &base, "docs/release-queue.md", 24).await;
    let pull = agent
        .open_change("release", &head, "Update the release queue example")
        .await
        .expect("open queued pull request");
    let mut changes = ChangeServiceClient::new(owner.channel().clone());
    changes
        .enqueue_change(agent.req(v1::EnqueueChangeRequest {
            repo: owner.repo(),
            id: pull.clone(),
            ..Default::default()
        }))
        .await
        .expect("enqueue pull request as the agent");

    let forge = server.srv.forge.as_ref().expect("forge");
    let cx = Cx::system();
    let repo = forge
        .forge
        .orgs
        .repo_any(&cx, &server.repo().0.to_text())
        .await
        .expect("repository row");
    assert_eq!(
        forge
            .changes
            .step_queue(&cx, &repo, "release")
            .await
            .expect("step while frozen"),
        QueueStep::Waiting
    );
    let mut events = EventServiceClient::new(owner.channel().clone())
        .subscribe(owner.req(v1::SubscribeRequest {
            repo: owner.repo(),
            kinds: vec!["cr.landed".into()],
            ..Default::default()
        }))
        .await
        .expect("subscribe to merges")
        .into_inner();
    safety
        .resume_everything(stop_request(&owner, "Release check complete."))
        .await
        .expect("resume merges");

    let event = tokio::time::timeout(Duration::from_secs(10), events.message())
        .await
        .expect("resumption wakes the release ref")
        .expect("read merge event")
        .expect("release pull request was merged");
    assert_eq!(event.subject.expect("pull request subject").id, pull);
    assert!(
        changes
            .get_merge_queue(owner.req(v1::GetMergeQueueRequest {
                repo: owner.repo(),
                r#ref: "release".into(),
            }))
            .await
            .expect("read the queue")
            .into_inner()
            .entries
            .is_empty()
    );
    drop(events);
    server.srv.shutdown().await;
}
