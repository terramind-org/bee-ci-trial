//! Shared test helpers: commits that go through the real write path ([`History`] over the
//! testkit's fault-wrapped in-memory stores) with the core eager rows.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bee_core::{ChangeId, ObjectId, RefName, RepoPath};
use bee_history::History;
use bee_object::{Meta, Provenance, Timestamp, blob_id};
use bee_store::{AccessSnapshot, Expected, RefUpdate, UploadRecord};
use bee_testkit::fixtures::apply_changes;
use bee_testkit::model::tree::{self as mtree, FileMap};
use bee_testkit::{Change, TestEnv, TestRepo};

/// One commit's description.
#[derive(Clone, Debug, Default)]
pub struct Spec {
    pub parents: Vec<ObjectId>,
    pub changes: Vec<Change>,
    pub author: Option<String>,
    pub message: Option<String>,
    pub provenance: Option<Provenance>,
    pub change_id: Option<ChangeId>,
}

pub struct Env {
    pub env: TestEnv,
    pub repo: TestRepo,
    snapshots: Mutex<BTreeMap<ObjectId, FileMap>>,
    counter: Mutex<u64>,
}

pub fn path(p: &str) -> RepoPath {
    RepoPath::new(p.as_bytes()).unwrap()
}

pub fn put(p: &str, data: &str) -> Change {
    Change::put(p, data.as_bytes())
}

pub fn del(p: &str) -> Change {
    Change::delete(p)
}

impl Env {
    pub async fn new() -> Env {
        Env::with_seed(1).await
    }

    pub async fn with_seed(seed: u64) -> Env {
        let env = TestEnv::with_seed(seed);
        let repo = env.create_repo("acme", "repo").await.unwrap();
        Env {
            env,
            repo,
            snapshots: Mutex::new(BTreeMap::new()),
            counter: Mutex::new(0),
        }
    }

    /// A fresh handle (empty caches), so access counts measure one operation.
    pub fn history(&self) -> History {
        History::new(self.env.writer.clone(), self.repo.org, self.repo.repo)
    }

    pub fn counters(&self) -> AccessSnapshot {
        self.env.counters.snapshot()
    }

    pub fn snapshot(&self, id: &ObjectId) -> FileMap {
        self.snapshots
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Records the snapshot of a changeset created elsewhere (rebase, merge).
    pub fn remember(&self, id: ObjectId, files: FileMap) {
        self.snapshots.lock().unwrap().insert(id, files);
    }

    pub async fn upload_blobs(&self, changes: &[Change]) {
        let mut blobs = BTreeMap::new();
        for c in changes {
            if let Change::Put { data, .. } = c {
                blobs.insert(blob_id(data), data.clone());
            }
        }
        if !blobs.is_empty() {
            self.env
                .writer
                .upload(
                    self.repo.org,
                    blobs.into_iter().collect(),
                    Some(UploadRecord {
                        repo: Some(self.repo.repo),
                        principal: "agent:test".into(),
                    }),
                    self.env.now(),
                )
                .await
                .unwrap();
        }
    }

    pub async fn upload_meta(&self, meta: &Meta) -> ObjectId {
        let id = meta.id();
        self.env
            .writer
            .upload(
                self.repo.org,
                vec![(id, meta.encode())],
                None,
                self.env.now(),
            )
            .await
            .unwrap();
        id
    }

    pub async fn meta(
        &self,
        author: &str,
        message: &str,
        provenance: Option<Provenance>,
    ) -> ObjectId {
        let mut m = Meta::new(
            author,
            Timestamp::new(self.env.clock.now_secs(), 0),
            "agent:test",
            message,
        );
        m.provenance = provenance;
        self.upload_meta(&m).await
    }

    pub async fn commit_spec(&self, spec: Spec) -> ObjectId {
        let n = {
            let mut c = self.counter.lock().unwrap();
            *c += 1;
            *c
        };
        let base = spec
            .parents
            .first()
            .map(|p| self.snapshot(p))
            .unwrap_or_default();
        let files = apply_changes(&base, &spec.changes).unwrap();
        let delta = mtree::diff(&base, &files);
        self.upload_blobs(&spec.changes).await;
        let meta = self
            .meta(
                spec.author.as_deref().unwrap_or("Test Agent <agent@test>"),
                &spec.message.unwrap_or_else(|| format!("commit {n}")),
                spec.provenance,
            )
            .await;
        let change_id = spec
            .change_id
            .unwrap_or_else(|| self.env.with_rng(|r| r.change_id()));
        let id = self
            .history()
            .create_changeset(spec.parents, meta, change_id, delta, self.env.now())
            .await
            .unwrap();
        self.remember(id, files);
        id
    }

    pub async fn commit(&self, parents: &[ObjectId], changes: &[Change]) -> ObjectId {
        self.commit_spec(Spec {
            parents: parents.to_vec(),
            changes: changes.to_vec(),
            ..Spec::default()
        })
        .await
    }

    /// Points `name` at `to` (creating or moving it, no precondition beyond the current
    /// version).
    pub async fn set_ref(&self, name: &str, to: ObjectId) -> u64 {
        let name = RefName::new(name).unwrap();
        let cur = self.env.meta.get_ref_version(self.repo.repo, &name).await;
        let expected = match cur {
            Some(v) => Expected::Version(v),
            None => Expected::Absent,
        };
        self.env
            .writer
            .update_ref(RefUpdate::set(
                self.repo.repo,
                name,
                expected,
                Some(to),
                "agent:test",
                self.env.now(),
            ))
            .await
            .unwrap()
            .new_version
    }
}

/// Small extension so tests can read a ref's version.
pub trait RefVersion {
    fn get_ref_version(
        &self,
        repo: bee_store::RepoId,
        name: &RefName,
    ) -> impl std::future::Future<Output = Option<u64>>;
}

impl<T: bee_store::MetaStore + ?Sized> RefVersion for T {
    async fn get_ref_version(&self, repo: bee_store::RepoId, name: &RefName) -> Option<u64> {
        self.get_ref(repo, name)
            .await
            .unwrap()
            .filter(|r| r.target.is_some() || r.version > 0)
            .map(|r| r.version)
    }
}

/// Reads a file's content in a snapshot.
pub fn file_id(files: &FileMap, p: &str) -> Option<ObjectId> {
    files.get(p.as_bytes()).map(|l| l.id)
}

pub fn arc_env(env: Env) -> Arc<Env> {
    Arc::new(env)
}
