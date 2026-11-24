//! Agents and workspaces end to end over real history (`HistoryVcs` on in-memory VCS stores)
//! with the deterministic `FakeRunner` and the directory-backed `LocalProvider`: the store on
//! every backend (memory, SQLite, Postgres when `BEE_TEST_PG` is set), the service
//! permission matrix, issue assignment to an agent through to a landed CR, mention triggers,
//! watcher debounce, budget refusals, workspace lifecycle, and the scenarios AS-AGENT-01/02
//! and AS-ISSUE-02 at the domain level.

use crate::common;
use std::sync::Arc;

use bee_auth::{
    AgentInfo, InMemoryRevocations, KeyRing, MintRequest, PrincipalRef, Scope, SigningKey, Verifier,
};
use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::agents::fake::{FakeRunner, FakeScript};
use bee_forge::agents::local::LocalProvider;
use bee_forge::agents::service::AgentApi;
use bee_forge::agents::workspaces::NewWorkspace;
use bee_forge::agents::{
    ActivityFilter, AgentDefinition, AgentPorts, Agents, AgentsConfig, KeyRingIssuer, MemoryBlobs,
    OwnerBudget, Schedule,
};
use bee_forge::changes::vcs::NewCommit;
use bee_forge::changes::{ChangeHooks, ChangeVcs, Changes, HistoryVcs, MemoryTeams};
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner};
use bee_forge::ids::{Clock, ManualClock, SystemClock};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::service::ForgeServices;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::work::{Actor, NewIssue, RepoInfo, TextEffects};
use bee_policy::Action;
use bee_proto::v1::agent_service_server::AgentService;
use bee_proto::v1::issue_service_server::IssueService;
use bee_proto::v1::workspace_service_server::WorkspaceService;
use bee_proto::v1::*;
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};
use tokio_stream::StreamExt;

use common::{reason, req};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_workspace_creates_respect_the_owner_limit_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_workspace_quota_race_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let left = world_on(first).await;
    let right = world_on(second).await;
    for w in [&left, &right] {
        w.agents
            .workspaces
            .set_org_policy(
                &cx("owner"),
                &w.org.id,
                bee_forge::agents::OrgWorkspacePolicy {
                    max_workspaces_per_owner: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }

    for i in 0..24 {
        let owner_id = format!("race-owner-{i}");
        let owner = Principal {
            id: owner_id.clone(),
            kind: PrincipalKind::Human as i32,
            display: owner_id.clone(),
            ..Default::default()
        };
        let caller = Cx::new(&owner_id, PrincipalKind::Human);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left_workspaces = left.agents.workspaces.clone();
        let right_workspaces = right.agents.workspaces.clone();
        let left_repo = left.repo.clone();
        let right_repo = right.repo.clone();
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let left_owner = owner.clone();
        let right_owner = owner.clone();
        let left_caller = caller.clone();
        let right_caller = caller.clone();
        let a = tokio::spawn(async move {
            left_barrier.wait().await;
            left_workspaces
                .create(
                    &left_caller,
                    &left_repo,
                    &left_owner,
                    NewWorkspace {
                        name: format!("left-{i}"),
                        ..Default::default()
                    },
                )
                .await
        });
        let b = tokio::spawn(async move {
            right_barrier.wait().await;
            right_workspaces
                .create(
                    &right_caller,
                    &right_repo,
                    &right_owner,
                    NewWorkspace {
                        name: format!("right-{i}"),
                        ..Default::default()
                    },
                )
                .await
        });
        let a = a.await.unwrap();
        let b = b.await.unwrap();
        assert!(
            a.is_ok() ^ b.is_ok(),
            "exactly one create must succeed in round {i}: {a:?} {b:?}"
        );
        let loser = a.as_ref().err().or(b.as_ref().err()).unwrap();
        assert_eq!(loser.reason, ErrorReason::QuotaExceeded);
        assert_eq!(loser.subcode, "workspace_quota");
    }
}

const T0: u64 = 1_760_000_000_000;

const PEOPLE: &[(&str, PrincipalKind, &str)] = &[
    ("owner", PrincipalKind::Human, ""),
    ("admin", PrincipalKind::Human, ""),
    ("writer", PrincipalKind::Human, ""),
    ("reader", PrincipalKind::Human, ""),
    ("outsider", PrincipalKind::Human, ""),
    ("bot", PrincipalKind::Agent, "writer"),
    ("bot2", PrincipalKind::Agent, "writer"),
    ("critic", PrincipalKind::Agent, "owner"),
    ("sentry", PrincipalKind::Agent, "admin"),
];

fn ring() -> KeyRing {
    KeyRing::new(SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap())
}

/// A repo policy the test can switch (none by default: built-in roles decide).
#[derive(Default)]
struct TestPolicy(std::sync::RwLock<Option<String>>);

#[async_trait::async_trait]
impl bee_forge::hooks::PolicySource for TestPolicy {
    async fn policy(&self, _repo: &Repo) -> bee_forge::Result<Option<bee_policy::Policy>> {
        let text = self.0.read().unwrap().clone();
        Ok(text.map(|t| bee_policy::Policy::parse(&t).unwrap()))
    }
}

pub(super) struct W {
    policy: Arc<TestPolicy>,
    pub(super) forge: Arc<Forge>,
    changes: Arc<Changes>,
    pub(super) vcs: Arc<HistoryVcs>,
    pub(super) agents: Arc<Agents>,
    pub(super) runner: Arc<FakeRunner>,
    ring: KeyRing,
    pub(super) revocations: Arc<InMemoryRevocations>,
    pub(super) clock: Arc<ManualClock>,
    api: AgentApi,
    issues: ForgeServices,
    pub(super) repo: Repo,
    org: Org,
    _dir: tempfile::TempDir,
    ws_root: std::path::PathBuf,
}

fn principal(id: &str) -> Principal {
    let (_, k, o) = PEOPLE
        .iter()
        .find(|(p, _, _)| *p == id)
        .copied()
        .unwrap_or((id, PrincipalKind::Human, ""));
    Principal {
        id: id.into(),
        kind: k as i32,
        display: id.into(),
        owner_id: o.into(),
        runtime: if k == PrincipalKind::Agent {
            "claude-code".into()
        } else {
            String::new()
        },
        ..Default::default()
    }
}

fn cx(id: &str) -> Cx {
    let p = principal(id);
    let c = Cx::new(id, PrincipalKind::try_from(p.kind).unwrap());
    if p.owner_id.is_empty() {
        c
    } else {
        c.with_chain(vec![id.into(), p.owner_id])
    }
}

pub(super) async fn world_on(store: Arc<dyn ForgeStore>) -> W {
    let clock = Arc::new(ManualClock::new(T0));
    let dynclock: Arc<dyn Clock> = clock.clone();
    let meta = Arc::new(MemoryMetaStore::new());
    let writer = Writer::new(
        meta.clone(),
        Arc::new(MemoryBlobStore::new(dynclock.clone())),
    );
    let directory = Arc::new(MemoryDirectory::new());
    for (id, _, _) in PEOPLE {
        directory.register(
            principal(id),
            vec![format!("{id}-login")],
            vec![format!("{id}@example.com")],
        );
    }
    let revocations = Arc::new(InMemoryRevocations::new());
    let policy = Arc::new(TestPolicy::default());
    let (forge, _) = Forge::open(
        store,
        dynclock.clone(),
        ring().verifying_keys(),
        revocations.clone(),
        Hooks {
            directory: directory.clone(),
            provisioner: Arc::new(MetaProvisioner::new(meta, dynclock.clone())),
            policy: Some(policy.clone()),
            ..Hooks::default()
        },
        common::test_config(),
    )
    .await
    .unwrap();
    let vcs = Arc::new(HistoryVcs::new(writer, dynclock.clone()));
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
    let dir = tempfile::tempdir().unwrap();
    let ws_root = dir.path().join("ws");
    let agents = Agents::new(
        forge.clone(),
        changes.clone(),
        AgentPorts {
            runner: runner.clone(),
            issuer: Arc::new(KeyRingIssuer::new(
                ring(),
                Some(revocations.clone()),
                dynclock.clone(),
            )),
            blobs: Arc::new(MemoryBlobs::new()),
            repo: vcs.clone(),
            workspaces: Arc::new(LocalProvider::new(ws_root.clone(), vcs.clone())),
        },
        AgentsConfig::default(),
    );
    agents.attach();
    let c = Cx::new("owner", PrincipalKind::Human);
    let name = format!("o{}", forge.db.ids().ulid().unwrap().to_lowercase());
    let org = forge
        .orgs
        .create_org(&c, &name, "", principal("owner"))
        .await
        .unwrap();
    let repo = forge
        .orgs
        .create_repo(
            &c,
            &org.id,
            NewRepo {
                id: forge.db.ids().ulid().unwrap(),
                name: "app".into(),
                ..NewRepo::default()
            },
        )
        .await
        .unwrap();
    forge.hooks.provisioner.create_org(&org).await.unwrap();
    forge
        .hooks
        .provisioner
        .create_repo(&repo, &bee_forge::hooks::RepoInit::default())
        .await
        .unwrap();
    forge
        .orgs
        .set_member(&c, &org.id, principal("admin"), Role::Admin)
        .await
        .unwrap();
    for who in ["writer", "reader", "bot", "bot2", "critic", "sentry"] {
        forge
            .orgs
            .set_member(&c, &org.id, principal(who), Role::Member)
            .await
            .unwrap();
    }
    for who in ["writer", "bot", "bot2", "critic", "sentry"] {
        forge
            .orgs
            .add_collaborator(&c, &repo.id, principal(who), RepoRole::Write, "")
            .await
            .unwrap();
    }
    forge
        .orgs
        .add_collaborator(&c, &repo.id, principal("reader"), RepoRole::Read, "")
        .await
        .unwrap();
    let root = vcs
        .create_commit(
            &repo,
            NewCommit {
                parent: None,
                edits: [
                    ("README.md", "# app\n"),
                    ("AGENTS.md", "Run scripts/check.sh before handing off.\n@docs/style.md\n"),
                    ("docs/style.md", "Short sentences.\n"),
                    ("src/AGENTS.md", "src rules\n"),
                    ("src/CLAUDE.md", "claude src rules\n"),
                    ("src/lib.rs", "fn a() {}\n"),
                    ("src/net/http.rs", "fn get() {}\n"),
                    (".github/copilot-instructions.md", "Be careful.\n"),
                    (".github/instructions/rust.instructions.md", "---\napplyTo: \"src/**\"\n---\nUse clippy.\n"),
                    (".github/agents/reviewer.md", "---\nname: reviewer\n---\nReview strictly.\n"),
                    ("agents/bot.md", "You are bot.\n"),
                    (
                        ".devcontainer/devcontainer.json",
                        "{\n // dev env\n \"image\": \"rust:1\",\n \"forwardPorts\": [8080],\n \"portsAttributes\": {\"8080\": {\"label\": \"web\"}},\n \"hostRequirements\": {\"cpus\": 4},\n}\n",
                    ),
                ]
                .iter()
                .map(|(p, c)| (p.to_string(), Some(c.as_bytes().to_vec())))
                .collect(),
                message: "root".into(),
                author: "owner".into(),
                creator: "owner".into(),
                chain: vec!["owner".into()],
                change_id: None,
            },
        )
        .await
        .unwrap();
    vcs.set_ref(&repo, "main", &root).await.unwrap();
    for (agent, owner, toolsets) in [
        (
            "bot",
            "writer",
            vec!["read", "commit", "lease", "review", "land"],
        ),
        ("bot2", "writer", vec!["read", "commit", "lease", "land"]),
        ("critic", "owner", vec!["read", "review"]),
        ("sentry", "admin", vec!["read", "commit", "triage"]),
    ] {
        agents
            .define_agent(
                &cx(owner),
                AgentDefinition {
                    org_id: org.id.clone(),
                    agent: principal(agent),
                    name: agent.into(),
                    runtime: "claude-code".into(),
                    model: "claude-sonnet".into(),
                    instructions_path: if agent == "bot" {
                        "agents/bot.md".into()
                    } else {
                        String::new()
                    },
                    toolsets: toolsets.into_iter().map(String::from).collect(),
                    default_budget: AgentRunBudget {
                        max_changesets: 5,
                        max_usd_micros: 5_000_000,
                        ..Default::default()
                    },
                    owners: vec![owner.into()],
                    purpose: format!("{agent} tests"),
                    enabled: true,
                    ..AgentDefinition::default()
                },
            )
            .await
            .unwrap();
    }
    W {
        policy,
        api: AgentApi::new(agents.clone()),
        issues: ForgeServices::new(forge.clone()),
        forge,
        changes,
        vcs,
        agents,
        runner,
        ring: ring(),
        revocations,
        clock,
        repo,
        org,
        _dir: dir,
        ws_root,
    }
}

async fn world() -> W {
    world_on(Arc::new(MemoryStore::new())).await
}

fn rref(r: &Repo) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(r.id.clone())),
    })
}

