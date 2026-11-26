//! PostgreSQL races in signing-key ownership claims.

use std::sync::Arc;

use crate::common;
use bee_forge::Cx;
use bee_forge::community::keys::SIGNING_KEYS;
use bee_forge::ids::SystemClock;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::store::{ForgeStore, Key, Scan};
use bee_proto::v1::{ErrorReason, PrincipalKind, UserSigningKeyKind};

const SSH_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKeNcRRA9NoRkyq1+PxtPqGTLOzHONZHYZQ1pOsaQ6IM test";

#[tokio::test]
async fn sqlite_signing_key_duplicate_still_has_one_owner() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let forge = common::env_with(store, Arc::new(SystemClock)).await.forge;
    let keys = &forge.community.keys;
    let key = keys
        .add(
            &Cx::new("signing-owner-a", PrincipalKind::Human),
            UserSigningKeyKind::Ssh,
            "test key",
            SSH_KEY,
            &[],
        )
        .await
        .unwrap();
    let duplicate = keys
        .add(
            &Cx::new("signing-owner-b", PrincipalKind::Human),
            UserSigningKeyKind::Ssh,
            "test key",
            SSH_KEY,
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(duplicate.reason, ErrorReason::AlreadyExists);
    assert_eq!(
        duplicate.message,
        format!("signing key {} already exists", key.key_id)
    );
    let (owner, resolved) = keys
        .owner_of(&Cx::system(), &key.key_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner, "signing-owner-a");
    assert_eq!(resolved.id, key.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_signing_key_has_one_owner_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_signing_key_race_{}", std::process::id());
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
    let key_id = bee_forge::community::keys::parse_ssh(SSH_KEY)
        .unwrap()
        .key_id;

    for round in 0..100 {
        let owner_a = format!("signing-owner-a-{round}");
        let owner_b = format!("signing-owner-b-{round}");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let left_keys = a_forge.community.keys.clone();
        let right_keys = b_forge.community.keys.clone();
        let left_owner = owner_a.clone();
        let right_owner = owner_b.clone();
        let (a, b) = tokio::join!(
            async move {
                left_barrier.wait().await;
                left_keys
                    .add(
                        &Cx::new(&left_owner, PrincipalKind::Human),
                        UserSigningKeyKind::Ssh,
                        "test key",
                        SSH_KEY,
                        &[],
                    )
                    .await
            },
            async move {
                right_barrier.wait().await;
                right_keys
                    .add(
                        &Cx::new(&right_owner, PrincipalKind::Human),
                        UserSigningKeyKind::Ssh,
                        "test key",
                        SSH_KEY,
                        &[],
                    )
                    .await
            }
        );
        assert!(
            a.is_ok() ^ b.is_ok(),
            "exactly one owner claims key: {a:?} {b:?}"
        );
        let loser = a.as_ref().err().or(b.as_ref().err()).unwrap();
        assert_eq!(loser.reason, ErrorReason::AlreadyExists);
        assert_eq!(loser.subcode, "");
        assert_eq!(
            loser.message,
            format!("signing key {key_id} already exists")
        );

        let mut tx = a_forge
            .db
            .begin(SIGNING_KEYS.surface, &Cx::system())
            .await
            .unwrap();
        let rows = tx
            .scan(
                &SIGNING_KEYS,
                &Scan {
                    key: Some(Key::K1(key_id.clone())),
                    limit: 10,
                    ..Scan::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "one key row after round {round}");
        tx.commit().await.unwrap();

        let (owner, _) = a_forge
            .community
            .keys
            .owner_of(&Cx::system(), &key_id)
            .await
            .unwrap()
            .unwrap();
        assert!(owner == owner_a || owner == owner_b);
        a_forge
            .community
            .keys
            .delete(&Cx::new(owner, PrincipalKind::Human), &key_id)
            .await
            .unwrap();
    }
}
