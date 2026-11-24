//! The per-tree git note: tree extension key 64 (SPEC §4.2, §17.4).
//!
//! A bee tree holds names, bee modes and ids. Rebuilding the git tree from it gives
//! canonical git modes in git order, which is right for nearly every tree. The note records
//! the rest, so the rebuilt git tree is byte-identical to the imported one:
//!
//! ```text
//! note   = { ?1: [[name, mode], ...],          ; visible entries whose git mode is non-canonical
//!            ?2: [[name, mode, note], ...],    ; hidden entries: git subtrees with no files
//!            ?3: [name, ...] }                 ; original entry order, when not git order
//! ```
//!
//! - `name` and `mode` are byte strings; `mode` is the exact octal digits (`100664`,
//!   `040000`). Arrays 1 and 2 are sorted bytewise by name, names unique.
//! - A mode in array 1 must classify to the same kind as the entry's bee mode and must not
//!   be the canonical spelling.
//! - A hidden entry is a git directory that contains no files (only, recursively, other
//!   empty directories). Bee trees cannot hold empty subtrees, so it lives here; its nested
//!   note describes its own entries, which are all hidden. Its mode must be a directory mode.
//! - Array 3 lists every entry name (visible and hidden) exactly once, and is present only
//!   when that order differs from git's order.
//! - Empty arrays are omitted; a note with nothing to say is not written at all.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use bee_core::{BeeError, Result, Subcode, validate_name};
use bee_object::{Extensions, Tree, TreeEntry, Value};

use crate::oid::{GitOid, ObjectFormat};
use crate::tree::{EntryKind, GitTree, GitTreeEntry, git_order};

/// The tree extension key of the git note.
pub const GIT_TREE_NOTE_KEY: u64 = 64;

/// An entry git has and the bee tree does not: an empty directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct HiddenEntry {
    /// The entry name.
    pub name: Vec<u8>,
    /// The exact git mode digits (a directory mode).
    pub mode: Vec<u8>,
    /// The note describing the directory's own (hidden) entries.
    pub note: TreeNote,
}

/// The git note of one tree.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct TreeNote {
    /// Visible entries whose git mode is not the canonical spelling: name → mode digits.
    pub modes: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Hidden entries, sorted by name.
    pub hidden: Vec<HiddenEntry>,
    /// The original entry order, when it is not git's order.
    pub order: Option<Vec<Vec<u8>>>,
}

fn bad(why: impl std::fmt::Display) -> BeeError {
    BeeError::invalid(Subcode::InvalidTree, format!("git tree note: {why}"))
}

fn bytes_of(v: &Value) -> Result<&[u8]> {
    match v {
        Value::Bytes(b) => Ok(b),
        _ => Err(bad("expected a byte string")),
    }
}

fn array_of(v: &Value) -> Result<&[Value]> {
    match v {
        Value::Array(a) => Ok(a),
        _ => Err(bad("expected an array")),
    }
}

impl TreeNote {
    /// True when the note records nothing.
    pub fn is_empty(&self) -> bool {
        self.modes.is_empty() && self.hidden.is_empty() && self.order.is_none()
    }

    /// The note's CBOR value.
    pub fn to_value(&self) -> Value {
        let mut m = BTreeMap::new();
        if !self.modes.is_empty() {
            m.insert(
                1,
                Value::Array(
                    self.modes
                        .iter()
                        .map(|(n, md)| {
                            Value::Array(vec![Value::Bytes(n.clone()), Value::Bytes(md.clone())])
                        })
                        .collect(),
                ),
            );
        }
        if !self.hidden.is_empty() {
            m.insert(
                2,
                Value::Array(
                    self.hidden
                        .iter()
                        .map(|h| {
                            Value::Array(vec![
                                Value::Bytes(h.name.clone()),
                                Value::Bytes(h.mode.clone()),
                                h.note.to_value(),
                            ])
                        })
                        .collect(),
                ),
            );
        }
        if let Some(order) = &self.order {
            m.insert(
                3,
                Value::Array(order.iter().map(|n| Value::Bytes(n.clone())).collect()),
            );
        }
        Value::Map(m)
    }

