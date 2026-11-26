use std::sync::Arc;

use crate::agents::world_on;
use crate::common;
use bee_auth::{KeyRing, SigningKey};
use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::agents::fake::FakeRunner;
use bee_forge::agents::local::LocalProvider;
use bee_forge::agents::{
    AgentPorts, Agents, AgentsConfig, KeyRingIssuer, MemoryBlobs, StartRun, Trigger,
};
use bee_forge::changes::{ChangeHooks, HistoryVcs, MemoryTeams};
use bee_forge::forge::Hooks;
use bee_forge::ids::{Clock, SystemClock};
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::store::{ForgeStore, Scan};
use bee_proto::v1::{AgentRunScope, AgentTask, AgentTaskKind};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_one_idempotency_key_starts_one_run_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_idem_runs_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let w = world_on(first).await;

    let ring = KeyRing::new(SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let revocations: Arc<dyn bee_auth::RevocationList + Send + Sync> = w.revocations.clone();
    let clock: Arc<dyn Clock> = w.clock.clone();
    let (forge, report) = Forge::open(
        second,
        clock.clone(),
        ring.verifying_keys(),
        revocations,
        Hooks {
            directory: w.forge.hooks.directory.clone(),
            ..Hooks::default()
        },
        common::test_config(),
    )
    .await
    .unwrap();
    assert!(report.pending_contract.is_empty());
    let vcs: Arc<HistoryVcs> = w.vcs.clone();
    let changes = bee_forge::changes::Changes::new(
        &forge,
        ChangeHooks {
            vcs: vcs.clone(),
            lander: vcs.clone(),
            teams: Arc::new(MemoryTeams::new()),
        },
    );
    changes.attach(&forge);
    let runner: Arc<FakeRunner> = w.runner.clone();
    let agents = Agents::new(
        forge.clone(),
        changes,
        AgentPorts {
            runner,
            issuer: Arc::new(KeyRingIssuer::new(
                ring,
                Some(w.revocations.clone()),
                clock.clone(),
            )),
            blobs: Arc::new(MemoryBlobs::new()),
            repo: vcs.clone(),
            workspaces: Arc::new(LocalProvider::new(
                std::env::temp_dir().join(format!("idem-{}", std::process::id())),
                vcs,
            )),
        },
        AgentsConfig::default(),
    );
    agents.attach();

    for i in 0..100 {
        let key = format!("same-key-{i}");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left = w.agents.clone();
        let right = agents.clone();
        let left_barrier = barrier.clone();
        let right_barrier = barrier;
        let left_cx = Cx::new("writer", bee_proto::v1::PrincipalKind::Human);
        let right_cx = left_cx.clone();
        let retry_cx = left_cx.clone();
        let request = |idempotency_key: String, repo: bee_proto::v1::Repo| StartRun {
            repo,
            task: AgentTask {
                kind: AgentTaskKind::Freeform as i32,
                prompt: "race".into(),
                ..Default::default()
            },
            scope: AgentRunScope {
                actions: vec!["read".into(), "commit".into()],
                ..Default::default()
            },
            idempotency_key,
            trigger: Trigger::Manual,
            ..Default::default()
        };
        let left_key = key.clone();
        let right_key = key.clone();
        let left_req = request(left_key, w.repo.clone());
        let right_req = request(right_key, w.repo.clone());
        let a = tokio::spawn(async move {
            left_barrier.wait().await;
            left.start_run(&left_cx, left_req).await
        });
        let b = tokio::spawn(async move {
            right_barrier.wait().await;
            right.start_run(&right_cx, right_req).await
        });
        let a = a.await.unwrap().unwrap();
        let b = b.await.unwrap().unwrap();
        assert_eq!(
            a.id, b.id,
            "idempotency key {key} returned different run ids"
        );
        let retry = w
            .agents
            .start_run(&retry_cx, request(key.clone(), w.repo.clone()))
            .await
            .unwrap();
        assert_eq!(retry.id, a.id, "sequential retry changed run for {key}");
    }

    let mut tx = w
        .forge
        .db
        .begin(bee_forge::agents::SURFACE, &Cx::system())
        .await
        .unwrap();
    let runs = tx
        .scan(
            &bee_forge::agents::RUNS,
            &Scan::scope(w.repo.id.clone(), 10_000),
        )
        .await
        .unwrap();
    let queue = tx
        .scan(
            &bee_forge::agents::QUEUE,
            &Scan::scope(String::new(), 10_000),
        )
        .await
        .unwrap();
    assert_eq!(
        runs.len(),
        100,
        "exactly one persisted run per idempotency key"
    );
    assert_eq!(
        queue.len(),
        100,
        "exactly one queued entry per idempotency key"
    );
}
