use crate::common;
use bee_forge::Cx;
use bee_forge::events::webhooks::{
    CURSORS, HOOKS, HookOwner, JOBS, Job, MAX_HOOKS_PER_OWNER, NewHook,
};
use bee_forge::ids::SystemClock;
use bee_forge::store::sql::{SqlBackend, SqlConfig, SqlStore};
use bee_forge::store::{ForgeStore, Scan, Surface};
use bee_proto::v1::{Event, PrincipalKind, WebhookFormat};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_two_dispatchers_queue_each_event_once_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_webhook_dispatch_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let left = common::env_with(first, Arc::new(SystemClock)).await;
    let (right_forge, report) = bee_forge::Forge::open(
        second,
        Arc::new(SystemClock),
        left.ring.verifying_keys(),
        left.revocations.clone(),
        bee_forge::forge::Hooks {
            directory: left.directory.clone(),
            ..Default::default()
        },
        common::test_config(),
    )
    .await
    .unwrap();
    assert!(report.pending_contract.is_empty());

    let cx = Cx::new("owner", PrincipalKind::Human);
    let principal = left.forge.hooks.directory.get("owner").await.unwrap();
    let org = left
        .forge
        .orgs
        .create_org(&cx, "webhook-race", "", principal)
        .await
        .unwrap();
    let repo_id = left.forge.db.ids().ulid().unwrap();
    let repo = left
        .forge
        .orgs
        .create_repo(
            &cx,
            &org.id,
            bee_forge::orgs::repos::NewRepo {
                id: repo_id,
                name: "app".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let owner = HookOwner::repo(&repo.id, &org.id);
    let hook = left
        .forge
        .webhooks
        .create(
            &cx,
            &owner,
            NewHook {
                url: "http://127.0.0.1:1/hook".into(),
                kinds: vec!["lease.*".into()],
                secret: "secret".into(),
                format: WebhookFormat::Bee,
                active: true,
                insecure_ssl: false,
            },
        )
        .await
        .unwrap();

    // Start with no cursor row, so the first dispatch also exercises absent-row serialization.
    let mut tx = left
        .forge
        .db
        .begin(Surface::WEBHOOKS, &Cx::system())
        .await
        .unwrap();
    assert!(tx.delete(&CURSORS, "", &owner.log).await.unwrap());
    tx.commit().await.unwrap();

    for round in 0..24 {
        let appended = left
            .forge
            .events
            .append(
                &Cx::system(),
                Event {
                    kind: "lease.acquired".into(),
                    repo_id: repo.id.clone(),
                    org_id: org.id.clone(),
                    ..Default::default()
                },
                &format!("webhook-dispatch-{round}"),
            )
            .await
            .unwrap();
        let seq = appended
            .iter()
            .find(|(log, _)| log == &owner.log)
            .unwrap()
            .1;
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let a = left.forge.webhooks.clone();
        let b = right_forge.webhooks.clone();
        let (queued_a, queued_b) = tokio::join!(
            async {
                barrier.wait().await;
                a.dispatch_once(&Cx::system()).await.unwrap()
            },
            async {
                barrier.wait().await;
                b.dispatch_once(&Cx::system()).await.unwrap()
            },
        );
        assert_eq!(queued_a + queued_b, 1, "round {round} queued counts");

        let mut tx = left
            .forge
            .db
            .begin(Surface::WEBHOOKS, &Cx::system())
            .await
            .unwrap();
        let jobs = tx
            .scan_msgs::<Job>(
                &JOBS,
                &Scan {
                    scope: Some(String::new()),
                    limit: 10_000,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let matched = jobs
            .iter()
            .filter(|(job, _)| {
                job.webhook_id == hook.id
                    && job.event.as_ref().is_some_and(|event| event.seq == seq)
            })
            .count();
        let cursor = tx
            .get(&CURSORS, "", &owner.log)
            .await
            .unwrap()
            .unwrap()
            .n1
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            matched, 1,
            "round {round}: each event must have exactly one job"
        );
        assert!(
            cursor as u64 >= seq,
            "round {round}: cursor {cursor} must cover event {seq}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_webhook_creates_respect_the_owner_cap_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_webhook_cap_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let left = common::env_with(first, Arc::new(SystemClock)).await;
    let (right_forge, report) = bee_forge::Forge::open(
        second,
        Arc::new(SystemClock),
        left.ring.verifying_keys(),
        left.revocations.clone(),
        bee_forge::forge::Hooks {
            directory: left.directory.clone(),
            ..Default::default()
        },
        common::test_config(),
    )
    .await
    .unwrap();
    assert!(report.pending_contract.is_empty());

    let cx = Cx::new("owner", PrincipalKind::Human);
    let principal = left.forge.hooks.directory.get("owner").await.unwrap();
    let org = left
        .forge
        .orgs
        .create_org(&cx, "webhook-cap", "", principal)
        .await
        .unwrap();
    let repo_id = left.forge.db.ids().ulid().unwrap();
    let repo = left
        .forge
        .orgs
        .create_repo(
            &cx,
            &org.id,
            bee_forge::orgs::repos::NewRepo {
                id: repo_id,
                name: "app".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let owner = HookOwner::repo(&repo.id, &org.id);
    for i in 0..MAX_HOOKS_PER_OWNER - 1 {
        left.forge
            .webhooks
            .create(
                &cx,
                &owner,
                NewHook {
                    url: format!("http://127.0.0.1:1/hook/{i}"),
                    kinds: vec!["lease.*".into()],
                    secret: "secret".into(),
                    format: WebhookFormat::Bee,
                    active: true,
                    insecure_ssl: false,
                },
            )
            .await
            .unwrap();
    }

    for round in 0..100 {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left_webhooks = left.forge.webhooks.clone();
        let right_webhooks = right_forge.webhooks.clone();
        let left_owner = owner.clone();
        let right_owner = owner.clone();
        let (a, b) = tokio::join!(
            async {
                barrier.wait().await;
                left_webhooks
                    .create(
                        &cx,
                        &left_owner,
                        NewHook {
                            url: format!("http://127.0.0.1:1/hook/{round}-a"),
                            kinds: vec!["lease.*".into()],
                            secret: "secret".into(),
                            format: WebhookFormat::Bee,
                            active: true,
                            insecure_ssl: false,
                        },
                    )
                    .await
            },
            async {
                barrier.wait().await;
                right_webhooks
                    .create(
                        &cx,
                        &right_owner,
                        NewHook {
                            url: format!("http://127.0.0.1:1/hook/{round}-b"),
                            kinds: vec!["lease.*".into()],
                            secret: "secret".into(),
                            format: WebhookFormat::Bee,
                            active: true,
                            insecure_ssl: false,
                        },
                    )
                    .await
            },
        );
        assert!(
            a.is_ok() ^ b.is_ok(),
            "round {round}: exactly one create must succeed: {a:?} {b:?}"
        );
        let created = a.as_ref().ok().or(b.as_ref().ok()).unwrap();
        let mut tx = left
            .forge
            .db
            .begin(Surface::WEBHOOKS, &Cx::system())
            .await
            .unwrap();
        let count = tx.count(&HOOKS, &owner.log, None).await.unwrap();
        assert_eq!(count, MAX_HOOKS_PER_OWNER, "round {round}");
        tx.delete(&HOOKS, &owner.log, &created.id).await.unwrap();
        tx.commit().await.unwrap();
    }
}
