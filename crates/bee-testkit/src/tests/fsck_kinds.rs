//! fsck reports each enumerated violation kind when, and only when, its invariant is broken.

use bee_core::{ChangeId, Mode, ObjectKind, RepoPath};
use bee_object::{Changeset, DeltaEntry, Meta, Tree, TreeEntry, blob_id, hash_id};
use bee_store::{
    EventRow, LandedRow, MetaStore, NewChangeset, ObjectRow, ObjectState, ReadFrom, RefRow,
    ViolationKind as V,
};

use crate::fixtures::{Change, TestEnv, TestRepo, ref_name};

async fn history(env: &TestEnv) -> (TestRepo, Vec<bee_core::ObjectId>) {
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    let a = env
        .commit_on(
            &r,
            &main,
            &[Change::put("src/a.rs", "a\n"), Change::put("x", "x")],
        )
        .await
        .unwrap();
    let b = env
        .commit_on(&r, &main, &[Change::put("src/b.rs", "b\n")])
        .await
        .unwrap();
    (r, vec![a, b])
}

async fn kinds(env: &TestEnv, r: &TestRepo) -> std::collections::BTreeSet<V> {
    env.fsck(r.org).await.unwrap().kinds()
}

#[tokio::test]
async fn clean_history_is_clean() {
    let env = TestEnv::new();
    let (r, _) = history(&env).await;
    let report = env.fsck(r.org).await.unwrap();
    assert!(report.is_clean(), "{:?}", report.violations);
    assert_eq!(report.stats.changesets, 2);
    assert_eq!(report.stats.refs, 1);
    assert_eq!(report.stats.ref_log_entries, 2);
    assert!(report.stats.objects > 0);
    let names: std::collections::BTreeSet<_> = V::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(names.len(), V::ALL.len());
}

#[tokio::test]
async fn object_level_violations() {
    let env = TestEnv::new();
    let (r, _) = history(&env).await;
    let blob = blob_id(b"a\n");
    // Bit rot in the blob store.
    env.mem_blobs()
        .debug_overwrite(r.org, &blob, b"A\n".to_vec());
    assert!(kinds(&env, &r).await.contains(&V::DigestMismatch));
    env.mem_blobs()
        .debug_overwrite(r.org, &blob, b"a\n".to_vec());
    assert!(env.fsck(r.org).await.unwrap().is_clean());

    // A live row whose bytes never reached the blob store: the forbidden ordering.
    let ghost = blob_id(b"ghost");
    env.mem().debug_put_object_row(ObjectRow {
        org: r.org,
        id: ghost,
        size: 5,
        state: ObjectState::Live,
        inline: None,
        created_at_ms: 0,
    });
    assert_eq!(kinds(&env, &r).await, [V::MissingBytes].into());
    env.mem().debug_remove_object_row(r.org, &ghost);

    // A non-canonical tree whose id is the hash of its (bad) bytes.
    let mut bad = Tree::empty().encode();
    bad.push(0);
    let bad_id = hash_id(ObjectKind::Tree, &bad);
    env.mem().debug_put_object_row(ObjectRow {
        org: r.org,
        id: bad_id,
        size: bad.len() as u64,
        state: ObjectState::Live,
        inline: Some(bad),
        created_at_ms: 0,
    });
    assert_eq!(kinds(&env, &r).await, [V::NonCanonical].into());
    env.mem().debug_remove_object_row(r.org, &bad_id);

    // Wrong size, bad inline placement, changeset rows in objects.
    let t = Tree::empty();
    env.mem().debug_put_object_row(ObjectRow {
        org: r.org,
        id: blob_id(b"x"),
        size: 9,
        state: ObjectState::Live,
        inline: None,
        created_at_ms: 0,
    });
    assert!(kinds(&env, &r).await.contains(&V::SizeMismatch));
    env.mem().debug_put_object_row(ObjectRow {
        org: r.org,
        id: blob_id(b"x"),
        size: 1,
        state: ObjectState::Live,
        inline: Some(b"x".to_vec()),
        created_at_ms: 0,
    });
    assert!(kinds(&env, &r).await.contains(&V::BadInline));
    env.mem().debug_put_object_row(ObjectRow {
        org: r.org,
        id: blob_id(b"x"),
        size: 1,
        state: ObjectState::Live,
        inline: None,
        created_at_ms: 0,
    });
    let cs_id = Changeset::empty_root().id();
    env.mem().debug_put_object_row(ObjectRow {
        org: r.org,
        id: cs_id,
        size: 1,
        state: ObjectState::Live,
        inline: None,
        created_at_ms: 0,
    });
    assert_eq!(kinds(&env, &r).await, [V::BadObjectRow].into());
    env.mem().debug_remove_object_row(r.org, &cs_id);

    // Purged without a redaction; a redaction whose row is live.
    env.mem().debug_put_object_row(ObjectRow {
        org: r.org,
        id: t.id(),
        size: t.encode().len() as u64,
        state: ObjectState::Purged,
        inline: None,
        created_at_ms: 0,
    });
    assert!(kinds(&env, &r).await.contains(&V::UnexplainedPurge));
    env.mem().debug_put_object_row(ObjectRow {
        org: r.org,
        id: t.id(),
        size: t.encode().len() as u64,
        state: ObjectState::Live,
        inline: Some(t.encode()),
        created_at_ms: 0,
    });
    env.meta
        .redact(bee_store::RedactionRow {
            org: r.org,
            object: blob_id(b"x"),
            principal: "admin".into(),
            reason: "r".into(),
            at_ms: 0,
        })
        .await
        .unwrap();
    // Bytes not deleted yet.
    assert_eq!(kinds(&env, &r).await, [V::RedactedBytesPresent].into());
    env.mem().debug_put_object_row(ObjectRow {
        org: r.org,
        id: blob_id(b"x"),
        size: 1,
        state: ObjectState::Live,
        inline: None,
        created_at_ms: 0,
    });
    assert!(kinds(&env, &r).await.contains(&V::RedactionIncomplete));
}

