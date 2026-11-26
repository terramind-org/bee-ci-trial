//! Safety detectors over attributed work, the change classifier, and forge stores.

use std::sync::Arc;

use async_trait::async_trait;
use bee_forge::changes::service::ChangeServices;
use bee_forge::changes::vcs::{LandFacts, NewCommit};
use bee_forge::ids::ManualClock;
use bee_forge::safety::{record_landing_attempt, service::SafetyApi};
use bee_forge::security::model::{CodeAlertRec, InstanceRec};
use bee_forge::security::{CODE_ALERTS, CODE_INSTANCES, SURFACE as SECURITY};
use bee_forge::store::{Row, memory::MemoryStore};
use bee_forge::swarms::service::SwarmApi;
use bee_forge::{Cx, Result};
use bee_proto::v1::change_service_server::ChangeService;
use bee_proto::v1::safety_service_server::SafetyService;
use bee_proto::v1::swarm_service_server::SwarmService;
use bee_proto::v1::*;

use crate::common::req;
use crate::swarms::{World, body, repo_ref};

async fn world() -> World {
    World::new(
        Arc::new(MemoryStore::new()),
        Arc::new(ManualClock::new(1_760_000_000_000)),
    )
    .await
}

fn work(world: &World, kind: SafetyWorkKind, id: &str) -> SafetyRef {
    SafetyRef {
        repo: repo_ref(&world.repo),
        kind: kind as i32,
        id: id.into(),
    }
}

async fn get(world: &World, work: SafetyRef) -> SafetyRecord {
    let api = SafetyApi::new(world.agents.clone());
    body(
        api.get_safety_record(req(&world.token("lead-owner"), work))
            .await,
    )
}

