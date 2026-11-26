//! In-process port implementations over the bee stores, for tests, `--dev` and as the
//! reference behaviour of each port.
//!
//! - [`HistoryGit`]: the [`GitPort`] over a [`Writer`] (the VCS stores), producing git ids
//!   with `bee-git`'s exporter, so every id is the one `git clone` through the gateway sees.
//! - [`HistoryTags`]: the forge's [`TagVcs`] over the same refs (`tags/<name>`), so releases
//!   and `refs/tags/*` agree.
//! - [`MemoryState`], [`MemoryAssets`], [`NoAssets`], [`GrepSearch`].

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bee_core::{ChangeId as CoreChangeId, Mode, ObjectId as CoreId, RefName, RepoPath};
use bee_forge::changes::HistoryVcs;
use bee_forge::changes::vcs::ChangeVcs;
use bee_forge::community::ports::{NewAnnotation, TagInfo, TagVcs};
use bee_forge::ids::Clock;
use bee_git::{
    BeeObjectSource, Exporter, GitCommit, GitMapping, GitOid, GitTree, GitTreeEntry, Ident,
    ObjectFormat, ObjectType, RawObject,
};
use bee_history::{BlobContent, History};
use bee_object::{Changeset, DeltaEntry, Meta, Provenance, Timestamp, Tree, blob_id};
use bee_proto::v1::{DiffStatus, ErrorReason, ObjectId, Repo};
use bee_store::{Expected, RefUpdate, UploadRecord, Writer};
use bytes::Bytes;

use crate::error::{ApiError, ApiResult};
use crate::ports::{
    Actor, AssetStore, CodeHit, CodeSearch, Comparison, CompatState, EntryKind, FileChange,
    FileEdit, GitCommitInfo, GitPort, GitRefInfo, NewGitCommit, NewTreeItem, PathEntry, RefMover,
    Signature, TreeItem, TreeListing,
};

/// Most entries in a recursive tree listing before `truncated`.
const TREE_LIMIT: usize = 100_000;

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(500, ErrorReason::Internal, e.to_string())
}

fn core(id: &ObjectId) -> ApiResult<CoreId> {
    CoreId::from_bytes(&id.raw).map_err(ApiError::from)
}

