//! Fixture builders: a test environment over the in-memory stores, repos, and commits that
//! go through the real write path ([`bee_store::Writer`]) with fault points armed.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use bee_core::{BeeError, ChangeId, Mode, ObjectId, Reason, RefName, RepoPath, Result};
use bee_object::{DeltaEntry, Meta, Timestamp, blob_id, digest};
use bee_store::{
    AccessCounters, DEFAULT_DRAFT_RETENTION_DAYS, DerivedRow, EagerType, Expected, FaultInjector,
    FaultyBlob, FaultyMeta, FsckOptions, FsckReport, MemoryBlobStore, MemoryMetaStore, MetaStore,
    OrgId, OrgRow, ReadFrom, RefUpdate, RefUpdated, RepoId, RepoRow, UploadRecord, Writer, fsck,
};

use crate::clock::TestClock;
use crate::model::tree::{self as mtree, FileMap, Leaf};
use crate::rng::TestRng;

/// The eager derived type the fixtures write for every changeset: the generation number
/// (1 for a root, else 1 + the largest parent generation) as 8 big-endian bytes. A stand-in
/// for the `graph` type until `bee-derived` defines the real encoding.
pub fn graph_type() -> EagerType {
    EagerType::new("graph", 1)
}

/// A file change for [`TestEnv::commit`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Write a leaf; replaces a file at an ancestor path or a directory at this path.
    Put {
        /// Path.
        path: String,
        /// Mode (file, executable or symlink).
        mode: Mode,
        /// Contents.
        data: Vec<u8>,
    },
    /// Remove a file, or every file under a directory.
    Delete {
        /// Path.
        path: String,
    },
}

impl Change {
    /// A regular file.
    pub fn put(path: &str, data: impl Into<Vec<u8>>) -> Change {
        Change::Put {
            path: path.to_string(),
            mode: Mode::File,
            data: data.into(),
        }
    }

    /// A deletion.
    pub fn delete(path: &str) -> Change {
        Change::Delete {
            path: path.to_string(),
        }
    }
}

fn is_inside(dir: &[u8], path: &[u8]) -> bool {
    path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
}

/// Applies changes to a snapshot the way a working copy would.
pub fn apply_changes(base: &FileMap, changes: &[Change]) -> Result<FileMap> {
    let mut files = base.clone();
    for c in changes {
        match c {
            Change::Put { path, mode, data } => {
                let p = RepoPath::new(path.as_bytes())?;
                let bytes = p.as_bytes().to_vec();
                for anc in p.ancestors() {
                    files.remove(anc.as_bytes());
                }
                files.retain(|k, _| !is_inside(&bytes, k));
                files.insert(
                    bytes,
                    Leaf {
                        mode: *mode,
                        id: blob_id(data),
                        size: data.len() as u64,
                    },
                );
            }
            Change::Delete { path } => {
                let p = RepoPath::new(path.as_bytes())?;
                let bytes = p.as_bytes().to_vec();
                if files.remove(&bytes).is_none() {
                    files.retain(|k, _| !is_inside(&bytes, k));
                }
            }
        }
    }
    Ok(files)
}

/// What to commit.
#[derive(Clone, Debug, Default)]
pub struct CommitSpec {
    /// Parents; the first one's snapshot is the base.
    pub parents: Vec<ObjectId>,
    /// File changes against the first parent.
    pub changes: Vec<Change>,
    /// Message (part of the meta).
    pub message: String,
    /// ChangeId; derived from the other inputs when `None`, so a retry builds the same id.
    pub change_id: Option<ChangeId>,
}

/// A repo created by [`TestEnv::create_repo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestRepo {
    /// Its org.
    pub org: OrgId,
    /// The repo.
    pub repo: RepoId,
}

