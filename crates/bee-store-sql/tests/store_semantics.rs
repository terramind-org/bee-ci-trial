//! The store-semantics suite (`bee_testkit::tests::store_semantics`), re-run against SQLite
//! and Postgres: insert-or-ignore, one-transaction atomicity, CAS and the landed index,
//! events, object states, uploads, redaction, inline thresholds, names and indexes.

use crate::common;
use bee_core::{ChangeId, Mode, ObjectIdPrefix, ObjectKind, Reason, Subcode};
use bee_object::{Changeset, DeltaEntry, Meta, Tree, TreeEntry, blob_id};
use bee_store::faults::points;
use bee_store::{
    BlobStore, Expected, Fault, FaultRule, LandedEntry, MetaStore, NewChangeset, NewEvent,
    NewObject, ObjectState, PutOutcome, ReadFrom, RedactionRow, RefStep, RefUpdate, Table,
    UploadRecord, trees,
};
use bee_testkit::fixtures::{Change, ref_name};
use common::Open;

fn ev(kind: &str) -> NewEvent {
    NewEvent {
        kind: kind.into(),
        payload: kind.as_bytes().to_vec(),
        at_ms: 1,
    }
}

async fn insert_or_ignore_is_idempotent<S: Open>() {
    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "app").await.unwrap();
    let a = env
        .commit(&r, None, &[Change::put("a.txt", "1")])
        .await
        .unwrap();
    let row = env
        .meta
        .get_changeset(r.repo, &a, ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    let mut m = bee_object::MemoryTrees::new();
    m.insert(
        trees::read_tree(
            env.meta_dyn(),
            env.blobs.as_ref(),
            r.org,
            row.changeset.root(),
            ReadFrom::Primary,
        )
        .await
        .unwrap(),
    );
    let paths = trees::object_paths_for(&m, &row.changeset).unwrap();
    let before = env.counters.snapshot();
    let again = env
        .meta
        .insert_changeset(NewChangeset {
            repo: r.repo,
            changeset: row.changeset.clone(),
            object_paths: paths,
            derived: vec![],
            at_ms: env.now() + 5,
        })
        .await
        .unwrap();
    assert_eq!(again, PutOutcome::AlreadyPresent);
    let d = env.counters.snapshot().since(&before);
    assert_eq!(d.writes_of(Table::Changesets), 0);
    assert_eq!(d.writes_of(Table::ParentEdges), 0);
    assert_eq!(d.writes_of(Table::ObjectPaths), 0);
    assert_eq!(d.transactions, 1);
    let row2 = env
        .meta
        .get_changeset(r.repo, &a, ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row2.created_at_ms, row.created_at_ms);
    assert_eq!(row2.last_used_ms, env.now() + 5);
    assert_eq!(row2.bytes, row.bytes);
    assert_eq!(row2.changeset, row.changeset);

    let a2 = env
        .commit(&r, None, &[Change::put("a.txt", "1")])
        .await
        .unwrap();
    assert_eq!(a, a2);

    let blob = blob_id(b"1");
    let out = env
        .meta
        .put_objects(
            r.org,
            vec![NewObject::for_bytes(blob, b"1")],
            None,
            env.now() + 99,
        )
        .await;
    assert_eq!(out.unwrap(), vec![PutOutcome::AlreadyPresent]);
    let obj = env
        .meta
        .get_object(r.org, &blob, ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(obj.created_at_ms, env.now());
    // Duplicates inside one batch: the first is inserted, the rest already present.
    let fresh = blob_id(b"dup-in-batch");
    let row = NewObject::for_bytes(fresh, b"dup-in-batch");
    env.blobs
        .put(
            r.org,
            &fresh,
            b"dup-in-batch".to_vec(),
            bee_store::StorageClass::Hot,
        )
        .await
        .unwrap();
    let out = env
        .meta
        .put_objects(r.org, vec![row.clone(), row], None, 1)
        .await
        .unwrap();
    assert_eq!(out, vec![PutOutcome::Inserted, PutOutcome::AlreadyPresent]);
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}
both!(insert_or_ignore_is_idempotent);

async fn landed_violation_aborts_the_whole_transaction<S: Open>() {
    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    let a = env
        .commit_on(&r, &main, &[Change::put("a", "1")])
        .await
        .unwrap();
    let b = env
        .commit(&r, Some(a), &[Change::put("b", "2")])
        .await
        .unwrap();
    let c = env
        .commit(&r, Some(b), &[Change::put("c", "3")])
        .await
        .unwrap();
    let k1 = ChangeId::new([1; 16]);
    let k2 = ChangeId::new([2; 16]);
    let land = |steps: Vec<RefStep>, v: u64| RefUpdate {
        repo: r.repo,
        name: main.clone(),
        expected: Expected::Version(v),
        steps,
        principal: "p".into(),
        at_ms: 7,
        events: vec![ev("ref.updated")],
    };
    let entry = |change_id, id| LandedEntry {
        change_id,
        source: id,
        result: id,
    };
    let out = env
        .meta
        .update_ref(land(
            vec![RefStep {
                target: Some(b),
                landed: vec![entry(k1, b)],
            }],
            1,
        ))
        .await
        .unwrap();
    assert_eq!((out.old_version, out.new_version), (1, 2));
    assert_eq!(out.event_seqs, vec![1]);

    let before = env.counters.snapshot();
    let err = env
        .meta
        .update_ref(land(
            vec![RefStep {
                target: Some(c),
                landed: vec![entry(k2, c), entry(k1, c)],
            }],
            2,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.reason, Reason::AlreadyLanded);
    assert_eq!(
        err.payload.get("existing_source"),
        Some(&bee_core::PayloadValue::Str(b.to_text()))
    );
    assert_eq!(
        err.payload.get("change_id"),
        Some(&bee_core::PayloadValue::Str(k1.to_text()))
    );
    assert_eq!(env.counters.snapshot().since(&before).total_writes(), 0);
    let rf = env.meta.get_ref(r.repo, &main).await.unwrap().unwrap();
    assert_eq!((rf.target, rf.version), (Some(b), 2));
    assert_eq!(
        env.meta.ref_log(r.repo, &main, 1, 10).await.unwrap().len(),
        2
    );
    assert!(env.meta.get_landed(r.repo, &main, &[k2]).await.unwrap()[0].is_none());
    assert_eq!(env.meta.event_head(r.repo).await.unwrap().written, 1);

    let dup = env
        .meta
        .update_ref(land(
            vec![
                RefStep {
                    target: Some(c),
                    landed: vec![entry(k2, c)],
                },
                RefStep {
                    target: Some(c),
                    landed: vec![entry(k2, c)],
                },
            ],
            2,
        ))
        .await
        .unwrap_err();
    assert_eq!(dup.reason, Reason::AlreadyLanded);
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}
both!(landed_violation_aborts_the_whole_transaction);

async fn cas_versions_deletion_and_old_versions<S: Open>() {
    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "app").await.unwrap();
    let ws = ref_name("ws/agent/x");
    let a = env
        .commit(&r, None, &[Change::put("a", "1")])
        .await
        .unwrap();
    let b = env
        .commit(&r, Some(a), &[Change::put("a", "2")])
        .await
        .unwrap();
    let set = |exp, t| RefUpdate::set(r.repo, ws.clone(), exp, t, "p", 1);
    let e = env
        .meta
        .update_ref(set(Expected::Version(0), Some(a)))
        .await
        .unwrap_err();
    assert_eq!(e.reason, Reason::VersionMismatch);
    assert_eq!(
        env.meta
            .update_ref(set(Expected::Absent, Some(a)))
            .await
            .unwrap()
            .new_version,
        1
    );
    let e = env
        .meta
        .update_ref(set(Expected::Absent, Some(b)))
        .await
        .unwrap_err();
    assert_eq!(
        e.payload.get("current_version"),
        Some(&bee_core::PayloadValue::Int(1))
    );
    env.meta
        .update_ref(set(Expected::Version(1), Some(b)))
        .await
        .unwrap();
    env.meta
        .update_ref(set(Expected::Version(2), None))
        .await
        .unwrap();
    let rf = env.meta.get_ref(r.repo, &ws).await.unwrap().unwrap();
    assert_eq!((rf.target, rf.version), (None, 3));
    assert_eq!(
        env.meta
            .update_ref(set(Expected::Absent, Some(a)))
            .await
            .unwrap()
            .new_version,
        4
    );
    let log = env.meta.ref_log(r.repo, &ws, 1, 100).await.unwrap();
    let targets: Vec<_> = log.iter().map(|e| (e.version, e.new)).collect();
    assert_eq!(
        targets,
        vec![(1, Some(a)), (2, Some(b)), (3, None), (4, Some(a))]
    );
    assert_eq!(
        env.meta.ref_log(r.repo, &ws, 2, 1).await.unwrap()[0].new,
        Some(b)
    );
    let bogus = Changeset::empty_root().id();
    let e = env
        .meta
        .update_ref(set(Expected::Version(4), Some(bogus)))
        .await
        .unwrap_err();
    assert_eq!(e.reason, Reason::MissingObject);
    let e = env
        .meta
        .update_ref(RefUpdate {
            steps: vec![],
            ..set(Expected::Version(4), Some(a))
        })
        .await
        .unwrap_err();
    assert!(e.is_invalid(Subcode::InvalidField));
    // Multi-step update (coalescer): K versions and K log entries in one transaction.
    let before = env.counters.snapshot();
    let out = env
        .meta
        .update_ref(RefUpdate {
            repo: r.repo,
            name: ws.clone(),
            expected: Expected::Version(4),
            steps: vec![
                RefStep {
                    target: Some(b),
                    landed: vec![],
                },
                RefStep {
                    target: Some(a),
                    landed: vec![],
                },
                RefStep {
                    target: Some(b),
                    landed: vec![],
                },
            ],
            principal: "owner".into(),
            at_ms: 2,
            events: vec![ev("a"), ev("b")],
        })
        .await
        .unwrap();
    assert_eq!((out.old_version, out.new_version), (4, 7));
    assert_eq!(out.event_seqs, vec![1, 2]);
    let d = env.counters.snapshot().since(&before);
    assert_eq!(d.transactions, 1);
    assert_eq!(d.writes_of(Table::RefLog), 3);
    assert_eq!(d.writes_of(Table::EventCounters), 1);
    let log = env.meta.ref_log(r.repo, &ws, 5, 100).await.unwrap();
    let chain: Vec<_> = log.iter().map(|e| (e.version, e.old, e.new)).collect();
    assert_eq!(
        chain,
        vec![
            (5, Some(a), Some(b)),
            (6, Some(b), Some(a)),
            (7, Some(a), Some(b))
        ]
    );
    assert_eq!(env.meta.list_refs(r.repo, "ws/").await.unwrap().len(), 1);
    assert!(env.meta.list_refs(r.repo, "ws/b").await.unwrap().is_empty());
    assert_eq!(env.meta.list_refs(r.repo, "").await.unwrap().len(), 1);
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}
both!(cas_versions_deletion_and_old_versions);

async fn changeset_insert_requires_referenced_rows<S: Open>() {
    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "app").await.unwrap();
    let blob = blob_id(b"never uploaded");
    let path = bee_core::RepoPath::new("f").unwrap();
    let tree = Tree::new(vec![TreeEntry::blob("f", Mode::File, blob, 14)]).unwrap();
    env.writer
        .upload(r.org, vec![(tree.id(), tree.encode())], None, 1)
        .await
        .unwrap();
    let meta = Meta::empty_root();
    env.writer
        .upload(r.org, vec![(meta.id(), meta.encode())], None, 1)
        .await
        .unwrap();
    let cs = Changeset::new(
        vec![],
        tree.id(),
        meta.id(),
        ChangeId::new([3; 16]),
        vec![DeltaEntry::put(path, Mode::File, blob, 14)],
    )
    .unwrap();
    let before = env.counters.snapshot();
    let err = env
        .meta
        .insert_changeset(NewChangeset {
            repo: r.repo,
            changeset: cs.clone(),
            object_paths: vec![],
            derived: vec![],
            at_ms: 1,
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason, Reason::MissingObject);
    assert_eq!(
        err.payload.get("object"),
        Some(&bee_core::PayloadValue::Str(blob.to_text()))
    );
    assert_eq!(env.counters.snapshot().since(&before).total_writes(), 0);
    let child = Changeset::new(
        vec![cs.id()],
        tree.id(),
        meta.id(),
        ChangeId::new([4; 16]),
        vec![],
    )
    .unwrap();
    let err = env
        .meta
        .insert_changeset(NewChangeset {
            repo: r.repo,
            changeset: child,
            object_paths: vec![],
            derived: vec![],
            at_ms: 1,
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason, Reason::MissingObject);
    let err = env
        .meta
        .put_objects(
            r.org,
            vec![NewObject {
                id: cs.id(),
                size: 1,
                inline: None,
            }],
            None,
            1,
        )
        .await
        .unwrap_err();
    assert!(err.is_invalid(Subcode::WrongKind));
    let err = env
        .writer
        .upload(r.org, vec![(cs.id(), cs.encode())], None, 1)
        .await
        .unwrap_err();
    assert!(err.is_invalid(Subcode::WrongKind));
    let other = bee_store::RepoId(bee_core::Ulid::from_u128(77));
    let err = env
        .meta
        .get_ref(other, &ref_name("main"))
        .await
        .unwrap_err();
    assert_eq!(err.reason, Reason::NotFound);
    let other_org = bee_store::OrgId(bee_core::Ulid::from_u128(78));
    let err = env
        .meta
        .put_objects(other_org, vec![], None, 1)
        .await
        .unwrap_err();
    assert_eq!(err.reason, Reason::NotFound);
    assert!(env.meta.get_org(other_org).await.unwrap().is_none());
    // A quarantined object cannot be newly referenced.
    let tree2 = Tree::empty();
    env.writer
        .upload(r.org, vec![(tree2.id(), tree2.encode())], None, 1)
        .await
        .unwrap();
    env.meta
        .set_object_state(r.org, &tree2.id(), ObjectState::Quarantined)
        .await
        .unwrap();
    let cs3 = Changeset::new(
        vec![],
        tree2.id(),
        meta.id(),
        ChangeId::new([5; 16]),
        vec![],
    )
    .unwrap();
    let err = env
        .meta
        .insert_changeset(NewChangeset {
            repo: r.repo,
            changeset: cs3,
            object_paths: vec![],
            derived: vec![],
            at_ms: 1,
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason, Reason::MissingObject);
    assert_eq!(
        env.meta
            .set_object_state(r.org, &blob, ObjectState::Live)
            .await
            .unwrap_err()
            .reason,
        Reason::NotFound
    );
}
both!(changeset_insert_requires_referenced_rows);

async fn event_sequences_are_block_allocated<S: Open>() {
    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "app").await.unwrap();
    let before = env.counters.snapshot();
    let seqs = env
        .meta
        .append_events(r.repo, vec![ev("a"), ev("b"), ev("c")])
        .await
        .unwrap();
    assert_eq!(seqs, vec![1, 2, 3]);
    assert_eq!(
        env.counters
            .snapshot()
            .since(&before)
            .writes_of(Table::EventCounters),
        1
    );
    let block = env.meta.allocate_event_seqs(r.repo, 10).await.unwrap();
    assert_eq!(block, 4..14);
    env.meta
        .insert_events(r.repo, vec![(5, ev("e5")), (4, ev("e4"))])
        .await
        .unwrap();
    env.meta
        .insert_events(r.repo, vec![(4, ev("other"))])
        .await
        .unwrap();
    let e = env
        .meta
        .insert_events(r.repo, vec![(99, ev("x"))])
        .await
        .unwrap_err();
    assert!(e.is_invalid(Subcode::InvalidField));
    let e = env
        .meta
        .insert_events(r.repo, vec![(0, ev("x"))])
        .await
        .unwrap_err();
    assert!(e.is_invalid(Subcode::InvalidField));
    let all = env
        .meta
        .read_events(r.repo, 0, 100, ReadFrom::Primary)
        .await
        .unwrap();
    let kinds: Vec<_> = all.iter().map(|e| (e.seq, e.kind.as_str())).collect();
    assert_eq!(
        kinds,
        vec![(1, "a"), (2, "b"), (3, "c"), (4, "e4"), (5, "e5")]
    );
    assert_eq!(all[0].payload, b"a".to_vec());
    let head = env.meta.event_head(r.repo).await.unwrap();
    assert_eq!((head.allocated, head.written), (13, 5));
    assert_eq!(
        env.meta
            .read_events(r.repo, 4, 1, ReadFrom::Primary)
            .await
            .unwrap()[0]
            .seq,
        4
    );
    assert_eq!(
        env.meta.allocate_event_seqs(r.repo, 0).await.unwrap(),
        14..14
    );
}
both!(event_sequences_are_block_allocated);

async fn object_states_uploads_and_redaction<S: Open>() {
    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    let secret = b"password=hunter2".to_vec();
    let a = env
        .commit_on(&r, &main, &[Change::put("cfg", secret.clone())])
        .await
        .unwrap();
    let id = blob_id(&secret);
    let ups = env.meta.get_uploads(r.org, &id).await.unwrap();
    assert_eq!(ups.len(), 1);
    assert_eq!(
        (ups[0].repo, ups[0].principal.as_str()),
        (Some(r.repo), "agent:test")
    );
    assert_eq!(
        env.meta.list_uploads(r.org, env.now()).await.unwrap().len(),
        1
    );
    assert!(
        env.meta
            .list_uploads(r.org, env.now() + 1)
            .await
            .unwrap()
            .is_empty()
    );
    // A later upload by the same principal without a repo keeps the repo, bumps the time.
    env.meta
        .put_objects(
            r.org,
            vec![NewObject::for_bytes(id, &secret)],
            Some(UploadRecord {
                repo: None,
                principal: "agent:test".into(),
            }),
            env.now() + 50,
        )
        .await
        .unwrap();
    let ups = env.meta.get_uploads(r.org, &id).await.unwrap();
    assert_eq!((ups[0].repo, ups[0].at_ms), (Some(r.repo), env.now() + 50));

    env.meta
        .set_object_state(r.org, &id, ObjectState::Quarantined)
        .await
        .unwrap();
    let e = trees::read_object_bytes(
        env.meta_dyn(),
        env.blobs.as_ref(),
        r.org,
        &id,
        ReadFrom::Primary,
    )
    .await
    .unwrap_err();
    assert_eq!(e.reason, Reason::MissingObject);
    env.commit(&r, Some(a), &[Change::put("copy", secret.clone())])
        .await
        .unwrap();
    let row = env
        .meta
        .get_object(r.org, &id, ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ObjectState::Live);
    assert!(
        env.meta
            .set_object_state(r.org, &id, ObjectState::Purged)
            .await
            .unwrap_err()
            .is_invalid(Subcode::InvalidField)
    );

    let epoch0 = env.meta.redaction_epoch(r.repo).await.unwrap();
    let red = env
        .writer
        .redact(RedactionRow {
            org: r.org,
            object: id,
            principal: "admin".into(),
            reason: "leaked secret".into(),
            at_ms: env.now(),
        })
        .await
        .unwrap();
    assert!(red.newly_redacted);
    assert_eq!(red.epochs, vec![(r.repo, epoch0 + 1)]);
    assert_eq!(env.meta.redaction_epoch(r.repo).await.unwrap(), epoch0 + 1);
    let row = env
        .meta
        .get_object(r.org, &id, ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ObjectState::Purged);
    assert!(!env.blobs.has(r.org, &[id]).await.unwrap()[0]);
    let e = trees::read_object_bytes(
        env.meta_dyn(),
        env.blobs.as_ref(),
        r.org,
        &id,
        ReadFrom::Primary,
    )
    .await
    .unwrap_err();
    assert_eq!(e.reason, Reason::Redacted);
    let e = env
        .writer
        .upload(r.org, vec![(id, secret.clone())], None, 1)
        .await
        .unwrap_err();
    assert_eq!(e.reason, Reason::Redacted);
    let e = env
        .meta
        .put_objects(r.org, vec![NewObject::for_bytes(id, &secret)], None, 1)
        .await
        .unwrap_err();
    assert_eq!(e.reason, Reason::Redacted);
    assert!(
        env.meta
            .set_object_state(r.org, &id, ObjectState::Live)
            .await
            .unwrap_err()
            .is_invalid(Subcode::InvalidField)
    );
    let again = env
        .meta
        .redact(RedactionRow {
            org: r.org,
            object: id,
            principal: "admin".into(),
            reason: "again".into(),
            at_ms: 1,
        })
        .await
        .unwrap();
    assert!(!again.newly_redacted);
    assert_eq!(again.epochs, vec![(r.repo, epoch0 + 1)]);
    let list = env.meta.list_redactions(r.org).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].reason, "leaked secret");
    assert_eq!(
        env.meta
            .get_redactions(r.org, &[blob_id(b"zz"), id])
            .await
            .unwrap()
            .len(),
        1
    );
    let e = env
        .meta
        .redact(RedactionRow {
            org: r.org,
            object: Tree::empty().id(),
            principal: "admin".into(),
            reason: "no".into(),
            at_ms: 1,
        })
        .await
        .unwrap_err();
    assert!(e.is_invalid(Subcode::WrongKind));
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}
both!(object_states_uploads_and_redaction);