    /// Parses a note value, checking the structural rules in the module docs (those that do
    /// not need the tree's entries).
    pub fn from_value(v: &Value) -> Result<TreeNote> {
        let Value::Map(m) = v else {
            return Err(bad("expected a map"));
        };
        if let Some(k) = m.keys().find(|k| !(1..=3).contains(*k)) {
            return Err(bad(format!("unknown key {k}")));
        }
        let mut note = TreeNote::default();
        if let Some(v) = m.get(&1) {
            let items = array_of(v)?;
            if items.is_empty() {
                return Err(bad("empty mode list"));
            }
            let mut prev: Option<&[u8]> = None;
            for it in items {
                let pair = array_of(it)?;
                let [name, mode] = pair else {
                    return Err(bad("mode entry is not [name, mode]"));
                };
                let (name, mode) = (bytes_of(name)?, bytes_of(mode)?);
                if prev.is_some_and(|p| p >= name) {
                    return Err(bad("mode entries unsorted or duplicated"));
                }
                EntryKind::from_mode(mode)?;
                prev = Some(name);
                note.modes.insert(name.to_vec(), mode.to_vec());
            }
        }
        if let Some(v) = m.get(&2) {
            let items = array_of(v)?;
            if items.is_empty() {
                return Err(bad("empty hidden list"));
            }
            for it in items {
                let triple = array_of(it)?;
                let [name, mode, sub] = triple else {
                    return Err(bad("hidden entry is not [name, mode, note]"));
                };
                let (name, mode) = (bytes_of(name)?, bytes_of(mode)?);
                validate_name(name)?;
                if EntryKind::from_mode(mode)? != EntryKind::Tree {
                    return Err(bad("hidden entry mode is not a directory"));
                }
                let sub = TreeNote::from_value(sub)?;
                if !sub.modes.is_empty() {
                    return Err(bad("a hidden directory has no visible entries"));
                }
                if note
                    .hidden
                    .last()
                    .is_some_and(|h: &HiddenEntry| h.name.as_slice() >= name)
                {
                    return Err(bad("hidden entries unsorted or duplicated"));
                }
                note.hidden.push(HiddenEntry {
                    name: name.to_vec(),
                    mode: mode.to_vec(),
                    note: sub,
                });
            }
        }
        if let Some(v) = m.get(&3) {
            let names = array_of(v)?
                .iter()
                .map(|n| bytes_of(n).map(<[u8]>::to_vec))
                .collect::<Result<Vec<_>>>()?;
            note.order = Some(names);
        }
        Ok(note)
    }

    /// The note stored in a tree's extensions, or an empty note.
    pub fn from_tree(tree: &Tree) -> Result<TreeNote> {
        match tree.extensions().get(&GIT_TREE_NOTE_KEY) {
            Some(v) => {
                let note = TreeNote::from_value(v)?;
                if note.is_empty() {
                    return Err(bad("an empty note is never written"));
                }
                Ok(note)
            }
            None => Ok(TreeNote::default()),
        }
    }

    /// A bee tree from entries (any order) carrying this note, if it is not empty.
    pub fn build_tree(&self, mut entries: Vec<TreeEntry>) -> Result<Tree> {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let mut ext = Extensions::new();
        if !self.is_empty() {
            ext.insert(GIT_TREE_NOTE_KEY, self.to_value());
        }
        Tree::with_extensions(entries, ext)
    }

    /// The git tree of a hidden directory described by this note (all entries hidden).
    pub fn hidden_git_tree(&self, format: ObjectFormat) -> Result<GitTree> {
        let mut entries = Vec::with_capacity(self.hidden.len());
        for h in &self.hidden {
            let sub = h.note.hidden_git_tree(format)?;
            entries.push(GitTreeEntry {
                mode: h.mode.clone(),
                name: h.name.clone(),
                oid: format.hash_object(crate::object::ObjectType::Tree, &sub.serialize())?,
            });
        }
        self.arrange(entries)
    }

