//! The [`ForgeStore`] conformance suite, shared by every backend.
//!
//! `cargo test -p bee-forge` runs it against the memory store and SQLite, and against
//! Postgres when `BEE_TEST_PG` holds a connection URL. A new backend passes when
//! [`run_all`] passes. The suite uses its own tables on two private surfaces, so it can run
//! against a database that also holds real forge tables.
//!
//! Note for domain code: on Postgres a failed statement aborts the transaction, so after any
//! error a transaction must be dropped, never continued. The suite follows that rule.

use std::sync::Arc;
use std::time::Duration;

use bee_proto::v1::{ErrorReason, Event};
use tokio::time::Instant;

use super::{
    Cursor, ForgeStore, ForgeTx, Index, Key, Migration, MigrationOp, OutboxRecord, Phase, Row,
    Scan, Surface, TableDef,
};

/// First private surface.
pub const SURFACE_A: Surface = Surface("conformance_a");
/// Second private surface.
pub const SURFACE_B: Surface = Surface("conformance_b");

/// A table on [`SURFACE_A`] with a unique `k1`.
pub static ROWS: TableDef = TableDef {
    name: "forge_conformance_rows",
    surface: SURFACE_A,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::ScopeK2, Index::ScopeN1, Index::K1, Index::N1],
};

/// A table on [`SURFACE_B`].
pub static OTHER: TableDef = TableDef {
    name: "forge_conformance_other",
    surface: SURFACE_B,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// The suite's migrations.
pub static MIGRATION_1: Migration = Migration {
    module: "conformance",
    version: 1,
    name: "create_tables",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&ROWS),
        MigrationOp::CreateTable(&OTHER),
    ],
};

/// An index added by a second expand migration.
pub static MIGRATION_2: Migration = Migration {
    module: "conformance",
    version: 2,
    name: "add_k2_index",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateIndex(&OTHER, Index::K2)],
};

/// The same id as [`MIGRATION_2`] with different operations: must be refused once
/// [`MIGRATION_2`] was applied.
pub static MIGRATION_2_EDITED: Migration = Migration {
    module: "conformance",
    version: 2,
    name: "add_k2_index",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateIndex(&OTHER, Index::N1)],
};

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

async fn begin(store: &dyn ForgeStore, s: Surface) -> Box<dyn ForgeTx> {
    store.begin(s, soon()).await.expect("begin")
}

fn reason<T: std::fmt::Debug>(r: crate::error::Result<T>) -> ErrorReason {
    r.expect_err("expected an error").reason
}

/// Runs every check. Each scope used is fresh (derived from `prefix`), so the suite can be
/// rerun against the same database.
pub async fn run_all(store: Arc<dyn ForgeStore>, prefix: &str) {
    migrations(store.as_ref()).await;
    crud_and_versions(store.as_ref(), &format!("{prefix}-crud")).await;
    rollback_on_drop(store.as_ref(), &format!("{prefix}-rb")).await;
    scans(store.as_ref(), &format!("{prefix}-scan")).await;
    counters(store.clone(), &format!("{prefix}-ctr")).await;
    outbox(store.as_ref()).await;
    surfaces_are_enforced(store.as_ref()).await;
    pools_are_isolated(store.as_ref()).await;
}

/// Migrations are idempotent and applied ones are immutable.
pub async fn migrations(store: &dyn ForgeStore) {
    store
        .migrate(&[&MIGRATION_1, &MIGRATION_2])
        .await
        .expect("first migration run");
    let again = store
        .migrate(&[&MIGRATION_1, &MIGRATION_2])
        .await
        .expect("second migration run");
    assert!(
        again.applied.is_empty(),
        "rerun applied {:?}",
        again.applied
    );
    assert!(!again.already_applied.is_empty());
    let edited = store.migrate(&[&MIGRATION_1, &MIGRATION_2_EDITED]).await;
    assert_eq!(reason(edited), ErrorReason::Internal);
}

