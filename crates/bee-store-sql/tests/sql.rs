//! Behaviour specific to the SQL backends: versioned migrations, reopening, read replicas with
//! `min_version` (SPEC §6.3), statement deadlines, retried serialization conflicts, and fsck
//! catching rows broken behind the store's back.

use crate::common;
use std::sync::Arc;
use std::time::Duration;

use bee_core::{ObjectId, Reason};
use bee_store::{
    AccessCounters, Expected, MetaStore, MinVersion, NewEvent, ReadFrom, RefUpdate,
    ViolationKind as V, consistency,
};
use bee_store_sql::{PgMetaStore, PgOptions, SqliteMetaStore, SqliteOptions, migrate};
use bee_testkit::fixtures::{Change, ref_name};
use common::{Cleanup, Env, Open, pg_url};

fn ev(kind: &str) -> NewEvent {
    NewEvent {
        kind: kind.into(),
        payload: vec![],
        at_ms: 1,
    }
}

fn sqlite_path<S: Open>(env: &Env<S>) -> std::path::PathBuf {
    match env.location() {
        Cleanup::Dir(d) => d.join("meta.db"),
        _ => panic!("not sqlite"),
    }
}

fn pg_schema<S: Open>(env: &Env<S>) -> (String, String) {
    match env.location() {
        Cleanup::Schema { url, schema } => (url.clone(), schema.clone()),
        _ => panic!("not postgres"),
    }
}

async fn raw_pg(url: &str) -> (tokio_postgres::Client, tokio::task::JoinHandle<()>) {
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect");
    let h = tokio::spawn(async move {
        let _ = conn.await;
    });
    (client, h)
}

// ----- migrations and reopening ------------------------------------------------------------

#[tokio::test]
async fn sqlite_reopen_keeps_data_and_migrations_are_versioned() {
    let env = env_or_skip!(SqliteMetaStore);
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    let a = env
        .commit_on(&r, &main, &[Change::put("a", "1")])
        .await
        .unwrap();
    let path = sqlite_path(&env);
    // A second store on the same file (as after a restart) sees everything; migrating again
    // is a no-op.
    let again = SqliteMetaStore::open(SqliteOptions::new(&path))
        .await
        .unwrap();
    let rf = again.get_ref(r.repo, &main).await.unwrap().unwrap();
    assert_eq!((rf.target, rf.version), (Some(a), 1));
    let conn = rusqlite::Connection::open(&path).unwrap();
    let versions: Vec<i64> = conn
        .prepare("SELECT version FROM bee_schema ORDER BY version")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        versions,
        (1..=migrate::latest_version()).collect::<Vec<_>>()
    );
    let journal: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    // A database migrated by a newer binary is refused, not silently used.
    conn.execute(
        "INSERT INTO bee_schema (version, name, digest) VALUES (999, 'future', 'x')",
        [],
    )
    .unwrap();
    let e = SqliteMetaStore::open(SqliteOptions::new(&path))
        .await
        .err()
        .expect("newer schema refused");
    assert_eq!(e.reason, Reason::Unavailable);
    assert!(!e.retryable);
}

#[tokio::test]
async fn postgres_reopen_keeps_data_and_migrations_are_versioned() {
    let env = env_or_skip!(PgMetaStore);
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    let a = env
        .commit_on(&r, &main, &[Change::put("a", "1")])
        .await
        .unwrap();
    let (url, schema) = pg_schema(&env);
    let mut opts = PgOptions::new(&url);
    opts.schema = Some(schema.clone());
    let again = PgMetaStore::connect(opts.clone()).await.unwrap();
    let rf = again.get_ref(r.repo, &main).await.unwrap().unwrap();
    assert_eq!((rf.target, rf.version), (Some(a), 1));
    let (client, _h) = raw_pg(&url).await;
    let rows = client
        .query(
            &format!("SELECT version FROM \"{schema}\".bee_schema ORDER BY version"),
            &[],
        )
        .await
        .unwrap();
    let versions: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(
        versions,
        (1..=migrate::latest_version()).collect::<Vec<_>>()
    );
    client
        .batch_execute(&format!(
            "INSERT INTO \"{schema}\".bee_schema (version, name, digest) VALUES (999, 'future', 'x')"
        ))
        .await
        .unwrap();
    let e = PgMetaStore::connect(opts)
        .await
        .err()
        .expect("newer schema refused");
    assert_eq!(e.reason, Reason::Unavailable);
    assert!(!e.retryable);
}

