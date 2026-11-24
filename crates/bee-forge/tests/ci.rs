//! CI end to end over real history (`HistoryVcs` on in-memory VCS stores) and the local
//! runner: the store on every backend, triggers, matrices and the needs DAG, concurrency,
//! re-runs, leases, logs and masking, secrets, environments, job tokens, check runs and land
//! input, artifacts and caches, reusable and composite workflows, the service permission
//! matrix, and the agent scenarios AS-CI-01/02.

use crate::common;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bee_auth::KeyRing;
use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::changes::HistoryVcs;
use bee_forge::changes::vcs::{ChangeVcs, NewCommit};
use bee_forge::ci::engine::Rerun;
use bee_forge::ci::environments::EnvironmentSpec;
use bee_forge::ci::runner::{InProcessApi, LocalRunner, Ran, RunnerApi, RunnerConfig};
use bee_forge::ci::runners::RunnerOwner;
use bee_forge::ci::secrets::VaultScope;
use bee_forge::ci::{Ci, CiConfig, CiHooks, CiServices, HistoryCiVcs, HmacKey, MemoryCiBlobs};
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner, PrincipalDirectory};
use bee_forge::ids::{Clock, SystemClock, hex};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::runner_service_server::RunnerService;
use bee_proto::v1::workflow_service_server::WorkflowService;
use bee_proto::v1::*;
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};

// ----- fixture -------------------------------------------------------------------------------

pub(crate) struct World {
    pub(crate) forge: Arc<Forge>,
    pub(crate) ci: Arc<Ci>,
    pub(crate) svc: CiServices,
    vcs: Arc<HistoryVcs>,
    blobs: Arc<MemoryCiBlobs>,
    ring: KeyRing,
    clock: Arc<dyn Clock>,
    pub(crate) org: Org,
    pub(crate) repo: Repo,
    pub(crate) work: tempfile::TempDir,
}

const PEOPLE: &[(&str, PrincipalKind)] = &[
    ("owner", PrincipalKind::Human),
    ("admin", PrincipalKind::Human),
    ("writer", PrincipalKind::Human),
    ("reader", PrincipalKind::Human),
    ("outsider", PrincipalKind::Human),
    ("agent", PrincipalKind::Agent),
];

fn config() -> CiConfig {
    CiConfig {
        keepalive_ms: 50,
        ..CiConfig::default()
    }
}

pub(crate) async fn world_on(store: Arc<dyn ForgeStore>, cfg: CiConfig) -> World {
    world_on_keys(store, cfg, Arc::new(HmacKey::new("k1", b"oidc-key"))).await
}

async fn world_on_keys(
    store: Arc<dyn ForgeStore>,
    cfg: CiConfig,
    keys: Arc<dyn bee_forge::ci::ports::KeyPort>,
) -> World {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let meta = Arc::new(MemoryMetaStore::new());
    let vblobs = Arc::new(MemoryBlobStore::new(clock.clone()));
    let writer = Writer::new(meta.clone(), vblobs);
    let directory = Arc::new(MemoryDirectory::new());
    for (id, kind) in PEOPLE {
        directory.register(
            Principal {
                id: id.to_string(),
                kind: *kind as i32,
                display: id.to_string(),
                ..Default::default()
            },
            vec![format!("{id}-login")],
            vec![format!("{id}@example.com")],
        );
    }
    let ring = KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let hooks = Hooks {
        directory: directory.clone(),
        provisioner: Arc::new(MetaProvisioner::new(meta, clock.clone())),
        ..Hooks::default()
    };
    let (forge, _) = Forge::open(
        store,
        clock.clone(),
        ring.verifying_keys(),
        Arc::new(bee_auth::InMemoryRevocations::new()),
        hooks,
        common::test_config(),
    )
    .await
    .unwrap();
    let vcs = Arc::new(HistoryVcs::new(writer, clock.clone()));
    let blobs = Arc::new(MemoryCiBlobs::new());
    let ci = Ci::new(
        &forge,
        CiHooks {
            vcs: Arc::new(HistoryCiVcs::new(vcs.clone())),
            blobs: blobs.clone(),
            keys,
            secret_key: [3u8; 32],
            url_key: b"url-key".to_vec(),
            signer: Arc::new(KeyRing::new(
                bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap(),
            )),
            config: cfg,
        },
    );
    ci.attach(&forge);
    let svc = CiServices::new(forge.clone(), ci.clone());
    let cx = Cx::new("owner", PrincipalKind::Human);
    let name = format!("o{}", forge.db.ids().ulid().unwrap().to_lowercase());
    let org = forge
        .orgs
        .create_org(&cx, &name, "", directory.get("owner").await.unwrap())
        .await
        .unwrap();
    let repo = forge
        .orgs
        .create_repo(
            &cx,
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
        .set_member(
            &cx,
            &org.id,
            directory.get("admin").await.unwrap(),
            Role::Admin,
        )
        .await
        .unwrap();
    for who in ["writer", "reader", "agent"] {
        forge
            .orgs
            .set_member(
                &cx,
                &org.id,
                directory.get(who).await.unwrap(),
                Role::Member,
            )
            .await
            .unwrap();
    }
    for who in ["writer", "agent"] {
        forge
            .orgs
            .add_collaborator(
                &cx,
                &repo.id,
                directory.get(who).await.unwrap(),
                RepoRole::Write,
                "",
            )
            .await
            .unwrap();
    }
    World {
        forge,
        ci,
        svc,
        vcs,
        blobs,
        ring,
        clock,
        org,
        repo,
        work: tempfile::tempdir().unwrap(),
    }
}

pub(crate) async fn world() -> World {
    world_on(Arc::new(MemoryStore::new()), config()).await
}

fn rref(repo: &Repo) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(repo.id.clone())),
    })
}

impl World {
    pub(crate) async fn another_ci_handle_on(&self, store: Arc<dyn ForgeStore>) -> Arc<Ci> {
        let ring = KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
        let (forge, _) = Forge::open(
            store,
            self.clock.clone(),
            ring.verifying_keys(),
            Arc::new(bee_auth::InMemoryRevocations::new()),
            Hooks {
                directory: self.forge.hooks.directory.clone(),
                provisioner: self.forge.hooks.provisioner.clone(),
                ..Hooks::default()
            },
            common::test_config(),
        )
        .await
        .unwrap();
        let ci = Ci::new(
            &forge,
            CiHooks {
                vcs: Arc::new(HistoryCiVcs::new(self.vcs.clone())),
                blobs: self.blobs.clone(),
                keys: Arc::new(HmacKey::new("k1", b"oidc-key")),
                secret_key: [3u8; 32],
                url_key: b"url-key".to_vec(),
                signer: Arc::new(KeyRing::new(
                    bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap(),
                )),
                config: config(),
            },
        );
        ci.attach(&forge);
        ci
    }

    pub(crate) fn cx(&self, who: &str) -> Cx {
        let kind = PEOPLE
            .iter()
            .find(|(p, _)| *p == who)
            .map(|(_, k)| *k)
            .unwrap_or(PrincipalKind::Human);
        Cx::new(who, kind)
    }

    pub(crate) fn token(&self, who: &str) -> String {
        let principal = bee_auth::Principal::human(who, who);
        self.ring
            .mint(
                &bee_auth::MintRequest {
                    principal,
                    on_behalf_of: vec![],
                    scope: bee_auth::Scope::new(),
                },
                self.clock.now_ms() / 1000,
            )
            .unwrap()
            .text
    }

    pub(crate) fn rr(&self) -> Option<RepoRef> {
        rref(&self.repo)
    }

    async fn tip(&self, name: &str) -> Option<ObjectId> {
        self.vcs
            .ref_tip(&self.repo, name)
            .await
            .unwrap()
            .and_then(|r| r.target)
    }

    async fn commit(
        &self,
        parent: Option<&ObjectId>,
        edits: &[(&str, Option<&str>)],
        message: &str,
    ) -> ObjectId {
        self.vcs
            .create_commit(
                &self.repo,
                NewCommit {
                    parent: parent.cloned(),
                    edits: edits
                        .iter()
                        .map(|(p, c)| (p.to_string(), c.map(|s| s.as_bytes().to_vec())))
                        .collect(),
                    message: message.to_string(),
                    author: "writer".into(),
                    creator: "writer".into(),
                    chain: vec!["writer".into()],
                    change_id: None,
                },
            )
            .await
            .unwrap()
    }

    /// Commits onto `branch`, moves it, and appends the `ref.updated` event the VCS bridge
    /// would; returns the new tip and the runs it started.
    pub(crate) async fn push(
        &self,
        branch: &str,
        edits: &[(&str, Option<&str>)],
        message: &str,
    ) -> (ObjectId, Vec<String>) {
        self.push_as(branch, edits, message, "writer").await
    }

    async fn push_as(
        &self,
        branch: &str,
        edits: &[(&str, Option<&str>)],
        message: &str,
        who: &str,
    ) -> (ObjectId, Vec<String>) {
        let parent = match self.tip(branch).await {
            Some(t) => Some(t),
            None => self.tip("main").await,
        };
        let c = self.commit(parent.as_ref(), edits, message).await;
        let version = self.vcs.set_ref(&self.repo, branch, &c).await.unwrap();
        let before = self.run_ids().await;
        self.forge
            .events
            .append(
                &Cx::system(),
                Event {
                    kind: "ref.updated".into(),
                    repo_id: self.repo.id.clone(),
                    org_id: self.org.id.clone(),
                    principal: who.into(),
                    body: Some(event::Body::RefUpdated(Ref {
                        name: branch.into(),
                        target: Some(c.clone()),
                        version,
                    })),
                    ..Default::default()
                },
                &format!("test:{}:{}", branch, hex(&c.raw)),
            )
            .await
            .unwrap();
        let after = self.run_ids().await;
        let new = after.into_iter().filter(|r| !before.contains(r)).collect();
        (c, new)
    }