#[tokio::test]
async fn changeset_level_violations() {
    let env = TestEnv::new();
    let (r, ids) = history(&env).await;
    let (a, b) = (ids[0], ids[1]);
    let row_b = env
        .meta
        .get_changeset(r.repo, &b, ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();

    env.mem()
        .debug_set_changeset_bytes(r.repo, &b, row_b.bytes[..row_b.bytes.len() - 1].to_vec());
    let k = kinds(&env, &r).await;
    assert!(k.contains(&V::ChangesetIdMismatch) && k.contains(&V::NonCanonical));
    env.mem()
        .debug_set_changeset_bytes(r.repo, &b, row_b.bytes.clone());

    env.mem().debug_remove_parent_edges(r.repo, &b);
    assert_eq!(kinds(&env, &r).await, [V::ParentEdgeMismatch].into());

    let env = TestEnv::new();
    let (r, ids) = history(&env).await;
    let (a2, b2) = (ids[0], ids[1]);
    assert_eq!((a2, b2), (a, b));
    // Deleting a parent row leaves the child pointing at nothing (and the ref log too).
    env.mem().debug_remove_changeset_row(r.repo, &a);
    let k = kinds(&env, &r).await;
    assert!(
        k.contains(&V::MissingParent) && k.contains(&V::DanglingRef),
        "{k:?}"
    );

    let env = TestEnv::new();
    let (r, _) = history(&env).await;
    // A referenced blob loses its row: dangling reference.
    env.mem().debug_remove_object_row(r.org, &blob_id(b"b\n"));
    let k = kinds(&env, &r).await;
    assert!(k.contains(&V::DanglingReference), "{k:?}");

    let env = TestEnv::new();
    let (r, ids) = history(&env).await;
    env.mem().debug_remove_derived(r.repo, &ids[1]);
    assert_eq!(kinds(&env, &r).await, [V::EagerRowMissing].into());

    let env = TestEnv::new();
    let (r, _) = history(&env).await;
    env.mem()
        .debug_remove_object_paths(r.repo, &blob_id(b"a\n"));
    assert_eq!(kinds(&env, &r).await, [V::ObjectPathMissing].into());
}

#[tokio::test]
async fn invariant_d_violation() {
    let env = TestEnv::new();
    let (r, ids) = history(&env).await;
    let parent = env
        .meta
        .get_changeset(r.repo, &ids[1], ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    // Same root as the parent but a delta claiming a change: the store accepts it (it does
    // not read trees), fsck catches it.
    let blob = blob_id(b"a\n");
    let cs = Changeset::new(
        vec![ids[1]],
        *parent.changeset.root(),
        *parent.changeset.meta(),
        ChangeId::new([9; 16]),
        vec![DeltaEntry::put(
            RepoPath::new("zz").unwrap(),
            Mode::File,
            blob,
            2,
        )],
    )
    .unwrap();
    env.meta
        .insert_changeset(NewChangeset {
            repo: r.repo,
            changeset: cs.clone(),
            object_paths: vec![
                bee_store::ObjectPathRow {
                    object: *parent.changeset.root(),
                    path: None,
                },
                bee_store::ObjectPathRow {
                    object: blob,
                    path: Some(RepoPath::new("zz").unwrap()),
                },
            ],
            derived: vec![bee_store::DerivedRow {
                key: cs.id(),
                type_name: "graph".into(),
                version: 1,
                value: 3u64.to_be_bytes().to_vec(),
            }],
            at_ms: 0,
        })
        .await
        .unwrap();
    assert_eq!(kinds(&env, &r).await, [V::InvariantD].into());
    let _ = (Meta::empty_root(), TreeEntry::tree("d", Tree::empty().id()));
}

#[tokio::test]
async fn ref_log_and_landed_violations() {
    let main = ref_name("main");

    let env = TestEnv::new();
    let (r, _) = history(&env).await;
    env.mem().debug_remove_ref_log_entry(r.repo, &main, 1);
    assert!(kinds(&env, &r).await.contains(&V::RefLogGap));

    let env = TestEnv::new();
    let (r, ids) = history(&env).await;
    env.mem()
        .debug_set_ref_log_old(r.repo, &main, 2, Some(ids[1]));
    assert_eq!(kinds(&env, &r).await, [V::RefLogDiscontinuity].into());

    let env = TestEnv::new();
    let (r, ids) = history(&env).await;
    env.mem().debug_set_ref(RefRow {
        repo: r.repo,
        name: main.clone(),
        target: Some(ids[0]),
        version: 2,
    });
    assert_eq!(kinds(&env, &r).await, [V::RefLogTargetMismatch].into());

    let env = TestEnv::new();
    let (r, ids) = history(&env).await;
    let cid = ChangeId::new([5; 16]);
    env.mem().debug_put_landed(LandedRow {
        repo: r.repo,
        ref_name: main.clone(),
        change_id: cid,
        source: ids[1],
        result: ids[1],
        version: 2,
    });
    assert_eq!(kinds(&env, &r).await, [V::LandedOrphan].into());

    let env = TestEnv::new();
    let (r, ids) = history(&env).await;
    let c = env
        .commit(&r, Some(ids[1]), &[Change::put("c", "c")])
        .await
        .unwrap();
    let d = env
        .commit(&r, Some(c), &[Change::put("d", "d")])
        .await
        .unwrap();
    let land = |t, v, id: u8| bee_store::RefUpdate {
        repo: r.repo,
        name: main.clone(),
        expected: bee_store::Expected::Version(v),
        steps: vec![bee_store::RefStep {
            target: Some(t),
            landed: vec![bee_store::LandedEntry {
                change_id: ChangeId::new([id; 16]),
                source: t,
                result: t,
            }],
        }],
        principal: "p".into(),
        at_ms: 0,
        events: vec![],
    };
    env.meta.update_ref(land(c, 2, 7)).await.unwrap();
    env.meta.update_ref(land(d, 3, 8)).await.unwrap();
    assert!(env.fsck(r.org).await.unwrap().is_clean());
    env.mem()
        .debug_remove_landed(r.repo, &main, &ChangeId::new([8; 16]));
    assert_eq!(kinds(&env, &r).await, [V::LandedMissing].into());
    env.mem().debug_put_landed(LandedRow {
        repo: r.repo,
        ref_name: main.clone(),
        change_id: ChangeId::new([8; 16]),
        source: d,
        result: d,
        version: 4,
    });
    assert!(env.fsck(r.org).await.unwrap().is_clean());
    // The same ChangeId recorded by two log entries.
    let mut dup = env
        .meta
        .ref_log(r.repo, &main, 4, 1)
        .await
        .unwrap()
        .remove(0);
    dup.version = 5;
    dup.old = Some(d);
    env.mem().debug_put_ref_log_entry(dup);
    env.mem().debug_set_ref(RefRow {
        repo: r.repo,
        name: main.clone(),
        target: Some(d),
        version: 5,
    });
    assert!(kinds(&env, &r).await.contains(&V::LandedDuplicate));

    let env = TestEnv::new();
    let (r, _) = history(&env).await;
    env.mem().debug_put_event(EventRow {
        repo: r.repo,
        seq: 50,
        kind: "x".into(),
        payload: vec![],
        at_ms: 0,
    });
    assert_eq!(kinds(&env, &r).await, [V::EventBeyondCounter].into());
}
