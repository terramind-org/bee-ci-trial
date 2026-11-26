//! Reading objects through both stores, prefetching the trees a delta touches, and computing
//! a changeset's `object_paths` rows.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use bee_core::{BeeError, Mode, ObjectId, Reason, RepoPath, Result};
use bee_object::{Changeset, DeltaOp, MemoryTrees, Tree, TreeSource, hash_id};

use crate::blob::BlobStore;
use crate::meta::MetaStore;
use crate::types::{ObjectPathRow, ObjectState, OrgId, ReadFrom};

/// Reads and verifies an object's bytes: inline from its row or from the blob store.
///
/// A replica miss is retried on the primary. No row, a quarantined row or absent bytes is
/// `MISSING_OBJECT`; a purged row is `REDACTED`; bytes that do not hash to the id are
/// `INTEGRITY`.
pub async fn read_object_bytes(
    meta: &dyn MetaStore,
    blobs: &dyn BlobStore,
    org: OrgId,
    id: &ObjectId,
    read: ReadFrom,
) -> Result<Vec<u8>> {
    let mut row = meta.get_object(org, id, read.clone()).await?;
    if row.is_none() && read != ReadFrom::Primary {
        row = meta.get_object(org, id, ReadFrom::Primary).await?;
    }
    let missing = || {
        BeeError::new(
            Reason::MissingObject,
            format!("object {id} is not available"),
        )
        .with_payload("object", id.to_text())
    };
    let row = row.ok_or_else(missing)?;
    let bytes = match row.state {
        ObjectState::Purged => {
            return Err(BeeError::new(Reason::Redacted, format!("{id} is redacted"))
                .with_payload("object", id.to_text()));
        }
        ObjectState::Quarantined => return Err(missing()),
        ObjectState::Live => match row.inline {
            Some(b) => b,
            None => blobs.get(org, id).await?.ok_or_else(missing)?,
        },
    };
    if hash_id(id.kind(), &bytes) != *id {
        return Err(BeeError::new(
            Reason::Integrity,
            format!("stored bytes do not hash to {id}"),
        ));
    }
    Ok(bytes)
}

/// Reads and decodes a tree.
pub async fn read_tree(
    meta: &dyn MetaStore,
    blobs: &dyn BlobStore,
    org: OrgId,
    id: &ObjectId,
    read: ReadFrom,
) -> Result<Tree> {
    id.expect_kind(bee_core::ObjectKind::Tree)?;
    let bytes = read_object_bytes(meta, blobs, org, id, read).await?;
    Tree::decode(&bytes)
}

/// Reads the root tree and every tree on the way to each path's parent directory: exactly
/// what [`bee_object::apply_delta`] needs for a delta over `paths`, O(depth) reads per path
/// (shared prefixes are read once). Paths through a file or a missing directory stop there.
pub async fn fetch_path_trees(
    meta: &dyn MetaStore,
    blobs: &dyn BlobStore,
    org: OrgId,
    root: Option<&ObjectId>,
    paths: &[RepoPath],
) -> Result<MemoryTrees> {
    let mut out = MemoryTrees::new();
    let Some(root) = root else {
        return Ok(out);
    };
    let mut loaded: BTreeMap<ObjectId, Tree> = BTreeMap::new();
    let root_tree = read_tree(meta, blobs, org, root, ReadFrom::Primary).await?;
    loaded.insert(*root, root_tree);
    for path in paths {
        let mut current = *root;
        let components: Vec<&[u8]> = path.components().collect();
        for name in &components[..components.len().saturating_sub(1)] {
            let next = loaded
                .get(&current)
                .and_then(|t| t.get(name))
                .filter(|e| e.mode == Mode::Tree)
                .map(|e| e.id);
            let Some(next) = next else { break };
            if let Entry::Vacant(v) = loaded.entry(next) {
                v.insert(read_tree(meta, blobs, org, &next, ReadFrom::Primary).await?);
            }
            current = next;
        }
    }
    for t in loaded.into_values() {
        out.insert(t);
    }
    Ok(out)
}

/// The `object_paths` rows a changeset contributes (SPEC §9.3): its root tree at the root;
/// every delta put's object at its path; and, for every ancestor directory of every delta
/// path that still exists in the changeset's root, the tree there.
///
/// Every `(object, path)` reachable in a changeset's tree is contributed by it or by one of
/// its first-parent ancestors, so the union over a repo's changesets is complete; fsck
/// checks that. `source` must hold the trees on the delta's paths (the parent's trees from
/// [`fetch_path_trees`] plus the new trees from the build).
pub fn object_paths_for(source: &dyn TreeSource, cs: &Changeset) -> Result<Vec<ObjectPathRow>> {
    let mut rows: BTreeSet<ObjectPathRow> = BTreeSet::new();
    rows.insert(ObjectPathRow {
        object: *cs.root(),
        path: None,
    });
    let mut dirs: BTreeMap<RepoPath, Option<ObjectId>> = BTreeMap::new();
    let mut trees: BTreeMap<ObjectId, Tree> = BTreeMap::new();
    for e in cs.delta() {
        if let DeltaOp::Put { id, .. } = e.op {
            rows.insert(ObjectPathRow {
                object: id,
                path: Some(e.path.clone()),
            });
        }
        for dir in e.path.ancestors() {
            let id = tree_at(source, cs.root(), &dir, &mut dirs, &mut trees)?;
            match id {
                Some(id) => {
                    rows.insert(ObjectPathRow {
                        object: id,
                        path: Some(dir),
                    });
                }
                None => break,
            }
        }
    }
    Ok(rows.into_iter().collect())
}

fn tree_at(
    source: &dyn TreeSource,
    root: &ObjectId,
    dir: &RepoPath,
    dirs: &mut BTreeMap<RepoPath, Option<ObjectId>>,
    trees: &mut BTreeMap<ObjectId, Tree>,
) -> Result<Option<ObjectId>> {
    if let Some(known) = dirs.get(dir) {
        return Ok(*known);
    }
    let parent = match dir.parent() {
        Some(p) => tree_at(source, root, &p, dirs, trees)?,
        None => Some(*root),
    };
    let found = match parent {
        None => None,
        Some(pid) => {
            if let Entry::Vacant(v) = trees.entry(pid) {
                v.insert(source.tree(&pid)?);
            }
            trees
                .get(&pid)
                .and_then(|t| t.get(dir.file_name()))
                .filter(|e| e.mode == Mode::Tree)
                .map(|e| e.id)
        }
    };
    dirs.insert(dir.clone(), found);
    Ok(found)
}
