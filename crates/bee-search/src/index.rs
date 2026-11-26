//! Incremental indexing of refs from the metadata and blob stores.
//!
//! [`Indexer::index_ref`] brings the manifest of one `(repo, ref)` to a target changeset:
//!
//! 1. Walk first parents from the target back to the manifest's changeset (or to a root),
//!    at most [`Indexer::with_max_delta_walk`] steps, and replay those changesets' deltas
//!    (SPEC §4.4 invariant D: each delta is the exact file-level difference from the first
//!    parent). Only when the walk does not connect does it fall back to listing the target's
//!    whole tree.
//! 2. For every path whose blob or mode changed, look the blob up in the org's index and
//!    fetch and index it only if it is new. A blob shared with another repo, ref or path of
//!    the org is indexed once.
//! 3. Write new documents, then the manifest (documents before the row that refers to them).
//!
//! Everything is a pure function of the stores' contents: re-running is a no-op, and
//! rebuilding from scratch yields the same manifests.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bee_core::{BeeError, Mode, ObjectId, Reason, RefName, RepoPath, Result};
use bee_object::{Changeset, DeltaOp};
use bee_store::trees::{read_object_bytes, read_tree};
use bee_store::{BlobStore, MetaStore, OrgId, ReadFrom, RepoId};

use crate::lang::{self, Language};
use crate::store::{
    DocNum, Document, FileEntry, IndexStore, MAX_INDEXED_BYTES, Manifest, SkipReason,
};

/// Default first-parent steps replayed before falling back to a full tree listing.
pub const DEFAULT_MAX_DELTA_WALK: usize = 1024;

/// What one indexing run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// The changeset the manifest now reflects (`None` when the ref was removed).
    pub changeset: Option<ObjectId>,
    /// Paths added, changed or removed relative to the previous manifest.
    pub paths_changed: usize,
    /// Changesets whose deltas were replayed.
    pub deltas_replayed: usize,
    /// Blobs read from the blob store and newly indexed.
    pub blobs_indexed: usize,
    /// Changed paths whose blob was already indexed in the org (dedup).
    pub blobs_reused: usize,
    /// True when the delta walk did not connect and the whole tree was listed.
    pub full_listing: bool,
}

type Leaf = (Mode, ObjectId, u64);

/// Indexes refs into an [`IndexStore`].
#[derive(Clone)]
pub struct Indexer {
    store: Arc<dyn IndexStore>,
    meta: Arc<dyn MetaStore>,
    blobs: Arc<dyn BlobStore>,
    max_delta_walk: usize,
}

impl Indexer {
    /// An indexer over the given stores.
    pub fn new(
        store: Arc<dyn IndexStore>,
        meta: Arc<dyn MetaStore>,
        blobs: Arc<dyn BlobStore>,
    ) -> Indexer {
        Indexer {
            store,
            meta,
            blobs,
            max_delta_walk: DEFAULT_MAX_DELTA_WALK,
        }
    }

    /// Sets the longest first-parent walk replayed as deltas.
    pub fn with_max_delta_walk(mut self, steps: usize) -> Indexer {
        self.max_delta_walk = steps;
        self
    }

    /// The index store.
    pub fn store(&self) -> &Arc<dyn IndexStore> {
        &self.store
    }

    /// Indexes a ref at its current target in the metadata store (removing the manifest
    /// when the ref is absent or deleted).
    pub async fn sync_ref(&self, org: OrgId, repo: RepoId, name: &RefName) -> Result<IndexStats> {
        let target = self.meta.get_ref(repo, name).await?.and_then(|r| r.target);
        self.index_ref(org, repo, name.as_str(), target).await
    }

