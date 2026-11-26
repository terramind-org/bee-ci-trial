//! GC: what is kept, what goes, dry runs, safety windows and concurrent writes.

use bee_core::ObjectId;
use bee_object::blob_id;
use bee_store::faults::points;
use bee_store::gc::{self, GcOptions};
use bee_store::{
    BlobStore, Expected, Fault, FaultRule, MetaStore, ReadFrom, RefUpdate, SweepOutcome,
};

use crate::fixtures::{Change, TestEnv, TestRepo, ref_name};

struct World {
    repo: TestRepo,
    main_tip: ObjectId,
    old_draft: ObjectId,
    young_draft: ObjectId,
}

async fn world(env: &TestEnv) -> World {
    let repo = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    env.commit_on(&repo, &main, &[Change::put("a", "keep-a")])
        .await
        .unwrap();
    let main_tip = env
        .commit_on(&repo, &main, &[Change::put("b", "keep-b")])
        .await
        .unwrap();
    // An abandoned draft with content nothing else uses.
    let old_draft = env
        .commit(
            &repo,
            Some(main_tip),
            &[Change::put("tmp/scratch", "old-draft-only")],
        )
        .await
        .unwrap();
    // An unreferenced upload whose crash left only bytes (no row).
    env.faults
        .add(FaultRule::nth(points::UPLOAD_ROW, 0, Fault::unavailable()));
    let _ = env
        .writer
        .upload(
            repo.org,
            vec![(blob_id(b"orphan"), b"orphan".to_vec())],
            None,
            env.now(),
        )
        .await;
    env.faults.clear_rules();
    // An upload with a row that nothing references.
    env.writer
        .upload(
            repo.org,
            vec![(blob_id(b"unreferenced"), b"unreferenced".to_vec())],
            None,
            env.now(),
        )
        .await
        .unwrap();
    env.clock.advance_days(31);
    let young_draft = env
        .commit(&repo, Some(main_tip), &[Change::put("y", "young-draft")])
        .await
        .unwrap();
    // A fresh upload, inside the 24-hour window.
    env.writer
        .upload(
            repo.org,
            vec![(blob_id(b"fresh"), b"fresh".to_vec())],
            None,
            env.now(),
        )
        .await
        .unwrap();
    World {
        repo,
        main_tip,
        old_draft,
        young_draft,
    }
}

async fn has_row(env: &TestEnv, w: &World, id: &ObjectId) -> bool {
    env.meta
        .get_object(w.repo.org, id, ReadFrom::Primary)
        .await
        .unwrap()
        .is_some()
}

#[tokio::test]
async fn dry_run_then_real_run() {
    let env = TestEnv::new();
    let w = world(&env).await;
    let mut opts = GcOptions::at(env.now());
    opts.dry_run = true;
    let dry = gc::run(env.meta_dyn(), env.blobs.as_ref(), w.repo.org, &opts)
        .await
        .unwrap();
    assert!(dry.outcome.is_none());
    assert!(dry.plan.epoch.is_none());
    assert_eq!(dry.plan.sweep.changesets, vec![(w.repo.repo, w.old_draft)]);
    let doomed = [blob_id(b"old-draft-only"), blob_id(b"unreferenced")];
    for id in &doomed {
        assert!(dry.plan.sweep.objects.contains(id), "{id}");
    }
    assert_eq!(dry.plan.orphan_bytes, vec![blob_id(b"orphan")]);
    // A dry run changes nothing.
    assert!(has_row(&env, &w, &doomed[0]).await);
    assert!(
        env.blobs
            .has(w.repo.org, &[blob_id(b"orphan")])
            .await
            .unwrap()[0]
    );
    assert!(env.fsck(w.repo.org).await.unwrap().is_clean());

    opts.dry_run = false;
    let real = gc::run(env.meta_dyn(), env.blobs.as_ref(), w.repo.org, &opts)
        .await
        .unwrap();
    assert!(matches!(real.outcome, Some(SweepOutcome::Swept { .. })));
    assert_eq!(real.plan.sweep, dry.plan.sweep);
    for id in &doomed {
        assert!(!has_row(&env, &w, id).await);
        assert!(!env.blobs.has(w.repo.org, &[*id]).await.unwrap()[0]);
    }
    assert!(
        !env.blobs
            .has(w.repo.org, &[blob_id(b"orphan")])
            .await
            .unwrap()[0]
    );
    assert!(
        env.meta
            .get_changeset(w.repo.repo, &w.old_draft, ReadFrom::Primary)
            .await
            .unwrap()
            .is_none()
    );
    // Kept: everything on main, the young draft, the fresh upload.
    for id in [w.main_tip, w.young_draft] {
        assert!(
            env.meta
                .get_changeset(w.repo.repo, &id, ReadFrom::Primary)
                .await
                .unwrap()
                .is_some()
        );
    }
    for data in [&b"keep-a"[..], b"keep-b", b"young-draft", b"fresh"] {
        assert!(has_row(&env, &w, &blob_id(data)).await);
    }
    let report = env.fsck(w.repo.org).await.unwrap();
    assert!(report.is_clean(), "{:?}", report.violations);
    assert!(report.stats.orphan_bytes.is_empty());
    // A second run finds nothing more.
    let again = gc::run(
        env.meta_dyn(),
        env.blobs.as_ref(),
        w.repo.org,
        &GcOptions::at(env.now()),
    )
    .await
    .unwrap();
    assert!(again.plan.sweep.changesets.is_empty() && again.plan.sweep.objects.is_empty());
}

