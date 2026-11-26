use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::agents::world_on;
use async_trait::async_trait;
use bee_auth::{KeyRing, SigningKey};
use bee_forge::Cx;
use bee_forge::Result;
use bee_forge::bisect::Outcome;
use bee_forge::bisect::prober::BisectProber;
use bee_forge::bisect::{BisectConfig, BisectRecord, Bisector, ProbeTarget};
use bee_forge::changes::{ChangeHooks, Changes, HistoryVcs, MemoryTeams};
use bee_forge::ci::{BISECTS, Ci, CiConfig, CiHooks, HistoryCiVcs, HmacKey, MemoryCiBlobs};
use bee_forge::ids::SystemClock;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::store::{ForgeStore, Row};
use bee_proto::v1::{Bisect, BisectState, ObjectId, Repo};

#[derive(Default)]
struct CountingProber(Mutex<HashMap<Vec<u8>, usize>>);

#[async_trait]
impl BisectProber for CountingProber {
    async fn start(
        &self,
        _cx: &Cx,
        _repo: &Repo,
        _target: &ProbeTarget,
        changeset: &ObjectId,
    ) -> Result<String> {
        *self
            .0
            .lock()
            .unwrap()
            .entry(changeset.raw.clone())
            .or_default() += 1;
        tokio::task::yield_now().await;
        Ok(format!("probe-{}", changeset.raw.len()))
    }

    async fn poll(
        &self,
        _cx: &Cx,
        _repo: &Repo,
        _target: &ProbeTarget,
        _probe_id: &str,
    ) -> Result<Option<Outcome>> {
        Ok(None)
    }

    async fn cancel(&self, _cx: &Cx, _repo: &Repo, _probe_id: &str) -> Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_bisect_probe_slots_are_claimed_once_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_bisect_claim_{}", std::process::id());
    }
    let store: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let world = world_on(store).await;
    let forge = world.forge.clone();
    let vcs: Arc<HistoryVcs> = world.vcs.clone();
    let ci = Ci::new(
        &forge,
        CiHooks {
            vcs: Arc::new(HistoryCiVcs::new(vcs.clone())),
            blobs: Arc::new(MemoryCiBlobs::new()),
            keys: Arc::new(HmacKey::new("k1", b"bisect-race")),
            secret_key: [3; 32],
            url_key: b"bisect-race-url".to_vec(),
            signer: Arc::new(KeyRing::new(
                SigningKey::from_secret_bytes(1, &[9; 32]).unwrap(),
            )),
            config: CiConfig::default(),
        },
    );
    let changes = Changes::new(
        &forge,
        ChangeHooks {
            vcs: vcs.clone(),
            lander: vcs,
            teams: Arc::new(MemoryTeams::new()),
        },
    );
    let left = Bisector::new(&forge, ci.clone(), changes.clone(), BisectConfig::default());
    let right = Bisector::new(&forge, ci, changes, BisectConfig::default());
    let prober = Arc::new(CountingProber::default());
    left.set_prober(prober.clone());
    right.set_prober(prober.clone());
    let repo = world.repo.clone();
    let cx = Cx::system();

    for round in 0..100 {
        prober.0.lock().unwrap().clear();
        let id = format!("bisect-race-{round}");
        let candidates: Vec<String> = (1..=8).map(|n| format!("{n:040x}")).collect();
        let bisect = Bisect {
            id: id.clone(),
            state: BisectState::Running as i32,
            r#ref: "main".into(),
            check_name: "ci/race".into(),
            ..Default::default()
        };
        let record = BisectRecord {
            bisect: Some(bisect),
            candidates: candidates.clone(),
            mode: "auto".into(),
            parallelism: 2,
            ..Default::default()
        };
        let mut tx = forge.db.begin(bee_forge::ci::SURFACE, &cx).await.unwrap();
        tx.insert(
            &BISECTS,
            &Row::new(&repo.id, &id)
                .k1("running")
                .k2("main")
                .msg(&record),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let a = left.clone();
        let b = right.clone();
        let r1 = repo.clone();
        let r2 = repo.clone();
        let c1 = cx.clone();
        let c2 = cx.clone();
        let id_a = id.clone();
        let id_b = id.clone();
        let barrier_a = barrier.clone();
        let task_a = tokio::spawn(async move {
            barrier_a.wait().await;
            a.step(&c1, &r1, &id_a).await
        });
        let task_b = tokio::spawn(async move {
            barrier.wait().await;
            b.step(&c2, &r2, &id_b).await
        });
        let result_a = task_a.await.unwrap();
        let result_b = task_b.await.unwrap();
        for (slot, count) in prober.0.lock().unwrap().iter() {
            assert!(
                *count <= 1,
                "round {round}: slot {slot:?} started {count} times"
            );
        }
        assert!(
            result_a.is_ok(),
            "round {round}: first step failed: {result_a:?}"
        );
        assert!(
            result_b.is_ok(),
            "round {round}: second step failed: {result_b:?}"
        );
    }
}