#[tokio::test]
async fn concurrent_postgres_openers_migrate_once() {
    let Some(url) = pg_url() else {
        return;
    };
    let opts = common::pg_options(&url);
    let schema = opts.schema.clone().unwrap();
    let _cleanup = Cleanup::Schema {
        url: url.clone(),
        schema,
    };
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let o = opts.clone();
        tasks.push(tokio::spawn(async move {
            PgMetaStore::connect(o).await.map(|_| ())
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
}

// ----- read replicas -----------------------------------------------------------------------

#[tokio::test]
async fn sqlite_replica_reads_fall_back_and_honour_min_version() {
    let env = env_or_skip!(SqliteMetaStore);
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    let a = env
        .commit_on(&r, &main, &[Change::put("a", "1")])
        .await
        .unwrap();
    let path = sqlite_path(&env);
    let dir = path.parent().unwrap().to_path_buf();
    let snapshot = |name: &str| {
        let p = dir.join(name);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("VACUUM INTO ?1", [p.to_str().unwrap()])
            .unwrap();
        p
    };
    let stale = snapshot("replica-1.db");
    let b = env
        .commit_on(&r, &main, &[Change::put("b", "2")])
        .await
        .unwrap();
    let seqs = env.meta.append_events(r.repo, vec![ev("x")]).await.unwrap();

    let counters = Arc::new(AccessCounters::new());
    let mut opts = SqliteOptions::new(&path);
    opts.replica_path = Some(stale);
    opts.read_wait = Duration::from_millis(20);
    let store = SqliteMetaStore::open_with_counters(opts, counters.clone())
        .await
        .unwrap();
    // The replica has a but not b: a direct replica read of b misses...
    assert!(
        store
            .get_changeset(r.repo, &a, ReadFrom::Replica(None))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .get_changeset(r.repo, &b, ReadFrom::Replica(None))
            .await
            .unwrap()
            .is_none()
    );
    // ...and the consistency helper retries on the primary before answering.
    let before = counters.snapshot();
    assert_eq!(
        consistency::changeset(&store, r.repo, &b, None)
            .await
            .unwrap()
            .id,
        b
    );
    assert_eq!(counters.snapshot().since(&before).replica_reads, 1);
    let e = consistency::changeset(
        &store,
        r.repo,
        &ObjectId::from_bytes(&[1; 34]).unwrap(),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(e.reason, Reason::NotFound);
    // read-your-writes: a min_version the replica has not reached is forwarded.
    let min = MinVersion::Ref {
        repo: r.repo,
        name: main.clone(),
        version: 2,
    };
    let before = counters.snapshot();
    let row = store
        .get_changeset(r.repo, &b, ReadFrom::Replica(Some(min.clone())))
        .await
        .unwrap();
    assert_eq!(row.map(|r| r.id), Some(b));
    assert_eq!(counters.snapshot().since(&before).replica_forwards, 1);
    let ev_min = MinVersion::Event {
        repo: r.repo,
        seq: seqs[0],
    };
    let fwd = store
        .read_events(r.repo, 0, 10, ReadFrom::Replica(Some(ev_min.clone())))
        .await
        .unwrap();
    assert_eq!(fwd.last().map(|e| e.seq), Some(seqs[0]));
    assert!(
        store
            .read_events(r.repo, 0, 10, ReadFrom::Replica(None))
            .await
            .unwrap()
            .is_empty()
    );
    // A replica that has caught up serves the read itself.
    let fresh = snapshot("replica-2.db");
    let mut opts = SqliteOptions::new(&path);
    opts.replica_path = Some(fresh);
    let caught_up = SqliteMetaStore::open_with_counters(opts, counters.clone())
        .await
        .unwrap();
    let before = counters.snapshot();
    assert!(
        caught_up
            .get_changeset(r.repo, &b, ReadFrom::Replica(Some(min)))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        caught_up
            .get_objects(
                r.org,
                &[bee_object::blob_id(b"2")],
                ReadFrom::Replica(Some(ev_min))
            )
            .await
            .unwrap()[0]
            .is_some()
    );
    let d = counters.snapshot().since(&before);
    assert_eq!((d.replica_reads, d.replica_forwards), (2, 0));
    assert_eq!(
        consistency::derived(&caught_up, r.repo, &b, "graph", 1, None)
            .await
            .unwrap(),
        Some(2u64.to_be_bytes().to_vec())
    );
}

/// Resumes WAL replay on the standby when dropped, even if the test fails.
struct Resume(String);

impl Drop for Resume {
    fn drop(&mut self) {
        let url = self.0.clone();
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                if let Ok((c, conn)) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await {
                    let h = tokio::spawn(conn);
                    let _ = c.batch_execute("SELECT pg_wal_replay_resume()").await;
                    drop(c);
                    let _ = h.await;
                }
            });
        })
        .join();
    }
}

