//! A test environment over any SQL-backed `MetaStore`: the store wrapped in `FaultyMeta`, an
//! in-memory blob store wrapped in `FaultyBlob`, a `Writer`, a deterministic clock and RNG,
//! and fixture builders that go through the real write path. It mirrors
//! `bee_testkit::TestEnv` (which is fixed to the in-memory store) so the testkit's store
//! suites can be re-run against SQLite and Postgres.
//!
//! SQLite stores live in fresh temp directories. Postgres stores run when `BEE_TEST_PG` is
//! a connection URL; each gets its own schema, dropped afterwards.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bee_core::{BeeError, ChangeId, ObjectId, Reason, RefName, Result};
use bee_object::{DeltaEntry, Meta, Timestamp, blob_id, digest};
use bee_store::{
    AccessCounters, DEFAULT_DRAFT_RETENTION_DAYS, DerivedRow, EagerType, Expected, FaultInjector,
    FaultyBlob, FaultyMeta, FsckOptions, FsckReport, MemoryBlobStore, MetaStore, OrgId, OrgRow,
    ReadFrom, RefUpdate, RefUpdated, RepoId, RepoRow, UploadRecord, Writer, fsck,
};
use bee_store_sql::{PgMetaStore, PgOptions, SqliteMetaStore, SqliteOptions};
use bee_testkit::fixtures::{Change, CommitSpec, apply_changes};
use bee_testkit::model::tree::{self as mtree, FileMap};
use bee_testkit::{TestClock, TestRng};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{prefix}_{}_{}_{nanos}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Removes a test database when dropped.
pub enum Cleanup {
    /// A SQLite temp directory.
    Dir(PathBuf),
    /// A Postgres schema.
    Schema { url: String, schema: String },
    /// Nothing to remove.
    None,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        match self {
            Cleanup::Dir(p) => {
                let _ = std::fs::remove_dir_all(p);
            }
            Cleanup::Schema { url, schema } => {
                let (url, schema) = (url.clone(), schema.clone());
                let _ = std::thread::spawn(move || drop_schema(&url, &schema)).join();
            }
            Cleanup::None => {}
        }
    }
}

fn drop_schema(url: &str, schema: &str) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    rt.block_on(async {
        if let Ok((client, conn)) = tokio_postgres::connect(url, tokio_postgres::NoTls).await {
            let h = tokio::spawn(conn);
            let _ = client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
                .await;
            drop(client);
            let _ = h.await;
        }
    });
}

/// The Postgres URL for gated tests.
pub fn pg_url() -> Option<String> {
    std::env::var("BEE_TEST_PG").ok().filter(|s| !s.is_empty())
}

/// A temp directory removed by the returned guard.
pub fn temp_dir() -> (PathBuf, Cleanup) {
    let dir = std::env::temp_dir().join(unique("bee-store-sql"));
    std::fs::create_dir_all(&dir).expect("temp dir");
    (dir.clone(), Cleanup::Dir(dir))
}

/// A SQL store that tests can open fresh.
pub trait Open: MetaStore + Sized + 'static {
    /// Backend label.
    const NAME: &'static str;

    /// A fresh, empty store counting into `counters`, or `None` when the backend is not
    /// available (Postgres without `BEE_TEST_PG`).
    fn fresh(counters: Arc<AccessCounters>)
    -> impl Future<Output = Option<(Self, Cleanup)>> + Send;
}

impl Open for SqliteMetaStore {
    const NAME: &'static str = "sqlite";

    async fn fresh(counters: Arc<AccessCounters>) -> Option<(Self, Cleanup)> {
        let (dir, guard) = temp_dir();
        let store =
            SqliteMetaStore::open_with_counters(SqliteOptions::new(dir.join("meta.db")), counters)
                .await
                .expect("open sqlite");
        Some((store, guard))
    }
}

