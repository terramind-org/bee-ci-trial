//! Failure isolation (SPEC §15.1, PRODUCT §4.22): the VCS data path works with the forge
//! database down, one forge surface's outage or backlog leaves the others working, and
//! outboxed events and audit entries arrive after recovery.

use crate::common;
use std::sync::Arc;
use std::time::Duration;

use bee_core::Ulid;
use bee_forge::Cx;
use bee_forge::events::bridge::VcsBridge;
use bee_forge::events::webhooks::{HookOwner, NewHook};
use bee_forge::ids::{IdGen, SystemClock};
use bee_forge::leases::{AcquireRequest, Leases};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::service::ForgeServices;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::store::unavailable::{ALL_SURFACES, SwitchableStore, UnavailableStore};
use bee_forge::store::{Db, ForgeStore, Surface};
use bee_proto::v1::repo_service_server::RepoService;
use bee_proto::v1::{
    ErrorReason, GetRepoRequest, LeaseMode, PrincipalKind, Repo, RepoRef, repo_ref,
};
use bee_store::{MemoryMetaStore, MetaStore, NewEvent, OrgId, OrgRow, ReadFrom, RepoId, RepoRow};

fn owner() -> Cx {
    Cx::new("owner", PrincipalKind::Human)
}

async fn org_and_repo(env: &common::Env) -> Repo {
    let p = env.forge.hooks.directory.get("owner").await.unwrap();
    let org = env
        .forge
        .orgs
        .create_org(&owner(), "acme", "", p)
        .await
        .unwrap();
    let id = env.forge.db.ids().ulid().unwrap();
    env.forge
        .orgs
        .create_repo(
            &owner(),
            &org.id,
            NewRepo {
                id,
                name: "app".into(),
                ..NewRepo::default()
            },
        )
        .await
        .unwrap()
}

fn lease(repo: &Repo, path: &str) -> AcquireRequest {
    AcquireRequest {
        repo_id: repo.id.clone(),
        org_id: repo.org_id.clone(),
        ref_name: "main".into(),
        paths: vec![path.as_bytes().to_vec()],
        mode: LeaseMode::Advisory,
        ..AcquireRequest::default()
    }
}