async fn open(world: &World, run: &AgentRun, edits: &[(&str, &str)]) -> (ChangeRequest, ObjectId) {
    let vcs = world.agents.changes.vcs();
    let base = vcs
        .ref_tip(&world.repo, "main")
        .await
        .unwrap()
        .unwrap()
        .target;
    let head = vcs
        .create_commit(
            &world.repo,
            NewCommit {
                parent: base,
                edits: edits
                    .iter()
                    .map(|(path, text)| ((*path).into(), Some(text.as_bytes().to_vec())))
                    .collect(),
                message: "Update the API".into(),
                author: "lead-agent".into(),
                creator: "lead-agent".into(),
                chain: vec!["lead-agent".into(), "lead-owner".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let changes = ChangeServices::new(world.agents.forge.clone(), world.agents.changes.clone());
    let change = body(
        changes
            .open_change(req(
                &world.run_token(&run.id),
                OpenChangeRequest {
                    repo: repo_ref(&world.repo),
                    target_ref: "main".into(),
                    head: Some(head.clone()),
                    title: "Update the API".into(),
                    ..Default::default()
                },
            ))
            .await,
    );
    (change, head)
}

fn report(record: &SafetyRecord, kind: SafetySignalKind) -> Option<&SafetyFindingReport> {
    record
        .findings
        .iter()
        .filter_map(|f| f.report.as_ref())
        .find(|r| r.kind() == kind)
}

struct RequiredWorkflow;

#[async_trait]
impl LandFacts for RequiredWorkflow {
    async fn signatures(
        &self,
        _repo: &Repo,
        _stack: &[ObjectId],
    ) -> Result<Vec<bee_policy::SignatureState>> {
        Ok(vec![])
    }

    async fn required_workflows(
        &self,
        _repo: &Repo,
        _heads: &[ObjectId],
    ) -> Result<Vec<bee_policy::WorkflowGate>> {
        Ok(vec![bee_policy::WorkflowGate {
            name: "verify".into(),
            passed: false,
        }])
    }

    async fn code_alerts(
        &self,
        _repo: &Repo,
        _target_ref: &str,
        _heads: &[ObjectId],
    ) -> Result<Vec<bee_policy::CodeAlert>> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn safety_attributes_changed_paths_checks_and_policy_to_the_attempt() {
    let world = world().await;
    world
        .agents
        .changes
        .set_land_facts(Arc::new(RequiredWorkflow));
    let run = world.start_run(None).await;
    let swarm = world.create(&run, "API update", None).await;
    let target = work(&world, SafetyWorkKind::Swarm, &swarm.id);
    let r = Some(SwarmRef {
        repo: repo_ref(&world.repo),
        id: swarm.id.clone(),
    });
    let api = SwarmApi::new(world.agents.clone());
    let owner = world.token("lead-owner");
    body(
        api.set_swarm_state(req(
            &owner,
            SetSwarmStateRequest {
                swarm: r.clone(),
                state: SwarmState::Working as i32,
            },
        ))
        .await,
    );
    let task = body(
        api.add_swarm_task(req(
            &owner,
            AddSwarmTaskRequest {
                swarm: r.clone(),
                title: "Edit only API code".into(),
                paths: vec![b"src".to_vec()],
            },
        ))
        .await,
    );
    let task = body(
        api.start_swarm_task(req(
            &owner,
            StartSwarmTaskRequest {
                swarm: r.clone(),
                task_id: task.id,
                run_id: run.id.clone(),
            },
        ))
        .await,
    );
    let (change, _) = open(
        &world,
        &run,
        &[
            ("src/api.rs", "pub const API_VERSION: u32 = 2;\n"),
            (".github/workflows/checks.yml", "name: Check API\n"),
        ],
    )
    .await;

    let before = get(&world, target.clone()).await;
    assert!(
        before
            .asked
            .iter()
            .any(|ask| ask.text == "Edit only API code")
    );
    assert_eq!(
        before.done.as_ref().unwrap().paths,
        [".github/workflows/checks.yml", "src/api.rs"]
    );
    assert_eq!(before.verdict(), SafetyVerdict::NeedsALook);
    assert!(report(&before, SafetySignalKind::SaidVsShown).is_none());
    for kind in [
        SafetySignalKind::OutsideTask,
        SafetySignalKind::TestsWeakened,
    ] {
        let evidence = report(&before, kind).unwrap().evidence.as_ref().unwrap();
        assert_eq!(evidence.paths, [".github/workflows/checks.yml"]);
        assert!(
            evidence
                .subjects
                .iter()
                .any(|s| s.id == change.number.to_string())
        );
    }

    body(
        api.finish_swarm_task(req(
            &owner,
            FinishSwarmTaskRequest {
                swarm: r,
                task_id: task.id,
                run_id: run.id.clone(),
                outcome: SwarmAttemptOutcome::Finished as i32,
                reason: "Implementation completed".into(),
                hand_back: Some(SwarmHandBack {
                    done: vec!["Updated the API".into()],
                    ..Default::default()
                }),
                attempt_number: 1,
            },
        ))
        .await,
    );
    let handed_back = get(&world, target.clone()).await;
    let checks = report(&handed_back, SafetySignalKind::SaidVsShown).unwrap();
    assert!(
        checks
            .evidence
            .as_ref()
            .unwrap()
            .observation
            .contains("verify")
    );

    open(
        &world,
        &run,
        &[("AGENTS.md", "Update repository agent instructions.\n")],
    )
    .await;
    let changed = get(&world, target).await;
    let leash = report(&changed, SafetySignalKind::OwnLeash).unwrap();
    assert_eq!(leash.severity(), SafetySeverity::Severe);
    assert_eq!(leash.evidence.as_ref().unwrap().paths, ["AGENTS.md"]);
    assert_eq!(world.get(&swarm).await.state(), SwarmState::Paused);
}

#[tokio::test]
async fn safety_refusal_follows_verified_run_and_exact_effect() {
    let world = world().await;
    let run = world.start_run(None).await;
    let bearer = world.run_token(&run.id);
    let forge = &world.agents.forge;
    let caller = forge.authn.authenticate(&req(&bearer, ())).unwrap();
    let target = work(&world, SafetyWorkKind::Run, &run.id);
    for (effect, result) in [
        ("ref:main:one", Some(ErrorReason::PolicyDenied)),
        ("ref:main:two", None),
    ] {
        record_landing_attempt(
            forge,
            &caller.cx,
            &world.repo,
            &caller.token,
            effect,
            result,
        )
        .await
        .unwrap();
    }
    assert!(
        report(
            &get(&world, target.clone()).await,
            SafetySignalKind::AroundRefusal
        )
        .is_none()
    );
    record_landing_attempt(
        forge,
        &caller.cx,
        &world.repo,
        &caller.token,
        "ref:main:one",
        None,
    )
    .await
    .unwrap();
    let record = get(&world, target).await;
    assert_eq!(record.verdict(), SafetyVerdict::NeedsALook);
    let report = report(&record, SafetySignalKind::AroundRefusal).unwrap();
    assert!(
        report
            .evidence
            .as_ref()
            .unwrap()
            .observation
            .contains("ref:main:one")
    );
}

#[tokio::test]
async fn safety_reads_code_scan_alerts_only_on_attributed_changes() {
    let world = world().await;
    let run = world.start_run(None).await;
    let (_, head) = open(
        &world,
        &run,
        &[("src/api.rs", "pub const API_VERSION: u32 = 2;\n")],
    )
    .await;
    let target = work(&world, SafetyWorkKind::Run, &run.id);
    let db = &world.agents.forge.db;
    let state = CodeScanningAlertState::Open as i32;
    let seed = |number, changeset| {
        (
            CodeAlertRec {
                alert: Some(CodeScanningAlert {
                    number,
                    state,
                    rule: Some(CodeScanningRule {
                        security_severity: SecuritySeverity::High as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                key: format!("scanner:rule:{number}"),
            },
            InstanceRec {
                instance: Some(CodeScanningAlertInstance {
                    state,
                    changeset: Some(changeset),
                    location: Some(CodeScanningLocation {
                        path: b"src/api.rs".to_vec(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            },
        )
    };
    let mut tx = db.begin(SECURITY, &Cx::system()).await.unwrap();
    let base = world
        .agents
        .changes
        .vcs()
        .ref_tip(&world.repo, "main")
        .await
        .unwrap()
        .unwrap()
        .target
        .unwrap();
    for (number, id) in [(1_u64, base), (2, head)] {
        let (alert, instance) = seed(number, id);
        let key = format!("{number:020}");
        tx.insert(
            &CODE_ALERTS,
            &Row::new(&world.repo.id, &key).k1(&alert.key).msg(&alert),
        )
        .await
        .unwrap();
        tx.insert(
            &CODE_INSTANCES,
            &Row::new(format!("{}:{key}", world.repo.id), "main").msg(&instance),
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let record = get(&world, target).await;
    let alerts: Vec<_> = record
        .findings
        .iter()
        .filter_map(|f| f.report.as_ref())
        .filter(|r| r.kind() == SafetySignalKind::HarmToCode)
        .collect();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].severity(), SafetySeverity::Severe);
    let evidence = alerts[0].evidence.as_ref().unwrap();
    assert_eq!(evidence.paths, ["src/api.rs"]);
    assert!(
        evidence
            .subjects
            .iter()
            .any(|s| s.kind() == SubjectKind::SecurityAlert && s.id == "2")
    );
}
