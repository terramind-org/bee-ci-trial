//! PostgreSQL race test for the single-owner verified-email invariant.

use std::sync::Arc;

use bee_forge::Cx;
use bee_forge::ids::SystemClock;
use bee_forge::store::ForgeStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::PrincipalKind;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_email_verification_has_one_owner_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_verified_email_race_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let a_forge = crate::common::env_with(first, Arc::new(SystemClock))
        .await
        .forge;
    let b_forge = crate::common::env_with(second, Arc::new(SystemClock))
        .await
        .forge;
    let a_users = a_forge.community.users.clone();
    let b_users = b_forge.community.users.clone();

    a_users
        .add_email(
            &Cx::new("reader", PrincipalKind::Human),
            "reader-primary@example.com",
        )
        .await
        .unwrap();
    b_users
        .add_email(
            &Cx::new("writer", PrincipalKind::Human),
            "writer-primary@example.com",
        )
        .await
        .unwrap();
    let primary_tokens = a_users
        .claim_verifications(&Cx::system(), 10)
        .await
        .unwrap();
    for token in primary_tokens {
        a_users.verify_email(&token.token).await.unwrap();
    }

    for round in 0..100 {
        let email = format!("verified-email-{round}@example.com");
        a_users
            .add_email(&Cx::new("reader", PrincipalKind::Human), &email)
            .await
            .unwrap();
        b_users
            .add_email(&Cx::new("writer", PrincipalKind::Human), &email)
            .await
            .unwrap();
        let tokens = a_users
            .claim_verifications(&Cx::system(), 10)
            .await
            .unwrap();
        let reader_token = tokens
            .iter()
            .find(|token| token.principal_id == "reader" && token.email == email)
            .unwrap()
            .token
            .clone();
        let writer_token = tokens
            .iter()
            .find(|token| token.principal_id == "writer" && token.email == email)
            .unwrap()
            .token
            .clone();

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let left_users = a_users.clone();
        let right_users = b_users.clone();
        let (left, right) = tokio::join!(
            async move {
                left_barrier.wait().await;
                left_users.verify_email(&reader_token).await
            },
            async move {
                right_barrier.wait().await;
                right_users.verify_email(&writer_token).await
            }
        );
        assert!(
            left.is_ok() ^ right.is_ok(),
            "round {round}: exactly one verification succeeds: {left:?} {right:?}"
        );
        let loser = left.as_ref().err().or(right.as_ref().err()).unwrap();
        assert_eq!(loser.reason, bee_proto::v1::ErrorReason::AlreadyExists);
        assert_eq!(loser.subcode, "email_taken");

        let resolved_a = a_users
            .resolve_commit_emails(&Cx::system(), std::slice::from_ref(&email))
            .await
            .unwrap();
        let resolved_b = b_users
            .resolve_commit_emails(&Cx::system(), std::slice::from_ref(&email))
            .await
            .unwrap();
        let owner_a = resolved_a[0].principal.as_ref().unwrap().id.as_str();
        let owner_b = resolved_b[0].principal.as_ref().unwrap().id.as_str();
        assert_eq!(owner_a, owner_b, "round {round}: handles disagree on owner");
        assert!(
            owner_a == "reader" || owner_a == "writer",
            "round {round}: unexpected owner {owner_a}"
        );
        assert_eq!(owner_a == "reader", left.is_ok());

        a_users
            .delete_email(&Cx::new("reader", PrincipalKind::Human), &email)
            .await
            .unwrap();
        b_users
            .delete_email(&Cx::new("writer", PrincipalKind::Human), &email)
            .await
            .unwrap();
    }
}
