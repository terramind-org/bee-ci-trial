//! Swarm coordination through the forge RPCs and the existing agent run and event stores.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bee_auth::{
    AgentInfo, InMemoryRevocations, KeyRing, MintRequest, PrincipalRef, Scope, SigningKey,
};
use bee_forge::agents::fake::FakeRunner;
use bee_forge::agents::local::LocalProvider;
use bee_forge::agents::ports::IssuedToken;
use bee_forge::agents::service::AgentApi;
use bee_forge::agents::{
    AgentDefinition, AgentPorts, Agents, AgentsConfig, KeyRingIssuer, MemoryBlobs, RunGrant,
    RunTokenIssuer,
};
use bee_forge::changes::service::ChangeServices;
use bee_forge::changes::vcs::NewCommit;
use bee_forge::changes::{ChangeHooks, ChangeVcs, Changes, HistoryVcs, MemoryTeams};
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner, PolicySource, RepoInit};
use bee_forge::ids::{Clock, ManualClock};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::service::ForgeServices;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::swarms::service::SwarmApi;
use bee_forge::work::{Actor, NewIssue, RepoInfo};
use bee_forge::{Cx, Forge};
use bee_proto::v1::agent_service_server::AgentService;
use bee_proto::v1::change_service_server::ChangeService;
use bee_proto::v1::event_service_server::EventService;
use bee_proto::v1::swarm_service_server::SwarmService;
use bee_proto::v1::*;
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};
use tokio_stream::StreamExt;

use crate::common::{reason, req, test_config};

const T0: u64 = 1_760_000_000_000;
const AGENTS: [(&str, &str); 4] = [
    ("lead-agent", "lead-owner"),
    ("worker-agent", "contributor"),
    ("declined-agent", "contributor"),
    ("refused-agent", "contributor"),
];

#[derive(Default)]
struct TestPolicy(RwLock<Option<bee_policy::Policy>>);

#[async_trait::async_trait]
impl PolicySource for TestPolicy {
    async fn policy(&self, _repo: &Repo) -> bee_forge::Result<Option<bee_policy::Policy>> {
        Ok(self.0.read().unwrap().clone())
    }
}

/// Captures issued credentials without dispatching the queued runs.
struct TestIssuer {
    inner: KeyRingIssuer,
    tokens: Mutex<BTreeMap<String, String>>,
}

#[async_trait::async_trait]
impl RunTokenIssuer for TestIssuer {
    async fn issue(&self, grant: &RunGrant) -> bee_forge::Result<IssuedToken> {
        let issued = self.inner.issue(grant).await?;
        self.tokens
            .lock()
            .unwrap()
            .insert(grant.run_id.clone(), issued.token.clone());
        Ok(issued)
    }

    async fn revoke(&self, token_id: &str) -> bee_forge::Result<()> {
        self.inner.revoke(token_id).await
    }
}

// Reused by the Safety API tests for genuine run-token authentication.
pub(super) struct World {
    api: SwarmApi,
    pub(super) agents: Arc<Agents>,
    pub(super) agent_api: AgentApi,
    changes: Arc<Changes>,
    forge: Arc<Forge>,
    pub(super) repo: Repo,
    pub(super) private_repo: Repo,
    pub(super) clock: Arc<ManualClock>,
    ring: KeyRing,
    issuer: Arc<TestIssuer>,
    policy: Arc<TestPolicy>,
    _workspace: tempfile::TempDir,
}

fn ring() -> KeyRing {
    KeyRing::new(SigningKey::from_secret_bytes(1, &[9; 32]).unwrap())
}

fn human(id: &str) -> Principal {
    Principal {
        id: id.into(),
        display: id.into(),
        kind: PrincipalKind::Human as i32,
        ..Default::default()
    }
}

fn worker(id: &str, owner: &str) -> Principal {
    Principal {
        id: id.into(),
        display: id.into(),
        kind: PrincipalKind::Agent as i32,
        owner_id: owner.into(),
        runtime: "codex".into(),
        ..Default::default()
    }
}

pub(super) fn repo_ref(repo: &Repo) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(repo.id.clone())),
    })
}

pub(super) fn body<T>(response: Result<tonic::Response<T>, tonic::Status>) -> T {
    response
        .unwrap_or_else(|status| panic!("{status:?}"))
        .into_inner()
}

