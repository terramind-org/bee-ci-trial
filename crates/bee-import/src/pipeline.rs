//! The import pipeline: git refs and objects in, changesets, objects, derived rows and refs
//! out, through [`Writer`] in the SPEC §6.2 order, with a fidelity report.
//!
//! 1. **Refs.** Names are mapped onto bee's grammar ([`crate::refs`]); annotated tags are
//!    peeled to their commit, and each tag object is kept byte-exact as a `git_tag` record.
//! 2. **Walk.** From the ref tips, parents first, stopping at commits an earlier run imported
//!    (found through their `git_mapping` row). Only commit ids and parent lists are held.
//! 3. **Convert and write.** Each commit is converted by [`Importer`] (which rebuilds every
//!    tree and commit and rejects any that would not reproduce its git id). New blobs, trees
//!    and metas are batched and uploaded (objects that already have a row are skipped), then
//!    the batch's changesets are inserted in parents-first order, each with its
//!    `object_paths` rows and its `git_mapping` rows. A commit that cannot be carried is
//!    dropped with a reason, and so are its descendants (they cannot keep their ids).
//! 4. **Refs again.** Each ref is created or moved to its changeset by the service principal
//!    (`Expected::Absent` or the current version); a ref already at its target is not written.
//! 5. **Verify** (optional). Every changeset written by this run is read back from the stores
//!    and exported with a fresh [`Exporter`]; its git id, its parents and every ref target are
//!    compared with the source.
//!
//! Re-running on unchanged history walks only the ref tips, finds them imported and writes
//! nothing at all.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use bee_core::{
    BeeError, Limits, Mode, ObjectId, ObjectKind, PayloadValue, Reason, RefName, Result, Subcode,
};
use bee_git::{
    BeeObjectSource, Exporter, GitCommit, GitMapping, GitObjectSource, GitOid, GitTag,
    ImportedCommit, Importer, ObjectFormat, ObjectType,
};
use bee_object::{Changeset, Meta, Tree};
use bee_store::trees::{object_paths_for, read_object_bytes, read_tree};
use bee_store::{
    DerivedRow, Expected, NewChangeset, ObjectState, OrgId, PutOutcome, ReadFrom, RefUpdate,
    RepoId, UploadRecord, Writer,
};

use crate::lfs::lfs_pointer;
use crate::refs::{MappedRef, map_ref_names};
use crate::report::{
    ImportReport, RefRecord, RenamedRef, ReportOptions, Verification, category, reason,
};
use crate::source::{GitRef, RepoSource, SharedSource};
use crate::{
    GIT_MAPPING_TYPE, GIT_MAPPING_VERSION, GIT_REF_TAG_TYPE, GIT_REF_TAG_VERSION, GIT_TAG_TYPE,
    GIT_TAG_VERSION,
};

/// Where an import writes.
#[derive(Clone)]
pub struct ImportTarget {
    /// The write-ordering protocol over the stores.
    pub writer: Writer,
    /// The org (objects are deduplicated per org).
    pub org: OrgId,
    /// The repo receiving changesets and refs.
    pub repo: RepoId,
}

/// Supplies the eager derived rows (SPEC §8, e.g. `graph`) the server computes for every
/// changeset. Called just before each changeset is inserted, after its parents are stored.
#[async_trait]
pub trait DerivedHook: Send + Sync {
    /// Extra rows for `changeset`, inserted in its CreateChangeset transaction.
    async fn rows(&self, target: &ImportTarget, changeset: &Changeset) -> Result<Vec<DerivedRow>>;
}

/// How to import.
#[derive(Clone)]
pub struct ImportOptions {
    /// The service principal recorded as Meta `creator`, uploader and ref-log principal.
    pub creator: String,
    /// A label for the report (`github:acme/api`, a path).
    pub source: String,
    /// Import `refs/pull/*` as `pull/*`.
    pub include_pull_refs: bool,
    /// Read every written changeset back and export it to git, comparing ids, parents and
    /// ref targets with the source (`--verify`).
    pub verify: bool,
    /// The repo's limits (blob size).
    pub limits: Limits,
    /// Upload batch size in bytes: a batch is written once it holds this much.
    pub batch_bytes: usize,
    /// Upload batch size in changesets.
    pub batch_commits: usize,
    /// Time stamped on rows and ref-log entries, from the caller's injected clock.
    pub at_ms: u64,
    /// Commits at the edge of a shallow clone (their parents are absent).
    pub shallow: BTreeSet<GitOid>,
    /// The ref the source's `HEAD` names, for the report's default branch.
    pub head: Option<Vec<u8>>,
    /// Eager derived rows to add to every changeset.
    pub derived: Option<Arc<dyn DerivedHook>>,
}

