//! PostgreSQL races in installed-app IP range replacement.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::common;
use bee_forge::Cx;
use bee_forge::enterprise::common::ScopeResolver;
use bee_forge::enterprise::{Enterprise, EnterpriseConfig};
use bee_forge::ids::SystemClock;
use bee_forge::store::ForgeStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::{Principal, PrincipalKind};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_app_ip_range_replacements_are_atomic_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_ip_ranges_race_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let a_env = common::env_with(first, Arc::new(SystemClock)).await;
    let b_env = common::env_with(second, Arc::new(SystemClock)).await;
    let a_enterprise = Enterprise::new(
        a_env.forge.clone(),
        EnterpriseConfig::new("https://example.test"),
    );
    let b_enterprise = Enterprise::new(
        b_env.forge.clone(),
        EnterpriseConfig::new("https://example.test"),
    );
    let actor = Cx::new("owner", PrincipalKind::Human);
    let org = a_env
        .forge
        .orgs
        .create_org(
            &actor,
            "ip-ranges-race",
            "",
            Principal {
                id: "owner".into(),
                kind: PrincipalKind::Human as i32,
                display: "owner".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let scope = a_enterprise
        .accounts
        .resolve_org(&actor, &org.id)
        .await
        .unwrap();

    for round in 0..100 {
        let app_id = format!("app-{round}");
        let left = [format!("10.{round}.0.0/16")];
        let right = [format!("192.0.{round}.0/24")];
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let left_network = a_enterprise.network.clone();
        let right_network = b_enterprise.network.clone();
        let left_scope = scope.clone();
        let right_scope = scope.clone();
        let left_app = app_id.clone();
        let right_app = app_id.clone();
        let left_ranges = left.clone();
        let right_ranges = right.clone();
        let actor_left = actor.clone();
        let actor_right = actor.clone();
        let (a, b) = tokio::join!(
            async move {
                left_barrier.wait().await;
                left_network
                    .set_app_ranges(&actor_left, &left_scope, &left_app, &left_ranges)
                    .await
            },
            async move {
                right_barrier.wait().await;
                right_network
                    .set_app_ranges(&actor_right, &right_scope, &right_app, &right_ranges)
                    .await
            }
        );
        a.unwrap();
        b.unwrap();

        let (entries, _) = a_enterprise
            .network
            .ip_entries(&actor, &scope, "", 1000)
            .await
            .unwrap();
        let actual: BTreeSet<_> = entries
            .into_iter()
            .filter(|entry| entry.app_id == app_id)
            .map(|entry| entry.cidr)
            .collect();
        let left: BTreeSet<_> = left.into_iter().collect();
        let right: BTreeSet<_> = right.into_iter().collect();
        assert!(
            actual == left || actual == right,
            "round {round}: final ranges must equal one replacement, got {actual:?}"
        );
    }
}