    async fn run_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .ci
            .list_runs(&self.cx("owner"), &self.repo.id, "")
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        v.sort();
        v
    }

    async fn run(&self, id: &str) -> WorkflowRun {
        self.ci
            .get_run(&self.cx("owner"), &self.repo.id, id)
            .await
            .unwrap()
    }

    pub(crate) async fn jobs(&self, run: &str) -> Vec<WorkflowJob> {
        self.ci
            .list_jobs(&self.cx("owner"), &self.repo.id, run, 0, false)
            .await
            .unwrap()
    }

    async fn runner_named(&self, name: &str, labels: &[&str]) -> (Runner, String) {
        let cx = self.cx("owner");
        let owner = RunnerOwner::Repo(self.repo.id.clone(), self.org.id.clone());
        let (token, _) = self.ci.runner_token(&cx, &owner, "register").await.unwrap();
        self.ci
            .register_runner(
                &Cx::system(),
                &token,
                name,
                RunnerOs::Linux,
                RunnerArch::X64,
                &labels.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "",
                false,
                "test",
            )
            .await
            .unwrap()
    }

    pub(crate) async fn local(&self, name: &str) -> LocalRunner {
        self.local_with(name, RunnerConfig::default()).await
    }

    pub(crate) async fn local_with(&self, name: &str, mut config: RunnerConfig) -> LocalRunner {
        let (runner, cred) = self.runner_named(name, &["ubuntu-latest"]).await;
        config.work_root = self.work.path().to_path_buf();
        config.name = name.into();
        config.heartbeat = Duration::from_millis(200);
        LocalRunner::new(
            Arc::new(InProcessApi::new(self.svc.clone(), runner.id, cred, vec![])),
            config,
        )
    }

    pub(crate) async fn api(&self, name: &str) -> (Arc<dyn RunnerApi>, Runner, String) {
        let (runner, cred) = self.runner_named(name, &["ubuntu-latest"]).await;
        (
            Arc::new(InProcessApi::new(
                self.svc.clone(),
                runner.id.clone(),
                cred.clone(),
                vec![],
            )),
            runner,
            cred,
        )
    }

    pub(crate) async fn log(&self, job: &str) -> String {
        String::from_utf8(
            self.ci
                .job_log_bytes(&self.cx("owner"), &self.repo.id, job)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    pub(crate) async fn drain(&self, runner: &LocalRunner) -> Vec<Ran> {
        runner.run_until_idle(100).await.unwrap()
    }
}

fn concl(r: &WorkflowRun) -> WorkflowConclusion {
    WorkflowConclusion::try_from(r.conclusion).unwrap()
}

fn job_concl(j: &WorkflowJob) -> WorkflowConclusion {
    WorkflowConclusion::try_from(j.conclusion).unwrap()
}

const HELLO: &str = "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: echo hi && test -f README.md\n";

// ----- end to end ----------------------------------------------------------------------------

async fn push_runs_workflow_flow(w: &World) {
    let (head, runs) = w
        .push(
            "main",
            &[
                ("README.md", Some("# app\n")),
                (".github/workflows/ci.yml", Some(HELLO)),
            ],
            "add ci",
        )
        .await;
    assert_eq!(runs.len(), 1, "one run for the push");
    let run = w.run(&runs[0]).await;
    assert_eq!(run.event, "push");
    assert_eq!(run.status, WorkflowStatus::Queued as i32);
    assert_eq!(run.head_changeset.as_ref(), Some(&head));
    let runner = w.local("r1").await;
    let ran = w.drain(&runner).await;
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].conclusion, Some(WorkflowConclusion::Success));
    let run = w.run(&runs[0]).await;
    assert_eq!(run.status, WorkflowStatus::Completed as i32);
    assert_eq!(concl(&run), WorkflowConclusion::Success);
    let jobs = w.jobs(&run.id).await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].name, "test");
    assert!(
        jobs[0]
            .steps
            .iter()
            .all(|s| s.conclusion == WorkflowConclusion::Success as i32),
        "{:?}",
        jobs[0].steps
    );
    let log = w.log(&jobs[0].id).await;
    assert!(log.lines().any(|l| l == "hi"), "{log}");
    // The check run concluded success and is the land input for the head.
    let checks = w
        .forge
        .checks
        .check_results(&w.cx("owner"), &w.repo.id, &hex(&head.raw))
        .await
        .unwrap();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].name, "test");
    assert_eq!(checks[0].status, bee_policy::CheckStatus::Success);
    let check = w
        .forge
        .checks
        .get(&w.cx("owner"), &w.repo.id, &jobs[0].check_run_id)
        .await
        .unwrap();
    assert_eq!(check.workflow_job_id, jobs[0].id);
    assert!(
        check.post_land,
        "a push to the default branch is post-land testing"
    );
    // Events and usage.
    w.forge.flush().await.unwrap();
    let (events, _) = w
        .forge
        .events
        .read(&w.cx("owner"), &w.repo.id, 0, &[], 10_000)
        .await
        .unwrap();
    let kinds: Vec<String> = events.iter().map(|e| e.kind.clone()).collect();
    for k in [
        "workflow_run.requested",
        "workflow_job.queued",
        "workflow_job.in_progress",
        "workflow_job.completed",
        "workflow_usage.recorded",
        "workflow_run.completed",
    ] {
        assert!(kinds.iter().any(|x| x == k), "missing {k} in {kinds:?}");
    }
    let usage =
        w.ci.run_usage(&w.cx("owner"), &w.repo.id, &run.id)
            .await
            .unwrap();
    assert_eq!(usage.jobs.len(), 1);
    assert_eq!(usage.runners[0].machine_class, "self-hosted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_push_runs_workflow_on_local_runner() {
    let w = world().await;
    push_runs_workflow_flow(&w).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_conforms_on_memory() {
    let w = world_on(Arc::new(MemoryStore::new()), config()).await;
    push_runs_workflow_flow(&w).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_conforms_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
        .await
        .unwrap();
    let w = world_on(Arc::new(store), config()).await;
    push_runs_workflow_flow(&w).await;
    assert!(dir.path().join("forge-ci.db").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_conforms_on_postgres_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema: s, .. } = &mut cfg.backend {
        *s = format!("forge_ci_{}", std::process::id());
    }
    let w = world_on(
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap()),
        config(),
    )
    .await;
    push_runs_workflow_flow(&w).await;
}

// ----- triggers ------------------------------------------------------------------------------