#[tokio::test]
async fn postgres_replica_reads_fall_back_and_honour_min_version() {
    let Ok(replica) = std::env::var("BEE_TEST_PG_REPLICA") else {
        eprintln!("skipped: BEE_TEST_PG_REPLICA is not set");
        return;
    };
    let env = env_or_skip!(PgMetaStore);
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    let a = env
        .commit_on(&r, &main, &[Change::put("a", "1")])
        .await
        .unwrap();
    let (url, schema) = pg_schema(&env);
    let counters = Arc::new(AccessCounters::new());
    let mut opts = PgOptions::new(&url);
    opts.schema = Some(schema);
    opts.replica_url = Some(replica.clone());
    opts.read_wait = Duration::from_millis(30);
    let store = PgMetaStore::connect_with_counters(opts, counters.clone())
        .await
        .unwrap();
    // Wait for the standby to replay a, then stop replay so it falls behind.
    let mut tries = 0;
    while store
        .get_changeset(r.repo, &a, ReadFrom::Replica(None))
        .await
        .map(|r| r.is_none())
        .unwrap_or(true)
    {
        tries += 1;
        assert!(tries < 500, "standby never caught up");
        common::pause(10).await;
    }
    let (standby, _h) = raw_pg(&replica).await;
    let _resume = Resume(replica.clone());
    standby
        .batch_execute("SELECT pg_wal_replay_pause()")
        .await
        .unwrap();
    let b = env
        .commit_on(&r, &main, &[Change::put("b", "2")])
        .await
        .unwrap();
    assert!(
        store
            .get_changeset(r.repo, &b, ReadFrom::Replica(None))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        consistency::changeset(&store, r.repo, &b, None)
            .await
            .unwrap()
            .id,
        b
    );
    let min = MinVersion::Ref {
        repo: r.repo,
        name: main.clone(),
        version: 2,
    };
    let before = counters.snapshot();
    let started = std::time::Instant::now();
    let row = store
        .get_changeset(r.repo, &b, ReadFrom::Replica(Some(min.clone())))
        .await
        .unwrap();
    assert_eq!(row.map(|r| r.id), Some(b));
    assert!(
        started.elapsed() >= Duration::from_millis(30),
        "did not wait"
    );
    assert_eq!(counters.snapshot().since(&before).replica_forwards, 1);
    // Resume replay: once the standby reaches version 2 it serves the read itself.
    standby
        .batch_execute("SELECT pg_wal_replay_resume()")
        .await
        .unwrap();
    let mut tries = 0;
    loop {
        let before = counters.snapshot();
        let row = store
            .get_changeset(r.repo, &b, ReadFrom::Replica(Some(min.clone())))
            .await
            .unwrap();
        assert!(row.is_some());
        if counters.snapshot().since(&before).replica_forwards == 0 {
            break;
        }
        tries += 1;
        assert!(tries < 200, "standby never caught up after resume");
    }
}

