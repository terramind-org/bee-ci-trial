//! The ForgeStore conformance suite against every backend: memory and SQLite always,
//! Postgres when `BEE_TEST_PG` holds a connection URL.

use std::sync::Arc;

use bee_forge::ids::SystemClock;
use bee_forge::module::Registry;
use bee_forge::store::ForgeStore;
use bee_forge::store::conformance::run_all;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_store_conforms() {
    run_all(Arc::new(MemoryStore::new()), "mem").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_store_conforms() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
        .await
        .unwrap();
    run_all(Arc::new(store), "sqlite").await;
    // One file per surface, separate from any VCS database.
    assert!(dir.path().join("forge-conformance_a.db").exists());
    assert!(dir.path().join("forge-conformance_b.db").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_runs_every_core_migration_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let reg = Registry::core().unwrap();
    let open = || async {
        SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
            .await
            .unwrap()
    };
    let first = open().await.migrate(&reg.migrations()).await.unwrap();
    assert!(first.applied.iter().any(|m| m.starts_with("leases/0001")));
    let second = open().await.migrate(&reg.migrations()).await.unwrap();
    assert!(second.applied.is_empty());
    assert_eq!(second.already_applied.len(), first.applied.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_store_conforms_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let schema = format!("forge_test_{}", std::process::id());
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema: s, .. } = &mut cfg.backend {
        *s = schema;
    }
    cfg.default_pool_size = 3;
    let store: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    run_all(store.clone(), "pg").await;
    let reg = Registry::core().unwrap();
    store.migrate(&reg.migrations()).await.unwrap();
}

/// Transactions dropped without commit (every read-only one, and ones that failed) are rolled
/// back and their connections reused: the load run opened ~49,000 connections in 3.5 minutes
/// when each of them discarded its connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_pool_reuses_connections_of_uncommitted_transactions() {
    use bee_forge::store::Row;
    use bee_forge::store::conformance::{MIGRATION_1, ROWS, SURFACE_A};
    use std::time::Duration;

    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let schema = format!("forge_pool_{}", std::process::id());
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema: s, .. } = &mut cfg.backend {
        *s = schema;
    }
    cfg.default_pool_size = 3;
    let store = Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    store.migrate(&[&MIGRATION_1]).await.unwrap();
    let after_migrate = store.connections_opened();
    let soon = || tokio::time::Instant::now() + Duration::from_secs(10);

    // 8 concurrent tasks x 40 transactions: reads, abandoned writes and failed statements,
    // none committed.
    let mut tasks = Vec::new();
    for t in 0..8 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..40 {
                let mut tx = store.begin(SURFACE_A, soon()).await.unwrap();
                match i % 3 {
                    0 => {
                        tx.get(&ROWS, "pool", "missing").await.unwrap();
                    }
                    1 => {
                        tx.insert(&ROWS, &Row::new("pool", format!("w{t}-{i}")))
                            .await
                            .unwrap();
                    }
                    _ => {
                        // A failed statement aborts the transaction; the rollback clears it.
                        let row = Row::new("pool", format!("dup{t}-{i}"));
                        tx.insert(&ROWS, &row).await.unwrap();
                        assert!(tx.insert(&ROWS, &row).await.is_err());
                    }
                }
                drop(tx);
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    // Nothing leaked from the abandoned transactions, and the pool's connections still work.
    let mut tx = store.begin(SURFACE_A, soon()).await.unwrap();
    assert!(tx.get(&ROWS, "pool", "w0-1").await.unwrap().is_none());
    assert!(tx.get(&ROWS, "pool", "dup0-2").await.unwrap().is_none());
    tx.commit().await.unwrap();
    let opened = store.connections_opened() - after_migrate;
    assert!(
        opened <= 3,
        "320 uncommitted transactions on a 3-connection pool opened {opened} connections"
    );
}

use crate::common;
/// The domain modules end to end on a real backend: orgs, repos, leases, checks, the outbox
/// relay into the event log and the audit chain.
async fn forge_flow(store: Arc<dyn ForgeStore>) {
    use bee_forge::Cx;
    use bee_forge::checks::Report;
    use bee_forge::leases::AcquireRequest;
    use bee_forge::orgs::repos::NewRepo;
    use bee_proto::v1::{CheckStatus, LeaseMode, ObjectId, PrincipalKind};

    let env = common::env_with(store, Arc::new(SystemClock)).await;
    let cx = Cx::new("owner", PrincipalKind::Human);
    let p = env.forge.hooks.directory.get("owner").await.unwrap();
    let name = format!("org{}", env.forge.db.ids().ulid().unwrap().to_lowercase());
    let org = env.forge.orgs.create_org(&cx, &name, "", p).await.unwrap();
    let id = env.forge.db.ids().ulid().unwrap();
    let repo = env
        .forge
        .orgs
        .create_repo(
            &cx,
            &org.id,
            NewRepo {
                id,
                name: "app".into(),
                ..NewRepo::default()
            },
        )
        .await
        .unwrap();
    let (lease, _) = env
        .forge
        .leases
        .acquire(
            &cx,
            AcquireRequest {
                repo_id: repo.id.clone(),
                org_id: org.id.clone(),
                ref_name: "main".into(),
                paths: vec![b"src".to_vec()],
                mode: LeaseMode::Exclusive,
                ..AcquireRequest::default()
            },
        )
        .await
        .unwrap();
    env.forge
        .checks
        .report(
            &cx,
            &org.id,
            &repo.id,
            Report {
                changeset: Some(ObjectId { raw: vec![1; 33] }),
                name: "t".into(),
                status: CheckStatus::Passed,
                ..Report::default()
            },
        )
        .await
        .unwrap();
    env.forge.flush().await.unwrap();
    let (events, _) = env
        .forge
        .events
        .read(&cx, &repo.id, 0, &[], 100)
        .await
        .unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    // The creator follows the repo when `repo.created` is relayed (posts' auto-follow), so its
    // event lands after whichever relay pass ran the listener.
    assert!(kinds.contains(&"follow.repo_added"), "{kinds:?}");
    assert_eq!(
        kinds
            .into_iter()
            .filter(|k| *k != "follow.repo_added")
            .collect::<Vec<_>>(),
        vec!["repo.created", "lease.acquired", "check.updated"]
    );
    let scope = bee_forge::audit::org_scope(&org.id);
    let v = env
        .forge
        .audit
        .verify(&cx, &scope, 0, 0, &[])
        .await
        .unwrap();
    // The auto-follow of the repo's creator is audited too (`follow.repo_add`), once the relay
    // ran its listener; the chain holds every entry and verifies.
    let (entries, _) = env
        .forge
        .audit
        .query(&cx, &scope, &Default::default(), true, "", 100)
        .await
        .unwrap();
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(
        actions
            .iter()
            .copied()
            .filter(|a| *a != "follow.repo_add")
            .collect::<Vec<_>>(),
        vec!["org.create", "repo.create", "lease.acquire", "check.report"]
    );
    assert!(
        v.ok && v.verified_to_seq == entries.len() as u64,
        "{v:?} {actions:?}"
    );
    env.forge
        .leases
        .release(&cx, &org.id, &repo.id, &lease.id, false)
        .await
        .unwrap();
    assert!(
        env.forge
            .leases
            .list(&cx, &repo.id, &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forge_modules_run_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
        .await
        .unwrap();
    forge_flow(Arc::new(store)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forge_modules_run_on_postgres_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema: s, .. } = &mut cfg.backend {
        *s = format!("forge_flow_{}", std::process::id());
    }
    forge_flow(Arc::new(
        SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap(),
    ))
    .await;
}
