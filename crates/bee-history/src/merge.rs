//! Tree-level rebase, merge and land helpers (SPEC §7.3, §7.4).
//!
//! All three are pure functions of their inputs (no clock, no randomness; `at_ms` only stamps
//! rows) and examine only the paths that changed:
//!
//! - **Rebase(C, onto X)** looks at C's delta paths (plus, only when X deleted a path C
//!   modified, the paths X added, to follow a rename). Paths where one side kept the base
//!   resolve from entries alone, so a rebase across unrelated changes reads no blobs.
//! - **Merge(A, B)** looks at the paths either side changed since the merge base. Several
//!   best bases (criss-cross) are folded into a virtual base (`diff3-v1` §7) over the same
//!   restricted path set, extended with every path any involved base differs on.
//! - Contested paths go through `bee-merge`'s rename- and copy-aware tree merge; when a
//!   merge produces an `add_add` the merge is recomputed over whole trees so copy sources
//!   outside the changed paths are seen, exactly as the contract specifies.
//! - Structure (rule 9) is checked against the tree being merged onto for paths outside the
//!   examined set.
//!
//! Conflicts are data ([`ConflictSet`]): every conflicting path with its sides and hunks, plus
//! the resolved entry of every clean examined path (merged blobs are uploaded, so a resolver
//! can reference them).

use std::collections::{BTreeMap, BTreeSet};

use bee_core::{
    BeeError, ChangeId, Mode, ObjectId, PayloadValue, Reason, RepoPath, Result, Subcode,
};
use bee_merge::{
    ConflictHunk, ConflictKind, Content, MergeGraph, OwnedContent, OwnedSide, OwnedTree,
    RenameRecord, Side, TreeContent, TreeMergeOptions, VirtualBase, Which,
};
use bee_object::{DeltaEntry, DeltaOp, blob_id};

use crate::pathhist::LeafEntry;
use crate::repo::{BlobContent, History};

/// The `bee-merge` mode of a tree entry mode.
pub fn merge_mode(m: Mode) -> bee_merge::Mode {
    match m {
        Mode::File => bee_merge::Mode::File,
        Mode::Executable => bee_merge::Mode::Executable,
        Mode::Symlink => bee_merge::Mode::Symlink,
        Mode::Gitlink | Mode::Tree => bee_merge::Mode::Gitlink,
    }
}

/// The tree entry mode of a `bee-merge` mode.
pub fn core_mode(m: bee_merge::Mode) -> Mode {
    match m {
        bee_merge::Mode::File => Mode::File,
        bee_merge::Mode::Executable => Mode::Executable,
        bee_merge::Mode::Symlink => Mode::Symlink,
        bee_merge::Mode::Gitlink => Mode::Gitlink,
    }
}

/// A `bee-merge` side for loaded content; redacted content compares by object id.
pub fn side_of<'a>(mode: bee_merge::Mode, content: &'a BlobContent, id: &'a ObjectId) -> Side<'a> {
    Side {
        mode,
        content: match content {
            BlobContent::Bytes(b) => Content::Bytes(b),
            BlobContent::Redacted => Content::Redacted(id.as_bytes()),
        },
    }
}

/// Rebase options.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RebaseOptions {
    /// Allow a result with an empty delta (otherwise `EMPTY_CHANGE`).
    pub allow_empty: bool,
    /// Merge limits and rename detection.
    pub tree: TreeMergeOptions,
}

/// Merge options.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeOptions {
    /// Allow a result with an empty delta from the first parent (otherwise `EMPTY_CHANGE`).
    pub allow_empty: bool,
    /// Merge limits and rename detection.
    pub tree: TreeMergeOptions,
}

/// The base a merge used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeBase {
    /// Unrelated histories: the empty tree.
    None,
    /// One best common ancestor.
    Unique(ObjectId),
    /// Several best common ancestors folded into a virtual base.
    Virtual(Vec<ObjectId>),
}

/// One side of a conflicting path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictSide {
    /// The side's path (differs from the conflict path when a rename was followed).
    pub path: RepoPath,
    /// Mode.
    pub mode: Mode,
    /// Object id; `None` for virtual-base content that exists only in the payload.
    pub id: Option<ObjectId>,
    /// Size in bytes.
    pub size: u64,
    /// The bytes, for virtual-base content only.
    pub content: Option<Vec<u8>>,
}