pub(super) fn failure<T: std::fmt::Debug>(
    response: Result<tonic::Response<T>, tonic::Status>,
    expected: &str,
) {
    let status = response.expect_err("expected refusal");
    let detail = bee_proto::errors::detail_from_status(&status).expect("SPEC 12.2 error detail");
    assert_eq!(detail.reason, expected);
    assert!(!detail.subcode.is_empty(), "{detail:?}");
}

impl World {
    pub(super) async fn new(store: Arc<dyn ForgeStore>, clock: Arc<ManualClock>) -> Self {
        let directory = Arc::new(MemoryDirectory::new());
        for id in ["admin", "lead-owner", "contributor", "reader"] {
            directory.register(human(id), Vec::new(), Vec::new());
        }
        for (id, owner) in AGENTS {
            directory.register(worker(id, owner), Vec::new(), Vec::new());
        }
        let key_ring = ring();
        let revocations = Arc::new(InMemoryRevocations::new());
        let policy = Arc::new(TestPolicy::default());
        let meta = Arc::new(MemoryMetaStore::new());
        let (forge, _) = Forge::open(
            store,
            clock.clone(),
            key_ring.verifying_keys(),
            revocations.clone(),
            Hooks {
                directory,
                provisioner: Arc::new(MetaProvisioner::new(meta.clone(), clock.clone())),
                policy: Some(policy.clone()),
                ..Hooks::default()
            },
            test_config(),
        )
        .await
        .unwrap();
        let admin = Cx::new("admin", PrincipalKind::Human);
        let org_name = format!("o{}", forge.db.ids().ulid().unwrap().to_lowercase());
        let org = forge
            .orgs
            .create_org(&admin, &org_name, "", human("admin"))
            .await
            .unwrap();
        forge.hooks.provisioner.create_org(&org).await.unwrap();
        let create_repo = |name: &'static str, visibility| {
            let forge = forge.clone();
            let admin = admin.clone();
            let org_id = org.id.clone();
            async move {
                let repo = forge
                    .orgs
                    .create_repo(
                        &admin,
                        &org_id,
                        NewRepo {
                            id: forge.db.ids().ulid().unwrap(),
                            name: name.into(),
                            visibility,
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                forge
                    .hooks
                    .provisioner
                    .create_repo(&repo, &RepoInit::default())
                    .await
                    .unwrap();
                repo
            }
        };
        let repo = create_repo("api", Visibility::Public).await;
        let private_repo = create_repo("private", Visibility::Private).await;
        for principal in [human("lead-owner"), worker("lead-agent", "lead-owner")] {
            forge
                .orgs
                .set_member(&admin, &org.id, principal.clone(), Role::Member)
                .await
                .unwrap();
            forge
                .orgs
                .add_collaborator(&admin, &repo.id, principal, RepoRole::Write, "")
                .await
                .unwrap();
        }
        let vcs = Arc::new(HistoryVcs::new(
            Writer::new(meta, Arc::new(MemoryBlobStore::new(clock.clone()))),
            clock.clone(),
        ));
        let root = vcs
            .create_commit(
                &repo,
                NewCommit {
                    parent: None,
                    edits: vec![("README.md".into(), Some(b"# API\n".to_vec()))],
                    message: "Initialize repository".into(),
                    author: "admin".into(),
                    creator: "admin".into(),
                    chain: vec!["admin".into()],
                    change_id: None,
                },
            )
            .await
            .unwrap();
        vcs.set_ref(&repo, "main", &root).await.unwrap();
        let changes = Changes::new(
            &forge,
            ChangeHooks {
                vcs: vcs.clone(),
                lander: vcs.clone(),
                teams: Arc::new(MemoryTeams::new()),
            },
        );
        changes.attach(&forge);
        let runner = Arc::new(FakeRunner::with_forge(
            changes.clone(),
            vcs.clone(),
            forge.leases.clone(),
        ));
        let workspace = tempfile::tempdir().unwrap();
        let issuer = Arc::new(TestIssuer {
            inner: KeyRingIssuer::new(ring(), Some(revocations), clock.clone()),
            tokens: Mutex::new(BTreeMap::new()),
        });
        let agents = Agents::new(
            forge.clone(),
            changes.clone(),
            AgentPorts {
                runner,
                issuer: issuer.clone(),
                blobs: Arc::new(MemoryBlobs::new()),
                repo: vcs.clone(),
                workspaces: Arc::new(LocalProvider::new(workspace.path(), vcs)),
            },
            AgentsConfig::default(),
        );
        agents.attach();
        for (id, owner) in AGENTS {
            agents
                .define_agent(
                    &admin,
                    AgentDefinition {
                        org_id: org.id.clone(),
                        agent: worker(id, owner),
                        name: id.into(),
                        runtime: "codex".into(),
                        owners: vec![owner.into()],
                        toolsets: ["read", "commit", "lease"].map(str::to_string).to_vec(),
                        default_budget: AgentRunBudget {
                            max_usd_micros: 5_000_000,
                            ..Default::default()
                        },
                        enabled: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        Self {
            api: SwarmApi::new(agents.clone()),
            agent_api: AgentApi::new(agents.clone()),
            agents,
            forge,
            changes,
            repo,
            private_repo,
            clock,
            ring: key_ring,
            issuer,
            policy,
            _workspace: workspace,
        }
    }

    pub(super) fn token(&self, who: &str) -> String {
        self.ring
            .mint(
                &MintRequest {
                    principal: bee_auth::Principal::human(who, who),
                    scope: Scope::new(),
                    on_behalf_of: Vec::new(),
                },
                self.clock.now_ms() / 1000,
            )
            .unwrap()
            .text
    }

    fn forged_lead_session(&self, session: &str) -> String {
        let (who, owner) = AGENTS[0];
        self.ring
            .mint(
                &MintRequest {
                    principal: bee_auth::Principal::agent(
                        who,
                        who,
                        AgentInfo {
                            owner: bee_policy::PrincipalId::new(owner),
                            runtime: "codex".into(),
                            purpose: "Swarm integration".into(),
                        },
                    ),
                    scope: Scope::new().session(session),
                    on_behalf_of: vec![PrincipalRef::new(owner, bee_policy::PrincipalKind::Human)],
                },
                self.clock.now_ms() / 1000,
            )
            .unwrap()
            .text
    }

    pub(super) fn run_token(&self, id: &str) -> String {
        self.issuer.tokens.lock().unwrap()[id].clone()
    }

    pub(super) async fn start_run(&self, issue: Option<&Issue>) -> AgentRun {
        let task = match issue {
            Some(issue) => AgentTask {
                kind: AgentTaskKind::Issue as i32,
                issue: issue.number.to_string(),
                ..Default::default()
            },
            None => AgentTask {
                kind: AgentTaskKind::Freeform as i32,
                prompt: "Update the API".into(),
                ..Default::default()
            },
        };
        body(
            self.agent_api
                .start_agent_run(req(
                    &self.token("lead-owner"),
                    StartAgentRunRequest {
                        repo: repo_ref(&self.repo),
                        task: Some(task),
                        base_ref: "main".into(),
                        agent_principal_id: "lead-agent".into(),
                        ..Default::default()
                    },
                ))
                .await,
        )
    }

    pub(super) async fn create(&self, run: &AgentRun, name: &str, issue: Option<&Issue>) -> Swarm {
        body(
            self.api
                .create_swarm(req(
                    &self.token("lead-owner"),
                    CreateSwarmRequest {
                        repo: repo_ref(&self.repo),
                        name: name.into(),
                        goal: "Make the public API easier to use.".into(),
                        issue: issue
                            .map(|issue| issue.number.to_string())
                            .unwrap_or_default(),
                        branch: "main".into(),
                        lead_run_id: run.id.clone(),
                    },
                ))
                .await,
        )
    }

    fn swarm_ref(&self, swarm: &Swarm) -> SwarmRef {
        SwarmRef {
            repo: repo_ref(&self.repo),
            id: swarm.id.clone(),
        }
    }

    pub(super) async fn get(&self, swarm: &Swarm) -> Swarm {
        body(
            self.api
                .get_swarm(req(&self.token("lead-owner"), self.swarm_ref(swarm)))
                .await,
        )
    }

    fn set_state(&self, swarm: &Swarm, state: SwarmState) -> SetSwarmStateRequest {
        SetSwarmStateRequest {
            swarm: Some(self.swarm_ref(swarm)),
            state: state as i32,
        }
    }

    async fn state(&self, swarm: &Swarm, who: &str, state: SwarmState) -> Swarm {
        body(
            self.api
                .set_swarm_state(req(&self.token(who), self.set_state(swarm, state)))
                .await,
        )
    }

    async fn request_join(&self, swarm: &Swarm, agent: &str) -> SwarmJoin {
        let join = body(
            self.api
                .request_swarm_join(req(
                    &self.token("contributor"),
                    RequestSwarmJoinRequest {
                        swarm: Some(self.swarm_ref(swarm)),
                        agent_principal_id: agent.into(),
                        budget: Some(AgentRunBudget {
                            max_usd_micros: 200_000,
                            ..Default::default()
                        }),
                    },
                ))
                .await,
        );
        assert_eq!(join.state(), SwarmJoinState::Pending);
        assert_eq!(join.requester.as_ref().unwrap().id, "contributor");
        assert_eq!(join.agent.as_ref().unwrap().id, agent);
        assert_eq!(join.budget.as_ref().unwrap().max_usd_micros, 200_000);
        join
    }

    async fn decide(
        &self,
        swarm: &Swarm,
        join: &SwarmJoin,
        accept: bool,
        token: &str,
    ) -> SwarmJoin {
        body(
            self.api
                .decide_swarm_join(req(
                    token,
                    DecideSwarmJoinRequest {
                        swarm: Some(self.swarm_ref(swarm)),
                        join_id: join.id.clone(),
                        accept,
                        reason: if accept {
                            "".into()
                        } else {
                            "The task is already staffed.".into()
                        },
                    },
                ))
                .await,
        )
    }

    async fn run(&self, id: &str) -> AgentRun {
        self.agents.get_run(&Cx::system(), id).await.unwrap()
    }
}

#[tokio::test]
async fn sqlite_swarm_coordinates_tasks_and_cancels_members() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(T0));
    let store = SqlStore::open(SqlConfig::sqlite(dir.path()), clock.clone())
        .await
        .unwrap();
    let world = World::new(Arc::new(store), clock).await;
    coordinates_tasks_and_cancels_members(&world).await;
}

#[tokio::test]
async fn postgres_swarm_coordinates_tasks_and_cancels_members_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_swarms_{}", std::process::id());
    }
    let clock = Arc::new(ManualClock::new(T0));
    let store = SqlStore::open(cfg, clock.clone()).await.unwrap();
    let world = World::new(Arc::new(store), clock).await;
    coordinates_tasks_and_cancels_members(&world).await;
}

