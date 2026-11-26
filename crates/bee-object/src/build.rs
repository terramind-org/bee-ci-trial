//! Building trees from a delta (SPEC §4.4, §7.2) and checking invariant D.
//!
//! [`apply_delta`] reads only the trees on the paths the delta touches, so a one-file change
//! reads at most `depth` trees no matter how large the repository is.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use bee_core::{
    BeeError, ChangeId, Limits, Mode, ObjectId, ObjectKind, Reason, RepoPath, Result, Subcode,
};

use crate::changeset::{Changeset, DeltaEntry, DeltaOp, validate_delta};
use crate::tree::{Tree, TreeEntry};

/// Somewhere trees can be read from by id.
pub trait TreeSource {
    /// Returns the tree, or `MISSING_OBJECT` if it is not available.
    fn tree(&self, id: &ObjectId) -> Result<Tree>;
}

impl<F> TreeSource for F
where
    F: Fn(&ObjectId) -> Result<Tree>,
{
    fn tree(&self, id: &ObjectId) -> Result<Tree> {
        self(id)
    }
}

/// An in-memory tree store that counts reads; for tests, tools and reference checks.
#[derive(Debug, Default)]
pub struct MemoryTrees {
    trees: BTreeMap<ObjectId, Tree>,
    reads: AtomicU64,
}

impl MemoryTrees {
    /// An empty store.
    pub fn new() -> MemoryTrees {
        MemoryTrees::default()
    }

    /// Stores a tree under its id and returns the id.
    pub fn insert(&mut self, tree: Tree) -> ObjectId {
        let id = tree.id();
        self.trees.insert(id, tree);
        id
    }

    /// Stores every tree of a build.
    pub fn extend(&mut self, trees: BTreeMap<ObjectId, Tree>) {
        self.trees.extend(trees);
    }

    /// Number of stored trees.
    pub fn len(&self) -> usize {
        self.trees.len()
    }

    /// True when nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.trees.is_empty()
    }

    /// How many reads [`TreeSource::tree`] has served.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Resets the read counter.
    pub fn reset_reads(&self) {
        self.reads.store(0, Ordering::Relaxed);
    }
}

impl TreeSource for MemoryTrees {
    fn tree(&self, id: &ObjectId) -> Result<Tree> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.trees
            .get(id)
            .cloned()
            .ok_or_else(|| BeeError::new(Reason::MissingObject, format!("tree {id} not found")))
    }
}

/// The result of [`apply_delta`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltTree {
    /// The new root tree id.
    pub root: ObjectId,
    /// Every tree the build created, root included, keyed by id. Unchanged subtrees are not
    /// listed; they already exist.
    pub trees: BTreeMap<ObjectId, Tree>,
}

fn delta_error(path: &[u8], why: &str) -> BeeError {
    BeeError::invalid(
        Subcode::InvalidDelta,
        format!(
            "delta does not apply at {:?}: {why}",
            String::from_utf8_lossy(path)
        ),
    )
    .with_payload("path", String::from_utf8_lossy(path).into_owned())
}

type Op<'a> = (Vec<&'a [u8]>, &'a DeltaOp);

/// Applies `delta` to the tree `base_root` (or the empty tree) and returns the new trees.
///
/// The delta must be valid (see [`validate_delta`]) and must be an exact file-level
/// difference: a delete names an existing leaf; a put changes its path; replacing a
/// directory with a file deletes every leaf under it; replacing a file with a directory
/// deletes the file. Directories left empty disappear. Anything else is `INVALID_DELTA`.
pub fn apply_delta(
    source: &dyn TreeSource,
    base_root: Option<&ObjectId>,
    delta: &[DeltaEntry],
) -> Result<BuiltTree> {
    validate_delta(delta, true, &Limits::DEFAULT)?;
    let mut trees = BTreeMap::new();
    if delta.is_empty()
        && let Some(root) = base_root
    {
        root.expect_kind(ObjectKind::Tree)?;
        return Ok(BuiltTree { root: *root, trees });
    }
    let ops: Vec<Op<'_>> = delta
        .iter()
        .map(|e| (e.path.components().collect(), &e.op))
        .collect();
    let root = match build_dir(source, base_root, &ops, 0, &mut Vec::new(), &mut trees)? {
        Some(id) => id,
        None => {
            let empty = Tree::empty();
            let id = empty.id();
            trees.insert(id, empty);
            id
        }
    };
    Ok(BuiltTree { root, trees })
}