/// A conflicting path (SPEC §7.3.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathConflictData {
    /// The result path.
    pub path: RepoPath,
    /// Why it conflicts.
    pub kind: ConflictKind,
    /// The base side.
    pub base: Option<ConflictSide>,
    /// Ours (the side merged onto).
    pub ours: Option<ConflictSide>,
    /// Theirs (the side merged in).
    pub theirs: Option<ConflictSide>,
    /// Conflicting line regions, when a line merge ran.
    pub hunks: Vec<ConflictHunk>,
    /// The base content came from resolving an inner conflict of a virtual base.
    pub base_conflicted: bool,
}

/// Conflicts as data, plus the resolution of every clean examined path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictSet {
    /// The conflicting paths, sorted.
    pub conflicts: Vec<PathConflictData>,
    /// Every clean examined path and its resolved entry (`None` = absent), sorted.
    pub resolved: Vec<(RepoPath, Option<LeafEntry>)>,
    /// The base used.
    pub base: MergeBase,
    /// Renames that were followed.
    pub renames: Vec<RenameRecord>,
}

impl ConflictSet {
    /// The `MERGE_CONFLICT` error carrying the conflicts under `conflicts` (path and kind).
    pub fn to_error(&self) -> BeeError {
        let list = self
            .conflicts
            .iter()
            .map(|c| {
                let mut m = BTreeMap::new();
                m.insert("path".to_string(), PayloadValue::Str(c.path.to_string()));
                m.insert(
                    "kind".to_string(),
                    PayloadValue::Str(c.kind.as_str().to_string()),
                );
                PayloadValue::Map(m)
            })
            .collect();
        BeeError::new(
            Reason::MergeConflict,
            format!("{} conflicting paths", self.conflicts.len()),
        )
        .with_payload("conflicts", PayloadValue::List(list))
    }
}

/// The result of [`History::rebase`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// The target already is the changeset's parent; the changeset is returned as is.
    Unchanged(ObjectId),
    /// The new changeset (same meta and ChangeId).
    Rebased(ObjectId),
    /// The rebase conflicts.
    Conflicts(ConflictSet),
}

/// The result of [`History::merge`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The new merge changeset.
    Merged {
        /// Its id.
        id: ObjectId,
        /// The base used.
        base: MergeBase,
    },
    /// The merge conflicts.
    Conflicts(ConflictSet),
}

/// One member of a landed stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackStep {
    /// The changeset asked to land.
    pub source: ObjectId,
    /// The changeset now on the ref (the rebased source, or the source itself).
    pub result: ObjectId,
    /// Its ChangeId.
    pub change_id: ChangeId,
}

/// The result of [`History::rebase_stack`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StackOutcome {
    /// Every member rebased, oldest first.
    Rebased(Vec<StackStep>),
    /// A member conflicts.
    Conflict {
        /// Its position in the stack.
        index: usize,
        /// The member.
        source: ObjectId,
        /// The conflicts.
        conflicts: ConflictSet,
    },
}

/// The base tree of a three-way merge.
enum BaseTree {
    Empty,
    Real(ObjectId),
    Virtual(VirtualBase),
}

/// The outcome of a restricted three-way merge.
struct ThreeWay {
    /// Result for every examined path (`None` = absent); conflicted paths keep ours.
    result: BTreeMap<RepoPath, Option<LeafEntry>>,
    conflicts: Vec<PathConflictData>,
    blobs: BTreeMap<ObjectId, Vec<u8>>,
    renames: Vec<RenameRecord>,
    /// The merge produced `add_add` (or an inner virtual-base conflict): copy sources outside
    /// the examined paths may matter.
    needs_full: bool,
    /// Some path was deleted by ours and modified by theirs.
    ours_deleted_modified: Vec<RepoPath>,
}

type FlatIds = BTreeMap<Vec<u8>, (bee_merge::Mode, ObjectId)>;

/// Borrows loaded contents as a `bee-merge` flat tree.
fn to_tree<'a>(
    f: &'a FlatIds,
    contents: &'a BTreeMap<ObjectId, BlobContent>,
) -> Result<bee_merge::Tree<'a>> {
    f.iter()
        .map(|(p, (mode, id))| {
            let c = contents.get(id).ok_or_else(|| {
                BeeError::new(Reason::Internal, format!("content of {id} not loaded"))
            })?;
            Ok((p.clone(), side_of(*mode, c, id)))
        })
        .collect()
}

