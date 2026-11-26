//! Reference apply-delta and diff over a flat file map (SPEC §4.4).
//!
//! Production builds trees incrementally, reading only the directories a delta touches. The
//! model ignores trees entirely: a snapshot is a sorted map from full path to leaf, a delta
//! is applied by editing that map, and the result is valid exactly when no leaf path is a
//! directory prefix of another. Trees appear only at the end, to compute the root id with the
//! shared [`Tree`] encoding.

use std::collections::{BTreeMap, BTreeSet};

use bee_core::{Mode, ObjectId, ObjectKind, RepoPath};
use bee_object::{DeltaEntry, DeltaOp, Tree, TreeEntry};

/// A leaf: what a file path holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Leaf {
    /// Mode (never [`Mode::Tree`]).
    pub mode: Mode,
    /// Blob id, or foreign id for a gitlink.
    pub id: ObjectId,
    /// Blob length; 0 for gitlinks.
    pub size: u64,
}

/// A snapshot: full path bytes to leaf, sorted bytewise.
pub type FileMap = BTreeMap<Vec<u8>, Leaf>;

/// Why the model rejected a delta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// The delta's shape is wrong regardless of the snapshot.
    Shape(String),
    /// The delta does not apply to this snapshot.
    Apply(String),
}

fn is_inside(dir: &[u8], path: &[u8]) -> bool {
    path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
}

fn leaf_ok(mode: Mode, id: &ObjectId, size: u64) -> bool {
    match mode {
        Mode::File | Mode::Executable | Mode::Symlink => id.kind() == ObjectKind::Blob,
        Mode::Gitlink => id.kind() == ObjectKind::Foreign && size == 0,
        Mode::Tree => false,
    }
}

/// Checks a delta's shape: strictly increasing paths, valid puts, no put inside another put,
/// no delete inside another delete, and no delete without a parent.
pub fn check_shape(delta: &[DeltaEntry], has_parent: bool) -> Result<(), ModelError> {
    for w in delta.windows(2) {
        if w[0].path.as_bytes() >= w[1].path.as_bytes() {
            return Err(ModelError::Shape("paths not strictly increasing".into()));
        }
    }
    let mut puts: Vec<&[u8]> = Vec::new();
    let mut deletes: Vec<&[u8]> = Vec::new();
    for e in delta {
        match e.op {
            DeltaOp::Put { mode, id, size } => {
                if !leaf_ok(mode, &id, size) {
                    return Err(ModelError::Shape("bad put".into()));
                }
                puts.push(e.path.as_bytes());
            }
            DeltaOp::Delete => {
                if !has_parent {
                    return Err(ModelError::Shape("delete without a parent".into()));
                }
                deletes.push(e.path.as_bytes());
            }
        }
    }
    for set in [&puts, &deletes] {
        for a in set.iter() {
            if set.iter().any(|b| is_inside(a, b)) {
                return Err(ModelError::Shape("nested paths with the same op".into()));
            }
        }
    }
    Ok(())
}

/// Applies a delta to a snapshot.
///
/// Rules: every delete names an existing leaf; every put changes its path; afterwards no
/// leaf lies inside another leaf's path (so replacing a directory with a file must delete
/// every leaf under it, and replacing a file with a directory must delete the file).
pub fn apply(files: &FileMap, delta: &[DeltaEntry]) -> Result<FileMap, ModelError> {
    check_shape(delta, true)?;
    let mut out = files.clone();
    for e in delta {
        let p = e.path.as_bytes();
        match e.op {
            DeltaOp::Delete => {
                if out.remove(p).is_none() {
                    return Err(ModelError::Apply(format!("delete of non-leaf {}", e.path)));
                }
            }
            DeltaOp::Put { .. } => {}
        }
    }
    for e in delta {
        if let DeltaOp::Put { mode, id, size } = e.op {
            let leaf = Leaf { mode, id, size };
            if files.get(e.path.as_bytes()) == Some(&leaf) {
                return Err(ModelError::Apply(format!("put does not change {}", e.path)));
            }
            out.insert(e.path.as_bytes().to_vec(), leaf);
        }
    }
    let paths: Vec<&Vec<u8>> = out.keys().collect();
    let all: BTreeSet<&[u8]> = paths.iter().map(|p| p.as_slice()).collect();
    for p in &paths {
        for (i, b) in p.iter().enumerate() {
            if *b == b'/' && all.contains(&p[..i]) {
                return Err(ModelError::Apply(format!(
                    "{} is both a file and a directory",
                    String::from_utf8_lossy(&p[..i])
                )));
            }
        }
    }
    Ok(out)
}

