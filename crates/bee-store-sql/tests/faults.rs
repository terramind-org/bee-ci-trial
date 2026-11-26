//! Fault injection at every store call and write-ordering step (SPEC §14.4), on SQLite and
//! Postgres: a crash, error or lost response anywhere never leaves a dangling reference, and a
//! retry converges on the fault-free result.

use crate::common;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bee_core::ObjectId;
use bee_store::faults::points;
use bee_store::{Fault, FaultRule, MetaStore, ReadFrom};
use bee_testkit::fixtures::{Change, CommitSpec, quiet_injected_crashes, ref_name};
use common::{Env, Open, TestRepo};

#[derive(Clone, Copy)]
struct Setup {
    repo: TestRepo,
    parent: ObjectId,
}

async fn setup<S: Open>(env: &Env<S>) -> Setup {
    let repo = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    env.commit_on(
        &repo,
        &main,
        &[
            Change::put("src/lib.rs", "fn a() {}\n"),
            Change::put("README", "hi\n"),
        ],
    )
    .await
    .unwrap();
    let parent = env
        .commit_on(
            &repo,
            &main,
            &[Change::put("src/util/mod.rs", "pub mod x;\n")],
        )
        .await
        .unwrap();
    Setup { repo, parent }
}

/// A commit with a large blob and a meta too big to inline, then a ref advance.
async fn op<S: Open>(env: &Env<S>, s: &Setup) -> bee_core::Result<(ObjectId, u64)> {
    let id = env
        .commit_with(
            &s.repo,
            CommitSpec {
                parents: vec![s.parent],
                changes: vec![
                    Change::put("src/util/x.rs", vec![b'x'; 70_000]),
                    Change::put("docs/guide.md", "# guide\n"),
                    Change::delete("README"),
                ],
                message: "m".repeat(70_000),
                change_id: None,
            },
        )
        .await?;
    let up = env
        .advance(&s.repo, &ref_name("main"), Some(s.parent), id)
        .await?;
    Ok((id, up.new_version))
}

async fn fresh<S: Open>() -> Option<(Arc<Env<S>>, Setup)> {
    let env = Arc::new(Env::<S>::with_seed(42).await?);
    let s = setup(&env).await;
    env.faults.reset();
    Some((env, s))
}

async fn final_state<S: Open>(env: &Env<S>, s: &Setup) -> (Option<ObjectId>, u64) {
    let rf = env
        .meta
        .get_ref(s.repo.repo, &ref_name("main"))
        .await
        .unwrap()
        .unwrap();
    (rf.target, rf.version)
}

async fn crash_at_every_write_step_leaves_fsck_clean<S: Open>() {
    quiet_injected_crashes();
    let Some((env, s)) = fresh::<S>().await else {
        return;
    };
    let (want_id, want_version) = op(&env, &s).await.unwrap();
    let hits: BTreeMap<String, u64> = env.faults.hits();
    assert!(env.fsck(s.repo.org).await.unwrap().is_clean());
    drop(env);
    for step in [
        points::UPLOAD_VERIFY,
        points::UPLOAD_BLOB,
        points::UPLOAD_ROW,
        points::CHANGESET_ROW,
        points::REF_CAS,
    ] {
        assert!(
            hits.get(step).copied().unwrap_or(0) > 0,
            "{step} not exercised"
        );
    }
    assert!(hits.keys().any(|p| p.starts_with("meta.")));
    let faults = [
        ("crash", Fault::Crash),
        ("error", Fault::unavailable()),
        ("response-lost", Fault::response_lost()),
    ];
    let mut runs = 0;
    for (point, count) in &hits {
        for n in 0..*count {
            for (label, fault) in &faults {
                let (env, s) = fresh::<S>().await.unwrap();
                env.faults
                    .add(FaultRule::nth(point.clone(), n, fault.clone()));
                let e2 = env.clone();
                let res = tokio::spawn(async move { op(&e2, &s).await }).await;
                match (label, &res) {
                    (&"crash", Err(j)) => assert!(j.is_panic(), "{point}#{n}: {j}"),
                    (&"crash", Ok(r)) => panic!("{point}#{n}: crash did not fire: {r:?}"),
                    (_, Ok(Err(_))) => {}
                    (_, other) => panic!("{point}#{n} {label}: unexpected {other:?}"),
                }
                assert_eq!(
                    env.faults.fired(),
                    vec![(point.clone(), n)],
                    "{point}#{n} {label}"
                );
                let report = env.fsck(s.repo.org).await.unwrap();
                assert!(
                    report.is_clean(),
                    "{} {label} at {point}#{n} left violations: {:?}",
                    S::NAME,
                    report.violations
                );
                env.faults.clear_rules();
                let (id, _) = op(&env, &s).await.unwrap();
                assert_eq!(id, want_id, "{label} at {point}#{n}");
                assert_eq!(final_state(&env, &s).await, (Some(want_id), want_version));
                let report = env.fsck(s.repo.org).await.unwrap();
                assert!(report.is_clean(), "after retry: {:?}", report.violations);
                runs += 1;
            }
        }
    }
    assert!(runs >= 60, "only {runs} fault runs");
}
both!(crash_at_every_write_step_leaves_fsck_clean, multi);

