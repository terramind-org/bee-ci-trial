//! [`History`]: the per-repo handle every query and write helper hangs off, with the reads
//! they share (changesets, trees, blobs, metas, derived rows) and small caches.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use bee_core::{BeeError, Mode, ObjectId, ObjectKind, Reason, RepoPath, Result};
use bee_object::{Changeset, DeltaEntry, Meta, Tree, TreeEntry};
use bee_store::trees::read_object_bytes;
use bee_store::{
    BlobStore, DerivedRow, MetaStore, MinVersion, OrgId, ReadFrom, RepoId, StagedMeta, Writer,
    consistency,
};

use crate::eager::{CoreEager, EagerRows};
use crate::graph::GraphNode;

/// Cached trees and changesets are dropped wholesale past this many entries.
const CACHE_LIMIT: usize = 8192;

/// Changesets and graph nodes read for one repo, shared by every [`History`] handle on it
/// (see [`History::with_shared_rows`]). Both are immutable under their id, so a row read by
/// one request stays right for the next. Trees are not shared: a tree can be redacted.
#[derive(Debug, Default)]
pub struct SharedRows {
    changesets: Mutex<BTreeMap<ObjectId, Changeset>>,
    graph_nodes: Mutex<BTreeMap<ObjectId, GraphNode>>,
}

/// A blob's content as history sees it: redacted bytes read as absent (SPEC §13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobContent {
    /// The bytes.
    Bytes(Vec<u8>),
    /// The blob was redacted; its bytes are gone.
    Redacted,
}

impl BlobContent {
    /// The bytes, with redacted content reading as empty.
    pub fn bytes_or_empty(&self) -> &[u8] {
        match self {
            BlobContent::Bytes(b) => b,
            BlobContent::Redacted => &[],
        }
    }

    /// True when redacted.
    pub fn is_redacted(&self) -> bool {
        matches!(self, BlobContent::Redacted)
    }
}

/// The history engine for one repository: queries (log, blame, diff, ancestry, merge base,
/// revisions, read path, list tree) and the tree-level write helpers (create, rebase, merge,
/// land) the server's `QueryService` and `WriteService` call.
///
/// Cheap to create per request. It holds small read caches of immutable rows (trees and
/// changesets never change under an id), so a long-lived handle is also fine.
pub struct History {
    writer: Writer,
    org: OrgId,
    repo: RepoId,
    min: Option<MinVersion>,
    eager: Arc<dyn EagerRows>,
    trees: Mutex<BTreeMap<ObjectId, Tree>>,
    rows: Arc<SharedRows>,
    /// Metas read through this handle. Per handle only: a meta object can be redacted.
    metas: Mutex<BTreeMap<ObjectId, Option<Meta>>>,
    /// The write buffer of a [`History::buffered`] handle.
    staged: Option<Arc<StagedMeta>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `NOT_FOUND` for a changeset the repo does not have.
pub fn changeset_not_found(id: &ObjectId) -> BeeError {
    BeeError::new(Reason::NotFound, format!("changeset {id} not found"))
        .with_payload("object", id.to_text())
}

impl History {
    /// A handle over `writer`'s stores for one repo, writing the core eager rows (`graph`,
    /// `path_history`) for every changeset it creates.
    pub fn new(writer: Writer, org: OrgId, repo: RepoId) -> History {
        History {
            writer,
            org,
            repo,
            min: None,
            eager: Arc::new(CoreEager),
            trees: Mutex::new(BTreeMap::new()),
            rows: Arc::default(),
            metas: Mutex::new(BTreeMap::new()),
            staged: None,
        }
    }

    /// This handle with its object and changeset writes buffered in memory
    /// ([`StagedMeta`]): everything it creates (trees, changesets, eager rows) is readable
    /// through it at once, and [`History::flush_buffered`] writes it all in one transaction.
    /// A land coalescer rebases a batch of stacks, each onto the last, over one buffered
    /// handle and flushes before its ref update. Results are the same as unbuffered.
    pub fn buffered(mut self) -> History {
        if self.staged.is_none() {
            // Rows read through the buffer may never be written: keep them to this handle.
            self.rows = Arc::default();
            let (writer, staged) = self.writer.buffered();
            self.writer = writer;
            self.staged = Some(staged);
        }
        self
    }

