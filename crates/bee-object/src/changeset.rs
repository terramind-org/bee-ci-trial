//! Changesets and deltas (SPEC §4.4).

use std::collections::{BTreeMap, BTreeSet};

use bee_core::{BeeError, ChangeId, Limits, Mode, ObjectId, ObjectKind, RepoPath, Result, Subcode};

use crate::cbor::Value;
use crate::fields::{Extensions, Fields, field_error, id_value, top_map, validate_extensions};
use crate::hash_id;
use crate::meta::Meta;
use crate::tree::{Tree, check_mode_id_size};

/// What a delta entry does to its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeltaOp {
    /// `{1: mode, 2: id, 3: size}`: the path now holds this leaf.
    Put {
        /// Any mode but [`Mode::Tree`].
        mode: Mode,
        /// Blob id, or foreign id for a gitlink.
        id: ObjectId,
        /// Blob length; 0 for gitlinks.
        size: u64,
    },
    /// The unsigned integer `0`: the path's leaf is removed.
    Delete,
}

impl DeltaOp {
    /// Checks that a put's mode, id and size agree and that it is not a tree.
    pub fn validate(&self) -> Result<()> {
        match *self {
            DeltaOp::Delete => Ok(()),
            DeltaOp::Put {
                mode: Mode::Tree, ..
            } => Err(BeeError::invalid(
                Subcode::InvalidMode,
                "a delta put cannot have mode tree",
            )),
            DeltaOp::Put { mode, id, size } => check_mode_id_size(mode, &id, size, "delta put"),
        }
    }

    fn to_value(self) -> Value {
        match self {
            DeltaOp::Delete => Value::Uint(0),
            DeltaOp::Put { mode, id, size } => Value::Map(BTreeMap::from([
                (1, Value::Uint(mode.as_u64())),
                (2, id_value(&id)),
                (3, Value::Uint(size)),
            ])),
        }
    }

    fn from_value(v: Value) -> Result<DeltaOp> {
        match v {
            Value::Uint(0) => Ok(DeltaOp::Delete),
            v @ Value::Map(_) => {
                let mut f = Fields::new("delta put", v)?;
                let mode = Mode::from_u64(f.uint(1)?)?;
                let id = f.object_id(2)?;
                let size = f.uint(3)?;
                f.finish()?;
                let op = DeltaOp::Put { mode, id, size };
                op.validate()?;
                Ok(op)
            }
            _ => Err(field_error("delta entry", "op must be a put map or 0")),
        }
    }
}

/// One delta entry: `{1: path, 2: op}`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeltaEntry {
    /// The path, normalized per SPEC §4.5.
    pub path: RepoPath,
    /// The operation.
    pub op: DeltaOp,
}

impl DeltaEntry {
    /// A put entry.
    pub fn put(path: RepoPath, mode: Mode, id: ObjectId, size: u64) -> DeltaEntry {
        DeltaEntry {
            path,
            op: DeltaOp::Put { mode, id, size },
        }
    }

    /// A delete entry.
    pub fn delete(path: RepoPath) -> DeltaEntry {
        DeltaEntry {
            path,
            op: DeltaOp::Delete,
        }
    }

    fn to_value(&self) -> Value {
        Value::Map(BTreeMap::from([
            (1, Value::Bytes(self.path.as_bytes().to_vec())),
            (2, self.op.to_value()),
        ]))
    }

    fn from_value(v: Value) -> Result<DeltaEntry> {
        let mut f = Fields::new("delta entry", v)?;
        let path = RepoPath::new(f.bytes(1)?)?;
        let op = DeltaOp::from_value(f.req(2)?)?;
        f.finish()?;
        Ok(DeltaEntry { path, op })
    }
}

fn delta_error(message: String) -> BeeError {
    BeeError::invalid(Subcode::InvalidDelta, message)
}

