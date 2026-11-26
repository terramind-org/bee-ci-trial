//! Shared fixtures: repos committed through the real write path, indexed into a store.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bee_core::{Mode, ObjectId, RepoPath};
use bee_object::blob_id;
use bee_search::search::{CodeHit, CodeSearchRequest, PathAuthorizer, RepoTarget, Searcher};
use bee_search::store::{Document, FileEntry, IndexStore, Manifest};
use bee_search::{Indexer, MemoryIndexStore, lang};
use bee_store::{BlobStore, MetaStore, OrgId, RepoId};
use bee_testkit::{Change, TestEnv, TestRepo, ref_name};

/// A test environment with an index.
pub struct World {
    pub env: TestEnv,
    pub store: Arc<dyn IndexStore>,
    pub indexer: Indexer,
    pub searcher: Searcher,
    pub targets: Vec<RepoTarget>,
    pub repos: BTreeMap<String, TestRepo>,
}

impl World {
    pub fn new() -> World {
        World::with_store(Arc::new(MemoryIndexStore::new()))
    }

    pub fn with_store(store: Arc<dyn IndexStore>) -> World {
        let env = TestEnv::new();
        let meta: Arc<dyn MetaStore> = env.meta.clone();
        let blobs: Arc<dyn BlobStore> = env.blobs.clone();
        let indexer = Indexer::new(store.clone(), meta, blobs);
        World {
            env,
            searcher: Searcher::new(store.clone()),
            store,
            indexer,
            targets: Vec::new(),
            repos: BTreeMap::new(),
        }
    }

    /// Creates `org/repo` with files on `heads/main` and indexes it.
    pub async fn repo(&mut self, org: &str, name: &str, files: &[(&str, &str)]) -> TestRepo {
        let r = self.env.create_repo(org, name).await.unwrap();
        let changes: Vec<Change> = files.iter().map(|(p, c)| Change::put(p, *c)).collect();
        self.env
            .commit_on(&r, &ref_name("heads/main"), &changes)
            .await
            .unwrap();
        self.indexer
            .sync_ref(r.org, r.repo, &ref_name("heads/main"))
            .await
            .unwrap();
        self.targets.push(RepoTarget {
            org: r.org,
            org_name: org.to_string(),
            repo: r.repo,
            repo_name: name.to_string(),
            ref_name: "heads/main".into(),
        });
        self.repos.insert(format!("{org}/{name}"), r);
        r
    }

    /// Commits changes on `heads/main` of a repo and re-indexes it.
    pub async fn change(&self, full: &str, changes: &[Change]) -> bee_search::IndexStats {
        let r = self.repos[full];
        self.env
            .commit_on(&r, &ref_name("heads/main"), changes)
            .await
            .unwrap();
        self.indexer
            .sync_ref(r.org, r.repo, &ref_name("heads/main"))
            .await
            .unwrap()
    }

    pub fn search(&self, q: &str, auth: &dyn PathAuthorizer) -> Vec<CodeHit> {
        all_hits(&self.searcher, q, &self.targets, auth)
    }
}

pub fn allow_all(_: RepoId, _: &RepoPath) -> bool {
    true
}

/// Every hit of a query, following page tokens.
pub fn all_hits(
    searcher: &Searcher,
    q: &str,
    targets: &[RepoTarget],
    auth: &dyn PathAuthorizer,
) -> Vec<CodeHit> {
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let resp = searcher
            .search_code(
                &CodeSearchRequest {
                    query: q.to_string(),
                    page_size: 7,
                    page_token: token.clone(),
                    max_line_matches_per_file: 10_000,
                    ..CodeSearchRequest::default()
                },
                targets,
                auth,
            )
            .unwrap_or_else(|e| panic!("{q}: {e}"));
        out.extend(resp.hits);
        match resp.next_page_token {
            Some(t) => token = t,
            None => break,
        }
    }
    out
}

/// Builds a manifest directly from in-memory files (no metadata store), for corpora.
pub fn index_files(
    store: &dyn IndexStore,
    org: OrgId,
    repo: RepoId,
    files: &BTreeMap<String, Vec<u8>>,
) -> RepoTarget {
    let docs: Vec<Document> = files
        .values()
        .map(|c| Document::from_bytes(blob_id(c), c.clone()))
        .collect();
    let nums = store.add_documents(org, docs).unwrap();
    let mut map = BTreeMap::new();
    for ((path, content), n) in files.iter().zip(nums) {
        map.insert(
            RepoPath::new(path.as_bytes()).unwrap(),
            FileEntry {
                blob: blob_id(content),
                mode: Mode::File,
                size: content.len() as u64,
                doc: Some(n),
                language: lang::detect(path.as_bytes(), content),
            },
        );
    }
    let changeset: ObjectId = bee_object::Changeset::empty_root().id();
    store
        .put_manifest(Manifest {
            org,
            repo,
            ref_name: "heads/main".into(),
            changeset,
            files: map,
        })
        .unwrap();
    RepoTarget {
        org,
        org_name: "org".into(),
        repo,
        repo_name: format!("r{}", repo.0.as_u128()),
        ref_name: "heads/main".into(),
    }
}
