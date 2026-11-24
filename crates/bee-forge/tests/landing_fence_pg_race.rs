//! PostgreSQL races that fence a change-request landing against review, revision and
//! auto-merge changes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use bee_forge::Cx;
use bee_forge::changes::vcs::{HistoryVcs, Lander, NewCommit};
use bee_forge::changes::{ChangeHooks, ChangeVcs, Changes, MemoryTeams, NewChange};
use bee_forge::error::{ForgeError, Result};
use bee_forge::forge::{Forge, Hooks};
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner, PolicySource};
use bee_forge::ids::{Clock, ManualClock, SystemClock};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::store::ForgeStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::{
    ChangeRequest, ChangeState, LandMethod, LandResponse, ObjectId, Principal, PrincipalKind, Repo,
    RepoRole, Role, Verdict,
};
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};

const POLICY: &str = r#"version = 1

[members]
write = ["@everyone"]

[[access]]
path = "/"
read = ["@everyone"]
write = ["@everyone"]

[[land]]
ref = "main"
dismiss_stale_approvals = true

[[land.approvals]]
classes = ["logic"]
count = 1
include_agents = true
"#;

static NEXT_WORLD: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct TestPolicy;

#[async_trait]
impl PolicySource for TestPolicy {
    async fn policy(&self, _repo: &Repo) -> Result<Option<bee_policy::Policy>> {
        bee_policy::Policy::parse(POLICY)
            .map(Some)
            .map_err(|e| ForgeError::invalid("invalid_policy", e.to_string()))
    }
}

struct ObservedLander {
    vcs: Arc<HistoryVcs>,
    disable_returned: Arc<AtomicBool>,
    lands_after_disable: Arc<AtomicUsize>,
    age_next_land: Arc<AtomicBool>,
    clock: Arc<ManualClock>,
}

#[async_trait]
impl Lander for ObservedLander {
    async fn land(
        &self,
        repo: &Repo,
        intent: &bee_forge::changes::LandIntent,
    ) -> Result<LandResponse> {
        if self.disable_returned.load(Ordering::SeqCst) {
            self.lands_after_disable.fetch_add(1, Ordering::SeqCst);
        }
        let response = Lander::land(self.vcs.as_ref(), repo, intent).await?;
        if self.age_next_land.swap(false, Ordering::SeqCst) {
            self.clock.advance(120_001);
        }
        Ok(response)
    }
}

struct World {
    changes_a: Arc<Changes>,
    changes_b: Arc<Changes>,
    vcs: Arc<HistoryVcs>,
    repo: Repo,
    disable_returned: Arc<AtomicBool>,
    lands_after_disable: Arc<AtomicUsize>,
    age_next_land: Arc<AtomicBool>,
}

impl World {
    async fn forge(
        store: Arc<dyn ForgeStore>,
        clock: Arc<dyn Clock>,
        directory: Arc<MemoryDirectory>,
        policy: Arc<TestPolicy>,
        provisioner: Arc<MetaProvisioner>,
        ring: &bee_auth::KeyRing,
    ) -> Arc<Forge> {
        let (forge, _) = Forge::open(
            store,
            clock,
            ring.verifying_keys(),
            Arc::new(bee_auth::InMemoryRevocations::new()),
            Hooks {
                directory,
                policy: Some(policy),
                provisioner,
                ..Hooks::default()
            },
            crate::common::test_config(),
        )
        .await
        .unwrap();
        forge
    }