/// Insert, get, CAS update, upsert and delete.
pub async fn crud_and_versions(store: &dyn ForgeStore, scope: &str) {
    let mut tx = begin(store, SURFACE_A).await;
    let row = Row::new(scope, "a").k1("name-a").n1(5).msg(&Event {
        seq: 7,
        ..Default::default()
    });
    tx.insert(&ROWS, &row).await.expect("insert");
    let got = tx.get(&ROWS, scope, "a").await.unwrap().expect("row");
    assert_eq!(got.version, 1);
    assert_eq!(got.k1.as_deref(), Some("name-a"));
    assert_eq!(got.decode::<Event>().unwrap().seq, 7);
    tx.commit().await.expect("commit");

    let mut tx = begin(store, SURFACE_A).await;
    assert_eq!(
        reason(tx.insert(&ROWS, &row).await),
        ErrorReason::AlreadyExists
    );
    drop(tx);

    let mut tx = begin(store, SURFACE_A).await;
    let clash = Row::new(scope, "b").k1("name-a");
    assert_eq!(
        reason(tx.insert(&ROWS, &clash).await),
        ErrorReason::AlreadyExists
    );
    drop(tx);

    let mut tx = begin(store, SURFACE_A).await;
    tx.insert(&ROWS, &Row::new(scope, "b").k1("name-b"))
        .await
        .expect("insert b");
    let v = tx
        .update(&ROWS, &got.clone().k1("name-a2").version(1))
        .await
        .expect("cas update");
    assert_eq!(v, 2);
    tx.commit().await.unwrap();

    let mut tx = begin(store, SURFACE_A).await;
    let stale = tx.update(&ROWS, &got.clone().version(1)).await;
    let e = stale.expect_err("stale update");
    assert_eq!(e.reason, ErrorReason::VersionMismatch);
    match e.payload.as_deref() {
        Some(bee_proto::v1::error_detail::Payload::VersionMismatch(vm)) => {
            assert_eq!(vm.current_version, 2)
        }
        other => panic!("no version payload: {other:?}"),
    }
    drop(tx);

    let mut tx = begin(store, SURFACE_A).await;
    let missing = tx.update(&ROWS, &Row::new(scope, "zz").version(1)).await;
    assert_eq!(reason(missing), ErrorReason::NotFound);
    drop(tx);

    let mut tx = begin(store, SURFACE_A).await;
    let taken = tx
        .update(&ROWS, &Row::new(scope, "b").k1("name-a2").version(1))
        .await;
    assert_eq!(reason(taken), ErrorReason::AlreadyExists);
    drop(tx);

    let mut tx = begin(store, SURFACE_A).await;
    assert_eq!(tx.put(&ROWS, &Row::new(scope, "c")).await.unwrap(), 1);
    assert_eq!(tx.put(&ROWS, &Row::new(scope, "c").n1(9)).await.unwrap(), 2);
    assert!(tx.delete(&ROWS, scope, "b").await.unwrap());
    assert!(!tx.delete(&ROWS, scope, "b").await.unwrap());
    tx.commit().await.unwrap();

    let mut tx = begin(store, SURFACE_A).await;
    assert!(tx.get(&ROWS, scope, "b").await.unwrap().is_none());
    assert_eq!(
        tx.get(&ROWS, scope, "c").await.unwrap().unwrap().n1,
        Some(9)
    );
    assert_eq!(tx.count(&ROWS, scope, None).await.unwrap(), 2);
    assert_eq!(
        tx.count(&ROWS, scope, Some(&Key::K1("name-a2".into())))
            .await
            .unwrap(),
        1
    );
}

/// Dropping a transaction discards its writes.
pub async fn rollback_on_drop(store: &dyn ForgeStore, scope: &str) {
    let mut tx = begin(store, SURFACE_A).await;
    tx.insert(&ROWS, &Row::new(scope, "x")).await.unwrap();
    tx.add_n1(&ROWS, scope, "ctr", 3).await.unwrap();
    drop(tx);
    let mut tx = begin(store, SURFACE_A).await;
    assert!(tx.get(&ROWS, scope, "x").await.unwrap().is_none());
    assert!(tx.get(&ROWS, scope, "ctr").await.unwrap().is_none());
}

