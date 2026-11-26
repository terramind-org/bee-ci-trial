//! The `path_history` derived type (SPEC §8): per-path history nodes, reused when content is
//! unchanged from parent 0.
//!
//! Every path that ever existed in a changeset's history (directories included, the root as
//! the empty path) has a **history node** at that changeset. A node records the path's state,
//! the changeset that produced that state (its *linknode*), and the nodes of the same path in
//! the changeset's parents. A directory node lists its children by name with their node ids
//! and kinds, so the nodes of one changeset form a manifest that shares every unchanged node
//! with its first parent: a changeset writes new nodes only for the paths in its delta and
//! their ancestor directories.
//!
//! Deleted paths stay in their directory as **tombstones** (kinds `deleted_file` and
//! `deleted_dir`) whose parents are the last live nodes, so a path deleted and later re-added
//! keeps one history: the new node's parent is the tombstone. Log over a path, a directory
//! log ("what changed under `src/auth`") and blame all follow node parents and read no trees
//! and no file contents.
//!
//! Rows, type `path_history`, version [`PATH_HISTORY_VERSION`]:
//!
//! - keyed by a changeset id: `{1: root node id}`;
//! - keyed by a node id (kind `d110`, BLAKE3 of the encoding):
//!   `{1: kind, 2: path (omitted at the root), 3: linknode changeset, 4: linknode generation,
//!   5: [parent node ids], 6: {1: mode, 2: id, 3: size} (files), 7: [{1: name, 2: node id,
//!   3: kind}] (directories, omitted when empty)}`. Kinds: 0 file, 1 directory,
//!   2 deleted file, 3 deleted directory.
//!
//! Parent rule for a new node at path `p` of changeset `C`: if some parent of `C` already has
//! a node at `p` in the identical state, that node is reused (parent 0 first; this is what
//! keeps a merge that took one side's version out of that path's log); otherwise a new node
//! lists the nodes at `p` in each parent, in parent order, without duplicates. A file added
//! (or re-added) at `p` whose blob is one this same delta deletes elsewhere also lists that
//! deleted file's node (an exact rename, matched by blob id in path order, each source once),
//! so path log and blame follow the move without reading contents.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use bee_core::{Mode, ObjectId, ObjectKind, RepoPath, Result};
use bee_object::{Changeset, DeltaEntry, DeltaOp, Value, digest};
use bee_store::DerivedRow;

use crate::codec::{self, Fields, corrupt};
use crate::derive::{Forward, ensure};
use crate::repo::{History, changeset_not_found};

/// The derived type name.
pub const PATH_HISTORY_TYPE: &str = "path_history";
/// The derived type version.
pub const PATH_HISTORY_VERSION: u32 = 1;
/// The derived object kind byte of history node ids (`d110_…`).
pub const PATH_NODE_KIND: u8 = 0x10;

/// The state a history node records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    /// A leaf (file, executable, symlink or gitlink).
    File,
    /// A directory with at least one live child (the root is always a directory).
    Dir,
    /// A deleted leaf.
    DeletedFile,
    /// A directory all of whose children are deleted.
    DeletedDir,
}

impl NodeKind {
    /// The encoded value.
    pub fn code(self) -> u64 {
        match self {
            NodeKind::File => 0,
            NodeKind::Dir => 1,
            NodeKind::DeletedFile => 2,
            NodeKind::DeletedDir => 3,
        }
    }

    /// Parses an encoded value.
    pub fn from_code(code: u64) -> Option<NodeKind> {
        Some(match code {
            0 => NodeKind::File,
            1 => NodeKind::Dir,
            2 => NodeKind::DeletedFile,
            3 => NodeKind::DeletedDir,
            _ => return None,
        })
    }

    /// True for states present in the tree.
    pub fn is_live(self) -> bool {
        matches!(self, NodeKind::File | NodeKind::Dir)
    }

    /// True for directory states.
    pub fn is_dir(self) -> bool {
        matches!(self, NodeKind::Dir | NodeKind::DeletedDir)
    }
}

/// A leaf's tree entry fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeafEntry {
    /// Mode.
    pub mode: Mode,
    /// Blob id (foreign id for gitlinks).
    pub id: ObjectId,
    /// Size.
    pub size: u64,
}

