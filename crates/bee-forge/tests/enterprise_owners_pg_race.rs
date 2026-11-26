//! PostgreSQL races in enterprise owner protection.

use std::sync::Arc;

use crate::common;
use bee_forge::Cx;
use bee_forge::enterprise::{Enterprise, EnterpriseConfig};
use bee_forge::ids::SystemClock;
use bee_forge::store::ForgeStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::{ErrorReason, Principal, PrincipalKind};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_owner_removals_leave_one_owner_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_enterprise_owner_race_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let a_forge = common::env_with(first, Arc::new(SystemClock)).await.forge;
    let b_forge = common::env_with(second, Arc::new(SystemClock)).await.forge;
    let a_accounts = Enterprise::new(a_forge.clone(), EnterpriseConfig::new("https://test"))
        .accounts
        .clone();
    let b_accounts = Enterprise::new(b_forge.clone(), EnterpriseConfig::new("https://test"))
        .accounts
        .clone();

    for i in 0..100 {
        let owner_a = format!("owner-a-{i}");
        let owner_b = format!("owner-b-{i}");
        let slug = format!("owner-race-{i}");
        let enterprise = a_accounts
            .create_enterprise(
                &Cx::system(),
                &slug,
                "",
                Principal {
                    id: owner_a.clone(),
                    kind: PrincipalKind::Human as i32,
                    display: owner_a.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        a_accounts
            .add_owner(&Cx::system(), &slug, &owner_b)
            .await
            .unwrap();

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let left_accounts = a_accounts.clone();
        let right_accounts = b_accounts.clone();
        let slug_left = slug.clone();
        let owner_left = owner_a.clone();
        let slug_right = slug.clone();
        let owner_right = owner_b.clone();
        let (left, right) = tokio::join!(
            async move {
                left_barrier.wait().await;
                left_accounts
                    .remove_owner(&Cx::system(), &slug_left, &owner_left)
                    .await
            },
            async move {
                right_barrier.wait().await;
                right_accounts
                    .remove_owner(&Cx::system(), &slug_right, &owner_right)
                    .await
            }
        );
        assert!(
            left.is_ok() ^ right.is_ok(),
            "exactly one removal succeeds: {left:?} {right:?}"
        );
        let loser = left.as_ref().err().or(right.as_ref().err()).unwrap();
        assert_eq!(loser.reason, ErrorReason::InvalidState);
        assert_eq!(loser.subcode, "");
        assert_eq!(loser.message, "an enterprise needs at least one owner");
        let (owners, _) = a_accounts
            .owners(&Cx::system(), &slug, "", 10)
            .await
            .unwrap();
        assert_eq!(
            owners.len(),
            1,
            "enterprise {} must retain one owner",
            enterprise.id
        );
    }
}