    async fn open(url: &str) -> Self {
        let mut cfg = SqlConfig::parse(url).unwrap();
        if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
            *schema = format!(
                "forge_landing_fence_{}_{}",
                std::process::id(),
                NEXT_WORLD.fetch_add(1, Ordering::SeqCst)
            );
        }
        let clock = Arc::new(ManualClock::new(SystemClock.now_ms()));
        let store_clock: Arc<dyn Clock> = clock.clone();
        let store_a: Arc<dyn ForgeStore> = Arc::new(
            SqlStore::open(cfg.clone(), store_clock.clone())
                .await
                .unwrap(),
        );
        let store_b: Arc<dyn ForgeStore> =
            Arc::new(SqlStore::open(cfg, store_clock.clone()).await.unwrap());
        let ring =
            bee_auth::KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
        let directory = Arc::new(MemoryDirectory::new());
        for id in ["owner", "alice", "reviewer", "ci"] {
            directory.register(
                Principal {
                    id: id.into(),
                    kind: PrincipalKind::Human as i32,
                    display: id.into(),
                    ..Default::default()
                },
                vec![format!("{id}-login")],
                vec![format!("{id}@example.com")],
            );
        }
        let meta = Arc::new(MemoryMetaStore::new());
        let provisioner = Arc::new(MetaProvisioner::new(meta.clone(), store_clock.clone()));
        let policy = Arc::new(TestPolicy);
        let forge_a = Self::forge(
            store_a,
            store_clock.clone(),
            directory.clone(),
            policy.clone(),
            provisioner.clone(),
            &ring,
        )
        .await;
        let forge_b = Self::forge(
            store_b,
            store_clock.clone(),
            directory.clone(),
            policy,
            provisioner,
            &ring,
        )
        .await;
        let writer = Writer::new(
            meta.clone(),
            Arc::new(MemoryBlobStore::new(store_clock.clone())),
        );
        let vcs = Arc::new(HistoryVcs::new(writer, store_clock));
        let disable_returned = Arc::new(AtomicBool::new(false));
        let lands_after_disable = Arc::new(AtomicUsize::new(0));
        let age_next_land = Arc::new(AtomicBool::new(false));
        let lander = Arc::new(ObservedLander {
            vcs: vcs.clone(),
            disable_returned: disable_returned.clone(),
            lands_after_disable: lands_after_disable.clone(),
            age_next_land: age_next_land.clone(),
            clock: clock.clone(),
        });
        let hooks = || ChangeHooks {
            vcs: vcs.clone(),
            lander: lander.clone(),
            teams: Arc::new(MemoryTeams::new()),
        };
        let changes_a = Changes::new(&forge_a, hooks());
        let changes_b = Changes::new(&forge_b, hooks());

        let owner = Cx::new("owner", PrincipalKind::Human);
        let org = forge_a
            .orgs
            .create_org(
                &owner,
                &format!("landing-fence-{}", std::process::id()),
                "",
                principal("owner"),
            )
            .await
            .unwrap();
        let repo = forge_a
            .orgs
            .create_repo(
                &owner,
                &org.id,
                NewRepo {
                    id: forge_a.db.ids().ulid().unwrap(),
                    name: "fence".into(),
                    ..NewRepo::default()
                },
            )
            .await
            .unwrap();
        forge_a.hooks.provisioner.create_org(&org).await.unwrap();
        forge_a
            .hooks
            .provisioner
            .create_repo(&repo, &bee_forge::hooks::RepoInit::default())
            .await
            .unwrap();
        for id in ["alice", "reviewer"] {
            forge_a
                .orgs
                .set_member(&owner, &org.id, principal(id), Role::Member)
                .await
                .unwrap();
            forge_a
                .orgs
                .add_collaborator(&owner, &repo.id, principal(id), RepoRole::Write, "")
                .await
                .unwrap();
        }
        let root = vcs
            .create_commit(
                &repo,
                NewCommit {
                    edits: vec![("src/lib.rs".into(), Some(b"pub fn base() {}\n".to_vec()))],
                    message: "root".into(),
                    author: "owner".into(),
                    creator: "owner".into(),
                    chain: vec!["owner".into()],
                    ..NewCommit::default()
                },
            )
            .await
            .unwrap();
        vcs.set_ref(&repo, "main", &root).await.unwrap();