/// One child of a directory node.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChildRef {
    /// The entry name.
    pub name: Vec<u8>,
    /// The child's node.
    pub node: ObjectId,
    /// The child's kind (so liveness is known without reading the child).
    pub kind: NodeKind,
}

/// A history node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathNode {
    /// The state.
    pub kind: NodeKind,
    /// The path; `None` for the root.
    pub path: Option<RepoPath>,
    /// The changeset that produced this state.
    pub changeset: ObjectId,
    /// That changeset's generation (orders log walks without reading graph rows).
    pub generation: u64,
    /// The nodes of this path in the changeset's parents, in parent order, deduplicated.
    pub parents: Vec<ObjectId>,
    /// For files: the entry.
    pub entry: Option<LeafEntry>,
    /// For directories: the children, sorted by name.
    pub children: Vec<ChildRef>,
}

impl PathNode {
    /// The canonical encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut m: BTreeMap<u64, Value> = BTreeMap::new();
        m.insert(1, Value::Uint(self.kind.code()));
        if let Some(p) = &self.path {
            m.insert(2, Value::Bytes(p.as_bytes().to_vec()));
        }
        m.insert(3, codec::id(&self.changeset));
        m.insert(4, Value::Uint(self.generation));
        m.insert(5, codec::ids(&self.parents));
        if let Some(e) = &self.entry {
            m.insert(
                6,
                codec::map([
                    (1, Value::Uint(e.mode.as_u64())),
                    (2, codec::id(&e.id)),
                    (3, Value::Uint(e.size)),
                ]),
            );
        }
        if !self.children.is_empty() {
            m.insert(
                7,
                Value::Array(
                    self.children
                        .iter()
                        .map(|c| {
                            codec::map([
                                (1, Value::Bytes(c.name.clone())),
                                (2, codec::id(&c.node)),
                                (3, Value::Uint(c.kind.code())),
                            ])
                        })
                        .collect(),
                ),
            );
        }
        Value::Map(m).encode()
    }

    /// The node id: derived kind `0x10`, BLAKE3 of the encoding.
    pub fn id(&self) -> ObjectId {
        ObjectId::new(ObjectKind::Derived(PATH_NODE_KIND), digest(&self.encode()))
    }

    /// Decodes a stored node.
    pub fn decode(bytes: &[u8]) -> Result<PathNode> {
        let what = "path_history node";
        let f = Fields::decode(what, bytes)?;
        let kind = NodeKind::from_code(f.uint(1)?).ok_or_else(|| corrupt(what, "bad kind"))?;
        let path = if f.has(2) {
            Some(RepoPath::new(f.bytes(2)?).map_err(|e| corrupt(what, e))?)
        } else {
            None
        };
        let entry = if f.has(6) {
            let e = f.sub(6)?;
            Some(LeafEntry {
                mode: Mode::from_u64(e.uint(1)?).map_err(|x| corrupt(what, x))?,
                id: e.id(2)?,
                size: e.uint(3)?,
            })
        } else {
            None
        };
        let mut children = Vec::new();
        for c in f.opt_array(7)? {
            let c = Fields::from_value(what, c)?;
            children.push(ChildRef {
                name: c.bytes(1)?,
                node: c.id(2)?,
                kind: NodeKind::from_code(c.uint(3)?).ok_or_else(|| corrupt(what, "bad kind"))?,
            });
        }
        Ok(PathNode {
            kind,
            path,
            changeset: f.id(3)?,
            generation: f.uint(4)?,
            parents: f.ids(5)?,
            entry,
            children,
        })
    }

    /// The row storing this node.
    pub fn row(&self) -> DerivedRow {
        DerivedRow {
            key: self.id(),
            type_name: PATH_HISTORY_TYPE.into(),
            version: PATH_HISTORY_VERSION,
            value: self.encode(),
        }
    }

    /// The child named `name`.
    pub fn child(&self, name: &[u8]) -> Option<&ChildRef> {
        self.children
            .binary_search_by(|c| c.name.as_slice().cmp(name))
            .ok()
            .map(|i| &self.children[i])
    }

    fn same_state(&self, other: &PathNode) -> bool {
        self.kind == other.kind && self.entry == other.entry && self.children == other.children
    }
}