impl W {
    fn now(&self) -> u64 {
        self.clock.now_ms() / 1000
    }

    fn token(&self, id: &str) -> String {
        self.token_scoped(id, Scope::new())
    }

    fn token_scoped(&self, id: &str, scope: Scope) -> String {
        let p = principal(id);
        let (principal, obo) = if p.kind == PrincipalKind::Agent as i32 {
            (
                bee_auth::Principal::agent(
                    id,
                    id,
                    AgentInfo {
                        owner: bee_policy::PrincipalId::new(p.owner_id.clone()),
                        runtime: "claude-code".into(),
                        purpose: "t".into(),
                    },
                ),
                vec![PrincipalRef::new(
                    p.owner_id.clone(),
                    bee_policy::PrincipalKind::Human,
                )],
            )
        } else {
            (bee_auth::Principal::human(id, id), vec![])
        };
        self.ring
            .mint(
                &MintRequest {
                    principal,
                    on_behalf_of: obo,
                    scope,
                },
                self.now(),
            )
            .unwrap()
            .text
    }

    async fn issue(&self, title: &str) -> Issue {
        self.forge
            .work
            .create_issue(
                &cx("reader"),
                &RepoInfo::from(&self.repo),
                &Actor::staff(principal("reader")),
                NewIssue {
                    title: title.into(),
                    body: format!("{title} body"),
                    ..NewIssue::default()
                },
            )
            .await
            .unwrap()
    }

    async fn settle(&self) {
        for _ in 0..4 {
            self.forge.flush().await.unwrap();
            self.agents.dispatch_once().await.unwrap();
        }
        self.forge.flush().await.unwrap();
    }

    async fn run(&self, id: &str) -> AgentRun {
        self.agents.get_run(&Cx::system(), id).await.unwrap()
    }

    async fn runs_of(&self, agent: &str) -> Vec<AgentRun> {
        self.agents
            .list_runs(
                &Cx::system(),
                std::slice::from_ref(&self.repo.id),
                agent,
                &[],
                0,
                "",
                "",
                1000,
            )
            .await
            .unwrap()
            .0
    }

    async fn kinds(&self) -> Vec<String> {
        self.forge.flush().await.unwrap();
        let (events, _) = self
            .forge
            .events
            .read(&Cx::system(), &self.repo.id, 0, &[], 10_000)
            .await
            .unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }

    async fn audit_actions(&self) -> Vec<String> {
        self.forge.flush().await.unwrap();
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let (entries, next) = self
                .forge
                .audit
                .query(
                    &Cx::system(),
                    &format!("org:{}", self.org.id),
                    &AuditLogFilter::default(),
                    true,
                    &token,
                    500,
                )
                .await
                .unwrap();
            out.extend(
                entries
                    .into_iter()
                    .map(|e| format!("{}:{}", e.action, e.result)),
            );
            if next.is_empty() {
                return out;
            }
            token = next;
        }
    }

    async fn land(&self, who: &str, number: u64) -> ChangeRequest {
        let cr = self
            .changes
            .get_by_number(&cx(who), &self.repo, number)
            .await
            .unwrap();
        self.changes
            .land(&cx(who), &self.repo, &cr.id, LandMethod::Unspecified, "", 0)
            .await
            .unwrap()
            .0
    }
}

// ----- AS-ISSUE-02: assignment → run → CR → land → issue closed -------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_issue_02_assignment_runs_with_attenuated_token_opens_cr_and_closes_on_land() {
    let w = world().await;
    let issue = w.issue("Crash on login").await;
    let res = w
        .issues
        .add_issue_assignees(req(
            &w.token("writer"),
            AddIssueAssigneesRequest {
                repo: rref(&w.repo),
                number: issue.number,
                assignees: vec!["bot".into()],
                agent_instructions: "keep the diff small".into(),
                base_ref: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(res.agent_runs.len(), 1);
    let run_id = res.agent_runs[0].agent_run_id.clone();
    w.forge.flush().await.unwrap();
    // Queued with the id the issues module assigned.
    let run = w.run(&run_id).await;
    assert_eq!(run.state, AgentRunState::Queued as i32);
    assert_eq!(run.task.as_ref().unwrap().issue, issue.number.to_string());
    assert_eq!(run.agent.as_ref().unwrap().id, "bot");
    assert_eq!(run.started_by.as_ref().unwrap().id, "writer");
    w.settle().await;
    let job = w
        .runner
        .jobs()
        .into_iter()
        .find(|j| j.run.id == run_id)
        .unwrap();
    // The token: the agent, on behalf of its owner (the assigning human), limited to the repo,
    // the definition's toolsets, the budget and the session.
    let vt = Verifier::new(&w.ring.verifying_keys(), w.revocations.as_ref())
        .verify(&job.token, w.now())
        .unwrap();
    assert_eq!(vt.principal.id.as_str(), "bot");
    assert_eq!(
        vt.chain()
            .iter()
            .map(|p| p.id.as_str().to_string())
            .collect::<Vec<_>>(),
        vec!["bot", "writer"]
    );
    assert_eq!(vt.delegation_root().id.as_str(), "writer");
    assert!(vt.allows_action(Action::Commit));
    assert!(!vt.allows_action(Action::ManageSettings));
    assert!(
        vt.authorize(
            &bee_auth::Request::new(w.repo.id.clone(), Action::Read),
            w.now()
        )
        .is_ok()
    );
    assert!(
        vt.authorize(&bee_auth::Request::new("other-repo", Action::Read), w.now())
            .is_err()
    );
    assert_eq!(vt.scope.budget.unwrap().max_changesets, Some(5));
    assert_eq!(vt.session(), Some(job.run.session.as_str()));
    assert!(job.instructions.contains("You are bot."));
    assert!(job.instructions.contains("Short sentences."));
    assert!(job.prompt.contains("keep the diff small"));
    // The run opened a CR and waits for review; the issue is linked to it.
    let run = w.run(&run_id).await;
    assert_eq!(run.state, AgentRunState::WaitingForReview as i32, "{run:?}");
    assert_eq!(run.change_requests.len(), 1);
    assert_eq!(run.changesets.len(), 1);
    assert!(run.cost.unwrap().usd_micros > 0);
    let number: u64 = run.change_requests[0].parse().unwrap();
    let dev = w
        .forge
        .work
        .get_development(&cx("reader"), &RepoInfo::from(&w.repo), issue.number)
        .await
        .unwrap();
    assert_eq!(dev.changes.len(), 1);
    assert!(dev.changes[0].closes);
    // Provenance: the session is queryable by the changeset and by the CR.
    let cr = w
        .changes
        .get_by_number(&cx("reader"), &w.repo, number)
        .await
        .unwrap();
    let s = w
        .agents
        .sessions_for_change(&Cx::system(), &w.repo.id, Some(&run.changesets[0]), None)
        .await
        .unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].run_id, run_id);
    assert_eq!(s[0].model, "claude-sonnet");
    assert_eq!(s[0].chain, vec!["bot".to_string(), "writer".to_string()]);
    assert_eq!(s[0].prompt_digest.len(), 32);
    let s2 = w
        .agents
        .sessions_for_change(&Cx::system(), &w.repo.id, None, Some(&cr.id))
        .await
        .unwrap();
    assert_eq!(s2[0].session, s[0].session);
    // Landing closes the issue and finishes the run; the token is revoked.
    w.land("bot", number).await;
    w.settle().await;
    let done = w
        .forge
        .work
        .get_issue_by_number(&cx("reader"), &RepoInfo::from(&w.repo), issue.number)
        .await
        .unwrap();
    assert_eq!(done.state, IssueState::Closed as i32);
    assert!(done.active_agent_run_ids.is_empty());
    let run = w.run(&run_id).await;
    assert_eq!(run.state, AgentRunState::Succeeded as i32);
    assert!(
        Verifier::new(&w.ring.verifying_keys(), w.revocations.as_ref())
            .verify(&job.token, w.now())
            .is_err()
    );
    let kinds = w.kinds().await;
    for k in [
        "agent_run.queued",
        "agent_run.started",
        "agent_run.state_changed",
        "agent_run.completed",
        "cr.opened",
        "cr.landed",
    ] {
        assert!(kinds.contains(&k.to_string()), "{k} in {kinds:?}");
    }
    let audit = w.audit_actions().await;
    for a in [
        "agent.run_start:ok",
        "agent.run_running:ok",
        "agent.run_finish:ok",
    ] {
        assert!(audit.iter().any(|x| x == a), "{a} in {audit:?}");
    }
    // Session events tell the story.
    let ev = w
        .agents
        .run_events(&Cx::system(), &run_id, 0, &[], 1000)
        .await
        .unwrap();
    let kinds: Vec<i32> = ev.iter().map(|e| e.kind).collect();
    for k in [
        AgentRunEventKind::StateChanged,
        AgentRunEventKind::LeaseAcquired,
        AgentRunEventKind::ChangesetCreated,
        AgentRunEventKind::ChangeRequestOpened,
        AgentRunEventKind::CostUpdate,
    ] {
        assert!(kinds.contains(&(k as i32)), "{k:?}");
    }
    assert!(ev.windows(2).all(|p| p[0].seq < p[1].seq));
    let transcript = ev.iter().find(|e| e.tool_name == "transcript").unwrap();
    let v: serde_json::Value = serde_json::from_str(&transcript.tool_payload).unwrap();
    assert!(v["transcript_ref"].as_str().unwrap().starts_with("blake3:"));
}

// ----- helpers for the remaining tests ---------------------------------------------------------