    /// Puts git entries into final order: the recorded order if any (which must name every
    /// entry exactly once and differ from git order), otherwise git order. Names must be
    /// unique.
    pub fn arrange(&self, mut entries: Vec<GitTreeEntry>) -> Result<GitTree> {
        let is_dir = |e: &GitTreeEntry| e.kind() == Ok(EntryKind::Tree);
        let names: BTreeSet<&[u8]> = entries.iter().map(|e| e.name.as_slice()).collect();
        if names.len() != entries.len() {
            return Err(bad("duplicate entry names"));
        }
        entries.sort_by(|a, b| git_order(&a.name, is_dir(a), &b.name, is_dir(b)));
        let Some(order) = &self.order else {
            return Ok(GitTree { entries });
        };
        if order.len() != entries.len() {
            return Err(bad("order does not list every entry"));
        }
        let mut by_name: BTreeMap<Vec<u8>, GitTreeEntry> = entries
            .iter()
            .map(|e| (e.name.clone(), e.clone()))
            .collect();
        let mut out = Vec::with_capacity(order.len());
        for n in order {
            out.push(
                by_name
                    .remove(n)
                    .ok_or_else(|| bad("order names an unknown entry"))?,
            );
        }
        if out == entries {
            return Err(bad("order is git order and must be omitted"));
        }
        Ok(GitTree { entries: out })
    }

    /// Checks the note against the tree's visible entries: mode overrides name visible
    /// entries of a matching kind and are non-canonical; hidden names do not collide.
    pub fn check_against(&self, tree: &Tree) -> Result<()> {
        for (name, mode) in &self.modes {
            let e = tree
                .get(name)
                .ok_or_else(|| bad("mode override names no entry"))?;
            let kind = EntryKind::from_mode(mode)?;
            if kind != EntryKind::from_bee(e.mode) {
                return Err(bad("mode override changes the entry kind"));
            }
            if kind.canonical_mode() == mode.as_slice() {
                return Err(bad("mode override is the canonical mode"));
            }
        }
        if self.hidden.iter().any(|h| tree.get(&h.name).is_some()) {
            return Err(bad("hidden entry shadows a visible entry"));
        }
        Ok(())
    }
}

/// True when entries are not strictly in git's order, so the note must record their order.
pub(crate) fn needs_order(entries: &[GitTreeEntry]) -> bool {
    entries.windows(2).any(|w| {
        let d = |e: &GitTreeEntry| e.kind() == Ok(EntryKind::Tree);
        git_order(&w[0].name, d(&w[0]), &w[1].name, d(&w[1])) != Ordering::Less
    })
}