impl ImportOptions {
    /// Defaults for an import run by `creator`: no pull refs, no verification, SPEC §4.6
    /// limits, 8 MiB / 512-changeset batches, time 0.
    pub fn new(creator: impl Into<String>) -> ImportOptions {
        ImportOptions {
            creator: creator.into(),
            source: String::new(),
            include_pull_refs: false,
            verify: false,
            limits: Limits::DEFAULT,
            batch_bytes: 8 << 20,
            batch_commits: 512,
            at_ms: 0,
            shallow: BTreeSet::new(),
            head: None,
            derived: None,
        }
    }
}

/// The result of an import.
#[derive(Clone, Debug)]
pub struct ImportOutcome {
    /// The fidelity report.
    pub report: ImportReport,
    /// Git ↔ bee ids of everything converted in this run (commits, trees, blobs), plus the
    /// already-imported commits the walk stopped at.
    pub mapping: GitMapping,
}

/// Imports a local repository (working tree or bare): every ref, its shallow list and
/// `HEAD`. `opts.source` defaults to the path.
pub async fn import_repository(
    path: &std::path::Path,
    target: &ImportTarget,
    opts: &ImportOptions,
) -> Result<ImportOutcome> {
    let src = RepoSource::open(path)?;
    let refs = src.refs()?;
    let mut opts = opts.clone();
    if opts.shallow.is_empty() {
        opts.shallow = src.shallow()?;
    }
    if opts.head.is_none() {
        opts.head = src.head();
    }
    if opts.source.is_empty() {
        opts.source = path.display().to_string();
    }
    import_refs(&src, src.format(), &refs, target, &opts).await
}

/// Imports a packfile and its ref list, as a fetch produces them. Thin packs need
/// `external` for bases outside the pack.
pub async fn import_pack(
    pack: Vec<u8>,
    format: ObjectFormat,
    refs: &[GitRef],
    external: Option<SharedSource>,
    target: &ImportTarget,
    opts: &ImportOptions,
) -> Result<ImportOutcome> {
    let src = crate::source::PackSource::from_pack(pack, format, external)?;
    import_refs(&src, format, refs, target, opts).await
}

/// Imports `refs` and their history from any object source.
///
/// Content problems (names bee forbids, limits, corrupt objects, missing parents) never
/// fail the call: they are dropped with a reason in the report. Errors are store or I/O
/// failures; re-running after one resumes where the last flushed batch ended.
pub async fn import_refs(
    src: &(dyn GitObjectSource + Sync),
    format: ObjectFormat,
    refs: &[GitRef],
    target: &ImportTarget,
    opts: &ImportOptions,
) -> Result<ImportOutcome> {
    let run = Run {
        src,
        format,
        target,
        opts,
        importer: Importer::new(format, opts.creator.clone()).with_limits(opts.limits),
        report: ImportReport::new(
            opts.source.clone(),
            format.name(),
            ReportOptions {
                include_pull_refs: opts.include_pull_refs,
                verify: opts.verify,
            },
        ),
        known: BTreeMap::new(),
        loaded_trees: BTreeSet::new(),
        parents: BTreeMap::new(),
        missing: BTreeMap::new(),
        dropped: BTreeSet::new(),
        imported: Vec::new(),
        batch: Batch::default(),
        gitlinks: BTreeSet::new(),
        lfs: BTreeSet::new(),
    };
    run.go(refs).await
}

/// Errors that mean the infrastructure failed, not the content: they abort the import.
fn is_fatal(e: &BeeError) -> bool {
    matches!(
        e.reason,
        Reason::Unavailable
            | Reason::Internal
            | Reason::DeadlineExceeded
            | Reason::ResourceExhausted
            | Reason::Unimplemented
            | Reason::Redacted
    )
}

/// The stable drop reason for a conversion error.
pub fn drop_reason(e: &BeeError) -> &'static str {
    match e.reason {
        Reason::TooLarge => match e.payload.get("limit") {
            Some(PayloadValue::Str(l)) if l == "parents" => reason::TOO_MANY_PARENTS,
            Some(PayloadValue::Str(l)) if l == "message bytes" => reason::MESSAGE_TOO_LARGE,
            Some(PayloadValue::Str(l)) if l == "blob_size" => reason::BLOB_TOO_LARGE,
            _ => reason::TOO_LARGE,
        },
        Reason::Integrity => reason::INTEGRITY,
        Reason::MissingObject => reason::MISSING_OBJECT,
        Reason::InvalidArgument => match e.subcode() {
            Some(Subcode::InvalidPath) if e.message.contains("deeper") => reason::DEPTH_LIMIT,
            Some(Subcode::InvalidPath) => reason::INVALID_PATH,
            Some(Subcode::InvalidName) => reason::UNSUPPORTED_NAME,
            Some(Subcode::InvalidTree) => reason::DUPLICATE_ENTRY,
            Some(Subcode::InvalidMode) => reason::INVALID_MODE,
            _ => reason::MALFORMED,
        },
        _ => reason::MALFORMED,
    }
}

