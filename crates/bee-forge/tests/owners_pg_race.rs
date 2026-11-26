//! PostgreSQL races in organization owner invariants.

use std::sync::Arc;

use crate::common;
use bee_forge::Cx;
use bee_forge::ids::SystemClock;
use bee_forge::store::ForgeStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::{ErrorReason, Principal, PrincipalKind, Role};

fn principal(id: &str) -> Principal {
    Principal {
        id: id.into(),
        kind: PrincipalKind::Human as i32,
        display: id.into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_owner_demotions_leave_one_owner_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_org_owner_race_{}", std::process::id());
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

    for i in 0..24 {
        let owner = format!("owner-{i}");
        let other = format!("other-{i}");
        let org_name = format!("owner-race-{i}");
        let org = a_forge
            .orgs
            .create_org(
                &Cx::new(&owner, PrincipalKind::Human),
                &org_name,
                "",
                principal(&owner),
            )
            .await
            .unwrap();
        a_forge
            .orgs
            .set_member(
                &Cx::new(&owner, PrincipalKind::Human),
                &org.id,
                principal(&other),
                Role::Owner,
            )
            .await
            .unwrap();

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let left_orgs = a_forge.orgs.clone();
        let right_orgs = b_forge.orgs.clone();
        let org_id = org.id.clone();
        let left_org_id = org_id.clone();
        let owner_left = owner.clone();
        let other_left = other.clone();
        let owner_right = owner.clone();
        let other_right = other.clone();
        let (a, b) = tokio::join!(
            async move {
                left_barrier.wait().await;
                left_orgs
                    .set_member(
                        &Cx::new(&owner_left, PrincipalKind::Human),
                        &left_org_id,
                        principal(&other_left),
                        Role::Member,
                    )
                    .await
            },
            async move {
                right_barrier.wait().await;
                right_orgs
                    .set_member(
                        &Cx::new(&other_right, PrincipalKind::Human),
                        &org_id,
                        principal(&owner_right),
                        Role::Member,
                    )
                    .await
            }
        );
        assert!(
            a.is_ok() ^ b.is_ok(),
            "both demotions must not succeed: {a:?} {b:?}"
        );
        let loser = a.as_ref().err().or(b.as_ref().err()).unwrap();
        assert_eq!(loser.reason, ErrorReason::InvalidState);
        assert_eq!(loser.subcode, "");
        assert_eq!(loser.message, "an org needs at least one owner");
        let (members, _) = a_forge
            .orgs
            .members(&Cx::system(), &org.id, "", 10)
            .await
            .unwrap();
        assert!(
            members
                .iter()
                .any(|member| member.role == Role::Owner as i32)
        );
    }

    let owner = "remove-owner-a";
    let other = "remove-owner-b";
    let org = a_forge
        .orgs
        .create_org(
            &Cx::new(owner, PrincipalKind::Human),
            "owner-remove-race",
            "",
            principal(owner),
        )
        .await
        .unwrap();
    a_forge
        .orgs
        .set_member(
            &Cx::new(owner, PrincipalKind::Human),
            &org.id,
            principal(other),
            Role::Owner,
        )
        .await
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let left_barrier = barrier.clone();
    let right_barrier = barrier.clone();
    let left_orgs = a_forge.orgs.clone();
    let right_orgs = b_forge.orgs.clone();
    let org_id = org.id.clone();
    let left_org_id = org_id.clone();
    let (a, b) = tokio::join!(
        async move {
            left_barrier.wait().await;
            left_orgs
                .set_member(
                    &Cx::new(owner, PrincipalKind::Human),
                    &left_org_id,
                    principal(other),
                    Role::Unspecified,
                )
                .await
        },
        async move {
            right_barrier.wait().await;
            right_orgs
                .set_member(
                    &Cx::new(other, PrincipalKind::Human),
                    &org_id,
                    principal(owner),
                    Role::Unspecified,
                )
                .await
        }
    );
    assert!(
        a.is_ok() ^ b.is_ok(),
        "both removals must not succeed: {a:?} {b:?}"
    );
    let loser = a.as_ref().err().or(b.as_ref().err()).unwrap();
    assert_eq!(loser.reason, ErrorReason::InvalidState);
    assert_eq!(loser.subcode, "");
    assert_eq!(loser.message, "an org needs at least one owner");
    let (members, _) = a_forge
        .orgs
        .members(&Cx::system(), &org.id, "", 10)
        .await
        .unwrap();
    assert!(
        members
            .iter()
            .any(|member| member.role == Role::Owner as i32)
    );
}
