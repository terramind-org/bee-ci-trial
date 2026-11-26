//! GC on SQLite and Postgres: what is kept, what goes, dry runs, safety windows, writes
//! between mark and sweep, and crashes during GC.

use crate::common;
use bee_core::ObjectId;
use bee_object::blob_id;
use bee_store::faults::points;
use bee_store::gc::{self, GcOptions};
use bee_store::{
    BlobStore, Expected, Fault, FaultRule, MetaStore, ReadFrom, RefUpdate, SweepOutcome, SweepPlan,
};
use bee_testkit::fixtures::{Change, ref_name};
use common::{Env, Open, TestRepo};

struct World {
    repo: TestRepo,
    main_tip: ObjectId,
    old_draft: ObjectId,
    young_draft: ObjectId,
}

async fn world<S: Open>(env: &Env<S>) -> World {
    let repo = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    env.commit_on(&repo, &main, &[Change::put("a", "keep-a")])
        .await
        .unwrap();
    let main_tip = env
        .commit_on(&repo, &main, &[Change::put("b", "keep-b")])
        .await
        .unwrap();
    let old_draft = env
        .commit(
            &repo,
            Some(main_tip),
            &[Change::put("tmp/scratch", "old-draft-only")],
        )
        .await
        .unwrap();
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

async fn has_row<S: Open>(env: &Env<S>, w: &World, id: &ObjectId) -> bool {
    env.meta
        .get_object(w.repo.org, id, ReadFrom::Primary)
        .await
        .unwrap()
        .is_some()
}

async fn dry_run_then_real_run<S: Open>() {
    let env = env_or_skip!(S);
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
    assert!(has_row(&env, &w, &doomed[0]).await);
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
        assert!(
            env.meta
                .get_uploads(w.repo.org, id)
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        env.meta
            .get_changeset(w.repo.repo, &w.old_draft, ReadFrom::Primary)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        env.meta
            .parents(w.repo.repo, &w.old_draft)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        env.meta
            .get_derived(w.repo.repo, &w.old_draft, "graph", 1, ReadFrom::Primary)
            .await
            .unwrap()
            .is_none()
    );
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
both!(dry_run_then_real_run);

async fn retention_is_configurable_and_refs_keep_history<S: Open>() {
    let env = env_or_skip!(S);
    let w = world(&env).await;
    let mut opts = GcOptions::at(env.now());
    opts.draft_retention_ms = Some(60 * bee_store::DAY_MS);
    opts.dry_run = true;
    let plan = gc::plan(env.meta_dyn(), env.blobs.as_ref(), w.repo.org, &opts)
        .await
        .unwrap();
    assert!(plan.sweep.changesets.is_empty());
    let mut opts = GcOptions::at(env.now());
    opts.dry_run = true;
    opts.extra_roots = vec![(w.repo.repo, w.old_draft)];
    let plan = gc::plan(env.meta_dyn(), env.blobs.as_ref(), w.repo.org, &opts)
        .await
        .unwrap();
    assert!(plan.sweep.changesets.is_empty());
    assert!(!plan.sweep.objects.contains(&blob_id(b"old-draft-only")));
}
both!(retention_is_configurable_and_refs_keep_history);

async fn a_write_after_the_mark_defers_the_sweep<S: Open>() {
    let env = env_or_skip!(S);
    let w = world(&env).await;
    let opts = GcOptions::at(env.now());
    let plan = gc::plan(env.meta_dyn(), env.blobs.as_ref(), w.repo.org, &opts)
        .await
        .unwrap();
    assert!(plan.sweep.changesets.contains(&(w.repo.repo, w.old_draft)));
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
both!(a_write_after_the_mark_defers_the_sweep);

async fn reuploading_a_candidate_after_the_mark_saves_it<S: Open>() {
    let env = env_or_skip!(S);
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
both!(reuploading_a_candidate_after_the_mark_saves_it);

async fn sweep_checks_epochs_children_and_purged_rows<S: Open>() {
    let env = env_or_skip!(S);
    let w = world(&env).await;
    let epoch = env.meta.gc_begin(w.repo.org, env.now()).await.unwrap();
    // An epoch the store never issued is refused.
    let mut bogus = epoch;
    bogus.at_ms += 1;
    let e = env
        .meta
        .gc_sweep(bogus, SweepPlan::default())
        .await
        .unwrap_err();
    assert!(e.is_invalid(bee_core::Subcode::InvalidField));
    // Deleting a parent while keeping its child is deferred.
    let e2 = env.meta.gc_begin(w.repo.org, env.now()).await.unwrap();
    assert_eq!(e2.seq, epoch.seq + 1);
    let out = env
        .meta
        .gc_sweep(
            e2,
            SweepPlan {
                changesets: vec![(w.repo.repo, w.main_tip)],
                objects: vec![],
            },
        )
        .await
        .unwrap();
    match out {
        SweepOutcome::Deferred { touched } => {
            assert!(
                touched.iter().any(|t| t.contains("kept child")),
                "{touched:?}"
            );
            assert!(
                touched.iter().any(|t| t.contains("ref target")),
                "{touched:?}"
            );
        }
        other => panic!("expected deferral, got {other:?}"),
    }
    // A planned id with no row is skipped; an unknown repo is NOT_FOUND.
    let out = env
        .meta
        .gc_sweep(
            e2,
            SweepPlan {
                changesets: vec![],
                objects: vec![blob_id(b"never existed")],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        SweepOutcome::Swept {
            changesets: vec![],
            objects: vec![]
        }
    );
    let e = env
        .meta
        .gc_sweep(
            e2,
            SweepPlan {
                changesets: vec![(bee_store::RepoId(bee_core::Ulid::from_u128(3)), w.old_draft)],
                objects: vec![],
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.reason, bee_core::Reason::NotFound);
    assert!(env.fsck(w.repo.org).await.unwrap().is_clean());
}
both!(sweep_checks_epochs_children_and_purged_rows);

async fn crash_during_gc_leaves_fsck_clean<S: Open>() {
    let env = env_or_skip!(S);
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
both!(crash_during_gc_leaves_fsck_clean);

async fn lfs_objects_are_roots<S: Open>() {
    use bee_store::DerivedRow;
    let env = env_or_skip!(S);
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
    let rows = env
        .meta
        .list_derived_by_type(w.repo.repo, gc::LFS_OBJECT_TYPE, None, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, 1);
    let none = env
        .meta
        .list_derived_by_type(w.repo.repo, gc::LFS_OBJECT_TYPE, Some(rows[0].0), 10)
        .await
        .unwrap();
    assert!(none.is_empty(), "the cursor is exclusive");
    env.clock.advance_days(2);
    gc::run(
        env.meta_dyn(),
        env.blobs.as_ref(),
        w.repo.org,
        &GcOptions::at(env.now()),
    )
    .await
    .unwrap();
    let has = |id| env.meta.get_object(w.repo.org, id, ReadFrom::Primary);
    assert!(
        has(&kept).await.unwrap().is_some(),
        "the LFS blob is a root"
    );
    assert!(env.blobs.has(w.repo.org, &[kept]).await.unwrap()[0]);
    assert!(
        has(&lost).await.unwrap().is_none(),
        "other unreferenced blobs go"
    );
    let report = env.fsck(w.repo.org).await.unwrap();
    assert!(report.is_clean(), "{:?}", report.violations);
}
both!(lfs_objects_are_roots);
