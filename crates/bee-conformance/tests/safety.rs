//! Use the running server to sample an agent merge and independently review its work.

use std::time::Duration;

use bee_forge::ids::ts;
use bee_proto::v1::agent_service_client::AgentServiceClient;
use bee_proto::v1::change_service_client::ChangeServiceClient;
use bee_proto::v1::safety_service_client::SafetyServiceClient;
use bee_proto::v1::*;

use crate::scale_support::start_fresh;
use crate::swarms::{issue_run, register_agent, wait_for_state};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_samples_agent_merge_and_accepts_independent_review() {
    let (s, _dir) = start_fresh(|cfg| cfg.forge.agent_runner = "fake".into(), None).await;
    let c = s.client.with_timeout(Duration::from_secs(30));
    let (issue, run) = issue_run(&c, "safety-writer").await;
    wait_for_state(&c, &run.id, AgentRunState::WaitingForReview).await;
    let agent = &run.agent.as_ref().unwrap().id;
    let author = c.with_token(c.mint_token(agent, &[], 0, 0).await.unwrap());
    let mut agent_api = AgentServiceClient::new(c.channel().clone());
    let run = agent_api
        .get_agent_run(c.req(GetAgentRunRequest { id: run.id }))
        .await
        .unwrap()
        .into_inner();
    for id in &run.lease_ids {
        author.release_lease(id).await.unwrap();
    }
    let changes = ChangeServiceClient::new(c.channel().clone())
        .list_changes(c.req(ListChangesRequest {
            repo: c.repo(),
            author_id: agent.clone(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner()
        .changes;
    assert_eq!(changes.len(), 1);
    let change = &changes[0];
    author
        .land_change(&change.id)
        .await
        .expect("Merge using the agent principal");
    wait_for_state(&c, &run.id, AgentRunState::Succeeded).await;

    let mut safety = SafetyServiceClient::new(c.channel().clone());
    let request = SpotCheckRequest {
        repo: c.repo(),
        period: Some(SafetyPeriod {
            start: Some(ts(0)),
            end: Some(ts(4_102_444_800_000)),
        }),
        seed: "spot check on library updates".into(),
        limit: 1,
    };
    let sampled = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let records = safety
                .spot_check(c.req(request.clone()))
                .await
                .unwrap()
                .into_inner()
                .records;
            if !records.is_empty() {
                break records;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the audit relay exposes the committed merge");
    assert_eq!(sampled.len(), 1);
    assert_eq!(sampled[0].work.as_ref().unwrap().id, run.id);
    assert_eq!(
        sampled[0].done.as_ref().unwrap().merged_change_request_ids,
        vec![change.id.clone()]
    );
    assert!(
        sampled[0]
            .asked
            .iter()
            .any(|ask| ask.text.contains(&issue.title))
    );
    let again = safety
        .spot_check(c.req(request))
        .await
        .unwrap()
        .into_inner()
        .records;
    assert_eq!(sampled, again);

    let reviewer_id = register_agent(&c, "safety-reviewer").await;
    let reviewer = c.with_token(c.mint_token(&reviewer_id, &[], 0, 0).await.unwrap());
    let work = sampled[0].work.clone();
    let review = safety
        .request_safety_review(c.req(RequestSafetyReviewRequest {
            work: work.clone(),
            reviewer_id,
        }))
        .await
        .unwrap()
        .into_inner();
    let submitted = safety
        .submit_safety_review(reviewer.req(SubmitSafetyReviewRequest {
            work: work.clone(),
            review_id: review.id,
            model: "independent-review-model".into(),
            proposed_verdict: SafetyVerdict::NeedsALook as i32,
            note: "Discuss the owner review requirement".into(),
            findings: vec![SafetyFindingReport {
                kind: SafetySignalKind::LegalFlags as i32,
                severity: SafetySeverity::Warning as i32,
                evidence: Some(SafetyEvidence {
                    observation:
                        "Clarify whether this generated example can be redistributed".into(),
                    ..Default::default()
                }),
            }],
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(submitted.model, "independent-review-model");
    assert_eq!(submitted.finding_ids.len(), 1);
    let record = safety
        .get_safety_record(c.req(work.unwrap()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(record.verdict(), SafetyVerdict::NeedsALook);
    assert_eq!(record.reviews, vec![submitted]);
}
