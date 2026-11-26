//! Fault injection at every store call and write-ordering step (SPEC §14.4, PRODUCT §4.3):
//! a crash, error or lost response anywhere never leaves a dangling reference, and a retry
//! converges on the fault-free result.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bee_core::ObjectId;
use bee_store::faults::points;
use bee_store::{Fault, FaultRule, MetaStore, ReadFrom};

use crate::fixtures::{Change, CommitSpec, TestEnv, TestRepo, quiet_injected_crashes, ref_name};

struct Setup {
    repo: TestRepo,
    parent: ObjectId,
}

async fn setup(env: &TestEnv) -> Setup {
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

/// The operation under test: a commit touching new and existing directories, with a large
/// blob and a meta too big to inline (so both blob-store paths run), then a ref advance.
async fn op(env: &TestEnv, s: &Setup) -> bee_core::Result<(ObjectId, u64)> {
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

async fn fresh() -> (Arc<TestEnv>, Setup) {
    let env = Arc::new(TestEnv::with_seed(42));
    let s = setup(&env).await;
    env.faults.reset();
    (env, s)
}

async fn baseline() -> ((ObjectId, u64), BTreeMap<String, u64>) {
    let (env, s) = fresh().await;
    let out = op(&env, &s).await.unwrap();
    let hits = env.faults.hits();
    assert!(env.fsck(s.repo.org).await.unwrap().is_clean());
    (out, hits)
}

async fn final_state(env: &TestEnv, s: &Setup) -> (Option<ObjectId>, u64) {
    let rf = env
        .meta
        .get_ref(s.repo.repo, &ref_name("main"))
        .await
        .unwrap()
        .unwrap();
    (rf.target, rf.version)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_at_every_write_step_leaves_fsck_clean() {
    quiet_injected_crashes();
    let ((want_id, want_version), hits) = baseline().await;
    // Every write-ordering step and every store call the operation makes is a point.
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
    assert!(hits.keys().any(|p| p.starts_with("blob.")));

    let faults = [
        ("crash", Fault::Crash),
        ("error", Fault::unavailable()),
        ("response-lost", Fault::response_lost()),
    ];
    let mut runs = 0;
    for (point, count) in &hits {
        for n in 0..*count {
            for (label, fault) in &faults {
                let (env, s) = fresh().await;
                env.faults
                    .add(FaultRule::nth(point.clone(), n, fault.clone()));
                let e2 = env.clone();
                let s2 = Setup {
                    repo: s.repo,
                    parent: s.parent,
                };
                let res = tokio::spawn(async move { op(&e2, &s2).await }).await;
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
                    "{label} at {point}#{n} left violations: {:?}",
                    report.violations
                );
                // Retrying converges on exactly the fault-free result.
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

#[tokio::test]
async fn delay_at_every_write_step_only_delays() {
    let ((want_id, want_version), _) = baseline().await;
    for step in points::WRITE_STEPS {
        let (env, s) = fresh().await;
        env.faults.add(FaultRule::always(
            step,
            Fault::Delay(Duration::from_millis(1)),
        ));
        let (id, version) = op(&env, &s).await.unwrap();
        assert_eq!((id, version), (want_id, want_version));
        assert!(env.fsck(s.repo.org).await.unwrap().is_clean());
    }
}

#[tokio::test]
async fn response_lost_on_cas_is_idempotent_for_land() {
    // "Committed, response lost" on the ref CAS: the client retries Land; every member is
    // already in the landed index from the same source, so the recorded result comes back
    // and nothing is written twice (SPEC §7.4 step 4).
    let (env, s) = fresh().await;
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
    // The retry: read the ref, consult the landed index, skip.
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
    // A blind re-send of the same CAS is refused rather than applied twice.
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

#[tokio::test]
async fn injected_transaction_failures_have_no_effect() {
    let (env, s) = fresh().await;
    let before = env.mem().commit_seq();
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
    assert!(env.mem().commit_seq() > before);
    let changesets = env
        .meta
        .list_changesets(s.repo.repo, None, 100)
        .await
        .unwrap();
    assert_eq!(changesets.len(), 2);
    assert!(env.fsck(s.repo.org).await.unwrap().is_clean());
}