// ----- deadlines and retries ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_statements_have_deadlines() {
    let env = env_or_skip!(PgMetaStore);
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
    let (url, schema) = pg_schema(&env);
    let mut opts = PgOptions::new(&url);
    opts.schema = Some(schema.clone());
    opts.query_timeout = Duration::from_millis(300);
    let short = PgMetaStore::connect(opts).await.unwrap();
    // Another session holds the ref row lock.
    let (holder, _h) = raw_pg(&url).await;
    holder
        .batch_execute(&format!(
            "BEGIN; SELECT * FROM \"{schema}\".refs WHERE name = 'main' FOR UPDATE"
        ))
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let e = short
        .update_ref(RefUpdate::set(
            r.repo,
            main.clone(),
            Expected::Version(1),
            Some(b),
            "p",
            1,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.reason, Reason::DeadlineExceeded, "{e}");
    assert!(e.retryable);
    assert!(started.elapsed() < Duration::from_secs(3));
    holder.batch_execute("ROLLBACK").await.unwrap();
    let up = short
        .update_ref(RefUpdate::set(
            r.repo,
            main,
            Expected::Version(1),
            Some(b),
            "p",
            1,
        ))
        .await
        .unwrap();
    assert_eq!(up.new_version, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_busy_writer_is_retried_then_unavailable() {
    let env = env_or_skip!(SqliteMetaStore);
    let r = env.create_repo("acme", "app").await.unwrap();
    let path = sqlite_path(&env);
    let mut opts = SqliteOptions::new(&path);
    opts.query_timeout = Duration::from_millis(20);
    let short = SqliteMetaStore::open(opts).await.unwrap();
    // Another process-like connection holds the write lock.
    let holder = rusqlite::Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let e = short
        .append_events(r.repo, vec![ev("x")])
        .await
        .unwrap_err();
    assert_eq!(e.reason, Reason::Unavailable, "{e}");
    assert!(e.retryable);
    holder.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        short.append_events(r.repo, vec![ev("y")]).await.unwrap(),
        vec![1]
    );
    // A lock released while the store is backing off is picked up by a retry.
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        holder.execute_batch("ROLLBACK").unwrap();
    });
    assert_eq!(
        short.append_events(r.repo, vec![ev("z")]).await.unwrap(),
        vec![2]
    );
    release.join().unwrap();
}

async fn serialization_conflicts_are_retried<S: Open>() {
    // Updates of different refs in one repo each append an event, so they all contend on
    // the repo's event counter; on Postgres (SERIALIZABLE) the losers get SQLSTATE 40001 and
    // must be retried by the store, never surfaced.
    let env = Arc::new(env_or_skip!(S));
    let r = env.create_repo("acme", "app").await.unwrap();
    let a = env
        .commit(&r, None, &[Change::put("a", "1")])
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..24 {
        let env = env.clone();
        tasks.push(tokio::spawn(async move {
            let mut up = RefUpdate::set(
                r.repo,
                ref_name(&format!("ws/agent/r{i}")),
                Expected::Absent,
                Some(a),
                "p",
                1,
            );
            up.events = vec![ev("ref.updated")];
            env.meta.update_ref(up).await
        }));
    }
    let mut seqs = Vec::new();
    for t in tasks {
        let up = t.await.unwrap().expect("conflict surfaced to the caller");
        seqs.extend(up.event_seqs);
    }
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=24).collect::<Vec<u64>>());
    assert_eq!(env.meta.list_refs(r.repo, "ws/").await.unwrap().len(), 24);
}
both!(serialization_conflicts_are_retried, multi);

// ----- fsck sees rows broken behind the store's back ----------------------------------------

