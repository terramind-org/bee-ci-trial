//! Trees (SPEC §4.2).

use std::collections::BTreeMap;

use bee_core::{BeeError, Limits, Mode, ObjectId, ObjectKind, Result, Subcode, validate_name};

use crate::cbor::Value;
use crate::fields::{Extensions, Fields, id_value, top_map, validate_extensions};
use crate::hash_id;

/// One tree entry: `{1: name, 2: mode, 3: id, 4: size}`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TreeEntry {
    /// The entry name (one path component).
    pub name: Vec<u8>,
    /// The mode.
    pub mode: Mode,
    /// A blob id (file, executable, symlink), tree id (tree) or foreign id (gitlink).
    pub id: ObjectId,
    /// Blob length for files and symlinks; 0 for trees and gitlinks.
    pub size: u64,
}

impl TreeEntry {
    /// A file, executable or symlink entry.
    pub fn blob(name: impl Into<Vec<u8>>, mode: Mode, id: ObjectId, size: u64) -> TreeEntry {
        TreeEntry {
            name: name.into(),
            mode,
            id,
            size,
        }
    }

    /// A subtree entry.
    pub fn tree(name: impl Into<Vec<u8>>, id: ObjectId) -> TreeEntry {
        TreeEntry {
            name: name.into(),
            mode: Mode::Tree,
            id,
            size: 0,
        }
    }

    /// A gitlink (submodule) entry.
    pub fn gitlink(name: impl Into<Vec<u8>>, id: ObjectId) -> TreeEntry {
        TreeEntry {
            name: name.into(),
            mode: Mode::Gitlink,
            id,
            size: 0,
        }
    }

    /// Checks the name rules and that mode, id kind and size agree.
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        check_mode_id_size(self.mode, &self.id, self.size, "tree entry")
    }

    /// True when the entry is anything but a subtree.
    pub fn is_leaf(&self) -> bool {
        self.mode.is_leaf()
    }

    fn to_value(&self) -> Value {
        let mut m = BTreeMap::new();
        m.insert(1, Value::Bytes(self.name.clone()));
        m.insert(2, Value::Uint(self.mode.as_u64()));
        m.insert(3, id_value(&self.id));
        m.insert(4, Value::Uint(self.size));
        Value::Map(m)
    }

    fn from_value(v: Value) -> Result<TreeEntry> {
        let mut f = Fields::new("tree entry", v)?;
        let name = f.bytes(1)?;
        let mode = Mode::from_u64(f.uint(2)?)?;
        let id = f.object_id(3)?;
        let size = f.uint(4)?;
        f.finish()?;
        let e = TreeEntry {
            name,
            mode,
            id,
            size,
        };
        e.validate()?;
        Ok(e)
    }
}

/// Checks that a mode, id and size agree: blob modes take blob ids, trees take tree ids with
/// size 0, gitlinks take foreign ids with size 0.
pub(crate) fn check_mode_id_size(mode: Mode, id: &ObjectId, size: u64, what: &str) -> Result<()> {
    let expected = match mode {
        Mode::File | Mode::Executable | Mode::Symlink => ObjectKind::Blob,
        Mode::Tree => ObjectKind::Tree,
        Mode::Gitlink => ObjectKind::Foreign,
    };
    if id.kind() != expected {
        return Err(BeeError::invalid(
            Subcode::WrongKind,
            format!(
                "{what}: mode {} needs a {} id, got {id}",
                mode.as_u64(),
                expected.text_tag()
            ),
        ));
    }
    if !mode.is_blob() && size != 0 {
        return Err(BeeError::invalid(
            Subcode::InvalidField,
            format!("{what}: size must be 0 for mode {}", mode.as_u64()),
        ));
    }
    Ok(())
}

/// A tree: entries sorted bytewise by name, names unique.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Tree {
    entries: Vec<TreeEntry>,
    extensions: Extensions,
}

impl Tree {
    /// The empty tree (only valid as a root).
    pub fn empty() -> Tree {
        Tree::default()
    }

