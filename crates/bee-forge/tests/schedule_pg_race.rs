//! PostgreSQL race coverage for scheduled workflow minute claims.

use std::sync::Arc;

use bee_forge::Cx;
use bee_forge::ci::CiConfig;
use bee_forge::ids::SystemClock;
use bee_forge::store::ForgeStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_workflow::cron::UtcMinute;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_scheduled_workflow_fires_once_per_minute_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_schedule_race_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let a = crate::ci::world_on(first, CiConfig::default()).await;
    let b_ci = a.another_ci_handle_on(second).await;

    let workflow = "on:\n  schedule:\n    - cron: '* * * * *'\njobs:\n  scheduled:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo scheduled\n";
    a.push(
        "main",
        &[(".github/workflows/scheduled.yml", Some(workflow))],
        "install scheduled workflow",
    )
    .await;

    let cx = Cx::system();
    for minute in 0..100 {
        let time = UtcMinute {
            year: 2026,
            month: 1,
            day: 1,
            hour: minute / 60,
            minute: minute % 60,
        };
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let left_ci = a.ci.clone();
        let right_ci = b_ci.clone();
        let left_cx = cx.clone();
        let right_cx = cx.clone();
        let (left, right) = tokio::join!(
            async move {
                left_barrier.wait().await;
                left_ci.tick_schedule(&left_cx, time).await
            },
            async move {
                right_barrier.wait().await;
                right_ci.tick_schedule(&right_cx, time).await
            }
        );
        let left = left.unwrap();
        let right = right.unwrap();
        let runs = a.ci.list_runs(&cx, &a.repo.id, "").await.unwrap();
        assert_eq!(
            runs.len(),
            (minute + 1) as usize,
            "exactly one new scheduled run for minute {minute}; tick results left={left:?}, right={right:?}, persisted runs={:?}",
            runs.iter()
                .map(|run| (&run.id, &run.event))
                .collect::<Vec<_>>()
        );
    }
}