impl W {
    async fn open_cr(&self, who: &str, path: &str, title: &str) -> ChangeRequest {
        let tip = self
            .vcs
            .ref_tip(&self.repo, "main")
            .await
            .unwrap()
            .unwrap()
            .target
            .unwrap();
        let head = self
            .vcs
            .create_commit(
                &self.repo,
                NewCommit {
                    parent: Some(tip),
                    edits: vec![(path.to_string(), Some(format!("{title}\n").into_bytes()))],
                    message: title.into(),
                    author: who.into(),
                    creator: who.into(),
                    chain: vec![who.into()],
                    change_id: None,
                },
            )
            .await
            .unwrap();
        self.changes
            .open(
                &cx(who),
                &self.repo,
                bee_forge::changes::NewChange {
                    head: Some(head),
                    title: title.into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    }

    async fn start(&self, who: &str, agent: &str, prompt: &str) -> Result<AgentRun, tonic::Status> {
        self.api
            .start_agent_run(req(
                &self.token(who),
                StartAgentRunRequest {
                    repo: rref(&self.repo),
                    task: Some(AgentTask {
                        kind: AgentTaskKind::Freeform as i32,
                        prompt: prompt.into(),
                        ..Default::default()
                    }),
                    agent_principal_id: agent.into(),
                    ..Default::default()
                },
            ))
            .await
            .map(|r| r.into_inner())
    }
}

fn ok<T>(r: Result<tonic::Response<T>, tonic::Status>) -> T {
    match r {
        Ok(r) => r.into_inner(),
        Err(s) => panic!("expected success: {s:?}"),
    }
}

// ----- service permission matrix ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_permission_matrix() {
    let w = world().await;
    let rt = |who: &str| {
        req(
            &w.token(who),
            RegisterAgentRuntimeRequest {
                org: w.org.id.clone(),
                name: format!("codex-{who}"),
                vendor: "openai".into(),
                version: "1".into(),
                capabilities: vec!["code".into()],
                ..Default::default()
            },
        )
    };
    // Runtimes: org admins only; members read; outsiders see nothing.
    let codex = ok(w.api.register_agent_runtime(rt("admin")).await);
    assert_eq!(
        reason(w.api.register_agent_runtime(rt("writer")).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(w.api.register_agent_runtime(rt("outsider")).await),
        "PERMISSION_DENIED"
    );
    let dup = req(
        &w.token("admin"),
        RegisterAgentRuntimeRequest {
            org: w.org.id.clone(),
            name: "codex-admin".into(),
            version: "1".into(),
            ..Default::default()
        },
    );
    assert_eq!(
        reason(w.api.register_agent_runtime(dup).await),
        "ALREADY_EXISTS"
    );
    ok(w.api
        .get_agent_runtime(req(
            &w.token("reader"),
            GetAgentRuntimeRequest {
                id: codex.id.clone(),
            },
        ))
        .await);
    assert_eq!(
        reason(
            w.api
                .get_agent_runtime(req(
                    &w.token("outsider"),
                    GetAgentRuntimeRequest {
                        id: codex.id.clone()
                    }
                ))
                .await
        ),
        "NOT_FOUND"
    );
    let list = ok(w
        .api
        .list_agent_runtimes(req(
            &w.token("reader"),
            ListAgentRuntimesRequest {
                org: w.org.id.clone(),
                capability: "code".into(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(list.runtimes.len(), 1);
    // Runs: the owner of the agent with `commit`; not readers, not other people's agents.
    let run = w.start("writer", "bot", "tidy docs").await.unwrap();
    assert_eq!(
        w.start("reader", "bot", "x")
            .await
            .map(|_| ())
            .map_err(|s| s.code()),
        Err(tonic::Code::PermissionDenied)
    );
    assert_eq!(
        w.start("outsider", "bot", "x")
            .await
            .map(|_| ())
            .map_err(|s| s.code()),
        Err(tonic::Code::NotFound)
    );
    assert_eq!(
        w.start("writer", "critic", "x")
            .await
            .map(|_| ())
            .map_err(|s| s.code()),
        Err(tonic::Code::PermissionDenied)
    );
    // A repo admin may drive any agent of the org.
    w.start("admin", "critic", "x")
        .await
        .map(|_| ())
        .unwrap_or_else(|s| {
            // critic has no commit toolset: the scope check refuses, not the role check.
            assert_eq!(
                bee_proto::errors::detail_from_status(&s).unwrap().subcode,
                "scope_too_narrow"
            );
        });
    // A token limited to `read` cannot start runs.
    let ro = w.token_scoped("writer", Scope::new().actions([Action::Read]));
    let r = w
        .api
        .start_agent_run(req(
            &ro,
            StartAgentRunRequest {
                repo: rref(&w.repo),
                task: Some(AgentTask {
                    kind: AgentTaskKind::Freeform as i32,
                    prompt: "x".into(),
                    ..Default::default()
                }),
                agent_principal_id: "bot".into(),
                ..Default::default()
            },
        ))
        .await;
    assert!(r.is_err());
    ok(w.api
        .get_agent_run(req(
            &w.token("reader"),
            GetAgentRunRequest { id: run.id.clone() },
        ))
        .await);
    assert_eq!(
        reason(
            w.api
                .get_agent_run(req(
                    &w.token("outsider"),
                    GetAgentRunRequest { id: run.id.clone() }
                ))
                .await
        ),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(
            w.api
                .cancel_agent_run(req(
                    &w.token("reader"),
                    CancelAgentRunRequest {
                        id: run.id.clone(),
                        reason: String::new()
                    }
                ))
                .await
        ),
        "PERMISSION_DENIED"
    );
    // Watchers and review configuration need a maintainer.
    let wreq = |who: &str| {
        req(
            &w.token(who),
            CreateComponentWatcherRequest {
                repo: rref(&w.repo),
                name: format!("net-{who}"),
                component: "net".into(),
                paths: vec![b"src/net".to_vec()],
                ..Default::default()
            },
        )
    };
    assert_eq!(
        reason(w.api.create_component_watcher(wreq("writer")).await),
        "PERMISSION_DENIED"
    );
    let watcher = ok(w.api.create_component_watcher(wreq("admin")).await);
    assert_eq!(watcher.agent.unwrap().id, "sentry");
    ok(w.api
        .list_component_watchers(req(
            &w.token("reader"),
            ListComponentWatchersRequest {
                repo: rref(&w.repo),
                ..Default::default()
            },
        ))
        .await);
    let rc = |who: &str| {
        req(
            &w.token(who),
            SetAgentReviewConfigRequest {
                repo: rref(&w.repo),
                trigger: Some(AgentReviewTrigger::OnOpen as i32),
                agent_principal_id: Some("critic".into()),
                ..Default::default()
            },
        )
    };
    assert_eq!(
        reason(w.api.set_agent_review_config(rc("writer")).await),
        "PERMISSION_DENIED"
    );
    ok(w.api.set_agent_review_config(rc("admin")).await);
    // Memory: repo writers write, readers read; agent memory is the agent's and its owners'.
    let put = |who: &str, scope: agent_memory_scope::Scope| {
        req(
            &w.token(who),
            PutAgentMemoryRequest {
                scope: Some(AgentMemoryScope { scope: Some(scope) }),
                key: "k".into(),
                value: "v".into(),
                ..Default::default()
            },
        )
    };
    let repo_scope = || {
        agent_memory_scope::Scope::Repo(RepoRef {
            repo: Some(repo_ref::Repo::Id(w.repo.id.clone())),
        })
    };
    ok(w.api.put_agent_memory(put("writer", repo_scope())).await);
    assert_eq!(
        reason(w.api.put_agent_memory(put("reader", repo_scope())).await),
        "PERMISSION_DENIED"
    );
    ok(w.api
        .put_agent_memory(put(
            "writer",
            agent_memory_scope::Scope::AgentPrincipalId("bot".into()),
        ))
        .await);
    ok(w.api
        .put_agent_memory(put(
            "bot",
            agent_memory_scope::Scope::AgentPrincipalId("bot".into()),
        ))
        .await);
    assert_eq!(
        reason(
            w.api
                .put_agent_memory(put(
                    "reader",
                    agent_memory_scope::Scope::AgentPrincipalId("bot".into())
                ))
                .await
        ),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(
            w.api
                .put_agent_memory(put(
                    "writer",
                    agent_memory_scope::Scope::Org(w.org.id.clone())
                ))
                .await
        ),
        "PERMISSION_DENIED"
    );
    ok(w.api
        .put_agent_memory(put(
            "admin",
            agent_memory_scope::Scope::Org(w.org.id.clone()),
        ))
        .await);
    // Workspaces: the owner and maintainers; others get NOT_FOUND.
    let ws = ok(w
        .api
        .create_workspace(req(
            &w.token("writer"),
            CreateWorkspaceRequest {
                repo: rref(&w.repo),
                name: "perm".into(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(
        reason(
            w.api
                .get_workspace(req(
                    &w.token("reader"),
                    GetWorkspaceRequest { id: ws.id.clone() }
                ))
                .await
        ),
        "NOT_FOUND"
    );
    ok(w.api
        .get_workspace(req(
            &w.token("admin"),
            GetWorkspaceRequest { id: ws.id.clone() },
        ))
        .await);
    assert_eq!(
        reason(
            w.api
                .stop_workspace(req(
                    &w.token("reader"),
                    StopWorkspaceRequest { id: ws.id.clone() }
                ))
                .await
        ),
        "PERMISSION_DENIED"
    );
    // An agent-owned workspace needs the caller to own the agent.
    let mk = |who: &str, owner: &str| {
        req(
            &w.token(who),
            CreateWorkspaceRequest {
                repo: rref(&w.repo),
                name: format!("{who}-{owner}"),
                owner_principal_id: owner.into(),
                ..Default::default()
            },
        )
    };
    ok(w.api.create_workspace(mk("writer", "bot")).await);
    assert_eq!(
        reason(w.api.create_workspace(mk("writer", "critic")).await),
        "PERMISSION_DENIED"
    );
    // Egress at org scope: org admins.
    let eg = |who: &str| {
        req(
            &w.token(who),
            SetWorkspaceEgressPolicyRequest {
                scope: Some(WorkspaceEgressScope {
                    scope: Some(workspace_egress_scope::Scope::Org(w.org.id.clone())),
                }),
                default_action: WorkspaceEgressDefault::Deny as i32,
                allowed_domains: vec!["*.crates.io".into()],
                ..Default::default()
            },
        )
    };
    assert_eq!(
        reason(w.api.set_workspace_egress_policy(eg("writer")).await),
        "PERMISSION_DENIED"
    );
    ok(w.api.set_workspace_egress_policy(eg("admin")).await);
    // Usage: one's own, or org admins.
    ok(w.api
        .get_workspace_usage(req(
            &w.token("writer"),
            GetWorkspaceUsageRequest {
                org: w.org.id.clone(),
                principal_id: "writer".into(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(
        reason(
            w.api
                .get_workspace_usage(req(
                    &w.token("writer"),
                    GetWorkspaceUsageRequest {
                        org: w.org.id.clone(),
                        ..Default::default()
                    }
                ))
                .await
        ),
        "PERMISSION_DENIED"
    );
    // Denied state changes are audited.
    let audit = w.audit_actions().await;
    for a in [
        "agent.runtime_register:PERMISSION_DENIED",
        "agent.run_start:PERMISSION_DENIED",
        "agent.watcher_create:PERMISSION_DENIED",
        "agent.review_config_set:PERMISSION_DENIED",
        "workspace.stop:PERMISSION_DENIED",
        "workspace.egress_set:PERMISSION_DENIED",
    ] {
        assert!(audit.iter().any(|x| x == a), "{a} in {audit:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_runs_derive_their_token_from_the_callers_token() {
    let w = world().await;
    // The caller's token is limited to docs/: the run token is derived from it.
    let narrow = w.token_scoped("writer", Scope::new().write_paths(["/docs"]).unwrap());
    let run = ok(w
        .api
        .start_agent_run(req(
            &narrow,
            StartAgentRunRequest {
                repo: rref(&w.repo),
                task: Some(AgentTask {
                    kind: AgentTaskKind::Freeform as i32,
                    prompt: "docs".into(),
                    ..Default::default()
                }),
                agent_principal_id: "bot".into(),
                scope: Some(AgentRunScope {
                    actions: vec!["read".into(), "commit".into()],
                    ..Default::default()
                }),
                budget: Some(AgentRunBudget {
                    max_changesets: 99,
                    max_usd_micros: 100,
                    ..Default::default()
                }),
                idempotency_key: "k1".into(),
                ..Default::default()
            },
        ))
        .await);
    // Same key, same run.
    let again = ok(w
        .api
        .start_agent_run(req(
            &narrow,
            StartAgentRunRequest {
                repo: rref(&w.repo),
                task: Some(AgentTask {
                    kind: AgentTaskKind::Freeform as i32,
                    prompt: "docs".into(),
                    ..Default::default()
                }),
                agent_principal_id: "bot".into(),
                idempotency_key: "k1".into(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(again.id, run.id);
    // The budget is clamped to the definition's default (5 changesets).
    assert_eq!(run.budget.unwrap().max_changesets, 5);
    assert_eq!(run.budget.unwrap().max_usd_micros, 100);
    // Asking for a toolset the agent lacks is refused.
    let wide = w
        .api
        .start_agent_run(req(
            &w.token("writer"),
            StartAgentRunRequest {
                repo: rref(&w.repo),
                task: Some(AgentTask {
                    kind: AgentTaskKind::Freeform as i32,
                    prompt: "x".into(),
                    ..Default::default()
                }),
                agent_principal_id: "bot2".into(),
                scope: Some(AgentRunScope {
                    actions: vec!["commit".into(), "manage_settings".into()],
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(
        bee_proto::errors::detail_from_status(&wide.unwrap_err())
            .unwrap()
            .subcode,
        "scope_too_broad"
    );
    w.runner.script(
        "bot",
        FakeScript {
            paths: vec!["docs/notes.md".into()],
            ..Default::default()
        },
    );
    w.settle().await;
    let job = w
        .runner
        .jobs()
        .into_iter()
        .find(|j| j.run.id == run.id)
        .unwrap();
    // The run ended, so its token is revoked; inspect its scope with an empty revocation list.
    assert!(
        Verifier::new(&w.ring.verifying_keys(), w.revocations.as_ref())
            .verify(&job.token, w.now())
            .is_err()
    );
    let none = InMemoryRevocations::new();
    let vt = Verifier::new(&w.ring.verifying_keys(), &none)
        .verify(&job.token, w.now())
        .unwrap();
    assert_eq!(vt.principal.id.as_str(), "bot");
    assert_eq!(vt.delegation_root().id.as_str(), "writer");
    let write = |p: &str| bee_auth::Request::new(w.repo.id.clone(), Action::Commit).paths([p]);
    assert!(vt.authorize(&write("/docs/a.md"), w.now()).is_ok());
    assert!(vt.authorize(&write("/src/a.rs"), w.now()).is_err());
    assert!(!vt.allows_action(Action::Lease));
    // The fake runner's cost exceeded the 100 micro budget: the run failed for budget.
    let done = w.run(&run.id).await;
    assert_eq!(done.state, AgentRunState::Failed as i32);
    assert_eq!(done.failure_reason, "BUDGET_EXHAUSTED");
}

// ----- triggers ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mentions_in_issue_and_cr_comments_start_runs() {
    let w = world().await;
    let issue = w.issue("Flaky test").await;
    w.forge
        .work
        .create_comment(
            &cx("reader"),
            &RepoInfo::from(&w.repo),
            &Actor::staff(principal("reader")),
            issue.number,
            "@bot-login can you take a look? cc @reader-login",
            "",
            false,
            false,
            &TextEffects::default(),
        )
        .await
        .unwrap();
    w.settle().await;
    let runs = w.runs_of("bot").await;
    assert_eq!(runs.len(), 1);
    let r = &runs[0];
    assert_eq!(r.task.as_ref().unwrap().kind, AgentTaskKind::Issue as i32);
    assert_eq!(r.task.as_ref().unwrap().issue, issue.number.to_string());
    assert!(r.task.as_ref().unwrap().prompt.contains("take a look"));
    assert_eq!(r.started_by.as_ref().unwrap().id, "reader");
    // Delivered twice (at least once), started once.
    let (events, _) = w
        .forge
        .events
        .read(
            &Cx::system(),
            &w.repo.id,
            0,
            &["issue_comment.created".into()],
            10,
        )
        .await
        .unwrap();
    w.agents.on_event(&events[0]).await.unwrap();
    assert_eq!(w.runs_of("bot").await.len(), 1);
    // A CR comment mentioning a reviewer agent starts a review.
    let cr = w.open_cr("writer", "src/feature.rs", "Add feature").await;
    w.changes
        .comment(
            &cx("writer"),
            &w.repo,
            &cr.id,
            "@critic-login please review",
            false,
        )
        .await
        .unwrap();
    w.settle().await;
    let reviews = w.runs_of("critic").await;
    assert_eq!(reviews.len(), 1);
    assert_eq!(
        reviews[0].task.as_ref().unwrap().kind,
        AgentTaskKind::Review as i32
    );
    assert_eq!(reviews[0].state, AgentRunState::Succeeded as i32);
    let (items, _) = w
        .agents
        .work_items(
            &Cx::system(),
            "critic",
            std::slice::from_ref(&w.org.id),
            "",
            0,
            "",
            100,
        )
        .await
        .unwrap();
    assert!(
        items.iter().any(|i| i.title == "review: approve"),
        "{items:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_debounce_folds_changes_into_one_run() {
    let w = world().await;
    let watcher = w
        .agents
        .create_watcher(
            &cx("admin"),
            &w.repo,
            bee_forge::agents::watchers::NewWatcher {
                name: "net".into(),
                component: "networking".into(),
                paths: vec![b"src/net".to_vec()],
                agent: principal("sentry"),
                check_interval_seconds: 60,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let cs = ObjectId { raw: vec![7u8; 32] };
    assert_eq!(
        w.agents
            .note_changes(&w.repo, &["src/net/http.rs".into()], &cs)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        w.agents
            .note_changes(&w.repo, &["docs/guide.md".into()], &cs)
            .await
            .unwrap(),
        0
    );
    w.clock.advance(30_000);
    w.agents
        .note_changes(&w.repo, &["src/net/tls.rs".into(), "README.md".into()], &cs)
        .await
        .unwrap();
    w.clock.advance(50_000);
    // 80 s after the first change, 50 s after the last: still inside the window.
    w.agents.dispatch_once().await.unwrap();
    assert!(w.runs_of("sentry").await.is_empty());
    w.clock.advance(11_000);
    w.settle().await;
    let runs = w.runs_of("sentry").await;
    assert_eq!(runs.len(), 1);
    let prompt = &runs[0].task.as_ref().unwrap().prompt;
    assert!(prompt.contains("src/net/http.rs") && prompt.contains("src/net/tls.rs"));
    assert!(!prompt.contains("README.md"));
    let rec = w.agents.get_run(&Cx::system(), &runs[0].id).await.unwrap();
    assert_eq!(rec.agent.unwrap().id, "sentry");
    // Nothing else is pending.
    w.clock.advance(600_000);
    w.settle().await;
    assert_eq!(w.runs_of("sentry").await.len(), 1);
    assert!(w.kinds().await.contains(&"watcher.triggered".to_string()));
    // A landed CR touching the watched paths triggers through the event log.
    let cr = w.open_cr("writer", "src/net/dns.rs", "DNS").await;
    w.land("writer", cr.number).await;
    w.forge.flush().await.unwrap();
    w.clock.advance(61_000);
    w.settle().await;
    assert_eq!(w.runs_of("sentry").await.len(), 2);
    // A paused watcher does not fire.
    w.agents
        .update_watcher(
            &cx("admin"),
            &w.repo,
            &watcher.id,
            bee_forge::agents::watchers::WatcherPatch {
                state: Some(WatcherState::Paused),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        w.agents
            .note_changes(&w.repo, &["src/net/x.rs".into()], &cs)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budget_exhaustion_refuses_runs() {
    let w = world().await;
    w.agents
        .set_owner_budget(
            &cx("owner"),
            &w.org.id,
            "writer",
            OwnerBudget {
                max_runs: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    w.start("writer", "bot", "first").await.unwrap();
    let second = w.start("writer", "bot2", "second").await;
    assert_eq!(reason(second.map(tonic::Response::new)), "BUDGET_EXHAUSTED");
    // Assigning an agent whose owner is out of budget is refused by the gate.
    let issue = w.issue("Needs work").await;
    let r = w
        .issues
        .add_issue_assignees(req(
            &w.token("writer"),
            AddIssueAssigneesRequest {
                repo: rref(&w.repo),
                number: issue.number,
                assignees: vec!["bot".into()],
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(r), "BUDGET_EXHAUSTED");
    let (b, spend) = w
        .agents
        .owner_budget(&Cx::system(), &w.org.id, "writer")
        .await
        .unwrap();
    assert_eq!(b.max_runs, 1);
    assert_eq!(spend.runs, 1);
    assert!(
        w.audit_actions()
            .await
            .iter()
            .any(|a| a == "agent.run_refused:BUDGET_EXHAUSTED")
    );
    // Spend is charged from run costs: an expensive run spends admin's USD budget.
    w.runner.script(
        "sentry",
        FakeScript {
            cost: Some(Cost {
                input_tokens: 10,
                output_tokens: 10,
                usd_micros: 900,
            }),
            ..Default::default()
        },
    );
    w.agents
        .set_owner_budget(
            &cx("owner"),
            &w.org.id,
            "admin",
            OwnerBudget {
                max_usd_micros: 500,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    w.start("admin", "sentry", "expensive").await.unwrap();
    w.settle().await;
    let (_, spend) = w
        .agents
        .owner_budget(&Cx::system(), &w.org.id, "admin")
        .await
        .unwrap();
    assert_eq!(spend.usd_micros, 900);
    assert_eq!(
        reason(
            w.start("admin", "sentry", "again")
                .await
                .map(tonic::Response::new)
        ),
        "BUDGET_EXHAUSTED"
    );
    // Billing sees the tokens.
    assert!(w.kinds().await.contains(&"usage.recorded".to_string()));
}

// ----- workspaces -------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_lifecycle_devcontainer_ports_export_and_expiry() {
    let w = world().await;
    let t = w.token("writer");
    let types = ok(w
        .api
        .list_workspace_machine_types(req(
            &t,
            ListWorkspaceMachineTypesRequest {
                repo: rref(&w.repo),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(types.machine_types.len(), 5);
    let ws = ok(w
        .api
        .create_workspace(req(
            &t,
            CreateWorkspaceRequest {
                repo: rref(&w.repo),
                name: "dev".into(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(ws.state, WorkspaceState::Available as i32);
    // hostRequirements.cpus = 4 picks the cheapest machine with 4 cores.
    assert_eq!(ws.machine_type, "standard-4");
    assert_eq!(ws.workspace_ref, "ws/writer/dev");
    let wc = w.ws_root.join(&ws.id).join("wc");
    assert_eq!(
        std::fs::read_to_string(wc.join("README.md")).unwrap(),
        "# app\n"
    );
    let ports = ok(w
        .api
        .list_workspace_ports(req(
            &t,
            ListWorkspacePortsRequest {
                workspace_id: ws.id.clone(),
            },
        ))
        .await);
    assert_eq!(ports.ports.len(), 1);
    assert_eq!(ports.ports[0].label, "web");
    assert_eq!(
        reason(
            w.api
                .create_workspace(req(
                    &t,
                    CreateWorkspaceRequest {
                        repo: rref(&w.repo),
                        name: "small".into(),
                        machine_type: "basic-2".into(),
                        ..Default::default()
                    }
                ))
                .await
        ),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(
            w.api
                .create_workspace(req(
                    &t,
                    CreateWorkspaceRequest {
                        repo: rref(&w.repo),
                        name: "dev".into(),
                        ..Default::default()
                    }
                ))
                .await
        ),
        "ALREADY_EXISTS"
    );
    // Connect.
    let conn = ok(w
        .api
        .get_workspace_connection(req(&t, GetWorkspaceConnectionRequest { id: ws.id.clone() }))
        .await);
    assert!(!conn.connection_token.is_empty());
    assert!(
        w.agents
            .workspaces
            .verify_connection_token(&Cx::system(), &ws.id, &conn.connection_token)
            .await
            .unwrap()
    );
    assert!(
        !w.agents
            .workspaces
            .verify_connection_token(&Cx::system(), &ws.id, "nope")
            .await
            .unwrap()
    );
    // Ports: public until org policy forbids it.
    let p = ok(w
        .api
        .set_workspace_port_visibility(req(
            &t,
            SetWorkspacePortVisibilityRequest {
                workspace_id: ws.id.clone(),
                port: 8080,
                visibility: WorkspacePortVisibility::Public as i32,
            },
        ))
        .await);
    assert_eq!(p.visibility, WorkspacePortVisibility::Public as i32);
    w.agents
        .workspaces
        .set_org_policy(
            &cx("owner"),
            &w.org.id,
            bee_forge::agents::OrgWorkspacePolicy {
                allow_public_ports: false,
                max_workspaces_per_owner: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        reason(
            w.api
                .set_workspace_port_visibility(req(
                    &t,
                    SetWorkspacePortVisibilityRequest {
                        workspace_id: ws.id.clone(),
                        port: 8080,
                        visibility: WorkspacePortVisibility::Public as i32
                    }
                ))
                .await
        ),
        "POLICY_DENIED"
    );
    // Edit, stop with unexported changes, refuse a plain delete.
    std::fs::write(wc.join("src/new.rs"), "fn new() {}\n").unwrap();
    let stopped = ok(w
        .api
        .stop_workspace(req(&t, StopWorkspaceRequest { id: ws.id.clone() }))
        .await);
    assert_eq!(stopped.state, WorkspaceState::Stopped as i32);
    assert!(stopped.has_unexported_changes);
    assert_eq!(
        reason(
            w.api
                .delete_workspace(req(
                    &t,
                    DeleteWorkspaceRequest {
                        id: ws.id.clone(),
                        force: false
                    }
                ))
                .await
        ),
        "INVALID_STATE"
    );
    assert_eq!(
        reason(
            w.api
                .stop_workspace(req(&t, StopWorkspaceRequest { id: ws.id.clone() }))
                .await
        ),
        "INVALID_STATE"
    );
    // A machine type change applies at the next start.
    ok(w.api
        .update_workspace(req(
            &t,
            UpdateWorkspaceRequest {
                id: ws.id.clone(),
                machine_type: Some("large-8".into()),
                ..Default::default()
            },
        ))
        .await);
    let started = ok(w
        .api
        .start_workspace(req(&t, StartWorkspaceRequest { id: ws.id.clone() }))
        .await);
    assert_eq!(started.state, WorkspaceState::Available as i32);
    assert_eq!(started.machine_type, "large-8");
    // Export onto the workspace ref and open a CR.
    let ex = ok(w
        .api
        .export_workspace_changes(req(
            &t,
            ExportWorkspaceChangesRequest {
                id: ws.id.clone(),
                message: "Add new.rs".into(),
                open_change_request: true,
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(ex.r#ref.as_ref().unwrap().name, "ws/writer/dev");
    let cr = w
        .changes
        .get_by_number(&cx("writer"), &w.repo, ex.change_request.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(cr.title, "Add new.rs");
    let tip = w
        .vcs
        .ref_tip(&w.repo, "ws/writer/dev")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tip.target, ex.changeset);
    assert_eq!(
        w.vcs
            .read_file(&w.repo, ex.changeset.as_ref().unwrap(), "src/new.rs")
            .await
            .unwrap()
            .unwrap(),
        b"fn new() {}\n".to_vec()
    );
    assert_eq!(
        reason(
            w.api
                .export_workspace_changes(req(
                    &t,
                    ExportWorkspaceChangesRequest {
                        id: ws.id.clone(),
                        ..Default::default()
                    }
                ))
                .await
        ),
        "EMPTY_CHANGE"
    );
    // Rebuild keeps the working copy.
    let rb = ok(w
        .api
        .rebuild_workspace(req(
            &t,
            RebuildWorkspaceRequest {
                id: ws.id.clone(),
                full: true,
            },
        ))
        .await);
    assert_eq!(rb.state, WorkspaceState::Available as i32);
    assert!(wc.join("src/new.rs").exists());
    // Idle timeout stops it; usage is recorded; retention deletes it.
    w.clock.advance(31 * 60 * 1000);
    w.agents.maintain().await.unwrap();
    let idle = ok(w
        .api
        .get_workspace(req(&t, GetWorkspaceRequest { id: ws.id.clone() }))
        .await);
    assert_eq!(idle.state, WorkspaceState::Stopped as i32);
    let usage = ok(w
        .api
        .get_workspace_usage(req(
            &w.token("owner"),
            GetWorkspaceUsageRequest {
                org: w.org.id.clone(),
                ..Default::default()
            },
        ))
        .await);
    assert!(usage.total_usd_micros > 0);
    assert!(
        usage
            .lines
            .iter()
            .any(|l| l.machine_type == "large-8" && l.core_seconds > 0)
    );
    w.clock.advance(31 * 86_400 * 1000);
    w.agents.maintain().await.unwrap();
    let t = w.token("writer");
    assert_eq!(
        reason(
            w.api
                .get_workspace(req(&t, GetWorkspaceRequest { id: ws.id.clone() }))
                .await
        ),
        "NOT_FOUND"
    );
    assert!(!w.ws_root.join(&ws.id).exists());
    // Quota from org policy.
    for n in ["a", "b", "c"] {
        ok(w.api
            .create_workspace(req(
                &t,
                CreateWorkspaceRequest {
                    repo: rref(&w.repo),
                    name: n.into(),
                    ..Default::default()
                },
            ))
            .await);
    }
    assert_eq!(
        reason(
            w.api
                .create_workspace(req(
                    &t,
                    CreateWorkspaceRequest {
                        repo: rref(&w.repo),
                        name: "d".into(),
                        ..Default::default()
                    }
                ))
                .await
        ),
        "QUOTA_EXCEEDED"
    );
    let mine = ok(w
        .api
        .list_workspaces(req(&t, ListWorkspacesRequest::default()))
        .await);
    assert_eq!(mine.workspaces.len(), 3);
    let kinds = w.kinds().await;
    for k in [
        "workspace.created",
        "workspace.state_changed",
        "workspace.exported",
        "workspace.deleted",
        "usage.recorded",
    ] {
        assert!(kinds.contains(&k.to_string()), "{k}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prebuilds_dotfiles_and_egress_policy() {
    let w = world().await;
    let admin = w.token("admin");
    let cfg = ok(w
        .api
        .create_workspace_prebuild_config(req(
            &admin,
            CreateWorkspacePrebuildConfigRequest {
                repo: rref(&w.repo),
                trigger: WorkspacePrebuildTrigger::Manual as i32,
                retained_versions: 1,
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(cfg.r#ref, "main");
    assert_eq!(
        reason(
            w.api
                .create_workspace_prebuild_config(req(
                    &w.token("writer"),
                    CreateWorkspacePrebuildConfigRequest {
                        repo: rref(&w.repo),
                        ..Default::default()
                    }
                ))
                .await
        ),
        "PERMISSION_DENIED"
    );
    let p1 = ok(w
        .api
        .trigger_workspace_prebuild(req(
            &admin,
            TriggerWorkspacePrebuildRequest {
                repo: rref(&w.repo),
                config_id: cfg.id.clone(),
            },
        ))
        .await);
    assert_eq!(p1.state, WorkspacePrebuildState::Ready as i32);
    let p2 = ok(w
        .api
        .trigger_workspace_prebuild(req(
            &admin,
            TriggerWorkspacePrebuildRequest {
                repo: rref(&w.repo),
                config_id: cfg.id.clone(),
            },
        ))
        .await);
    let list = ok(w
        .api
        .list_workspace_prebuilds(req(
            &admin,
            ListWorkspacePrebuildsRequest {
                repo: rref(&w.repo),
                config_id: cfg.id.clone(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(list.prebuilds.len(), 2);
    assert_eq!(
        list.prebuilds.iter().find(|p| p.id == p1.id).unwrap().state,
        WorkspacePrebuildState::Superseded as i32
    );
    let types = ok(w
        .api
        .list_workspace_machine_types(req(
            &admin,
            ListWorkspaceMachineTypesRequest {
                repo: rref(&w.repo),
                r#ref: "main".into(),
                ..Default::default()
            },
        ))
        .await);
    assert!(types.machine_types.iter().all(|m| m.prebuild_available));
    let t = w.token("writer");
    let ws = ok(w
        .api
        .create_workspace(req(
            &t,
            CreateWorkspaceRequest {
                repo: rref(&w.repo),
                name: "fast".into(),
                use_prebuild: true,
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(ws.prebuild_id, p2.id);
    // An every-land configuration builds when the ref moves.
    let every = ok(w
        .api
        .create_workspace_prebuild_config(req(
            &admin,
            CreateWorkspacePrebuildConfigRequest {
                repo: rref(&w.repo),
                devcontainer_path: ".devcontainer/devcontainer.json".into(),
                trigger: WorkspacePrebuildTrigger::EveryLand as i32,
                ..Default::default()
            },
        ))
        .await);
    let cr = w.open_cr("writer", "src/x.rs", "x").await;
    w.land("writer", cr.number).await;
    w.forge.flush().await.unwrap();
    w.agents.maintain().await.unwrap();
    let built = ok(w
        .api
        .list_workspace_prebuilds(req(
            &admin,
            ListWorkspacePrebuildsRequest {
                repo: rref(&w.repo),
                config_id: every.id.clone(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(built.prebuilds.len(), 1);
    assert_eq!(
        built.prebuilds[0].state,
        WorkspacePrebuildState::Ready as i32
    );
    // Dotfiles: one's own or an owned agent's.
    let d = ok(w
        .api
        .set_workspace_dotfiles(req(
            &t,
            SetWorkspaceDotfilesRequest {
                principal_id: "bot".into(),
                repo: rref(&w.repo),
                install_command: "./install.sh".into(),
                enabled: true,
            },
        ))
        .await);
    assert_eq!(d.principal_id, "bot");
    assert_eq!(
        ok(w.api
            .get_workspace_dotfiles(req(&w.token("bot"), GetWorkspaceDotfilesRequest::default()))
            .await)
        .install_command,
        "./install.sh"
    );
    assert_eq!(
        reason(
            w.api
                .get_workspace_dotfiles(req(
                    &t,
                    GetWorkspaceDotfilesRequest {
                        principal_id: "critic".into()
                    }
                ))
                .await
        ),
        "PERMISSION_DENIED"
    );
    // Egress: the org denies by default; a narrower scope may only restrict.
    let set = |who: &str, scope: workspace_egress_scope::Scope, deny: bool, allowed: &[&str]| {
        req(
            &w.token(who),
            SetWorkspaceEgressPolicyRequest {
                scope: Some(WorkspaceEgressScope { scope: Some(scope) }),
                default_action: if deny {
                    WorkspaceEgressDefault::Deny
                } else {
                    WorkspaceEgressDefault::Allow
                } as i32,
                allowed_domains: allowed.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        )
    };
    ok(w.api
        .set_workspace_egress_policy(set(
            "admin",
            workspace_egress_scope::Scope::Org(w.org.id.clone()),
            true,
            &["*.crates.io", "github.com"],
        ))
        .await);
    let wsid = workspace_egress_scope::Scope::WorkspaceId(ws.id.clone());
    assert_eq!(
        reason(
            w.api
                .set_workspace_egress_policy(set("writer", wsid.clone(), false, &[]))
                .await
        ),
        "POLICY_DENIED"
    );
    assert_eq!(
        reason(
            w.api
                .set_workspace_egress_policy(set("writer", wsid.clone(), true, &["evil.com"]))
                .await
        ),
        "POLICY_DENIED"
    );
    ok(w.api
        .set_workspace_egress_policy(set("writer", wsid.clone(), true, &["static.crates.io"]))
        .await);
    let got = ok(w
        .api
        .get_workspace_egress_policy(req(
            &t,
            GetWorkspaceEgressPolicyRequest {
                scope: Some(WorkspaceEgressScope { scope: Some(wsid) }),
            },
        ))
        .await);
    let eff = got.effective.unwrap();
    assert_eq!(eff.default_action, WorkspaceEgressDefault::Deny as i32);
    assert_eq!(eff.allowed_domains, vec!["static.crates.io".to_string()]);
}

// ----- agent scenarios --------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_agent_01_overlapping_tasks_route_around_leases_and_both_land() {
    let w = world().await;
    w.runner.script(
        "bot",
        FakeScript {
            paths: vec!["src/shared.rs".into(), "src/a.rs".into()],
            ..Default::default()
        },
    );
    w.runner.script(
        "bot2",
        FakeScript {
            paths: vec!["src/shared.rs".into(), "src/b.rs".into()],
            ..Default::default()
        },
    );
    let i1 = w.issue("Task one").await;
    let i2 = w.issue("Task two").await;
    for (n, agent) in [(i1.number, "bot"), (i2.number, "bot2")] {
        ok(w.issues
            .add_issue_assignees(req(
                &w.token("writer"),
                AddIssueAssigneesRequest {
                    repo: rref(&w.repo),
                    number: n,
                    assignees: vec![agent.into()],
                    ..Default::default()
                },
            ))
            .await);
    }
    w.settle().await;
    let r1 = w.runs_of("bot").await.remove(0);
    let r2 = w.runs_of("bot2").await.remove(0);
    for r in [&r1, &r2] {
        assert_eq!(r.state, AgentRunState::WaitingForReview as i32, "{r:?}");
        assert_eq!(r.lease_ids.len(), 1);
    }
    // Exactly one of them hit the other's lease and routed around it.
    let routed = |id: &str| {
        let agents = w.agents.clone();
        let id = id.to_string();
        async move {
            agents
                .run_events(&Cx::system(), &id, 0, &[AgentRunEventKind::Log as i32], 100)
                .await
                .unwrap()
                .iter()
                .any(|e| e.text.contains("routing around"))
        }
    };
    assert!(routed(&r1.id).await ^ routed(&r2.id).await);
    let leases = w
        .forge
        .leases
        .list(&Cx::system(), &w.repo.id, &[])
        .await
        .unwrap();
    assert_eq!(leases.len(), 2);
    assert!(
        leases
            .iter()
            .any(|l| l.paths == vec![b"src/shared.rs".to_vec()])
    );
    // Both land without a conflict and both issues close.
    for (r, agent) in [(&r1, "bot"), (&r2, "bot2")] {
        w.land(agent, r.change_requests[0].parse().unwrap()).await;
    }
    w.settle().await;
    for n in [i1.number, i2.number] {
        let i = w
            .forge
            .work
            .get_issue_by_number(&cx("reader"), &RepoInfo::from(&w.repo), n)
            .await
            .unwrap();
        assert_eq!(i.state, IssueState::Closed as i32);
    }
    assert_eq!(w.run(&r1.id).await.state, AgentRunState::Succeeded as i32);
    assert_eq!(w.run(&r2.id).await.state, AgentRunState::Succeeded as i32);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_agent_02_watcher_reports_latency_regression_and_opens_issue_with_evidence() {
    let w = world().await;
    let watcher = ok(w
        .api
        .create_component_watcher(req(
            &w.token("admin"),
            CreateComponentWatcherRequest {
                repo: rref(&w.repo),
                name: "checkout".into(),
                component: "checkout service".into(),
                paths: vec![b"src/net".to_vec()],
                service_name: "checkout".into(),
                telemetry: Some(WatcherTelemetrySelector {
                    metric_names: vec!["p99_latency_ms".into()],
                    ..Default::default()
                }),
                auto_open_issues: true,
                ..Default::default()
            },
        ))
        .await);
    // Subscribe to the anomaly feed first.
    let mut stream = ok(w
        .api
        .stream_watcher_anomalies(req(
            &w.token("reader"),
            StreamWatcherAnomaliesRequest {
                repo: rref(&w.repo),
                ..Default::default()
            },
        ))
        .await);
    let suspect = w
        .vcs
        .ref_tip(&w.repo, "main")
        .await
        .unwrap()
        .unwrap()
        .target
        .unwrap();
    let report = |note: &str| {
        req(
            &w.token("sentry"),
            ReportWatcherAnomalyRequest {
                repo: rref(&w.repo),
                watcher_id: watcher.id.clone(),
                title: "checkout p99 latency regressed".into(),
                description: "p99 jumped after the last land.".into(),
                severity: AnomalySeverity::Critical as i32,
                evidence: vec![AnomalyEvidence {
                    kind: "metric".into(),
                    metric_name: "p99_latency_ms".into(),
                    labels: vec![KeyValue {
                        key: "service".into(),
                        value: "checkout".into(),
                    }],
                    baseline: 120.0,
                    observed: 480.0,
                    suspect_changeset: Some(suspect.clone()),
                    note: note.into(),
                    ..Default::default()
                }],
                fingerprint: "checkout-p99".into(),
                ..Default::default()
            },
        )
    };
    let a = ok(w.api.report_watcher_anomaly(report("first")).await);
    assert!(!a.issue.is_empty());
    let issue = w
        .forge
        .work
        .get_issue_by_number(
            &cx("reader"),
            &RepoInfo::from(&w.repo),
            a.issue.parse().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(issue.title, "checkout p99 latency regressed");
    assert_eq!(issue.author.unwrap().id, "sentry");
    assert!(
        issue.body.contains("p99_latency_ms") && issue.body.contains("+300%"),
        "{}",
        issue.body
    );
    assert!(issue.body.contains(&bee_forge::ids::hex(&suspect.raw)));
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.id, a.id);
    // The same fingerprint updates the open anomaly.
    let again = ok(w.api.report_watcher_anomaly(report("second")).await);
    assert_eq!(again.id, a.id);
    assert_eq!(again.evidence.len(), 2);
    assert!(again.seq > a.seq);
    // Someone without commit who is not the watcher's agent cannot report.
    let mut bad = report("x");
    *bad.metadata_mut() = req(&w.token("reader"), ()).metadata().clone();
    assert_eq!(
        reason(w.api.report_watcher_anomaly(bad).await),
        "PERMISSION_DENIED"
    );
    let list = ok(w
        .api
        .list_watcher_anomalies(req(
            &w.token("reader"),
            ListWatcherAnomaliesRequest {
                repo: rref(&w.repo),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(list.anomalies.len(), 1);
    assert_eq!(
        reason(
            w.api
                .acknowledge_watcher_anomaly(req(
                    &w.token("reader"),
                    AcknowledgeWatcherAnomalyRequest {
                        repo: rref(&w.repo),
                        id: a.id.clone(),
                        ..Default::default()
                    }
                ))
                .await
        ),
        "PERMISSION_DENIED"
    );
    let resolved = ok(w
        .api
        .acknowledge_watcher_anomaly(req(
            &w.token("writer"),
            AcknowledgeWatcherAnomalyRequest {
                repo: rref(&w.repo),
                id: a.id.clone(),
                note: "reverted".into(),
                resolve: true,
            },
        ))
        .await);
    assert_eq!(resolved.state, AnomalyState::Resolved as i32);
    // After resolution the fingerprint opens a fresh anomaly.
    let fresh = ok(w.api.report_watcher_anomaly(report("third")).await);
    assert_ne!(fresh.id, a.id);
    // Linking to an existing issue.
    let linked = ok(w
        .api
        .link_watcher_anomaly_to_issue(req(
            &w.token("writer"),
            LinkWatcherAnomalyToIssueRequest {
                repo: rref(&w.repo),
                id: fresh.id.clone(),
                issue: issue.number.to_string(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(linked.issue, issue.number.to_string());
    let kinds = w.kinds().await;
    assert!(
        kinds.contains(&"anomaly.reported".to_string())
            && kinds.contains(&"anomaly.updated".to_string())
    );
}

// ----- principals, dashboards, activity ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn principals_dashboards_work_items_and_activity_stream() {
    let w = world().await;
    let issue = w.issue("Dashboard").await;
    ok(w.issues
        .add_issue_assignees(req(
            &w.token("writer"),
            AddIssueAssigneesRequest {
                repo: rref(&w.repo),
                number: issue.number,
                assignees: vec!["bot".into()],
                ..Default::default()
            },
        ))
        .await);
    w.settle().await;
    let list = ok(w
        .api
        .list_agent_principals(req(
            &w.token("reader"),
            ListAgentPrincipalsRequest {
                org: w.org.id.clone(),
                owner_id: "writer".into(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(list.agents.len(), 2);
    let bot = list
        .agents
        .iter()
        .find(|a| a.principal.as_ref().unwrap().id == "bot")
        .unwrap();
    assert_eq!(bot.last_model, "claude-sonnet");
    assert!(bot.last_session.starts_with("run-"));
    assert_eq!(bot.open_change_requests, 1);
    let stats = bot.stats.unwrap();
    assert_eq!(stats.changesets, 1);
    assert_eq!(stats.active_runs, 1);
    assert_eq!(stats.active_leases, 1);
    assert_eq!(
        reason(
            w.api
                .list_agent_principals(req(
                    &w.token("outsider"),
                    ListAgentPrincipalsRequest {
                        org: w.org.id.clone(),
                        ..Default::default()
                    }
                ))
                .await
        ),
        "PERMISSION_DENIED"
    );
    let dash = ok(w
        .api
        .get_agent_dashboard(req(
            &w.token("reader"),
            GetAgentDashboardRequest {
                principal_id: "bot".into(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(dash.on_behalf_of, vec!["writer".to_string()]);
    assert_eq!(
        dash.totals
            .unwrap()
            .outcomes
            .unwrap()
            .change_requests_opened,
        1
    );
    assert_eq!(dash.models, vec!["claude-sonnet".to_string()]);
    assert_eq!(dash.top_repo_ids, vec![w.repo.id.clone()]);
    assert_eq!(dash.daily.len(), 1);
    assert_eq!(
        reason(
            w.api
                .get_agent_dashboard(req(
                    &w.token("outsider"),
                    GetAgentDashboardRequest {
                        principal_id: "bot".into(),
                        ..Default::default()
                    }
                ))
                .await
        ),
        "NOT_FOUND"
    );
    let items = ok(w
        .api
        .list_agent_work_items(req(
            &w.token("reader"),
            ListAgentWorkItemsRequest {
                principal_id: "bot".into(),
                ..Default::default()
            },
        ))
        .await);
    let kinds: Vec<i32> = items
        .items
        .iter()
        .map(|i| i.subject.as_ref().unwrap().kind)
        .collect();
    for k in [
        SubjectKind::AgentRun,
        SubjectKind::ChangeRequest,
        SubjectKind::Changeset,
    ] {
        assert!(kinds.contains(&(k as i32)), "{k:?}");
    }
    assert!(
        items
            .items
            .iter()
            .filter(|i| i.subject.as_ref().unwrap().kind == SubjectKind::Changeset as i32)
            .all(|i| !i.session.is_empty())
    );
    // Land it: outcomes move.
    let n: u64 = w.runs_of("bot").await[0].change_requests[0]
        .parse()
        .unwrap();
    w.land("bot", n).await;
    w.settle().await;
    let dash = ok(w
        .api
        .get_agent_dashboard(req(
            &w.token("reader"),
            GetAgentDashboardRequest {
                principal_id: "bot".into(),
                ..Default::default()
            },
        ))
        .await);
    let o = dash.totals.unwrap().outcomes.unwrap();
    assert_eq!((o.change_requests_landed, o.runs_succeeded), (1, 1));
    // The activity stream: by agent, and by delegation root.
    let (by_agent, _) = w
        .agents
        .activity(
            &Cx::system(),
            &ActivityFilter {
                org_id: w.org.id.clone(),
                agent_id: "bot".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!by_agent.is_empty());
    assert!(by_agent.iter().all(|e| e.principal == "bot"));
    assert!(by_agent.iter().any(|e| e.kind == "cr.opened"));
    let (by_root, _) = w
        .agents
        .activity(
            &Cx::system(),
            &ActivityFilter {
                org_id: w.org.id.clone(),
                delegation_root: "writer".into(),
                kinds: vec!["cr.*".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        by_root
            .iter()
            .any(|e| e.principal == "bot" && e.kind == "cr.opened"),
        "{by_root:?}"
    );
    assert!(
        by_root
            .iter()
            .all(|e| e.on_behalf_of.last().unwrap_or(&e.principal) == "writer")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_events_stream_steering_cancel_and_retry() {
    let w = world().await;
    w.runner.script(
        "bot",
        FakeScript {
            hold: true,
            ..Default::default()
        },
    );
    let run = w.start("writer", "bot", "long task").await.unwrap();
    // Steering before the run starts is queued and handed to the runner.
    ok(w.api
        .send_agent_run_message(req(
            &w.token("writer"),
            SendAgentRunMessageRequest {
                run_id: run.id.clone(),
                body: "use small commits".into(),
            },
        ))
        .await);
    let agents = w.agents.clone();
    let dispatcher = tokio::spawn(async move { agents.dispatch_once().await });
    for _ in 0..200 {
        if w.run(&run.id).await.state == AgentRunState::Running as i32 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(w.run(&run.id).await.state, AgentRunState::Running as i32);
    assert_eq!(
        w.runner.jobs()[0].messages,
        vec!["use small commits".to_string()]
    );
    let mut stream = ok(w
        .api
        .stream_agent_run_events(req(
            &w.token("reader"),
            StreamAgentRunEventsRequest {
                run_id: run.id.clone(),
                ..Default::default()
            },
        ))
        .await);
    ok(w.api
        .send_agent_run_message(req(
            &w.token("writer"),
            SendAgentRunMessageRequest {
                run_id: run.id.clone(),
                body: "also add tests".into(),
            },
        ))
        .await);
    assert_eq!(
        w.runner.steering(),
        vec![(run.id.clone(), "also add tests".to_string())]
    );
    let cancelled = ok(w
        .api
        .cancel_agent_run(req(
            &w.token("writer"),
            CancelAgentRunRequest {
                id: run.id.clone(),
                reason: "changed my mind".into(),
            },
        ))
        .await);
    assert_eq!(cancelled.state, AgentRunState::Cancelled as i32);
    dispatcher.await.unwrap().unwrap();
    assert_eq!(w.run(&run.id).await.state, AgentRunState::Cancelled as i32);
    assert!(w.runner.cancelled().contains(&run.id));
    let mut seen = Vec::new();
    while let Ok(Some(e)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await
    {
        seen.push(e.unwrap());
    }
    assert!(
        seen.iter()
            .any(|e| e.kind == AgentRunEventKind::SteeringMessage as i32
                && e.text == "also add tests")
    );
    assert!(
        seen.iter()
            .any(|e| e.state == AgentRunState::Cancelled as i32)
    );
    assert!(seen.windows(2).all(|p| p[0].seq < p[1].seq));
    assert_eq!(
        reason(
            w.api
                .cancel_agent_run(req(
                    &w.token("writer"),
                    CancelAgentRunRequest {
                        id: run.id.clone(),
                        reason: String::new()
                    }
                ))
                .await
        ),
        "INVALID_STATE"
    );
    assert_eq!(
        reason(
            w.api
                .send_agent_run_message(req(
                    &w.token("writer"),
                    SendAgentRunMessageRequest {
                        run_id: run.id.clone(),
                        body: "x".into()
                    }
                ))
                .await
        ),
        "INVALID_STATE"
    );
    // Paging through the session log.
    let p1 = ok(w
        .api
        .list_agent_run_events(req(
            &w.token("reader"),
            ListAgentRunEventsRequest {
                run_id: run.id.clone(),
                page: Some(PageRequest {
                    page_size: 2,
                    page_token: String::new(),
                }),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(p1.events.len(), 2);
    let p2 = ok(w
        .api
        .list_agent_run_events(req(
            &w.token("reader"),
            ListAgentRunEventsRequest {
                run_id: run.id.clone(),
                page: p1.page.map(|p| PageRequest {
                    page_size: 2,
                    page_token: p.next_page_token,
                }),
                ..Default::default()
            },
        ))
        .await);
    assert!(p2.events[0].seq > p1.events[1].seq);
    // Retry starts a fresh run with the same task.
    w.runner.script("bot", FakeScript::default());
    let retried = ok(w
        .api
        .retry_agent_run(req(
            &w.token("writer"),
            RetryAgentRunRequest {
                id: run.id.clone(),
                ..Default::default()
            },
        ))
        .await);
    assert_ne!(retried.id, run.id);
    assert_eq!(retried.task, run.task);
    w.settle().await;
    assert_eq!(
        w.run(&retried.id).await.state,
        AgentRunState::Succeeded as i32
    );
    let listed = ok(w
        .api
        .list_agent_runs(req(
            &w.token("reader"),
            ListAgentRunsRequest {
                repo: rref(&w.repo),
                states: vec![AgentRunState::Cancelled as i32],
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(listed.runs.len(), 1);
}

// ----- instructions, memory, bundles ------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn instructions_memory_and_context_bundles() {
    let w = world().await;
    let t = w.token("writer");
    let resolve = |runtime: &str, custom: &str| {
        req(
            &t,
            ResolveAgentInstructionsRequest {
                repo: rref(&w.repo),
                paths: vec![b"src/net/http.rs".to_vec()],
                runtime: runtime.into(),
                custom_agent: custom.into(),
                ..Default::default()
            },
        )
    };
    let r = ok(w
        .api
        .resolve_agent_instructions(resolve("claude-code", "reviewer"))
        .await);
    let paths: Vec<String> = r
        .files
        .iter()
        .map(|f| String::from_utf8_lossy(&f.path).into_owned())
        .collect();
    assert_eq!(
        paths,
        vec![
            ".github/copilot-instructions.md",
            ".github/instructions/rust.instructions.md",
            "AGENTS.md",
            "src/CLAUDE.md",
            ".github/agents/reviewer.md"
        ]
    );
    assert_eq!(r.files[2].includes, vec![b"docs/style.md".to_vec()]);
    assert!(r.combined.contains("Short sentences.") && r.combined.contains("Review strictly."));
    let codex = ok(w.api.resolve_agent_instructions(resolve("codex", "")).await);
    assert!(
        codex
            .files
            .iter()
            .any(|f| f.path == b"src/AGENTS.md".to_vec())
    );
    assert_eq!(
        reason(
            w.api
                .resolve_agent_instructions(resolve("codex", "nobody"))
                .await
        ),
        "NOT_FOUND"
    );
    let all = ok(w
        .api
        .list_agent_instruction_files(req(
            &t,
            ListAgentInstructionFilesRequest {
                repo: rref(&w.repo),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(all.files.len(), 6);
    assert!(all.files.iter().all(|f| f.content.is_empty()));
    // Org instructions come first.
    let org_scope = || {
        Some(AgentMemoryScope {
            scope: Some(agent_memory_scope::Scope::Org(w.org.id.clone())),
        })
    };
    ok(w.api
        .put_agent_memory(req(
            &w.token("admin"),
            PutAgentMemoryRequest {
                scope: org_scope(),
                key: "instructions".into(),
                value: "Org rules.".into(),
                ..Default::default()
            },
        ))
        .await);
    let r = ok(w.api.resolve_agent_instructions(resolve("codex", "")).await);
    assert_eq!(r.files[0].source, AgentInstructionSource::Org as i32);
    // Memory with versions, TTL and search.
    let scope = || {
        Some(AgentMemoryScope {
            scope: Some(agent_memory_scope::Scope::Repo(RepoRef {
                repo: Some(repo_ref::Repo::Id(w.repo.id.clone())),
            })),
        })
    };
    let put = |key: &str, value: &str, expected: u64, ttl: u64, tags: &[&str]| {
        req(
            &t,
            PutAgentMemoryRequest {
                scope: scope(),
                key: key.into(),
                value: value.into(),
                expected_version: expected,
                ttl_seconds: ttl,
                tags: tags.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        )
    };
    assert_eq!(
        ok(w.api
            .put_agent_memory(put("build/cache", "use sccache for builds", 0, 0, &["ci"]))
            .await)
        .version,
        1
    );
    assert_eq!(
        ok(w.api
            .put_agent_memory(put(
                "build/cache",
                "use sccache; cache is warm",
                1,
                0,
                &["ci"]
            ))
            .await)
        .version,
        2
    );
    assert_eq!(
        reason(
            w.api
                .put_agent_memory(put("build/cache", "stale", 1, 0, &[]))
                .await
        ),
        "VERSION_MISMATCH"
    );
    ok(w.api
        .put_agent_memory(put("style", "prefer small diffs", 0, 0, &[]))
        .await);
    ok(w.api
        .put_agent_memory(put("tmp", "scratch", 0, 10, &[]))
        .await);
    let got = ok(w
        .api
        .get_agent_memory(req(
            &w.token("reader"),
            GetAgentMemoryRequest {
                scope: scope(),
                key: "tmp".into(),
            },
        ))
        .await);
    assert!(got.expires_at.is_some());
    w.clock.advance(11_000);
    assert_eq!(
        reason(
            w.api
                .get_agent_memory(req(
                    &t,
                    GetAgentMemoryRequest {
                        scope: scope(),
                        key: "tmp".into()
                    }
                ))
                .await
        ),
        "NOT_FOUND"
    );
    let listed = ok(w
        .api
        .list_agent_memories(req(
            &t,
            ListAgentMemoriesRequest {
                scope: scope(),
                key_prefix: "build/".into(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(listed.memories.len(), 1);
    let tagged = ok(w
        .api
        .list_agent_memories(req(
            &t,
            ListAgentMemoriesRequest {
                scope: scope(),
                tags: vec!["ci".into()],
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(tagged.memories.len(), 1);
    let hits = ok(w
        .api
        .search_agent_memories(req(
            &t,
            SearchAgentMemoriesRequest {
                scope: scope(),
                query: "cache".into(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(hits.hits[0].memory.as_ref().unwrap().key, "build/cache");
    ok(w.api
        .delete_agent_memory(req(
            &t,
            DeleteAgentMemoryRequest {
                scope: scope(),
                key: "style".into(),
            },
        ))
        .await);
    assert_eq!(
        reason(
            w.api
                .delete_agent_memory(req(
                    &t,
                    DeleteAgentMemoryRequest {
                        scope: scope(),
                        key: "style".into()
                    }
                ))
                .await
        ),
        "NOT_FOUND"
    );
    w.agents.maintain().await.unwrap();
    // Context bundles: private until shared; materialized at a revision.
    let issue = w.issue("Context").await;
    let b = ok(w
        .api
        .create_context_bundle(req(
            &t,
            CreateContextBundleRequest {
                repo: rref(&w.repo),
                name: "net".into(),
                items: vec![
                    ContextBundleItem {
                        item: Some(context_bundle_item::Item::Path(b"src/net".to_vec())),
                        label: "net code".into(),
                    },
                    ContextBundleItem {
                        item: Some(context_bundle_item::Item::Subject(SubjectRef {
                            kind: SubjectKind::Issue as i32,
                            repo: None,
                            id: issue.number.to_string(),
                        })),
                        label: String::new(),
                    },
                    ContextBundleItem {
                        item: Some(context_bundle_item::Item::Note("remember TLS".into())),
                        label: "note".into(),
                    },
                    ContextBundleItem {
                        item: Some(context_bundle_item::Item::Url(
                            "https://example.com/rfc".into(),
                        )),
                        label: "rfc".into(),
                    },
                ],
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(
        reason(
            w.api
                .get_context_bundle(req(
                    &w.token("reader"),
                    GetContextBundleRequest { id: b.id.clone() }
                ))
                .await
        ),
        "NOT_FOUND"
    );
    let m = ok(w
        .api
        .materialize_context_bundle(req(
            &t,
            MaterializeContextBundleRequest {
                id: b.id.clone(),
                include_bodies: true,
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(m.items[0].files.len(), 1);
    assert_eq!(m.items[0].files[0].path, b"src/net/http.rs".to_vec());
    assert_eq!(m.items[0].files[0].size, 12);
    assert_eq!(m.items[1].title, "Context");
    assert_eq!(m.items[2].body, "remember TLS");
    ok(w.api
        .update_context_bundle(req(
            &t,
            UpdateContextBundleRequest {
                id: b.id.clone(),
                shared: Some(true),
                ..Default::default()
            },
        ))
        .await);
    ok(w.api
        .get_context_bundle(req(
            &w.token("reader"),
            GetContextBundleRequest { id: b.id.clone() },
        ))
        .await);
    assert_eq!(
        reason(
            w.api
                .update_context_bundle(req(
                    &w.token("reader"),
                    UpdateContextBundleRequest {
                        id: b.id.clone(),
                        name: Some("x".into()),
                        ..Default::default()
                    }
                ))
                .await
        ),
        "PERMISSION_DENIED"
    );
    let bl = ok(w
        .api
        .list_context_bundles(req(
            &w.token("reader"),
            ListContextBundlesRequest {
                repo: rref(&w.repo),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(bl.bundles.len(), 1);
    // A run with the bundle sees it in its prompt.
    let run = ok(w
        .api
        .start_agent_run(req(
            &t,
            StartAgentRunRequest {
                repo: rref(&w.repo),
                task: Some(AgentTask {
                    kind: AgentTaskKind::Freeform as i32,
                    prompt: "look".into(),
                    context_bundle_ids: vec![b.id.clone()],
                    ..Default::default()
                }),
                agent_principal_id: "bot".into(),
                ..Default::default()
            },
        ))
        .await);
    w.settle().await;
    let job = w
        .runner
        .jobs()
        .into_iter()
        .find(|j| j.run.id == run.id)
        .unwrap();
    assert!(job.prompt.contains("src/net/http.rs"));
    ok(w.api
        .delete_context_bundle(req(&t, DeleteContextBundleRequest { id: b.id.clone() }))
        .await);
    assert_eq!(
        reason(
            w.api
                .get_context_bundle(req(&t, GetContextBundleRequest { id: b.id.clone() }))
                .await
        ),
        "NOT_FOUND"
    );
}

// ----- review configuration, revise, schedules, runtimes ----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_config_reviews_and_requested_changes_start_a_revise_run() {
    let w = world().await;
    ok(w.api
        .set_agent_review_config(req(
            &w.token("admin"),
            SetAgentReviewConfigRequest {
                repo: rref(&w.repo),
                trigger: Some(AgentReviewTrigger::OnOpen as i32),
                agent_principal_id: Some("critic".into()),
                instructions: Some("be kind".into()),
                ..Default::default()
            },
        ))
        .await);
    let cfg = ok(w
        .api
        .get_agent_review_config(req(
            &w.token("reader"),
            GetAgentReviewConfigRequest {
                repo: rref(&w.repo),
            },
        ))
        .await);
    assert_eq!(cfg.agent_principal_id, "critic");
    let cr = w.open_cr("writer", "src/feature.rs", "Feature").await;
    w.settle().await;
    let reviews = w.runs_of("critic").await;
    assert_eq!(reviews.len(), 1);
    assert_eq!(
        reviews[0].task.as_ref().unwrap().change_request,
        cr.number.to_string()
    );
    assert_eq!(reviews[0].state, AgentRunState::Succeeded as i32);
    // An agent-authored CR is not auto-reviewed (review_agent_authored is off).
    let issue = w.issue("Fix it").await;
    ok(w.issues
        .add_issue_assignees(req(
            &w.token("writer"),
            AddIssueAssigneesRequest {
                repo: rref(&w.repo),
                number: issue.number,
                assignees: vec!["bot".into()],
                ..Default::default()
            },
        ))
        .await);
    w.settle().await;
    assert_eq!(w.runs_of("critic").await.len(), 1);
    let bot_run = w.runs_of("bot").await.remove(0);
    let n: u64 = bot_run.change_requests[0].parse().unwrap();
    let bot_cr = w
        .changes
        .get_by_number(&cx("writer"), &w.repo, n)
        .await
        .unwrap();
    // Requested changes make the agent revise its CR.
    w.changes
        .submit_review(
            &cx("writer"),
            &w.repo,
            &bot_cr.id,
            0,
            Verdict::RequestChanges,
            "please rename",
            vec![],
        )
        .await
        .unwrap();
    w.settle().await;
    let runs = w.runs_of("bot").await;
    let revise = runs
        .iter()
        .find(|r| r.task.as_ref().unwrap().kind == AgentTaskKind::Revise as i32)
        .unwrap();
    assert_eq!(revise.state, AgentRunState::Succeeded as i32, "{revise:?}");
    assert!(
        revise
            .task
            .as_ref()
            .unwrap()
            .prompt
            .contains("please rename")
    );
    let after = w
        .changes
        .get_by_number(&cx("writer"), &w.repo, n)
        .await
        .unwrap();
    assert_eq!(after.revisions.len(), 2);
}

/// F49-009: a land rule with `require_agent_review` is a policy hook: the repo's review
/// agent reviews every opened change request (and new revision) even when the configured
/// trigger would not fire, and drafts or path filters do not skip it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn require_agent_review_policy_requests_the_review_agent_on_open() {
    let w = world().await;
    ok(w.api
        .set_agent_review_config(req(
            &w.token("admin"),
            SetAgentReviewConfigRequest {
                repo: rref(&w.repo),
                trigger: Some(AgentReviewTrigger::OnRequest as i32),
                agent_principal_id: Some("critic".into()),
                paths: vec![b"docs".to_vec()],
                ..Default::default()
            },
        ))
        .await);
    // Without the policy, an on-request config outside its paths starts nothing.
    w.open_cr("writer", "src/a.rs", "First").await;
    w.settle().await;
    assert!(w.runs_of("critic").await.is_empty());
    *w.policy.0.write().unwrap() = Some(
        "version = 1\n[members]\nwrite = [\"@everyone\"]\n[[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n[[land]]\nref = \"main\"\nrequire_agent_review = true\n".into(),
    );
    let cr = w.open_cr("writer", "src/b.rs", "Second").await;
    w.settle().await;
    let runs = w.runs_of("critic").await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    let task = runs[0].task.as_ref().unwrap();
    assert_eq!(task.kind, AgentTaskKind::Review as i32);
    assert_eq!(task.change_request, cr.number.to_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedules_fire_runs_and_runtimes_gate_deletion() {
    let w = world().await;
    w.agents
        .update_agent(
            &cx("writer"),
            &w.org.id,
            "bot",
            bee_forge::agents::DefinitionPatch {
                schedules: Some(vec![Schedule {
                    repo_id: w.repo.id.clone(),
                    cron: "*/5 * * * *".into(),
                    prompt: "nightly sweep".into(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    w.settle().await;
    assert!(w.runs_of("bot").await.is_empty());
    w.clock.advance(6 * 60 * 1000);
    w.settle().await;
    let runs = w.runs_of("bot").await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].task.as_ref().unwrap().prompt, "nightly sweep");
    w.settle().await;
    assert_eq!(w.runs_of("bot").await.len(), 1);
    // A runtime with an active run cannot be deleted.
    let rt = ok(w
        .api
        .register_agent_runtime(req(
            &w.token("admin"),
            RegisterAgentRuntimeRequest {
                org: w.org.id.clone(),
                name: "codex".into(),
                version: "2".into(),
                default_models: vec!["gpt-x".into()],
                ..Default::default()
            },
        ))
        .await);
    w.runner.script(
        "bot2",
        FakeScript {
            hold: true,
            ..Default::default()
        },
    );
    let run = ok(w
        .api
        .start_agent_run(req(
            &w.token("writer"),
            StartAgentRunRequest {
                repo: rref(&w.repo),
                task: Some(AgentTask {
                    kind: AgentTaskKind::Freeform as i32,
                    prompt: "x".into(),
                    ..Default::default()
                }),
                agent_principal_id: "bot2".into(),
                runtime_id: rt.id.clone(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(run.runtime_id, rt.id);
    assert_eq!(
        reason(
            w.api
                .delete_agent_runtime(req(
                    &w.token("admin"),
                    DeleteAgentRuntimeRequest { id: rt.id.clone() }
                ))
                .await
        ),
        "INVALID_STATE"
    );
    ok(w.api
        .cancel_agent_run(req(
            &w.token("writer"),
            CancelAgentRunRequest {
                id: run.id.clone(),
                reason: String::new(),
            },
        ))
        .await);
    let upd = ok(w
        .api
        .update_agent_runtime(req(
            &w.token("admin"),
            UpdateAgentRuntimeRequest {
                id: rt.id.clone(),
                enabled: Some(false),
                ..Default::default()
            },
        ))
        .await);
    assert!(!upd.enabled);
    ok(w.api
        .delete_agent_runtime(req(
            &w.token("admin"),
            DeleteAgentRuntimeRequest { id: rt.id.clone() },
        ))
        .await);
    assert_eq!(
        reason(
            w.api
                .get_agent_runtime(req(&w.token("admin"), GetAgentRuntimeRequest { id: rt.id }))
                .await
        ),
        "NOT_FOUND"
    );
}

// ----- the store on every backend ---------------------------------------------------------------

async fn scenario(store: Arc<dyn ForgeStore>) {
    let w = world_on(store).await;
    // Registry.
    let defs = w
        .agents
        .definitions(&Cx::system(), &w.org.id)
        .await
        .unwrap();
    assert_eq!(defs.len(), 4);
    let dup = w
        .agents
        .define_agent(
            &cx("writer"),
            AgentDefinition {
                org_id: w.org.id.clone(),
                agent: principal("bot"),
                name: "bot-again".into(),
                runtime: "codex".into(),
                owners: vec!["writer".into()],
                ..Default::default()
            },
        )
        .await;
    assert_eq!(dup.unwrap_err().reason, ErrorReason::AlreadyExists);
    let not_agent = w
        .agents
        .define_agent(
            &cx("writer"),
            AgentDefinition {
                org_id: w.org.id.clone(),
                agent: principal("reader"),
                name: "r".into(),
                runtime: "codex".into(),
                owners: vec!["writer".into()],
                ..Default::default()
            },
        )
        .await;
    assert_eq!(not_agent.unwrap_err().subcode, "not_an_agent");
    let bad = w
        .agents
        .define_agent(
            &cx("writer"),
            AgentDefinition {
                org_id: w.org.id.clone(),
                agent: principal("bot"),
                name: "b".into(),
                runtime: "codex".into(),
                toolsets: vec!["purge".into()],
                ..Default::default()
            },
        )
        .await;
    assert_eq!(bad.unwrap_err().subcode, "invalid_toolset");
    // A run end to end.
    let issue = w.issue("Backend").await;
    ok(w.issues
        .add_issue_assignees(req(
            &w.token("writer"),
            AddIssueAssigneesRequest {
                repo: rref(&w.repo),
                number: issue.number,
                assignees: vec!["bot".into()],
                ..Default::default()
            },
        ))
        .await);
    w.settle().await;
    let run = w.runs_of("bot").await.remove(0);
    assert_eq!(run.state, AgentRunState::WaitingForReview as i32);
    w.land("bot", run.change_requests[0].parse().unwrap()).await;
    w.settle().await;
    assert_eq!(w.run(&run.id).await.state, AgentRunState::Succeeded as i32);
    // Memory, watcher and anomaly, workspace.
    let scope = AgentMemoryScope {
        scope: Some(agent_memory_scope::Scope::AgentPrincipalId("bot".into())),
    };
    w.agents
        .put_memory(
            &cx("bot"),
            &w.org.id,
            AgentMemory {
                scope: Some(scope.clone()),
                key: "k".into(),
                value: "v".into(),
                ..Default::default()
            },
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(
        w.agents
            .get_memory(&Cx::system(), &scope, "k")
            .await
            .unwrap()
            .version,
        1
    );
    let watcher = w
        .agents
        .create_watcher(
            &cx("admin"),
            &w.repo,
            bee_forge::agents::watchers::NewWatcher {
                name: "w".into(),
                agent: principal("sentry"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let a = w
        .agents
        .report_anomaly(
            &cx("sentry"),
            &w.repo,
            bee_forge::agents::watchers::NewAnomaly {
                watcher_id: watcher.id.clone(),
                title: "slow".into(),
                fingerprint: "f".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let b = w
        .agents
        .report_anomaly(
            &cx("sentry"),
            &w.repo,
            bee_forge::agents::watchers::NewAnomaly {
                watcher_id: watcher.id.clone(),
                title: "slower".into(),
                fingerprint: "f".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(b.seq, a.seq + 1);
    let ws = w
        .agents
        .workspaces
        .create(
            &cx("writer"),
            &w.repo,
            &principal("writer"),
            bee_forge::agents::workspaces::NewWorkspace {
                name: "db".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        w.agents
            .workspaces
            .stop(&cx("writer"), &ws.id)
            .await
            .unwrap()
            .state,
        WorkspaceState::Stopped as i32
    );
    w.agents
        .workspaces
        .delete(&cx("writer"), &ws.id, false)
        .await
        .unwrap();
    // Every state change reached the audit chain, and the chain verifies.
    let audit = w.audit_actions().await;
    for a in [
        "agent.define:ok",
        "agent.run_start:ok",
        "agent.memory_put:ok",
        "agent.watcher_create:ok",
        "agent.anomaly_report:ok",
        "workspace.create:ok",
        "workspace.delete:ok",
    ] {
        assert!(audit.iter().any(|x| x == a), "{a}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_store_on_memory() {
    scenario(Arc::new(MemoryStore::new())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_store_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqlStore::open(
        SqlConfig::sqlite(dir.path()),
        Arc::new(bee_forge::ids::SystemClock),
    )
    .await
    .unwrap();
    scenario(Arc::new(store)).await;
    assert!(dir.path().join("forge-agents.db").exists());
    assert!(dir.path().join("forge-workspaces.db").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_store_on_postgres_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_agents_test_{}", std::process::id());
    }
    cfg.default_pool_size = 4;
    let store = SqlStore::open(cfg, Arc::new(bee_forge::ids::SystemClock))
        .await
        .unwrap();
    scenario(Arc::new(store)).await;
}