    /// Writes what a [`History::buffered`] handle created (no-op otherwise, or when nothing
    /// is buffered). Must complete before anything made reachable by a ref update.
    pub async fn flush_buffered(&self) -> Result<()> {
        match &self.staged {
            Some(s) => s.flush().await,
            None => Ok(()),
        }
    }

    pub(crate) fn cached_graph_node(&self, id: &ObjectId) -> Option<GraphNode> {
        lock(&self.rows.graph_nodes).get(id).cloned()
    }

    pub(crate) fn cache_graph_node(&self, id: ObjectId, node: GraphNode) {
        let mut cache = lock(&self.rows.graph_nodes);
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(id, node);
    }

    /// This handle sharing `rows` with every other handle given the same rows. A buffered
    /// handle keeps its own.
    pub fn with_shared_rows(mut self, rows: Arc<SharedRows>) -> History {
        if self.staged.is_none() {
            self.rows = rows;
        }
        self
    }

    /// Reads immutable rows from replicas that have reached `min` (SPEC §6.3), falling back to
    /// the primary on a miss.
    pub fn with_min_version(mut self, min: Option<MinVersion>) -> History {
        self.min = min;
        self
    }

    /// Replaces the eager-row hook used when this handle creates changesets (for example the
    /// `bee-derived` registry, which adds any extra eager types it knows).
    pub fn with_eager(mut self, eager: Arc<dyn EagerRows>) -> History {
        self.eager = eager;
        self
    }

    /// The writer.
    pub fn writer(&self) -> &Writer {
        &self.writer
    }

    /// The metadata store.
    pub fn meta(&self) -> &dyn MetaStore {
        self.writer.meta().as_ref()
    }

    /// The blob store.
    pub fn blobs(&self) -> &dyn BlobStore {
        self.writer.blobs().as_ref()
    }

    /// The org.
    pub fn org(&self) -> OrgId {
        self.org
    }

    /// The repo.
    pub fn repo(&self) -> RepoId {
        self.repo
    }

    /// The eager-row hook.
    pub fn eager(&self) -> &Arc<dyn EagerRows> {
        &self.eager
    }

    fn read_from(&self) -> ReadFrom {
        ReadFrom::Replica(self.min.clone())
    }

    /// Reads a changeset of this repo (`NOT_FOUND` when absent).
    pub async fn changeset(&self, id: &ObjectId) -> Result<Changeset> {
        if let Some(c) = lock(&self.rows.changesets).get(id) {
            return Ok(c.clone());
        }
        if id.kind() != ObjectKind::Changeset {
            return Err(changeset_not_found(id));
        }
        let row = consistency::changeset(self.meta(), self.repo, id, self.min.clone()).await?;
        let mut cache = lock(&self.rows.changesets);
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(*id, row.changeset.clone());
        Ok(row.changeset)
    }