/// The VCS store has no handle on the forge store, so a VCS-only path cannot depend on it;
/// this test runs that path while the forge database is down and checks both sides.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vcs_path_works_with_the_forge_database_down() {
    let switch = Arc::new(SwitchableStore::new(Arc::new(MemoryStore::new())));
    let env = common::env_with(switch.clone(), Arc::new(SystemClock)).await;
    let repo = org_and_repo(&env).await;
    let services = ForgeServices::new(env.forge.clone());

    switch.take_down(Surface(ALL_SURFACES));

    // Forge calls fail with a retryable UNAVAILABLE...
    let st = services
        .get_repo(common::req(
            &env.full("owner"),
            GetRepoRequest {
                repo: Some(RepoRef {
                    repo: Some(repo_ref::Repo::Id(repo.id.clone())),
                }),
            },
        ))
        .await
        .unwrap_err();
    let d = bee_proto::errors::detail_from_status(&st).unwrap();
    assert_eq!((d.reason.as_str(), d.retryable), ("UNAVAILABLE", true));

    // ...while the VCS data path keeps working.
    let meta = Arc::new(MemoryMetaStore::new());
    let org_id = OrgId(Ulid::parse(&repo.org_id).unwrap());
    let repo_id = RepoId(Ulid::parse(&repo.id).unwrap());
    meta.create_org(OrgRow {
        id: org_id,
        name: "acme".into(),
        created_at_ms: 1,
    })
    .await
    .unwrap();
    meta.create_repo(RepoRow {
        id: repo_id,
        org: org_id,
        name: "app".into(),
        created_at_ms: 1,
        draft_retention_days: 30,
    })
    .await
    .unwrap();
    meta.append_events(
        repo_id,
        vec![NewEvent {
            kind: "ref.updated".into(),
            payload: Vec::new(),
            at_ms: 2,
        }],
    )
    .await
    .unwrap();
    assert_eq!(
        meta.read_events(repo_id, 1, 10, ReadFrom::Primary)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        meta.get_ref(repo_id, &bee_core::RefName::new("main").unwrap())
            .await
            .unwrap()
            .is_none()
    );

    // The land path's lease check fails open on a forge outage.
    let paths = vec!["src/a.rs".to_string()];
    env.forge
        .leases
        .land_check_fail_open(&Cx::system(), &repo.id, "main", &paths, "lander")
        .await
        .expect("land is not blocked by the forge");
    let detached = Leases::new(Db::new(
        Arc::new(UnavailableStore),
        Arc::new(IdGen::new(Arc::new(SystemClock))),
    ));
    detached
        .land_check_fail_open(&Cx::system(), &repo.id, "main", &paths, "lander")
        .await
        .expect("fail open with no forge database at all");

    // After recovery the VCS events are bridged into the forge log.
    switch.bring_up(Surface(ALL_SURFACES));
    let bridge = VcsBridge::new(meta, env.forge.events.clone(), env.forge.db.clone());
    assert_eq!(
        bridge
            .sync_repo(&Cx::system(), &repo.id, &repo.org_id)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_surface_down_leaves_the_others_working_and_loses_nothing() {
    let switch = Arc::new(SwitchableStore::new(Arc::new(MemoryStore::new())));
    let env = common::env_with(switch.clone(), Arc::new(SystemClock)).await;
    let repo = org_and_repo(&env).await;
    env.forge.flush().await.unwrap();

    // Event log down: leases and the orgs surface still work; events wait in the outbox.
    switch.take_down(Surface::EVENTS);
    env.forge
        .leases
        .acquire(&owner(), lease(&repo, "src"))
        .await
        .expect("leases work without the event log");
    env.forge
        .orgs
        .set_topics(&owner(), &repo.id, vec!["x".into()])
        .await
        .expect("orgs work without the event log");
    assert_eq!(
        env.forge
            .relay
            .drain_all(&Cx::system())
            .await
            .unwrap_err()
            .reason,
        ErrorReason::Unavailable
    );
    switch.bring_up(Surface::EVENTS);
    env.forge.flush().await.unwrap();
    let (events, _) = env
        .forge
        .events
        .read(
            &Cx::system(),
            &repo.id,
            0,
            &["lease.*".into(), "repo.topics_updated".into()],
            10,
        )
        .await
        .unwrap();
    assert_eq!(events.len(), 2, "outboxed events arrive after recovery");

    // Audit log down: changes still commit; entries are chained after recovery.
    switch.take_down(Surface::AUDIT);
    env.forge
        .leases
        .acquire(&owner(), lease(&repo, "docs"))
        .await
        .unwrap();
    switch.bring_up(Surface::AUDIT);
    env.forge.flush().await.unwrap();
    let scope = bee_forge::audit::org_scope(&repo.org_id);
    let v = env
        .forge
        .audit
        .verify(&Cx::system(), &scope, 0, 0, &[])
        .await
        .unwrap();
    assert!(v.ok);
    let (entries, _) = env
        .forge
        .audit
        .query(&Cx::system(), &scope, &Default::default(), true, "", 100)
        .await
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.action == "lease.acquire")
            .count(),
        2
    );

    // Webhooks down: leases keep working; deliveries resume later.
    let hook_owner = HookOwner::repo(&repo.id, &repo.org_id);
    env.forge
        .webhooks
        .create(
            &owner(),
            &hook_owner,
            NewHook {
                url: "http://127.0.0.1:9/never".into(),
                kinds: vec!["lease.*".into()],
                active: true,
                ..NewHook::default()
            },
        )
        .await
        .unwrap();
    switch.take_down(Surface::WEBHOOKS);
    env.forge
        .leases
        .acquire(&owner(), lease(&repo, "ci"))
        .await
        .expect("leases work without webhooks");
    assert!(
        env.forge
            .webhooks
            .dispatch_once(&Cx::system())
            .await
            .is_err()
    );
    switch.bring_up(Surface::WEBHOOKS);
    env.forge.relay.drain_all(&Cx::system()).await.unwrap();
    assert_eq!(
        env.forge
            .webhooks
            .dispatch_once(&Cx::system())
            .await
            .unwrap(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saturated_surface_pool_does_not_block_other_surfaces() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let env = common::env_with(store.clone(), Arc::new(SystemClock)).await;
    let repo = org_and_repo(&env).await;

    // A webhook backlog holds the webhooks surface.
    let held = store
        .begin(
            Surface::WEBHOOKS,
            tokio::time::Instant::now() + Duration::from_secs(30),
        )
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    env.forge
        .leases
        .acquire(
            &owner().with_timeout(Duration::from_secs(3)),
            lease(&repo, "src"),
        )
        .await
        .expect("leases unaffected by the webhook backlog");
    assert!(started.elapsed() < Duration::from_secs(3));
    let err = env
        .forge
        .webhooks
        .create(
            &owner().with_timeout(Duration::from_millis(300)),
            &HookOwner::repo(&repo.id, &repo.org_id),
            NewHook {
                url: "http://127.0.0.1:9/x".into(),
                active: true,
                ..NewHook::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::DeadlineExceeded);
    drop(held);
}
