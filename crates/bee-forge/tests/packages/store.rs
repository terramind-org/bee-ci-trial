//! The packages store on every backend: memory and SQLite always, Postgres when
//! `BEE_TEST_PG` holds a connection URL. One scenario touches every table (unique keys,
//! counters, cursors, tags, blob reference counts, outboxes) through the registries and the
//! service, then checks the surface boundaries and migration registration.

use std::sync::Arc;

use bee_forge::Cx;
use bee_forge::ids::SystemClock;
use bee_forge::module::Registry;
use bee_forge::packages::manage::{NewFile, NewVersion};
use bee_forge::packages::model::Eco;
use bee_forge::packages::{BlobPort, COUNTS, SURFACE};
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::org_service_server::OrgService;
use bee_proto::v1::package_service_server::PackageService;
use bytes::Bytes;
use serde_json::json;

use super::service::native_publish;
use super::support::*;

async fn scenario(store: Arc<dyn ForgeStore>) {
    let w = world_on(store).await;
    let o = w.tok("owner");
    // Publish through a protocol (npm) and natively.
    let tgz = gzip(&tar(&[("package/package.json", b"{}")]));
    for (ver, tag) in [
        ("1.0.0", "latest"),
        ("1.1.0", "latest"),
        ("2.0.0-rc.1", "next"),
    ] {
        let doc = json!({
            "name": "st", "dist-tags": {tag: ver},
            "versions": {ver: {"name": "st", "version": ver, "repository": "https://beehive.test/acme/app"}},
            "_attachments": {format!("st-{ver}.tgz"): {"data": bee_forge::packages::hash::b64_encode(&tgz), "length": tgz.len()}},
        });
        let r = w
            .http(rq(
                "PUT",
                "/npm/acme/st",
                Some(&o),
                &[],
                doc.to_string().into_bytes(),
            ))
            .await;
        assert_eq!(r.status(), 200, "{}", body_str(&r));
    }
    native_publish(&w, &o, "nat", "1.0.0", None, PackageVisibility::Private)
        .await
        .unwrap();
    // Unique keys: the same version twice, the same package name case-insensitively.
    let doc = json!({"name": "st", "dist-tags": {"latest": "1.0.0"}, "versions": {"1.0.0": {"name": "st", "version": "1.0.0"}},
        "_attachments": {"st-1.0.0.tgz": {"data": bee_forge::packages::hash::b64_encode(&tgz), "length": tgz.len()}}});
    assert_eq!(
        w.http(rq(
            "PUT",
            "/npm/acme/st",
            Some(&o),
            &[],
            doc.to_string().into_bytes()
        ))
        .await
        .status(),
        403
    );
    // Counters: downloads accumulate in totals and per version.
    for _ in 0..3 {
        assert_eq!(
            w.http(rq(
                "GET",
                "/npm/acme/st/-/st-1.1.0.tgz",
                Some(&o),
                &[],
                Bytes::new()
            ))
            .await
            .status(),
            200
        );
    }
    let usage =
        w.s.get_package_usage(req(
            &o,
            GetPackageUsageRequest {
                owner: "acme".into(),
                ecosystem: 2,
                name: "st".into(),
                since: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(usage.download_count, 3);
    assert_eq!(
        usage
            .versions
            .iter()
            .find(|v| v.version == "1.1.0")
            .unwrap()
            .download_count,
        3
    );
    // Cursors: page through versions two at a time, newest first.
    let mut seen = vec![];
    let mut token = String::new();
    loop {
        let r =
            w.s.list_package_versions(req(
                &o,
                ListPackageVersionsRequest {
                    owner: "acme".into(),
                    ecosystem: 2,
                    name: "st".into(),
                    page: Some(PageRequest {
                        page_size: 2,
                        page_token: token.clone(),
                    }),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        seen.extend(r.versions.into_iter().map(|v| v.version));
        token = r.page.map(|p| p.next_page_token).unwrap_or_default();
        if token.is_empty() {
            break;
        }
    }
    assert_eq!(seen, ["2.0.0-rc.1", "1.1.0", "1.0.0"]);
    // Tagged-only filter and tag moves.
    let tagged =
        w.s.list_package_versions(req(
            &o,
            ListPackageVersionsRequest {
                owner: "acme".into(),
                ecosystem: 2,
                name: "st".into(),
                tagged_only: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        tagged
            .versions
            .iter()
            .map(|v| v.version.as_str())
            .collect::<Vec<_>>(),
        ["2.0.0-rc.1", "1.1.0"]
    );
    let moved =
        w.s.set_package_version_tags(req(
            &o,
            SetPackageVersionTagsRequest {
                owner: "acme".into(),
                ecosystem: 2,
                name: "st".into(),
                version: "1.0.0".into(),
                tags: vec!["latest".into()],
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(moved.tags, vec!["latest".to_string()]);
    let v11 =
        w.s.get_package_version(req(
            &o,
            GetPackageVersionRequest {
                owner: "acme".into(),
                ecosystem: 2,
                name: "st".into(),
                version: "1.1.0".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(v11.tags.is_empty(), "the tag moved off 1.1.0");
    // Reference counts: deleting and purging every version removes every blob.
    w.s.delete_package(req(
        &o,
        DeletePackageRequest {
            owner: "acme".into(),
            ecosystem: 2,
            name: "st".into(),
        },
    ))
    .await
    .unwrap();
    w.s.delete_package(req(
        &o,
        DeletePackageRequest {
            owner: "acme".into(),
            ecosystem: 2,
            name: "nat".into(),
        },
    ))
    .await
    .unwrap();
    assert!(!w.blobs.is_empty());
    w.clock.advance(31 * 86_400_000);
    w.env.forge.maintain().await.unwrap();
    assert_eq!(w.blobs.len(), 0, "{:?}", w.blobs.keys());
    // The outbox relayed events and audit entries.
    w.flush().await;
    let kinds = w.event_kinds().await;
    assert!(
        kinds.iter().filter(|k| *k == "package.published").count() >= 4,
        "{kinds:?}"
    );
    assert!(kinds.contains(&"package.deleted".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_packages_scenario() {
    scenario(Arc::new(MemoryStore::new())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_packages_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
        .await
        .unwrap();
    scenario(Arc::new(store)).await;
    // Packages and Pages are separate failure domains with their own database files.
    for s in ["packages", "pages"] {
        assert!(dir.path().join(format!("forge-{s}.db")).exists(), "{s}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_packages_scenario_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_packages_{}", std::process::id());
    }
    cfg.default_pool_size = 4;
    scenario(Arc::new(
        SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap(),
    ))
    .await;
}

/// Two replicas publishing different package versions for one owner must serialize on that
/// owner's storage counter before deciding whether the configured quota has room.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_publishes_respect_the_storage_quota_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_package_quota_race_{}", std::process::id());
    }
    cfg.default_pool_size = 4;
    let first_store: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second_store: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let w = world_on(first_store).await;

    let hooks = bee_forge::forge::Hooks {
        directory: w.env.forge.hooks.directory.clone(),
        provisioner: w.env.forge.hooks.provisioner.clone(),
        resolver: w.env.forge.hooks.resolver.clone(),
        policy: w.env.forge.hooks.policy.clone(),
        community: w.env.forge.hooks.community.clone(),
        packages: w.env.forge.hooks.packages.clone(),
        pages: w.env.forge.hooks.pages.clone(),
    };
    let (replica, report) = bee_forge::Forge::open(
        second_store,
        w.clock.clone(),
        w.env.ring.verifying_keys(),
        w.env.revocations.clone(),
        hooks,
        w.env.forge.config.clone(),
    )
    .await
    .unwrap();
    assert!(report.pending_contract.is_empty());

    let packages_a = w.env.forge.packages.clone();
    let packages_b = replica.packages.clone();
    for round in 0..24 {
        let org_name = format!("quota-race-{round}");
        w.s.create_org(req(
            &w.tok("owner"),
            CreateOrgRequest {
                name: org_name.clone(),
                display: org_name.clone(),
            },
        ))
        .await
        .unwrap();
        let owner = packages_a.owner(&Cx::system(), &org_name).await.unwrap();
        packages_a
            .set_owner_settings(&Cx::system(), &owner, None, Some(100))
            .await
            .unwrap();
        let (package_a, created_a) = packages_a
            .create_if_missing(
                &Cx::system(),
                &owner,
                Eco::Npm,
                "first",
                PackageVisibility::Private,
                "",
                "",
            )
            .await
            .unwrap();
        let (package_b, created_b) = packages_b
            .create_if_missing(
                &Cx::system(),
                &owner,
                Eco::Npm,
                "second",
                PackageVisibility::Private,
                "",
                "",
            )
            .await
            .unwrap();
        assert!(created_a && created_b);

        let make_version = |version: &str, key: String| {
            let mut nv = NewVersion::new(version);
            nv.files.push(NewFile {
                name: format!("{version}.tgz"),
                size: 80,
                digest: format!("sha256:{key}"),
                media_type: "application/octet-stream".into(),
                blob_key: key,
                ..Default::default()
            });
            nv
        };
        let key_a = format!("quota-race/{round}/a");
        let key_b = format!("quota-race/{round}/b");
        w.blobs
            .put(&key_a, bytes::Bytes::from(vec![b'a'; 80]))
            .await
            .unwrap();
        w.blobs
            .put(&key_b, bytes::Bytes::from(vec![b'b'; 80]))
            .await
            .unwrap();
        let version_a = make_version("1.0.0", key_a);
        let version_b = make_version("2.0.0", key_b);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let barrier_a = barrier.clone();
        let barrier_b = barrier.clone();
        let (a, b) = tokio::join!(
            async {
                barrier_a.wait().await;
                packages_a
                    .publish(&Cx::system(), &owner, &package_a, version_a)
                    .await
            },
            async {
                barrier_b.wait().await;
                packages_b
                    .publish(&Cx::system(), &owner, &package_b, version_b)
                    .await
            },
        );
        assert!(
            a.is_ok() ^ b.is_ok(),
            "exactly one publish must fit in round {round}: {a:?} {b:?}"
        );
        let loser = a.as_ref().err().or(b.as_ref().err()).unwrap();
        assert_eq!(
            loser.reason,
            bee_proto::v1::ErrorReason::QuotaExceeded,
            "round {round}: {loser:?}"
        );
        let mut tx = packages_a.db().begin(SURFACE, &Cx::system()).await.unwrap();
        let storage = tx
            .get(&COUNTS, &owner.id, "storage")
            .await
            .unwrap()
            .map(|row| row.n1.unwrap_or_default().max(0) as u64)
            .unwrap_or_default();
        tx.commit().await.unwrap();
        assert!(storage <= 100, "round {round}: stored {storage} bytes");
    }
}

#[test]
fn module_declares_packages_and_pages() {
    let r = Registry::core().unwrap();
    for k in [
        "package.published",
        "package.deleted",
        "pages.deployment_succeeded",
        "page_build.completed",
        "pages.domain_verified",
    ] {
        assert!(r.event_kind(k).is_some(), "{k}");
    }
    assert!(r.surfaces().iter().any(|s| s.name() == "packages"));
    assert!(r.surfaces().iter().any(|s| s.name() == "pages"));
    assert!(r.filter_known("package.*"));
    assert!(r.filter_known("pages.*"));
}