#[tokio::test]
async fn retention_is_configurable_and_refs_keep_history() {
    let env = TestEnv::new();
    let w = world(&env).await;
    // A 60-day retention keeps the 31-day-old draft.
    let mut opts = GcOptions::at(env.now());
    opts.draft_retention_ms = Some(60 * bee_store::DAY_MS);
    opts.dry_run = true;
    let plan = gc::plan(env.meta_dyn(), env.blobs.as_ref(), w.repo.org, &opts)
        .await
        .unwrap();
    assert!(plan.sweep.changesets.is_empty());
    // A workspace ref on the old draft keeps it too, as does an extra root.
    let mut opts = GcOptions::at(env.now());
    opts.dry_run = true;
    opts.extra_roots = vec![(w.repo.repo, w.old_draft)];
    let plan = gc::plan(env.meta_dyn(), env.blobs.as_ref(), w.repo.org, &opts)
        .await
        .unwrap();
    assert!(plan.sweep.changesets.is_empty());
    assert!(!plan.sweep.objects.contains(&blob_id(b"old-draft-only")));
}

#[tokio::test]
async fn a_write_after_the_mark_defers_the_sweep() {
    let env = TestEnv::new();
    let w = world(&env).await;
    let opts = GcOptions::at(env.now());
    let plan = gc::plan(env.meta_dyn(), env.blobs.as_ref(), w.repo.org, &opts)
        .await
        .unwrap();
    assert!(plan.sweep.changesets.contains(&(w.repo.repo, w.old_draft)));
    // Between mark and sweep an agent parks the old draft on a workspace ref.
    env.meta
        .update_ref(RefUpdate::set(
            w.repo.repo,
            ref_name("ws/agent/park"),
            Expected::Absent,
            Some(w.old_draft),
            "agent",
            env.now(),
        ))
        .await
        .unwrap();
    let report = gc::sweep(env.meta_dyn(), env.blobs.as_ref(), w.repo.org, plan, &opts)
        .await
        .unwrap();
    assert!(report.deferred());
    assert!(report.deleted_bytes.is_empty());
    assert!(has_row(&env, &w, &blob_id(b"old-draft-only")).await);
    assert!(env.fsck(w.repo.org).await.unwrap().is_clean());
    // The next run sees the ref and keeps the draft.
    let next = gc::run(
        env.meta_dyn(),
        env.blobs.as_ref(),
        w.repo.org,
        &GcOptions::at(env.now()),
    )
    .await
    .unwrap();
    assert!(!next.deferred());
    assert!(
        !next
            .plan
            .sweep
            .changesets
            .contains(&(w.repo.repo, w.old_draft))
    );
    assert!(env.fsck(w.repo.org).await.unwrap().is_clean());
}

#[tokio::test]
async fn reuploading_a_candidate_after_the_mark_saves_it() {
    let env = TestEnv::new();
    let w = world(&env).await;
    let opts = GcOptions::at(env.now());
    let plan = gc::plan(env.meta_dyn(), env.blobs.as_ref(), w.repo.org, &opts)
        .await
        .unwrap();
    let id = blob_id(b"unreferenced");
    assert!(plan.sweep.objects.contains(&id));
    env.writer
        .upload(
            w.repo.org,
            vec![(id, b"unreferenced".to_vec())],
            None,
            env.now(),
        )
        .await
        .unwrap();
    let report = gc::sweep(env.meta_dyn(), env.blobs.as_ref(), w.repo.org, plan, &opts)
        .await
        .unwrap();
    assert!(report.deferred());
    assert!(has_row(&env, &w, &id).await);
    assert!(env.blobs.has(w.repo.org, &[id]).await.unwrap()[0]);
}

#[tokio::test]
async fn crash_during_gc_leaves_fsck_clean() {
    // Rows are deleted before bytes, so a crash between the two leaves orphans only.
    let env = TestEnv::new();
    let w = world(&env).await;
    env.faults
        .add(FaultRule::always("blob.delete", Fault::unavailable()));
    let res = gc::run(
        env.meta_dyn(),
        env.blobs.as_ref(),
        w.repo.org,
        &GcOptions::at(env.now()),
    )
    .await;
    assert!(res.is_err());
    let report = env.fsck(w.repo.org).await.unwrap();
    assert!(report.is_clean(), "{:?}", report.violations);
    assert!(!report.stats.orphan_bytes.is_empty());
    env.faults.clear_rules();
    env.clock.advance_days(2);
    gc::run(
        env.meta_dyn(),
        env.blobs.as_ref(),
        w.repo.org,
        &GcOptions::at(env.now()),
    )
    .await
    .unwrap();
    let report = env.fsck(w.repo.org).await.unwrap();
    assert!(report.is_clean());
    assert!(report.stats.orphan_bytes.is_empty());
}

#[tokio::test]
async fn lfs_objects_are_roots() {
    use bee_store::DerivedRow;
    let env = TestEnv::new();
    let w = world(&env).await;
    let kept = blob_id(b"lfs-object-bytes");
    let lost = blob_id(b"not-an-lfs-object");
    env.writer
        .upload(
            w.repo.org,
            vec![
                (kept, b"lfs-object-bytes".to_vec()),
                (lost, b"not-an-lfs-object".to_vec()),
            ],
            None,
            env.now(),
        )
        .await
        .unwrap();
    env.meta
        .put_derived(
            w.repo.repo,
            vec![DerivedRow {
                key: ObjectId::git_foreign(&[7u8; 32]).unwrap(),
                type_name: gc::LFS_OBJECT_TYPE.into(),
                version: 1,
                value: kept.as_bytes().to_vec(),
            }],
        )
        .await
        .unwrap();
    env.clock.advance_days(2);
    gc::run(
        env.meta_dyn(),
        env.blobs.as_ref(),
        w.repo.org,
        &GcOptions::at(env.now()),
    )
    .await
    .unwrap();
    assert!(has_row(&env, &w, &kept).await, "the LFS blob is a root");
    assert!(!has_row(&env, &w, &lost).await);
}