fn path_of(bytes: &[u8]) -> Result<RepoPath> {
    RepoPath::new(bytes.to_vec())
}

fn multi_parent(id: &ObjectId, n: usize) -> BeeError {
    BeeError::invalid(
        Subcode::InvalidParents,
        format!("{id} has {n} parents; only single-parent changesets rebase"),
    )
    .with_payload("rule", "multi_parent")
}

/// A virtual-base graph whose answers were computed ahead of time (the merge-graph trait is
/// synchronous; the store is not).
struct Prepared {
    bases: BTreeMap<(Vec<ObjectId>, ObjectId), Vec<ObjectId>>,
    trees: BTreeMap<ObjectId, OwnedTree>,
}

impl MergeGraph for Prepared {
    type Id = ObjectId;

    fn merge_bases(&self, left: &[ObjectId], right: &[ObjectId]) -> Vec<ObjectId> {
        let key = (left.to_vec(), right.first().copied().unwrap_or(left[0]));
        self.bases.get(&key).cloned().unwrap_or_default()
    }

    fn tree(&self, id: &ObjectId) -> OwnedTree {
        self.trees.get(id).cloned().unwrap_or_default()
    }
}

impl History {
    async fn leaves_at(
        &self,
        root: &ObjectId,
        paths: &BTreeSet<RepoPath>,
    ) -> Result<BTreeMap<RepoPath, LeafEntry>> {
        let mut out = BTreeMap::new();
        for p in paths {
            if let Some(e) = self.entry_at(root, p).await?
                && e.is_leaf()
            {
                out.insert(
                    p.clone(),
                    LeafEntry {
                        mode: e.mode,
                        id: e.id,
                        size: e.size,
                    },
                );
            }
        }
        Ok(out)
    }

    async fn all_leaf_paths(&self, root: &ObjectId, into: &mut BTreeSet<RepoPath>) -> Result<()> {
        for (p, _) in self.list_leaves(root, None).await? {
            into.insert(p);
        }
        Ok(())
    }

    async fn load_contents(
        &self,
        entries: &BTreeMap<RepoPath, LeafEntry>,
        paths: &BTreeSet<RepoPath>,
        cache: &mut BTreeMap<ObjectId, BlobContent>,
    ) -> Result<()> {
        for p in paths {
            if let Some(e) = entries.get(p)
                && !cache.contains_key(&e.id)
            {
                cache.insert(e.id, self.leaf_content(e.mode, &e.id).await?);
            }
        }
        Ok(())
    }

    /// The flat tree of `root` restricted to `paths`, with contents.
    async fn owned_tree(&self, root: &ObjectId, paths: &BTreeSet<RepoPath>) -> Result<OwnedTree> {
        let entries = self.leaves_at(root, paths).await?;
        let mut out = OwnedTree::new();
        for (p, e) in entries {
            let content = match self.leaf_content(e.mode, &e.id).await? {
                BlobContent::Bytes(b) => OwnedContent::Bytes(b),
                BlobContent::Redacted => OwnedContent::Redacted(e.id.as_bytes().to_vec()),
            };
            out.insert(
                p.into_bytes(),
                OwnedSide {
                    mode: merge_mode(e.mode),
                    content,
                },
            );
        }
        Ok(out)
    }

    /// Every changeset a virtual base over `bases` folds, with the merge-base answers it
    /// needs (`diff3-v1` §7, replayed ahead of time).
    async fn prepare_virtual(
        &self,
        bases: &[ObjectId],
    ) -> Result<(
        BTreeMap<(Vec<ObjectId>, ObjectId), Vec<ObjectId>>,
        BTreeSet<ObjectId>,
    )> {
        let mut answers = BTreeMap::new();
        let mut involved = BTreeSet::new();
        let mut work: Vec<(Vec<ObjectId>, usize)> = vec![(bases.to_vec(), 0)];
        while let Some((mut ids, depth)) = work.pop() {
            ids.sort();
            ids.dedup();
            involved.extend(ids.iter().copied());
            let Some((first, rest)) = ids.split_first() else {
                continue;
            };
            let mut folded = vec![*first];
            for next in rest {
                if depth < 64 {
                    let key = (folded.clone(), *next);
                    if let std::collections::btree_map::Entry::Vacant(v) = answers.entry(key) {
                        let mb = self
                            .merge_bases_of(&folded, std::slice::from_ref(next))
                            .await?;
                        work.push((mb.clone(), depth + 1));
                        v.insert(mb);
                    }
                }
                folded.push(*next);
            }
        }
        Ok((answers, involved))
    }