/// Checks a delta's shape without looking at trees: sorted bytewise by path with no
/// duplicates; every op valid; no two puts where one path is inside the other (a path cannot
/// be both a file and a directory afterwards); no two deletes where one path is inside the
/// other (it could not have been both before); and, for a parentless changeset, only puts.
pub fn validate_delta(delta: &[DeltaEntry], has_parent: bool, limits: &Limits) -> Result<()> {
    Limits::check("delta entries", limits.delta_entries, delta.len() as u64)?;
    for pair in delta.windows(2) {
        if pair[0].path >= pair[1].path {
            return Err(delta_error(format!(
                "delta unsorted or duplicated at {}",
                pair[1].path
            )));
        }
    }
    let mut puts = BTreeSet::new();
    let mut deletes = BTreeSet::new();
    for e in delta {
        e.op.validate()?;
        match e.op {
            DeltaOp::Put { .. } => puts.insert(e.path.as_bytes()),
            DeltaOp::Delete => deletes.insert(e.path.as_bytes()),
        };
        if !has_parent && e.op == DeltaOp::Delete {
            return Err(delta_error(format!(
                "delete of {} without a parent",
                e.path
            )));
        }
    }
    for e in delta {
        let set = match e.op {
            DeltaOp::Put { .. } => &puts,
            DeltaOp::Delete => &deletes,
        };
        for anc in e.path.ancestors() {
            if set.contains(anc.as_bytes()) {
                return Err(delta_error(format!(
                    "{} and {} cannot both be {}",
                    anc,
                    e.path,
                    if e.op == DeltaOp::Delete {
                        "deleted"
                    } else {
                        "put"
                    }
                )));
            }
        }
    }
    Ok(())
}

/// A changeset (SPEC §4.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changeset {
    parents: Vec<ObjectId>,
    root: ObjectId,
    meta: ObjectId,
    change_id: ChangeId,
    delta: Vec<DeltaEntry>,
    extensions: Extensions,
}

impl Changeset {
    /// Builds and validates a changeset (everything checkable without trees).
    pub fn new(
        parents: Vec<ObjectId>,
        root: ObjectId,
        meta: ObjectId,
        change_id: ChangeId,
        delta: Vec<DeltaEntry>,
    ) -> Result<Changeset> {
        Changeset::with_extensions(parents, root, meta, change_id, delta, Extensions::new())
    }

    /// As [`Changeset::new`], with ignorable extension fields (even keys ≥ 64).
    pub fn with_extensions(
        parents: Vec<ObjectId>,
        root: ObjectId,
        meta: ObjectId,
        change_id: ChangeId,
        delta: Vec<DeltaEntry>,
        extensions: Extensions,
    ) -> Result<Changeset> {
        validate_extensions(&extensions, &[])?;
        let cs = Changeset {
            parents,
            root,
            meta,
            change_id,
            delta,
            extensions,
        };
        cs.validate(&Limits::DEFAULT)?;
        Ok(cs)
    }

    fn validate(&self, limits: &Limits) -> Result<()> {
        Limits::check("parents", limits.parents, self.parents.len() as u64)?;
        let mut seen = BTreeSet::new();
        for p in &self.parents {
            p.expect_kind(ObjectKind::Changeset)?;
            if !seen.insert(p) {
                return Err(BeeError::invalid(
                    Subcode::InvalidParents,
                    format!("parent {p} listed twice"),
                ));
            }
        }
        self.root.expect_kind(ObjectKind::Tree)?;
        self.meta.expect_kind(ObjectKind::Meta)?;
        validate_delta(&self.delta, !self.parents.is_empty(), limits)
    }

    /// The every-repo empty root: no parents, empty tree, [`Meta::empty_root`], the zero
    /// ChangeId and an empty delta. Its id is the same in every repo.
    pub fn empty_root() -> Changeset {
        Changeset {
            parents: Vec::new(),
            root: Tree::empty().id(),
            meta: Meta::empty_root().id(),
            change_id: ChangeId::ZERO,
            delta: Vec::new(),
            extensions: Extensions::new(),
        }
    }

    /// Parents, in order.
    pub fn parents(&self) -> &[ObjectId] {
        &self.parents
    }

    /// The first parent, whose root the delta is relative to.
    pub fn first_parent(&self) -> Option<&ObjectId> {
        self.parents.first()
    }

    /// The root tree id.
    pub fn root(&self) -> &ObjectId {
        &self.root
    }

    /// The meta id.
    pub fn meta(&self) -> &ObjectId {
        &self.meta
    }

    /// The ChangeId.
    pub fn change_id(&self) -> &ChangeId {
        &self.change_id
    }

    /// The delta from the first parent's root, sorted by path.
    pub fn delta(&self) -> &[DeltaEntry] {
        &self.delta
    }