async fn fsck_catches_rows_broken_with_raw_sql<S: Open>() {
    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "app").await.unwrap();
    let main = ref_name("main");
    env.commit_on(&r, &main, &[Change::put("src/a.rs", "a\n")])
        .await
        .unwrap();
    env.commit_on(&r, &main, &[Change::put("src/b.rs", "b\n")])
        .await
        .unwrap();
    env.meta.append_events(r.repo, vec![ev("e")]).await.unwrap();
    assert!(env.fsck(r.org).await.unwrap().is_clean());

    env.raw_sql("UPDATE event_counters SET allocated = 0").await;
    assert!(
        env.fsck(r.org)
            .await
            .unwrap()
            .kinds()
            .contains(&V::EventBeyondCounter)
    );
    env.raw_sql("UPDATE event_counters SET allocated = 1").await;
    assert!(env.fsck(r.org).await.unwrap().is_clean());

    env.raw_sql("DELETE FROM ref_log WHERE version = 1").await;
    assert!(
        env.fsck(r.org)
            .await
            .unwrap()
            .kinds()
            .contains(&V::RefLogGap)
    );

    env.raw_sql("DELETE FROM object_paths").await;
    assert!(
        env.fsck(r.org)
            .await
            .unwrap()
            .kinds()
            .contains(&V::ObjectPathMissing)
    );
}
both!(fsck_catches_rows_broken_with_raw_sql);

// ----- buffered writes (the land coalescer's batch) ------------------------------------------