    async fn virtual_base_for(
        &self,
        bases: &[ObjectId],
        answers: BTreeMap<(Vec<ObjectId>, ObjectId), Vec<ObjectId>>,
        involved: &BTreeSet<ObjectId>,
        paths: &BTreeSet<RepoPath>,
        opts: &TreeMergeOptions,
    ) -> Result<VirtualBase> {
        let mut trees = BTreeMap::new();
        for id in involved {
            let root = *self.changeset(id).await?.root();
            trees.insert(*id, self.owned_tree(&root, paths).await?);
        }
        let graph = Prepared {
            bases: answers,
            trees,
        };
        Ok(bee_merge::virtual_base(&graph, bases, opts))
    }

    /// The three-way merge of `paths` (everything else is identical on the sides that
    /// matter, or resolved by the caller).
    async fn three_way(
        &self,
        base: &BaseTree,
        ours_root: &ObjectId,
        theirs_root: &ObjectId,
        paths: &BTreeSet<RepoPath>,
        opts: &TreeMergeOptions,
        whole: bool,
    ) -> Result<ThreeWay> {
        let ours = self.leaves_at(ours_root, paths).await?;
        let theirs = self.leaves_at(theirs_root, paths).await?;
        let base_entries = match base {
            BaseTree::Real(r) => self.leaves_at(r, paths).await?,
            _ => BTreeMap::new(),
        };
        let mut out = ThreeWay {
            result: BTreeMap::new(),
            conflicts: Vec::new(),
            blobs: BTreeMap::new(),
            renames: Vec::new(),
            needs_full: false,
            ours_deleted_modified: Vec::new(),
        };
        let mut contested: BTreeSet<RepoPath> = BTreeSet::new();
        let mut one_sided: Vec<(RepoPath, Option<LeafEntry>)> = Vec::new();
        let mut deleted_both = false;
        let (mut ours_adds, mut theirs_adds) = (false, false);
        if whole || matches!(base, BaseTree::Virtual(_)) {
            contested = paths.clone();
        } else {
            for p in paths {
                let (b, o, t) = (base_entries.get(p), ours.get(p), theirs.get(p));
                ours_adds |= b.is_none() && o.is_some();
                theirs_adds |= b.is_none() && t.is_some();
                if o == t {
                    deleted_both |= b.is_some() && o.is_none();
                    out.result.insert(p.clone(), o.copied());
                } else if b == o {
                    if b.is_some() && t.is_some() {
                        out.result.insert(p.clone(), t.copied());
                    } else {
                        one_sided.push((p.clone(), t.copied()));
                    }
                } else if b == t {
                    if b.is_some() && o.is_some() {
                        out.result.insert(p.clone(), o.copied());
                    } else {
                        one_sided.push((p.clone(), o.copied()));
                    }
                } else {
                    if b.is_some() && o.is_none() && t.is_some() {
                        out.ours_deleted_modified.push(p.clone());
                    }
                    contested.insert(p.clone());
                }
            }
        }
        let renames_on = opts.renames.enabled;
        let trigger =
            !contested.is_empty() || (renames_on && deleted_both && ours_adds && theirs_adds);
        if !trigger {
            for (p, e) in one_sided {
                out.result.insert(p, e);
            }
            return Ok(out);
        }
        let mut m = contested;
        m.extend(one_sided.into_iter().map(|(p, _)| p));
        if deleted_both {
            for p in paths {
                if base_entries.contains_key(p) && !ours.contains_key(p) && !theirs.contains_key(p)
                {
                    m.insert(p.clone());
                    out.result.remove(p);
                }
            }
        }
        let mut contents: BTreeMap<ObjectId, BlobContent> = BTreeMap::new();
        self.load_contents(&ours, &m, &mut contents).await?;
        self.load_contents(&theirs, &m, &mut contents).await?;
        self.load_contents(&base_entries, &m, &mut contents).await?;
        let flat = |entries: &BTreeMap<RepoPath, LeafEntry>| -> FlatIds {
            entries
                .iter()
                .filter(|(p, _)| m.contains(*p))
                .map(|(p, e)| (p.as_bytes().to_vec(), (merge_mode(e.mode), e.id)))
                .collect()
        };
        let (fo, ft, fb) = (flat(&ours), flat(&theirs), flat(&base_entries));
        let (to, tt) = (to_tree(&fo, &contents)?, to_tree(&ft, &contents)?);
        let (tb, virtual_conflicted) = match base {
            BaseTree::Virtual(vb) => {
                let t: bee_merge::Tree<'_> = vb
                    .tree
                    .iter()
                    .filter(|(p, _)| path_of(p).is_ok_and(|p| m.contains(&p)))
                    .map(|(p, s)| (p.clone(), s.as_side()))
                    .collect();
                (t, vb.conflicted.clone())
            }
            _ => (to_tree(&fb, &contents)?, BTreeSet::new()),
        };
        let tm = bee_merge::merge_trees(&tb, &to, &tt, opts);
        out.needs_full = renames_on
            && (tm
                .conflicts
                .values()
                .any(|c| c.kind == ConflictKind::AddAdd)
                || !virtual_conflicted.is_empty());
        out.renames = tm.renames.clone();
        for p in &m {
            out.result.insert(p.clone(), None);
        }
        let lookup = |which: Which, p: &[u8]| -> Option<LeafEntry> {
            let map = match which {
                Which::Ours => &ours,
                Which::Theirs => &theirs,
                Which::Base => &base_entries,
            };
            path_of(p).ok().and_then(|p| map.get(&p).copied())
        };
        for (path, entry) in &tm.entries {
            let mode = core_mode(entry.mode);
            let le = match &entry.content {
                TreeContent::Side { side, path: sp } => match lookup(*side, sp) {
                    Some(e) => LeafEntry { mode, ..e },
                    None => {
                        // Virtual-base content: materialize it as a blob.
                        let bytes = match base {
                            BaseTree::Virtual(vb) => match vb.tree.get(sp).map(|s| &s.content) {
                                Some(OwnedContent::Bytes(b)) => b.clone(),
                                _ => Vec::new(),
                            },
                            _ => Vec::new(),
                        };
                        let id = blob_id(&bytes);
                        let size = bytes.len() as u64;
                        out.blobs.insert(id, bytes);
                        LeafEntry { mode, id, size }
                    }
                },
                TreeContent::Merged(bytes) => {
                    let id = blob_id(bytes);
                    out.blobs.insert(id, bytes.clone());
                    LeafEntry {
                        mode,
                        id,
                        size: bytes.len() as u64,
                    }
                }
            };
            out.result.insert(path_of(path)?, Some(le));
        }
        for (path, c) in &tm.conflicts {
            let p = path_of(path)?;
            let side =
                |which: Which, s: &Option<(Vec<u8>, Side<'_>)>| -> Result<Option<ConflictSide>> {
                    let Some((sp, side)) = s else { return Ok(None) };
                    let spath = path_of(sp)?;
                    Ok(Some(match lookup(which, sp) {
                        Some(e) => ConflictSide {
                            path: spath,
                            mode: e.mode,
                            id: Some(e.id),
                            size: e.size,
                            content: None,
                        },
                        None => {
                            let bytes = side.content.bytes().unwrap_or_default().to_vec();
                            ConflictSide {
                                path: spath,
                                mode: core_mode(side.mode),
                                id: None,
                                size: bytes.len() as u64,
                                content: Some(bytes),
                            }
                        }
                    }))
                };
            out.result.insert(p.clone(), ours.get(&p).copied());
            out.conflicts.push(PathConflictData {
                path: p,
                kind: c.kind,
                base: side(Which::Base, &c.base)?,
                ours: side(Which::Ours, &c.ours)?,
                theirs: side(Which::Theirs, &c.theirs)?,
                hunks: c.hunks.clone(),
                base_conflicted: virtual_conflicted.contains(path),
            });
        }
        Ok(out)
    }