/// The exact file-level difference from `old` to `new`, sorted by path.
pub fn diff(old: &FileMap, new: &FileMap) -> Vec<DeltaEntry> {
    let keys: BTreeSet<&Vec<u8>> = old.keys().chain(new.keys()).collect();
    let mut out = Vec::new();
    for k in keys {
        let path = RepoPath::new(k.clone()).expect("snapshot paths are valid");
        match (old.get(k), new.get(k)) {
            (Some(_), None) => out.push(DeltaEntry::delete(path)),
            (a, Some(b)) if a != Some(b) => out.push(DeltaEntry::put(path, b.mode, b.id, b.size)),
            _ => {}
        }
    }
    out
}

enum Node {
    Leaf(Leaf),
    Dir(BTreeMap<Vec<u8>, Node>),
}

fn insert(dir: &mut BTreeMap<Vec<u8>, Node>, components: &[&[u8]], leaf: Leaf) {
    let (first, rest) = components.split_first().expect("non-empty path");
    if rest.is_empty() {
        dir.insert(first.to_vec(), Node::Leaf(leaf));
    } else {
        let child = dir
            .entry(first.to_vec())
            .or_insert_with(|| Node::Dir(BTreeMap::new()));
        if let Node::Dir(d) = child {
            insert(d, rest, leaf);
        }
    }
}

fn encode(dir: &BTreeMap<Vec<u8>, Node>, out: &mut BTreeMap<ObjectId, Tree>) -> ObjectId {
    let entries = dir
        .iter()
        .map(|(name, node)| match node {
            Node::Leaf(l) => TreeEntry {
                name: name.clone(),
                mode: l.mode,
                id: l.id,
                size: l.size,
            },
            Node::Dir(d) => TreeEntry::tree(name.clone(), encode(d, out)),
        })
        .collect();
    let tree = Tree::new(entries).expect("model trees are valid");
    let id = tree.id();
    out.insert(id, tree);
    id
}

/// Every tree of a snapshot and the root id (the empty snapshot has the empty root tree).
pub fn trees(files: &FileMap) -> (ObjectId, BTreeMap<ObjectId, Tree>) {
    let mut root = BTreeMap::new();
    for (path, leaf) in files {
        let comps: Vec<&[u8]> = path.split(|b| *b == b'/').collect();
        insert(&mut root, &comps, *leaf);
    }
    let mut out = BTreeMap::new();
    let id = encode(&root, &mut out);
    (id, out)
}

/// The root tree id of a snapshot.
pub fn root_id(files: &FileMap) -> ObjectId {
    trees(files).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_object::blob_id;

    fn leaf(data: &[u8]) -> Leaf {
        Leaf {
            mode: Mode::File,
            id: blob_id(data),
            size: data.len() as u64,
        }
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn apply_and_diff() {
        let mut a = FileMap::new();
        a.insert(b"x/y".to_vec(), leaf(b"1"));
        a.insert(b"z".to_vec(), leaf(b"2"));
        // file to directory: z becomes z/w
        let d = vec![
            DeltaEntry::delete(p("z")),
            DeltaEntry::put(p("z/w"), Mode::File, blob_id(b"3"), 1),
        ];
        let b = apply(&a, &d).unwrap();
        assert_eq!(diff(&a, &b), d);
        // directory to file without deleting the leaf below
        let bad = vec![DeltaEntry::put(p("x"), Mode::File, blob_id(b"4"), 1)];
        assert!(matches!(apply(&b, &bad), Err(ModelError::Apply(_))));
        // unchanged put
        let same = vec![DeltaEntry::put(p("x/y"), Mode::File, blob_id(b"1"), 1)];
        assert!(apply(&b, &same).is_err());
        // delete of a directory
        assert!(apply(&b, &[DeltaEntry::delete(p("x"))]).is_err());
        // shape errors
        assert!(matches!(
            check_shape(&[DeltaEntry::delete(p("a"))], false),
            Err(ModelError::Shape(_))
        ));
        assert!(
            check_shape(
                &[DeltaEntry::delete(p("b")), DeltaEntry::delete(p("a"))],
                true
            )
            .is_err()
        );
        assert!(
            check_shape(
                &[DeltaEntry::delete(p("a")), DeltaEntry::delete(p("a/b"))],
                true
            )
            .is_err()
        );
        assert_eq!(root_id(&FileMap::new()), Tree::empty().id());
        let (root, ts) = trees(&b);
        assert_eq!(ts.len(), 3);
        assert!(ts.contains_key(&root));
    }
}