/// The id of a git tree.
pub(crate) fn tree_oid(format: ObjectFormat, tree: &GitTree) -> Result<GitOid> {
    format.hash_object(crate::object::ObjectType::Tree, &tree.serialize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::Mode;

    fn sample() -> TreeNote {
        let mut modes = BTreeMap::new();
        modes.insert(b"f".to_vec(), b"100664".to_vec());
        TreeNote {
            modes,
            hidden: vec![HiddenEntry {
                name: b"empty".to_vec(),
                mode: b"40000".to_vec(),
                note: TreeNote::default(),
            }],
            order: Some(vec![b"f".to_vec(), b"empty".to_vec()]),
        }
    }

    #[test]
    fn value_round_trip() {
        let n = sample();
        assert_eq!(TreeNote::from_value(&n.to_value()).unwrap(), n);
        assert!(TreeNote::default().is_empty());
        assert_eq!(TreeNote::default().to_value(), Value::Map(BTreeMap::new()));
    }

    #[test]
    fn rejects_malformed_notes() {
        let pair = |n: &[u8], m: &[u8]| {
            Value::Array(vec![Value::Bytes(n.to_vec()), Value::Bytes(m.to_vec())])
        };
        let map = |k: u64, v: Value| Value::Map(BTreeMap::from([(k, v)]));
        for bad_value in [
            Value::Uint(1),
            map(9, Value::Array(vec![])),
            map(1, Value::Array(vec![])),
            map(
                1,
                Value::Array(vec![pair(b"b", b"100664"), pair(b"a", b"100664")]),
            ),
            map(1, Value::Array(vec![pair(b"a", b"999")])),
            map(1, Value::Array(vec![Value::Uint(3)])),
            map(2, Value::Array(vec![])),
            map(
                2,
                Value::Array(vec![Value::Array(vec![
                    Value::Bytes(b"x".to_vec()),
                    Value::Bytes(b"100644".to_vec()),
                    Value::Map(BTreeMap::new()),
                ])]),
            ),
            map(
                2,
                Value::Array(vec![Value::Array(vec![
                    Value::Bytes(b".bee".to_vec()),
                    Value::Bytes(b"40000".to_vec()),
                    Value::Map(BTreeMap::new()),
                ])]),
            ),
            map(3, Value::Array(vec![Value::Uint(1)])),
        ] {
            assert!(TreeNote::from_value(&bad_value).is_err(), "{bad_value:?}");
        }
    }

    #[test]
    fn arrange_and_hidden_trees() {
        let fmt = ObjectFormat::Sha1;
        let n = sample();
        let hidden = n.hidden[0].note.hidden_git_tree(fmt).unwrap();
        assert!(hidden.entries.is_empty());
        let f = GitTreeEntry {
            mode: b"100664".to_vec(),
            name: b"f".to_vec(),
            oid: GitOid::Sha1([1; 20]),
        };
        let e = GitTreeEntry {
            mode: b"40000".to_vec(),
            name: b"empty".to_vec(),
            oid: fmt.empty_tree(),
        };
        let t = n.arrange(vec![e.clone(), f.clone()]).unwrap();
        assert_eq!(t.entries, vec![f.clone(), e.clone()]);
        assert!(needs_order(&t.entries));
        let plain = TreeNote::default()
            .arrange(vec![f.clone(), e.clone()])
            .unwrap();
        assert_eq!(plain.entries, vec![e.clone(), f.clone()]);
        assert!(!needs_order(&plain.entries));
        let git_order_recorded = TreeNote {
            order: Some(vec![b"empty".to_vec(), b"f".to_vec()]),
            ..TreeNote::default()
        };
        assert!(
            git_order_recorded
                .arrange(vec![f.clone(), e.clone()])
                .is_err()
        );
        assert!(
            TreeNote::default()
                .arrange(vec![f.clone(), f.clone()])
                .is_err()
        );
        let short = TreeNote {
            order: Some(vec![b"f".to_vec()]),
            ..TreeNote::default()
        };
        assert!(short.arrange(vec![f.clone(), e.clone()]).is_err());
        assert_eq!(
            tree_oid(fmt, &GitTree::default()).unwrap(),
            fmt.empty_tree()
        );
    }

    #[test]
    fn check_against_tree() {
        let blob = bee_object::blob_id(b"x");
        let n = sample();
        let tree = n
            .build_tree(vec![TreeEntry::blob("f", Mode::File, blob, 1)])
            .unwrap();
        assert_eq!(TreeNote::from_tree(&tree).unwrap(), n);
        n.check_against(&tree).unwrap();
        let exec = Tree::new(vec![TreeEntry::blob("f", Mode::Executable, blob, 1)]).unwrap();
        assert!(n.check_against(&exec).is_err());
        let missing = Tree::new(vec![TreeEntry::blob("g", Mode::File, blob, 1)]).unwrap();
        assert!(n.check_against(&missing).is_err());
        let mut canon = TreeNote::default();
        canon.modes.insert(b"f".to_vec(), b"100644".to_vec());
        let plain = Tree::new(vec![TreeEntry::blob("f", Mode::File, blob, 1)]).unwrap();
        assert!(canon.check_against(&plain).is_err());
        let shadow = TreeNote {
            hidden: vec![HiddenEntry {
                name: b"f".to_vec(),
                mode: b"40000".to_vec(),
                note: TreeNote::default(),
            }],
            ..TreeNote::default()
        };
        assert!(shadow.check_against(&plain).is_err());
        assert_eq!(TreeNote::from_tree(&plain).unwrap(), TreeNote::default());
    }
}