    /// A tree from entries already sorted bytewise by name; validates every rule.
    pub fn new(entries: Vec<TreeEntry>) -> Result<Tree> {
        Tree::with_extensions(entries, Extensions::new())
    }

    /// A tree from entries in any order; sorts them, then validates.
    pub fn from_unsorted(mut entries: Vec<TreeEntry>) -> Result<Tree> {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Tree::new(entries)
    }

    /// A tree with ignorable extension fields (even keys ≥ 64).
    pub fn with_extensions(entries: Vec<TreeEntry>, extensions: Extensions) -> Result<Tree> {
        validate_extensions(&extensions, &[])?;
        Tree::checked(entries, extensions, &Limits::DEFAULT)
    }

    fn checked(entries: Vec<TreeEntry>, extensions: Extensions, limits: &Limits) -> Result<Tree> {
        Limits::check("tree entries", limits.tree_entries, entries.len() as u64)?;
        for e in &entries {
            e.validate()?;
        }
        for pair in entries.windows(2) {
            if pair[0].name >= pair[1].name {
                return Err(BeeError::invalid(
                    Subcode::InvalidTree,
                    format!(
                        "tree entries unsorted or duplicated at {:?}",
                        String::from_utf8_lossy(&pair[1].name)
                    ),
                ));
            }
        }
        Ok(Tree {
            entries,
            extensions,
        })
    }

    /// The entries, sorted by name.
    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    /// Consumes the tree, returning its entries.
    pub fn into_entries(self) -> Vec<TreeEntry> {
        self.entries
    }

    /// Preserved extension fields.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the tree has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Looks up an entry by name (binary search).
    pub fn get(&self, name: &[u8]) -> Option<&TreeEntry> {
        self.entries
            .binary_search_by(|e| e.name.as_slice().cmp(name))
            .ok()
            .map(|i| &self.entries[i])
    }

    /// Fails with `INVALID_TREE` if this tree is empty; non-root trees must not be.
    pub fn check_non_root(&self) -> Result<()> {
        if self.is_empty() {
            Err(BeeError::invalid(
                Subcode::InvalidTree,
                "a non-root tree is empty",
            ))
        } else {
            Ok(())
        }
    }