/// The row recording a changeset's root node.
pub fn changeset_row(changeset: ObjectId, root: ObjectId) -> DerivedRow {
    DerivedRow {
        key: changeset,
        type_name: PATH_HISTORY_TYPE.into(),
        version: PATH_HISTORY_VERSION,
        value: codec::map([(1, codec::id(&root))]).encode(),
    }
}

/// Decodes a changeset row into its root node id.
pub fn decode_changeset_row(bytes: &[u8]) -> Result<ObjectId> {
    Fields::decode("path_history changeset", bytes)?.id(1)
}

/// The nodes of one parent along the affected paths: path (`None` = root) → node.
pub type ParentNodes = BTreeMap<Option<RepoPath>, (ObjectId, PathNode)>;

/// The new nodes of a changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltNodes {
    /// The changeset's root node (new or reused).
    pub root: ObjectId,
    /// Nodes created by this changeset, by id.
    pub nodes: BTreeMap<ObjectId, PathNode>,
}

fn depth(p: &Option<RepoPath>) -> usize {
    p.as_ref().map_or(0, |p| p.depth())
}

/// The paths a delta affects: its paths (minus those below a put, which belonged to a
/// replaced directory), their ancestor directories and the root.
pub fn affected_paths(delta: &[DeltaEntry]) -> BTreeSet<Option<RepoPath>> {
    let puts: Vec<&RepoPath> = delta
        .iter()
        .filter(|e| matches!(e.op, DeltaOp::Put { .. }))
        .map(|e| &e.path)
        .collect();
    let mut out = BTreeSet::new();
    out.insert(None);
    for e in delta {
        if puts.iter().any(|p| p.is_ancestor_of(&e.path)) {
            continue;
        }
        for a in e.path.ancestors() {
            out.insert(Some(a));
        }
        out.insert(Some(e.path.clone()));
    }
    out
}

/// Builds a changeset's history nodes from its delta and its parents' nodes along the
/// affected paths. Pure: the same inputs give the same nodes and ids.
pub fn build_nodes(
    changeset: ObjectId,
    generation: u64,
    delta: &[DeltaEntry],
    parents: &[ParentNodes],
) -> BuiltNodes {
    let affected = affected_paths(delta);
    let puts: BTreeMap<&RepoPath, LeafEntry> = delta
        .iter()
        .filter_map(|e| match e.op {
            DeltaOp::Put { mode, id, size } => Some((&e.path, LeafEntry { mode, id, size })),
            DeltaOp::Delete => None,
        })
        .collect();
    let mut dirs: BTreeSet<Option<RepoPath>> = BTreeSet::new();
    dirs.insert(None);
    for p in affected.iter().flatten() {
        for a in p.ancestors() {
            dirs.insert(Some(a));
        }
    }
    let mut order: Vec<&Option<RepoPath>> = affected.iter().collect();
    order.sort_by(|a, b| depth(b).cmp(&depth(a)).then_with(|| a.cmp(b)));

    // Exact renames: files this delta deletes, by blob id, to link an added file to.
    let mut renamed_from: BTreeMap<ObjectId, Vec<ObjectId>> = BTreeMap::new();
    if let Some(p0) = parents.first() {
        for e in delta {
            if e.op == DeltaOp::Delete
                && let Some((nid, n)) = p0.get(&Some(e.path.clone()))
                && n.kind == NodeKind::File
                && let Some(entry) = n.entry
            {
                renamed_from.entry(entry.id).or_default().push(*nid);
            }
        }
    }
    let mut results: BTreeMap<Option<RepoPath>, (ObjectId, NodeKind)> = BTreeMap::new();
    let mut new_children: BTreeMap<Option<RepoPath>, Vec<ChildRef>> = BTreeMap::new();
    let mut nodes = BTreeMap::new();
    for x in order {
        let put = x.as_ref().and_then(|p| puts.get(p)).copied();
        let candidates: Vec<&(ObjectId, PathNode)> =
            parents.iter().filter_map(|m| m.get(x)).collect();
        let mut node = PathNode {
            kind: NodeKind::File,
            path: x.clone(),
            changeset,
            generation,
            parents: Vec::new(),
            entry: None,
            children: Vec::new(),
        };
        if let Some(entry) = put {
            node.entry = Some(entry);
        } else if dirs.contains(x) {
            let mut children: BTreeMap<Vec<u8>, ChildRef> =
                match parents.first().and_then(|m| m.get(x)) {
                    Some((_, n)) if n.kind.is_dir() => n
                        .children
                        .iter()
                        .map(|c| (c.name.clone(), c.clone()))
                        .collect(),
                    _ => BTreeMap::new(),
                };
            for c in new_children.remove(x).unwrap_or_default() {
                children.insert(c.name.clone(), c);
            }
            node.children = children.into_values().collect();
            let live = node.children.iter().any(|c| c.kind.is_live());
            node.kind = if live || x.is_none() {
                NodeKind::Dir
            } else {
                NodeKind::DeletedDir
            };
        } else {
            node.kind = NodeKind::DeletedFile;
        }
        let reused = candidates
            .iter()
            .find(|(_, n)| n.same_state(&node))
            .map(|(id, n)| (*id, n.kind));
        let (id, kind) = match reused {
            Some(r) => r,
            None => {
                let mut ps: Vec<ObjectId> = Vec::new();
                for (id, _) in &candidates {
                    if !ps.contains(id) {
                        ps.push(*id);
                    }
                }
                let was_file = parents
                    .first()
                    .and_then(|m| m.get(x))
                    .is_some_and(|(_, n)| n.kind == NodeKind::File);
                if let Some(entry) = put
                    && !was_file
                    && let Some(sources) = renamed_from.get_mut(&entry.id)
                    && !sources.is_empty()
                {
                    ps.push(sources.remove(0));
                }
                node.parents = ps;
                let id = node.id();
                let kind = node.kind;
                nodes.insert(id, node);
                (id, kind)
            }
        };
        if let Some(p) = x {
            new_children.entry(p.parent()).or_default().push(ChildRef {
                name: p.file_name().to_vec(),
                node: id,
                kind,
            });
        }
        results.insert(x.clone(), (id, kind));
    }
    let root = results
        .get(&None)
        .map(|(id, _)| *id)
        .expect("root is always affected");
    BuiltNodes { root, nodes }
}

