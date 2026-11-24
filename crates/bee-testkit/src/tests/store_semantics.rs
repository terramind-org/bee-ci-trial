//! Semantics of the store traits on the in-memory implementation: insert-or-ignore,
//! one-transaction atomicity, CAS and the landed index, stale replicas, events, object
//! states, uploads, redaction and storage classes.

use bee_core::{ChangeId, Mode, ObjectIdPrefix, ObjectKind, Reason, Subcode};
use bee_object::{Changeset, DeltaEntry, Meta, Tree, TreeEntry, blob_id};
use bee_store::faults::points;
use bee_store::{
    BlobStore, Expected, Fault, FaultRule, LandedEntry, MetaStore, MinVersion, NewChangeset,
    NewEvent, NewObject, ObjectState, PutOutcome, ReadFrom, RedactionRow, RefStep, RefUpdate,
    StorageClass, Table, UploadRecord, consistency, trees,
};

use crate::fixtures::{Change, TestEnv, ref_name};

fn ev(kind: &str) -> NewEvent {
    NewEvent {
        kind: kind.into(),
        payload: kind.as_bytes().to_vec(),
        at_ms: 1,
    }
}

#[tokio::test]
async fn insert_or_ignore_is_idempotent() {
    let env = TestEnv::new();
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
    // Re-inserting the identical changeset writes no new rows and reports AlreadyPresent.
    let paths = trees::object_paths_for(
        &{
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
            m
        },
        &row.changeset,
    )
    .unwrap();
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

    // Committing the same thing again through the whole write path yields the same id.
    let a2 = env
        .commit(&r, None, &[Change::put("a.txt", "1")])
        .await
        .unwrap();
    assert_eq!(a, a2);

    // Objects: second put reports AlreadyPresent and keeps the original creation time.
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
    // b"1" is a blob, which is never inline: the row needs blob-store bytes and says so.
    assert_eq!(out.unwrap(), vec![PutOutcome::AlreadyPresent]);
    let obj = env
        .meta
        .get_object(r.org, &blob, ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(obj.created_at_ms, env.now());
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}

#[tokio::test]
async fn landed_violation_aborts_the_whole_transaction() {
    let env = TestEnv::new();
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
    let out = env
        .meta
        .update_ref(land(
            vec![RefStep {
                target: Some(b),
                landed: vec![LandedEntry {
                    change_id: k1,
                    source: b,
                    result: b,
                }],
            }],
            1,
        ))
        .await
        .unwrap();
    assert_eq!((out.old_version, out.new_version), (1, 2));
    assert_eq!(out.event_seqs, vec![1]);

    // k2 is new but k1 is already landed: nothing at all is written.
    let before = env.counters.snapshot();
    let err = env
        .meta
        .update_ref(land(
            vec![RefStep {
                target: Some(c),
                landed: vec![
                    LandedEntry {
                        change_id: k2,
                        source: c,
                        result: c,
                    },
                    LandedEntry {
                        change_id: k1,
                        source: c,
                        result: c,
                    },
                ],
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
    let d = env.counters.snapshot().since(&before);
    assert_eq!(d.total_writes(), 0);
    let rf = env.meta.get_ref(r.repo, &main).await.unwrap().unwrap();
    assert_eq!((rf.target, rf.version), (Some(b), 2));
    assert_eq!(
        env.meta.ref_log(r.repo, &main, 1, 10).await.unwrap().len(),
        2
    );
    assert!(env.meta.get_landed(r.repo, &main, &[k2]).await.unwrap()[0].is_none());
    assert_eq!(env.meta.event_head(r.repo).await.unwrap().written, 1);

    // The same ChangeId twice inside one update is also a violation.
    let dup = env
        .meta
        .update_ref(land(
            vec![
                RefStep {
                    target: Some(c),
                    landed: vec![LandedEntry {
                        change_id: k2,
                        source: c,
                        result: c,
                    }],
                },
                RefStep {
                    target: Some(c),
                    landed: vec![LandedEntry {
                        change_id: k2,
                        source: c,
                        result: c,
                    }],
                },
            ],
            2,
        ))
        .await
        .unwrap_err();
    assert_eq!(dup.reason, Reason::AlreadyLanded);
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}

#[tokio::test]
async fn cas_versions_deletion_and_old_versions() {
    let env = TestEnv::new();
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
    // Delete keeps numbering; recreate continues it.
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
    // name@version: the ref log answers for every old version.
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
    // A target that is not a changeset of the repo is MISSING_OBJECT.
    let bogus = Changeset::empty_root().id();
    let e = env
        .meta
        .update_ref(set(Expected::Version(4), Some(bogus)))
        .await
        .unwrap_err();
    assert_eq!(e.reason, Reason::MissingObject);
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
            events: vec![],
        })
        .await
        .unwrap();
    assert_eq!((out.old_version, out.new_version), (4, 7));
    assert_eq!(env.counters.snapshot().since(&before).transactions, 1);
    assert_eq!(env.meta.list_refs(r.repo, "ws/").await.unwrap().len(), 1);
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}

#[tokio::test]
async fn changeset_insert_requires_referenced_rows() {
    let env = TestEnv::new();
    let r = env.create_repo("acme", "app").await.unwrap();
    // A changeset whose blob was never uploaded.
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
    assert_eq!(env.counters.snapshot().since(&before).total_writes(), 0);
    // A missing parent is MISSING_OBJECT too.
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
    // Changesets live only in their row: put_objects refuses changeset ids.
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
    // Unknown repo is NOT_FOUND.
    let other = bee_store::RepoId(bee_core::Ulid::from_u128(77));
    let err = env
        .meta
        .get_ref(other, &ref_name("main"))
        .await
        .unwrap_err();
    assert_eq!(err.reason, Reason::NotFound);
}

#[tokio::test]
async fn stale_replica_reads_fall_back_to_the_primary() {
    let env = TestEnv::new();
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    env.mem().set_replica_lag(1000);
    let a = env
        .commit_on(&r, &main, &[Change::put("a", "1")])
        .await
        .unwrap();
    // The replica is far behind: a direct replica read misses...
    assert!(
        env.meta
            .get_changeset(r.repo, &a, ReadFrom::Replica(None))
            .await
            .unwrap()
            .is_none()
    );
    // ...and the consistency helper retries on the primary before answering.
    let before = env.counters.snapshot();
    let row = consistency::changeset(env.meta_dyn(), r.repo, &a, None)
        .await
        .unwrap();
    assert_eq!(row.id, a);
    assert_eq!(env.counters.snapshot().since(&before).replica_reads, 1);
    // A truly absent changeset is NOT_FOUND only after the primary also misses.
    let absent = Changeset::empty_root().id();
    let e = consistency::changeset(env.meta_dyn(), r.repo, &absent, None)
        .await
        .unwrap_err();
    assert_eq!(e.reason, Reason::NotFound);
    let e = consistency::object(env.meta_dyn(), r.org, &blob_id(b"zz"), None)
        .await
        .unwrap_err();
    assert_eq!(e.reason, Reason::MissingObject);
    // read-your-writes: a min_version the replica has not reached is forwarded.
    let min = MinVersion::Ref {
        repo: r.repo,
        name: main.clone(),
        version: 1,
    };
    let before = env.counters.snapshot();
    let row = env
        .meta
        .get_changeset(r.repo, &a, ReadFrom::Replica(Some(min.clone())))
        .await
        .unwrap();
    assert_eq!(row.map(|r| r.id), Some(a));
    assert_eq!(env.counters.snapshot().since(&before).replica_forwards, 1);
    // A replica that has caught up serves the read itself.
    env.mem().set_replica_lag(0);
    let before = env.counters.snapshot();
    let row = env
        .meta
        .get_changeset(r.repo, &a, ReadFrom::Replica(Some(min)))
        .await
        .unwrap();
    assert!(row.is_some());
    let d = env.counters.snapshot().since(&before);
    assert_eq!((d.replica_reads, d.replica_forwards), (1, 0));
    // Lag of exactly one commit: the latest write is invisible, earlier ones visible.
    env.mem().set_replica_lag(1);
    let seqs = env.meta.append_events(r.repo, vec![ev("x")]).await.unwrap();
    let on_replica = env
        .meta
        .read_events(r.repo, 0, 10, ReadFrom::Replica(None))
        .await
        .unwrap();
    assert!(on_replica.iter().all(|e| e.seq < seqs[0]));
    let fwd = env
        .meta
        .read_events(
            r.repo,
            0,
            10,
            ReadFrom::Replica(Some(MinVersion::Event {
                repo: r.repo,
                seq: seqs[0],
            })),
        )
        .await
        .unwrap();
    assert_eq!(fwd.last().map(|e| e.seq), Some(seqs[0]));
    let got = consistency::derived(env.meta_dyn(), r.repo, &a, "graph", 1, None)
        .await
        .unwrap();
    assert_eq!(got, Some(1u64.to_be_bytes().to_vec()));
}

#[tokio::test]
async fn event_sequences_are_block_allocated() {
    let env = TestEnv::new();
    let r = env.create_repo("acme", "app").await.unwrap();
    let before = env.counters.snapshot();
    let seqs = env
        .meta
        .append_events(r.repo, vec![ev("a"), ev("b"), ev("c")])
        .await
        .unwrap();
    assert_eq!(seqs, vec![1, 2, 3]);
    // One counter update for the whole block.
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
    // insert-or-ignore
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
}

#[tokio::test]
async fn object_states_uploads_and_redaction() {
    let env = TestEnv::new();
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    let secret = b"password=hunter2".to_vec();
    let a = env
        .commit_on(&r, &main, &[Change::put("cfg", secret.clone())])
        .await
        .unwrap();
    let id = blob_id(&secret);
    // Upload records name the uploader and the repo.
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

    // Quarantine: not served, changesets cannot newly reference it, re-upload repairs it.
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
    let e = env
        .commit(&r, Some(a), &[Change::put("copy", secret.clone())])
        .await;
    // The fixture re-uploads the blob first, which makes it live again.
    assert!(e.is_ok());
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

    // Redaction: row purged first, epoch bumped, bytes deleted, id blocked forever.
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
    // Idempotent.
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
    assert_eq!(env.meta.list_redactions(r.org).await.unwrap().len(), 1);
    // Trees cannot be redacted.
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
    // Changesets referencing the redacted blob are not dangling.
    assert!(env.fsck(r.org).await.unwrap().is_clean());
}

#[tokio::test]
async fn redaction_crash_between_row_and_bytes_is_finished_by_a_retry() {
    let env = TestEnv::new();
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

#[tokio::test]
async fn inline_threshold_and_blob_store_classes() {
    let env = TestEnv::new();
    let r = env.create_repo("acme", "app").await.unwrap();
    // A meta exactly at the threshold is inline; one byte more goes to the blob store.
    let base = Meta::new("a", bee_object::Timestamp::new(0, 0), "c", "");
    let overhead = base.encode().len();
    let mk = |len: usize| {
        let mut m = base.clone();
        m.message = "x".repeat(len);
        m
    };
    let mut at = mk(bee_store::INLINE_MAX_BYTES - overhead);
    // Adjust for the length prefix growing with the message.
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
        .get_objects(r.org, &[at.id(), over.id()], ReadFrom::Primary)
        .await
        .unwrap();
    assert!(rows[0].as_ref().unwrap().inline.is_some());
    assert!(rows[1].as_ref().unwrap().inline.is_none());
    assert!(!env.blobs.has(r.org, &[at.id()]).await.unwrap()[0]);
    assert!(env.blobs.has(r.org, &[over.id()]).await.unwrap()[0]);
    // A row that claims inline bytes for a too-large object is refused.
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

    // Hot to cold: same id, transparent reads, range reads.
    let big = over.id();
    assert!(
        env.blobs
            .set_class(r.org, &big, StorageClass::Cold)
            .await
            .unwrap()
    );
    let before = env.counters.snapshot();
    let bytes = trees::read_object_bytes(
        env.meta_dyn(),
        env.blobs.as_ref(),
        r.org,
        &big,
        ReadFrom::Primary,
    )
    .await
    .unwrap();
    assert_eq!(bytes, over.encode());
    assert_eq!(env.counters.snapshot().since(&before).blob.cold_reads, 1);
    let st = env.blobs.stat(r.org, &big).await.unwrap().unwrap();
    assert_eq!(st.class, StorageClass::Cold);
    let range = env
        .blobs
        .get_range(r.org, &big, 2, 5)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(range, over.encode()[2..7].to_vec());
    let tail = env
        .blobs
        .get_range(r.org, &big, st.len - 3, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tail.len(), 3);
    // Quarantine hides bytes; release restores them.
    assert!(env.blobs.quarantine(r.org, &big).await.unwrap());
    assert!(env.blobs.get(r.org, &big).await.unwrap().is_none());
    assert!(
        env.blobs
            .stat(r.org, &big)
            .await
            .unwrap()
            .unwrap()
            .quarantined
    );
    assert!(env.blobs.release(r.org, &big).await.unwrap());
    assert!(!env.blobs.release(r.org, &big).await.unwrap());
    assert!(env.blobs.get(r.org, &big).await.unwrap().is_some());
    // Conditional delete keeps bytes written at or after the cutoff.
    assert!(
        !env.blobs
            .delete(r.org, &big, Some(env.now()))
            .await
            .unwrap()
    );
    assert!(
        env.blobs
            .delete(r.org, &big, Some(env.now() + 1))
            .await
            .unwrap()
    );
    assert!(!env.blobs.delete(r.org, &big, None).await.unwrap());
    assert_eq!(env.blobs.list(r.org, None, 10).await.unwrap().len(), 0);
}

#[tokio::test]
async fn names_prefixes_and_indexes() {
    let env = TestEnv::new();
    let r = env.create_repo("acme", "app").await.unwrap();
    let r2 = env.create_repo("acme", "lib").await.unwrap();
    assert_eq!(r.org, r2.org);
    assert!(env.create_repo("acme", "app").await.is_err());
    let org = env.meta.org_by_name("acme").await.unwrap().unwrap();
    assert_eq!(env.meta.get_org(org.id).await.unwrap(), Some(org.clone()));
    assert_eq!(env.meta.list_repos(org.id).await.unwrap().len(), 2);
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
    let mut renamed = repo_row;
    renamed.name = "other".into();
    assert_eq!(
        env.meta.create_repo(renamed).await.unwrap_err().reason,
        Reason::AlreadyExists
    );
    assert_eq!(
        env.meta.create_org(org.clone()).await.unwrap(),
        PutOutcome::AlreadyPresent
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
    let prefix = ObjectIdPrefix::parse(&b.to_text()[..13]).unwrap();
    assert_eq!(
        env.meta
            .changesets_with_prefix(r.repo, &prefix, 10)
            .await
            .unwrap(),
        vec![b]
    );
    // Dedup per org, visibility per repo: the blob has a row once, paths only in r.
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
    let page1 = env.meta.list_changesets(r.repo, None, 1).await.unwrap();
    let page2 = env
        .meta
        .list_changesets(r.repo, Some(page1[0].id), 10)
        .await
        .unwrap();
    assert_eq!(page1.len() + page2.len(), 2);
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
    assert_eq!(a.kind(), ObjectKind::Changeset);
}

/// Purging content deletes the rows of content-bearing derived types (SPEC §13):
/// `delete_derived_type` removes every key and version of one type in one repo.
#[tokio::test]
async fn delete_derived_type_removes_one_type_in_one_repo() {
    let env = TestEnv::new();
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

/// SPEC §6.2: a changeset contributes an `object_paths` row for every new tree at its
/// directory path (and the root tree at the root), not only for blobs, so a tree is readable
/// exactly where a blob under it would be.
#[tokio::test]
async fn path_index_rows_cover_new_trees_at_their_directory_paths() {
    let env = TestEnv::new();
    let r = env.create_repo("acme", "app").await.unwrap();
    let paths = |id: bee_core::ObjectId| {
        let env = &env;
        async move {
            env.meta
                .object_paths(r.repo, &id, ReadFrom::Primary)
                .await
                .unwrap()
                .into_iter()
                .map(|p| p.map(|p| p.to_string()))
                .collect::<Vec<_>>()
        }
    };
    let trees_of = |cs: bee_core::ObjectId| {
        let env = &env;
        async move {
            let row = env
                .meta
                .get_changeset(r.repo, &cs, ReadFrom::Primary)
                .await
                .unwrap()
                .unwrap();
            let c = Changeset::decode(&row.bytes).unwrap();
            let read = |id: bee_core::ObjectId| async move {
                trees::read_tree(
                    env.meta_dyn(),
                    env.mem_blobs(),
                    r.org,
                    &id,
                    ReadFrom::Primary,
                )
                .await
                .unwrap()
            };
            let root = *c.root();
            let src = read(root).await.get(b"src").unwrap().id;
            let net = read(src).await.get(b"net").unwrap().id;
            (root, src, net)
        }
    };
    let a = env
        .commit(
            &r,
            None,
            &[
                Change::put("src/net/tcp.rs", "tcp"),
                Change::put("README", "r"),
            ],
        )
        .await
        .unwrap();
    let (root, src, net) = trees_of(a).await;
    assert_eq!(paths(root).await, vec![None]);
    assert_eq!(paths(src).await, vec![Some("src".to_string())]);
    assert_eq!(paths(net).await, vec![Some("src/net".to_string())]);
    assert_eq!(
        paths(blob_id(b"tcp")).await,
        vec![Some("src/net/tcp.rs".to_string())]
    );
    // A change deep in the tree: the new trees on its path get rows at their directories.
    let b = env
        .commit(&r, Some(a), &[Change::put("src/net/udp.rs", "udp")])
        .await
        .unwrap();
    let (root2, src2, net2) = trees_of(b).await;
    assert_ne!(src2, src);
    assert_eq!(paths(root2).await, vec![None]);
    assert_eq!(paths(src2).await, vec![Some("src".to_string())]);
    assert_eq!(paths(net2).await, vec![Some("src/net".to_string())]);
}

/// Every object reachable from `root` (trees and blobs), depth first.
async fn reachable(
    env: &TestEnv,
    org: bee_store::OrgId,
    root: bee_core::ObjectId,
) -> Vec<bee_core::ObjectId> {
    let mut out = vec![root];
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let tree = trees::read_tree(env.meta_dyn(), env.mem_blobs(), org, &id, ReadFrom::Primary)
            .await
            .unwrap();
        for e in tree.entries() {
            out.push(e.id);
            if e.mode == Mode::Tree {
                stack.push(e.id);
            }
        }
    }
    out
}

/// SPEC §4.7, the four global invariants, across commits, ref moves, a purge and a real GC run:
/// (1) stored bytes hash to their id; (2) a changeset row exists only with every object it
/// references; (3) nothing is mutated except the listed mutable state (ref tips, appended ref
/// log entries, object states, `last_used_ms`, redaction records); (4) ids never change.
#[tokio::test]
async fn global_invariants_hold_across_every_kind_of_write() {
    let env = TestEnv::new();
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    let a = env
        .commit_on(&r, &main, &[Change::put("src/lib.rs", "pub fn a() {}\n")])
        .await
        .unwrap();
    let b = env
        .commit_on(&r, &main, &[Change::put("secret.env", "KEY=1\n")])
        .await
        .unwrap();
    let c = env
        .commit_on(&r, &main, &[Change::put("src/net/tcp.rs", "tcp\n")])
        .await
        .unwrap();
    // The snapshot: every changeset row, every reachable object row with its path rows,
    // the eager rows and the ref log so far.
    let mut changesets = std::collections::BTreeMap::new();
    let mut objects = std::collections::BTreeMap::new();
    for id in [a, b, c] {
        let row = env
            .meta
            .get_changeset(r.repo, &id, ReadFrom::Primary)
            .await
            .unwrap()
            .unwrap();
        let graph = env
            .meta
            .get_derived(r.repo, &id, "graph", 1, ReadFrom::Primary)
            .await
            .unwrap();
        let mut objs = reachable(&env, r.org, *row.changeset.root()).await;
        objs.push(*row.changeset.meta());
        for o in objs {
            let orow = env
                .meta
                .get_object(r.org, &o, ReadFrom::Primary)
                .await
                .unwrap()
                .unwrap();
            let paths = env
                .meta
                .object_paths(r.repo, &o, ReadFrom::Primary)
                .await
                .unwrap();
            objects.insert(o, (orow, paths));
        }
        changesets.insert(id, (row.bytes.clone(), row.created_at_ms, graph));
    }
    let log_before = env.meta.ref_log(r.repo, &main, 1, 100).await.unwrap();
    // Every kind of write: another commit and ref move, a purge, a real GC run.
    let d = env
        .commit_on(&r, &main, &[Change::put("src/lib.rs", "pub fn b() {}\n")])
        .await
        .unwrap();
    let secret = blob_id(b"KEY=1\n");
    env.writer
        .redact(RedactionRow {
            org: r.org,
            object: secret,
            principal: "admin".into(),
            reason: "leak".into(),
            at_ms: env.now(),
        })
        .await
        .unwrap();
    bee_store::gc::run(
        env.meta_dyn(),
        env.blobs.as_ref(),
        r.org,
        &bee_store::gc::GcOptions::at(env.now()),
    )
    .await
    .unwrap();
    // (1) and (4): every changeset row is byte-identical and still hashes to its id.
    for (id, (bytes, created, graph)) in &changesets {
        let row = env
            .meta
            .get_changeset(r.repo, id, ReadFrom::Primary)
            .await
            .unwrap()
            .expect("still there");
        assert_eq!(&row.bytes, bytes, "changeset {id} unchanged");
        assert_eq!(row.created_at_ms, *created);
        assert_eq!(bee_object::hash_id(ObjectKind::Changeset, &row.bytes), *id);
        let g = env
            .meta
            .get_derived(r.repo, id, "graph", 1, ReadFrom::Primary)
            .await
            .unwrap();
        assert_eq!(&g, graph, "eager rows are never rewritten");
    }
    // (3): object rows and path rows are unchanged; only the purged object's state moved.
    for (id, (before, paths)) in &objects {
        let after = env
            .meta
            .get_object(r.org, id, ReadFrom::Primary)
            .await
            .unwrap()
            .expect("rows are never deleted while referenced");
        assert_eq!(after.size, before.size);
        assert_eq!(after.inline, before.inline);
        assert_eq!(after.created_at_ms, before.created_at_ms);
        if *id == secret {
            assert_eq!(after.state, ObjectState::Purged);
        } else {
            assert_eq!(after.state, before.state, "{id}");
            if id.kind() != ObjectKind::Changeset {
                let bytes = trees::read_object_bytes(
                    env.meta_dyn(),
                    env.mem_blobs(),
                    r.org,
                    id,
                    ReadFrom::Primary,
                )
                .await
                .unwrap();
                assert_eq!(bee_object::hash_id(id.kind(), &bytes), *id, "(1) for {id}");
            }
        }
        let p = env
            .meta
            .object_paths(r.repo, id, ReadFrom::Primary)
            .await
            .unwrap();
        assert_eq!(&p, paths, "path rows of {id}");
    }
    // The ref log only grew; the ref moved by CAS.
    let log_after = env.meta.ref_log(r.repo, &main, 1, 100).await.unwrap();
    assert_eq!(&log_after[..log_before.len()], &log_before[..]);
    assert_eq!(log_after.len(), log_before.len() + 1);
    let tip = env.meta.get_ref(r.repo, &main).await.unwrap().unwrap();
    assert_eq!(tip.target, Some(d));
    // (2): fsck finds every reference resolved (the purged blob is explained by its record).
    let report = env.fsck(r.org).await.unwrap();
    assert!(report.is_clean(), "{:?}", report.violations);
}
