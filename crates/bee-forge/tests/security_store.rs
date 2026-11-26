//! The security module on every store backend (memory, SQLite, Postgres when `BEE_TEST_PG`
//! is set): one flow that writes every kind of record, then a reopen proving it is durable.

use crate::common;
use crate::security_support;

use std::sync::Arc;

use bee_forge::Cx;
use bee_forge::ids::SystemClock;
use bee_forge::security::Security;
use bee_forge::security::ports::Ports;
use bee_forge::security::secrets::PatternOwner;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::*;
use security_support::*;

async fn flow(store: Arc<dyn ForgeStore>) -> Repo {
    let w = world_on(store).await;
    w.load_advisories().await;
    w.plant_project().await;
    w.evaluate().await;
    let open = w
        .sec
        .list_dependency_alerts(
            &w.cx("owner"),
            &w.repo,
            &ListDependencyAlertsRequest {
                repo: w.rref(),
                ..Default::default()
            },
            "",
            50,
        )
        .await
        .unwrap()
        .0;
    assert_eq!(open.len(), 4);
    w.push(
        &[("k.env", Some("aws_access_key_id = AKIAIOSFODNN7EXAMPLE\n"))],
        "leak",
    )
    .await;
    let tip = w.tip().await;
    assert_eq!(
        w.sec
            .scan_history(&Cx::system(), &w.repo, &tip, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    w.sec
        .create_pattern(
            &w.cx("owner"),
            PatternOwner::Repo(&w.repo),
            &CreateSecretScanningPatternRequest {
                name: "n".into(),
                regex: "zzz-[0-9]+".into(),
                published: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let doc = serde_json::json!({"version":"2.1.0","runs":[{"tool":{"driver":{"name":"T"}},"results":[
        {"ruleId":"r","message":{"text":"m"},"partialFingerprints":{"a":"1"},
         "locations":[{"physicalLocation":{"artifactLocation":{"uri":"k.env"},"region":{"startLine":1}}}]}]}]});
    let up = w
        .sec
        .upload_sarif(
            &w.cx("ci"),
            &w.repo,
            &UploadSarifRequest {
                repo: w.rref(),
                changeset: Some(tip),
                sarif: Some(upload_sarif_request::Sarif::SarifGzip(gzip(
                    doc.to_string().as_bytes(),
                ))),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(up.status, CodeScanningSarifStatus::Complete as i32);
    let a = w
        .sec
        .create_advisory(
            &w.cx("maintainer"),
            &w.repo,
            &CreateSecurityAdvisoryRequest {
                repo: w.rref(),
                summary: "s".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(a.ghsa_id.starts_with("GHSA-"));
    w.sec
        .create_campaign(
            &w.cx("secmgr"),
            &w.org.id,
            &w.org.name,
            &CreateSecurityCampaignRequest {
                org: w.org.name.clone(),
                name: "c".into(),
                filter: Some(SecurityAlertFilter {
                    kind: SecurityAlertKind::Dependency as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let kinds = w.kinds().await;
    for k in [
        "security_alert.created",
        "secret_scanning_alert.created",
        "code_scanning_alert.created",
        "security_advisory.created",
    ] {
        assert!(kinds.contains(&k.to_string()), "missing {k}");
    }
    assert!(
        w.audit_actions()
            .await
            .contains(&"security.campaign.create".to_string())
    );
    w.repo.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_conforms_on_memory() {
    flow(Arc::new(MemoryStore::new())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_conforms_on_sqlite_and_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let open = || async {
        Arc::new(
            SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
                .await
                .unwrap(),
        )
    };
    let repo = flow(open().await).await;
    assert!(dir.path().join("forge-security.db").exists());
    // A second forge on the same files (migrations run again) sees the stored alerts.
    let store = open().await;
    let ring =
        bee_auth::KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let (forge, report) = bee_forge::Forge::open(
        store,
        Arc::new(SystemClock),
        ring.verifying_keys(),
        Arc::new(bee_auth::InMemoryRevocations::new()),
        Default::default(),
        common::test_config(),
    )
    .await
    .unwrap();
    assert!(report.pending_contract.is_empty());
    let sec = Security::new(&forge, None, Ports::default());
    let cx = Cx::system();
    let deps = sec
        .list_dependency_alerts(&cx, &repo, &ListDependencyAlertsRequest::default(), "", 50)
        .await
        .unwrap()
        .0;
    assert_eq!(deps.len(), 4);
    let secrets = sec
        .list_secret_alerts(
            &cx,
            &repo,
            &ListSecretScanningAlertsRequest::default(),
            "",
            50,
        )
        .await
        .unwrap()
        .0;
    assert_eq!(secrets.len(), 1);
    assert_eq!(
        sec.list_analyses(&cx, &repo, "", "", None, "", 10)
            .await
            .unwrap()
            .0
            .len(),
        1
    );
}

/// Separate store handles model two forge replicas sharing one SQLite database. The queue key
/// must reject the loser after both replicas have observed no live run for the entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_replicas_queue_one_live_run_per_entry() {
    let dir = tempfile::tempdir().unwrap();
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let w = world_on(first).await;
    let ring =
        bee_auth::KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let (forge, report) = bee_forge::Forge::open(
        second,
        Arc::new(SystemClock),
        ring.verifying_keys(),
        Arc::new(bee_auth::InMemoryRevocations::new()),
        Default::default(),
        common::test_config(),
    )
    .await
    .unwrap();
    assert!(report.pending_contract.is_empty());
    let replica = Security::new(&forge, None, Ports::default());

    for i in 0..24 {
        let directory = format!("/race-{i}");
        let left = w.sec.clone();
        let right = replica.clone();
        let repo = w.repo.clone();
        let other_repo = repo.clone();
        let left_directory = directory.clone();
        let (a, b) = tokio::join!(
            async move {
                left.queue_run(&Cx::system(), &repo, "npm", &left_directory, true)
                    .await
            },
            async move {
                right
                    .queue_run(&Cx::system(), &other_repo, "npm", &directory, true)
                    .await
            },
        );
        assert!(
            a.is_ok() ^ b.is_ok(),
            "both replicas must not queue {i}: {a:?} {b:?}"
        );
        let loser = a.as_ref().err().or(b.as_ref().err()).unwrap();
        assert_eq!(loser.reason, ErrorReason::ResourceExhausted);
        assert_eq!(loser.subcode, "update_run_queued");
    }
    let (runs, _) = w
        .sec
        .list_runs(&Cx::system(), &w.repo, 0, "", 100)
        .await
        .unwrap();
    assert_eq!(runs.len(), 24);
    assert!(
        runs.iter()
            .all(|run| run.state == DependencyUpdateRunState::Queued as i32)
    );
}

/// Separate handles on one Postgres schema model two forge replicas. The durable queue key
/// must admit exactly one update run after both replicas see no live run for an entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_replicas_queue_one_live_run_per_entry_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_security_race_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let w = world_on(first).await;
    let ring =
        bee_auth::KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let (forge, report) = bee_forge::Forge::open(
        second,
        Arc::new(SystemClock),
        ring.verifying_keys(),
        Arc::new(bee_auth::InMemoryRevocations::new()),
        Default::default(),
        common::test_config(),
    )
    .await
    .unwrap();
    assert!(report.pending_contract.is_empty());
    let replica = Security::new(&forge, None, Ports::default());

    for i in 0..24 {
        let directory = format!("/postgres-race-{i}");
        let left = w.sec.clone();
        let right = replica.clone();
        let repo = w.repo.clone();
        let other_repo = repo.clone();
        let left_directory = directory.clone();
        let (a, b) = tokio::join!(
            async move {
                left.queue_run(&Cx::system(), &repo, "npm", &left_directory, true)
                    .await
            },
            async move {
                right
                    .queue_run(&Cx::system(), &other_repo, "npm", &directory, true)
                    .await
            },
        );
        assert!(
            a.is_ok() ^ b.is_ok(),
            "both replicas must not queue {i}: {a:?} {b:?}"
        );
        let loser = a.as_ref().err().or(b.as_ref().err()).unwrap();
        assert_eq!(loser.reason, ErrorReason::ResourceExhausted);
        assert_eq!(loser.subcode, "update_run_queued");
    }
    let (runs, _) = w
        .sec
        .list_runs(&Cx::system(), &w.repo, 0, "", 100)
        .await
        .unwrap();
    assert_eq!(runs.len(), 24);
    assert!(
        runs.iter()
            .all(|run| run.state == DependencyUpdateRunState::Queued as i32)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_conforms_on_postgres_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema: s, .. } = &mut cfg.backend {
        *s = format!("forge_security_{}", std::process::id());
    }
    flow(Arc::new(
        SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap(),
    ))
    .await;
}