        Self {
            changes_a,
            changes_b,
            vcs,
            repo,
            disable_returned,
            lands_after_disable,
            age_next_land,
        }
    }

    async fn head(&self) -> ObjectId {
        self.vcs
            .ref_tip(&self.repo, "main")
            .await
            .unwrap()
            .unwrap()
            .target
            .unwrap()
    }

    async fn commit(
        &self,
        parent: &ObjectId,
        path: &str,
        content: &str,
        message: &str,
    ) -> ObjectId {
        self.vcs
            .create_commit(
                &self.repo,
                NewCommit {
                    parent: Some(parent.clone()),
                    edits: vec![(path.into(), Some(content.as_bytes().to_vec()))],
                    message: message.into(),
                    author: "alice".into(),
                    creator: "alice".into(),
                    chain: vec!["alice".into()],
                    ..NewCommit::default()
                },
            )
            .await
            .unwrap()
    }

    async fn change(&self, round: usize) -> (ChangeRequest, ObjectId, String) {
        let path = format!("src/fence_{round}.rs");
        let base = self.head().await;
        let head = self
            .commit(
                &base,
                &path,
                &format!("pub fn value_{round}() {{}}\n"),
                "change",
            )
            .await;
        let cr = self
            .changes_a
            .open(
                &Cx::new("alice", PrincipalKind::Human),
                &self.repo,
                NewChange {
                    head: Some(head.clone()),
                    title: format!("Change {round}"),
                    ..NewChange::default()
                },
            )
            .await
            .unwrap();
        (cr, head, path)
    }

    async fn approve(&self, cr: &ChangeRequest) -> String {
        self.changes_a
            .submit_review(
                &Cx::new("reviewer", PrincipalKind::Human),
                &self.repo,
                &cr.id,
                0,
                Verdict::Approve,
                "",
                vec![],
            )
            .await
            .unwrap()
            .id
    }

    async fn get(&self, id: &str) -> ChangeRequest {
        self.changes_a
            .get(&Cx::system(), &self.repo, id)
            .await
            .unwrap()
    }
}