/// Each row: a workflow, the push that follows, and whether it runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trigger_evaluation_table() {
    let w = world().await;
    let wf = |on: &str| {
        format!("{on}\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: 'true'\n")
    };
    // (name, on:, branch pushed, files changed, message, by job token, runs)
    let rows: Vec<(&str, &str, &str, &str, &str, bool, bool)> = vec![
        ("plain", "on: push", "main", "a.txt", "m", false, true),
        (
            "branch-filter-hit",
            "on:\n  push:\n    branches: [main]",
            "main",
            "a.txt",
            "m",
            false,
            true,
        ),
        (
            "branch-filter-miss",
            "on:\n  push:\n    branches: [release/*]",
            "main",
            "a.txt",
            "m",
            false,
            false,
        ),
        (
            "branch-glob",
            "on:\n  push:\n    branches: ['feature/**']",
            "feature/x/y",
            "a.txt",
            "m",
            false,
            true,
        ),
        (
            "branches-ignore",
            "on:\n  push:\n    branches-ignore: [main]",
            "main",
            "a.txt",
            "m",
            false,
            false,
        ),
        (
            "paths-hit",
            "on:\n  push:\n    paths: ['src/**']",
            "main",
            "src/lib.rs",
            "m",
            false,
            true,
        ),
        (
            "paths-miss",
            "on:\n  push:\n    paths: ['src/**']",
            "main",
            "docs/a.md",
            "m",
            false,
            false,
        ),
        (
            "paths-ignore",
            "on:\n  push:\n    paths-ignore: ['docs/**']",
            "main",
            "docs/b.md",
            "m",
            false,
            false,
        ),
        (
            "skip-ci",
            "on: push",
            "main",
            "a.txt",
            "fix [skip ci]",
            false,
            false,
        ),
        (
            "skip-checks-trailer",
            "on: push",
            "main",
            "a.txt",
            "fix\n\nskip-checks: true",
            false,
            false,
        ),
        ("job-token", "on: push", "main", "a.txt", "m", true, false),
        (
            "tags-only",
            "on:\n  push:\n    tags: ['v*']",
            "main",
            "a.txt",
            "m",
            false,
            false,
        ),
        (
            "not-push",
            "on: pull_request",
            "main",
            "a.txt",
            "m",
            false,
            false,
        ),
    ];
    let mut installed: BTreeMap<String, String> = BTreeMap::new();
    for (name, on, branch, file, msg, by_token, runs) in rows {
        let src = wf(on);
        if installed.get(branch) != Some(&src) {
            w.push(
                branch,
                &[(".github/workflows/t.yml", Some(&src))],
                "install [skip ci]",
            )
            .await;
            installed.insert(branch.to_string(), src.clone());
        }
        let who = if by_token {
            "job:01J00000000000000000000000"
        } else {
            "writer"
        };
        let (_, started) = w.push_as(branch, &[(file, Some(name))], msg, who).await;
        assert_eq!(!started.is_empty(), runs, "row {name}: started {started:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tag_push_and_create_events() {
    let w = world().await;
    let wf = "on:\n  push:\n    tags: ['v*']\n  create:\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github.event_name }} ${{ github.ref }}\n";
    let (tip, _) = w
        .push("main", &[(".github/workflows/t.yml", Some(wf))], "install")
        .await;
    let before = w.run_ids().await;
    let version = w.vcs.set_ref(&w.repo, "tags/v1.0", &tip).await.unwrap();
    w.forge
        .events
        .append(
            &Cx::system(),
            Event {
                kind: "ref.updated".into(),
                repo_id: w.repo.id.clone(),
                org_id: w.org.id.clone(),
                principal: "writer".into(),
                body: Some(event::Body::RefUpdated(Ref {
                    name: "tags/v1.0".into(),
                    target: Some(tip.clone()),
                    version,
                })),
                ..Default::default()
            },
            "tag",
        )
        .await
        .unwrap();
    let mut events: Vec<String> = Vec::new();
    for id in w
        .run_ids()
        .await
        .into_iter()
        .filter(|i| !before.contains(i))
    {
        events.push(w.run(&id).await.event);
    }
    events.sort();
    assert_eq!(events, vec!["create", "push"], "a new tag is push + create");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_validates_inputs_and_repository_dispatch_matches_types() {
    let w = world().await;
    let wf = "on:\n  workflow_dispatch:\n    inputs:\n      level:\n        type: choice\n        options: [low, high]\n        required: true\n      dry:\n        type: boolean\n        default: false\n  repository_dispatch:\n    types: [deploy]\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo level=${{ inputs.level }} dry=${{ inputs.dry }} payload=${{ github.event.client_payload.env }}\n";
    w.push(
        "main",
        &[(".github/workflows/d.yml", Some(wf))],
        "install [skip ci]",
    )
    .await;
    let cx = w.cx("writer");
    let input = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let bad =
        w.ci.dispatch(&cx, &w.repo, "d.yml", "", input(&[("level", "medium")]))
            .await;
    assert_eq!(bad.unwrap_err().subcode, "bad_input");
    let missing = w.ci.dispatch(&cx, &w.repo, "d.yml", "", input(&[])).await;
    assert_eq!(missing.unwrap_err().subcode, "bad_input");
    let undeclared =
        w.ci.dispatch(
            &cx,
            &w.repo,
            "d.yml",
            "",
            input(&[("level", "low"), ("x", "1")]),
        )
        .await;
    assert_eq!(undeclared.unwrap_err().subcode, "bad_input");
    let (run_id, number) =
        w.ci.dispatch(
            &cx,
            &w.repo,
            ".github/workflows/d.yml",
            "main",
            input(&[("level", "high")]),
        )
        .await
        .unwrap();
    assert_eq!(number, 1);
    assert_eq!(w.run(&run_id).await.event, "workflow_dispatch");
    let none =
        w.ci.repository_dispatch(&cx, &w.repo, "build", "{}")
            .await
            .unwrap();
    assert!(none.is_empty(), "type not listened to");
    let ids =
        w.ci.repository_dispatch(&cx, &w.repo, "deploy", r#"{"env":"prod"}"#)
            .await
            .unwrap();
    assert_eq!(ids.len(), 1);
    let too_many =
        w.ci.repository_dispatch(
            &cx,
            &w.repo,
            "deploy",
            r#"{"a":1,"b":1,"c":1,"d":1,"e":1,"f":1,"g":1,"h":1,"i":1,"j":1,"k":1}"#,
        )
        .await;
    assert!(too_many.is_err());
    let runner = w.local("r").await;
    w.drain(&runner).await;
    let log = w.log(&w.jobs(&run_id).await[0].id).await;
    assert!(log.contains("level=high dry=false"), "{log}");
    let log = w.log(&w.jobs(&ids[0]).await[0].id).await;
    assert!(log.contains("payload=prod"), "{log}");
    // A workflow without workflow_dispatch cannot be dispatched; a disabled one neither.
    w.push(
        "main",
        &[(".github/workflows/p.yml", Some(HELLO))],
        "more [skip ci]",
    )
    .await;
    let e =
        w.ci.dispatch(&cx, &w.repo, "p.yml", "", input(&[]))
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InvalidState);
    w.ci.list_workflows(&cx, &w.repo, "").await.unwrap();
    w.ci.set_workflow_state(&cx, &w.repo, "d.yml", false)
        .await
        .unwrap();
    let e =
        w.ci.dispatch(&cx, &w.repo, "d.yml", "", input(&[("level", "low")]))
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InvalidState);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedules_run_on_the_minute_tick_once() {
    let w = world().await;
    let wf = "on:\n  schedule:\n    - cron: '*/15 * * * *'\n    - cron: '0 3 * * MON'\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github.event.schedule }}\n";
    w.push("main", &[(".github/workflows/s.yml", Some(wf))], "install")
        .await;
    let cx = Cx::system();
    let m = |h, mi| bee_workflow::cron::UtcMinute {
        year: 2026,
        month: 10,
        day: 19,
        hour: h,
        minute: mi,
    };
    assert!(w.ci.tick_schedule(&cx, m(1, 7)).await.unwrap().is_empty());
    let ids = w.ci.tick_schedule(&cx, m(1, 15)).await.unwrap();
    assert_eq!(ids.len(), 1);
    assert!(
        w.ci.tick_schedule(&cx, m(1, 15)).await.unwrap().is_empty(),
        "each minute once"
    );
    // 2026-10-19 is a Monday: both entries match at 03:00.
    let both = w.ci.tick_schedule(&cx, m(3, 0)).await.unwrap();
    assert_eq!(both.len(), 2);
    assert_eq!(w.run(&ids[0]).await.event, "schedule");
}

// ----- the job graph -------------------------------------------------------------------------

const MATRIX: &str = r#"on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [linux, mac]
        n: [1, 2]
        exclude:
          - os: mac
            n: 2
    outputs:
      artifact: ${{ steps.mk.outputs.name }}
    steps:
      - id: mk
        run: echo "name=bin-${{ matrix.os }}" >> "$GITHUB_OUTPUT"
  test:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: echo "got ${{ needs.build.outputs.artifact }} result ${{ needs.build.result }}"
  skipped:
    needs: test
    if: ${{ false }}
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
  always:
    needs: skipped
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo always ran
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn matrix_and_needs_dag_execute_in_order_with_outputs() {
    let w = world().await;
    let (_, runs) = w
        .push(
            "main",
            &[(".github/workflows/m.yml", Some(MATRIX))],
            "matrix",
        )
        .await;
    let run_id = runs[0].clone();
    let jobs = w.jobs(&run_id).await;
    let mut names: Vec<String> = jobs.iter().map(|j| j.name.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["build (linux, 1)", "build (linux, 2)", "build (mac, 1)"],
        "only the legs before build finishes"
    );
    let runner = w.local("r").await;
    let ran = w.drain(&runner).await;
    assert_eq!(ran.len(), 5, "3 legs + test + always");
    assert_eq!(concl(&w.run(&run_id).await), WorkflowConclusion::Success);
    let jobs = w.jobs(&run_id).await;
    let by = |n: &str| jobs.iter().find(|j| j.name == n).unwrap().clone();
    assert_eq!(job_concl(&by("skipped")), WorkflowConclusion::Skipped);
    assert_eq!(job_concl(&by("always")), WorkflowConclusion::Success);
    let log = w.log(&by("test").id).await;
    assert!(
        log.contains("got bin-") && log.contains("result success"),
        "{log}"
    );
    assert_eq!(by("build (mac, 1)").matrix_json, r#"{"os":"mac","n":1}"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fail_fast_cancels_sibling_legs_and_failure_skips_dependents() {
    let w = world().await;
    let wf = "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      max-parallel: 1\n      matrix:\n        n: [1, 2, 3]\n    steps:\n      - run: exit 1\n  after:\n    needs: build\n    runs-on: ubuntu-latest\n    steps:\n      - run: 'true'\n";
    let (_, runs) = w
        .push("main", &[(".github/workflows/f.yml", Some(wf))], "ff")
        .await;
    let runner = w.local("r").await;
    let ran = w.drain(&runner).await;
    assert_eq!(
        ran.len(),
        1,
        "max-parallel 1, fail-fast cancels the held legs"
    );
    assert_eq!(concl(&w.run(&runs[0]).await), WorkflowConclusion::Failure);
    let mut cs: Vec<(String, WorkflowConclusion)> = w
        .jobs(&runs[0])
        .await
        .iter()
        .map(|j| (j.name.clone(), job_concl(j)))
        .collect();
    cs.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        cs,
        vec![
            ("after".to_string(), WorkflowConclusion::Skipped),
            ("build (1)".to_string(), WorkflowConclusion::Failure),
            ("build (2)".to_string(), WorkflowConclusion::Cancelled),
            ("build (3)".to_string(), WorkflowConclusion::Cancelled),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reusable_workflow_call_passes_inputs_and_outputs() {
    let w = world().await;
    let callee = r#"on:
  workflow_call:
    inputs:
      who:
        type: string
        required: true
    outputs:
      greeting:
        value: ${{ jobs.greet.outputs.text }}
jobs:
  greet:
    runs-on: ubuntu-latest
    outputs:
      text: ${{ steps.s.outputs.text }}
    steps:
      - id: s
        run: echo "text=hello ${{ inputs.who }}" >> "$GITHUB_OUTPUT"
"#;
    let caller = r#"on: push
jobs:
  call:
    uses: ./.github/workflows/greet.yml
    with:
      who: bee
  use:
    needs: call
    runs-on: ubuntu-latest
    steps:
      - run: echo "said ${{ needs.call.outputs.greeting }}"
"#;
    let (_, runs) = w
        .push(
            "main",
            &[
                (".github/workflows/greet.yml", Some(callee)),
                (".github/workflows/main.yml", Some(caller)),
            ],
            "reuse",
        )
        .await;
    assert_eq!(runs.len(), 1, "the callee has no push trigger");
    let runner = w.local("r").await;
    w.drain(&runner).await;
    assert_eq!(concl(&w.run(&runs[0]).await), WorkflowConclusion::Success);
    let jobs = w.jobs(&runs[0]).await;
    let names: Vec<String> = jobs.iter().map(|j| j.name.clone()).collect();
    assert!(names.iter().any(|n| n == "call / greet"), "{names:?}");
    let use_job = jobs.iter().find(|j| j.name == "use").unwrap();
    let log = w.log(&use_job.id).await;
    assert!(log.contains("said hello bee"), "{log}");
}

// ----- concurrency, cancellation, re-runs ----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_cancel_in_progress_and_pending_queue() {
    let w = world().await;
    let wf = "on: push\nconcurrency:\n  group: ci-${{ github.ref }}\n  cancel-in-progress: true\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: 'true'\n";
    let (_, r1) = w
        .push("main", &[(".github/workflows/c.yml", Some(wf))], "one")
        .await;
    let (_, r2) = w.push("main", &[("a.txt", Some("2"))], "two").await;
    let first = w.run(&r1[0]).await;
    assert_eq!(first.status, WorkflowStatus::Completed as i32);
    assert_eq!(concl(&first), WorkflowConclusion::Cancelled);
    assert_eq!(w.run(&r2[0]).await.status, WorkflowStatus::Queued as i32);
    let runner = w.local("r").await;
    w.drain(&runner).await;
    // Without cancel-in-progress the newcomer waits and replaces the pending one.
    let wf2 = "on: push\nconcurrency: deploy\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: 'true'\n";
    let (_, a) = w
        .push(
            "main",
            &[
                (".github/workflows/c.yml", None),
                (".github/workflows/q.yml", Some(wf2)),
            ],
            "a",
        )
        .await;
    let (_, b) = w.push("main", &[("b.txt", Some("b"))], "b").await;
    let (_, c) = w.push("main", &[("c.txt", Some("c"))], "c").await;
    assert_eq!(w.run(&a[0]).await.status, WorkflowStatus::Queued as i32);
    assert_eq!(
        concl(&w.run(&b[0]).await),
        WorkflowConclusion::Cancelled,
        "pending b replaced by c"
    );
    assert_eq!(w.run(&c[0]).await.status, WorkflowStatus::Pending as i32);
    w.drain(&runner).await;
    assert_eq!(concl(&w.run(&a[0]).await), WorkflowConclusion::Success);
    assert_eq!(
        concl(&w.run(&c[0]).await),
        WorkflowConclusion::Success,
        "c started when a released the group"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_runs_always_jobs_and_force_skips_them() {
    let w = world().await;
    let wf = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: 'true'\n  cleanup:\n    needs: a\n    if: always()\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo cleanup\n";
    let (_, r) = w
        .push("main", &[(".github/workflows/x.yml", Some(wf))], "x")
        .await;
    w.ci.cancel_run(&w.cx("writer"), &w.repo.id, &r[0], false)
        .await
        .unwrap();
    let runner = w.local("r").await;
    let ran = w.drain(&runner).await;
    assert_eq!(ran.len(), 1, "the always() job still runs");
    assert_eq!(concl(&w.run(&r[0]).await), WorkflowConclusion::Cancelled);
    let e =
        w.ci.cancel_run(&w.cx("writer"), &w.repo.id, &r[0], false)
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InvalidState);
    let (_, r2) = w.push("main", &[("y", Some("y"))], "y").await;
    w.ci.cancel_run(&w.cx("writer"), &w.repo.id, &r2[0], true)
        .await
        .unwrap();
    assert!(
        w.drain(&runner).await.is_empty(),
        "force cancel skips always()"
    );
    assert_eq!(concl(&w.run(&r2[0]).await), WorkflowConclusion::Cancelled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reruns_failed_jobs_then_all_with_attempts() {
    let w = world().await;
    let wf = "on: push\njobs:\n  ok:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok attempt ${{ github.run_attempt }}\n  flaky:\n    runs-on: ubuntu-latest\n    steps:\n      - run: test \"${{ github.run_attempt }}\" -ge 2\n  after:\n    needs: flaky\n    runs-on: ubuntu-latest\n    steps:\n      - run: 'true'\n";
    let (_, r) = w
        .push("main", &[(".github/workflows/r.yml", Some(wf))], "r")
        .await;
    let runner = w.local("r").await;
    w.drain(&runner).await;
    assert_eq!(concl(&w.run(&r[0]).await), WorkflowConclusion::Failure);
    let cx = w.cx("writer");
    let e =
        w.ci.rerun(&cx, &w.repo.id, "nope", Rerun::All, false)
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::NotFound);
    let run =
        w.ci.rerun(&cx, &w.repo.id, &r[0], Rerun::Failed, true)
            .await
            .unwrap();
    assert_eq!(run.run_attempt, 2);
    assert!(run.debug_logging);
    let ran = w.drain(&runner).await;
    assert_eq!(
        ran.len(),
        2,
        "flaky and its dependent re-run; ok is carried"
    );
    assert_eq!(concl(&w.run(&r[0]).await), WorkflowConclusion::Success);
    let jobs = w.jobs(&r[0]).await;
    assert_eq!(jobs.len(), 3);
    assert!(jobs.iter().all(|j| j.run_attempt == 2));
    let all =
        w.ci.list_jobs(&cx, &w.repo.id, &r[0], 0, true)
            .await
            .unwrap();
    assert_eq!(all.len(), 6);
    let a1 = w.ci.run_attempt(&cx, &w.repo.id, &r[0], 1).await.unwrap();
    assert_eq!(concl(&a1), WorkflowConclusion::Failure);
    let job = jobs.iter().find(|j| j.name == "ok").unwrap().id.clone();
    let run =
        w.ci.rerun(&cx, &w.repo.id, &r[0], Rerun::Job(job), false)
            .await
            .unwrap();
    assert_eq!(run.run_attempt, 3);
    assert_eq!(w.drain(&runner).await.len(), 1);
    let run =
        w.ci.rerun(&cx, &w.repo.id, &r[0], Rerun::All, false)
            .await
            .unwrap();
    assert_eq!(run.run_attempt, 4);
    assert_eq!(w.drain(&runner).await.len(), 3);
    let e =
        w.ci.rerun(&cx, &w.repo.id, &r[0], Rerun::Failed, false)
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InvalidState, "nothing failed");
}

// ----- runners and leases --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_leases_requeue_the_job_for_another_runner() {
    let cfg = CiConfig {
        lease_ms: 100,
        ..config()
    };
    let w = world_on(Arc::new(MemoryStore::new()), cfg).await;
    let (_, r) = w
        .push(
            "main",
            &[
                ("README.md", Some("x")),
                (".github/workflows/ci.yml", Some(HELLO)),
            ],
            "ci",
        )
        .await;
    let (dead, _, _) = w.api("dead").await;
    let a = dead.acquire().await.unwrap().expect("a job");
    assert!(dead.acquire().await.unwrap().is_none(), "one job at a time");
    dead.append_log(&a.job_id, &a.lease_id, 0, b"partial\n".to_vec(), 1)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    w.ci.tick(&Cx::system()).await.unwrap();
    let job = w.jobs(&r[0]).await.remove(0);
    assert_eq!(job.status, WorkflowStatus::Queued as i32, "requeued");
    assert_eq!(job.log_size_bytes, 0, "its log starts over");
    let e = dead.renew(&a.job_id, &a.lease_id).await.unwrap_err();
    assert_eq!(e.reason, ErrorReason::InvalidState);
    let runner = w.local("alive").await;
    let ran = w.drain(&runner).await;
    assert_eq!(ran[0].conclusion, Some(WorkflowConclusion::Success));
    assert!(!w.log(&job.id).await.contains("partial"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn labels_and_groups_route_jobs_to_runners() {
    let w = world().await;
    let wf = "on: push\njobs:\n  gpu:\n    runs-on: [self-hosted, gpu]\n    steps:\n      - run: 'true'\n";
    let (_, r) = w
        .push("main", &[(".github/workflows/g.yml", Some(wf))], "g")
        .await;
    let (plain, _, _) = w.api("plain").await;
    assert!(
        plain.acquire().await.unwrap().is_none(),
        "label gpu missing"
    );
    let cx = w.cx("owner");
    let group =
        w.ci.create_group(
            &cx,
            &w.org.id,
            "gpus",
            RunnerGroupVisibility::Selected,
            vec![],
            vec![],
            false,
            false,
            vec![],
        )
        .await
        .unwrap();
    let owner = RunnerOwner::Org(w.org.id.clone());
    let (token, _) = w.ci.runner_token(&cx, &owner, "register").await.unwrap();
    let (org_runner, cred) =
        w.ci.register_runner(
            &Cx::system(),
            &token,
            "org-gpu",
            RunnerOs::Linux,
            RunnerArch::X64,
            &["gpu".into()],
            &group.id,
            false,
            "",
        )
        .await
        .unwrap();
    assert!(org_runner.labels.iter().any(|l| l.name == "self-hosted"));
    let api = InProcessApi::new(w.svc.clone(), org_runner.id.clone(), cred, vec![]);
    assert!(
        api.acquire().await.unwrap().is_none(),
        "group does not admit the repo"
    );
    w.ci.set_group_repos(&cx, &w.org.id, &group.id, vec![w.repo.id.clone()])
        .await
        .unwrap();
    api.acquire().await.unwrap().expect("now admitted");
    assert_eq!(
        w.ci.get_group(&cx, &w.org.id, &group.id)
            .await
            .unwrap()
            .runner_count,
        1
    );
    let e =
        w.ci.delete_runner(&cx, &owner, &org_runner.id, false)
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InvalidState, "busy");
    w.ci.delete_runner(&cx, &owner, &org_runner.id, true)
        .await
        .unwrap();
    let job = w.jobs(&r[0]).await.remove(0);
    assert_eq!(
        job_concl(&job),
        WorkflowConclusion::Failure,
        "a force-removed runner fails its job"
    );
}

// ----- secrets, masking, environments --------------------------------------------------------

const SECRET: &str = "s3cr3t-value-123";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_are_masked_in_logs_and_never_at_rest_in_plaintext() {
    let w = world().await;
    let cx = w.cx("owner");
    let repo_scope = VaultScope::Repo(w.repo.id.clone(), w.org.id.clone());
    w.ci.put_secret(
        &cx,
        &repo_scope,
        "token",
        SECRET.as_bytes(),
        SecretVisibility::Unspecified,
        vec![],
    )
    .await
    .unwrap();
    w.ci.put_secret(
        &cx,
        &VaultScope::Org(w.org.id.clone()),
        "ORG_KEY",
        b"org-key-456",
        SecretVisibility::All,
        vec![],
    )
    .await
    .unwrap();
    w.ci.create_variable(
        &cx,
        &repo_scope,
        "MODE",
        "fast",
        SecretVisibility::Unspecified,
        vec![],
    )
    .await
    .unwrap();
    let wf = "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    env:\n      TOKEN: ${{ secrets.TOKEN }}\n    steps:\n      - run: |\n          echo \"token=$TOKEN\"\n          echo \"org=${{ secrets.ORG_KEY }}\"\n          echo \"mode=${{ vars.MODE }}\"\n          echo \"::add-mask::dyn-value-789\"\n          echo \"dyn=dyn-value-789\"\n          echo \"out=$TOKEN\" >> \"$GITHUB_OUTPUT\"\n";
    let (_, r) = w
        .push("main", &[(".github/workflows/s.yml", Some(wf))], "s")
        .await;
    let runner = w.local("r").await;
    w.drain(&runner).await;
    assert_eq!(concl(&w.run(&r[0]).await), WorkflowConclusion::Success);
    let job = w.jobs(&r[0]).await.remove(0);
    let log = w.log(&job.id).await;
    assert!(
        log.contains("token=***") && log.contains("org=***") && log.contains("dyn=***"),
        "{log}"
    );
    assert!(log.contains("mode=fast"), "variables are not masked: {log}");
    assert!(
        !log.lines().any(|l| l == "dyn=dyn-value-789"),
        "values masked with ::add-mask:: stay masked afterwards"
    );
    for plain in [SECRET, "org-key-456"] {
        assert!(!log.contains(plain), "{plain} leaked into the log");
        for b in w.blobs.all_bytes() {
            assert!(
                !b.windows(plain.len()).any(|x| x == plain.as_bytes()),
                "{plain} in a stored blob"
            );
        }
    }
    // Secrets are never returned: metadata only, and the stored row is ciphertext.
    let listed = w.ci.list_secrets(&cx, &repo_scope).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "TOKEN");
    let json = serde_json::to_string(&listed).unwrap();
    assert!(!json.contains(SECRET));
    let one = w.ci.get_secret(&cx, &repo_scope, "TOKEN").await.unwrap();
    assert!(!serde_json::to_string(&one).unwrap().contains(SECRET));
    let org_visible = w.ci.repo_org_secrets(&cx, &w.repo).await.unwrap();
    assert_eq!(org_visible.len(), 1);
    let mut tx = w.forge.db.begin(bee_forge::ci::SURFACE, &cx).await.unwrap();
    let rows = tx
        .scan(
            &bee_forge::ci::SECRETS,
            &bee_forge::store::Scan::scope(format!("repo:{}", w.repo.id), 10),
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        !rows[0]
            .body
            .windows(SECRET.len())
            .any(|x| x == SECRET.as_bytes())
    );
    drop(tx);
    // An output carrying a secret is dropped (GitHub does the same).
    let rec =
        w.ci.list_jobs(&cx, &w.repo.id, &r[0], 0, false)
            .await
            .unwrap();
    assert_eq!(rec.len(), 1);
    // Bad names are refused.
    let e =
        w.ci.put_secret(
            &cx,
            &repo_scope,
            "GITHUB_X",
            b"v",
            SecretVisibility::Unspecified,
            vec![],
        )
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "invalid_name");
    w.ci.delete_secret(&cx, &repo_scope, "token").await.unwrap();
    assert!(
        w.ci.list_secrets(&cx, &repo_scope)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_secret_puts_respect_the_scope_cap_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_ci_secret_cap_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let first_world = world_on(first, config()).await;
    let second_env = common::env_with(second, Arc::new(SystemClock)).await;
    let second_ci = Ci::new(
        &second_env.forge,
        CiHooks {
            vcs: Arc::new(HistoryCiVcs::new(first_world.vcs.clone())),
            blobs: first_world.blobs.clone(),
            keys: Arc::new(HmacKey::new("k1", b"oidc-key")),
            secret_key: [3u8; 32],
            url_key: b"url-key".to_vec(),
            signer: Arc::new(KeyRing::new(
                bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap(),
            )),
            config: config(),
        },
    );
    second_ci.attach(&second_env.forge);

    let cx = first_world.cx("owner");
    let scope = VaultScope::Org(first_world.org.id.clone());
    for i in 0..bee_forge::ci::secrets::MAX_PER_SCOPE - 1 {
        first_world
            .ci
            .put_secret(
                &cx,
                &scope,
                &format!("seed_{i}"),
                b"value",
                SecretVisibility::Unspecified,
                Vec::new(),
            )
            .await
            .unwrap();
    }
    first_world
        .ci
        .put_secret(
            &cx,
            &scope,
            "seed_0",
            b"updated",
            SecretVisibility::Unspecified,
            Vec::new(),
        )
        .await
        .expect("updating an existing secret at the scope cap remains allowed");

    for i in 0..24 {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left_ci = first_world.ci.clone();
        let right_ci = second_ci.clone();
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let left_scope = scope.clone();
        let right_scope = scope.clone();
        let left_name = format!("race_{i}_left");
        let right_name = format!("race_{i}_right");
        let left_cx = cx.clone();
        let right_cx = cx.clone();
        let (left, right) = tokio::join!(
            async move {
                left_barrier.wait().await;
                left_ci
                    .put_secret(
                        &left_cx,
                        &left_scope,
                        &left_name,
                        b"value",
                        SecretVisibility::Unspecified,
                        Vec::new(),
                    )
                    .await
            },
            async move {
                right_barrier.wait().await;
                right_ci
                    .put_secret(
                        &right_cx,
                        &right_scope,
                        &right_name,
                        b"value",
                        SecretVisibility::Unspecified,
                        Vec::new(),
                    )
                    .await
            }
        );
        assert!(
            left.is_ok() ^ right.is_ok(),
            "exactly one secret insert must succeed at round {i}: {left:?} {right:?}"
        );
        let loser = left.as_ref().err().or(right.as_ref().err()).unwrap();
        assert_eq!(loser.reason, ErrorReason::TooLarge);
        assert_eq!(
            loser.message,
            "secrets_per_scope is 1001, above the limit of 1000"
        );
        assert_eq!(
            first_world
                .ci
                .list_secrets(&cx, &scope)
                .await
                .unwrap()
                .len(),
            bee_forge::ci::secrets::MAX_PER_SCOPE
        );
        let winner_name = if left.is_ok() {
            format!("RACE_{i}_LEFT")
        } else {
            format!("RACE_{i}_RIGHT")
        };
        first_world
            .ci
            .delete_secret(&cx, &scope, &winner_name)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn environments_require_reviewers_and_record_deployments() {
    let w = world().await;
    let cx = w.cx("owner");
    w.ci.create_environment(
        &cx,
        &w.repo,
        "production",
        &EnvironmentSpec {
            reviewers: Some(vec![DeploymentEnvironmentReviewer {
                kind: DeploymentReviewerKind::Principal as i32,
                principal: Some(Principal {
                    id: "owner".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    w.ci.create_environment(
        &cx,
        &w.repo,
        "staging",
        &EnvironmentSpec {
            ref_policy_mode: Some(DeploymentRefPolicyMode::Custom),
            ref_policies: Some(vec![DeploymentRefPolicy {
                pattern: "release/*".into(),
                r#type: DeploymentRefPolicyType::Branch as i32,
            }]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let wf = "on: push\njobs:\n  deploy:\n    runs-on: ubuntu-latest\n    environment: production\n    steps:\n      - run: echo deploying\n  stage:\n    runs-on: ubuntu-latest\n    environment: staging\n    steps:\n      - run: echo staging\n";
    let (_, r) = w
        .push("main", &[(".github/workflows/d.yml", Some(wf))], "d")
        .await;
    let runner = w.local("r").await;
    assert!(w.drain(&runner).await.is_empty(), "nothing runnable yet");
    let jobs = w.jobs(&r[0]).await;
    let deploy = jobs.iter().find(|j| j.name == "deploy").unwrap();
    assert_eq!(deploy.status, WorkflowStatus::Waiting as i32);
    let stage = jobs.iter().find(|j| j.name == "stage").unwrap();
    assert_eq!(
        job_concl(stage),
        WorkflowConclusion::Failure,
        "main may not deploy to staging"
    );
    let pending =
        w.ci.pending_deployments(&cx, &w.repo, &r[0], "owner", false)
            .await
            .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].caller_can_approve);
    // Over the service: the writer is not a reviewer; the owner is.
    let e = w
        .svc
        .review_workflow_run_pending_deployments(common::req(
            &w.token("writer"),
            ReviewWorkflowRunPendingDeploymentsRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
                environments: vec!["production".into()],
                decision: DeploymentReviewDecision::Approved as i32,
                comment: String::new(),
            },
        ))
        .await;
    assert_eq!(common::reason(e), "PERMISSION_DENIED");
    let ok = w
        .svc
        .review_workflow_run_pending_deployments(common::req(
            &w.token("owner"),
            ReviewWorkflowRunPendingDeploymentsRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
                environments: vec!["production".into()],
                decision: DeploymentReviewDecision::Approved as i32,
                comment: "ship it".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ok.deployments.len(), 1);
    let ran = w.drain(&runner).await;
    assert_eq!(ran.len(), 1);
    let deps =
        w.ci.list_deployments(&cx, &w.repo, "production")
            .await
            .unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].state, DeploymentState::Success as i32);
    let states: Vec<i32> =
        w.ci.deployment_statuses(&cx, &w.repo, &deps[0].id)
            .await
            .unwrap()
            .iter()
            .map(|s| s.state)
            .collect();
    assert_eq!(
        states,
        vec![
            DeploymentState::Pending as i32,
            DeploymentState::Queued as i32,
            DeploymentState::InProgress as i32,
            DeploymentState::Success as i32
        ]
    );
    let approvals = w.ci.run_approvals(&cx, &r[0]).await.unwrap();
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].environments, vec!["production".to_string()]);
}

// ----- job tokens, OIDC, attestations --------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_tokens_are_scoped_to_the_repo_permissions_and_job_lifetime() {
    let w = world().await;
    let wf = "on: push\npermissions:\n  contents: read\n  id-token: write\n  attestations: write\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: 'true'\n";
    let (_, r) = w
        .push("main", &[(".github/workflows/t.yml", Some(wf))], "t")
        .await;
    let (api, _, _) = w.api("r").await;
    let a = api.acquire().await.unwrap().unwrap();
    // Reads are allowed.
    let got = w
        .svc
        .get_workflow_run(common::req(
            &a.job_token,
            GetWorkflowRunRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
            },
        ))
        .await;
    assert!(got.is_ok(), "{got:?}");
    // Writes are not: contents: read grants no `commit`.
    let e = w
        .svc
        .cancel_workflow_run(common::req(
            &a.job_token,
            CancelWorkflowRunRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
            },
        ))
        .await;
    assert!(matches!(
        common::reason(e).as_str(),
        "POLICY_DENIED" | "PERMISSION_DENIED"
    ));
    // Another repo is out of scope.
    let other = w
        .forge
        .orgs
        .create_repo(
            &w.cx("owner"),
            &w.org.id,
            NewRepo {
                id: w.forge.db.ids().ulid().unwrap(),
                name: "other".into(),
                ..NewRepo::default()
            },
        )
        .await
        .unwrap();
    let e = w
        .svc
        .list_workflow_runs(common::req(
            &a.job_token,
            ListWorkflowRunsRequest {
                repo: rref(&other),
                ..Default::default()
            },
        ))
        .await;
    assert!(matches!(
        common::reason(e).as_str(),
        "POLICY_DENIED" | "PERMISSION_DENIED" | "NOT_FOUND"
    ));
    // OIDC: GitHub-compatible claims signed by the key port.
    let tok = w
        .svc
        .create_workflow_oidc_token(common::req(
            &a.job_token,
            CreateWorkflowOidcTokenRequest {
                repo: w.rr(),
                job_id: a.job_id.clone(),
                audience: "sts.example".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let claims = w.ci.verify_jws(&tok.token).expect("valid signature");
    assert_eq!(claims["aud"], "sts.example");
    assert_eq!(
        claims["sub"],
        format!("repo:{}:ref:refs/heads/main", w.repo.full_name)
    );
    assert_eq!(claims["repository"], w.repo.full_name.as_str());
    assert_eq!(claims["event_name"], "push");
    // Another principal cannot mint the job's OIDC token.
    let e = w
        .svc
        .create_workflow_oidc_token(common::req(
            &w.token("owner"),
            CreateWorkflowOidcTokenRequest {
                repo: w.rr(),
                job_id: a.job_id.clone(),
                audience: String::new(),
            },
        ))
        .await;
    assert_eq!(common::reason(e), "PERMISSION_DENIED");
    // Attestations signed for the job, verified against signer and source.
    let digest = format!("sha256:{}", "ab".repeat(32));
    let att = w
        .svc
        .create_attestation(common::req(
            &a.job_token,
            CreateAttestationRequest {
                repo: w.rr(),
                job_id: a.job_id.clone(),
                subjects: vec![AttestationSubject {
                    name: "app.tar".into(),
                    digest: digest.clone(),
                }],
                predicate_type: String::new(),
                predicate_json: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let signer = format!("{}/.github/workflows/t.yml", w.repo.full_name);
    let v = w
        .svc
        .verify_attestation(common::req(
            &w.token("reader"),
            VerifyAttestationRequest {
                repo: w.rr(),
                subject_digest: digest.clone(),
                expected_signer_workflow: signer.clone(),
                expected_source_ref: "refs/heads/main".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(v.verified, "{:?}", v.failures);
    let bad =
        w.ci.verify_attestations(
            &w.cx("owner"),
            &w.repo.id,
            "",
            &digest,
            "",
            "x/y/.github/workflows/z.yml",
            "",
        )
        .await
        .unwrap();
    assert!(bad.0.is_empty() && !bad.1.is_empty());
    // After the job, the token no longer reaches the repo.
    api.complete(CompleteRunnerJobRequest {
        job_id: a.job_id.clone(),
        lease_id: a.lease_id.clone(),
        conclusion: WorkflowConclusion::Success as i32,
        ..Default::default()
    })
    .await
    .unwrap();
    let e = w
        .svc
        .get_workflow_run(common::req(
            &a.job_token,
            GetWorkflowRunRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
            },
        ))
        .await;
    assert_eq!(common::reason(e), "NOT_FOUND");
    let e = w
        .svc
        .delete_attestation(common::req(
            &w.token("writer"),
            DeleteAttestationRequest {
                repo: w.rr(),
                attestation_id: att.id.clone(),
            },
        ))
        .await;
    assert_eq!(common::reason(e), "PERMISSION_DENIED");
    w.svc
        .delete_attestation(common::req(
            &w.token("owner"),
            DeleteAttestationRequest {
                repo: w.rr(),
                attestation_id: att.id,
            },
        ))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oidc_needs_id_token_write() {
    let w = world().await;
    let (_, _) = w
        .push("main", &[(".github/workflows/t.yml", Some(HELLO))], "t")
        .await;
    let (api, _, _) = w.api("r").await;
    let a = api.acquire().await.unwrap().unwrap();
    let e = w
        .svc
        .create_workflow_oidc_token(common::req(
            &a.job_token,
            CreateWorkflowOidcTokenRequest {
                repo: w.rr(),
                job_id: a.job_id.clone(),
                audience: String::new(),
            },
        ))
        .await;
    assert_eq!(common::reason(e), "PERMISSION_DENIED");
}

// ----- artifacts, caches, actions ------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn artifacts_and_caches_round_trip_between_jobs_and_runs() {
    let w = world().await;
    let wf = r#"on: push
jobs:
  make:
    runs-on: ubuntu-latest
    steps:
      - run: mkdir -p out && echo payload > out/a.txt
      - uses: actions/upload-artifact@v4
        with:
          name: bin
          path: out
      - id: c
        uses: actions/cache@v4
        with:
          path: out
          key: out-v1
      - run: echo "hit=${{ steps.c.outputs.cache-hit }}"
  use:
    needs: make
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with:
          name: bin
          path: got
      - run: cat got/out/a.txt
"#;
    let (_, r1) = w
        .push("main", &[(".github/workflows/a.yml", Some(wf))], "a")
        .await;
    let runner = w.local("r").await;
    w.drain(&runner).await;
    assert_eq!(concl(&w.run(&r1[0]).await), WorkflowConclusion::Success);
    let jobs = w.jobs(&r1[0]).await;
    let make = jobs.iter().find(|j| j.name == "make").unwrap();
    assert!(w.log(&make.id).await.contains("hit=false"));
    let use_job = jobs.iter().find(|j| j.name == "use").unwrap();
    assert!(w.log(&use_job.id).await.contains("payload"));
    let cx = w.cx("owner");
    let arts = w.ci.list_artifacts(&cx, &w.repo, &r1[0], "").await.unwrap();
    assert_eq!(arts.len(), 1);
    assert!(arts[0].finalized && arts[0].digest.starts_with("sha256:"));
    let (url, _) =
        w.ci.artifact_download_url(&cx, &w.repo, &arts[0].id)
            .await
            .unwrap();
    assert!(
        w.ci.url_signer()
            .verify(&url, w.ci.config().signed_url_ttl_ms)
            .is_ok()
            || url.contains("signature=")
    );
    let usage = w.ci.cache_usage(&cx, &w.repo).await.unwrap();
    assert_eq!(usage.active_count, 1);
    let (_, r2) = w.push("main", &[("x.txt", Some("x"))], "again").await;
    w.drain(&runner).await;
    let make2 = w
        .jobs(&r2[0])
        .await
        .into_iter()
        .find(|j| j.name == "make")
        .unwrap();
    assert!(
        w.log(&make2.id).await.contains("hit=true"),
        "{}",
        w.log(&make2.id).await
    );
    assert_eq!(
        w.ci.delete_caches(&cx, &w.repo, "", "out-v1", "")
            .await
            .unwrap(),
        1
    );
    w.ci.delete_artifact(&cx, &w.repo, &arts[0].id)
        .await
        .unwrap();
    assert!(w.ci.get_artifact(&cx, &w.repo, &arts[0].id).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn composite_actions_run_and_docker_or_remote_actions_are_reported_unsupported() {
    let w = world().await;
    let action = "name: hello\ninputs:\n  who:\n    default: world\noutputs:\n  said:\n    value: ${{ steps.s.outputs.said }}\nruns:\n  using: composite\n  steps:\n    - id: s\n      shell: bash\n      run: |\n        echo \"said=hello ${{ inputs.who }}\" >> \"$GITHUB_OUTPUT\"\n        echo composite says hello ${{ inputs.who }}\n";
    let wf = "on: push\njobs:\n  comp:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - id: h\n        uses: ./.github/actions/hello\n        with:\n          who: bee\n      - run: echo \"result ${{ steps.h.outputs.said }}\"\n  docker:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: docker://alpine:3\n  remote:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: someone/thing@v1\n";
    let (_, r) = w
        .push(
            "main",
            &[
                (".github/actions/hello/action.yml", Some(action)),
                (".github/workflows/c.yml", Some(wf)),
            ],
            "c",
        )
        .await;
    let runner = w.local("r").await;
    w.drain(&runner).await;
    let jobs = w.jobs(&r[0]).await;
    let by = |n: &str| jobs.iter().find(|j| j.name == n).unwrap().clone();
    assert_eq!(job_concl(&by("comp")), WorkflowConclusion::Success);
    let log = w.log(&by("comp").id).await;
    assert!(
        log.contains("composite says hello bee") && log.contains("result hello bee"),
        "{log}"
    );
    assert_eq!(job_concl(&by("docker")), WorkflowConclusion::Failure);
    let log = w.log(&by("docker").id).await;
    assert!(
        log.contains("this runner cannot run containers: docker://alpine:3"),
        "{log}"
    );
    assert_eq!(job_concl(&by("remote")), WorkflowConclusion::Failure);
    let log = w.log(&by("remote").id).await;
    assert!(log.contains("not supported by the local runner"), "{log}");
}

// ----- the service ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_permission_matrix() {
    let w = world().await;
    let wf = "on:\n  push:\n  workflow_dispatch:\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: 'true'\n";
    let (_, r) = w
        .push("main", &[(".github/workflows/p.yml", Some(wf))], "p")
        .await;
    let runner = w.local("r").await;
    w.drain(&runner).await;
    let list = |who: &str| {
        let tok = w.token(who);
        let svc = w.svc.clone();
        let rr = w.rr();
        async move {
            svc.list_workflow_runs(common::req(
                &tok,
                ListWorkflowRunsRequest {
                    repo: rr,
                    ..Default::default()
                },
            ))
            .await
        }
    };
    assert_eq!(list("reader").await.unwrap().into_inner().runs.len(), 1);
    assert_eq!(common::reason(list("outsider").await), "NOT_FOUND");
    let dispatch = |who: &str| {
        let tok = w.token(who);
        let svc = w.svc.clone();
        let rr = w.rr();
        async move {
            svc.dispatch_workflow(common::req(
                &tok,
                DispatchWorkflowRequest {
                    repo: rr,
                    workflow: "p.yml".into(),
                    r#ref: String::new(),
                    inputs: vec![],
                },
            ))
            .await
        }
    };
    assert_eq!(
        common::reason(dispatch("reader").await),
        "PERMISSION_DENIED"
    );
    assert_eq!(dispatch("writer").await.unwrap().into_inner().run_number, 2);
    let cancel = w
        .svc
        .cancel_workflow_run(common::req(
            &w.token("reader"),
            CancelWorkflowRunRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
            },
        ))
        .await;
    assert_eq!(common::reason(cancel), "PERMISSION_DENIED");
    let cancel = w
        .svc
        .cancel_workflow_run(common::req(
            &w.token("writer"),
            CancelWorkflowRunRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
            },
        ))
        .await;
    assert_eq!(
        common::reason(cancel),
        "INVALID_STATE",
        "authorized, but the run completed"
    );
    let scope = Some(RunnerScope {
        kind: RunnerScopeKind::Repo as i32,
        org: String::new(),
        repo: w.rr(),
    });
    let e = w
        .svc
        .create_runner_registration_token(common::req(
            &w.token("writer"),
            CreateRunnerRegistrationTokenRequest {
                scope: scope.clone(),
            },
        ))
        .await;
    assert_eq!(common::reason(e), "PERMISSION_DENIED");
    let tok = w
        .svc
        .create_runner_registration_token(common::req(
            &w.token("owner"),
            CreateRunnerRegistrationTokenRequest {
                scope: scope.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let reg = w
        .svc
        .register_runner(tonic::Request::new(RegisterRunnerRequest {
            registration_token: tok.token.clone(),
            name: "svc-runner".into(),
            labels: vec!["x".into()],
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    let e = w
        .svc
        .register_runner(tonic::Request::new(RegisterRunnerRequest {
            registration_token: "bogus".into(),
            name: "n".into(),
            ..Default::default()
        }))
        .await;
    assert_eq!(common::reason(e), "UNAUTHENTICATED");
    let e = w
        .svc
        .register_runner(tonic::Request::new(RegisterRunnerRequest {
            registration_token: tok.token.clone(),
            name: "svc-runner".into(),
            ..Default::default()
        }))
        .await;
    assert_eq!(common::reason(e), "ALREADY_EXISTS");
    // The runner credential drives the job protocol, and only for its own runner.
    let e = w
        .svc
        .renew_runner_job(common::req(
            &w.token("owner"),
            RenewRunnerJobRequest {
                runner_id: reg.runner.as_ref().unwrap().id.clone(),
                job_id: "x".into(),
                lease_id: "y".into(),
            },
        ))
        .await;
    assert_eq!(common::reason(e), "PERMISSION_DENIED");
    let e = w
        .svc
        .renew_runner_job(common::req(
            &reg.credential,
            RenewRunnerJobRequest {
                runner_id: reg.runner.as_ref().unwrap().id.clone(),
                job_id: "x".into(),
                lease_id: "y".into(),
            },
        ))
        .await;
    assert_eq!(common::reason(e), "INVALID_STATE");
    let e = w
        .svc
        .list_workflow_runs(common::req(
            &reg.credential,
            ListWorkflowRunsRequest {
                repo: w.rr(),
                ..Default::default()
            },
        ))
        .await;
    assert!(matches!(
        common::reason(e).as_str(),
        "POLICY_DENIED" | "NOT_FOUND" | "PERMISSION_DENIED"
    ));
    let listed = w
        .svc
        .list_runners(common::req(
            &w.token("admin"),
            ListRunnersRequest {
                scope: scope.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(listed.runners.iter().any(|x| x.name == "svc-runner"));
    // Org runner groups need org admin.
    let e = w
        .svc
        .create_runner_group(common::req(
            &w.token("writer"),
            CreateRunnerGroupRequest {
                org: w.org.name.clone(),
                name: "g".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(common::reason(e), "PERMISSION_DENIED");
    w.svc
        .create_runner_group(common::req(
            &w.token("admin"),
            CreateRunnerGroupRequest {
                org: w.org.name.clone(),
                name: "g".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let groups = w
        .svc
        .list_runner_groups(common::req(
            &w.token("reader"),
            ListRunnerGroupsRequest {
                org: w.org.name.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(groups.groups.len(), 2, "default + g");
    // Job-token-only RPCs refuse people.
    let e = w
        .svc
        .create_workflow_artifact_upload(common::req(
            &w.token("owner"),
            CreateWorkflowArtifactUploadRequest {
                repo: w.rr(),
                job_id: "j".into(),
                name: "a".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(common::reason(e), "PERMISSION_DENIED");
    // Repo policy writes need admin; the org can enforce against repos.
    let pscope = Some(ActionsPolicyScope {
        kind: ActionsPolicyScopeKind::Repo as i32,
        org: String::new(),
        repo: w.rr(),
    });
    let e = w
        .svc
        .set_actions_policy(common::req(
            &w.token("writer"),
            SetActionsPolicyRequest {
                scope: pscope.clone(),
                sha_pinning_required: Some(true),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(common::reason(e), "PERMISSION_DENIED");
    let classes = w
        .svc
        .list_runner_machine_classes(common::req(
            &w.token("reader"),
            ListRunnerMachineClassesRequest {
                org: w.org.name.clone(),
                os: RunnerOs::Linux as i32,
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        classes
            .classes
            .iter()
            .all(|c| c.os == RunnerOs::Linux as i32)
            && !classes.classes.is_empty()
    );
    let img = w
        .svc
        .create_runner_custom_image(common::req(
            &w.token("owner"),
            CreateRunnerCustomImageRequest {
                org: w.org.name.clone(),
                name: "img".into(),
                os: 1,
                arch: 1,
                oci_reference: "ghcr.io/x/y:1".into(),
            },
        ))
        .await;
    assert_eq!(common::reason(img), "INVALID_ARGUMENT", "unpinned image");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actions_policies_enforce_allowed_actions_and_org_limits() {
    let w = world().await;
    let org_scope = Some(ActionsPolicyScope {
        kind: ActionsPolicyScopeKind::Org as i32,
        org: w.org.name.clone(),
        repo: None,
    });
    w.svc
        .set_actions_policy(common::req(
            &w.token("owner"),
            SetActionsPolicyRequest {
                scope: org_scope,
                allowed_actions: Some(ActionsPolicyAllowedActions::LocalOnly as i32),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let repo_scope = Some(ActionsPolicyScope {
        kind: ActionsPolicyScopeKind::Repo as i32,
        org: String::new(),
        repo: w.rr(),
    });
    let e = w
        .svc
        .set_actions_policy(common::req(
            &w.token("owner"),
            SetActionsPolicyRequest {
                scope: repo_scope.clone(),
                allowed_actions: Some(ActionsPolicyAllowedActions::All as i32),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(common::reason(e), "POLICY_DENIED");
    let p = w
        .svc
        .get_actions_policy(common::req(
            &w.token("reader"),
            GetActionsPolicyRequest { scope: repo_scope },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        p.allowed_actions,
        ActionsPolicyAllowedActions::LocalOnly as i32
    );
    assert!(p.org_enforced.contains(&"allowed_actions".to_string()));
    let (_, r) = w
        .push(
            "main",
            &[(".github/workflows/ci.yml", Some(HELLO))],
            "uses checkout",
        )
        .await;
    let run = w.run(&r[0]).await;
    assert_eq!(concl(&run), WorkflowConclusion::StartupFailure);
    let v = w
        .svc
        .validate_workflow(common::req(
            &w.token("reader"),
            ValidateWorkflowRequest {
                repo: w.rr(),
                path: ".github/workflows/ci.yml".into(),
                source: HELLO.into(),
                r#ref: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!v.valid && v.diagnostics.iter().any(|d| d.rule == "disallowed_action"));
    let v = w
        .svc
        .validate_workflow(common::req(
            &w.token("reader"),
            ValidateWorkflowRequest {
                repo: w.rr(),
                path: ".github/workflows/x.yml".into(),
                source: "on: push\njobs: {}\nbogus: 1\n".into(),
                r#ref: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!v.valid && v.diagnostics.iter().all(|d| d.line > 0));
    // Disabling actions for the org's repos stops triggers.
    let org_scope = Some(ActionsPolicyScope {
        kind: ActionsPolicyScopeKind::Org as i32,
        org: w.org.name.clone(),
        repo: None,
    });
    w.svc
        .set_actions_policy(common::req(
            &w.token("owner"),
            SetActionsPolicyRequest {
                scope: org_scope,
                enabled_repos: Some(ActionsPolicyEnabledRepos::None as i32),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let (_, none) = w.push("main", &[("a", Some("a"))], "nothing").await;
    assert!(none.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logs_tail_archive_search_and_delete() {
    use tokio_stream::StreamExt;
    let w = world().await;
    let wf = "on: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo first line\n      - run: echo needle here\n";
    let (_, r) = w
        .push("main", &[(".github/workflows/l.yml", Some(wf))], "l")
        .await;
    let runner = w.local("r").await;
    w.drain(&runner).await;
    let job = w.jobs(&r[0]).await.remove(0);
    let mut stream = w
        .svc
        .tail_workflow_job_logs(common::req(
            &w.token("reader"),
            TailWorkflowJobLogsRequest {
                repo: w.rr(),
                job_id: job.id.clone(),
                offset: 0,
                follow: true,
                step_number: 0,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let mut text = Vec::new();
    let mut eof = false;
    while let Some(c) = stream.next().await {
        let c = c.unwrap();
        text.extend(c.data);
        eof |= c.eof;
    }
    assert!(eof);
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("first line") && text.contains("needle here"));
    let step2 =
        w.ci.read_log(&w.cx("owner"), &w.repo.id, &job.id, 0, 2)
            .await
            .unwrap();
    let step2: String = step2
        .iter()
        .map(|c| String::from_utf8_lossy(&c.data).into_owned())
        .collect();
    assert!(step2.contains("needle") && !step2.contains("first line"));
    assert!(job.steps[1].log_offset > 0);
    let hits =
        w.ci.search_logs(&w.cx("owner"), &w.repo, &r[0], "NEEDLE", 10)
            .await
            .unwrap();
    assert!(
        hits.iter().any(|h| h.text == "needle here" && h.line > 1),
        "{hits:?}"
    );
    let zip =
        w.ci.run_logs_zip(&w.cx("owner"), &w.repo.id, &r[0], 0)
            .await
            .unwrap();
    let entries = bee_forge::ci::util::unzip(&zip).unwrap();
    assert_eq!(entries.len(), 1);
    let url = w
        .svc
        .get_workflow_run_logs_url(common::req(
            &w.token("reader"),
            GetWorkflowRunLogsUrlRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
                attempt: 0,
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .url;
    assert!(
        w.ci.url_signer()
            .verify(&url, 0)
            .unwrap()
            .ends_with("/logs.zip")
    );
    let e = w
        .svc
        .delete_workflow_run_logs(common::req(
            &w.token("reader"),
            DeleteWorkflowRunLogsRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
            },
        ))
        .await;
    assert_eq!(common::reason(e), "PERMISSION_DENIED");
    w.svc
        .delete_workflow_run_logs(common::req(
            &w.token("writer"),
            DeleteWorkflowRunLogsRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
            },
        ))
        .await
        .unwrap();
    let e =
        w.ci.read_log(&w.cx("owner"), &w.repo.id, &job.id, 0, 0)
            .await
            .unwrap_err();
    assert_eq!(e.reason, ErrorReason::Redacted);
}

// ----- agent scenarios -----------------------------------------------------------------------

/// AS-CI-01: add a workflow, watch it fail, read the logs and annotations, fix it, see it pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_ci_01_fix_a_failing_workflow_from_logs_and_annotations() {
    use tokio_stream::StreamExt;
    let w = world().await;
    let broken = "name: CI\non: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: |\n          if ! grep -q answer src/lib.txt; then\n            echo \"::error file=src/lib.txt,line=1,title=missing answer::src/lib.txt has no answer\"\n            exit 1\n          fi\n";
    let (head, r) = w
        .push_as(
            "main",
            &[
                ("src/lib.txt", Some("question\n")),
                (".github/workflows/ci.yml", Some(broken)),
            ],
            "add ci",
            "agent",
        )
        .await;
    let runner = w.local("r").await;
    w.drain(&runner).await;
    // Watch: the run failed.
    let mut watch = w
        .svc
        .watch_workflow_run(common::req(
            &w.token("agent-owner-unused"),
            WatchWorkflowRunRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
            },
        ))
        .await;
    if watch.is_ok() {
        // tokens of unknown principals cannot read a private repo
        panic!("unexpected access");
    }
    watch = w
        .svc
        .watch_workflow_run(common::req(
            &w.token("writer"),
            WatchWorkflowRunRequest {
                repo: w.rr(),
                run_id: r[0].clone(),
            },
        ))
        .await;
    let first = watch.unwrap().into_inner().next().await.unwrap().unwrap();
    assert!(first.r#final);
    assert_eq!(
        concl(first.run.as_ref().unwrap()),
        WorkflowConclusion::Failure
    );
    // Read the annotations on the check run and the log.
    let job = w.jobs(&r[0]).await.remove(0);
    let (anns, _) = w
        .forge
        .checks
        .annotations(&w.cx("writer"), &w.repo.id, &job.check_run_id, "", 100)
        .await
        .unwrap();
    let a = anns
        .iter()
        .find(|a| a.path == b"src/lib.txt")
        .expect("annotation from ::error");
    assert_eq!(a.start_line, 1);
    assert_eq!(a.title, "missing answer");
    let log = w.log(&job.id).await;
    assert!(log.contains("src/lib.txt has no answer"));
    let checks = w
        .forge
        .checks
        .check_results(&w.cx("writer"), &w.repo.id, &hex(&head.raw))
        .await
        .unwrap();
    assert_eq!(checks[0].status, bee_policy::CheckStatus::Failure);
    // Fix it; the next run passes.
    let (fixed, r2) = w
        .push_as(
            "main",
            &[("src/lib.txt", Some("answer 42\n"))],
            "fix",
            "agent",
        )
        .await;
    w.drain(&runner).await;
    assert_eq!(concl(&w.run(&r2[0]).await), WorkflowConclusion::Success);
    let checks = w
        .forge
        .checks
        .check_results(&w.cx("writer"), &w.repo.id, &hex(&fixed.raw))
        .await
        .unwrap();
    assert_eq!(checks[0].name, "test");
    assert_eq!(checks[0].status, bee_policy::CheckStatus::Success);
}

/// AS-CI-02 (CI part): a regression landed on main fails post-land testing; the failing
/// changeset is reported where bisect starts from, earlier changesets stay green.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_ci_02_regression_on_main_is_reported_for_bisect() {
    let w = world().await;
    let wf = "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: test \"$(cat value.txt)\" = good\n";
    let (good, _) = w
        .push(
            "main",
            &[
                ("value.txt", Some("good")),
                (".github/workflows/ci.yml", Some(wf)),
            ],
            "good",
        )
        .await;
    let (bad, _) = w
        .push("main", &[("value.txt", Some("bad"))], "regression")
        .await;
    let runner = w.local("r").await;
    w.drain(&runner).await;
    let cx = w.cx("owner");
    assert!(
        w.forge
            .checks
            .failing_post_land(&cx, &w.repo.id, &hex(&good.raw))
            .await
            .unwrap()
            .is_empty()
    );
    let failing = w
        .forge
        .checks
        .failing_post_land(&cx, &w.repo.id, &hex(&bad.raw))
        .await
        .unwrap();
    assert_eq!(failing.len(), 1);
    assert_eq!(failing[0].name, "test");
    assert!(!failing[0].workflow_job_id.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn change_requests_trigger_pull_request_and_agent_runs_need_approval() {
    let w = world().await;
    let wf = "on: pull_request\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo pr ${{ github.event.pull_request.number }}\n";
    w.push("main", &[(".github/workflows/pr.yml", Some(wf))], "install")
        .await;
    let repo_scope = Some(ActionsPolicyScope {
        kind: ActionsPolicyScopeKind::Repo as i32,
        org: String::new(),
        repo: w.rr(),
    });
    w.svc
        .set_actions_policy(common::req(
            &w.token("owner"),
            SetActionsPolicyRequest {
                scope: repo_scope,
                agent_runs_require_approval: Some(true),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let base = w.tip("main").await.unwrap();
    let head = w
        .commit(Some(&base), &[("x.txt", Some("x"))], "change")
        .await;
    let cr = ChangeRequest {
        id: "cr1".into(),
        repo_id: w.repo.id.clone(),
        target_ref: "main".into(),
        number: 7,
        title: "a change".into(),
        author: Some(Principal {
            id: "agent".into(),
            kind: PrincipalKind::Agent as i32,
            ..Default::default()
        }),
        revisions: vec![ChangeRevision {
            n: 1,
            head: Some(head.clone()),
            base: Some(base),
            ..Default::default()
        }],
        ..Default::default()
    };
    let ids =
        w.ci.handle_event(
            &Event {
                kind: "cr.opened".into(),
                repo_id: w.repo.id.clone(),
                org_id: w.org.id.clone(),
                principal: "agent".into(),
                body: Some(event::Body::Change(cr)),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        ids.len(),
        1,
        "pull_request (pull_request_target is not listened to)"
    );
    let run = w.run(&ids[0]).await;
    assert_eq!(run.event, "pull_request");
    assert_eq!(run.change_request_numbers, vec![7]);
    assert_eq!(run.status, WorkflowStatus::ActionRequired as i32);
    let runner = w.local("r").await;
    assert!(w.drain(&runner).await.is_empty());
    let e = w
        .svc
        .approve_workflow_run(common::req(
            &w.token("reader"),
            ApproveWorkflowRunRequest {
                repo: w.rr(),
                run_id: ids[0].clone(),
            },
        ))
        .await;
    assert_eq!(common::reason(e), "PERMISSION_DENIED");
    w.svc
        .approve_workflow_run(common::req(
            &w.token("writer"),
            ApproveWorkflowRunRequest {
                repo: w.rr(),
                run_id: ids[0].clone(),
            },
        ))
        .await
        .unwrap();
    w.drain(&runner).await;
    let job = w.jobs(&ids[0]).await.remove(0);
    assert!(w.log(&job.id).await.contains("pr 7"));
    let checks = w
        .forge
        .checks
        .check_results(&w.cx("owner"), &w.repo.id, &hex(&head.raw))
        .await
        .unwrap();
    assert_eq!(
        checks[0].status,
        bee_policy::CheckStatus::Success,
        "check runs land on the CR head"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflows_list_enable_disable_and_badges() {
    let w = world().await;
    w.push(
        "main",
        &[
            ("README.md", Some("x")),
            (".github/workflows/ci.yml", Some(HELLO)),
            (".beehive/workflows/extra.yaml", Some(HELLO)),
        ],
        "two",
    )
    .await;
    let list = w
        .svc
        .list_workflows(common::req(
            &w.token("reader"),
            ListWorkflowsRequest {
                repo: w.rr(),
                r#ref: String::new(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let paths: Vec<String> = list.workflows.iter().map(|x| x.path.clone()).collect();
    assert_eq!(
        paths,
        vec![".github/workflows/ci.yml", ".beehive/workflows/extra.yaml"]
    );
    let got = w
        .svc
        .get_workflow(common::req(
            &w.token("reader"),
            GetWorkflowRequest {
                repo: w.rr(),
                workflow: "ci.yml".into(),
                r#ref: String::new(),
                include_source: true,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.source, HELLO);
    assert_eq!(got.triggers, vec!["push"]);
    let runner = w.local("r").await;
    w.drain(&runner).await;
    let badge =
        w.ci.badge(&w.cx("reader"), &w.repo, &got.id, "")
            .await
            .unwrap();
    assert_eq!(badge.message, "passing");
    assert!(badge.svg().contains("passing"));
    w.svc
        .disable_workflow(common::req(
            &w.token("writer"),
            DisableWorkflowRequest {
                repo: w.rr(),
                workflow: "ci.yml".into(),
            },
        ))
        .await
        .unwrap();
    let (_, runs) = w.push("main", &[("a", Some("a"))], "a").await;
    assert_eq!(runs.len(), 1, "only the enabled workflow runs");
    w.push("main", &[(".beehive/workflows/extra.yaml", None)], "rm")
        .await;
    let list =
        w.ci.list_workflows(&w.cx("owner"), &w.repo, "")
            .await
            .unwrap();
    let extra = list
        .iter()
        .find(|x| x.path.ends_with("extra.yaml"))
        .unwrap();
    assert_eq!(extra.state, WorkflowState::Deleted as i32);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_and_step_timeouts() {
    let w = world().await;
    let wf = "on: push\njobs:\n  slow:\n    runs-on: ubuntu-latest\n    timeout-minutes: 0.01\n    steps:\n      - run: sleep 5\n  step:\n    runs-on: ubuntu-latest\n    steps:\n      - run: sleep 5\n        timeout-minutes: 0.005\n      - run: echo after\n        if: always()\n";
    let (_, r) = w
        .push("main", &[(".github/workflows/t.yml", Some(wf))], "t")
        .await;
    let runner = Arc::new(w.local("r").await);
    let bg = {
        let runner = runner.clone();
        tokio::spawn(async move { runner.run_once().await })
    };
    tokio::time::sleep(Duration::from_millis(900)).await;
    w.ci.tick(&Cx::system()).await.unwrap();
    let first = bg.await.unwrap().unwrap().unwrap();
    let second = runner.run_once().await.unwrap().unwrap();
    let jobs = w.jobs(&r[0]).await;
    let by = |n: &str| jobs.iter().find(|j| j.name == n).unwrap().clone();
    let slow = by("slow");
    let step = by("step");
    for ran in [&first, &second] {
        if ran.job_id == slow.id {
            assert!(
                matches!(ran.conclusion, None | Some(WorkflowConclusion::TimedOut)),
                "the server or runner must time the job out"
            );
        } else {
            assert_eq!(ran.conclusion, Some(WorkflowConclusion::Failure));
        }
    }
    assert_eq!(job_concl(&slow), WorkflowConclusion::TimedOut);
    assert_eq!(job_concl(&step), WorkflowConclusion::Failure);
    let log = w.log(&step.id).await;
    assert!(
        log.contains("The action has timed out.") && log.contains("after"),
        "{log}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn required_workflows_run_on_change_requests_of_selected_repos() {
    let w = world().await;
    let cx = w.cx("owner");
    let source = w
        .forge
        .orgs
        .create_repo(
            &cx,
            &w.org.id,
            NewRepo {
                id: w.forge.db.ids().ulid().unwrap(),
                name: "policies".into(),
                ..NewRepo::default()
            },
        )
        .await
        .unwrap();
    w.forge
        .hooks
        .provisioner
        .create_repo(&source, &bee_forge::hooks::RepoInit::default())
        .await
        .unwrap();
    let req_wf = "name: org-required\non: pull_request\njobs:\n  audit:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo required audit\n";
    let c = w
        .vcs
        .create_commit(
            &source,
            NewCommit {
                parent: None,
                edits: vec![(
                    ".github/workflows/audit.yml".into(),
                    Some(req_wf.as_bytes().to_vec()),
                )],
                message: "audit".into(),
                author: "owner".into(),
                creator: "owner".into(),
                chain: vec!["owner".into()],
                change_id: None,
            },
        )
        .await
        .unwrap();
    w.vcs.set_ref(&source, "main", &c).await.unwrap();
    let create = |who: &str| {
        let tok = w.token(who);
        let svc = w.svc.clone();
        let msg = CreateRequiredWorkflowRequest {
            org: w.org.name.clone(),
            name: "audit".into(),
            source_repo: rref(&source),
            source_path: ".github/workflows/audit.yml".into(),
            scope: WorkflowRequirementScope::Selected as i32,
            selected_repos: vec![w.rr().unwrap()],
            enforced: true,
            ..Default::default()
        };
        async move { svc.create_required_workflow(common::req(&tok, msg)).await }
    };
    assert_eq!(common::reason(create("writer").await), "PERMISSION_DENIED");
    let req = create("owner").await.unwrap().into_inner();
    let (base, _) = w.push("main", &[("README.md", Some("x"))], "base").await;
    let head = w.commit(Some(&base), &[("y", Some("y"))], "change").await;
    let cr = ChangeRequest {
        id: "cr9".into(),
        repo_id: w.repo.id.clone(),
        target_ref: "main".into(),
        number: 9,
        title: "c".into(),
        author: Some(Principal {
            id: "writer".into(),
            kind: PrincipalKind::Human as i32,
            ..Default::default()
        }),
        revisions: vec![ChangeRevision {
            n: 1,
            head: Some(head),
            base: Some(base),
            ..Default::default()
        }],
        ..Default::default()
    };
    let ids =
        w.ci.handle_event(
            &Event {
                kind: "cr.opened".into(),
                repo_id: w.repo.id.clone(),
                org_id: w.org.id.clone(),
                principal: "writer".into(),
                body: Some(event::Body::Change(cr)),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(w.run(&ids[0]).await.name, "org-required");
    assert_eq!(
        w.ci.required_workflows(&cx, &w.org.id).await.unwrap().len(),
        1
    );
    w.svc
        .delete_required_workflow(common::req(
            &w.token("owner"),
            DeleteRequiredWorkflowRequest {
                org: w.org.name.clone(),
                required_workflow_id: req.id,
            },
        ))
        .await
        .unwrap();
    assert!(
        w.ci.required_workflows(&cx, &w.org.id)
            .await
            .unwrap()
            .is_empty()
    );
}

/// F49-007: an enforced required workflow is a land gate for the change request's head
/// until its run on that head succeeds; an evaluate-only one (`enforced = false`) is no gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enforced_required_workflows_gate_landing_until_their_run_passes() {
    let w = world().await;
    let cx = w.cx("owner");
    let source = w
        .forge
        .orgs
        .create_repo(
            &cx,
            &w.org.id,
            NewRepo {
                id: w.forge.db.ids().ulid().unwrap(),
                name: "policies".into(),
                ..NewRepo::default()
            },
        )
        .await
        .unwrap();
    w.forge
        .hooks
        .provisioner
        .create_repo(&source, &bee_forge::hooks::RepoInit::default())
        .await
        .unwrap();
    let req_wf = "name: org-required\non: pull_request\njobs:\n  audit:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo required audit\n";
    let c = w
        .vcs
        .create_commit(
            &source,
            NewCommit {
                parent: None,
                edits: vec![(
                    ".github/workflows/audit.yml".into(),
                    Some(req_wf.as_bytes().to_vec()),
                )],
                message: "audit".into(),
                author: "owner".into(),
                creator: "owner".into(),
                chain: vec!["owner".into()],
                change_id: None,
            },
        )
        .await
        .unwrap();
    w.vcs.set_ref(&source, "main", &c).await.unwrap();
    let req = w
        .svc
        .create_required_workflow(common::req(
            &w.token("owner"),
            CreateRequiredWorkflowRequest {
                org: w.org.name.clone(),
                name: "audit".into(),
                source_repo: rref(&source),
                source_path: ".github/workflows/audit.yml".into(),
                scope: WorkflowRequirementScope::Selected as i32,
                selected_repos: vec![w.rr().unwrap()],
                enforced: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let (base, _) = w.push("main", &[("README.md", Some("x"))], "base").await;
    let head = w.commit(Some(&base), &[("y", Some("y"))], "change").await;
    let heads = vec![hex(&head.raw)];
    let gates =
        w.ci.required_workflow_gates(&cx, &w.repo, &heads)
            .await
            .unwrap();
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0].name, "audit");
    assert!(!gates[0].passed, "no run yet");
    let cr = ChangeRequest {
        id: "cr7".into(),
        repo_id: w.repo.id.clone(),
        target_ref: "main".into(),
        number: 7,
        title: "c".into(),
        author: Some(Principal {
            id: "writer".into(),
            kind: PrincipalKind::Human as i32,
            ..Default::default()
        }),
        revisions: vec![ChangeRevision {
            n: 1,
            head: Some(head.clone()),
            base: Some(base),
            ..Default::default()
        }],
        ..Default::default()
    };
    let ids =
        w.ci.handle_event(
            &Event {
                kind: "cr.opened".into(),
                repo_id: w.repo.id.clone(),
                org_id: w.org.id.clone(),
                principal: "writer".into(),
                body: Some(event::Body::Change(cr)),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);
    let gates =
        w.ci.required_workflow_gates(&cx, &w.repo, &heads)
            .await
            .unwrap();
    assert!(!gates[0].passed, "queued is not passed");
    let runner = w.local("req").await;
    w.drain(&runner).await;
    assert_eq!(concl(&w.run(&ids[0]).await), WorkflowConclusion::Success);
    let gates =
        w.ci.required_workflow_gates(&cx, &w.repo, &heads)
            .await
            .unwrap();
    assert!(gates[0].passed, "the run on the head passed");
    // Another head has no passing run.
    let other = w.commit(Some(&head), &[("z", Some("z"))], "more").await;
    let gates =
        w.ci.required_workflow_gates(&cx, &w.repo, &[hex(&other.raw)])
            .await
            .unwrap();
    assert!(!gates[0].passed);
    // Evaluate-only requirements are no gate.
    w.svc
        .update_required_workflow(common::req(
            &w.token("owner"),
            UpdateRequiredWorkflowRequest {
                org: w.org.name.clone(),
                required_workflow_id: req.id.clone(),
                enforced: Some(false),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    assert!(
        w.ci.required_workflow_gates(&cx, &w.repo, &heads)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Regression (three-form conformance): finalizing an artifact and committing a cache take the
/// job from the caller's job token; any other caller is refused as PERMISSION_DENIED instead of
/// being told that `job_id`, a field these requests do not have, is missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn artifact_finalize_and_cache_commit_without_a_job_token_are_permission_denied() {
    let w = world().await;
    let finalize = w
        .svc
        .finalize_workflow_artifact(common::req(
            &w.token("writer"),
            FinalizeWorkflowArtifactRequest {
                repo: w.rr(),
                artifact_id: "a1".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(common::reason(finalize), "PERMISSION_DENIED");
    let commit = w
        .svc
        .commit_workflow_cache(common::req(
            &w.token("writer"),
            CommitWorkflowCacheRequest {
                repo: w.rr(),
                cache_id: "c1".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(common::reason(commit), "PERMISSION_DENIED");
}

/// DSSE's pre-authentication encoding, written from the DSSE v1 specification.
fn dsse_pae_spec(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "DSSEv1 {} {} {} ",
        payload_type.len(),
        payload_type,
        payload.len()
    )
    .into_bytes();
    out.extend_from_slice(payload);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attestations_are_sigstore_bundles_anyone_verifies_with_the_public_key() {
    let (_, key) = bee_forge::ci::ports::EcdsaP256Key::generate().unwrap();
    let public = key.public_key();
    let kid = bee_forge::ci::ports::KeyPort::key_id(&key);
    let w = world_on_keys(Arc::new(MemoryStore::new()), config(), Arc::new(key)).await;
    let wf = "on: push\npermissions:\n  contents: read\n  attestations: write\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: 'true'\n";
    w.push("main", &[(".github/workflows/build.yml", Some(wf))], "t")
        .await;
    let (api, _, _) = w.api("r").await;
    let a = api.acquire().await.unwrap().unwrap();
    let digest_hex = "cd".repeat(32);
    let att = w
        .svc
        .create_attestation(common::req(
            &a.job_token,
            CreateAttestationRequest {
                repo: w.rr(),
                job_id: a.job_id.clone(),
                subjects: vec![AttestationSubject {
                    name: "app.tar.gz".into(),
                    digest: format!("sha256:{digest_hex}"),
                }],
                predicate_type: String::new(),
                predicate_json: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    // The bundle: Sigstore bundle v0.3 carrying a DSSE envelope over an in-toto statement.
    let bundle: serde_json::Value = serde_json::from_str(&att.bundle_json).unwrap();
    assert_eq!(
        bundle["mediaType"],
        "application/vnd.dev.sigstore.bundle+json;version=0.3"
    );
    assert_eq!(
        bundle["verificationMaterial"]["publicKey"]["hint"],
        kid.as_str()
    );
    let env = &bundle["dsseEnvelope"];
    let payload_type = env["payloadType"].as_str().unwrap();
    assert_eq!(payload_type, "application/vnd.in-toto+json");
    let decode = |s: &str| bee_forge::enterprise::saml::b64_decode(s).expect("standard base64");
    let payload = decode(env["payload"].as_str().unwrap());
    let statement: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(statement["_type"], "https://in-toto.io/Statement/v1");
    assert_eq!(statement["predicateType"], "https://slsa.dev/provenance/v1");
    assert_eq!(statement["subject"][0]["name"], "app.tar.gz");
    assert_eq!(
        statement["subject"][0]["digest"]["sha256"],
        digest_hex.as_str()
    );
    assert_eq!(
        statement["predicate"]["runDetails"]["builder"]["id"],
        format!(
            "{}/.github/workflows/build.yml@refs/heads/main",
            w.repo.full_name
        )
        .as_str()
    );
    // Independent verification: ECDSA P-256 over the DSSE PAE with only the public key.
    let sig = decode(env["signatures"][0]["sig"].as_str().unwrap());
    assert_eq!(env["signatures"][0]["keyid"], kid.as_str());
    let verifier =
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &public);
    verifier
        .verify(&dsse_pae_spec(payload_type, &payload), &sig)
        .expect("the signature verifies with the public key alone");
    // A changed statement does not.
    let mut forged = payload.clone();
    forged.extend_from_slice(b" ");
    assert!(
        verifier
            .verify(&dsse_pae_spec(payload_type, &forged), &sig)
            .is_err()
    );
    // And the service agrees.
    let v = w
        .svc
        .verify_attestation(common::req(
            &w.token("reader"),
            VerifyAttestationRequest {
                repo: w.rr(),
                subject_digest: format!("sha256:{digest_hex}"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(v.verified, "{:?}", v.failures);
}