    /// Preserved extension fields.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// The canonical encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut m = top_map(&self.extensions);
        m.insert(1, Value::Array(self.parents.iter().map(id_value).collect()));
        m.insert(2, id_value(&self.root));
        m.insert(3, id_value(&self.meta));
        m.insert(4, Value::Bytes(self.change_id.as_bytes().to_vec()));
        m.insert(
            5,
            Value::Array(self.delta.iter().map(DeltaEntry::to_value).collect()),
        );
        Value::Map(m).encode()
    }

    /// The changeset's id: BLAKE3 over the canonical encoding, kind changeset.
    pub fn id(&self) -> ObjectId {
        hash_id(ObjectKind::Changeset, &self.encode())
    }

    /// Decodes and validates with the default limits.
    pub fn decode(bytes: &[u8]) -> Result<Changeset> {
        Changeset::decode_with_limits(bytes, &Limits::DEFAULT)
    }

    /// Decodes and validates with the given limits.
    pub fn decode_with_limits(bytes: &[u8], limits: &Limits) -> Result<Changeset> {
        let mut f = Fields::top("changeset", bytes, limits)?;
        let raw_parents = f.array(1)?;
        Limits::check("parents", limits.parents, raw_parents.len() as u64)?;
        let parents = raw_parents
            .into_iter()
            .map(|v| match v {
                Value::Bytes(b) => ObjectId::from_bytes(&b),
                _ => Err(field_error("changeset", "parents hold byte strings")),
            })
            .collect::<Result<Vec<_>>>()?;
        let root = f.object_id(2)?;
        let meta = f.object_id(3)?;
        let change_id = f.change_id(4)?;
        let raw_delta = f.array(5)?;
        Limits::check(
            "delta entries",
            limits.delta_entries,
            raw_delta.len() as u64,
        )?;
        let delta = raw_delta
            .into_iter()
            .map(DeltaEntry::from_value)
            .collect::<Result<Vec<_>>>()?;
        let extensions = f.finish_top()?;
        let cs = Changeset {
            parents,
            root,
            meta,
            change_id,
            delta,
            extensions,
        };
        cs.validate(limits)?;
        Ok(cs)
    }

    /// The root tree id of an encoded changeset, read from a prefix of its canonical
    /// encoding (`prefix` is the first bytes of an encoding `total_len` bytes long) without
    /// decoding its delta. The root follows only the format version and the parents (at
    /// most [`Limits::parents`] ids), so a prefix of a few hundred bytes holds it whatever
    /// the delta's size. `Ok(None)` when the prefix ends before the root; then decode the
    /// whole encoding instead. The leading fields are checked as [`Changeset::decode`]
    /// checks them, but the rest is not looked at: use this only on bytes that were
    /// validated when they were stored (a changeset row).
    pub fn root_from_prefix(prefix: &[u8], total_len: usize) -> Result<Option<ObjectId>> {
        match root_of(prefix) {
            Ok(root) => Ok(Some(root)),
            Err(_) if prefix.len() < total_len => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Walks the heads of `{0: format_version, 1: [parents], 2: root, ...}` up to the root.
fn root_of(bytes: &[u8]) -> Result<ObjectId> {
    let mut r = crate::cbor::HeadReader::new(bytes);
    let bad = |what: &str| field_error("changeset", what);
    let (major, entries) = r.head()?;
    if major != 5 || entries < 3 {
        return Err(bad(
            "expected a map with a format version, parents and a root",
        ));
    }
    if r.head()? != (0, 0) || r.head()? != (0, crate::fields::FORMAT_VERSION) {
        return Err(bad("unsupported format_version"));
    }
    if r.head()? != (0, 1) {
        return Err(bad("missing parents"));
    }
    let (major, parents) = r.head()?;
    if major != 4 {
        return Err(bad("parents hold byte strings"));
    }
    for _ in 0..parents {
        let (major, len) = r.head()?;
        if major != 2 {
            return Err(bad("parents hold byte strings"));
        }
        ObjectId::from_bytes(r.take(len)?)?;
    }
    if r.head()? != (0, 2) {
        return Err(bad("missing root"));
    }
    let (major, len) = r.head()?;
    if major != 2 {
        return Err(bad("root is a byte string"));
    }
    ObjectId::from_bytes(r.take(len)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_id;
    use bee_core::{Reason, hex};

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn put(s: &str, content: &[u8]) -> DeltaEntry {
        DeltaEntry::put(p(s), Mode::File, blob_id(content), content.len() as u64)
    }

    fn cs_id(seed: u8) -> ObjectId {
        ObjectId::new(ObjectKind::Changeset, [seed; 32])
    }

    fn sample() -> Changeset {
        Changeset::new(
            vec![cs_id(1), cs_id(2)],
            Tree::empty().id(),
            Meta::empty_root().id(),
            ChangeId::new([3; 16]),
            vec![DeltaEntry::delete(p("a")), put("a/b", b"x"), put("c", b"y")],
        )
        .unwrap()
    }

    #[test]
    fn changeset_round_trip() {
        let c = sample();
        let back = Changeset::decode(&c.encode()).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.id(), c.id());
        assert_eq!(c.id().kind(), ObjectKind::Changeset);
        assert_eq!(c.first_parent(), Some(&cs_id(1)));
        assert_eq!(c.parents().len(), 2);
        assert_eq!(c.root(), &Tree::empty().id());
        assert_eq!(c.meta(), &Meta::empty_root().id());
        assert_eq!(c.change_id(), &ChangeId::new([3; 16]));
        assert_eq!(c.delta().len(), 3);
        assert!(c.extensions().is_empty());
    }

    #[test]
    fn empty_root_changeset() {
        let r = Changeset::empty_root();
        assert!(r.parents().is_empty());
        assert!(r.delta().is_empty());
        assert_eq!(r.change_id(), &ChangeId::ZERO);
        assert_eq!(Changeset::decode(&r.encode()).unwrap(), r);
        assert!(hex::encode(&r.encode()).starts_with("a6000101800258220102"));
    }

    #[test]
    fn delta_rejects_unsorted_and_duplicates() {
        let mk = |delta| {
            Changeset::new(
                vec![cs_id(1)],
                Tree::empty().id(),
                Meta::empty_root().id(),
                ChangeId::ZERO,
                delta,
            )
        };
        assert!(
            mk(vec![put("b", b""), put("a", b"")])
                .unwrap_err()
                .is_invalid(Subcode::InvalidDelta)
        );
        assert!(
            mk(vec![put("a", b""), put("a", b"1")])
                .unwrap_err()
                .is_invalid(Subcode::InvalidDelta)
        );
        // Bytewise path order: "a-b" < "a/x".
        assert!(mk(vec![put("a-b", b""), put("a/x", b"")]).is_ok());
        assert!(mk(vec![put("a/x", b""), put("a-b", b"")]).is_err());
    }

    #[test]
    fn delta_rejects_contradictions() {
        let mk = |delta: Vec<DeltaEntry>| validate_delta(&delta, true, &Limits::DEFAULT);
        assert!(
            mk(vec![put("a", b""), put("a/b", b"")])
                .unwrap_err()
                .is_invalid(Subcode::InvalidDelta)
        );
        assert!(
            mk(vec![
                DeltaEntry::delete(p("a")),
                DeltaEntry::delete(p("a/b/c"))
            ])
            .is_err()
        );
        assert!(
            mk(vec![DeltaEntry::delete(p("a")), put("a/b", b"")]).is_ok(),
            "file -> dir"
        );
        assert!(
            mk(vec![put("a", b""), DeltaEntry::delete(p("a/b"))]).is_ok(),
            "dir -> file"
        );
        assert!(validate_delta(&[DeltaEntry::delete(p("a"))], false, &Limits::DEFAULT).is_err());
        assert!(validate_delta(&[put("a", b"")], false, &Limits::DEFAULT).is_ok());
        let tight = Limits {
            delta_entries: 1,
            ..Limits::DEFAULT
        };
        assert_eq!(
            validate_delta(&[put("a", b""), put("b", b"")], true, &tight)
                .unwrap_err()
                .reason,
            Reason::TooLarge
        );
    }

    #[test]
    fn delta_op_validation() {
        let t = Tree::empty().id();
        assert!(
            DeltaOp::Put {
                mode: Mode::Tree,
                id: t,
                size: 0
            }
            .validate()
            .unwrap_err()
            .is_invalid(Subcode::InvalidMode)
        );
        assert!(
            DeltaOp::Put {
                mode: Mode::File,
                id: t,
                size: 0
            }
            .validate()
            .unwrap_err()
            .is_invalid(Subcode::WrongKind)
        );
        let g = ObjectId::git_foreign(&[1; 20]).unwrap();
        assert!(
            DeltaOp::Put {
                mode: Mode::Gitlink,
                id: g,
                size: 0
            }
            .validate()
            .is_ok()
        );
        assert!(
            DeltaOp::Put {
                mode: Mode::Gitlink,
                id: g,
                size: 1
            }
            .validate()
            .is_err()
        );
        assert!(DeltaOp::Delete.validate().is_ok());
    }

    #[test]
    fn parents_rules() {
        let mk = |parents: Vec<ObjectId>| {
            Changeset::new(
                parents,
                Tree::empty().id(),
                Meta::empty_root().id(),
                ChangeId::ZERO,
                vec![],
            )
        };
        assert!(
            mk(vec![cs_id(1), cs_id(1)])
                .unwrap_err()
                .is_invalid(Subcode::InvalidParents)
        );
        assert!(
            mk(vec![Tree::empty().id()])
                .unwrap_err()
                .is_invalid(Subcode::WrongKind)
        );
        assert!(mk((0..16).map(cs_id).collect()).is_ok());
        assert_eq!(
            mk((0..17).map(cs_id).collect()).unwrap_err().reason,
            Reason::TooLarge
        );
        let wrong_root = Changeset::new(
            vec![],
            Meta::empty_root().id(),
            Meta::empty_root().id(),
            ChangeId::ZERO,
            vec![],
        );
        assert!(wrong_root.unwrap_err().is_invalid(Subcode::WrongKind));
        let wrong_meta = Changeset::new(
            vec![],
            Tree::empty().id(),
            Tree::empty().id(),
            ChangeId::ZERO,
            vec![],
        );
        assert!(wrong_meta.unwrap_err().is_invalid(Subcode::WrongKind));
    }

    fn mutate(c: &Changeset, f: impl FnOnce(&mut BTreeMap<u64, Value>)) -> Vec<u8> {
        let Value::Map(mut map) = Value::decode(&c.encode()).unwrap() else {
            unreachable!()
        };
        f(&mut map);
        Value::Map(map).encode()
    }

    #[test]
    fn changeset_decode_rejections() {
        let c = sample();
        let cases: Vec<(Vec<u8>, Subcode)> = vec![
            (
                mutate(&c, |m| {
                    m.insert(4, Value::Bytes(vec![0; 15]));
                }),
                Subcode::InvalidId,
            ),
            (
                mutate(&c, |m| {
                    m.insert(2, Value::Bytes(vec![0; 34]));
                }),
                Subcode::InvalidId,
            ),
            (
                mutate(&c, |m| {
                    m.insert(1, Value::Array(vec![Value::Uint(1)]));
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&c, |m| {
                    m.remove(&5);
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&c, |m| {
                    m.insert(5, Value::Array(vec![Value::Uint(0)]));
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&c, |m| {
                    m.insert(
                        5,
                        Value::Array(vec![Value::Map(BTreeMap::from([
                            (1, Value::Bytes(b"a//b".to_vec())),
                            (2, Value::Uint(0)),
                        ]))]),
                    );
                }),
                Subcode::InvalidPath,
            ),
            (
                mutate(&c, |m| {
                    m.insert(
                        5,
                        Value::Array(vec![Value::Map(BTreeMap::from([
                            (1, Value::Bytes(b"a".to_vec())),
                            (2, Value::Uint(1)),
                        ]))]),
                    );
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&c, |m| {
                    m.insert(
                        5,
                        Value::Array(vec![Value::Map(BTreeMap::from([
                            (1, Value::Bytes(b"a".to_vec())),
                            (2, Value::Uint(0)),
                            (3, Value::Uint(0)),
                        ]))]),
                    );
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&c, |m| {
                    m.insert(63, Value::Uint(0));
                }),
                Subcode::InvalidField,
            ),
            (
                mutate(&c, |m| {
                    m.insert(101, Value::Uint(0));
                }),
                Subcode::UnknownCriticalExtension,
            ),
            (
                mutate(&c, |m| {
                    m.insert(0, Value::Uint(7));
                }),
                Subcode::UnknownFormatVersion,
            ),
        ];
        for (i, (bytes, sub)) in cases.into_iter().enumerate() {
            let err = Changeset::decode(&bytes).unwrap_err();
            assert!(err.is_invalid(sub), "case {i}: {err}");
        }
        let ext = mutate(&c, |m| {
            m.insert(100, Value::Bytes(vec![1]));
        });
        let back = Changeset::decode(&ext).unwrap();
        assert_eq!(back.extensions().len(), 1);
        assert_eq!(back.encode(), ext);
        assert_ne!(back.id(), c.id());
    }

    #[test]
    fn changeset_with_extensions_validates_keys() {
        let mut ext = Extensions::new();
        ext.insert(64, Value::Uint(1));
        let c = Changeset::with_extensions(
            vec![],
            Tree::empty().id(),
            Meta::empty_root().id(),
            ChangeId::ZERO,
            vec![],
            ext,
        )
        .unwrap();
        assert_eq!(Changeset::decode(&c.encode()).unwrap(), c);
        let mut odd = Extensions::new();
        odd.insert(67, Value::Uint(1));
        assert!(
            Changeset::with_extensions(
                vec![],
                Tree::empty().id(),
                Meta::empty_root().id(),
                ChangeId::ZERO,
                vec![],
                odd
            )
            .is_err()
        );
    }

    #[test]
    fn decode_limits() {
        let c = sample();
        let tight = Limits {
            parents: 1,
            ..Limits::DEFAULT
        };
        assert_eq!(
            Changeset::decode_with_limits(&c.encode(), &tight)
                .unwrap_err()
                .reason,
            Reason::TooLarge
        );
        let tight = Limits {
            delta_entries: 2,
            ..Limits::DEFAULT
        };
        assert_eq!(
            Changeset::decode_with_limits(&c.encode(), &tight)
                .unwrap_err()
                .reason,
            Reason::TooLarge
        );
    }

    #[test]
    fn root_from_prefix_reads_the_root_without_the_delta() {
        // A root-only changeset, the sample (two parents) and one with 16 parents and a
        // large delta: every prefix gives the right root or asks for the whole encoding.
        let many = Changeset::new(
            (1..=16).map(cs_id).collect(),
            blob_tree_root(),
            Meta::empty_root().id(),
            ChangeId::new([4; 16]),
            (0..2_000)
                .map(|i| put(&format!("d/f{i:05}"), format!("{i}").as_bytes()))
                .collect(),
        )
        .unwrap();
        let root_only = Changeset::new(
            vec![],
            Tree::empty().id(),
            Meta::empty_root().id(),
            ChangeId::new([5; 16]),
            vec![],
        )
        .unwrap();
        for c in [root_only, sample(), many] {
            let bytes = c.encode();
            let mut first = None;
            for n in 0..=bytes.len().min(1_000) {
                match Changeset::root_from_prefix(&bytes[..n], bytes.len()).unwrap() {
                    Some(root) => {
                        assert_eq!(&root, c.root());
                        first.get_or_insert(n);
                    }
                    None => assert!(first.is_none(), "a longer prefix lost the root"),
                }
            }
            let first = first.expect("found within 1,000 bytes");
            // Six bytes of heads and keys, then 36 per id (a two-byte head and a 34-byte
            // id) for each parent and the root: 618 bytes with 16 parents.
            assert_eq!(first, 6 + 36 * (c.parents().len() + 1), "{first}");
            assert_eq!(
                Changeset::root_from_prefix(&bytes, bytes.len()).unwrap(),
                Some(*c.root())
            );
        }
        // A complete encoding that is not a changeset is an error, not "read more".
        let tree = Tree::empty().encode();
        assert!(Changeset::root_from_prefix(&tree, tree.len()).is_err());
        let mut bad = sample().encode();
        assert_eq!(&bad[..3], &[0xa6, 0x00, 0x01]); // {0: 1, ...} with six keys
        bad[2] = 0x02; // format_version 2
        assert!(Changeset::root_from_prefix(&bad, bad.len()).is_err());
    }

    fn blob_tree_root() -> ObjectId {
        ObjectId::new(ObjectKind::Tree, [9; 32])
    }
}