async fn redaction_crash_between_row_and_bytes_is_finished_by_a_retry<S: Open>() {
    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    env.commit_on(&r, &main, &[Change::put("k", "secret")])
        .await
        .unwrap();
    let id = blob_id(b"secret");
    env.faults.add(FaultRule::nth(
        points::REDACT_BYTES,
        0,
        Fault::unavailable(),
    ));
    let row = RedactionRow {
        org: r.org,
        object: id,
        principal: "admin".into(),
        reason: "x".into(),
        at_ms: 1,
    };
    assert!(env.writer.redact(row.clone()).await.is_err());
    let report = env.fsck(r.org).await.unwrap();
    assert_eq!(
        report.kinds(),
        [bee_store::ViolationKind::RedactedBytesPresent].into()
    );
    env.writer.redact(row).await.unwrap();
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}
both!(redaction_crash_between_row_and_bytes_is_finished_by_a_retry);

async fn inline_threshold<S: Open>() {
    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "app").await.unwrap();
    let base = Meta::new("a", bee_object::Timestamp::new(0, 0), "c", "");
    let mut at = base.clone();
    at.message = "x".repeat(bee_store::INLINE_MAX_BYTES - base.encode().len());
    while at.encode().len() > bee_store::INLINE_MAX_BYTES {
        at.message.pop();
    }
    while at.encode().len() < bee_store::INLINE_MAX_BYTES {
        at.message.push('x');
    }
    assert_eq!(at.encode().len(), bee_store::INLINE_MAX_BYTES);
    let mut over = at.clone();
    over.message.push('x');
    assert_eq!(over.encode().len(), bee_store::INLINE_MAX_BYTES + 1);
    env.writer
        .upload(
            r.org,
            vec![(at.id(), at.encode()), (over.id(), over.encode())],
            Some(UploadRecord {
                repo: None,
                principal: "p".into(),
            }),
            1,
        )
        .await
        .unwrap();
    let rows = env
        .meta
        .get_objects(
            r.org,
            &[at.id(), over.id(), blob_id(b"none")],
            ReadFrom::Primary,
        )
        .await
        .unwrap();
    assert_eq!(rows[0].as_ref().unwrap().inline, Some(at.encode()));
    assert!(rows[1].as_ref().unwrap().inline.is_none());
    assert!(rows[2].is_none());
    assert!(!env.blobs.has(r.org, &[at.id()]).await.unwrap()[0]);
    assert!(env.blobs.has(r.org, &[over.id()]).await.unwrap()[0]);
    let bad = env
        .meta
        .put_objects(
            r.org,
            vec![NewObject {
                id: over.id(),
                size: over.encode().len() as u64,
                inline: Some(over.encode()),
            }],
            None,
            1,
        )
        .await
        .unwrap_err();
    assert!(bad.is_invalid(Subcode::InvalidField));
    let bad = env
        .meta
        .put_objects(
            r.org,
            vec![NewObject {
                id: at.id(),
                size: at.encode().len() as u64,
                inline: None,
            }],
            None,
            1,
        )
        .await
        .unwrap_err();
    assert!(bad.is_invalid(Subcode::InvalidField));
    let mut wrong = at.encode();
    wrong[0] ^= 1;
    let bad = env
        .meta
        .put_objects(
            r.org,
            vec![NewObject {
                id: at.id(),
                size: wrong.len() as u64,
                inline: Some(wrong),
            }],
            None,
            1,
        )
        .await
        .unwrap_err();
    assert_eq!(bad.reason, Reason::Integrity);
    let bytes = trees::read_object_bytes(
        env.meta_dyn(),
        env.blobs.as_ref(),
        r.org,
        &at.id(),
        ReadFrom::Primary,
    )
    .await
    .unwrap();
    assert_eq!(bytes, at.encode());
}
both!(inline_threshold);