async fn coordinates_tasks_and_cancels_members(w: &World) {
    let lead = w.start_run(None).await;
    let swarm = w.create(&lead, "API ergonomics", None).await;
    assert_eq!(swarm.state(), SwarmState::Planning);
    assert_eq!(swarm.name, "API ergonomics");
    assert_eq!(swarm.goal, "Make the public API easier to use.");
    assert_eq!(swarm.branch, "main");
    assert_eq!(swarm.owner.as_ref().unwrap().id, "lead-owner");
    assert_eq!(swarm.lead.as_ref().unwrap().id, "lead-agent");
    assert_eq!(swarm.lead_run_id, lead.id);
    assert_eq!(
        swarm.member_run_ids.as_slice(),
        std::slice::from_ref(&lead.id)
    );

    w.forge.flush().await.unwrap();
    let events = ForgeServices::new(w.forge.clone());
    let mut stream = body(
        events
            .subscribe(req(
                &w.token("lead-owner"),
                SubscribeRequest {
                    repo: repo_ref(&w.repo),
                    from_seq: 0,
                    kinds: vec!["swarm.*".into()],
                    ..Default::default()
                },
            ))
            .await,
    );
    let created = next_event(&mut stream).await;
    assert_eq!(created.kind, "swarm.created");
    let subject = created.subject.unwrap();
    assert_eq!(subject.kind(), SubjectKind::Swarm);
    assert_eq!(subject.id, swarm.id);

    let start = w.set_state(&swarm, SwarmState::Working);
    failure(
        w.api.set_swarm_state(req(&w.token("reader"), start)).await,
        "PERMISSION_DENIED",
    );
    assert_eq!(
        w.state(&swarm, "lead-owner", SwarmState::Working)
            .await
            .state(),
        SwarmState::Working
    );
    assert_eq!(
        w.state(&swarm, "admin", SwarmState::Paused).await.state(),
        SwarmState::Paused
    );
    assert_eq!(
        w.state(&swarm, "lead-owner", SwarmState::Working)
            .await
            .state(),
        SwarmState::Working
    );

    let status = SetSwarmStatusRequest {
        swarm: Some(w.swarm_ref(&swarm)),
        status: "The API changes are ready to assign.".into(),
    };
    failure(
        w.api
            .set_swarm_status(req(&w.forged_lead_session(&lead.session), status.clone()))
            .await,
        "PERMISSION_DENIED",
    );
    let status = body(
        w.api
            .set_swarm_status(req(&w.run_token(&lead.id), status))
            .await,
    );
    assert_eq!(status.status, "The API changes are ready to assign.");

    let pending = w.request_join(&swarm, "worker-agent").await;
    failure(
        w.api
            .decide_swarm_join(req(
                &w.token("contributor"),
                DecideSwarmJoinRequest {
                    swarm: Some(w.swarm_ref(&swarm)),
                    join_id: pending.id.clone(),
                    accept: true,
                    reason: "".into(),
                },
            ))
            .await,
        "PERMISSION_DENIED",
    );
    let joined = w
        .decide(&swarm, &pending, true, &w.run_token(&lead.id))
        .await;
    assert_eq!(joined.state(), SwarmJoinState::Accepted);
    let member = w.run(&joined.run_id).await;
    assert_eq!(member.agent.as_ref().unwrap().id, "worker-agent");
    assert_eq!(member.base_ref, swarm.branch);
    assert_eq!(member.budget.as_ref().unwrap().max_usd_micros, 200_000);
    let declined = w.request_join(&swarm, "declined-agent").await;
    let declined = w
        .decide(&swarm, &declined, false, &w.token("lead-owner"))
        .await;
    assert_eq!(declined.state(), SwarmJoinState::Declined);
    assert!(declined.run_id.is_empty());
    assert_eq!(declined.reason, "The task is already staffed.");

    let denied = w.request_join(&swarm, "refused-agent").await;
    *w.policy.0.write().unwrap() = Some(
        bee_policy::Policy::parse(
            r#"
        version = 1
        [roles.guest]
        actions = ["read"]
        [roles.steward]
        actions = ["read", "commit", "manage_settings", "swarm_join"]
        [members]
        guest = ["@everyone"]
        steward = ["lead-owner", "admin", "contributor"]
        [[access]]
        path = "/"
        read = ["@everyone"]
        write = ["@everyone"]
    "#,
        )
        .unwrap(),
    );
    let refused = w
        .decide(&swarm, &denied, true, &w.token("lead-owner"))
        .await;
    assert_eq!(refused.state(), SwarmJoinState::Refused);
    assert!(refused.run_id.is_empty());
    assert!(!refused.reason.is_empty());
    *w.policy.0.write().unwrap() = None;
    assert_eq!(
        w.get(&swarm)
            .await
            .joins
            .iter()
            .find(|join| join.id == refused.id),
        Some(&refused)
    );

    let task = body(
        w.api
            .add_swarm_task(req(
                &w.token("lead-owner"),
                AddSwarmTaskRequest {
                    swarm: Some(w.swarm_ref(&swarm)),
                    title: "Document the public API".into(),
                    paths: vec![b"docs/api.md".to_vec()],
                },
            ))
            .await,
    );
    assert_eq!(task.state(), SwarmTaskState::Open);
    assert_eq!(task.paths, [b"docs/api.md".to_vec()]);
    let start = StartSwarmTaskRequest {
        swarm: Some(w.swarm_ref(&swarm)),
        task_id: task.id,
        run_id: member.id.clone(),
    };
    w.state(&swarm, "lead-owner", SwarmState::Paused).await;
    assert_eq!(w.agents.dispatch_once().await.unwrap(), 0);
    for run in [&lead, &member] {
        assert_eq!(w.run(&run.id).await.state(), AgentRunState::Queued);
    }
    failure(
        w.api
            .start_swarm_task(req(&w.token("lead-owner"), start.clone()))
            .await,
        "INVALID_STATE",
    );
    w.state(&swarm, "lead-owner", SwarmState::Working).await;
    let member_token = w.run_token(&member.id);
    for (index, outcome, explanation) in [
        (
            0,
            SwarmAttemptOutcome::Empty,
            "The specification was missing.",
        ),
        (
            1,
            SwarmAttemptOutcome::Refused,
            "The rules protect this path.",
        ),
        (
            2,
            SwarmAttemptOutcome::Finished,
            "The maintainer supplied the specification.",
        ),
    ] {
        w.clock.advance(10);
        let active = body(
            w.api
                .start_swarm_task(req(&w.token("lead-owner"), start.clone()))
                .await,
        );
        assert_eq!(active.state(), SwarmTaskState::Working);
        assert_eq!(active.run_id, member.id);
        assert_eq!(active.attempts.len(), index + 1);
        let hand_back = SwarmHandBack {
            done: vec!["Checked the public contract.".into()],
            deviations: vec!["Used a smaller example.".into()],
            concerns: vec!["The contract has a missing case.".into()],
            follow_ups: vec!["Expand the reference later.".into()],
        };
        w.clock.advance(10);
        let finish = FinishSwarmTaskRequest {
            swarm: start.swarm.clone(),
            task_id: start.task_id.clone(),
            run_id: member.id.clone(),
            outcome: outcome as i32,
            reason: explanation.into(),
            hand_back: Some(hand_back.clone()),
            attempt_number: u32::try_from(index + 1).unwrap(),
        };
        if index == 1 {
            let stale = FinishSwarmTaskRequest {
                attempt_number: 1,
                ..finish.clone()
            };
            failure(
                w.api.finish_swarm_task(req(&member_token, stale)).await,
                "VERSION_MISMATCH",
            );
        }
        let finished = body(w.api.finish_swarm_task(req(&member_token, finish)).await);
        assert_eq!(
            finished.state(),
            if outcome == SwarmAttemptOutcome::Finished {
                SwarmTaskState::Finished
            } else {
                SwarmTaskState::Empty
            }
        );
        assert!(finished.run_id.is_empty());
        let attempt = &finished.attempts[index];
        assert_eq!(attempt.outcome(), outcome);
        assert_eq!(attempt.run_id, member.id);
        assert_eq!(attempt.agent.as_ref().unwrap().id, "worker-agent");
        assert_eq!(attempt.reason, explanation);
        assert_eq!(attempt.hand_back.as_ref(), Some(&hand_back));
        assert!(
            bee_forge::ids::ts_ms(attempt.finished_at.as_ref().unwrap())
                > bee_forge::ids::ts_ms(attempt.started_at.as_ref().unwrap())
        );
    }
    let send = SendSwarmMessageRequest {
        swarm: Some(w.swarm_ref(&swarm)),
        body: "Keep the examples short.".into(),
    };
    failure(
        w.api
            .send_swarm_message(req(&member_token, send.clone()))
            .await,
        "PERMISSION_DENIED",
    );
    let event = body(
        w.api
            .send_swarm_message(req(&w.token("lead-owner"), send))
            .await,
    );
    assert_eq!(event.run_id, lead.id);
    assert_eq!(event.author.as_ref().unwrap().id, "lead-owner");
    assert_eq!(event.text, "Keep the examples short.");
    let kind = AgentRunEventKind::SteeringMessage as i32;
    for (run, expected) in [(&lead, 1), (&member, 0)] {
        let messages = w
            .agents
            .run_events(&Cx::system(), &run.id, 0, &[kind], 10)
            .await
            .unwrap();
        assert_eq!(messages.len(), expected);
    }

    let active = w.get(&swarm).await;
    assert_eq!(active.member_run_ids.len(), 2);
    assert!(active.member_run_ids.contains(&member.id));
    assert_eq!(active.budget.as_ref().unwrap().max_usd_micros, 5_200_000);
    let visible = body(
        w.api
            .get_swarm(req(&w.token("reader"), w.swarm_ref(&swarm)))
            .await,
    );
    assert_eq!(visible.id, active.id);
    assert!(visible.cost.is_none());
    assert!(visible.budget.is_none());
    assert!(visible.joins.is_empty());

    let unfinished = body(
        w.api
            .add_swarm_task(req(
                &w.token("lead-owner"),
                AddSwarmTaskRequest {
                    swarm: Some(w.swarm_ref(&swarm)),
                    title: "Review the API examples".into(),
                    paths: vec![b"docs/examples.md".to_vec()],
                },
            ))
            .await,
    );
    body(
        w.api
            .start_swarm_task(req(
                &w.token("lead-owner"),
                StartSwarmTaskRequest {
                    swarm: Some(w.swarm_ref(&swarm)),
                    task_id: unfinished.id.clone(),
                    run_id: lead.id.clone(),
                },
            ))
            .await,
    );
    let stopped = w.state(&swarm, "admin", SwarmState::Stopped).await;
    assert_eq!(stopped.state(), SwarmState::Stopped);
    let unfinished = stopped
        .tasks
        .iter()
        .find(|task| task.id == unfinished.id)
        .unwrap();
    assert_eq!(unfinished.state(), SwarmTaskState::Empty);
    let attempt = &unfinished.attempts[0];
    assert_eq!(attempt.outcome(), SwarmAttemptOutcome::Empty);
    assert!(attempt.finished_at.is_some());
    assert!(attempt.hand_back.is_some());
    for run in [&lead, &member] {
        assert_eq!(w.run(&run.id).await.state(), AgentRunState::Cancelled);
    }
    w.forge.flush().await.unwrap();
    let (logged, _) = w
        .forge
        .events
        .read(&Cx::system(), &w.repo.id, 0, &["swarm.*".into()], 100)
        .await
        .unwrap();
    for (kind, expected) in [
        ("swarm.created", 1),
        ("swarm.state_changed", 6),
        ("swarm.status_changed", 1),
        ("swarm.join_requested", 3),
        ("swarm.join_decided", 3),
        ("swarm.task_added", 2),
        ("swarm.task_started", 4),
        ("swarm.task_finished", 4),
    ] {
        assert_eq!(
            logged.iter().filter(|event| event.kind == kind).count(),
            expected,
            "{kind}"
        );
    }
    assert_eq!(next_event(&mut stream).await.kind, "swarm.state_changed");
}