/// [`Forward`] over `path_history` changeset rows: backfills missing rows (writing the new
/// nodes first, then the changeset row).
pub struct PathHistoryRows<'a> {
    history: &'a History,
}

impl<'a> PathHistoryRows<'a> {
    /// Path-history rows of a repo.
    pub fn new(history: &'a History) -> PathHistoryRows<'a> {
        PathHistoryRows { history }
    }
}

#[async_trait]
impl Forward for PathHistoryRows<'_> {
    fn what(&self) -> &str {
        PATH_HISTORY_TYPE
    }

    async fn stored(&self, key: &ObjectId) -> Result<Option<Vec<u8>>> {
        self.history
            .derived(key, PATH_HISTORY_TYPE, PATH_HISTORY_VERSION)
            .await
    }

    async fn deps(&self, key: &ObjectId) -> Result<Vec<ObjectId>> {
        Ok(self.history.changeset(key).await?.parents().to_vec())
    }

    async fn derive(&self, key: &ObjectId, deps: &[(ObjectId, Vec<u8>)]) -> Result<Vec<u8>> {
        let cs = self.history.changeset(key).await?;
        let roots: Vec<ObjectId> = deps
            .iter()
            .map(|(_, v)| decode_changeset_row(v))
            .collect::<Result<_>>()?;
        let generation = self.history.graph_node(key).await?.generation;
        let built = self
            .history
            .build_path_nodes(*key, generation, &cs, &roots)
            .await?;
        self.history
            .put_derived(built.nodes.values().map(PathNode::row).collect())
            .await?;
        Ok(changeset_row(*key, built.root).value)
    }

    async fn store(&self, key: &ObjectId, value: &[u8]) -> Result<()> {
        self.history
            .put_derived(vec![DerivedRow {
                key: *key,
                type_name: PATH_HISTORY_TYPE.into(),
                version: PATH_HISTORY_VERSION,
                value: value.to_vec(),
            }])
            .await
    }
}

impl History {
    /// Reads a history node by id.
    pub async fn path_node(&self, id: &ObjectId) -> Result<PathNode> {
        let bytes = self
            .derived(id, PATH_HISTORY_TYPE, PATH_HISTORY_VERSION)
            .await?
            .ok_or_else(|| corrupt("path_history node", format!("{id} is missing")))?;
        PathNode::decode(&bytes)
    }