/// Postgres options for a fresh schema.
pub fn pg_options(url: &str) -> PgOptions {
    let mut o = PgOptions::new(url);
    o.schema = Some(unique("t"));
    o.max_connections = 8;
    o
}

impl Open for PgMetaStore {
    const NAME: &'static str = "postgres";

    async fn fresh(counters: Arc<AccessCounters>) -> Option<(Self, Cleanup)> {
        let url = pg_url()?;
        let opts = pg_options(&url);
        let schema = opts.schema.clone().unwrap_or_default();
        let store = PgMetaStore::connect_with_counters(opts, counters)
            .await
            .expect("connect postgres");
        Some((store, Cleanup::Schema { url, schema }))
    }
}

/// The eager derived type the fixtures write (see `bee_testkit::fixtures::graph_type`).
pub fn graph_type() -> EagerType {
    EagerType::new("graph", 1)
}

/// A repo created by [`Env::create_repo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestRepo {
    pub org: OrgId,
    pub repo: RepoId,
}

/// The environment.
pub struct Env<S: Open> {
    pub clock: Arc<TestClock>,
    pub faults: Arc<FaultInjector>,
    pub counters: Arc<AccessCounters>,
    pub meta: Arc<FaultyMeta<S>>,
    pub blobs: Arc<FaultyBlob<MemoryBlobStore>>,
    pub writer: Writer,
    rng: Mutex<TestRng>,
    snapshots: Mutex<BTreeMap<ObjectId, FileMap>>,
    _cleanup: Cleanup,
}

impl<S: Open> Env<S> {
    /// A fresh environment with seed 1.
    pub async fn new() -> Option<Env<S>> {
        Env::with_seed(1).await
    }

    /// A fresh environment whose ids come from `seed`.
    pub async fn with_seed(seed: u64) -> Option<Env<S>> {
        let clock = Arc::new(TestClock::default());
        let faults = FaultInjector::new();
        let counters = Arc::new(AccessCounters::new());
        let (store, cleanup) = S::fresh(counters.clone()).await?;
        let meta = Arc::new(FaultyMeta::new(store, faults.clone()));
        let blobs = Arc::new(FaultyBlob::new(
            MemoryBlobStore::with_counters(clock.clone(), counters.clone()),
            faults.clone(),
        ));
        let writer = Writer::new(meta.clone(), blobs.clone()).with_faults(faults.clone());
        Some(Env {
            clock,
            faults,
            counters,
            meta,
            blobs,
            writer,
            rng: Mutex::new(TestRng::seed(seed)),
            snapshots: Mutex::new(BTreeMap::new()),
            _cleanup: cleanup,
        })
    }

    /// Where the database lives.
    pub fn location(&self) -> &Cleanup {
        &self._cleanup
    }

    pub fn store(&self) -> &S {
        self.meta.inner()
    }

    pub fn meta_dyn(&self) -> &dyn MetaStore {
        self.meta.as_ref()
    }

    pub fn now(&self) -> u64 {
        self.clock.now()
    }

