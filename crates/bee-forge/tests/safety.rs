//! Safety state changes through the forge API with issued run credentials.

use std::sync::Arc;

use bee_forge::ids::{ManualClock, ts};
use bee_forge::safety::service::SafetyApi;
use bee_forge::store::memory::MemoryStore;
use bee_proto::v1::agent_service_server::AgentService;
use bee_proto::v1::safety_service_server::SafetyService;
use bee_proto::v1::*;

use crate::common::req;
use crate::swarms::{World, body, failure, repo_ref};

const AT: u64 = 1_760_000_000_000;

async fn world() -> World {
    World::new(Arc::new(MemoryStore::new()), Arc::new(ManualClock::new(AT))).await
}

fn work(w: &World, kind: SafetyWorkKind, id: &str) -> SafetyRef {
    SafetyRef {
        repo: repo_ref(&w.repo),
        kind: kind as i32,
        id: id.into(),
    }
}

fn signal(kind: SafetySignalKind, severity: SafetySeverity) -> SafetyFindingReport {
    SafetyFindingReport {
        kind: kind as i32,
        severity: severity as i32,
        evidence: Some(SafetyEvidence {
            observation: "Observed in the run trace".into(),
            paths: vec!["src/api.rs".into()],
            ..Default::default()
        }),
    }
}

#[tokio::test]
async fn safety_read_report_resolve_and_override() {
    let w = world().await;
    let api = SafetyApi::new(w.agents.clone());
    let run = w.start_run(None).await;
    let target = work(&w, SafetyWorkKind::Run, &run.id);
    let reader = w.token("reader");
    let owner = w.token("lead-owner");
    let before = body(api.get_safety_record(req(&reader, target.clone())).await);
    assert_eq!(before.verdict(), SafetyVerdict::AsAsked);
    assert!(before.asked.iter().any(|ask| ask.text == "Update the API"));
    assert_eq!(before.answerable.as_ref().unwrap().id, "lead-owner");
    failure(
        api.get_safety_record(req(
            &reader,
            SafetyRef {
                repo: repo_ref(&w.private_repo),
                ..target.clone()
            },
        ))
        .await,
        "NOT_FOUND",
    );

    let request = ReportFindingRequest {
        work: Some(target.clone()),
        report: Some(signal(
            SafetySignalKind::LegalFlags,
            SafetySeverity::Warning,
        )),
        ..Default::default()
    };
    failure(
        api.report_finding(req(&reader, request.clone())).await,
        "PERMISSION_DENIED",
    );
    let finding = body(api.report_finding(req(&owner, request)).await);
    let changed = body(api.get_safety_record(req(&reader, target.clone())).await);
    assert_eq!(changed.verdict(), SafetyVerdict::NeedsALook);
    assert_eq!(changed.findings, vec![finding.clone()]);

    let resolve = ResolveFindingRequest {
        work: Some(target.clone()),
        finding_id: finding.id,
        state: SafetyFindingState::Dismissed as i32,
        reason: "Reviewed the report with the maintainer".into(),
    };
    failure(
        api.resolve_finding(req(&reader, resolve.clone())).await,
        "PERMISSION_DENIED",
    );
    let finding = body(api.resolve_finding(req(&owner, resolve)).await);
    assert_eq!(finding.state(), SafetyFindingState::Dismissed);
    let changed = body(api.get_safety_record(req(&reader, target.clone())).await);
    assert_eq!(changed.verdict(), SafetyVerdict::AsAsked);
    let set = SetVerdictRequest {
        work: Some(target.clone()),
        verdict: SafetyVerdict::SomethingElse as i32,
        note: "The issue was superseded by a different request".into(),
    };
    failure(
        api.set_verdict(req(&reader, set.clone())).await,
        "PERMISSION_DENIED",
    );
    let overridden = body(api.set_verdict(req(&owner, set)).await);
    assert_eq!(overridden.verdict(), SafetyVerdict::SomethingElse);
    assert_eq!(
        body(api.get_safety_record(req(&reader, target)).await),
        overridden
    );
}