    /// Indexes every ref of a repo whose name starts with `prefix` (e.g. `heads/`), and
    /// removes manifests of refs that no longer exist. Returns stats per ref, ordered.
    pub async fn sync_repo(
        &self,
        org: OrgId,
        repo: RepoId,
        prefix: &str,
    ) -> Result<Vec<(String, IndexStats)>> {
        let refs = self.meta.list_refs(repo, prefix).await?;
        let mut out = Vec::new();
        let mut live = BTreeSet::new();
        for r in refs {
            let name = r.name.as_str().to_string();
            live.insert(name.clone());
            out.push((
                name.clone(),
                self.index_ref(org, repo, &name, r.target).await?,
            ));
        }
        for (rid, name) in self.store.list_manifests(org)? {
            if rid == repo && name.starts_with(prefix) && !live.contains(&name) {
                self.store.delete_manifest(org, repo, &name)?;
                out.push((name, IndexStats::default()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Brings the manifest of `(repo, ref_name)` to `target` (or deletes it for `None`).
    pub async fn index_ref(
        &self,
        org: OrgId,
        repo: RepoId,
        ref_name: &str,
        target: Option<ObjectId>,
    ) -> Result<IndexStats> {
        let old = self.store.manifest(org, repo, ref_name)?;
        let Some(target) = target else {
            let paths_changed = old.as_ref().map_or(0, |m| m.files.len());
            self.store.delete_manifest(org, repo, ref_name)?;
            return Ok(IndexStats {
                paths_changed,
                ..IndexStats::default()
            });
        };
        let mut stats = IndexStats {
            changeset: Some(target),
            ..IndexStats::default()
        };
        if old.as_ref().is_some_and(|m| m.changeset == target) {
            return Ok(stats);
        }
        let old_files: BTreeMap<RepoPath, FileEntry> =
            old.as_ref().map(|m| m.files.clone()).unwrap_or_default();
        let new_leaves = self
            .target_leaves(org, repo, target, old.as_deref(), &old_files, &mut stats)
            .await?;

        // Paths whose content changed and need a document.
        let mut sizes: BTreeMap<ObjectId, u64> = BTreeMap::new();
        for (path, (mode, id, size)) in &new_leaves {
            let same = old_files
                .get(path)
                .is_some_and(|f| f.blob == *id && f.mode == *mode);
            if !same && mode.is_blob() && *mode != Mode::Symlink {
                sizes.insert(*id, *size);
            }
        }
        let need: Vec<ObjectId> = sizes.keys().copied().collect();
        let found = self.store.lookup(org, &need)?;
        let mut docs_by_blob: BTreeMap<ObjectId, DocNum> = BTreeMap::new();
        let mut contents: BTreeMap<ObjectId, Arc<Vec<u8>>> = BTreeMap::new();
        let mut fresh = Vec::new();
        for (id, f) in need.iter().zip(found) {
            match f {
                Some(n) => {
                    docs_by_blob.insert(*id, n);
                }
                None => {
                    let size = sizes.get(id).copied().unwrap_or(0);
                    let doc = if size > MAX_INDEXED_BYTES {
                        Document::skipped(*id, SkipReason::TooLarge)
                    } else {
                        let bytes = read_object_bytes(
                            self.meta.as_ref(),
                            self.blobs.as_ref(),
                            org,
                            id,
                            ReadFrom::Primary,
                        )
                        .await?;
                        Document::from_bytes(*id, bytes)
                    };
                    contents.insert(*id, Arc::new(doc.content.clone()));
                    fresh.push(doc);
                }
            }
        }
        stats.blobs_indexed = fresh.len();
        let fresh_ids: Vec<ObjectId> = fresh.iter().map(|d| d.blob).collect();
        let nums = self.store.add_documents(org, fresh)?;
        for (id, n) in fresh_ids.iter().zip(nums) {
            docs_by_blob.insert(*id, n);
        }

        let mut files = BTreeMap::new();
        for (path, (mode, id, size)) in new_leaves {
            if let Some(f) = old_files.get(&path)
                && f.blob == id
                && f.mode == mode
            {
                files.insert(path, f.clone());
                continue;
            }
            stats.paths_changed += 1;
            let doc = docs_by_blob.get(&id).copied();
            if doc.is_some() && !contents.contains_key(&id) {
                stats.blobs_reused += 1;
            }
            let language = self.language(org, &path, doc, contents.get(&id))?;
            files.insert(
                path,
                FileEntry {
                    blob: id,
                    mode,
                    size,
                    doc,
                    language,
                },
            );
        }
        stats.paths_changed += old_files.keys().filter(|p| !files.contains_key(*p)).count();
        self.store.put_manifest(Manifest {
            org,
            repo,
            ref_name: ref_name.to_string(),
            changeset: target,
            files,
        })?;
        Ok(stats)
    }

    fn language(
        &self,
        org: OrgId,
        path: &RepoPath,
        doc: Option<DocNum>,
        content: Option<&Arc<Vec<u8>>>,
    ) -> Result<Option<Language>> {
        if let Some(l) = lang::detect(path.as_bytes(), b"") {
            return Ok(Some(l));
        }
        if let Some(c) = content {
            return Ok(lang::detect(path.as_bytes(), c));
        }
        match doc {
            Some(n) => Ok(self
                .store
                .document(org, n)?
                .and_then(|d| lang::detect(path.as_bytes(), &d.content))),
            None => Ok(None),
        }
    }

    async fn changeset(&self, repo: RepoId, id: &ObjectId) -> Result<Changeset> {
        self.meta
            .get_changeset(repo, id, ReadFrom::Primary)
            .await?
            .map(|r| r.changeset)
            .ok_or_else(|| {
                BeeError::new(Reason::MissingObject, format!("changeset {id} not found"))
                    .with_payload("object", id.to_text())
            })
    }

    /// The target's leaves, by replaying deltas when the first-parent walk connects to the
    /// previous manifest or a root, else by listing the whole tree.
    async fn target_leaves(
        &self,
        org: OrgId,
        repo: RepoId,
        target: ObjectId,
        old: Option<&Manifest>,
        old_files: &BTreeMap<RepoPath, FileEntry>,
        stats: &mut IndexStats,
    ) -> Result<BTreeMap<RepoPath, Leaf>> {
        let stop = old.map(|m| m.changeset);
        let mut chain = Vec::new();
        let mut cur = target;
        let mut base: Option<BTreeMap<RepoPath, Leaf>> = None;
        for _ in 0..self.max_delta_walk {
            if Some(cur) == stop {
                base = Some(
                    old_files
                        .iter()
                        .map(|(p, f)| (p.clone(), (f.mode, f.blob, f.size)))
                        .collect(),
                );
                break;
            }
            let cs = self.changeset(repo, &cur).await?;
            let parent = cs.parents().first().copied();
            chain.push(cs);
            match parent {
                Some(p) => cur = p,
                None => {
                    base = Some(BTreeMap::new());
                    break;
                }
            }
        }
        match base {
            Some(mut leaves) => {
                stats.deltas_replayed = chain.len();
                for cs in chain.iter().rev() {
                    for e in cs.delta() {
                        match e.op {
                            DeltaOp::Put { mode, id, size } => {
                                leaves.insert(e.path.clone(), (mode, id, size));
                            }
                            DeltaOp::Delete => {
                                leaves.remove(&e.path);
                            }
                        }
                    }
                }
                Ok(leaves)
            }
            None => {
                stats.full_listing = true;
                let root = *self.changeset(repo, &target).await?.root();
                self.list_tree(org, root).await
            }
        }
    }

    /// Every leaf under a root tree (iterative, so deep trees cannot overflow the stack).
    async fn list_tree(&self, org: OrgId, root: ObjectId) -> Result<BTreeMap<RepoPath, Leaf>> {
        let mut out = BTreeMap::new();
        let mut stack: Vec<(Option<RepoPath>, ObjectId)> = vec![(None, root)];
        while let Some((prefix, id)) = stack.pop() {
            let tree = read_tree(
                self.meta.as_ref(),
                self.blobs.as_ref(),
                org,
                &id,
                ReadFrom::Primary,
            )
            .await?;
            for e in tree.entries() {
                let path = match &prefix {
                    Some(p) => p.join(&e.name)?,
                    None => RepoPath::new(e.name.clone())?,
                };
                if e.mode == Mode::Tree {
                    stack.push((Some(path), e.id));
                } else {
                    out.insert(path, (e.mode, e.id, e.size));
                }
            }
        }
        Ok(out)
    }

    /// Drops documents no manifest of the org references (after refs were deleted or
    /// rewritten). Returns how many were dropped.
    pub fn collect_garbage(&self, org: OrgId) -> Result<usize> {
        let mut live: BTreeSet<ObjectId> = BTreeSet::new();
        for (repo, name) in self.store.list_manifests(org)? {
            if let Some(m) = self.store.manifest(org, repo, &name)? {
                live.extend(m.files.values().map(|f| f.blob));
            }
        }
        self.store.compact(org, &|b| live.contains(b))
    }
}