/// Builds one directory. Returns `None` when the directory ends up empty.
fn build_dir(
    source: &dyn TreeSource,
    base: Option<&ObjectId>,
    ops: &[Op<'_>],
    depth: usize,
    prefix: &mut Vec<u8>,
    out: &mut BTreeMap<ObjectId, Tree>,
) -> Result<Option<ObjectId>> {
    let mut entries: BTreeMap<Vec<u8>, TreeEntry> = match base {
        Some(id) => {
            id.expect_kind(ObjectKind::Tree)?;
            source
                .tree(id)?
                .into_entries()
                .into_iter()
                .map(|e| (e.name.clone(), e))
                .collect()
        }
        None => BTreeMap::new(),
    };

    // Group by the component at this depth: the op on that name itself, and ops below it.
    let mut groups: BTreeMap<&[u8], (Option<&DeltaOp>, Vec<Op<'_>>)> = BTreeMap::new();
    for (components, op) in ops {
        let g = groups.entry(components[depth]).or_default();
        if components.len() == depth + 1 {
            g.0 = Some(*op);
        } else {
            g.1.push((components.clone(), *op));
        }
    }

    for (name, (direct, below)) in groups {
        let saved = prefix.len();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(name);
        let existing = entries.get(name).cloned();
        let existing_tree = existing
            .as_ref()
            .filter(|e| e.mode == Mode::Tree)
            .map(|e| e.id);
        let existing_leaf = existing.as_ref().filter(|e| e.is_leaf());

        let sub = if below.is_empty() {
            None
        } else {
            if existing_leaf.is_some() && direct != Some(&DeltaOp::Delete) {
                return Err(delta_error(
                    prefix,
                    "paths below a file that is not deleted",
                ));
            }
            build_dir(
                source,
                existing_tree.as_ref(),
                &below,
                depth + 1,
                prefix,
                out,
            )?
        };

        let new_entry = match direct {
            None => sub.map(|id| TreeEntry::tree(name, id)),
            Some(DeltaOp::Delete) => {
                if existing_leaf.is_none() {
                    let why = if existing_tree.is_some() {
                        "delete of a directory"
                    } else {
                        "delete of a path that does not exist"
                    };
                    return Err(delta_error(prefix, why));
                }
                sub.map(|id| TreeEntry::tree(name, id))
            }
            Some(&DeltaOp::Put { mode, id, size }) => {
                if existing_tree.is_some() && (below.is_empty() || sub.is_some()) {
                    return Err(delta_error(
                        prefix,
                        "put over a directory that still has files",
                    ));
                }
                if let Some(e) = existing_leaf
                    && e.mode == mode
                    && e.id == id
                    && e.size == size
                {
                    return Err(delta_error(prefix, "put does not change the path"));
                }
                Some(TreeEntry {
                    name: name.to_vec(),
                    mode,
                    id,
                    size,
                })
            }
        };
        match new_entry {
            Some(e) => {
                entries.insert(name.to_vec(), e);
            }
            None => {
                entries.remove(name);
            }
        }
        prefix.truncate(saved);
    }

    if entries.is_empty() {
        return Ok(None);
    }
    let tree = Tree::new(entries.into_values().collect())?;
    let id = tree.id();
    out.insert(id, tree);
    Ok(Some(id))
}

/// Builds a changeset from its parents, the first parent's root tree, a meta id, a ChangeId
/// and a delta: applies the delta (clients never send trees, SPEC §7.2) and returns the
/// changeset with every new tree.
pub fn build_changeset(
    source: &dyn TreeSource,
    parents: Vec<ObjectId>,
    first_parent_root: Option<&ObjectId>,
    meta: ObjectId,
    change_id: ChangeId,
    delta: Vec<DeltaEntry>,
) -> Result<(Changeset, BTreeMap<ObjectId, Tree>)> {
    if parents.is_empty() != first_parent_root.is_none() {
        return Err(BeeError::invalid(
            Subcode::InvalidParents,
            "a first-parent root is given exactly when there are parents",
        ));
    }
    let built = apply_delta(source, first_parent_root, &delta)?;
    let cs = Changeset::new(parents, built.root, meta, change_id, delta)?;
    Ok((cs, built.trees))
}

/// Lists every leaf under the tree `id` as `(path, entry)`, in bytewise path order.
pub fn list_leaves(
    source: &dyn TreeSource,
    id: &ObjectId,
    prefix: Option<&RepoPath>,
) -> Result<Vec<(RepoPath, TreeEntry)>> {
    let mut out = Vec::new();
    collect_leaves(source, id, prefix, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn child_path(prefix: Option<&RepoPath>, name: &[u8]) -> Result<RepoPath> {
    match prefix {
        Some(p) => p.join(name),
        None => RepoPath::new(name),
    }
}

fn collect_leaves(
    source: &dyn TreeSource,
    id: &ObjectId,
    prefix: Option<&RepoPath>,
    out: &mut Vec<(RepoPath, TreeEntry)>,
) -> Result<()> {
    let tree = source.tree(id)?;
    if prefix.is_some() {
        tree.check_non_root()?;
    }
    for e in tree.into_entries() {
        let path = child_path(prefix, &e.name)?;
        if e.mode == Mode::Tree {
            collect_leaves(source, &e.id, Some(&path), out)?;
        } else {
            out.push((path, e));
        }
    }
    Ok(())
}

/// The exact file-level difference from `old` to `new` (either may be the empty tree, given
/// as `None`), sorted bytewise by path. Subtrees with equal ids are skipped unread.
pub fn diff_trees(
    source: &dyn TreeSource,
    old: Option<&ObjectId>,
    new: Option<&ObjectId>,
) -> Result<Vec<DeltaEntry>> {
    let mut out = Vec::new();
    if old != new {
        let old_tree = old.map(|id| source.tree(id)).transpose()?;
        let new_tree = new.map(|id| source.tree(id)).transpose()?;
        diff_dir(source, None, old_tree.as_ref(), new_tree.as_ref(), &mut out)?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn put_of(path: RepoPath, e: &TreeEntry) -> DeltaEntry {
    DeltaEntry::put(path, e.mode, e.id, e.size)
}

fn removed(
    source: &dyn TreeSource,
    path: RepoPath,
    e: &TreeEntry,
    out: &mut Vec<DeltaEntry>,
) -> Result<()> {
    if e.mode == Mode::Tree {
        for (p, _) in list_leaves(source, &e.id, Some(&path))? {
            out.push(DeltaEntry::delete(p));
        }
    } else {
        out.push(DeltaEntry::delete(path));
    }
    Ok(())
}

fn added(
    source: &dyn TreeSource,
    path: RepoPath,
    e: &TreeEntry,
    out: &mut Vec<DeltaEntry>,
) -> Result<()> {
    if e.mode == Mode::Tree {
        for (p, leaf) in list_leaves(source, &e.id, Some(&path))? {
            out.push(put_of(p, &leaf));
        }
    } else {
        out.push(put_of(path, e));
    }
    Ok(())
}

fn diff_dir(
    source: &dyn TreeSource,
    prefix: Option<&RepoPath>,
    old: Option<&Tree>,
    new: Option<&Tree>,
    out: &mut Vec<DeltaEntry>,
) -> Result<()> {
    let empty = Tree::empty();
    let old = old.unwrap_or(&empty).entries();
    let new = new.unwrap_or(&empty).entries();
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        let order = match (old.get(i), new.get(j)) {
            (Some(o), Some(n)) => o.name.cmp(&n.name),
            (Some(_), None) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Greater,
        };
        match order {
            std::cmp::Ordering::Less => {
                removed(source, child_path(prefix, &old[i].name)?, &old[i], out)?;
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                added(source, child_path(prefix, &new[j].name)?, &new[j], out)?;
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let (o, n) = (&old[i], &new[j]);
                if o != n {
                    let path = child_path(prefix, &o.name)?;
                    match (o.mode == Mode::Tree, n.mode == Mode::Tree) {
                        (true, true) => {
                            let ot = source.tree(&o.id)?;
                            let nt = source.tree(&n.id)?;
                            diff_dir(source, Some(&path), Some(&ot), Some(&nt), out)?;
                        }
                        (false, false) => out.push(put_of(path, n)),
                        _ => {
                            removed(source, path.clone(), o, out)?;
                            added(source, path, n, out)?;
                        }
                    }
                }
                i += 1;
                j += 1;
            }
        }
    }
    Ok(())
}

/// Checks invariant D (SPEC §4.4): the changeset's delta equals the exact file-level
/// difference from `first_parent_root` (the root tree of `parents[0]`, or `None` for a
/// parentless changeset) to its root. A mismatch is `INVALID_ARGUMENT`/`DELTA_MISMATCH`
/// with the first differing path in the payload.
pub fn check_invariant_d(
    source: &dyn TreeSource,
    first_parent_root: Option<&ObjectId>,
    changeset: &Changeset,
) -> Result<()> {
    if changeset.parents().is_empty() != first_parent_root.is_none() {
        return Err(BeeError::invalid(
            Subcode::InvalidParents,
            "a first-parent root is given exactly when the changeset has parents",
        ));
    }
    let expected = diff_trees(source, first_parent_root, Some(changeset.root()))?;
    let actual = changeset.delta();
    if expected.as_slice() == actual {
        return Ok(());
    }
    let first_diff = expected
        .iter()
        .zip(actual)
        .find(|(e, a)| e != a)
        .map(|(e, a)| std::cmp::min(&e.path, &a.path).clone())
        .or_else(|| {
            let n = expected.len().min(actual.len());
            expected.get(n).or(actual.get(n)).map(|e| e.path.clone())
        });
    let mut err = BeeError::invalid(
        Subcode::DeltaMismatch,
        format!(
            "delta has {} entries but the trees differ in {}",
            actual.len(),
            expected.len()
        ),
    );
    if let Some(p) = first_diff {
        err = err.with_payload("path", p.to_string());
    }
    Err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_id;
    use crate::meta::Meta;
    use proptest::prelude::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn put(s: &str, content: &[u8]) -> DeltaEntry {
        DeltaEntry::put(p(s), Mode::File, blob_id(content), content.len() as u64)
    }

    fn del(s: &str) -> DeltaEntry {
        DeltaEntry::delete(p(s))
    }

    /// Applies a delta and stores the results, returning the new root.
    fn commit(store: &mut MemoryTrees, base: Option<&ObjectId>, delta: &[DeltaEntry]) -> ObjectId {
        let built = apply_delta(store, base, delta).unwrap();
        store.extend(built.trees);
        built.root
    }

    fn listing(store: &MemoryTrees, root: &ObjectId) -> Vec<String> {
        list_leaves(store, root, None)
            .unwrap()
            .into_iter()
            .map(|(p, _)| p.to_string())
            .collect()
    }

    #[test]
    fn apply_to_empty() {
        let mut s = MemoryTrees::new();
        let root = commit(
            &mut s,
            None,
            &[put("a/b/c", b"1"), put("a/d", b"2"), put("e", b"3")],
        );
        assert_eq!(listing(&s, &root), ["a/b/c", "a/d", "e"]);
        // Trees created: root, a, a/b.
        let built = apply_delta(
            &s,
            None,
            &[put("a/b/c", b"1"), put("a/d", b"2"), put("e", b"3")],
        )
        .unwrap();
        assert_eq!(built.trees.len(), 3);
        assert_eq!(built.root, root);
    }

    #[test]
    fn apply_empty_delta() {
        let s = MemoryTrees::new();
        let built = apply_delta(&s, None, &[]).unwrap();
        assert_eq!(built.root, Tree::empty().id());
        let some = Tree::empty().id();
        let same = apply_delta(&s, Some(&some), &[]).unwrap();
        assert_eq!(same.root, some);
        assert!(same.trees.is_empty());
        let bad = blob_id(b"");
        assert!(
            apply_delta(&s, Some(&bad), &[])
                .unwrap_err()
                .is_invalid(Subcode::WrongKind)
        );
    }

    #[test]
    fn modify_and_delete() {
        let mut s = MemoryTrees::new();
        let r1 = commit(
            &mut s,
            None,
            &[put("a/b", b"1"), put("a/c", b"2"), put("d", b"3")],
        );
        let r2 = commit(&mut s, Some(&r1), &[put("a/b", b"changed"), del("d")]);
        assert_eq!(listing(&s, &r2), ["a/b", "a/c"]);
        let r3 = commit(&mut s, Some(&r2), &[del("a/b"), del("a/c")]);
        assert_eq!(
            r3,
            Tree::empty().id(),
            "empty directories vanish up to the root"
        );
    }

    #[test]
    fn file_to_directory_and_back() {
        let mut s = MemoryTrees::new();
        let r1 = commit(&mut s, None, &[put("a", b"file")]);
        let r2 = commit(
            &mut s,
            Some(&r1),
            &[del("a"), put("a/x", b"1"), put("a/y", b"2")],
        );
        assert_eq!(listing(&s, &r2), ["a/x", "a/y"]);
        let r3 = commit(
            &mut s,
            Some(&r2),
            &[put("a", b"file"), del("a/x"), del("a/y")],
        );
        assert_eq!(r3, r1, "same content, same id");
    }

    #[test]
    fn apply_rejects_inexact_deltas() {
        let mut s = MemoryTrees::new();
        let r1 = commit(&mut s, None, &[put("a/b", b"1"), put("f", b"2")]);
        let cases: Vec<(Vec<DeltaEntry>, &str)> = vec![
            (vec![del("zz")], "delete of missing path"),
            (vec![del("a")], "delete of directory"),
            (vec![put("a", b"x")], "put over non-empty directory"),
            (
                vec![put("a", b"x"), del("a/zz")],
                "put over dir, deleting missing child",
            ),
            (vec![put("f", b"2")], "no-op put"),
            (vec![put("f/x", b"1")], "path below a file"),
            (
                vec![put("f", b"9"), del("f/x")]
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>(),
                "unsorted",
            ),
        ];
        for (delta, why) in cases {
            let err = apply_delta(&s, Some(&r1), &delta).unwrap_err();
            assert!(err.is_invalid(Subcode::InvalidDelta), "{why}: {err}");
        }
        let err = apply_delta(&s, Some(&r1), &[del("zz")]).unwrap_err();
        assert_eq!(
            err.payload.get("path"),
            Some(&bee_core::PayloadValue::Str("zz".into()))
        );
    }

    #[test]
    fn missing_tree_is_reported() {
        let s = MemoryTrees::new();
        let ghost = Tree::new(vec![TreeEntry::blob("x", Mode::File, blob_id(b""), 0)])
            .unwrap()
            .id();
        let err = apply_delta(&s, Some(&ghost), &[put("y", b"")]).unwrap_err();
        assert_eq!(err.reason, Reason::MissingObject);
    }

    #[test]
    fn reads_are_bounded_by_depth() {
        // A wide, deep repo: 20 dirs x 20 dirs x 5 files. A one-file change reads 3 trees.
        let mut s = MemoryTrees::new();
        let mut delta = Vec::new();
        for i in 0..20 {
            for j in 0..20 {
                for k in 0..5 {
                    delta.push(put(
                        &format!("d{i:02}/e{j:02}/f{k}"),
                        format!("{i}{j}{k}").as_bytes(),
                    ));
                }
            }
        }
        let root = commit(&mut s, None, &delta);
        s.reset_reads();
        let built = apply_delta(&s, Some(&root), &[put("d07/e13/f2", b"new")]).unwrap();
        assert_eq!(s.reads(), 3);
        assert_eq!(built.trees.len(), 3);
        s.reset_reads();
        let d = diff_trees(&s, Some(&root), Some(&built.root));
        s.extend(built.trees.clone());
        s.reset_reads();
        let d2 = diff_trees(&s, Some(&root), Some(&built.root)).unwrap();
        assert!(d.is_err(), "new trees were not stored yet");
        assert_eq!(d2, [put("d07/e13/f2", b"new")]);
        assert_eq!(
            s.reads(),
            6,
            "diff reads only the changed spine on both sides"
        );
    }

    #[test]
    fn diff_examples() {
        let mut s = MemoryTrees::new();
        let r1 = commit(
            &mut s,
            None,
            &[put("a-z", b"3"), put("a/b", b"1"), put("a/c", b"2")],
        );
        let r2 = commit(&mut s, Some(&r1), &[put("a", b"f"), del("a/b"), del("a/c")]);
        let d = diff_trees(&s, Some(&r1), Some(&r2)).unwrap();
        assert_eq!(d, [put("a", b"f"), del("a/b"), del("a/c")]);
        let from_empty = diff_trees(&s, None, Some(&r1)).unwrap();
        assert_eq!(
            from_empty,
            [put("a-z", b"3"), put("a/b", b"1"), put("a/c", b"2")],
            "bytewise"
        );
        let to_empty = diff_trees(&s, Some(&r1), None).unwrap();
        assert_eq!(to_empty.len(), 3);
        assert!(to_empty.iter().all(|e| e.op == DeltaOp::Delete));
        assert!(diff_trees(&s, Some(&r1), Some(&r1)).unwrap().is_empty());
        let back = diff_trees(&s, Some(&r2), Some(&r1)).unwrap();
        assert_eq!(back, [del("a"), put("a/b", b"1"), put("a/c", b"2")]);
    }

    #[test]
    fn diff_detects_mode_change() {
        let mut s = MemoryTrees::new();
        let r1 = commit(&mut s, None, &[put("x", b"1")]);
        let exec = DeltaEntry::put(p("x"), Mode::Executable, blob_id(b"1"), 1);
        let r2 = commit(&mut s, Some(&r1), std::slice::from_ref(&exec));
        assert_eq!(diff_trees(&s, Some(&r1), Some(&r2)).unwrap(), [exec]);
    }

    #[test]
    fn invariant_d_holds_for_built_changesets() {
        let mut s = MemoryTrees::new();
        let meta = Meta::empty_root().id();
        let (c1, t1) = build_changeset(
            &s,
            vec![],
            None,
            meta,
            ChangeId::new([1; 16]),
            vec![put("a/b", b"1")],
        )
        .unwrap();
        s.extend(t1);
        check_invariant_d(&s, None, &c1).unwrap();
        let (c2, t2) = build_changeset(
            &s,
            vec![c1.id()],
            Some(c1.root()),
            meta,
            ChangeId::new([2; 16]),
            vec![del("a/b"), put("c", b"2")],
        )
        .unwrap();
        s.extend(t2);
        check_invariant_d(&s, Some(c1.root()), &c2).unwrap();
        assert!(build_changeset(&s, vec![c1.id()], None, meta, ChangeId::ZERO, vec![]).is_err());
        assert!(
            check_invariant_d(&s, Some(c1.root()), &c1)
                .unwrap_err()
                .is_invalid(Subcode::InvalidParents)
        );
    }

    #[test]
    fn invariant_d_detects_mismatch() {
        let mut s = MemoryTrees::new();
        let meta = Meta::empty_root().id();
        let r1 = commit(&mut s, None, &[put("a", b"1"), put("b", b"2")]);
        let r2 = commit(&mut s, Some(&r1), &[put("a", b"x"), put("b", b"y")]);
        let parent = ObjectId::new(ObjectKind::Changeset, [5; 32]);
        // Claims only one of the two changes.
        let lying =
            Changeset::new(vec![parent], r2, meta, ChangeId::ZERO, vec![put("a", b"x")]).unwrap();
        let err = check_invariant_d(&s, Some(&r1), &lying).unwrap_err();
        assert!(err.is_invalid(Subcode::DeltaMismatch));
        assert_eq!(
            err.payload.get("path"),
            Some(&bee_core::PayloadValue::Str("b".into()))
        );
        // Wrong content for a path.
        let wrong = Changeset::new(
            vec![parent],
            r2,
            meta,
            ChangeId::ZERO,
            vec![put("a", b"x"), put("b", b"z")],
        )
        .unwrap();
        let err = check_invariant_d(&s, Some(&r1), &wrong).unwrap_err();
        assert_eq!(
            err.payload.get("path"),
            Some(&bee_core::PayloadValue::Str("b".into()))
        );
        // An extra no-op entry.
        let extra =
            Changeset::new(vec![parent], r1, meta, ChangeId::ZERO, vec![put("a", b"1")]).unwrap();
        assert!(
            check_invariant_d(&s, Some(&r1), &extra)
                .unwrap_err()
                .is_invalid(Subcode::DeltaMismatch)
        );
    }

    #[test]
    fn closure_source_and_store_helpers() {
        let mut s = MemoryTrees::new();
        assert!(s.is_empty());
        let id = s.insert(Tree::empty());
        assert_eq!(s.len(), 1);
        let src = |i: &ObjectId| s.tree(i);
        assert_eq!(src.tree(&id).unwrap(), Tree::empty());
    }

    #[test]
    fn corrupt_empty_subtree_is_rejected_by_listing() {
        let mut s = MemoryTrees::new();
        let empty = s.insert(Tree::empty());
        let root = s.insert(Tree::new(vec![TreeEntry::tree("d", empty)]).unwrap());
        assert!(
            list_leaves(&s, &root, None)
                .unwrap_err()
                .is_invalid(Subcode::InvalidTree)
        );
    }

    /// A random set of files: path -> content.
    fn arb_files() -> impl Strategy<Value = BTreeMap<String, Vec<u8>>> {
        proptest::collection::btree_map(
            "[ab]{1,2}(/[ab]{1,2}){0,2}",
            proptest::collection::vec(0u8..3, 0..3),
            0..12,
        )
        .prop_map(|m| {
            // Drop paths that are a directory prefix of another path (file/dir clash).
            let keys: Vec<String> = m.keys().cloned().collect();
            m.into_iter()
                .filter(|(k, _)| !keys.iter().any(|o| o.starts_with(&format!("{k}/"))))
                .collect()
        })
    }

    fn tree_of(store: &mut MemoryTrees, files: &BTreeMap<String, Vec<u8>>) -> ObjectId {
        let mut delta: Vec<DeltaEntry> = files.iter().map(|(k, v)| put(k, v)).collect();
        delta.sort_by(|a, b| a.path.cmp(&b.path));
        commit(store, None, &delta)
    }

    proptest! {
        /// diff(old, new) applied to old yields new, for arbitrary pairs of trees, and the
        /// changeset built from that delta satisfies invariant D.
        #[test]
        fn diff_then_apply_round_trips(a in arb_files(), b in arb_files()) {
            let mut s = MemoryTrees::new();
            let ra = tree_of(&mut s, &a);
            let rb = tree_of(&mut s, &b);
            let delta = diff_trees(&s, Some(&ra), Some(&rb)).unwrap();
            let built = apply_delta(&s, Some(&ra), &delta).unwrap();
            prop_assert_eq!(built.root, rb);
            s.extend(built.trees);
            let parent = ObjectId::new(ObjectKind::Changeset, [1; 32]);
            let cs = Changeset::new(vec![parent], rb, Meta::empty_root().id(), ChangeId::ZERO, delta).unwrap();
            prop_assert!(check_invariant_d(&s, Some(&ra), &cs).is_ok());
        }

        /// Building is deterministic and independent of history: the same files give the
        /// same root id whether built at once or via an intermediate tree.
        #[test]
        fn root_depends_only_on_content(a in arb_files(), b in arb_files()) {
            let mut s = MemoryTrees::new();
            let ra = tree_of(&mut s, &a);
            let rb_direct = tree_of(&mut s, &b);
            let delta = diff_trees(&s, Some(&ra), Some(&rb_direct)).unwrap();
            let via = commit(&mut s, Some(&ra), &delta);
            prop_assert_eq!(via, rb_direct);
        }
    }
}