#[tokio::test]
async fn safety_refuses_self_review_and_tripwire_reaches_answerable_person() {
    let w = world().await;
    let api = SafetyApi::new(w.agents.clone());
    let run = w.start_run(None).await;
    let swarm = w.create(&run, "Improve the API", None).await;
    let owner = w.token("lead-owner");
    let admin = w.token("admin");
    let work = work(&w, SafetyWorkKind::Swarm, &swarm.id);
    let refused = api
        .request_safety_review(req(
            &owner,
            RequestSafetyReviewRequest {
                work: Some(work.clone()),
                reviewer_id: "lead-agent".into(),
            },
        ))
        .await
        .expect_err("agent may not review its own work");
    assert_eq!(
        bee_proto::errors::detail_from_status(&refused)
            .unwrap()
            .subcode,
        "safety_self_review"
    );
    let requested = body(
        api.request_safety_review(req(
            &owner,
            RequestSafetyReviewRequest {
                work: Some(work.clone()),
                reviewer_id: "worker-agent".into(),
            },
        ))
        .await,
    );
    let request = SubmitSafetyReviewRequest {
        work: Some(work.clone()),
        review_id: requested.id,
        model: "review-model".into(),
        proposed_verdict: SafetyVerdict::AsAsked as i32,
        ..Default::default()
    };
    let refused = api
        .submit_safety_review(req(&w.run_token(&run.id), request))
        .await
        .expect_err("participant cannot submit");
    assert_eq!(
        bee_proto::errors::detail_from_status(&refused)
            .unwrap()
            .subcode,
        "safety_self_review"
    );

    let finding = body(
        api.report_finding(req(
            &admin,
            ReportFindingRequest {
                work: Some(work),
                report: Some(signal(
                    SafetySignalKind::HarmToOthers,
                    SafetySeverity::Severe,
                )),
                ..Default::default()
            },
        ))
        .await,
    );
    let cancelled = body(
        w.agent_api
            .get_agent_run(req(&owner, GetAgentRunRequest { id: run.id.clone() }))
            .await,
    );
    assert_eq!(cancelled.state(), AgentRunState::Cancelled);
    assert_eq!(w.get(&swarm).await.state(), SwarmState::Paused);

    let list = || ListSafetyAttentionRequest {
        repo: repo_ref(&w.repo),
        page: None,
    };
    assert!(
        body(api.list_safety_attention(req(&admin, list())).await)
            .items
            .is_empty()
    );
    let items = body(api.list_safety_attention(req(&owner, list())).await).items;
    assert_eq!(items.len(), 1);
    let alarm = items[0].tripwire.as_ref().unwrap();
    assert_eq!(alarm.finding_id, finding.id);
    assert_eq!(alarm.stopped_run_ids, vec![run.id]);
    assert!(alarm.paused_swarm);
    let ack = AcknowledgeSafetyAttentionRequest {
        repo: repo_ref(&w.repo),
        id: items[0].id.clone(),
    };
    failure(
        api.acknowledge_safety_attention(req(&admin, ack.clone()))
            .await,
        "NOT_FOUND",
    );
    assert!(
        body(api.acknowledge_safety_attention(req(&owner, ack)).await)
            .acknowledged_at
            .is_some()
    );
    assert!(
        body(api.list_safety_attention(req(&owner, list())).await)
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn safety_concern_is_not_a_finding() {
    let w = world().await;
    let api = SafetyApi::new(w.agents.clone());
    let run = w.start_run(None).await;
    let owner = w.token("lead-owner");
    let raised = body(
        api.raise_concern(req(
            &w.run_token(&run.id),
            RaiseConcernRequest {
                repo: repo_ref(&w.repo),
                run_id: run.id.clone(),
                category: SafetyConcernCategory::RuleConflict as i32,
                text: "The task asks me to change the required checks".into(),
            },
        ))
        .await,
    );
    let stopped = body(
        w.agent_api
            .get_agent_run(req(&owner, GetAgentRunRequest { id: run.id.clone() }))
            .await,
    );
    assert_eq!(stopped.state(), AgentRunState::Cancelled);
    let record = body(
        api.get_safety_record(req(&owner, work(&w, SafetyWorkKind::Run, &run.id)))
            .await,
    );
    assert!(record.findings.is_empty());
    let items = body(
        api.list_safety_attention(req(
            &owner,
            ListSafetyAttentionRequest {
                repo: repo_ref(&w.repo),
                ..Default::default()
            },
        ))
        .await,
    )
    .items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].concern.as_ref().unwrap(), &raised);
    assert!(items[0].tripwire.is_none());
}