fn proto(id: &CoreId) -> ObjectId {
    ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

/// Bee ref name → git ref name (SPEC §17.2, reversed).
pub fn git_ref_name(bee: &str) -> String {
    if let Some(t) = bee.strip_prefix("tags/") {
        format!("refs/tags/{t}")
    } else if let Some(p) = bee.strip_prefix("pull/") {
        format!("refs/pull/{p}")
    } else if let Some(o) = bee.strip_prefix("git/") {
        format!("refs/{o}")
    } else {
        format!("refs/heads/{bee}")
    }
}

/// Git ref name → bee ref name (SPEC §17.2).
pub fn bee_ref_name(git: &str) -> String {
    if let Some(b) = git.strip_prefix("refs/heads/") {
        b.to_string()
    } else if let Some(t) = git.strip_prefix("refs/tags/") {
        format!("tags/{t}")
    } else if let Some(p) = git.strip_prefix("refs/pull/") {
        format!("pull/{p}")
    } else if let Some(o) = git.strip_prefix("refs/") {
        format!("git/{o}")
    } else {
        git.to_string()
    }
}

/// Bee objects preloaded for the (synchronous) exporter.
#[derive(Default)]
struct Loaded {
    blobs: HashMap<CoreId, Vec<u8>>,
    trees: HashMap<CoreId, Tree>,
    metas: HashMap<CoreId, Meta>,
    changesets: HashMap<CoreId, Changeset>,
}

fn missing(what: &str, id: &CoreId) -> bee_core::BeeError {
    bee_core::BeeError::new(
        bee_core::Reason::MissingObject,
        format!("{what} {id} not loaded"),
    )
}

impl BeeObjectSource for Loaded {
    fn blob(&self, id: &CoreId) -> bee_core::Result<Vec<u8>> {
        self.blobs
            .get(id)
            .cloned()
            .ok_or_else(|| missing("blob", id))
    }
    fn tree(&self, id: &CoreId) -> bee_core::Result<Tree> {
        self.trees
            .get(id)
            .cloned()
            .ok_or_else(|| missing("tree", id))
    }
    fn meta(&self, id: &CoreId) -> bee_core::Result<Meta> {
        self.metas
            .get(id)
            .cloned()
            .ok_or_else(|| missing("meta", id))
    }
    fn changeset(&self, id: &CoreId) -> bee_core::Result<Changeset> {
        self.changesets
            .get(id)
            .cloned()
            .ok_or_else(|| missing("changeset", id))
    }
}

#[derive(Default)]
struct RepoCache {
    mapping: GitMapping,
    objects: HashMap<GitOid, RawObject>,
}

/// The [`GitPort`] over the VCS stores. Git objects are exported on demand and cached;
/// git-data writes (`POST /git/blobs`, `/git/trees`) are held until a commit uses them.
pub struct HistoryGit {
    writer: Writer,
    vcs: Arc<HistoryVcs>,
    clock: Arc<dyn Clock>,
    format: ObjectFormat,
    repos: Mutex<HashMap<String, RepoCache>>,
    mover: Option<Arc<dyn RefMover>>,
}

impl HistoryGit {
    /// Over `writer`, timing new changesets with `clock`.
    pub fn new(writer: Writer, clock: Arc<dyn Clock>) -> HistoryGit {
        HistoryGit {
            vcs: Arc::new(HistoryVcs::new(writer.clone(), clock.clone())),
            writer,
            clock,
            format: ObjectFormat::Sha1,
            repos: Mutex::new(HashMap::new()),
            mover: None,
        }
    }

    /// Moves refs through `mover` (the server's write path) instead of a plain
    /// compare-and-swap on the VCS store.
    pub fn with_ref_mover(mut self, mover: Arc<dyn RefMover>) -> HistoryGit {
        self.mover = Some(mover);
        self
    }

    async fn history(&self, repo: &Repo) -> ApiResult<History> {
        self.vcs.history(repo).await.map_err(ApiError::from)
    }

    fn known(&self, repo: &Repo, id: &CoreId) -> Option<GitOid> {
        self.repos
            .lock()
            .ok()
            .and_then(|m| m.get(&repo.id).and_then(|c| c.mapping.git(id)))
    }

    fn object(&self, repo: &Repo, oid: &GitOid) -> Option<RawObject> {
        self.repos
            .lock()
            .ok()
            .and_then(|m| m.get(&repo.id).and_then(|c| c.objects.get(oid).cloned()))
    }

    fn bee_of(&self, repo: &Repo, oid: &GitOid) -> Option<CoreId> {
        self.repos
            .lock()
            .ok()
            .and_then(|m| m.get(&repo.id).and_then(|c| c.mapping.bee(oid)))
    }

    fn stage(&self, repo: &Repo, raw: RawObject) -> ApiResult<GitOid> {
        let oid = raw.oid(self.format).map_err(ApiError::from)?;
        if let Ok(mut m) = self.repos.lock() {
            m.entry(repo.id.clone())
                .or_default()
                .objects
                .insert(oid, raw);
        }
        Ok(oid)
    }

    async fn load_tree(
        &self,
        h: &History,
        root: &CoreId,
        repo: &Repo,
        out: &mut Loaded,
    ) -> ApiResult<()> {
        let mut stack = vec![*root];
        while let Some(t) = stack.pop() {
            if out.trees.contains_key(&t) || self.known(repo, &t).is_some() {
                continue;
            }
            let tree = h.tree(&t).await.map_err(ApiError::from)?;
            for e in tree.entries() {
                match e.mode {
                    Mode::Tree => stack.push(e.id),
                    Mode::Gitlink => {}
                    _ => {
                        if !out.blobs.contains_key(&e.id) && self.known(repo, &e.id).is_none() {
                            let bytes = match h.blob(&e.id).await.map_err(ApiError::from)? {
                                BlobContent::Bytes(b) => b,
                                BlobContent::Redacted => Vec::new(),
                            };
                            out.blobs.insert(e.id, bytes);
                        }
                    }
                }
            }
            out.trees.insert(t, tree);
        }
        Ok(())
    }

    /// The git id of a changeset, exporting it and its ancestors when new.
    async fn export(&self, repo: &Repo, cs: &CoreId) -> ApiResult<GitOid> {
        if let Some(g) = self.known(repo, cs) {
            return Ok(g);
        }
        let h = self.history(repo).await?;
        let mut loaded = Loaded::default();
        let mut stack = vec![*cs];
        while let Some(id) = stack.pop() {
            if loaded.changesets.contains_key(&id) || self.known(repo, &id).is_some() {
                continue;
            }
            let c = h.changeset(&id).await.map_err(ApiError::from)?;
            let meta = h
                .meta_object(c.meta())
                .await
                .map_err(ApiError::from)?
                .unwrap_or_else(|| Meta::new("unknown", Timestamp::new(0, 0), "unknown", ""));
            loaded.metas.insert(*c.meta(), meta);
            self.load_tree(&h, c.root(), repo, &mut loaded).await?;
            stack.extend(c.parents().iter().copied());
            loaded.changesets.insert(id, c);
        }
        let mapping = self
            .repos
            .lock()
            .ok()
            .and_then(|m| m.get(&repo.id).map(|c| c.mapping.clone()))
            .unwrap_or_default();
        let mut exp = Exporter::with_mapping(self.format, mapping);
        let mut emitted = Vec::new();
        let mut sink = |oid: GitOid, raw: RawObject| {
            emitted.push((oid, raw));
            Ok(())
        };
        let out = exp
            .export_history(&loaded, &[*cs], &mut sink)
            .map_err(ApiError::from)?;
        let oid = match out.last() {
            Some(e) if e.changeset == *cs => e.oid,
            _ => exp
                .mapping()
                .git(cs)
                .ok_or_else(|| internal("export produced no commit"))?,
        };
        if let Ok(mut m) = self.repos.lock() {
            let c = m.entry(repo.id.clone()).or_default();
            c.mapping = exp.into_mapping();
            for (o, raw) in emitted {
                c.objects.insert(o, raw);
            }
        }
        Ok(oid)
    }

    fn parse_oid(&self, sha: &str) -> Option<GitOid> {
        self.format.parse_hex(sha.as_bytes()).ok()
    }

    async fn ref_rows(&self, repo: &Repo) -> ApiResult<Vec<(String, CoreId)>> {
        let h = self.history(repo).await?;
        let rows = self
            .writer
            .meta()
            .list_refs(h.repo(), "")
            .await
            .map_err(ApiError::from)?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.target.map(|t| (r.name.as_str().to_string(), t)))
            .collect())
    }

    async fn ref_tip(&self, repo: &Repo, bee_name: &str) -> ApiResult<Option<(CoreId, u64)>> {
        let h = self.history(repo).await?;
        let Ok(name) = RefName::new(bee_name) else {
            return Ok(None);
        };
        let row = self
            .writer
            .meta()
            .get_ref(h.repo(), &name)
            .await
            .map_err(ApiError::from)?;
        Ok(row.and_then(|r| r.target.map(|t| (t, r.version))))
    }

    /// The commit object of `oid` (exporting it when it is a known changeset).
    fn commit_obj(&self, repo: &Repo, oid: &GitOid) -> ApiResult<Option<GitCommit>> {
        match self.object(repo, oid) {
            Some(raw) if raw.kind == ObjectType::Commit => GitCommit::parse(&raw.data, self.format)
                .map(Some)
                .map_err(ApiError::from),
            _ => Ok(None),
        }
    }

    fn tree_obj(&self, repo: &Repo, oid: &GitOid) -> ApiResult<Option<GitTree>> {
        match self.object(repo, oid) {
            Some(raw) if raw.kind == ObjectType::Tree => GitTree::parse(&raw.data, self.format)
                .map(Some)
                .map_err(ApiError::from),
            _ => Ok(None),
        }
    }

    fn info(&self, oid: &GitOid, c: &GitCommit) -> GitCommitInfo {
        let sig = |i: Option<Ident>| {
            i.map(|i| Signature {
                name: String::from_utf8_lossy(&i.name).into_owned(),
                email: String::from_utf8_lossy(&i.email).into_owned(),
                time_secs: i.seconds,
            })
            .unwrap_or_default()
        };
        GitCommitInfo {
            sha: oid.to_hex(),
            tree: c.tree().to_hex(),
            parents: c.parents().iter().map(|p| p.to_hex()).collect(),
            message: String::from_utf8_lossy(c.message().unwrap_or_default()).into_owned(),
            author: sig(c.author()),
            committer: sig(c.committer()),
            // Without a key registry a signature is reported but never verified; the
            // server's port re-verifies it against the forge's signing keys.
            verification: c.signature().map(|s| crate::ports::CommitVerification {
                verified: false,
                reason: "unknown_key".into(),
                signature: String::from_utf8_lossy(&s.signature).into_owned(),
                payload: String::from_utf8_lossy(&s.payload).into_owned(),
            }),
        }
    }

    /// A commit by id: cached, or an imported/exported changeset found through the stores.
    async fn commit_by_oid(&self, repo: &Repo, oid: &GitOid) -> ApiResult<Option<GitCommit>> {
        if let Some(c) = self.commit_obj(repo, oid)? {
            return Ok(Some(c));
        }
        let h = self.history(repo).await?;
        if let Some(cs) = h
            .changeset_for_git_oid(oid.as_bytes())
            .await
            .map_err(ApiError::from)?
        {
            let g = self.export(repo, &cs).await?;
            return self.commit_obj(repo, &g);
        }
        Ok(None)
    }

    fn mode_of(e: &GitTreeEntry) -> (String, EntryKind) {
        let mode = String::from_utf8_lossy(&e.mode).into_owned();
        let kind = match e.kind() {
            Ok(bee_git::EntryKind::Tree) => EntryKind::Tree,
            Ok(bee_git::EntryKind::Gitlink) => EntryKind::Commit,
            _ => EntryKind::Blob,
        };
        let mode = if kind == EntryKind::Tree && mode == "40000" {
            "040000".to_string()
        } else {
            mode
        };
        (mode, kind)
    }

    fn items(
        &self,
        repo: &Repo,
        tree: &GitTree,
        prefix: &str,
        recursive: bool,
        out: &mut Vec<TreeItem>,
    ) -> ApiResult<bool> {
        for e in &tree.entries {
            if out.len() >= TREE_LIMIT {
                return Ok(true);
            }
            let (mode, kind) = Self::mode_of(e);
            let name = String::from_utf8_lossy(&e.name);
            let path = if prefix.is_empty() {
                name.into_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let size = match kind {
                EntryKind::Blob => self.object(repo, &e.oid).map(|o| o.data.len() as u64),
                _ => None,
            };
            out.push(TreeItem {
                path: path.clone(),
                mode,
                kind,
                sha: e.oid.to_hex(),
                size,
            });
            if recursive
                && kind == EntryKind::Tree
                && let Some(sub) = self.tree_obj(repo, &e.oid)?
                && self.items(repo, &sub, &path, true, out)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The flat file map of a git tree: path → (mode, blob oid).
    fn flatten(
        &self,
        repo: &Repo,
        tree: &GitOid,
        prefix: &str,
        out: &mut BTreeMap<String, (Vec<u8>, GitOid)>,
    ) -> ApiResult<()> {
        let t = self
            .tree_obj(repo, tree)?
            .ok_or_else(|| ApiError::unprocessable(format!("tree {} not found", tree.to_hex())))?;
        for e in &t.entries {
            let name = String::from_utf8_lossy(&e.name);
            let path = if prefix.is_empty() {
                name.into_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if e.kind().ok() == Some(bee_git::EntryKind::Tree) {
                self.flatten(repo, &e.oid, &path, out)?;
            } else {
                out.insert(path, (e.mode.clone(), e.oid));
            }
        }
        Ok(())
    }

    /// Builds nested git trees from a flat map and stages them; returns the root id.
    fn build_tree(
        &self,
        repo: &Repo,
        files: &BTreeMap<String, (Vec<u8>, GitOid)>,
    ) -> ApiResult<GitOid> {
        #[derive(Default)]
        struct Dir {
            files: Vec<(Vec<u8>, Vec<u8>, GitOid)>,
            dirs: BTreeMap<String, Dir>,
        }
        let mut root = Dir::default();
        for (path, (mode, oid)) in files {
            let parts: Vec<&str> = path.split('/').collect();
            let mut d = &mut root;
            for p in &parts[..parts.len() - 1] {
                d = d.dirs.entry(p.to_string()).or_default();
            }
            d.files.push((
                mode.clone(),
                parts[parts.len() - 1].as_bytes().to_vec(),
                *oid,
            ));
        }
        fn write(me: &HistoryGit, repo: &Repo, d: &Dir) -> ApiResult<GitOid> {
            let mut t = GitTree::default();
            for (mode, name, oid) in &d.files {
                t.entries.push(GitTreeEntry {
                    mode: mode.clone(),
                    name: name.clone(),
                    oid: *oid,
                });
            }
            for (name, sub) in &d.dirs {
                let oid = write(me, repo, sub)?;
                t.entries.push(GitTreeEntry::new(
                    bee_git::EntryKind::Tree,
                    name.as_bytes().to_vec(),
                    oid,
                ));
            }
            t.sort();
            me.stage(repo, RawObject::new(ObjectType::Tree, t.serialize()))
        }
        write(self, repo, &root)
    }

    async fn files_of_commit(
        &self,
        repo: &Repo,
        oid: &GitOid,
    ) -> ApiResult<BTreeMap<String, (Vec<u8>, GitOid)>> {
        let c = self
            .commit_by_oid(repo, oid)
            .await?
            .ok_or_else(|| ApiError::unprocessable(format!("commit {} not found", oid.to_hex())))?;
        let mut files = BTreeMap::new();
        self.flatten(repo, &c.tree(), "", &mut files)?;
        Ok(files)
    }

    fn bee_mode(mode: &[u8]) -> Mode {
        match bee_git::EntryKind::from_mode(mode) {
            Ok(k) => k.to_bee(),
            Err(_) => Mode::File,
        }
    }

    /// Creates a changeset with `parents` whose root is `files`; returns its git id.
    #[allow(clippy::too_many_arguments)]
    async fn write_changeset(
        &self,
        repo: &Repo,
        actor: &Actor,
        parents: &[CoreId],
        base: &BTreeMap<String, (Vec<u8>, GitOid)>,
        files: &BTreeMap<String, (Vec<u8>, GitOid)>,
        message: &str,
        author: Option<&Signature>,
    ) -> ApiResult<GitOid> {
        let h = self.history(repo).await?;
        let now = self.clock.now_ms();
        let mut delta = Vec::new();
        let mut blobs = Vec::new();
        let paths: BTreeSet<&String> = base.keys().chain(files.keys()).collect();
        for p in paths {
            let rp = RepoPath::new(p.as_str()).map_err(ApiError::from)?;
            match (base.get(p), files.get(p)) {
                (Some(a), Some(b)) if a == b => {}
                (_, Some((mode, oid))) => {
                    let mode = Self::bee_mode(mode);
                    if mode == Mode::Gitlink {
                        return Err(ApiError::unprocessable(
                            "submodules cannot be written through the git data API",
                        ));
                    }
                    let raw = self.object(repo, oid).ok_or_else(|| {
                        ApiError::unprocessable(format!("blob {} not found", oid.to_hex()))
                    })?;
                    let id = blob_id(&raw.data);
                    delta.push(DeltaEntry::put(rp, mode, id, raw.data.len() as u64));
                    blobs.push((id, raw.data));
                }
                (Some(_), None) => delta.push(DeltaEntry::delete(rp)),
                (None, None) => {}
            }
        }
        if !blobs.is_empty() {
            self.writer
                .upload(
                    h.org(),
                    blobs,
                    Some(UploadRecord {
                        repo: Some(h.repo()),
                        principal: actor.principal_id.clone(),
                    }),
                    now,
                )
                .await
                .map_err(ApiError::from)?;
        }
        let who = |s: Option<&Signature>| match s {
            Some(s) if !s.email.is_empty() => format!("{} <{}>", s.name, s.email),
            Some(s) => s.name.clone(),
            None => format!("{} <{}>", actor.name, actor.email),
        };
        let time = author
            .map(|s| s.time_secs)
            .filter(|t| *t > 0)
            .unwrap_or((now / 1000) as i64);
        let mut meta = Meta::new(
            who(author),
            Timestamp::new(time, 0),
            actor.principal_id.clone(),
            message.to_string(),
        );
        meta.committer = Some(who(None));
        meta.committer_time = Some(Timestamp::new((now / 1000) as i64, 0));
        meta.provenance = Some(Provenance {
            principal_chain: actor.chain.clone(),
            runtime: None,
            model: None,
            session: None,
            prompt_digest: None,
            checks_run: Vec::new(),
            cost: None,
        });
        let meta_id = meta.id();
        self.writer
            .upload(h.org(), vec![(meta_id, meta.encode())], None, now)
            .await
            .map_err(ApiError::from)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"swarm-compat change-id\0");
        for p in parents {
            hasher.update(p.as_bytes());
        }
        hasher.update(meta_id.as_bytes());
        for d in &delta {
            hasher.update(format!("{d:?}").as_bytes());
        }
        let mut raw = [0u8; 16];
        raw.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
        let id = h
            .create_changeset(
                parents.to_vec(),
                meta_id,
                CoreChangeId::new(raw),
                delta,
                now,
            )
            .await
            .map_err(ApiError::from)?;
        self.export(repo, &id).await
    }

    async fn move_ref(
        &self,
        repo: &Repo,
        actor: &Actor,
        bee_name: &str,
        target: Option<CoreId>,
        expected_version: Option<u64>,
        force: bool,
    ) -> ApiResult<()> {
        if let Some(m) = &self.mover {
            return m
                .move_ref(
                    repo,
                    actor,
                    bee_name,
                    target.as_ref().map(proto).as_ref(),
                    expected_version.filter(|v| *v > 0),
                    force,
                )
                .await;
        }
        let h = self.history(repo).await?;
        let name = RefName::new(bee_name).map_err(|e| {
            ApiError::unprocessable(format!("Reference name is invalid: {}", e.message))
        })?;
        let expected = match expected_version {
            Some(v) if v > 0 => Expected::Version(v),
            _ => Expected::Absent,
        };
        self.writer
            .update_ref(RefUpdate::set(
                h.repo(),
                name,
                expected,
                target,
                actor.principal_id.clone(),
                self.clock.now_ms(),
            ))
            .await
            .map_err(ApiError::from)?;
        Ok(())
    }

    fn reachable(&self, repo: &Repo, from: &GitOid) -> ApiResult<BTreeSet<GitOid>> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![*from];
        while let Some(o) = stack.pop() {
            if !seen.insert(o) {
                continue;
            }
            if let Some(c) = self.commit_obj(repo, &o)? {
                stack.extend(c.parents());
            }
        }
        Ok(seen)
    }

    /// Commits from `heads` newest first (committer time, then id).
    fn walk(&self, repo: &Repo, heads: &[GitOid]) -> ApiResult<Vec<(GitOid, GitCommit)>> {
        let mut heap = BinaryHeap::new();
        let mut seen = BTreeSet::new();
        for h in heads {
            if let Some(c) = self.commit_obj(repo, h)? {
                let t = c.committer().map(|i| i.seconds).unwrap_or(0);
                heap.push((t, *h));
                seen.insert(*h);
            }
        }
        let mut out = Vec::new();
        while let Some((_, o)) = heap.pop() {
            let Some(c) = self.commit_obj(repo, &o)? else {
                continue;
            };
            for p in c.parents() {
                if seen.insert(p)
                    && let Some(pc) = self.commit_obj(repo, &p)?
                {
                    heap.push((pc.committer().map(|i| i.seconds).unwrap_or(0), p));
                }
            }
            out.push((o, c));
        }
        Ok(out)
    }

    fn entry_at(&self, repo: &Repo, tree: &GitOid, path: &str) -> ApiResult<Option<GitTreeEntry>> {
        let mut cur = *tree;
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        for (i, p) in parts.iter().enumerate() {
            let Some(t) = self.tree_obj(repo, &cur)? else {
                return Ok(None);
            };
            let Some(e) = t.entries.iter().find(|e| e.name == p.as_bytes()) else {
                return Ok(None);
            };
            if i + 1 == parts.len() {
                return Ok(Some(e.clone()));
            }
            cur = e.oid;
        }
        Ok(None)
    }

    fn bee_blob_git(&self, repo: &Repo, id: &Option<ObjectId>) -> String {
        id.as_ref()
            .and_then(|i| CoreId::from_bytes(&i.raw).ok())
            .and_then(|i| self.known(repo, &i))
            .map(|o| o.to_hex())
            .unwrap_or_default()
    }
}

#[async_trait]
impl GitPort for HistoryGit {
    async fn refs(&self, repo: &Repo, prefix: &str) -> ApiResult<Vec<GitRefInfo>> {
        let mut out = Vec::new();
        for (name, target) in self.ref_rows(repo).await? {
            let git = git_ref_name(&name);
            if !git.starts_with(prefix) {
                continue;
            }
            let oid = self.export(repo, &target).await?;
            out.push(GitRefInfo {
                name: git,
                sha: oid.to_hex(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn resolve(&self, repo: &Repo, rev: &str) -> ApiResult<Option<String>> {
        let rev = rev.trim();
        if rev.is_empty() {
            return Ok(None);
        }
        let is_hex =
            rev.len() >= 7 && rev.len() <= 64 && rev.bytes().all(|b| b.is_ascii_hexdigit());
        if is_hex
            && rev.len() == self.format.oid_len() * 2
            && let Some(o) = self.parse_oid(&rev.to_lowercase())
            && self.commit_by_oid(repo, &o).await?.is_some()
        {
            return Ok(Some(o.to_hex()));
        }
        let candidates: Vec<String> = if rev == "HEAD" {
            vec![if repo.default_ref.is_empty() {
                "main".to_string()
            } else {
                repo.default_ref.clone()
            }]
        } else if rev.starts_with("refs/") {
            vec![bee_ref_name(rev)]
        } else {
            vec![rev.to_string(), format!("tags/{rev}")]
        };
        for c in candidates {
            if let Some((t, _)) = self.ref_tip(repo, &c).await? {
                return Ok(Some(self.export(repo, &t).await?.to_hex()));
            }
        }
        if is_hex {
            let lower = rev.to_lowercase();
            // Abbreviations resolve among commits already exported (refs export everything
            // reachable from them).
            self.refs(repo, "refs/").await?;
            let found: Vec<String> = self
                .repos
                .lock()
                .ok()
                .map(|m| {
                    m.get(&repo.id)
                        .map(|c| {
                            c.objects
                                .iter()
                                .filter(|(o, r)| {
                                    r.kind == ObjectType::Commit && o.to_hex().starts_with(&lower)
                                })
                                .map(|(o, _)| o.to_hex())
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            if found.len() == 1 {
                return Ok(found.into_iter().next());
            }
        }
        Ok(None)
    }

    async fn commit(&self, repo: &Repo, sha: &str) -> ApiResult<Option<GitCommitInfo>> {
        let Some(oid) = self.parse_oid(sha) else {
            return Ok(None);
        };
        Ok(self
            .commit_by_oid(repo, &oid)
            .await?
            .map(|c| self.info(&oid, &c)))
    }

    async fn log(
        &self,
        repo: &Repo,
        head: &str,
        path: Option<&str>,
        skip: usize,
        limit: usize,
    ) -> ApiResult<Vec<GitCommitInfo>> {
        let Some(oid) = self.parse_oid(head) else {
            return Ok(Vec::new());
        };
        if self.commit_by_oid(repo, &oid).await?.is_none() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut skipped = 0;
        for (o, c) in self.walk(repo, &[oid])? {
            if let Some(p) = path.filter(|p| !p.is_empty()) {
                let mine = self.entry_at(repo, &c.tree(), p)?.map(|e| e.oid);
                let parent = match c.parents().first() {
                    Some(po) => match self.commit_obj(repo, po)? {
                        Some(pc) => self.entry_at(repo, &pc.tree(), p)?.map(|e| e.oid),
                        None => None,
                    },
                    None => None,
                };
                if mine == parent {
                    continue;
                }
            }
            if skipped < skip {
                skipped += 1;
                continue;
            }
            out.push(self.info(&o, &c));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn tree(
        &self,
        repo: &Repo,
        sha: &str,
        recursive: bool,
    ) -> ApiResult<Option<TreeListing>> {
        let Some(oid) = self.parse_oid(sha) else {
            return Ok(None);
        };
        let tree_oid = match self.commit_by_oid(repo, &oid).await? {
            Some(c) => c.tree(),
            None => oid,
        };
        let Some(t) = self.tree_obj(repo, &tree_oid)? else {
            return Ok(None);
        };
        let mut entries = Vec::new();
        let truncated = self.items(repo, &t, "", recursive, &mut entries)?;
        Ok(Some(TreeListing {
            sha: tree_oid.to_hex(),
            entries,
            truncated,
        }))
    }

    async fn blob(&self, repo: &Repo, sha: &str) -> ApiResult<Option<Vec<u8>>> {
        let Some(oid) = self.parse_oid(sha) else {
            return Ok(None);
        };
        Ok(self
            .object(repo, &oid)
            .filter(|o| o.kind == ObjectType::Blob)
            .map(|o| o.data))
    }

    async fn path(&self, repo: &Repo, commit: &str, path: &str) -> ApiResult<Option<PathEntry>> {
        let Some(oid) = self.parse_oid(commit) else {
            return Ok(None);
        };
        let Some(c) = self.commit_by_oid(repo, &oid).await? else {
            return Ok(None);
        };
        let path = path.trim_matches('/');
        let (oid, mode) = if path.is_empty() {
            (c.tree(), b"40000".to_vec())
        } else {
            match self.entry_at(repo, &c.tree(), path)? {
                Some(e) => (e.oid, e.mode),
                None => return Ok(None),
            }
        };
        let kind = bee_git::EntryKind::from_mode(&mode).map_err(ApiError::from)?;
        Ok(Some(match kind {
            bee_git::EntryKind::Tree => {
                let t = self
                    .tree_obj(repo, &oid)?
                    .ok_or_else(|| internal("tree missing from the export"))?;
                let mut entries = Vec::new();
                self.items(repo, &t, "", false, &mut entries)?;
                PathEntry::Dir {
                    sha: oid.to_hex(),
                    entries,
                }
            }
            bee_git::EntryKind::Gitlink => PathEntry::Submodule { sha: oid.to_hex() },
            bee_git::EntryKind::Symlink => PathEntry::Symlink {
                sha: oid.to_hex(),
                target: String::from_utf8_lossy(
                    &self.object(repo, &oid).map(|o| o.data).unwrap_or_default(),
                )
                .into_owned(),
            },
            _ => PathEntry::File {
                sha: oid.to_hex(),
                bytes: self.object(repo, &oid).map(|o| o.data).unwrap_or_default(),
            },
        }))
    }

    async fn compare(&self, repo: &Repo, base: &str, head: &str) -> ApiResult<Comparison> {
        let (Some(b), Some(hd)) = (self.parse_oid(base), self.parse_oid(head)) else {
            return Err(ApiError::not_found());
        };
        if self.commit_by_oid(repo, &b).await?.is_none()
            || self.commit_by_oid(repo, &hd).await?.is_none()
        {
            return Err(ApiError::not_found());
        }
        let (bs, hs) = match (self.bee_of(repo, &b), self.bee_of(repo, &hd)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Err(ApiError::not_found()),
        };
        let h = self.history(repo).await?;
        let bases = h.merge_bases(&bs, &hs).await.map_err(ApiError::from)?;
        let merge_base = match bases.first() {
            Some(mb) => Some(self.export(repo, mb).await?),
            None => None,
        };
        let from_base = self.reachable(repo, &b)?;
        let from_head = self.reachable(repo, &hd)?;
        let mut ahead: Vec<GitCommitInfo> = self
            .walk(repo, &[hd])?
            .into_iter()
            .filter(|(o, _)| !from_base.contains(o))
            .map(|(o, c)| self.info(&o, &c))
            .collect();
        ahead.reverse();
        let behind_by = from_base.difference(&from_head).count() as u64;
        let files = match &merge_base {
            Some(mb) => self.diff(repo, &mb.to_hex(), head).await?,
            None => Vec::new(),
        };
        Ok(Comparison {
            merge_base: merge_base.map(|m| m.to_hex()),
            ahead,
            behind_by,
            files,
        })
    }

    async fn diff(&self, repo: &Repo, from: &str, to: &str) -> ApiResult<Vec<FileChange>> {
        let (Some(f), Some(t)) = (self.parse_oid(from), self.parse_oid(to)) else {
            return Err(ApiError::not_found());
        };
        self.commit_by_oid(repo, &f).await?;
        self.commit_by_oid(repo, &t).await?;
        let (Some(fs), Some(ts)) = (self.bee_of(repo, &f), self.bee_of(repo, &t)) else {
            return Err(ApiError::not_found());
        };
        let items = self
            .vcs
            .diff(repo, &proto(&fs), &proto(&ts), &[], true, 3)
            .await
            .map_err(ApiError::from)?;
        Ok(items
            .into_iter()
            .map(|d| {
                let status = match DiffStatus::try_from(d.status).unwrap_or_default() {
                    DiffStatus::Added => "added",
                    DiffStatus::Deleted => "removed",
                    DiffStatus::Renamed => "renamed",
                    DiffStatus::ModeChanged => "changed",
                    _ => "modified",
                };
                let patch = (!d.binary).then(|| {
                    let mut p = String::new();
                    for hk in &d.hunks {
                        p.push_str(&format!(
                            "@@ -{},{} +{},{} @@\n",
                            hk.old_start, hk.old_len, hk.new_start, hk.new_len
                        ));
                        for l in &hk.lines {
                            p.push_str(l);
                            p.push('\n');
                        }
                    }
                    p.trim_end_matches('\n').to_string()
                });
                let sha = if status == "removed" {
                    self.bee_blob_git(repo, &d.old.as_ref().and_then(|s| s.blob.clone()))
                } else {
                    self.bee_blob_git(repo, &d.new.as_ref().and_then(|s| s.blob.clone()))
                };
                FileChange {
                    filename: String::from_utf8_lossy(&d.path).into_owned(),
                    previous_filename: (!d.old_path.is_empty())
                        .then(|| String::from_utf8_lossy(&d.old_path).into_owned()),
                    status: status.into(),
                    additions: d.additions as u64,
                    deletions: d.deletions as u64,
                    patch,
                    sha,
                }
            })
            .collect())
    }

    async fn create_blob(&self, repo: &Repo, _actor: &Actor, bytes: Vec<u8>) -> ApiResult<String> {
        Ok(self
            .stage(repo, RawObject::new(ObjectType::Blob, bytes))?
            .to_hex())
    }

    async fn create_tree(
        &self,
        repo: &Repo,
        _actor: &Actor,
        base_tree: Option<&str>,
        items: Vec<NewTreeItem>,
    ) -> ApiResult<String> {
        let mut files = BTreeMap::new();
        if let Some(b) = base_tree {
            let oid = self
                .parse_oid(b)
                .ok_or_else(|| ApiError::unprocessable("base_tree is not a valid tree id"))?;
            self.flatten(repo, &oid, "", &mut files)?;
        }
        for it in items {
            let path = it.path.trim_matches('/').to_string();
            if path.is_empty() {
                return Err(ApiError::validation(
                    "Tree",
                    "path",
                    "invalid",
                    "empty path",
                ));
            }
            bee_git::EntryKind::from_mode(it.mode.as_bytes())
                .map_err(|_| ApiError::validation("Tree", "mode", "invalid", &it.mode))?;
            let mode = if it.mode == "040000" {
                "40000".to_string()
            } else {
                it.mode.clone()
            };
            match (it.sha, it.content) {
                (None, None) => {
                    let prefix = format!("{path}/");
                    files.retain(|p, _| p != &path && !p.starts_with(&prefix));
                }
                (_, Some(content)) => {
                    let oid = self.stage(repo, RawObject::new(ObjectType::Blob, content))?;
                    files.insert(path, (mode.into_bytes(), oid));
                }
                (Some(sha), None) => {
                    let oid = self
                        .parse_oid(&sha)
                        .ok_or_else(|| ApiError::validation("Tree", "sha", "invalid", &sha))?;
                    if mode == "40000" {
                        let prefix = format!("{path}/");
                        files.retain(|p, _| p != &path && !p.starts_with(&prefix));
                        let mut sub = BTreeMap::new();
                        self.flatten(repo, &oid, &path, &mut sub)?;
                        files.extend(sub);
                    } else {
                        if mode != "160000" && self.object(repo, &oid).is_none() {
                            return Err(ApiError::validation(
                                "Tree",
                                "sha",
                                "invalid",
                                &format!("{sha} is not a blob"),
                            ));
                        }
                        files.insert(path, (mode.into_bytes(), oid));
                    }
                }
            }
        }
        Ok(self.build_tree(repo, &files)?.to_hex())
    }

    async fn create_commit(
        &self,
        repo: &Repo,
        actor: &Actor,
        c: NewGitCommit,
    ) -> ApiResult<String> {
        let tree = self
            .parse_oid(&c.tree)
            .ok_or_else(|| ApiError::validation("Commit", "tree", "invalid", &c.tree))?;
        let mut files = BTreeMap::new();
        self.flatten(repo, &tree, "", &mut files)?;
        let mut parents = Vec::new();
        let mut base = BTreeMap::new();
        for (i, p) in c.parents.iter().enumerate() {
            let oid = self
                .parse_oid(p)
                .ok_or_else(|| ApiError::validation("Commit", "parents", "invalid", p))?;
            if self.commit_by_oid(repo, &oid).await?.is_none() {
                return Err(ApiError::validation(
                    "Commit",
                    "parents",
                    "invalid",
                    &format!("{p} is not a commit"),
                ));
            }
            let cs = self
                .bee_of(repo, &oid)
                .ok_or_else(|| internal("exported commit without a changeset"))?;
            if i == 0 {
                base = self.files_of_commit(repo, &oid).await?;
            }
            parents.push(cs);
        }
        let oid = self
            .write_changeset(
                repo,
                actor,
                &parents,
                &base,
                &files,
                &c.message,
                c.author.as_ref(),
            )
            .await?;
        Ok(oid.to_hex())
    }

    async fn update_ref(
        &self,
        repo: &Repo,
        actor: &Actor,
        name: &str,
        sha: Option<&str>,
        expected: Option<&str>,
        force: bool,
    ) -> ApiResult<()> {
        let bee = bee_ref_name(name);
        let cur = self.ref_tip(repo, &bee).await?;
        if let Some(exp) = expected {
            let cur_git = match &cur {
                Some((t, _)) => Some(self.export(repo, t).await?.to_hex()),
                None => None,
            };
            if cur_git.as_deref() != Some(exp) {
                return Err(ApiError::new(
                    409,
                    ErrorReason::VersionMismatch,
                    "Reference update conflict",
                ));
            }
        }
        let target = match sha {
            Some(s) => {
                let oid = self
                    .parse_oid(s)
                    .ok_or_else(|| ApiError::unprocessable("Object does not exist"))?;
                if self.commit_by_oid(repo, &oid).await?.is_none() {
                    return Err(ApiError::unprocessable("Object does not exist"));
                }
                Some(
                    self.bee_of(repo, &oid)
                        .ok_or_else(|| ApiError::unprocessable("Object does not exist"))?,
                )
            }
            None => None,
        };
        if !force
            && let (Some((old, _)), Some(new)) = (&cur, &target)
            && old != new
        {
            let h = self.history(repo).await?;
            if !h.is_ancestor(old, new).await.map_err(ApiError::from)? {
                return Err(ApiError::new(
                    422,
                    ErrorReason::NotFastForward,
                    "Update is not a fast forward",
                ));
            }
        }
        if cur.is_none() && target.is_none() {
            return Err(ApiError::unprocessable("Reference does not exist"));
        }
        self.move_ref(repo, actor, &bee, target, cur.map(|(_, v)| v), force)
            .await
    }

    async fn commit_files(
        &self,
        repo: &Repo,
        actor: &Actor,
        branch: &str,
        parent: Option<&str>,
        edits: Vec<FileEdit>,
        message: &str,
    ) -> ApiResult<String> {
        let tip = self.ref_tip(repo, branch).await?;
        let (base, parents, version) = match &tip {
            Some((t, v)) => {
                let oid = self.export(repo, t).await?;
                if let Some(p) = parent
                    && p != oid.to_hex()
                {
                    return Err(ApiError::new(
                        409,
                        ErrorReason::VersionMismatch,
                        format!("{branch} is at {} but expected {p}", oid.to_hex()),
                    ));
                }
                (self.files_of_commit(repo, &oid).await?, vec![*t], Some(*v))
            }
            None => (BTreeMap::new(), Vec::new(), None),
        };
        let mut files = base.clone();
        for e in edits {
            let path = e.path.trim_matches('/').to_string();
            match e.content {
                Some(bytes) => {
                    let mode = files
                        .get(&path)
                        .map(|(m, _)| m.clone())
                        .unwrap_or_else(|| b"100644".to_vec());
                    let oid = self.stage(repo, RawObject::new(ObjectType::Blob, bytes))?;
                    files.insert(path, (mode, oid));
                }
                None => {
                    files.remove(&path);
                }
            }
        }
        let oid = self
            .write_changeset(repo, actor, &parents, &base, &files, message, None)
            .await?;
        let cs = self
            .bee_of(repo, &oid)
            .ok_or_else(|| internal("new commit has no changeset"))?;
        self.move_ref(repo, actor, branch, Some(cs), version, false)
            .await?;
        Ok(oid.to_hex())
    }

    async fn changeset(&self, repo: &Repo, sha: &str) -> ApiResult<Option<ObjectId>> {
        let Some(oid) = self.parse_oid(sha) else {
            return Ok(None);
        };
        if self.commit_by_oid(repo, &oid).await?.is_none() {
            return Ok(None);
        }
        Ok(self.bee_of(repo, &oid).map(|c| proto(&c)))
    }

    async fn git_id(&self, repo: &Repo, changeset: &ObjectId) -> ApiResult<Option<String>> {
        let cs = core(changeset)?;
        match self.export(repo, &cs).await {
            Ok(o) => Ok(Some(o.to_hex())),
            Err(e) if e.status == 404 => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// The forge's tag store over the VCS refs (`tags/<name>`), lightweight tags only.
pub struct HistoryTags {
    writer: Writer,
    vcs: Arc<HistoryVcs>,
    clock: Arc<dyn Clock>,
}

impl HistoryTags {
    /// Over `writer`.
    pub fn new(writer: Writer, clock: Arc<dyn Clock>) -> HistoryTags {
        HistoryTags {
            vcs: Arc::new(HistoryVcs::new(writer.clone(), clock.clone())),
            writer,
            clock,
        }
    }

    /// The repo (ids only) from the VCS repo row: the tag port is keyed by repo id alone.
    async fn repo(&self, id: &str) -> bee_forge::Result<Repo> {
        let ulid = bee_core::Ulid::parse(id)
            .map_err(|e| bee_forge::ForgeError::invalid("invalid_repo", e.message))?;
        let row = self
            .writer
            .meta()
            .get_repo(bee_store::RepoId(ulid))
            .await
            .map_err(bee_forge::changes::vcs::vcs_err)?
            .ok_or_else(|| bee_forge::ForgeError::not_found(format!("repo {id}")))?;
        Ok(Repo {
            id: id.to_string(),
            org_id: row.org.0.to_string(),
            ..Default::default()
        })
    }
}

#[async_trait]
impl TagVcs for HistoryTags {
    async fn list(&self, repo_id: &str, prefix: &str) -> bee_forge::Result<Vec<TagInfo>> {
        let repo = self.repo(repo_id).await?;
        let h = self.vcs.history(&repo).await?;
        let rows = self
            .writer
            .meta()
            .list_refs(h.repo(), &format!("tags/{prefix}"))
            .await
            .map_err(bee_forge::changes::vcs::vcs_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let t = r.target?;
                Some(TagInfo {
                    name: r.name.as_str().trim_start_matches("tags/").to_string(),
                    target: proto(&t),
                    ref_version: r.version,
                    annotation: None,
                    protected: false,
                })
            })
            .collect())
    }

    async fn get(&self, repo_id: &str, name: &str) -> bee_forge::Result<Option<TagInfo>> {
        Ok(self
            .list(repo_id, name)
            .await?
            .into_iter()
            .find(|t| t.name == name))
    }

    async fn create(
        &self,
        repo_id: &str,
        name: &str,
        target: &ObjectId,
        _annotation: Option<NewAnnotation>,
    ) -> bee_forge::Result<TagInfo> {
        let repo = self.repo(repo_id).await?;
        if self.get(repo_id, name).await?.is_some() {
            return Err(bee_forge::ForgeError::already_exists(format!("tag {name}")));
        }
        let version = self
            .vcs
            .set_ref(&repo, &format!("tags/{name}"), target)
            .await?;
        Ok(TagInfo {
            name: name.to_string(),
            target: target.clone(),
            ref_version: version,
            annotation: None,
            protected: false,
        })
    }

    async fn delete(
        &self,
        repo_id: &str,
        name: &str,
        expected_version: u64,
    ) -> bee_forge::Result<()> {
        let repo = self.repo(repo_id).await?;
        let h = self.vcs.history(&repo).await?;
        let cur = self
            .get(repo_id, name)
            .await?
            .ok_or_else(|| bee_forge::ForgeError::not_found(format!("tag {name}")))?;
        let version = if expected_version == 0 {
            cur.ref_version
        } else {
            expected_version
        };
        let rn = RefName::new(format!("tags/{name}"))
            .map_err(|e| bee_forge::ForgeError::invalid("invalid_ref", e.message))?;
        self.writer
            .update_ref(RefUpdate::set(
                h.repo(),
                rn,
                Expected::Version(version),
                None,
                "forge",
                self.clock.now_ms(),
            ))
            .await
            .map_err(bee_forge::changes::vcs::vcs_err)?;
        Ok(())
    }
}

/// The forge's revision resolver over the VCS stores: SPEC revision syntax (text ids, refs,
/// `git:<sha>`), relative to the repo's default ref.
pub struct HistoryResolver {
    vcs: Arc<HistoryVcs>,
}

impl HistoryResolver {
    /// Over `writer`.
    pub fn new(writer: Writer, clock: Arc<dyn Clock>) -> HistoryResolver {
        HistoryResolver {
            vcs: Arc::new(HistoryVcs::new(writer, clock)),
        }
    }
}

#[async_trait]
impl bee_forge::hooks::RevisionResolver for HistoryResolver {
    async fn resolve(&self, repo: &Repo, expr: &str) -> bee_forge::Result<ObjectId> {
        let h = self.vcs.history(repo).await?;
        let default = if repo.default_ref.is_empty() {
            "main"
        } else {
            repo.default_ref.as_str()
        };
        let on = RefName::new(default).map_err(bee_forge::changes::vcs::vcs_err)?;
        let expr = if expr.is_empty() { default } else { expr };
        let id = h
            .resolve_str(expr, Some(&on))
            .await
            .map_err(bee_forge::changes::vcs::vcs_err)?;
        Ok(proto(&id))
    }
}

/// [`CompatState`] in memory.
#[derive(Default)]
pub struct MemoryState {
    map: Mutex<BTreeMap<String, String>>,
}

#[async_trait]
impl CompatState for MemoryState {
    async fn get(&self, key: &str) -> ApiResult<Option<String>> {
        Ok(self.map.lock().ok().and_then(|m| m.get(key).cloned()))
    }
    async fn put(&self, key: &str, value: &str) -> ApiResult<()> {
        if let Ok(mut m) = self.map.lock() {
            m.insert(key.to_string(), value.to_string());
        }
        Ok(())
    }
    async fn delete(&self, key: &str) -> ApiResult<()> {
        if let Ok(mut m) = self.map.lock() {
            m.remove(key);
        }
        Ok(())
    }
}

/// Refuses asset uploads (no blob store configured).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoAssets;

#[async_trait]
impl AssetStore for NoAssets {
    async fn put(&self, _url: &str, _headers: &[(String, String)], _bytes: Bytes) -> ApiResult<()> {
        Err(ApiError::new(
            501,
            ErrorReason::Unimplemented,
            "release asset storage is not configured on this server",
        ))
    }
}

/// Asset bytes in memory, keyed by the URL path (upload and download URLs of one key share
/// it).
#[derive(Default)]
pub struct MemoryAssets {
    map: Mutex<BTreeMap<String, Bytes>>,
}

impl MemoryAssets {
    /// The bytes stored for a signed URL (upload or download).
    pub fn get(&self, url: &str) -> Option<Bytes> {
        let key = url.split('?').next().unwrap_or(url).to_string();
        self.map.lock().ok().and_then(|m| m.get(&key).cloned())
    }
}

#[async_trait]
impl AssetStore for MemoryAssets {
    async fn put(&self, url: &str, _headers: &[(String, String)], bytes: Bytes) -> ApiResult<()> {
        let key = url.split('?').next().unwrap_or(url).to_string();
        if let Ok(mut m) = self.map.lock() {
            m.insert(key, bytes);
        }
        Ok(())
    }
}

/// Code search by scanning default-branch files through the git port (case-insensitive
/// substring per term). Adequate for tests and small repos; the server wires `bee-search`.
pub struct GrepSearch {
    git: Arc<dyn GitPort>,
}

impl GrepSearch {
    /// Over `git`.
    pub fn new(git: Arc<dyn GitPort>) -> GrepSearch {
        GrepSearch { git }
    }
}

#[async_trait]
impl CodeSearch for GrepSearch {
    async fn search(&self, repos: &[Repo], query: &str, limit: usize) -> ApiResult<Vec<CodeHit>> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|t| t.trim_matches('"').to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        let mut out = Vec::new();
        if terms.is_empty() {
            return Ok(out);
        }
        for repo in repos {
            let branch = if repo.default_ref.is_empty() {
                "main"
            } else {
                repo.default_ref.as_str()
            };
            let Some(head) = self.git.resolve(repo, branch).await? else {
                continue;
            };
            let Some(tree) = self.git.tree(repo, &head, true).await? else {
                continue;
            };
            for e in tree.entries.iter().filter(|e| e.kind == EntryKind::Blob) {
                let Some(bytes) = self.git.blob(repo, &e.sha).await? else {
                    continue;
                };
                let Ok(text) = std::str::from_utf8(&bytes) else {
                    continue;
                };
                let lower = text.to_lowercase();
                let path_lower = e.path.to_lowercase();
                if terms
                    .iter()
                    .all(|t| lower.contains(t.as_str()) || path_lower.contains(t.as_str()))
                {
                    let fragments = text
                        .lines()
                        .filter(|l| terms.iter().any(|t| l.to_lowercase().contains(t.as_str())))
                        .take(3)
                        .map(str::to_string)
                        .collect();
                    out.push(CodeHit {
                        repo_id: repo.id.clone(),
                        path: e.path.clone(),
                        sha: e.sha.clone(),
                        fragments,
                    });
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_names_map_both_ways() {
        for (s, g) in [
            ("main", "refs/heads/main"),
            ("feature/x", "refs/heads/feature/x"),
            ("tags/v1", "refs/tags/v1"),
            ("pull/3/head", "refs/pull/3/head"),
            ("git/notes/commits", "refs/notes/commits"),
        ] {
            assert_eq!(git_ref_name(s), g);
            assert_eq!(bee_ref_name(g), s);
        }
    }
}