/// The `git_mapping` rows for one pair: bee id → raw git id, and the git id's foreign id
/// → bee id bytes.
pub fn git_mapping_rows(bee: ObjectId, git: &GitOid) -> [DerivedRow; 2] {
    [
        DerivedRow {
            key: bee,
            type_name: GIT_MAPPING_TYPE.to_string(),
            version: GIT_MAPPING_VERSION,
            value: git.as_bytes().to_vec(),
        },
        DerivedRow {
            key: git.to_foreign(),
            type_name: GIT_MAPPING_TYPE.to_string(),
            version: GIT_MAPPING_VERSION,
            value: bee.as_bytes().to_vec(),
        },
    ]
}

/// The bee id recorded for a git id in a repo, from its `git_mapping` rows.
pub async fn lookup_bee_id(
    meta: &dyn bee_store::MetaStore,
    repo: RepoId,
    git: &GitOid,
) -> Result<Option<ObjectId>> {
    match meta
        .get_derived(
            repo,
            &git.to_foreign(),
            GIT_MAPPING_TYPE,
            GIT_MAPPING_VERSION,
            ReadFrom::Primary,
        )
        .await?
    {
        Some(v) => Ok(Some(ObjectId::from_bytes(&v)?)),
        None => Ok(None),
    }
}

/// The git id recorded for a bee changeset, tree or blob in a repo.
pub async fn lookup_git_id(
    meta: &dyn bee_store::MetaStore,
    repo: RepoId,
    bee: &ObjectId,
) -> Result<Option<GitOid>> {
    match meta
        .get_derived(
            repo,
            bee,
            GIT_MAPPING_TYPE,
            GIT_MAPPING_VERSION,
            ReadFrom::Primary,
        )
        .await?
    {
        Some(v) => Ok(Some(GitOid::from_bytes(&v)?)),
        None => Ok(None),
    }
}

/// The exact bytes of an annotated tag object recorded by an import.
pub async fn lookup_git_tag(
    meta: &dyn bee_store::MetaStore,
    repo: RepoId,
    tag: &GitOid,
) -> Result<Option<Vec<u8>>> {
    meta.get_derived(
        repo,
        &tag.to_foreign(),
        GIT_TAG_TYPE,
        GIT_TAG_VERSION,
        ReadFrom::Primary,
    )
    .await
}

/// Objects and changesets waiting to be written.
#[derive(Default)]
struct Batch {
    objects: Vec<(ObjectId, Vec<u8>)>,
    ids: BTreeSet<ObjectId>,
    bytes: usize,
    changesets: Vec<NewChangeset>,
}

impl Batch {
    fn add(&mut self, id: ObjectId, bytes: Vec<u8>) {
        if self.ids.insert(id) {
            self.bytes += bytes.len();
            self.objects.push((id, bytes));
        }
    }
}

/// A ref resolved to its commit.
struct Peeled {
    commit: GitOid,
    tags: Vec<(GitOid, Vec<u8>)>,
}

/// Bee objects prefetched from the stores for one export step.
#[derive(Default)]
struct Scratch {
    blobs: BTreeMap<ObjectId, Vec<u8>>,
    trees: BTreeMap<ObjectId, Tree>,
    metas: BTreeMap<ObjectId, Meta>,
    changesets: BTreeMap<ObjectId, Changeset>,
}

fn not_prefetched(id: &ObjectId) -> BeeError {
    BeeError::new(
        Reason::MissingObject,
        format!("{id} was not read back from the stores"),
    )
    .with_payload("object", id.to_text())
}

impl BeeObjectSource for Scratch {
    fn blob(&self, id: &ObjectId) -> Result<Vec<u8>> {
        self.blobs
            .get(id)
            .cloned()
            .ok_or_else(|| not_prefetched(id))
    }
    fn tree(&self, id: &ObjectId) -> Result<Tree> {
        self.trees
            .get(id)
            .cloned()
            .ok_or_else(|| not_prefetched(id))
    }
    fn meta(&self, id: &ObjectId) -> Result<Meta> {
        self.metas
            .get(id)
            .cloned()
            .ok_or_else(|| not_prefetched(id))
    }
    fn changeset(&self, id: &ObjectId) -> Result<Changeset> {
        self.changesets
            .get(id)
            .cloned()
            .ok_or_else(|| not_prefetched(id))
    }
}