    /// The root history node of a changeset, deriving missing rows forward.
    pub async fn path_root(&self, changeset: &ObjectId) -> Result<ObjectId> {
        let bytes = match self
            .derived(changeset, PATH_HISTORY_TYPE, PATH_HISTORY_VERSION)
            .await?
        {
            Some(b) => b,
            None => {
                if !self.has_changeset(changeset).await? {
                    return Err(changeset_not_found(changeset));
                }
                ensure(&PathHistoryRows::new(self), changeset).await?
            }
        };
        decode_changeset_row(&bytes)
    }

    /// The history node of `path` (`None` = root) at a changeset, tombstones included;
    /// `None` when the path never existed in that changeset's history. O(depth) row reads.
    pub async fn node_at(
        &self,
        changeset: &ObjectId,
        path: Option<&RepoPath>,
    ) -> Result<Option<(ObjectId, PathNode)>> {
        let mut id = self.path_root(changeset).await?;
        let mut node = self.path_node(&id).await?;
        if let Some(path) = path {
            for name in path.components() {
                let Some(c) = node.child(name) else {
                    return Ok(None);
                };
                id = c.node;
                node = self.path_node(&id).await?;
            }
        }
        Ok(Some((id, node)))
    }

    /// Loads one parent's nodes along `paths`, starting from its root node.
    async fn load_parent_nodes(
        &self,
        root: ObjectId,
        paths: &BTreeSet<Option<RepoPath>>,
    ) -> Result<ParentNodes> {
        let mut out = ParentNodes::new();
        let root_node = self.path_node(&root).await?;
        out.insert(None, (root, root_node));
        for p in paths.iter().flatten() {
            // Paths are sorted, so each parent directory is loaded before its children.
            let parent = p.parent();
            let Some((_, dir)) = out.get(&parent) else {
                continue;
            };
            let Some(c) = dir.child(p.file_name()).cloned() else {
                continue;
            };
            let node = self.path_node(&c.node).await?;
            out.insert(Some(p.clone()), (c.node, node));
        }
        Ok(out)
    }

    /// Builds a changeset's history nodes, loading its parents' nodes along the affected
    /// paths (O(delta × depth) row reads per parent).
    pub async fn build_path_nodes(
        &self,
        id: ObjectId,
        generation: u64,
        changeset: &Changeset,
        parent_roots: &[ObjectId],
    ) -> Result<BuiltNodes> {
        let affected = affected_paths(changeset.delta());
        let mut parents = Vec::with_capacity(parent_roots.len());
        for r in parent_roots {
            parents.push(self.load_parent_nodes(*r, &affected).await?);
        }
        Ok(build_nodes(id, generation, changeset.delta(), &parents))
    }