    pub fn with_rng<T>(&self, f: impl FnOnce(&mut TestRng) -> T) -> T {
        f(&mut self.rng.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub async fn create_repo(&self, org_name: &str, repo_name: &str) -> Result<TestRepo> {
        let now = self.now();
        let org = match self.meta.org_by_name(org_name).await? {
            Some(o) => o.id,
            None => {
                let id = OrgId(self.with_rng(|r| r.ulid(now)));
                self.meta
                    .create_org(OrgRow {
                        id,
                        name: org_name.to_string(),
                        created_at_ms: now,
                    })
                    .await?;
                id
            }
        };
        let repo = RepoId(self.with_rng(|r| r.ulid(now)));
        self.meta
            .create_repo(RepoRow {
                id: repo,
                org,
                name: repo_name.to_string(),
                created_at_ms: now,
                draft_retention_days: DEFAULT_DRAFT_RETENTION_DAYS,
            })
            .await?;
        Ok(TestRepo { org, repo })
    }

    pub fn snapshot(&self, id: &ObjectId) -> Option<FileMap> {
        self.snapshots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    pub async fn commit(
        &self,
        repo: &TestRepo,
        parent: Option<ObjectId>,
        changes: &[Change],
    ) -> Result<ObjectId> {
        self.commit_with(
            repo,
            CommitSpec {
                parents: parent.into_iter().collect(),
                changes: changes.to_vec(),
                message: "test".into(),
                change_id: None,
            },
        )
        .await
    }

    /// Commits through the full write path, exactly as `bee_testkit::TestEnv` does.
    pub async fn commit_with(&self, repo: &TestRepo, spec: CommitSpec) -> Result<ObjectId> {
        let base = match spec.parents.first() {
            Some(p) => self.snapshot(p).ok_or_else(|| {
                BeeError::new(Reason::NotFound, format!("no snapshot for parent {p}"))
            })?,
            None => FileMap::new(),
        };
        let files = apply_changes(&base, &spec.changes)?;
        let delta: Vec<DeltaEntry> = mtree::diff(&base, &files);
        let now = self.now();
        let principal = "agent:test".to_string();
        let mut blobs: BTreeMap<ObjectId, Vec<u8>> = BTreeMap::new();
        for c in &spec.changes {
            if let Change::Put { data, .. } = c {
                blobs.insert(blob_id(data), data.clone());
            }
        }
        if !blobs.is_empty() {
            self.writer
                .upload(
                    repo.org,
                    blobs.into_iter().collect(),
                    Some(UploadRecord {
                        repo: Some(repo.repo),
                        principal: principal.clone(),
                    }),
                    now,
                )
                .await?;
        }
        let meta = Meta::new(
            "Test Agent <agent@test>",
            Timestamp::new((now / 1000) as i64, 0),
            principal.clone(),
            spec.message.clone(),
        );
        let meta_id = meta.id();
        self.writer
            .upload(repo.org, vec![(meta_id, meta.encode())], None, now)
            .await?;
        let change_id = spec.change_id.unwrap_or_else(|| {
            let mut input = Vec::new();
            for p in &spec.parents {
                input.extend_from_slice(p.as_bytes());
            }
            input.extend_from_slice(meta_id.as_bytes());
            for e in &delta {
                input.extend_from_slice(e.path.as_bytes());
                input.push(0);
                if let bee_object::DeltaOp::Put { id, .. } = e.op {
                    input.extend_from_slice(id.as_bytes());
                }
            }
            let d = digest(&input);
            let mut b = [0u8; 16];
            b.copy_from_slice(&d[..16]);
            b[0] |= 1;
            ChangeId::new(b)
        });
        let built = self
            .writer
            .build(
                repo.org,
                repo.repo,
                spec.parents.clone(),
                meta_id,
                change_id,
                delta,
            )
            .await?;
        let id = built.id();
        let mut generation = 0u64;
        for p in &spec.parents {
            let g = self
                .meta
                .get_derived(repo.repo, p, "graph", 1, ReadFrom::Primary)
                .await?
                .and_then(|v| <[u8; 8]>::try_from(v.as_slice()).ok())
                .map(u64::from_be_bytes)
                .unwrap_or(0);
            generation = generation.max(g);
        }
        let graph = DerivedRow {
            key: id,
            type_name: "graph".into(),
            version: 1,
            value: (generation + 1).to_be_bytes().to_vec(),
        };
        self.writer
            .store(repo.org, repo.repo, &built, vec![graph], now)
            .await?;
        self.snapshots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, files);
        Ok(id)
    }

    /// Moves `name` from `from` to `to`; idempotent when it already targets `to`.
    pub async fn advance(
        &self,
        repo: &TestRepo,
        name: &RefName,
        from: Option<ObjectId>,
        to: ObjectId,
    ) -> Result<RefUpdated> {
        let cur = self.meta.get_ref(repo.repo, name).await?;
        let (target, version) = cur.map(|r| (r.target, r.version)).unwrap_or((None, 0));
        if target == Some(to) {
            return Ok(RefUpdated {
                old_version: version,
                new_version: version,
                event_seqs: Vec::new(),
            });
        }
        if target != from {
            return Err(BeeError::new(
                Reason::VersionMismatch,
                format!("{name} moved to {target:?}"),
            )
            .with_payload("current_version", version as i64));
        }
        let expected = if target.is_none() {
            Expected::Absent
        } else {
            Expected::Version(version)
        };
        self.writer
            .update_ref(RefUpdate::set(
                repo.repo,
                name.clone(),
                expected,
                Some(to),
                "agent:test",
                self.now(),
            ))
            .await
    }

    pub async fn commit_on(
        &self,
        repo: &TestRepo,
        name: &RefName,
        changes: &[Change],
    ) -> Result<ObjectId> {
        let from = self
            .meta
            .get_ref(repo.repo, name)
            .await?
            .and_then(|r| r.target);
        let id = self.commit(repo, from, changes).await?;
        self.advance(repo, name, from, id).await?;
        Ok(id)
    }

    /// Runs raw SQL against the store's database, bypassing the store (to break invariants
    /// on purpose).
    pub async fn raw_sql(&self, sql: &str) {
        match &self._cleanup {
            Cleanup::Dir(dir) => {
                let conn = rusqlite::Connection::open(dir.join("meta.db")).expect("open raw");
                conn.busy_timeout(Duration::from_secs(5))
                    .expect("busy timeout");
                conn.execute_batch(sql).expect("raw sql");
            }
            Cleanup::Schema { url, schema } => {
                let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
                    .await
                    .expect("connect raw");
                let h = tokio::spawn(conn);
                client
                    .batch_execute(&format!("SET search_path = \"{schema}\"; {sql}"))
                    .await
                    .expect("raw sql");
                drop(client);
                let _ = h.await;
            }
            Cleanup::None => panic!("no database"),
        }
    }

    pub async fn fsck(&self, org: OrgId) -> Result<FsckReport> {
        let opts = FsckOptions {
            eager: vec![graph_type()],
            ..FsckOptions::default()
        };
        fsck(self.meta.as_ref(), self.blobs.as_ref(), org, &opts).await
    }
}

/// Waits briefly (used by tests that need wall time to pass).
pub async fn pause(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Generates one `#[tokio::test]` per backend for a generic test function
/// `async fn name<S: Open>()`. The Postgres variant returns early when `BEE_TEST_PG` is
/// unset (the function itself sees `Env::new()` return `None`).
#[macro_export]
macro_rules! both {
    ($name:ident) => {
        both!($name, "current_thread");
    };
    ($name:ident, multi) => {
        mod $name {
            #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
            async fn sqlite() {
                super::$name::<bee_store_sql::SqliteMetaStore>().await;
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
            async fn postgres() {
                super::$name::<bee_store_sql::PgMetaStore>().await;
            }
        }
    };
    ($name:ident, "current_thread") => {
        mod $name {
            #[tokio::test]
            async fn sqlite() {
                super::$name::<bee_store_sql::SqliteMetaStore>().await;
            }
            #[tokio::test]
            async fn postgres() {
                super::$name::<bee_store_sql::PgMetaStore>().await;
            }
        }
    };
}

/// Builds an environment or returns from the test (Postgres not configured).
#[macro_export]
macro_rules! env_or_skip {
    ($s:ty) => {
        env_or_skip!($s, 1)
    };
    ($s:ty, $seed:expr) => {
        match $crate::common::Env::<$s>::with_seed($seed).await {
            Some(e) => e,
            None => {
                eprintln!("skipped: BEE_TEST_PG is not set");
                return;
            }
        }
    };
}