const MAX_TAG_DEPTH: usize = 32;

/// The longest run of missing-object repairs for one changeset before giving up.
const MAX_REPAIRS: usize = 100_000;

struct Run<'a> {
    src: &'a (dyn GitObjectSource + Sync),
    format: ObjectFormat,
    target: &'a ImportTarget,
    opts: &'a ImportOptions,
    importer: Importer,
    report: ImportReport,
    /// Commits imported by an earlier run that the walk reached: `(changeset, root)`.
    known: BTreeMap<GitOid, (ObjectId, ObjectId)>,
    loaded_trees: BTreeSet<ObjectId>,
    /// Parent lists of the commits this run walked.
    parents: BTreeMap<GitOid, Vec<GitOid>>,
    /// Commits named as parents that the source cannot supply, with why.
    missing: BTreeMap<GitOid, String>,
    dropped: BTreeSet<GitOid>,
    /// Commits written by this run, parents first.
    imported: Vec<(GitOid, ObjectId)>,
    batch: Batch,
    gitlinks: BTreeSet<ObjectId>,
    lfs: BTreeSet<crate::report::LfsObject>,
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

impl Run<'_> {
    fn meta(&self) -> &dyn bee_store::MetaStore {
        self.target.writer.meta().as_ref()
    }

    fn blobs(&self) -> &dyn bee_store::BlobStore {
        self.target.writer.blobs().as_ref()
    }

    async fn go(mut self, refs: &[GitRef]) -> Result<ImportOutcome> {
        let mapping = map_ref_names(refs, self.opts.include_pull_refs);
        {
            let c = self.report.category_mut(category::REFS);
            c.excluded = mapping.excluded.len() as u64;
        }
        for d in &mapping.dropped {
            self.report.drop_item(
                category::REFS,
                lossy(&d.git.name),
                d.reason,
                format!("git ref {:?} has no bee name", lossy(&d.git.name)),
            );
        }
        for m in &mapping.mapped {
            if let Some(why) = m.renamed {
                self.report.renamed_refs.push(RenamedRef {
                    git: lossy(&m.git.name),
                    git_hex: bee_core::hex::encode(&m.git.name),
                    bee: m.bee.as_str().to_string(),
                    reason: why.to_string(),
                });
            }
        }
        if let Some(head) = &self.opts.head {
            self.report.default_branch = mapping
                .mapped
                .iter()
                .find(|m| &m.git.name == head)
                .map(|m| m.bee.as_str().to_string());
        }

        // Peel every ref to its commit.
        let mut carried: Vec<(MappedRef, Peeled)> = Vec::new();
        for m in mapping.mapped {
            match self.peel(&m.git.target)? {
                Ok(p) => carried.push((m, p)),
                Err((why, detail)) => {
                    self.report
                        .drop_item(category::REFS, lossy(&m.git.name), why, detail)
                }
            }
        }
        let tips: BTreeSet<GitOid> = carried.iter().map(|(_, p)| p.commit).collect();
        let order = self.walk(&tips).await?;
        self.import_all(&order).await?;
        self.update_refs(&carried).await?;
        if self.opts.verify {
            self.verify(&carried).await?;
        }
        self.report.stats.commits_already_imported = self.known.len() as u64;
        self.report.finish();
        let mut out = self.importer.mapping().clone();
        for (oid, (cs, _)) in &self.known {
            out.insert(*oid, *cs)?;
        }
        Ok(ImportOutcome {
            report: self.report,
            mapping: out,
        })
    }

    /// Follows annotated tags to a commit. The outer `Result` is a fatal error; the inner one
    /// a drop reason with detail.
    #[allow(clippy::type_complexity)]
    fn peel(&self, target: &GitOid) -> Result<std::result::Result<Peeled, (&'static str, String)>> {
        let mut cur = *target;
        let mut tags = Vec::new();
        for _ in 0..=MAX_TAG_DEPTH {
            let raw = match self.src.git_object(&cur) {
                Ok(r) => r,
                Err(e) if is_fatal(&e) => return Err(e),
                Err(e) => return Ok(Err((drop_reason(&e), e.message))),
            };
            match raw.kind {
                ObjectType::Commit => return Ok(Ok(Peeled { commit: cur, tags })),
                ObjectType::Tag => {
                    let tag = match GitTag::parse(&raw.data, self.format) {
                        Ok(t) => t,
                        Err(e) => return Ok(Err((reason::MALFORMED, e.message))),
                    };
                    tags.push((cur, raw.data));
                    cur = tag.object();
                }
                other => {
                    return Ok(Err((
                        reason::TARGET_NOT_COMMIT,
                        format!("{cur} is a {}, not a commit", other.name()),
                    )));
                }
            }
        }
        Ok(Err((
            reason::MALFORMED,
            format!("tag chain from {target} is deeper than {MAX_TAG_DEPTH}"),
        )))
    }

    /// The changeset and root an earlier run recorded for a commit.
    async fn known_commit(&self, oid: &GitOid) -> Result<Option<(ObjectId, ObjectId)>> {
        let Some(id) = lookup_bee_id(self.meta(), self.target.repo, oid).await? else {
            return Ok(None);
        };
        if id.kind() != ObjectKind::Changeset {
            return Ok(None);
        }
        Ok(self
            .meta()
            .get_changeset(self.target.repo, &id, ReadFrom::Primary)
            .await?
            .map(|row| (id, *row.changeset.root())))
    }

    /// Commits to import, parents before children.
    async fn walk(&mut self, tips: &BTreeSet<GitOid>) -> Result<Vec<GitOid>> {
        let mut seen: BTreeSet<GitOid> = BTreeSet::new();
        let mut order = Vec::new();
        let mut stack: Vec<(GitOid, bool)> = tips.iter().rev().map(|t| (*t, false)).collect();
        while let Some((oid, ready)) = stack.pop() {
            if ready {
                order.push(oid);
                continue;
            }
            if !seen.insert(oid) {
                continue;
            }
            if let Some((cs, root)) = self.known_commit(&oid).await? {
                self.importer.add_known_commit(oid, cs, root)?;
                self.known.insert(oid, (cs, root));
                continue;
            }
            if self.opts.shallow.contains(&oid) {
                self.report.stats.commits_walked += 1;
                self.parents.insert(oid, Vec::new());
                order.push(oid);
                continue;
            }
            let raw = match self.src.git_object(&oid) {
                Ok(r) => r,
                Err(e) if is_fatal(&e) => return Err(e),
                Err(e) => {
                    self.missing.insert(oid, e.message);
                    continue;
                }
            };
            if raw.kind != ObjectType::Commit {
                self.missing
                    .insert(oid, format!("{oid} is a {}, not a commit", raw.kind.name()));
                continue;
            }
            self.report.stats.commits_walked += 1;
            let parents = match GitCommit::parse(&raw.data, self.format) {
                Ok(c) => c.parents(),
                // The importer reports the parse error when it converts the commit.
                Err(_) => Vec::new(),
            };
            stack.push((oid, true));
            for p in parents.iter().rev() {
                if !seen.contains(p) {
                    stack.push((*p, false));
                }
            }
            self.parents.insert(oid, parents);
        }
        Ok(order)
    }

    fn drop_commit(&mut self, oid: &GitOid, why: &str, detail: String) {
        self.dropped.insert(*oid);
        self.report
            .drop_item(category::COMMITS, oid.to_hex(), why, detail);
    }

    /// Loads an already imported tree and its subtrees into the importer, so the delta of
    /// the first new child can be computed.
    async fn load_known_tree(&mut self, root: ObjectId) -> Result<()> {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if !self.loaded_trees.insert(id) {
                continue;
            }
            let tree = read_tree(
                self.meta(),
                self.blobs(),
                self.target.org,
                &id,
                ReadFrom::Primary,
            )
            .await?;
            for e in tree.entries() {
                if e.mode == Mode::Tree {
                    stack.push(e.id);
                }
            }
            self.importer.add_known_tree(tree);
        }
        Ok(())
    }

    async fn import_all(&mut self, order: &[GitOid]) -> Result<()> {
        for oid in order {
            let parents = self.parents.get(oid).cloned().unwrap_or_default();
            if self.opts.shallow.contains(oid) {
                self.drop_commit(
                    oid,
                    reason::SHALLOW_BOUNDARY,
                    "its parents are outside the shallow clone".to_string(),
                );
                continue;
            }
            if let Some(p) = parents.iter().find(|p| self.dropped.contains(p)) {
                let detail = format!("parent {p} was dropped");
                self.drop_commit(oid, reason::PARENT_DROPPED, detail);
                continue;
            }
            if let Some((p, why)) = parents
                .iter()
                .find_map(|p| self.missing.get(p).map(|w| (*p, w.clone())))
            {
                let detail = format!("parent {p}: {why}");
                self.drop_commit(oid, reason::MISSING_OBJECT, detail);
                continue;
            }
            if let Some(first) = parents.first()
                && let Some(&(_, root)) = self.known.get(first)
            {
                self.load_known_tree(root).await?;
            }
            match self.importer.import_commit(self.src, oid) {
                Ok(ic) => self.stage(ic)?,
                Err(e) if is_fatal(&e) => return Err(e),
                Err(e) => self.drop_commit(oid, drop_reason(&e), e.message),
            }
            if self.batch.bytes >= self.opts.batch_bytes
                || self.batch.changesets.len() >= self.opts.batch_commits
            {
                self.flush().await?;
            }
        }
        self.flush().await
    }

    fn stage(&mut self, ic: ImportedCommit) -> Result<()> {
        let cs_id = ic.changeset.id();
        self.imported.push((ic.oid, cs_id));
        let mut derived: Vec<DerivedRow> = git_mapping_rows(cs_id, &ic.oid).into();
        self.report.category_mut(category::COMMITS).imported += 1;
        for t in ic.trees {
            self.report.category_mut(category::TREES).imported += 1;
            for e in t.entries() {
                if e.mode == Mode::Gitlink && self.gitlinks.insert(e.id) {
                    self.report.category_mut(category::SUBMODULES).imported += 1;
                }
            }
            let id = t.id();
            if let Some(g) = self.importer.mapping().git(&id) {
                derived.extend(git_mapping_rows(id, &g));
            }
            self.batch.add(id, t.encode());
        }
        for (id, bytes) in ic.blobs {
            self.report.category_mut(category::BLOBS).imported += 1;
            if let Some(p) = lfs_pointer(&bytes)
                && self.lfs.insert(p.clone())
            {
                self.report.category_mut(category::LFS).imported += 1;
                self.report.lfs_objects.push(p);
            }
            if let Some(g) = self.importer.mapping().git(&id) {
                derived.extend(git_mapping_rows(id, &g));
            }
            self.batch.add(id, bytes);
        }
        self.batch.add(ic.meta.id(), ic.meta.encode());
        let object_paths = object_paths_for(&self.importer, &ic.changeset)?;
        self.batch.changesets.push(NewChangeset {
            repo: self.target.repo,
            changeset: ic.changeset,
            object_paths,
            derived,
            at_ms: self.opts.at_ms,
        });
        Ok(())
    }

    /// Uploads objects that have no live row yet, in byte-bounded calls.
    async fn upload(&mut self, objects: Vec<(ObjectId, Vec<u8>)>) -> Result<()> {
        let mut todo = Vec::with_capacity(objects.len());
        for chunk in objects.chunks(1000) {
            let ids: Vec<ObjectId> = chunk.iter().map(|(id, _)| *id).collect();
            let rows = self
                .meta()
                .get_objects(self.target.org, &ids, ReadFrom::Primary)
                .await?;
            for ((id, bytes), row) in chunk.iter().zip(rows) {
                if !matches!(row, Some(r) if r.state == ObjectState::Live) {
                    todo.push((*id, bytes.clone()));
                }
            }
        }
        let mut group: Vec<(ObjectId, Vec<u8>)> = Vec::new();
        let mut group_bytes = 0usize;
        let mut rest = todo.into_iter().peekable();
        while let Some(item) = rest.next() {
            group_bytes += item.1.len();
            group.push(item);
            if group_bytes >= self.opts.batch_bytes || rest.peek().is_none() {
                let sizes: Vec<u64> = group.iter().map(|(_, b)| b.len() as u64).collect();
                let outcomes = self
                    .target
                    .writer
                    .upload(
                        self.target.org,
                        std::mem::take(&mut group),
                        Some(UploadRecord {
                            repo: Some(self.target.repo),
                            principal: self.opts.creator.clone(),
                        }),
                        self.opts.at_ms,
                    )
                    .await?;
                for (o, size) in outcomes.iter().zip(sizes) {
                    if *o == PutOutcome::Inserted {
                        self.report.stats.objects_written += 1;
                        self.report.stats.bytes_written += size;
                    }
                }
                group_bytes = 0;
            }
        }
        Ok(())
    }

    /// Uploads an object the importer converted for a commit that was then dropped (that
    /// commit's output was discarded, but the importer remembers having converted it), with
    /// its `git_mapping` rows.
    async fn repair(&mut self, id: ObjectId) -> Result<()> {
        let never = || {
            BeeError::new(
                Reason::MissingObject,
                format!("{id} is missing from the stores and was never converted"),
            )
        };
        let git = self.importer.mapping().git(&id).ok_or_else(never)?;
        let (bytes, cat) = match id.kind() {
            ObjectKind::Tree => (
                bee_object::TreeSource::tree(&self.importer, &id)?.encode(),
                category::TREES,
            ),
            ObjectKind::Blob => (self.src.git_object(&git)?.data, category::BLOBS),
            _ => return Err(never()),
        };
        self.upload(vec![(id, bytes)]).await?;
        self.meta()
            .put_derived(self.target.repo, git_mapping_rows(id, &git).into())
            .await?;
        self.report.category_mut(cat).imported += 1;
        Ok(())
    }

    async fn insert(&mut self, mut new: NewChangeset) -> Result<()> {
        if let Some(hook) = &self.opts.derived {
            new.derived
                .extend(hook.rows(self.target, &new.changeset).await?);
        }
        for _ in 0..MAX_REPAIRS {
            match self.target.writer.insert_changeset(new.clone()).await {
                Ok(PutOutcome::Inserted) => {
                    self.report.stats.changesets_written += 1;
                    return Ok(());
                }
                Ok(PutOutcome::AlreadyPresent) => return Ok(()),
                Err(e) if e.reason == Reason::MissingObject => {
                    let id = match e.payload.get("object") {
                        Some(PayloadValue::Str(s)) => ObjectId::parse(s).map_err(|_| e.clone())?,
                        _ => return Err(e),
                    };
                    if id.kind() == ObjectKind::Changeset {
                        return Err(e);
                    }
                    self.repair(id).await?;
                }
                Err(e) => return Err(e),
            }
        }
        Err(BeeError::new(
            Reason::Internal,
            format!("changeset {} still misses objects", new.changeset.id()),
        ))
    }

    async fn flush(&mut self) -> Result<()> {
        let batch = std::mem::take(&mut self.batch);
        if !batch.objects.is_empty() {
            self.upload(batch.objects).await?;
        }
        for new in batch.changesets {
            self.insert(new).await?;
        }
        Ok(())
    }

    fn changeset_of(&self, commit: &GitOid) -> Option<ObjectId> {
        self.importer
            .mapping()
            .bee(commit)
            .filter(|id| id.kind() == ObjectKind::Changeset)
            .or_else(|| self.known.get(commit).map(|k| k.0))
    }

    async fn record_tag(&mut self, oid: &GitOid, bytes: &[u8]) -> Result<bool> {
        let repo = self.target.repo;
        if lookup_git_tag(self.meta(), repo, oid).await?.is_some() {
            return Ok(false);
        }
        self.meta()
            .put_derived(
                repo,
                vec![DerivedRow {
                    key: oid.to_foreign(),
                    type_name: GIT_TAG_TYPE.to_string(),
                    version: GIT_TAG_VERSION,
                    value: bytes.to_vec(),
                }],
            )
            .await?;
        self.report.stats.records_written += 1;
        Ok(true)
    }

    async fn update_refs(&mut self, carried: &[(MappedRef, Peeled)]) -> Result<()> {
        let mut tags_seen: BTreeSet<GitOid> = BTreeSet::new();
        for (m, peeled) in carried {
            let Some(cs) = self.changeset_of(&peeled.commit) else {
                let detail = format!("commit {} was not imported", peeled.commit);
                self.report.drop_item(
                    category::REFS,
                    lossy(&m.git.name),
                    reason::TARGET_DROPPED,
                    detail,
                );
                continue;
            };
            for (toid, bytes) in &peeled.tags {
                self.record_tag(toid, bytes).await?;
                if tags_seen.insert(*toid) {
                    self.report.category_mut(category::TAGS).imported += 1;
                }
            }
            let (action, version) = self.set_ref(&m.bee, cs).await?;
            if m.kind == crate::RefKind::Tag
                && let [(toid, bytes)] = peeled.tags.as_slice()
                && GitTag::parse(bytes, self.format).is_ok_and(|t| t.object() == peeled.commit)
            {
                let key = crate::ref_tag_key(&m.bee, &cs)?;
                let known = self
                    .meta()
                    .get_derived(
                        self.target.repo,
                        &key,
                        GIT_REF_TAG_TYPE,
                        GIT_REF_TAG_VERSION,
                        ReadFrom::Primary,
                    )
                    .await?
                    .is_some();
                if !known {
                    self.meta()
                        .put_derived(
                            self.target.repo,
                            vec![DerivedRow {
                                key,
                                type_name: GIT_REF_TAG_TYPE.to_string(),
                                version: GIT_REF_TAG_VERSION,
                                value: toid.as_bytes().to_vec(),
                            }],
                        )
                        .await?;
                    self.report.stats.records_written += 1;
                }
            }
            self.report.category_mut(category::REFS).imported += 1;
            self.report.refs.push(RefRecord {
                git: lossy(&m.git.name),
                bee: m.bee.as_str().to_string(),
                kind: m.kind.as_str().to_string(),
                git_target: m.git.target.to_hex(),
                commit: peeled.commit.to_hex(),
                changeset: cs.to_text(),
                tags: peeled.tags.iter().map(|(t, _)| t.to_hex()).collect(),
                action: action.to_string(),
                version,
            });
        }
        Ok(())
    }

    /// Points a ref at a changeset with the service principal; a ref already there is left
    /// alone (no write).
    async fn set_ref(&mut self, name: &RefName, cs: ObjectId) -> Result<(&'static str, u64)> {
        let repo = self.target.repo;
        let current = self.meta().get_ref(repo, name).await?;
        let (expected, action) = match &current {
            Some(r) if r.target == Some(cs) => return Ok(("unchanged", r.version)),
            Some(r) if r.target.is_some() => (Expected::Version(r.version), "updated"),
            _ => (Expected::Absent, "created"),
        };
        let out = self
            .target
            .writer
            .update_ref(RefUpdate::set(
                repo,
                name.clone(),
                expected,
                Some(cs),
                self.opts.creator.clone(),
                self.opts.at_ms,
            ))
            .await?;
        self.report.stats.refs_written += 1;
        Ok((action, out.new_version))
    }

    /// Reads a changeset and every object under it the exporter has not seen yet.
    async fn prefetch(&self, exp: &Exporter, cs: &ObjectId) -> Result<Scratch> {
        let (meta, blobs, org) = (self.meta(), self.blobs(), self.target.org);
        let mut s = Scratch::default();
        let row = meta
            .get_changeset(self.target.repo, cs, ReadFrom::Primary)
            .await?
            .ok_or_else(|| not_prefetched(cs))?;
        let m = row.changeset.meta();
        let bytes = read_object_bytes(meta, blobs, org, m, ReadFrom::Primary).await?;
        s.metas.insert(*m, Meta::decode(&bytes)?);
        let mut stack = vec![*row.changeset.root()];
        s.changesets.insert(*cs, row.changeset);
        while let Some(id) = stack.pop() {
            if exp.mapping().git(&id).is_some() || s.trees.contains_key(&id) {
                continue;
            }
            let tree = read_tree(meta, blobs, org, &id, ReadFrom::Primary).await?;
            for e in tree.entries() {
                match e.mode {
                    Mode::Tree => stack.push(e.id),
                    Mode::Gitlink => {}
                    _ => {
                        if exp.mapping().git(&e.id).is_none() && !s.blobs.contains_key(&e.id) {
                            let b = read_object_bytes(meta, blobs, org, &e.id, ReadFrom::Primary)
                                .await?;
                            s.blobs.insert(e.id, b);
                        }
                    }
                }
            }
            s.trees.insert(id, tree);
        }
        Ok(s)
    }

    async fn verify(&mut self, carried: &[(MappedRef, Peeled)]) -> Result<()> {
        let mut seed = GitMapping::new();
        for (oid, (cs, _)) in &self.known {
            seed.insert(*oid, *cs)?;
        }
        let mut exp = Exporter::with_mapping(self.format, seed);
        let mut v = Verification {
            performed: true,
            ..Verification::default()
        };
        let imported = std::mem::take(&mut self.imported);
        for (oid, cs) in &imported {
            v.commits_checked += 1;
            let scratch = self.prefetch(&exp, cs).await?;
            match exp.export_changeset(&scratch, cs, &mut |_, _| Ok(())) {
                Ok(out) if out.oid == *oid => {}
                Ok(out) => v.commit_id_mismatches.push(format!("{oid} -> {}", out.oid)),
                Err(e) => v
                    .commit_id_mismatches
                    .push(format!("{oid} -> export failed: {}", e.message)),
            }
            let stored = self.meta().parents(self.target.repo, cs).await?;
            let mut want: Vec<ObjectId> = Vec::new();
            for p in self.parents.get(oid).map(Vec::as_slice).unwrap_or_default() {
                if let Some(id) = self.changeset_of(p)
                    && !want.contains(&id)
                {
                    want.push(id);
                }
            }
            if stored != want {
                v.graph_mismatches.push(format!(
                    "{oid}: stored parents {} differ from git parents {}",
                    stored.len(),
                    want.len()
                ));
            }
        }
        self.imported = imported;
        for (m, peeled) in carried {
            let Some(r) = self.meta().get_ref(self.target.repo, &m.bee).await? else {
                continue;
            };
            let Some(target) = r.target else { continue };
            v.refs_checked += 1;
            let git = exp
                .mapping()
                .git(&target)
                .or_else(|| self.importer.mapping().git(&target));
            if git != Some(peeled.commit) {
                v.ref_mismatches.push(format!(
                    "{}: {} does not export to {}",
                    m.bee, target, peeled.commit
                ));
            }
        }
        self.report.verification = v;
        Ok(())
    }
}