/// A buffered writer serves the changesets and trees it built before they are written, and
/// `flush` writes a whole chain of them in one write transaction (one durable commit), with
/// every row the unbuffered writes would have made.
async fn a_buffered_chain_is_written_in_one_transaction<S: Open>() {
    use bee_core::{ChangeId, Mode, RepoPath};
    use bee_object::{DeltaEntry, Meta, Timestamp, blob_id};
    use bee_store::{DerivedRow, ReadFrom};

    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "staged").await.unwrap();
    let base = env
        .commit(&r, None, &[Change::put("a", "x")])
        .await
        .unwrap();
    let now = env.now();
    let datas: Vec<Vec<u8>> = (0..20u8)
        .map(|i| format!("file {i}\n").into_bytes())
        .collect();
    // Blobs are uploaded first, as clients do.
    env.writer
        .upload(
            r.org,
            datas.iter().map(|d| (blob_id(d), d.clone())).collect(),
            None,
            now,
        )
        .await
        .unwrap();
    let (writer, staged) = env.writer.buffered();
    let mut parent = base;
    let mut chain = Vec::new();
    for (i, d) in datas.iter().enumerate() {
        let meta = Meta::new(
            "Test Agent <agent@test>",
            Timestamp::new(1, 0),
            "agent:test",
            format!("step {i}"),
        );
        writer
            .upload(r.org, vec![(meta.id(), meta.encode())], None, now)
            .await
            .unwrap();
        let mut cid = [i as u8; 16];
        cid[0] |= 1;
        cid[15] = 0xc5;
        let built = writer
            .build(
                r.org,
                r.repo,
                vec![parent],
                meta.id(),
                ChangeId::new(cid),
                vec![DeltaEntry::put(
                    RepoPath::new(format!("d/f{i:02}").into_bytes()).unwrap(),
                    Mode::File,
                    blob_id(d),
                    d.len() as u64,
                )],
            )
            .await
            .unwrap();
        let graph = DerivedRow {
            key: built.id(),
            type_name: "graph".into(),
            version: 1,
            value: (i as u64 + 2).to_be_bytes().to_vec(),
        };
        writer
            .store(r.org, r.repo, &built, vec![graph], now)
            .await
            .unwrap();
        // Readable through the buffer (the next step builds on it), not yet in the store.
        let id = built.id();
        assert!(
            writer
                .meta()
                .get_changeset(r.repo, &id, ReadFrom::Primary)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            env.meta
                .get_changeset(r.repo, &id, ReadFrom::Primary)
                .await
                .unwrap()
                .is_none()
        );
        parent = id;
        chain.push(id);
    }
    assert_eq!(staged.pending_changesets(), 20);
    let before = env.counters.snapshot();
    staged.flush().await.unwrap();
    let spent = env.counters.snapshot().since(&before);
    assert_eq!(
        spent.write_transactions, 1,
        "20 changesets with their trees in one commit"
    );
    for id in &chain {
        assert!(
            env.meta
                .get_changeset(r.repo, id, ReadFrom::Primary)
                .await
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(staged.pending_changesets(), 0);
    let report = env.fsck(r.org).await.unwrap();
    assert!(report.violations.is_empty(), "{:?}", report.violations);
}
both!(a_buffered_chain_is_written_in_one_transaction);

// ----- CreateChangeset (SPEC §16: independent of repo size) ------------------------------------

/// A changeset's root is read from a prefix of its row, whatever the size of its delta, and
/// a new changeset with its meta and trees is written in one write transaction: all of it
/// or (when a referenced object is missing) none of it.
async fn a_new_changeset_commits_once_and_parents_are_read_by_root<S: Open>() {
    use bee_core::{ChangeId, Mode, RepoPath};
    use bee_object::{DeltaEntry, Meta, Timestamp, blob_id};
    use bee_store::UploadRecord;

    let env = env_or_skip!(S);
    let r = env.create_repo("acme", "create").await.unwrap();
    let files: Vec<Change> = (0..2_000)
        .map(|i| Change::put(&format!("bulk/d{:02}/f{i:05}", i % 20), format!("{i}")))
        .collect();
    let bulk = env.commit(&r, None, &files).await.unwrap();
    let row = env
        .meta
        .get_changeset(r.repo, &bulk, ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    assert!(row.bytes.len() > 100_000, "{}", row.bytes.len());

    // The root costs one row read and at most 1 KiB of the 2,000-entry row.
    let before = env.counters.snapshot();
    let root = env
        .meta
        .get_changeset_root(r.repo, &bulk, ReadFrom::Primary)
        .await
        .unwrap();
    let spent = env.counters.snapshot().since(&before);
    assert_eq!(root, Some(*row.changeset.root()));
    assert!(spent.changeset_bytes_read <= 1_024, "{spent:?}");
    assert_eq!(spent.total_reads(), 1, "{spent:?}");
    let full = env.counters.snapshot();
    env.meta
        .get_changeset(r.repo, &bulk, ReadFrom::Primary)
        .await
        .unwrap();
    assert_eq!(
        env.counters.snapshot().since(&full).changeset_bytes_read,
        row.bytes.len() as u64,
        "a whole-row read counts every byte"
    );
    let unknown = ObjectId::new(bee_core::ObjectKind::Changeset, [7; 32]);
    assert_eq!(
        env.meta
            .get_changeset_root(r.repo, &unknown, ReadFrom::Primary)
            .await
            .unwrap(),
        None
    );

    // One new file on top of the bulk changeset, with its meta, stored in one commit.
    let now = env.now();
    let data = b"hot\n".to_vec();
    env.writer
        .upload(r.org, vec![(blob_id(&data), data.clone())], None, now)
        .await
        .unwrap();
    let (writer, org, repo) = (&env.writer, r.org, r.repo);
    let new_change = |path: &str, blob: ObjectId, size: u64, n: u8| {
        let path = path.to_string();
        async move {
            let meta = Meta::new(
                "Test Agent <agent@test>",
                Timestamp::new(1, 0),
                "agent:test",
                format!("hot {n}"),
            );
            let built = writer
                .build(
                    org,
                    repo,
                    vec![bulk],
                    meta.id(),
                    ChangeId::new([n | 1; 16]),
                    vec![DeltaEntry::put(
                        RepoPath::new(path.as_bytes().to_vec()).unwrap(),
                        Mode::File,
                        blob,
                        size,
                    )],
                )
                .await
                .unwrap();
            (meta, built)
        }
    };
    let (meta, built) = new_change("hot/file", blob_id(&data), data.len() as u64, 2).await;
    // Its eager graph row (generation 2 over the root-level bulk changeset), as the server
    // computes it, so fsck finds the changeset complete.
    let graph = vec![bee_store::DerivedRow {
        key: built.id(),
        type_name: "graph".into(),
        version: 1,
        value: 2u64.to_be_bytes().to_vec(),
    }];
    let upload = UploadRecord {
        repo: Some(r.repo),
        principal: "agent:test".into(),
    };
    let before = env.counters.snapshot();
    env.writer
        .store_new(
            r.org,
            r.repo,
            (meta.id(), meta.encode()),
            &built,
            Some(upload.clone()),
            graph.clone(),
            now,
        )
        .await
        .unwrap();
    let spent = env.counters.snapshot().since(&before);
    assert_eq!(spent.write_transactions, 1, "{spent:?}");
    assert!(
        env.meta
            .get_changeset(r.repo, &built.id(), ReadFrom::Primary)
            .await
            .unwrap()
            .is_some()
    );
    for id in built.new_trees.keys() {
        assert!(
            env.meta
                .get_object(r.org, id, ReadFrom::Primary)
                .await
                .unwrap()
                .is_some()
        );
    }
    let uploads = env.meta.get_uploads(r.org, &meta.id()).await.unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(
        (uploads[0].repo, uploads[0].principal.as_str()),
        (Some(r.repo), "agent:test")
    );
    // Idempotent.
    env.writer
        .store_new(
            r.org,
            r.repo,
            (meta.id(), meta.encode()),
            &built,
            Some(upload.clone()),
            graph.clone(),
            now,
        )
        .await
        .unwrap();

    // A put of a blob that was never uploaded: MISSING_OBJECT, and the meta and trees of
    // that changeset were not written either.
    let ghost = b"never uploaded".to_vec();
    let (meta, built) = new_change("hot/ghost", blob_id(&ghost), ghost.len() as u64, 4).await;
    let err = env
        .writer
        .store_new(
            r.org,
            r.repo,
            (meta.id(), meta.encode()),
            &built,
            Some(upload),
            vec![],
            now,
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason, Reason::MissingObject);
    assert!(
        env.meta
            .get_object(r.org, &meta.id(), ReadFrom::Primary)
            .await
            .unwrap()
            .is_none(),
        "the meta rolled back with the changeset"
    );
    assert!(
        env.meta
            .get_object(r.org, built.changeset.root(), ReadFrom::Primary)
            .await
            .unwrap()
            .is_none(),
        "the new root tree rolled back with the changeset"
    );
    let report = env.fsck(r.org).await.unwrap();
    assert!(report.violations.is_empty(), "{:?}", report.violations);
}
both!(a_new_changeset_commits_once_and_parents_are_read_by_root);

/// A write cancelled part-way through its transaction (a crashed caller) releases SQLite's
/// write lock at once, so another store on the same file (another server process) is not
/// blocked behind it until the first store happens to begin again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_cancelled_write_releases_the_lock_for_other_stores() {
    use bee_object::blob_id;
    use bee_store::NewObject;

    let env = env_or_skip!(SqliteMetaStore);
    let r = env.create_repo("acme", "cancel").await.unwrap();
    let other = SqliteMetaStore::open(SqliteOptions::new(sqlite_path(&env)))
        .await
        .unwrap();
    let rows = |n: u32, tag: &str| -> Vec<NewObject> {
        (0..n)
            .map(|i| {
                let b = format!("{tag} {i}").into_bytes();
                NewObject::for_bytes(blob_id(&b), &b)
            })
            .collect()
    };
    // A long batch on the first store, cancelled while its transaction is open.
    let long = env
        .store()
        .put_batch(vec![(r.org, rows(20_000, "long"), 1)], vec![]);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), long)
            .await
            .is_err(),
        "the batch is still running when cancelled"
    );
    // The other store writes promptly; nothing of the cancelled batch was committed.
    let t0 = std::time::Instant::now();
    other
        .put_objects(r.org, rows(10, "other"), None, 2)
        .await
        .expect("the write lock was released");
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    let b = b"long 0".to_vec();
    assert!(
        other
            .get_object(r.org, &blob_id(&b), ReadFrom::Primary)
            .await
            .unwrap()
            .is_none()
    );
}