fn principal(id: &str) -> Principal {
    Principal {
        id: id.into(),
        kind: PrincipalKind::Human as i32,
        display: id.into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_landing_fence_races_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let w = World::open(&url).await;

    for round in 0..100 {
        let (cr, _, _) = w.change(round).await;
        let review_id = w.approve(&cr).await;
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let land_barrier = barrier.clone();
        let dismiss_barrier = barrier.clone();
        let land_changes = w.changes_a.clone();
        let dismiss_changes = w.changes_b.clone();
        let repo_land = w.repo.clone();
        let repo_dismiss = w.repo.clone();
        let id_land = cr.id.clone();
        let id_dismiss = cr.id.clone();
        let review = review_id.clone();
        let (land, dismiss) = tokio::join!(
            async move {
                land_barrier.wait().await;
                land_changes
                    .land(
                        &Cx::new("alice", PrincipalKind::Human),
                        &repo_land,
                        &id_land,
                        LandMethod::Squash,
                        "",
                        1,
                    )
                    .await
            },
            async move {
                dismiss_barrier.wait().await;
                dismiss_changes
                    .dismiss_review(
                        &Cx::new("owner", PrincipalKind::Human),
                        &repo_dismiss,
                        &id_dismiss,
                        &review,
                        "race test",
                    )
                    .await
            }
        );
        let after = w.get(&cr.id).await;
        assert!(
            land.is_ok() ^ dismiss.is_ok(),
            "exactly one of landing and dismissal wins in round {round}: land={land:?}, dismiss={dismiss:?}"
        );
        assert_eq!(after.state == ChangeState::Landed as i32, land.is_ok());
    }

    for round in 100..200 {
        let (cr, old_head, path) = w.change(round).await;
        w.approve(&cr).await;
        let new_head = w
            .commit(
                &old_head,
                &path,
                &format!("pub fn value_{round}() {{ 2 }}\n"),
                "push",
            )
            .await;
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let land_barrier = barrier.clone();
        let revise_barrier = barrier.clone();
        let land_changes = w.changes_a.clone();
        let revise_changes = w.changes_b.clone();
        let repo_land = w.repo.clone();
        let repo_revise = w.repo.clone();
        let id_land = cr.id.clone();
        let id_revise = cr.id.clone();
        let head_revise = new_head.clone();
        let (land, revise) = tokio::join!(
            async move {
                land_barrier.wait().await;
                land_changes
                    .land(
                        &Cx::new("alice", PrincipalKind::Human),
                        &repo_land,
                        &id_land,
                        LandMethod::Squash,
                        "",
                        1,
                    )
                    .await
            },
            async move {
                revise_barrier.wait().await;
                revise_changes
                    .revise(
                        &Cx::new("alice", PrincipalKind::Human),
                        &repo_revise,
                        &id_revise,
                        &head_revise,
                    )
                    .await
            }
        );
        assert!(
            !(land.is_ok() && revise.is_ok()),
            "both landed and revised in round {round}"
        );
        let after = w.get(&cr.id).await;
        if revise.is_ok() {
            assert_eq!(after.state, ChangeState::Open as i32);
            assert_eq!(
                after.revisions.last().unwrap().head.as_ref(),
                Some(&new_head)
            );
        }
        if land.is_ok() {
            assert_eq!(after.state, ChangeState::Landed as i32);
            assert!(revise.is_err());
        }
    }

    for round in 200..300 {
        let (cr, _, _) = w.change(round).await;
        w.changes_a
            .enable_auto_merge(
                &Cx::new("alice", PrincipalKind::Human),
                &w.repo,
                &cr.id,
                LandMethod::Squash,
                "",
            )
            .await
            .unwrap();
        w.approve(&cr).await;
        w.disable_returned.store(false, Ordering::SeqCst);
        w.lands_after_disable.store(0, Ordering::SeqCst);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let land_barrier = barrier.clone();
        let disable_barrier = barrier.clone();
        let land_changes = w.changes_a.clone();
        let disable_changes = w.changes_b.clone();
        let repo_land = w.repo.clone();
        let repo_disable = w.repo.clone();
        let id_land = cr.id.clone();
        let id_disable = cr.id.clone();
        let disable_returned = w.disable_returned.clone();
        let (land, disable) = tokio::join!(
            async move {
                land_barrier.wait().await;
                land_changes.try_auto_merge(&repo_land, &id_land).await
            },
            async move {
                disable_barrier.wait().await;
                let result = disable_changes
                    .disable_auto_merge(
                        &Cx::new("alice", PrincipalKind::Human),
                        &repo_disable,
                        &id_disable,
                    )
                    .await;
                disable_returned.store(true, Ordering::SeqCst);
                result
            }
        );
        assert_eq!(w.lands_after_disable.load(Ordering::SeqCst), 0);
        if disable.is_ok()
            && land
                .as_ref()
                .map(|result| result.is_some())
                .unwrap_or(false)
        {
            // A land that began before disable returned is allowed; the instrumented lander
            // proves no auto-merge attempt begins after a successful disable.
            assert_eq!(w.get(&cr.id).await.state, ChangeState::Landed as i32);
        }
        let _ = (land, disable);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_landing_intent_records_after_expiry_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let w = World::open(&url).await;
    let (cr, _, _) = w.change(0).await;
    w.approve(&cr).await;
    w.disable_returned.store(false, Ordering::SeqCst);
    w.age_next_land.store(true, Ordering::SeqCst);
    w.changes_a
        .land(
            &Cx::new("alice", PrincipalKind::Human),
            &w.repo,
            &cr.id,
            LandMethod::Squash,
            "",
            1,
        )
        .await
        .unwrap();
    assert_eq!(w.get(&cr.id).await.state, ChangeState::Landed as i32);
}