async fn names_prefixes_and_indexes<S: Open>() {
    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "app").await.unwrap();
    let r2 = env.create_repo("acme", "lib").await.unwrap();
    assert_eq!(r.org, r2.org);
    assert!(env.create_repo("acme", "app").await.is_err());
    let org = env.meta.org_by_name("acme").await.unwrap().unwrap();
    assert_eq!(env.meta.get_org(org.id).await.unwrap(), Some(org.clone()));
    let repos = env.meta.list_repos(org.id).await.unwrap();
    assert_eq!(repos.len(), 2);
    assert!(repos[0].id < repos[1].id);
    assert_eq!(
        env.meta
            .repo_by_name(org.id, "lib")
            .await
            .unwrap()
            .map(|x| x.id),
        Some(r2.repo)
    );
    let repo_row = env.meta.get_repo(r.repo).await.unwrap().unwrap();
    assert_eq!(
        env.meta.create_repo(repo_row.clone()).await.unwrap(),
        PutOutcome::AlreadyPresent
    );
    let mut renamed = repo_row.clone();
    renamed.name = "other".into();
    assert_eq!(
        env.meta.create_repo(renamed).await.unwrap_err().reason,
        Reason::AlreadyExists
    );
    let mut fresh_id = repo_row;
    fresh_id.id = bee_store::RepoId(bee_core::Ulid::from_u128(5));
    assert_eq!(
        env.meta
            .create_repo(fresh_id.clone())
            .await
            .unwrap_err()
            .reason,
        Reason::AlreadyExists
    );
    fresh_id.org = bee_store::OrgId(bee_core::Ulid::from_u128(6));
    assert_eq!(
        env.meta.create_repo(fresh_id).await.unwrap_err().reason,
        Reason::NotFound
    );
    assert_eq!(
        env.meta.create_org(org.clone()).await.unwrap(),
        PutOutcome::AlreadyPresent
    );
    let mut other = org.clone();
    other.created_at_ms += 1;
    assert_eq!(
        env.meta.create_org(other).await.unwrap_err().reason,
        Reason::AlreadyExists
    );
    let mut taken = org.clone();
    taken.id = bee_store::OrgId(bee_core::Ulid::from_u128(9));
    assert_eq!(
        env.meta.create_org(taken).await.unwrap_err().reason,
        Reason::AlreadyExists
    );

    let a = env
        .commit(&r, None, &[Change::put("a", "1")])
        .await
        .unwrap();
    let b = env
        .commit(&r, Some(a), &[Change::put("b", "2")])
        .await
        .unwrap();
    assert_eq!(env.meta.parents(r.repo, &b).await.unwrap(), vec![a]);
    assert_eq!(env.meta.children(r.repo, &a).await.unwrap(), vec![b]);
    let row = env
        .meta
        .get_changeset(r.repo, &b, ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        env.meta
            .changesets_by_change_id(r.repo, row.changeset.change_id())
            .await
            .unwrap(),
        vec![b]
    );
    for n in [10, 11, 13, 64] {
        let prefix =
            ObjectIdPrefix::parse(&b.to_text()[..b.to_text().find('_').unwrap() + 1 + n]).unwrap();
        assert_eq!(
            env.meta
                .changesets_with_prefix(r.repo, &prefix, 10)
                .await
                .unwrap(),
            vec![b],
            "{n}"
        );
    }
    let blob = blob_id(b"1");
    assert_eq!(
        env.meta
            .object_paths(r.repo, &blob, ReadFrom::Primary)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        env.meta
            .object_paths(r2.repo, &blob, ReadFrom::Primary)
            .await
            .unwrap()
            .is_empty()
    );
    let root = *row.changeset.root();
    assert_eq!(
        env.meta
            .object_paths(r.repo, &root, ReadFrom::Primary)
            .await
            .unwrap(),
        vec![None]
    );
    let page1 = env.meta.list_changesets(r.repo, None, 1).await.unwrap();
    let page2 = env
        .meta
        .list_changesets(r.repo, Some(page1[0].id), 10)
        .await
        .unwrap();
    assert_eq!(page1.len() + page2.len(), 2);
    assert!(page1[0].id < page2[0].id);
    let objs = env.meta.list_objects(r.org, None, 1000).await.unwrap();
    assert!(objs.windows(2).all(|w| w[0].id < w[1].id));
    let rest = env
        .meta
        .list_objects(r.org, Some(objs[0].id), 1000)
        .await
        .unwrap();
    assert_eq!(rest.len(), objs.len() - 1);
    env.meta
        .touch_changesets(r.repo, &[a], env.now() + 1000)
        .await
        .unwrap();
    let ra = env
        .meta
        .get_changeset(r.repo, &a, ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ra.last_used_ms, env.now() + 1000);
    env.meta
        .put_derived(
            r.repo,
            vec![bee_store::DerivedRow {
                key: a,
                type_name: "blame".into(),
                version: 2,
                value: vec![9],
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        env.meta
            .get_derived(r.repo, &a, "blame", 2, ReadFrom::Primary)
            .await
            .unwrap(),
        Some(vec![9])
    );
    assert_eq!(
        env.meta
            .get_derived(r.repo, &a, "blame", 3, ReadFrom::Primary)
            .await
            .unwrap(),
        None
    );
    assert_eq!(a.kind(), ObjectKind::Changeset);
}
both!(names_prefixes_and_indexes);

async fn delete_derived_type_removes_one_type_in_one_repo<S: Open>() {
    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "app").await.unwrap();
    let r2 = env.create_repo("acme", "lib").await.unwrap();
    let a = env
        .commit(&r, None, &[Change::put("a.txt", "1")])
        .await
        .unwrap();
    let b = env
        .commit(&r, Some(a), &[Change::put("b.txt", "2")])
        .await
        .unwrap();
    let row = |key, ty: &str, version| bee_store::DerivedRow {
        key,
        type_name: ty.into(),
        version,
        value: vec![1],
    };
    env.meta
        .put_derived(
            r.repo,
            vec![
                row(a, "blame", 1),
                row(a, "blame", 2),
                row(b, "blame", 2),
                row(a, "language_stats", 1),
            ],
        )
        .await
        .unwrap();
    env.meta
        .put_derived(r2.repo, vec![row(a, "blame", 1)])
        .await
        .unwrap();
    // Every version and key of the type in that repo, nothing else.
    assert_eq!(
        env.meta.delete_derived_type(r.repo, "blame").await.unwrap(),
        3
    );
    for (key, version) in [(a, 1), (a, 2), (b, 2)] {
        assert_eq!(
            env.meta
                .get_derived(r.repo, &key, "blame", version, ReadFrom::Primary)
                .await
                .unwrap(),
            None
        );
    }
    assert!(
        env.meta
            .get_derived(r.repo, &a, "language_stats", 1, ReadFrom::Primary)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        env.meta
            .get_derived(r2.repo, &a, "blame", 1, ReadFrom::Primary)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        env.meta.delete_derived_type(r.repo, "blame").await.unwrap(),
        0,
        "idempotent"
    );
}
both!(delete_derived_type_removes_one_type_in_one_repo);