    /// The canonical encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut m = top_map(&self.extensions);
        m.insert(
            1,
            Value::Array(self.entries.iter().map(TreeEntry::to_value).collect()),
        );
        Value::Map(m).encode()
    }

    /// The tree's id: BLAKE3 over the canonical encoding, kind tree.
    pub fn id(&self) -> ObjectId {
        hash_id(ObjectKind::Tree, &self.encode())
    }

    /// Decodes and validates with the default limits.
    pub fn decode(bytes: &[u8]) -> Result<Tree> {
        Tree::decode_with_limits(bytes, &Limits::DEFAULT)
    }

    /// Decodes and validates with the given limits.
    pub fn decode_with_limits(bytes: &[u8], limits: &Limits) -> Result<Tree> {
        let mut f = Fields::top("tree", bytes, limits)?;
        let raw = f.array(1)?;
        Limits::check("tree entries", limits.tree_entries, raw.len() as u64)?;
        let entries = raw
            .into_iter()
            .map(TreeEntry::from_value)
            .collect::<Result<Vec<_>>>()?;
        let extensions = f.finish_top()?;
        Tree::checked(entries, extensions, limits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_id;
    use bee_core::hex;

    fn file(name: &str, content: &[u8]) -> TreeEntry {
        TreeEntry::blob(name, Mode::File, blob_id(content), content.len() as u64)
    }

    #[test]
    fn empty_tree_encoding() {
        let t = Tree::empty();
        assert_eq!(hex::encode(&t.encode()), "a2000101 80".replace(' ', ""));
        assert_eq!(Tree::decode(&t.encode()).unwrap(), t);
        assert!(t.check_non_root().is_err());
        assert!(t.is_empty());
    }

    #[test]
    fn tree_round_trip_and_lookup() {
        let sub = Tree::new(vec![file("x", b"x")]).unwrap();
        let t = Tree::new(vec![
            file("a", b"hello"),
            TreeEntry::blob("b.sh", Mode::Executable, blob_id(b"#!"), 2),
            TreeEntry::blob("c", Mode::Symlink, blob_id(b"a"), 1),
            TreeEntry::tree("d", sub.id()),
            TreeEntry::gitlink("e", ObjectId::git_foreign(&[7; 20]).unwrap()),
        ])
        .unwrap();
        let bytes = t.encode();
        let back = Tree::decode(&bytes).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.id(), t.id());
        assert_eq!(t.id().kind(), ObjectKind::Tree);
        assert_eq!(t.get(b"d").unwrap().mode, Mode::Tree);
        assert!(t.get(b"zz").is_none());
        assert_eq!(t.len(), 5);
        assert!(t.check_non_root().is_ok());
        assert!(t.get(b"a").unwrap().is_leaf());
        assert_eq!(t.clone().into_entries().len(), 5);
    }

    #[test]
    fn tree_rejects_unsorted() {
        let err = Tree::new(vec![file("b", b""), file("a", b"")]).unwrap_err();
        assert!(err.is_invalid(Subcode::InvalidTree));
        let sorted = Tree::from_unsorted(vec![file("b", b""), file("a", b"")]).unwrap();
        assert_eq!(sorted.entries()[0].name, b"a");
    }

    #[test]
    fn tree_rejects_duplicates() {
        let err = Tree::from_unsorted(vec![file("a", b"1"), file("a", b"2")]).unwrap_err();
        assert!(err.is_invalid(Subcode::InvalidTree));
    }

    #[test]
    fn tree_sorts_bytewise_not_by_path() {
        // "a-b" < "a" + "/"... but as names, "a" < "a-b" < "a0": plain bytewise order.
        let t =
            Tree::from_unsorted(vec![file("a0", b""), file("a-b", b""), file("a", b"")]).unwrap();
        let names: Vec<_> = t.entries().iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, [b"a".to_vec(), b"a-b".to_vec(), b"a0".to_vec()]);
        let upper = Tree::from_unsorted(vec![file("b", b""), file("B", b"")]).unwrap();
        assert_eq!(upper.entries()[0].name, b"B");
    }

    #[test]
    fn tree_rejects_bad_names() {
        for bad in [&b""[..], b".", b"..", b"a/b", b"a\0", b".bee", b".BEE"] {
            let e = TreeEntry::blob(bad, Mode::File, blob_id(b""), 0);
            assert!(
                Tree::new(vec![e])
                    .unwrap_err()
                    .is_invalid(Subcode::InvalidName),
                "{bad:?}"
            );
        }
        let long = TreeEntry::blob(vec![b'a'; 256], Mode::File, blob_id(b""), 0);
        assert!(Tree::new(vec![long]).is_err());
    }

    #[test]
    fn tree_rejects_mode_id_mismatch() {
        let t = Tree::empty().id();
        let b = blob_id(b"x");
        let g = ObjectId::git_foreign(&[1; 20]).unwrap();
        for e in [
            TreeEntry::blob("a", Mode::File, t, 0),
            TreeEntry::blob("a", Mode::Symlink, g, 0),
            TreeEntry::tree("a", b),
            TreeEntry::gitlink("a", b),
        ] {
            assert!(
                e.validate().unwrap_err().is_invalid(Subcode::WrongKind),
                "{e:?}"
            );
        }
        let sized_tree = TreeEntry {
            size: 3,
            ..TreeEntry::tree("a", t)
        };
        assert!(
            sized_tree
                .validate()
                .unwrap_err()
                .is_invalid(Subcode::InvalidField)
        );
        let sized_link = TreeEntry {
            size: 1,
            ..TreeEntry::gitlink("a", g)
        };
        assert!(sized_link.validate().is_err());
    }

    #[test]
    fn tree_preserves_even_extensions() {
        let mut ext = Extensions::new();
        ext.insert(64, Value::Text("future".into()));
        let t = Tree::with_extensions(vec![file("a", b"")], ext.clone()).unwrap();
        let back = Tree::decode(&t.encode()).unwrap();
        assert_eq!(back.extensions(), &ext);
        assert_eq!(back.encode(), t.encode());
        assert_ne!(
            t.id(),
            Tree::new(vec![file("a", b"")]).unwrap().id(),
            "extensions count toward the id"
        );
        let mut odd = Extensions::new();
        odd.insert(65, Value::Uint(1));
        assert!(Tree::with_extensions(vec![], odd).is_err());
        let mut low = Extensions::new();
        low.insert(2, Value::Uint(1));
        assert!(Tree::with_extensions(vec![], low).is_err());
    }

    fn with_field(t: &Tree, key: u64, v: Value) -> Vec<u8> {
        let Value::Map(mut m) = Value::decode(&t.encode()).unwrap() else {
            unreachable!()
        };
        m.insert(key, v);
        Value::Map(m).encode()
    }

    #[test]
    fn tree_decode_rejections() {
        let t = Tree::new(vec![file("a", b"")]).unwrap();
        let odd = with_field(&t, 65, Value::Uint(0));
        assert!(
            Tree::decode(&odd)
                .unwrap_err()
                .is_invalid(Subcode::UnknownCriticalExtension)
        );
        let unknown = with_field(&t, 2, Value::Uint(0));
        assert!(
            Tree::decode(&unknown)
                .unwrap_err()
                .is_invalid(Subcode::InvalidField)
        );
        let v2 = with_field(&t, 0, Value::Uint(2));
        assert!(
            Tree::decode(&v2)
                .unwrap_err()
                .is_invalid(Subcode::UnknownFormatVersion)
        );
        let vt = with_field(&t, 0, Value::Text("1".into()));
        assert!(
            Tree::decode(&vt)
                .unwrap_err()
                .is_invalid(Subcode::InvalidField)
        );
        let no_entries = Value::Map([(0, Value::Uint(1))].into_iter().collect()).encode();
        assert!(
            Tree::decode(&no_entries)
                .unwrap_err()
                .is_invalid(Subcode::InvalidField)
        );
        let no_version = Value::Map([(1, Value::Array(vec![]))].into_iter().collect()).encode();
        assert!(
            Tree::decode(&no_version)
                .unwrap_err()
                .is_invalid(Subcode::InvalidField)
        );
        let mut trailing = t.encode();
        trailing.push(0);
        assert!(
            Tree::decode(&trailing)
                .unwrap_err()
                .is_invalid(Subcode::NonCanonical)
        );
        assert!(Tree::decode(&Value::Uint(1).encode()).is_err(), "not a map");
        // An entry with an extra key is rejected: nested maps take no extensions.
        let mut entry = BTreeMap::new();
        entry.insert(1, Value::Bytes(b"a".to_vec()));
        entry.insert(2, Value::Uint(1));
        entry.insert(3, id_value(&blob_id(b"")));
        entry.insert(4, Value::Uint(0));
        entry.insert(64, Value::Uint(0));
        let bad = Value::Map(
            [
                (0, Value::Uint(1)),
                (1, Value::Array(vec![Value::Map(entry)])),
            ]
            .into_iter()
            .collect(),
        )
        .encode();
        assert!(
            Tree::decode(&bad)
                .unwrap_err()
                .is_invalid(Subcode::InvalidField)
        );
    }

    #[test]
    fn tree_entry_limit() {
        let limits = Limits {
            tree_entries: 1,
            ..Limits::DEFAULT
        };
        let t = Tree::new(vec![file("a", b""), file("b", b"")]).unwrap();
        let err = Tree::decode_with_limits(&t.encode(), &limits).unwrap_err();
        assert_eq!(err.reason, bee_core::Reason::TooLarge);
        let tiny = Limits {
            encoded_object_bytes: 4,
            ..Limits::DEFAULT
        };
        assert_eq!(
            Tree::decode_with_limits(&t.encode(), &tiny)
                .unwrap_err()
                .reason,
            bee_core::Reason::TooLarge
        );
    }
}