/// Ordering, filters and cursors.
pub async fn scans(store: &dyn ForgeStore, scope: &str) {
    let mut tx = begin(store, SURFACE_A).await;
    for i in 0..10i64 {
        let row = Row::new(scope, format!("r{i:02}"))
            .k2(if i % 2 == 0 { "even" } else { "odd" })
            .n1(100 - i);
        tx.insert(&ROWS, &row).await.unwrap();
    }
    tx.insert(&ROWS, &Row::new(format!("{scope}-other"), "r00").n1(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = begin(store, SURFACE_A).await;
    let all = tx.scan(&ROWS, &Scan::scope(scope, 100)).await.unwrap();
    let ids: Vec<_> = all.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids.len(), 10);
    assert!(ids.windows(2).all(|w| w[0] < w[1]));

    let page1 = tx.scan(&ROWS, &Scan::scope(scope, 4)).await.unwrap();
    let page2 = tx
        .scan(
            &ROWS,
            &Scan::scope(scope, 4).after(page1.last().map(Cursor::after)),
        )
        .await
        .unwrap();
    assert_eq!(page1.len(), 4);
    assert_eq!(page2.first().unwrap().id, "r04");

    let by_n1 = tx
        .scan(&ROWS, &Scan::scope(scope, 3).by_n1())
        .await
        .unwrap();
    assert_eq!(by_n1[0].id, "r09");
    let next = tx
        .scan(
            &ROWS,
            &Scan::scope(scope, 3)
                .by_n1()
                .after(by_n1.last().map(Cursor::after)),
        )
        .await
        .unwrap();
    assert_eq!(next[0].id, "r06");

    let desc = tx
        .scan(&ROWS, &Scan::scope(scope, 2).by_n1().desc())
        .await
        .unwrap();
    assert_eq!(desc[0].id, "r00");
    let desc2 = tx
        .scan(
            &ROWS,
            &Scan::scope(scope, 2)
                .by_n1()
                .desc()
                .after(desc.last().map(Cursor::after)),
        )
        .await
        .unwrap();
    assert_eq!(desc2[0].id, "r02");

    let evens = tx
        .scan(&ROWS, &Scan::scope(scope, 100).key(Key::K2("even".into())))
        .await
        .unwrap();
    assert_eq!(evens.len(), 5);

    let ranged = tx
        .scan(
            &ROWS,
            &Scan::scope(scope, 100).by_n1().n1_min(93).n1_max(95),
        )
        .await
        .unwrap();
    assert_eq!(ranged.len(), 3);

    let cross = tx
        .scan(
            &ROWS,
            &Scan {
                limit: 1000,
                n1_max: Some(1),
                order: super::Order::N1,
                ..Scan::default()
            },
        )
        .await
        .unwrap();
    assert!(cross.iter().any(|r| r.scope == format!("{scope}-other")));
}

/// Counters are atomic under concurrent transactions.
pub async fn counters(store: Arc<dyn ForgeStore>, scope: &str) {
    let mut handles = Vec::new();
    for _ in 0..8 {
        let store = store.clone();
        let scope = scope.to_string();
        handles.push(tokio::spawn(async move {
            let mut tx = store.begin(SURFACE_A, soon()).await.expect("begin");
            let v = tx.add_n1(&ROWS, &scope, "seq", 10).await.expect("add");
            tx.commit().await.expect("commit");
            v
        }));
    }
    let mut seen = Vec::new();
    for h in handles {
        seen.push(h.await.expect("join"));
    }
    seen.sort();
    assert_eq!(seen, (1..=8).map(|i| i * 10).collect::<Vec<i64>>());
}

/// The outbox is ordered, transactional and drained by id.
pub async fn outbox(store: &dyn ForgeStore) {
    let mut tx = begin(store, SURFACE_B).await;
    let existing = tx.outbox_peek(1000).await.unwrap();
    let ids: Vec<String> = existing.into_iter().map(|(i, _)| i).collect();
    tx.outbox_remove(&ids).await.unwrap();
    tx.commit().await.unwrap();

    let rec = |seq| OutboxRecord {
        event: Some(Event {
            seq,
            ..Default::default()
        }),
        audit: None,
    };
    let mut tx = begin(store, SURFACE_B).await;
    tx.outbox_push("01B", &rec(2)).await.unwrap();
    tx.outbox_push("01A", &rec(1)).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = begin(store, SURFACE_B).await;
    tx.outbox_push("01C", &rec(3)).await.unwrap();
    drop(tx);

    let mut tx = begin(store, SURFACE_B).await;
    let got = tx.outbox_peek(10).await.unwrap();
    let ids: Vec<_> = got.iter().map(|(i, _)| i.as_str()).collect();
    assert_eq!(ids, vec!["01A", "01B"]);
    assert_eq!(got[0].1.event.as_ref().unwrap().seq, 1);
    tx.outbox_remove(&["01A".to_string()]).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = begin(store, SURFACE_B).await;
    assert_eq!(tx.outbox_peek(10).await.unwrap().len(), 1);
    tx.outbox_remove(&["01B".to_string()]).await.unwrap();
    tx.commit().await.unwrap();
}

/// A transaction cannot touch another surface's table.
pub async fn surfaces_are_enforced(store: &dyn ForgeStore) {
    let mut tx = begin(store, SURFACE_A).await;
    assert_eq!(
        reason(tx.get(&OTHER, "s", "x").await),
        ErrorReason::Internal
    );
}

/// Holding a surface's connections never blocks another surface, and waits are bounded by
/// the deadline.
pub async fn pools_are_isolated(store: &dyn ForgeStore) {
    let mut held = Vec::new();
    let mut saturated = false;
    for _ in 0..64 {
        match store
            .begin(SURFACE_A, Instant::now() + Duration::from_millis(300))
            .await
        {
            Ok(tx) => held.push(tx),
            Err(e) => {
                assert_eq!(e.reason, ErrorReason::DeadlineExceeded, "{e}");
                saturated = true;
                break;
            }
        }
        // SQLite serializes writers at BEGIN IMMEDIATE; a held transaction already saturates
        // the surface for writers, which the next iteration observes as a deadline.
    }
    assert!(saturated, "surface A never saturated");
    let started = Instant::now();
    let mut other = store
        .begin(SURFACE_B, Instant::now() + Duration::from_secs(2))
        .await
        .expect("surface B must not wait for surface A");
    other.get(&OTHER, "s", "x").await.expect("read on B");
    other.commit().await.expect("commit on B");
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(held);
    let mut tx = begin(store, SURFACE_A).await;
    tx.get(&ROWS, "s", "x").await.expect("A usable again");
}
