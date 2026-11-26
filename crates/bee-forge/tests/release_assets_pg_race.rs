//! PostgreSQL races between release lifecycle changes and asset mutations.

use std::sync::Arc;

use crate::community_support as support;
use bee_auth::Scope;
use bee_forge::Cx;
use bee_forge::ids::SystemClock;
use bee_forge::service::ForgeServices;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::store::{ForgeStore, Key, Scan};
use bee_proto::v1::release_service_server::ReleaseService;
use bee_proto::v1::*;
use support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_release_asset_mutations_serialize_with_freeze_and_delete_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_release_assets_race_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let w = support::world_on(first).await;
    let clock = w.clock.clone();
    let parts = support::forge_on(second, clock).await;
    let other = ForgeServices::new(parts.env.forge.clone());
    let writer = w.env.agent_token("writer", Scope::new());
    let second_writer = parts.env.agent_token("writer", Scope::new());
    w.vcs
        .seed(&w.vr(&w.app), &[("a", "1\n")], &human("writer"), "seed", T0)
        .await
        .unwrap();

    for round in 0..100 {
        let request = CreateReleaseRequest {
            repo: rid(&w.app.id),
            tag_name: format!("freeze-{round}"),
            target: Some(Revision {
                expr: "main".into(),
            }),
            draft: true,
            immutable: true,
            ..Default::default()
        };
        let release =
            w.s.create_release(req(&writer, request.clone()))
                .await
                .unwrap()
                .into_inner();
        let begun =
            w.s.begin_release_asset_upload(req(
                &writer,
                BeginReleaseAssetUploadRequest {
                    repo: rid(&w.app.id),
                    release_id: release.id.clone(),
                    name: "artifact".into(),
                    size: 1,
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let b1 = barrier.clone();
        let b2 = barrier.clone();
        let svc1 = w.s.clone();
        let svc2 = other.clone();
        let repo = rid(&w.app.id);
        let repo2 = rid(&w.app.id);
        let token1 = writer.clone();
        let token2 = second_writer.clone();
        let release_id = release.id.clone();
        let asset_id = begun.asset_id.clone();
        let (finalized, frozen) = tokio::join!(
            async move {
                b1.wait().await;
                svc1.finalize_release_asset(req(
                    &token1,
                    FinalizeReleaseAssetRequest {
                        repo,
                        asset_id,
                        sha256: vec![7; 32],
                    },
                ))
                .await
            },
            async move {
                b2.wait().await;
                svc2.publish_release(req(
                    &token2,
                    PublishReleaseRequest {
                        repo: repo2,
                        release_id,
                        make_latest: 0,
                    },
                ))
                .await
            }
        );
        frozen.unwrap();
        if finalized.is_err() {
            assert_eq!(reason::<()>(Err(finalized.unwrap_err())), "INVALID_STATE");
        } else {
            let listed =
                w.s.list_release_assets(req(
                    &w.tok("reader"),
                    ListReleaseAssetsRequest {
                        repo: rid(&w.app.id),
                        release_id: release.id.clone(),
                        ..Default::default()
                    },
                ))
                .await
                .unwrap()
                .into_inner();
            assert!(
                listed.assets.iter().any(
                    |a| a.id == begun.asset_id && a.state == ReleaseAssetState::Uploaded as i32
                ),
                "freeze did not include finalized asset in round {round}"
            );
        }
    }

    for round in 0..100 {
        let release =
            w.s.create_release(req(
                &writer,
                CreateReleaseRequest {
                    repo: rid(&w.app.id),
                    tag_name: format!("delete-{round}"),
                    target: Some(Revision {
                        expr: "main".into(),
                    }),
                    draft: true,
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let b1 = barrier.clone();
        let b2 = barrier.clone();
        let svc1 = w.s.clone();
        let svc2 = other.clone();
        let release_id = release.id.clone();
        let deleted_release_id = release_id.clone();
        let begin_release_id = release_id.clone();
        let repo1 = rid(&w.app.id);
        let repo2 = rid(&w.app.id);
        let token1 = writer.clone();
        let token2 = second_writer.clone();
        let (deleted, begun) = tokio::join!(
            async move {
                b1.wait().await;
                svc1.delete_release(req(
                    &token1,
                    DeleteReleaseRequest {
                        repo: repo1,
                        release_id: deleted_release_id,
                        delete_tag: false,
                    },
                ))
                .await
            },
            async move {
                b2.wait().await;
                svc2.begin_release_asset_upload(req(
                    &token2,
                    BeginReleaseAssetUploadRequest {
                        repo: repo2,
                        release_id: begin_release_id,
                        name: "orphan".into(),
                        size: 1,
                        ..Default::default()
                    },
                ))
                .await
            }
        );
        // Whether upload or deletion won, a completed release deletion leaves no rows.
        deleted.unwrap();
        let _ = begun;
        let mut tx = w
            .env
            .forge
            .db
            .begin(bee_forge::community::SURFACE, &Cx::system())
            .await
            .unwrap();
        let assets = tx
            .scan(
                &bee_forge::community::releases::ASSETS,
                &Scan::scope(&w.app.id, 1_001).key(Key::K1(release.id.clone())),
            )
            .await
            .unwrap();
        assert!(
            assets.is_empty(),
            "deleted release left asset rows in round {round}"
        );
    }
}