    /// True when the repo has the changeset.
    pub async fn has_changeset(&self, id: &ObjectId) -> Result<bool> {
        match self.changeset(id).await {
            Ok(_) => Ok(true),
            Err(e) if e.reason == Reason::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Reads a tree.
    pub async fn tree(&self, id: &ObjectId) -> Result<Tree> {
        if let Some(t) = lock(&self.trees).get(id) {
            return Ok(t.clone());
        }
        let t =
            bee_store::trees::read_tree(self.meta(), self.blobs(), self.org, id, self.read_from())
                .await?;
        let mut cache = lock(&self.trees);
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(*id, t.clone());
        Ok(t)
    }

    /// Reads a blob; a redacted blob is [`BlobContent::Redacted`] rather than an error.
    pub async fn blob(&self, id: &ObjectId) -> Result<BlobContent> {
        match read_object_bytes(self.meta(), self.blobs(), self.org, id, self.read_from()).await {
            Ok(b) => Ok(BlobContent::Bytes(b)),
            Err(e) if e.reason == Reason::Redacted => Ok(BlobContent::Redacted),
            Err(e) => Err(e),
        }
    }

    /// The content of a leaf for line-level work: blob bytes, the hex git id for a gitlink,
    /// and redacted content as [`BlobContent::Redacted`].
    pub async fn leaf_content(&self, mode: Mode, id: &ObjectId) -> Result<BlobContent> {
        if mode == Mode::Gitlink || id.kind() != ObjectKind::Blob {
            return Ok(BlobContent::Bytes(
                bee_core::hex::encode(id.digest()).into_bytes(),
            ));
        }
        self.blob(id).await
    }

    /// Reads the metas of `ids` with one row read into this handle's meta cache, so the
    /// [`History::meta_object`] calls that follow hit it. Only rows that read exactly as
    /// `meta_object` would read them alone are cached (live inline bytes that hash to the
    /// id, or purged); anything else is left for `meta_object` to read and report.
    pub async fn prefetch_metas(&self, ids: &[ObjectId]) -> Result<()> {
        let mut wanted: Vec<ObjectId> = {
            let cache = lock(&self.metas);
            ids.iter()
                .filter(|id| !cache.contains_key(id))
                .copied()
                .collect()
        };
        wanted.sort();
        wanted.dedup();
        if wanted.is_empty() {
            return Ok(());
        }
        let rows = self
            .meta()
            .get_objects(self.org, &wanted, self.read_from())
            .await?;
        let mut cache = lock(&self.metas);
        if cache.len() + wanted.len() >= CACHE_LIMIT {
            cache.clear();
        }
        for (id, row) in wanted.iter().zip(rows) {
            let Some(row) = row else { continue };
            let meta = match (row.state, row.inline) {
                (bee_store::ObjectState::Purged, _) => None,
                (bee_store::ObjectState::Live, Some(b))
                    if bee_object::hash_id(id.kind(), &b) == *id =>
                {
                    match Meta::decode(&b) {
                        Ok(m) => Some(m),
                        Err(_) => continue,
                    }
                }
                _ => continue,
            };
            cache.insert(*id, meta);
        }
        Ok(())
    }

    /// Reads a meta; `None` when it was redacted.
    pub async fn meta_object(&self, id: &ObjectId) -> Result<Option<Meta>> {
        if let Some(m) = lock(&self.metas).get(id) {
            return Ok(m.clone());
        }
        let meta = match read_object_bytes(
            self.meta(),
            self.blobs(),
            self.org,
            id,
            self.read_from(),
        )
        .await
        {
            Ok(b) => Meta::decode(&b).map(Some)?,
            Err(e) if e.reason == Reason::Redacted => None,
            Err(e) => return Err(e),
        };
        let mut cache = lock(&self.metas);
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(*id, meta.clone());
        Ok(meta)
    }

    /// Reads a derived value (replica first, then primary).
    pub async fn derived(
        &self,
        key: &ObjectId,
        type_name: &str,
        version: u32,
    ) -> Result<Option<Vec<u8>>> {
        consistency::derived(
            self.meta(),
            self.repo,
            key,
            type_name,
            version,
            self.min.clone(),
        )
        .await
    }

    /// Writes derived rows, insert-or-ignore.
    pub async fn put_derived(&self, rows: Vec<DerivedRow>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.meta().put_derived(self.repo, rows).await
    }

    /// The entry at `path` under the tree `root`; `None` when absent (or below a file).
    pub async fn entry_at(&self, root: &ObjectId, path: &RepoPath) -> Result<Option<TreeEntry>> {
        let mut current = *root;
        let components: Vec<&[u8]> = path.components().collect();
        for (i, name) in components.iter().enumerate() {
            let tree = self.tree(&current).await?;
            let Some(e) = tree.get(name) else {
                return Ok(None);
            };
            if i + 1 == components.len() {
                return Ok(Some(e.clone()));
            }
            if e.mode != Mode::Tree {
                return Ok(None);
            }
            current = e.id;
        }
        Ok(None)
    }

    /// Every leaf under the tree `id` (whose path is `prefix`), sorted by path. Iterative.
    pub async fn list_leaves(
        &self,
        id: &ObjectId,
        prefix: Option<&RepoPath>,
    ) -> Result<Vec<(RepoPath, TreeEntry)>> {
        let mut out = Vec::new();
        let mut stack: Vec<(Option<RepoPath>, ObjectId)> = vec![(prefix.cloned(), *id)];
        while let Some((dir, tid)) = stack.pop() {
            let tree = self.tree(&tid).await?;
            for e in tree.into_entries() {
                let path = child(dir.as_ref(), &e.name)?;
                if e.mode == Mode::Tree {
                    stack.push((Some(path), e.id));
                } else {
                    out.push((path, e));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// The exact file-level difference between two trees (either may be the empty tree),
    /// sorted by path; equal subtrees are skipped unread. `prefix` is the path both trees sit
    /// at (`None` for roots). Iterative.
    pub async fn diff_trees(
        &self,
        old: Option<&ObjectId>,
        new: Option<&ObjectId>,
        prefix: Option<&RepoPath>,
    ) -> Result<Vec<DeltaEntry>> {
        let mut out = Vec::new();
        let mut stack: Vec<(Option<RepoPath>, Option<ObjectId>, Option<ObjectId>)> =
            vec![(prefix.cloned(), old.copied(), new.copied())];
        let empty = Tree::empty();
        while let Some((dir, o, n)) = stack.pop() {
            if o == n {
                continue;
            }
            let ot = match o {
                Some(id) => self.tree(&id).await?,
                None => empty.clone(),
            };
            let nt = match n {
                Some(id) => self.tree(&id).await?,
                None => empty.clone(),
            };
            let (oe, ne) = (ot.entries(), nt.entries());
            let (mut i, mut j) = (0, 0);
            while i < oe.len() || j < ne.len() {
                let order = match (oe.get(i), ne.get(j)) {
                    (Some(a), Some(b)) => a.name.cmp(&b.name),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    _ => std::cmp::Ordering::Greater,
                };
                let (a, b) = match order {
                    std::cmp::Ordering::Less => {
                        i += 1;
                        (Some(&oe[i - 1]), None)
                    }
                    std::cmp::Ordering::Greater => {
                        j += 1;
                        (None, Some(&ne[j - 1]))
                    }
                    std::cmp::Ordering::Equal => {
                        i += 1;
                        j += 1;
                        (Some(&oe[i - 1]), Some(&ne[j - 1]))
                    }
                };
                if a == b {
                    continue;
                }
                let name = &a.or(b).expect("one side exists").name;
                let path = child(dir.as_ref(), name)?;
                let old_tree = a.filter(|e| e.mode == Mode::Tree).map(|e| e.id);
                let new_tree = b.filter(|e| e.mode == Mode::Tree).map(|e| e.id);
                if old_tree.is_some() || new_tree.is_some() {
                    stack.push((Some(path.clone()), old_tree, new_tree));
                }
                match (a.filter(|e| e.is_leaf()), b.filter(|e| e.is_leaf())) {
                    (_, Some(nl)) => out.push(DeltaEntry::put(path, nl.mode, nl.id, nl.size)),
                    (Some(_), None) => out.push(DeltaEntry::delete(path)),
                    (None, None) => {}
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// The difference between two root trees restricted to `scope` (the whole tree when
    /// `None`): reads only the trees on the way to `scope` plus the differing subtrees.
    pub async fn diff_scoped(
        &self,
        old_root: Option<&ObjectId>,
        new_root: Option<&ObjectId>,
        scope: Option<&RepoPath>,
    ) -> Result<Vec<DeltaEntry>> {
        let Some(scope) = scope else {
            return self.diff_trees(old_root, new_root, None).await;
        };
        let o = match old_root {
            Some(r) => self.entry_at(r, scope).await?,
            None => None,
        };
        let n = match new_root {
            Some(r) => self.entry_at(r, scope).await?,
            None => None,
        };
        if o == n {
            return Ok(Vec::new());
        }
        let ot = o.as_ref().filter(|e| e.mode == Mode::Tree).map(|e| e.id);
        let nt = n.as_ref().filter(|e| e.mode == Mode::Tree).map(|e| e.id);
        let mut out = if ot.is_some() || nt.is_some() {
            self.diff_trees(ot.as_ref(), nt.as_ref(), Some(scope))
                .await?
        } else {
            Vec::new()
        };
        match (o.filter(|e| e.is_leaf()), n.filter(|e| e.is_leaf())) {
            (_, Some(nl)) => out.push(DeltaEntry::put(scope.clone(), nl.mode, nl.id, nl.size)),
            (Some(_), None) => out.push(DeltaEntry::delete(scope.clone())),
            (None, None) => {}
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
}

/// `prefix/name`, or `name` at the root.
pub fn child(prefix: Option<&RepoPath>, name: &[u8]) -> Result<RepoPath> {
    match prefix {
        Some(p) => p.join(name),
        None => RepoPath::new(name),
    }
}