async fn next_event(
    stream: &mut (impl tokio_stream::Stream<Item = Result<Event, tonic::Status>> + Unpin),
) -> Event {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn repo_work_lists_swarms_and_single_runs_by_latest_activity_and_links_outcomes() {
    let w = World::new(Arc::new(MemoryStore::new()), Arc::new(ManualClock::new(T0))).await;
    let lead_issue = issue(&w, "Improve the reference").await;
    let single_issue = issue(&w, "Clarify the error message").await;
    let lead = w.start_run(Some(&lead_issue)).await;
    let swarm = w
        .create(&lead, "Reference improvements", Some(&lead_issue))
        .await;
    assert_eq!(swarm.issue, lead_issue.number.to_string());

    let other_repo = SwarmRef {
        repo: repo_ref(&w.private_repo),
        id: swarm.id.clone(),
    };
    assert_eq!(
        reason(
            w.api
                .get_swarm(req(&w.token("contributor"), other_repo.clone()))
                .await
        ),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(w.api.get_swarm(req(&w.token("admin"), other_repo)).await),
        "NOT_FOUND"
    );

    w.state(&swarm, "lead-owner", SwarmState::Working).await;
    w.clock.advance(20);
    let solo = w.start_run(Some(&single_issue)).await;
    w.agents.dispatch_once().await.unwrap();
    w.forge.flush().await.unwrap();
    let lead_pr = close_produced_pull_request(&w, &lead).await;
    w.clock.advance(20);
    let solo_pr = close_produced_pull_request(&w, &solo).await;
    let solo = w.run(&solo.id).await;
    assert_eq!(solo.state(), AgentRunState::Failed);

    let list = |page_token: String| {
        req(
            &w.token("lead-owner"),
            ListRepoWorkRequest {
                repo: repo_ref(&w.repo),
                page: Some(PageRequest {
                    page_size: 1,
                    page_token,
                }),
            },
        )
    };
    let recent = body(w.api.list_repo_work(list(String::new())).await);
    assert_eq!(recent.items.len(), 1);
    match &recent.items[0].work {
        Some(repo_work_item::Work::Run(run)) => assert_eq!(run.id, solo.id),
        other => panic!("expected the standalone run, got {other:?}"),
    }
    assert_eq!(recent.items[0].change_request_ids, [solo_pr.id]);
    let next_token = recent.page.unwrap().next_page_token;
    assert!(!next_token.is_empty());
    let next = body(w.api.list_repo_work(list(next_token)).await);
    assert_eq!(next.items.len(), 1);
    match &next.items[0].work {
        Some(repo_work_item::Work::Swarm(item)) => assert_eq!(item.id, swarm.id),
        other => panic!("expected the swarm, got {other:?}"),
    }
    assert!(next.page.is_none_or(|page| page.next_page_token.is_empty()));

    w.clock.advance(20);
    let finished = w.state(&swarm, "lead-owner", SwarmState::Finished).await;
    assert_eq!(finished.state(), SwarmState::Finished);
    assert_eq!(finished.change_request_ids, [lead_pr.id]);
    assert_eq!(finished.cost, w.run(&lead.id).await.cost);
    assert!(finished.cost.unwrap().usd_micros > 0);
    let newest = body(w.api.list_repo_work(list(String::new())).await);
    assert!(
        matches!(&newest.items[0].work, Some(repo_work_item::Work::Swarm(item)) if item.id == swarm.id)
    );
}

async fn issue(w: &World, title: &str) -> Issue {
    let owner = human("lead-owner");
    w.forge
        .work
        .create_issue(
            &Cx::new(&owner.id, PrincipalKind::Human),
            &RepoInfo::from(&w.repo),
            &Actor::staff(owner.clone()),
            NewIssue {
                title: title.into(),
                ..Default::default()
            },
        )
        .await
        .unwrap()
}

async fn close_produced_pull_request(w: &World, run: &AgentRun) -> ChangeRequest {
    let run = w.run(&run.id).await;
    assert_eq!(run.state(), AgentRunState::WaitingForReview);
    assert_eq!(run.change_requests.len(), 1);
    let cx = Cx::new("lead-owner", PrincipalKind::Human);
    let cr = w
        .changes
        .get_by_number(&cx, &w.repo, run.change_requests[0].parse().unwrap())
        .await
        .unwrap();
    w.changes
        .close(&cx, &w.repo, &cr.id, "Superseded")
        .await
        .unwrap();
    w.forge.flush().await.unwrap();
    assert_eq!(w.run(&run.id).await.state(), AgentRunState::Failed);
    cr
}

#[tokio::test]
async fn repo_work_links_pull_requests_opened_with_the_issued_run_credential() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(T0));
    let store = SqlStore::open(SqlConfig::sqlite(dir.path()), clock.clone())
        .await
        .unwrap();
    let w = World::new(Arc::new(store), clock).await;
    let run = w.start_run(None).await;
    let changes = ChangeServices::new(w.forge.clone(), w.changes.clone());
    let vcs = w.changes.vcs();
    let base = vcs.ref_tip(&w.repo, "main").await.unwrap().unwrap().target;
    let mut opened = Vec::new();
    for (token, path) in [
        (w.run_token(&run.id), "api.md"),
        (w.forged_lead_session(&run.session), "other.md"),
    ] {
        let head = vcs
            .create_commit(
                &w.repo,
                NewCommit {
                    parent: base.clone(),
                    edits: vec![(path.into(), Some(b"Public API reference.\n".to_vec()))],
                    message: format!("Document {path}"),
                    author: "lead-agent".into(),
                    creator: "lead-agent".into(),
                    chain: vec!["lead-agent".into(), "lead-owner".into()],
                    change_id: None,
                },
            )
            .await
            .unwrap();
        let change = body(
            changes
                .open_change(req(
                    &token,
                    OpenChangeRequest {
                        repo: repo_ref(&w.repo),
                        target_ref: "main".into(),
                        head: Some(head),
                        title: format!("Document {path}"),
                        ..Default::default()
                    },
                ))
                .await,
        );
        opened.push(change.id);
    }
    assert_ne!(opened[0], opened[1]);

    assert_eq!(w.agents.dispatch_once().await.unwrap(), 1);
    let run = w.run(&run.id).await;
    assert_eq!(run.state(), AgentRunState::Succeeded);
    assert!(run.change_requests.is_empty());
    let work = body(
        w.api
            .list_repo_work(req(
                &w.token("lead-owner"),
                ListRepoWorkRequest {
                    repo: repo_ref(&w.repo),
                    page: None,
                },
            ))
            .await,
    );
    assert_eq!(work.items.len(), 1);
    let item = &work.items[0];
    assert!(matches!(&item.work, Some(repo_work_item::Work::Run(item)) if item.id == run.id));
    assert_eq!(item.change_request_ids, [opened[0].clone()]);
}