    /// Rule 9 against `ours_root` for the parts of the tree outside the examined paths.
    async fn structural_conflicts(&self, ours_root: &ObjectId, tw: &mut ThreeWay) -> Result<()> {
        let conflicted: BTreeSet<RepoPath> = tw.conflicts.iter().map(|c| c.path.clone()).collect();
        let mut extra: BTreeMap<RepoPath, ()> = BTreeMap::new();
        for (p, e) in &tw.result {
            if e.is_none() || conflicted.contains(p) {
                continue;
            }
            for a in p.ancestors() {
                let leaf_there = match tw.result.get(&a) {
                    Some(r) => r.is_some(),
                    None => self
                        .entry_at(ours_root, &a)
                        .await?
                        .is_some_and(|e| e.is_leaf()),
                };
                if leaf_there {
                    extra.insert(p.clone(), ());
                    extra.insert(a, ());
                }
            }
            if let Some(dir) = self.entry_at(ours_root, p).await?
                && dir.mode == Mode::Tree
            {
                for (q, _) in self.list_leaves(&dir.id, Some(p)).await? {
                    if !matches!(tw.result.get(&q), Some(None)) {
                        extra.insert(p.clone(), ());
                        extra.insert(q, ());
                    }
                }
            }
        }
        for p in extra.into_keys() {
            if conflicted.contains(&p) {
                continue;
            }
            let ours = self.entry_at(ours_root, &p).await?.filter(|e| e.is_leaf());
            let theirs = tw.result.get(&p).copied().flatten();
            let side = |e: Option<LeafEntry>| {
                e.map(|e| ConflictSide {
                    path: p.clone(),
                    mode: e.mode,
                    id: Some(e.id),
                    size: e.size,
                    content: None,
                })
            };
            tw.conflicts.push(PathConflictData {
                path: p.clone(),
                kind: ConflictKind::FileDirectory,
                base: None,
                ours: side(ours.map(|e| LeafEntry {
                    mode: e.mode,
                    id: e.id,
                    size: e.size,
                })),
                theirs: side(theirs),
                hunks: Vec::new(),
                base_conflicted: false,
            });
        }
        tw.conflicts.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(())
    }