/// In-memory stores with fault injection, a deterministic clock and RNG, and a writer.
pub struct TestEnv {
    /// The clock every store and fixture uses.
    pub clock: Arc<TestClock>,
    /// Faults for store calls and write-ordering steps.
    pub faults: Arc<FaultInjector>,
    /// Counters shared by both stores.
    pub counters: Arc<AccessCounters>,
    /// The metadata store (faulty wrapper around the in-memory store).
    pub meta: Arc<FaultyMeta<MemoryMetaStore>>,
    /// The blob store (faulty wrapper around the in-memory store).
    pub blobs: Arc<FaultyBlob<MemoryBlobStore>>,
    /// The writer over both.
    pub writer: Writer,
    rng: Mutex<TestRng>,
    snapshots: Mutex<BTreeMap<ObjectId, FileMap>>,
}

impl Default for TestEnv {
    fn default() -> Self {
        TestEnv::new()
    }
}

impl TestEnv {
    /// A fresh environment with seed 1.
    pub fn new() -> TestEnv {
        TestEnv::with_seed(1)
    }

    /// A fresh environment whose ids come from `seed`.
    pub fn with_seed(seed: u64) -> TestEnv {
        let clock = Arc::new(TestClock::default());
        let faults = FaultInjector::new();
        let counters = Arc::new(AccessCounters::new());
        let meta = Arc::new(FaultyMeta::new(
            MemoryMetaStore::with_counters(counters.clone()),
            faults.clone(),
        ));
        let blobs = Arc::new(FaultyBlob::new(
            MemoryBlobStore::with_counters(clock.clone(), counters.clone()),
            faults.clone(),
        ));
        let writer = Writer::new(meta.clone(), blobs.clone()).with_faults(faults.clone());
        TestEnv {
            clock,
            faults,
            counters,
            meta,
            blobs,
            writer,
            rng: Mutex::new(TestRng::seed(seed)),
            snapshots: Mutex::new(BTreeMap::new()),
        }
    }

    /// The in-memory metadata store (test hooks live there).
    pub fn mem(&self) -> &MemoryMetaStore {
        self.meta.inner()
    }

    /// The in-memory blob store.
    pub fn mem_blobs(&self) -> &MemoryBlobStore {
        self.blobs.inner()
    }

    /// The metadata store as a trait object.
    pub fn meta_dyn(&self) -> &dyn MetaStore {
        self.meta.as_ref()
    }

    /// The current time.
    pub fn now(&self) -> u64 {
        self.clock.now()
    }

    /// Draws from the environment's RNG.
    pub fn with_rng<T>(&self, f: impl FnOnce(&mut TestRng) -> T) -> T {
        f(&mut self.rng.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Creates (or reuses) an org and creates a repo in it.
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

    /// The snapshot of a changeset this environment created.
    pub fn snapshot(&self, id: &ObjectId) -> Option<FileMap> {
        self.snapshots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// Commits a draft (no ref) on `parent` (or as a root) with `changes`.
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

    /// Commits through the full write path: uploads blobs, uploads the meta, builds trees
    /// from the delta, uploads them, inserts the changeset with its `object_paths` and graph
    /// row. Deterministic: the same spec at the same clock time gives the same id.
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

    /// Moves `name` from `from` to `to`. Idempotent: if the ref already targets `to` it
    /// returns its current version without writing (a retry after "response lost").
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

    /// Commits `changes` on top of `name`'s target and advances the ref to the result.
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

    /// fsck of an org, requiring the fixtures' graph rows.
    pub async fn fsck(&self, org: OrgId) -> Result<FsckReport> {
        let opts = FsckOptions {
            eager: vec![graph_type()],
            ..FsckOptions::default()
        };
        fsck(self.meta.as_ref(), self.blobs.as_ref(), org, &opts).await
    }
}

/// A ref name, panicking on invalid input (for tests).
pub fn ref_name(name: &str) -> RefName {
    RefName::new(name).expect("valid ref name")
}

/// Silences the default panic message for injected crashes (other panics still print).
pub fn quiet_injected_crashes() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let msg = info
                .payload()
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| info.payload().downcast_ref::<&str>().copied())
                .unwrap_or("");
            if !bee_store::faults::is_injected_crash(msg) {
                default(info);
            }
        }));
    });
}