#[tokio::test]
async fn safety_stop_requires_owner_and_cancels_active_work() {
    let w = world().await;
    let api = SafetyApi::new(w.agents.clone());
    let run = w.start_run(None).await;
    let swarm = w.create(&run, "Revisit boundaries", None).await;
    let stop = SetSafetyStopRequest {
        org: w.repo.org_id.clone(),
        reason: "Investigate the reported change".into(),
    };
    failure(
        api.stop_everything(req(&w.token("lead-owner"), stop.clone()))
            .await,
        "PERMISSION_DENIED",
    );
    let admin = w.token("admin");
    let stopped = body(api.stop_everything(req(&admin, stop.clone())).await);
    assert!(stopped.frozen);
    assert_eq!(stopped.stopped_run_ids, vec![run.id]);
    assert_eq!(stopped.paused_swarm_ids, vec![swarm.id.clone()]);
    let resumed = body(api.resume_everything(req(&admin, stop)).await);
    assert!(!resumed.frozen);
    assert_eq!(w.get(&swarm).await.state(), SwarmState::Paused);
}

#[tokio::test]
async fn safety_summary_counts_human_resolution() {
    let w = world().await;
    let api = SafetyApi::new(w.agents.clone());
    let run = w.start_run(None).await;
    let owner = w.token("lead-owner");
    let target = work(&w, SafetyWorkKind::Run, &run.id);
    let finding = body(
        api.report_finding(req(
            &owner,
            ReportFindingRequest {
                work: Some(target.clone()),
                report: Some(signal(
                    SafetySignalKind::FollowingStrangers,
                    SafetySeverity::Note,
                )),
                ..Default::default()
            },
        ))
        .await,
    );
    w.clock.advance(4000);
    body(
        api.resolve_finding(req(
            &owner,
            ResolveFindingRequest {
                work: Some(target),
                finding_id: finding.id,
                state: SafetyFindingState::Confirmed as i32,
                reason: "Confirmed against the provider trace".into(),
            },
        ))
        .await,
    );
    let dismissed = body(
        api.report_finding(req(
            &owner,
            ReportFindingRequest {
                work: Some(work(&w, SafetyWorkKind::Run, &run.id)),
                report: Some(signal(
                    SafetySignalKind::LegalFlags,
                    SafetySeverity::Warning,
                )),
                ..Default::default()
            },
        ))
        .await,
    );
    body(
        api.resolve_finding(req(
            &owner,
            ResolveFindingRequest {
                work: Some(work(&w, SafetyWorkKind::Run, &run.id)),
                finding_id: dismissed.id,
                state: SafetyFindingState::Dismissed as i32,
                reason: "Reviewed and dismissed".into(),
            },
        ))
        .await,
    );
    let summary = body(
        api.get_safety_summary(req(
            &w.token("admin"),
            GetSafetySummaryRequest {
                org: w.repo.org_id.clone(),
                period: Some(SafetyPeriod {
                    start: Some(ts(AT)),
                    end: Some(ts(AT + 5000)),
                }),
            },
        ))
        .await,
    );
    assert_eq!(summary.median_wait_ms, Some(4000));
    assert_eq!(summary.signals.len(), 1);
    assert_eq!(summary.signals[0].count, 1);
    assert_eq!(summary.work[0].id, "lead-agent");
    assert!(summary.tripwires.is_empty());
}