async fn delay_at_every_write_step_only_delays<S: Open>() {
    let Some((env, s)) = fresh::<S>().await else {
        return;
    };
    let want = op(&env, &s).await.unwrap();
    drop(env);
    for step in points::WRITE_STEPS {
        let (env, s) = fresh::<S>().await.unwrap();
        env.faults.add(FaultRule::always(
            step,
            Fault::Delay(Duration::from_millis(1)),
        ));
        assert_eq!(op(&env, &s).await.unwrap(), want);
        assert!(env.fsck(s.repo.org).await.unwrap().is_clean());
    }
}
both!(delay_at_every_write_step_only_delays);

async fn response_lost_on_cas_is_idempotent_for_land<S: Open>() {
    let Some((env, s)) = fresh::<S>().await else {
        return;
    };
    let main = ref_name("main");
    let c = env
        .commit(&s.repo, Some(s.parent), &[Change::put("f", "1")])
        .await
        .unwrap();
    let row = env
        .meta
        .get_changeset(s.repo.repo, &c, ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    let cid = *row.changeset.change_id();
    env.faults
        .add(FaultRule::nth(points::REF_CAS, 0, Fault::response_lost()));
    let land = |version: u64| bee_store::RefUpdate {
        repo: s.repo.repo,
        name: main.clone(),
        expected: bee_store::Expected::Version(version),
        steps: vec![bee_store::RefStep {
            target: Some(c),
            landed: vec![bee_store::LandedEntry {
                change_id: cid,
                source: c,
                result: c,
            }],
        }],
        principal: "agent".into(),
        at_ms: 1,
        events: vec![],
    };
    assert!(env.writer.update_ref(land(2)).await.is_err());
    let rf = env.meta.get_ref(s.repo.repo, &main).await.unwrap().unwrap();
    let recorded = env
        .meta
        .get_landed(s.repo.repo, &main, &[cid])
        .await
        .unwrap()
        .pop()
        .flatten()
        .expect("landed row committed");
    assert_eq!(
        (recorded.source, recorded.result, recorded.version),
        (c, c, 3)
    );
    assert_eq!((rf.target, rf.version), (Some(c), 3));
    let again = env.writer.update_ref(land(2)).await.unwrap_err();
    assert_eq!(again.reason, bee_core::Reason::VersionMismatch);
    assert_eq!(
        env.meta
            .ref_log(s.repo.repo, &main, 1, 10)
            .await
            .unwrap()
            .len(),
        3
    );
    assert!(env.fsck(s.repo.org).await.unwrap().is_clean());
}
both!(response_lost_on_cas_is_idempotent_for_land);

async fn injected_transaction_failures_have_no_effect<S: Open>() {
    let Some((env, s)) = fresh::<S>().await else {
        return;
    };
    let objects_before = env
        .meta
        .list_objects(s.repo.org, None, 10_000)
        .await
        .unwrap();
    env.faults.add(FaultRule::always(
        "meta.insert_changeset",
        Fault::unavailable(),
    ));
    let err = env
        .commit(&s.repo, Some(s.parent), &[Change::put("g", "2")])
        .await
        .unwrap_err();
    assert!(err.retryable);
    assert_eq!(err.reason, bee_core::Reason::Unavailable);
    // Uploads committed, the changeset did not.
    let objects_after = env
        .meta
        .list_objects(s.repo.org, None, 10_000)
        .await
        .unwrap();
    assert!(objects_after.len() > objects_before.len());
    let changesets = env
        .meta
        .list_changesets(s.repo.repo, None, 100)
        .await
        .unwrap();
    assert_eq!(changesets.len(), 2);
    assert!(env.fsck(s.repo.org).await.unwrap().is_clean());
}
both!(injected_transaction_failures_have_no_effect);