    /// The eager `path_history` rows of a new changeset: its new nodes, then its changeset
    /// row. Parents' rows are derived first if missing.
    pub async fn path_history_rows(
        &self,
        id: ObjectId,
        generation: u64,
        changeset: &Changeset,
    ) -> Result<Vec<DerivedRow>> {
        let mut roots = Vec::new();
        for p in changeset.parents() {
            roots.push(self.path_root(p).await?);
        }
        let built = self
            .build_path_nodes(id, generation, changeset, &roots)
            .await?;
        let mut rows: Vec<DerivedRow> = built.nodes.values().map(PathNode::row).collect();
        rows.push(changeset_row(id, built.root));
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_object::blob_id;

    fn cs(n: u8) -> ObjectId {
        ObjectId::new(ObjectKind::Changeset, [n; 32])
    }

    fn put(p: &str, data: &[u8]) -> DeltaEntry {
        DeltaEntry::put(
            RepoPath::new(p.as_bytes()).unwrap(),
            Mode::File,
            blob_id(data),
            data.len() as u64,
        )
    }

    fn del(p: &str) -> DeltaEntry {
        DeltaEntry::delete(RepoPath::new(p.as_bytes()).unwrap())
    }

    /// Flattens a built manifest into parent nodes for every path (test-only full load).
    fn all_nodes(built: &BuiltNodes, prev: &[ParentNodes]) -> ParentNodes {
        let mut by_id: BTreeMap<ObjectId, PathNode> = built.nodes.clone();
        for m in prev {
            for (id, n) in m.values() {
                by_id.insert(*id, n.clone());
            }
        }
        let mut out = ParentNodes::new();
        let mut stack = vec![built.root];
        while let Some(id) = stack.pop() {
            let n = by_id[&id].clone();
            for c in &n.children {
                stack.push(c.node);
            }
            out.insert(n.path.clone(), (id, n));
        }
        out
    }

    #[test]
    fn nodes_are_reused_tombstoned_and_relinked() {
        let a = build_nodes(cs(1), 1, &[put("src/a.rs", b"1"), put("x", b"x")], &[]);
        let ma = all_nodes(&a, &[]);
        let b = build_nodes(
            cs(2),
            2,
            &[put("src/a.rs", b"2")],
            std::slice::from_ref(&ma),
        );
        let mb = all_nodes(&b, std::slice::from_ref(&ma));
        // `x` is unchanged: same node.
        assert_eq!(
            ma[&Some(RepoPath::new(b"x".to_vec()).unwrap())].0,
            mb[&Some(RepoPath::new(b"x".to_vec()).unwrap())].0
        );
        let src = Some(RepoPath::new(b"src".to_vec()).unwrap());
        let file = Some(RepoPath::new(b"src/a.rs".to_vec()).unwrap());
        assert_eq!(mb[&file].1.parents, vec![ma[&file].0]);
        assert_eq!(mb[&src].1.parents, vec![ma[&src].0]);
        // Delete: the file and its directory become tombstones.
        let c = build_nodes(cs(3), 3, &[del("src/a.rs")], std::slice::from_ref(&mb));
        let mc = all_nodes(&c, std::slice::from_ref(&mb));
        assert_eq!(mc[&file].1.kind, NodeKind::DeletedFile);
        assert_eq!(mc[&src].1.kind, NodeKind::DeletedDir);
        assert_eq!(mc[&None].1.kind, NodeKind::Dir);
        // Re-add: the new node's parent is the tombstone.
        let d = build_nodes(
            cs(4),
            4,
            &[put("src/a.rs", b"3")],
            std::slice::from_ref(&mc),
        );
        let md = all_nodes(&d, std::slice::from_ref(&mc));
        assert_eq!(md[&file].1.parents, vec![mc[&file].0]);
        assert_eq!(md[&src].1.kind, NodeKind::Dir);
        // A merge taking theirs' version reuses theirs' node.
        let m = build_nodes(
            cs(5),
            5,
            &[put("src/a.rs", b"2")],
            &[md.clone(), mb.clone()],
        );
        let mm = all_nodes(&m, &[md.clone(), mb.clone()]);
        assert_eq!(mm[&file].0, mb[&file].0);
        // Empty delta: the root is parent 0's.
        let e = build_nodes(cs(6), 6, &[], std::slice::from_ref(&md));
        assert_eq!(e.root, md[&None].0);
        assert!(e.nodes.is_empty());
        // Codec round trip of every node kind.
        for n in a.nodes.values().chain(c.nodes.values()) {
            assert_eq!(&PathNode::decode(&n.encode()).unwrap(), n);
        }
        assert_eq!(
            decode_changeset_row(&changeset_row(cs(1), a.root).value).unwrap(),
            a.root
        );
        assert!(NodeKind::from_code(9).is_none());
    }

    #[test]
    fn file_directory_replacement() {
        let a = build_nodes(cs(1), 1, &[put("d/x", b"1"), put("d/y", b"2")], &[]);
        let ma = all_nodes(&a, &[]);
        // Directory replaced by a file: the old leaves are not recorded under it.
        let b = build_nodes(
            cs(2),
            2,
            &[put("d", b"f"), del("d/x"), del("d/y")],
            std::slice::from_ref(&ma),
        );
        let mb = all_nodes(&b, std::slice::from_ref(&ma));
        let d = Some(RepoPath::new(b"d".to_vec()).unwrap());
        assert_eq!(mb[&d].1.kind, NodeKind::File);
        assert_eq!(mb[&d].1.parents, vec![ma[&d].0]);
        assert_eq!(mb.len(), 2);
    }
}