    /// Turns a merge result into a changeset on `parents` (or a conflict set).
    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &self,
        parents: Vec<ObjectId>,
        meta: ObjectId,
        change_id: ChangeId,
        ours_root: &ObjectId,
        mut tw: ThreeWay,
        base: MergeBase,
        allow_empty: bool,
        at_ms: u64,
    ) -> Result<std::result::Result<ObjectId, ConflictSet>> {
        self.structural_conflicts(ours_root, &mut tw).await?;
        if !tw.blobs.is_empty() {
            self.writer()
                .upload(
                    self.org(),
                    tw.blobs.clone().into_iter().collect(),
                    None,
                    at_ms,
                )
                .await?;
        }
        if !tw.conflicts.is_empty() {
            let conflicted: BTreeSet<&RepoPath> = tw.conflicts.iter().map(|c| &c.path).collect();
            let resolved = tw
                .result
                .iter()
                .filter(|(p, _)| !conflicted.contains(p))
                .map(|(p, e)| (p.clone(), *e))
                .collect();
            return Ok(Err(ConflictSet {
                conflicts: tw.conflicts,
                resolved,
                base,
                renames: tw.renames,
            }));
        }
        let ours = self
            .leaves_at(ours_root, &tw.result.keys().cloned().collect())
            .await?;
        let mut delta = Vec::new();
        for (p, r) in &tw.result {
            let o = ours.get(p);
            if o == r.as_ref() {
                continue;
            }
            delta.push(match r {
                Some(e) => DeltaEntry::put(p.clone(), e.mode, e.id, e.size),
                None => DeltaEntry::delete(p.clone()),
            });
        }
        if delta.is_empty() && !allow_empty {
            return Err(BeeError::new(
                Reason::EmptyChange,
                "the result has no changes from its first parent",
            ));
        }
        let id = self
            .create_changeset(parents, meta, change_id, delta, at_ms)
            .await?;
        Ok(Ok(id))
    }

    /// Rebase(C, onto X) (SPEC §7.3): replays C's delta on X. Keeps C's meta and ChangeId.
    /// `Unchanged` when X already is C's parent; `EMPTY_CHANGE` for an empty result unless
    /// allowed; a multi-parent or parentless C is `INVALID_ARGUMENT`/`INVALID_PARENTS`
    /// (payload `rule` = `multi_parent`).
    pub async fn rebase(
        &self,
        c: &ObjectId,
        onto: &ObjectId,
        opts: &RebaseOptions,
        at_ms: u64,
    ) -> Result<RebaseOutcome> {
        self.rebase_with(c, onto, opts, at_ms, false).await
    }

    /// [`History::rebase`], and with `keep_merges` a merge changeset too: its delta from its
    /// first parent is replayed onto `onto` and its other parents are kept, so the result is
    /// a merge of `onto` with the same side branches (SPEC §7.4).
    async fn rebase_with(
        &self,
        c: &ObjectId,
        onto: &ObjectId,
        opts: &RebaseOptions,
        at_ms: u64,
        keep_merges: bool,
    ) -> Result<RebaseOutcome> {
        let cs = self.changeset(c).await?;
        if cs.parents().is_empty() || (cs.parents().len() > 1 && !keep_merges) {
            return Err(multi_parent(c, cs.parents().len()));
        }
        let parent = cs.parents()[0];
        if parent == *onto {
            return Ok(RebaseOutcome::Unchanged(*c));
        }
        let base_root = *self.changeset(&parent).await?.root();
        let ours_root = *self.changeset(onto).await?.root();
        let mut paths: BTreeSet<RepoPath> = cs.delta().iter().map(|e| e.path.clone()).collect();
        let base = BaseTree::Real(base_root);
        let mut tw = self
            .three_way(&base, &ours_root, cs.root(), &paths, &opts.tree, false)
            .await?;
        if opts.tree.renames.enabled && !tw.ours_deleted_modified.is_empty() {
            // Ours deleted a path C modified: follow a rename ours may have made.
            let gone = self
                .leaves_at(
                    &base_root,
                    &tw.ours_deleted_modified.iter().cloned().collect(),
                )
                .await?;
            let gone_ids: BTreeSet<ObjectId> = gone.values().map(|e| e.id).collect();
            let mut exact = BTreeSet::new();
            let mut added = BTreeSet::new();
            for e in self
                .diff_trees(Some(&base_root), Some(&ours_root), None)
                .await?
            {
                if let DeltaOp::Put { id, .. } = e.op
                    && self.entry_at(&base_root, &e.path).await?.is_none()
                {
                    if gone_ids.contains(&id) {
                        exact.insert(e.path.clone());
                    }
                    added.insert(e.path);
                }
            }
            let extra = if exact.is_empty() { added } else { exact };
            if !extra.is_empty() {
                paths.extend(extra);
                tw = self
                    .three_way(&base, &ours_root, cs.root(), &paths, &opts.tree, false)
                    .await?;
            }
        }
        let mut parents = vec![*onto];
        parents.extend(cs.parents()[1..].iter().copied());
        let out = self
            .finish(
                parents,
                *cs.meta(),
                *cs.change_id(),
                &ours_root,
                tw,
                MergeBase::Unique(parent),
                opts.allow_empty,
                at_ms,
            )
            .await?;
        Ok(match out {
            Ok(id) => RebaseOutcome::Rebased(id),
            Err(c) => RebaseOutcome::Conflicts(c),
        })
    }

    /// Merge(ours, theirs) (SPEC §7.3): a new changeset with parents `[ours, theirs]`, the
    /// given meta (already uploaded) and ChangeId. Uses the unique best merge base, a virtual
    /// base for several, the empty tree for none.
    pub async fn merge(
        &self,
        ours: &ObjectId,
        theirs: &ObjectId,
        meta: ObjectId,
        change_id: ChangeId,
        opts: &MergeOptions,
        at_ms: u64,
    ) -> Result<MergeOutcome> {
        let ours_root = *self.changeset(ours).await?.root();
        let theirs_root = *self.changeset(theirs).await?.root();
        let bases = self.merge_bases(ours, theirs).await?;
        let merge_base = match bases.len() {
            0 => MergeBase::None,
            1 => MergeBase::Unique(bases[0]),
            _ => MergeBase::Virtual(bases.clone()),
        };
        let mut full = false;
        loop {
            let mut paths = BTreeSet::new();
            let base = match &merge_base {
                MergeBase::None | MergeBase::Unique(_) => {
                    let root = match &merge_base {
                        MergeBase::Unique(b) => Some(*self.changeset(b).await?.root()),
                        _ => None,
                    };
                    if full {
                        for r in root.iter().chain([&ours_root, &theirs_root]) {
                            self.all_leaf_paths(r, &mut paths).await?;
                        }
                    } else {
                        for side in [&ours_root, &theirs_root] {
                            for e in self.diff_trees(root.as_ref(), Some(side), None).await? {
                                paths.insert(e.path);
                            }
                        }
                    }
                    match root {
                        Some(r) => BaseTree::Real(r),
                        None => BaseTree::Empty,
                    }
                }
                MergeBase::Virtual(ids) => {
                    let (answers, involved) = self.prepare_virtual(ids).await?;
                    let mut roots = vec![theirs_root];
                    for id in &involved {
                        roots.push(*self.changeset(id).await?.root());
                    }
                    if full {
                        self.all_leaf_paths(&ours_root, &mut paths).await?;
                        for r in &roots {
                            self.all_leaf_paths(r, &mut paths).await?;
                        }
                    } else {
                        for r in &roots {
                            for e in self.diff_trees(Some(&ours_root), Some(r), None).await? {
                                paths.insert(e.path);
                            }
                        }
                    }
                    BaseTree::Virtual(
                        self.virtual_base_for(ids, answers, &involved, &paths, &opts.tree)
                            .await?,
                    )
                }
            };
            let tw = self
                .three_way(&base, &ours_root, &theirs_root, &paths, &opts.tree, full)
                .await?;
            if tw.needs_full && !full {
                full = true;
                continue;
            }
            let out = self
                .finish(
                    vec![*ours, *theirs],
                    meta,
                    change_id,
                    &ours_root,
                    tw,
                    merge_base.clone(),
                    opts.allow_empty,
                    at_ms,
                )
                .await?;
            return Ok(match out {
                Ok(id) => MergeOutcome::Merged {
                    id,
                    base: merge_base,
                },
                Err(c) => MergeOutcome::Conflicts(c),
            });
        }
    }

    /// The stack Land would put on a ref whose tip is `tip` (SPEC §7.4 step 2): the
    /// first-parent chain from `head` back to (excluding) the first ancestor of `tip`, oldest
    /// first. Merge changesets may be members: the walk follows first parents, and their side
    /// branches come along when they are rebased ([`History::rebase_stack`]).
    pub async fn land_stack(&self, head: &ObjectId, tip: &ObjectId) -> Result<Vec<ObjectId>> {
        let mut out = Vec::new();
        let mut x = *head;
        loop {
            if self.is_ancestor(&x, tip).await? {
                break;
            }
            let node = self.graph_node(&x).await?;
            out.push(x);
            match node.parents.first() {
                Some(p) => x = *p,
                None => break,
            }
        }
        out.reverse();
        Ok(out)
    }

    /// Rebases a stack (oldest first, each the parent of the next) onto `onto`, each member
    /// onto the previous result (SPEC §7.4 step 3). Members already in place are kept.
    pub async fn rebase_stack(
        &self,
        stack: &[ObjectId],
        onto: &ObjectId,
        opts: &RebaseOptions,
        at_ms: u64,
    ) -> Result<StackOutcome> {
        let mut steps = Vec::with_capacity(stack.len());
        let mut tip = *onto;
        // With `allow_empty`, a member that rebases to nothing writes no changeset: it is
        // recorded with the running tip as its result (SPEC §7.4).
        let strict = RebaseOptions {
            allow_empty: false,
            ..*opts
        };
        for (index, source) in stack.iter().enumerate() {
            let change_id = *self.changeset(source).await?.change_id();
            let rebased = match self.rebase_with(source, &tip, &strict, at_ms, true).await {
                Err(e) if opts.allow_empty && e.reason == Reason::EmptyChange => {
                    steps.push(StackStep {
                        source: *source,
                        result: tip,
                        change_id,
                    });
                    continue;
                }
                other => other?,
            };
            let result = match rebased {
                RebaseOutcome::Unchanged(id) | RebaseOutcome::Rebased(id) => id,
                RebaseOutcome::Conflicts(conflicts) => {
                    return Ok(StackOutcome::Conflict {
                        index,
                        source: *source,
                        conflicts,
                    });
                }
            };
            steps.push(StackStep {
                source: *source,
                result,
                change_id,
            });
            tip = result;
        }
        Ok(StackOutcome::Rebased(steps))
    }
}
