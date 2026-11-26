//! Runs a store flow on every backend: memory, SQLite in a temporary directory, and
//! Postgres when `BEE_TEST_PG` holds a connection URL. Declared by the including test file
//! with `#[path = "enterprise/billing_models_backends.rs"] mod backends;`.
#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bee_forge::ids::ManualClock;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlBackend, SqlConfig, SqlStore};

/// 2026-10-18T00:00:00Z.
pub const T0: u64 = 1_792_281_600_000;
/// One hour in milliseconds.
pub const HOUR: u64 = 3_600_000;
/// One day in milliseconds.
pub const DAY: u64 = 86_400_000;

static SCHEMAS: AtomicU32 = AtomicU32::new(0);

/// A manual clock reading [`T0`].
pub fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(T0))
}

/// Calls `f` once per available backend with a fresh store and a fresh clock.
pub async fn on_backends<F, Fut>(f: F)
where
    F: Fn(Arc<dyn ForgeStore>, Arc<ManualClock>) -> Fut,
    Fut: Future<Output = ()>,
{
    f(Arc::new(MemoryStore::new()), clock()).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let c = clock();
    let sqlite = SqlStore::open(SqlConfig::sqlite(dir.path()), c.clone())
        .await
        .expect("open sqlite");
    f(Arc::new(sqlite), c).await;

    if let Ok(url) = std::env::var("BEE_TEST_PG") {
        let schema = format!(
            "forge_test_bm_{}_{}",
            std::process::id(),
            SCHEMAS.fetch_add(1, Ordering::SeqCst)
        );
        let mut cfg = SqlConfig::parse(&url).expect("parse BEE_TEST_PG");
        if let SqlBackend::Postgres { schema: s, .. } = &mut cfg.backend {
            *s = schema;
        }
        cfg.default_pool_size = 3;
        let c = clock();
        let pg = SqlStore::open(cfg, c.clone()).await.expect("open postgres");
        f(Arc::new(pg), c).await;
    }
}
