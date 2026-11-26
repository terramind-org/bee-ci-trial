//! git → bee (SPEC §17.4).
//!
//! - A git blob becomes a bee blob with the same bytes.
//! - A git tree becomes a bee tree: names as bytes, modes mapped by type bits (regular
//!   files with the owner exec bit are executables), subtrees recursively, gitlinks as mode 5
//!   with a foreign id. What bee's tree cannot say goes into the tree's git note
//!   ([`crate::note`]): non-canonical mode digits, a non-git entry order, and directories that
//!   contain no files. Each converted tree is rebuilt into git bytes on the spot and compared
//!   with the original, so a tree that would not reproduce is an error, never a silent loss.
//! - A git commit becomes a Meta and a changeset. Meta's `author` and `committer` are the
//!   identity text `Name <email>` (lossy UTF-8), the times come from the identity lines (0
//!   when unparseable), the message is the commit message (lossy UTF-8), and the git
//!   extension (key 64) holds the bytes needed to rebuild the commit exactly
//!   ([`git_extension`]). The changeset's parents are the imported parents (duplicates
//!   dropped), its ChangeId is the importer ChangeId of the commit id (SPEC §3.3), and its
//!   delta is the difference from the first parent's tree. The rebuilt commit is hashed and
//!   compared with the original id.
//!
//! Things bee cannot hold are rejected with the SPEC §12.2 reason the object layer gives:
//! names bee forbids (`.bee`, longer than 255 bytes: `INVALID_NAME`), duplicate names in
//! one tree (`INVALID_TREE`), more than 16 parents or an over-long message (`TOO_LARGE`).

use std::collections::BTreeMap;

use bee_core::{BeeError, Limits, Mode, ObjectId, Reason, Result, Subcode, validate_name};
use bee_object::{
    Changeset, Meta, Timestamp, Tree, TreeEntry, TreeSource, blob_id, diff_trees, import_change_id,
};

use crate::commit::{GitCommit, Ident};
use crate::error::integrity;
use crate::export::{imported_commit, rebuild_git_tree};
use crate::mapping::GitMapping;
use crate::note::{HiddenEntry, TreeNote, needs_order};
use crate::object::{ObjectType, RawObject};
use crate::oid::{GitOid, ObjectFormat};
use crate::tree::{EntryKind, GitTree};

/// Where the importer reads git objects.
pub trait GitObjectSource {
    /// The object with this id (`MISSING_OBJECT` if absent).
    fn git_object(&self, oid: &GitOid) -> Result<RawObject>;
}

impl<F> GitObjectSource for F
where
    F: Fn(&GitOid) -> Result<RawObject>,
{
    fn git_object(&self, oid: &GitOid) -> Result<RawObject> {
        self(oid)
    }
}

/// Everything one imported commit introduced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedCommit {
    /// The git commit id.
    pub oid: GitOid,
    /// The changeset.
    pub changeset: Changeset,
    /// Its meta.
    pub meta: Meta,
    /// Trees first seen in this commit, children before parents.
    pub trees: Vec<Tree>,
    /// Blobs first seen in this commit: `(bee id, bytes)`.
    pub blobs: Vec<(ObjectId, Vec<u8>)>,
}

#[derive(Clone, Debug)]
struct TreeInfo {
    /// The bee tree (possibly empty, with a note, when the git tree has no files).
    id: ObjectId,
    /// True when the bee tree has entries.
    has_entries: bool,
    /// The note, needed when this tree is a hidden subtree of another.
    note: TreeNote,
}

/// The `(blob id, size)` of a converted git blob.
type BlobInfo = (ObjectId, u64);

/// Converts git objects to bee objects, remembering what it has converted.
#[derive(Debug)]
pub struct Importer {
    format: ObjectFormat,
    creator: String,
    limits: Limits,
    mapping: GitMapping,
    trees: BTreeMap<ObjectId, Tree>,
    tree_info: BTreeMap<GitOid, TreeInfo>,
    blobs: BTreeMap<GitOid, BlobInfo>,
    roots: BTreeMap<ObjectId, ObjectId>,
}

struct Out {
    trees: Vec<Tree>,
    blobs: Vec<(ObjectId, Vec<u8>)>,
}

impl TreeSource for Importer {
    fn tree(&self, id: &ObjectId) -> Result<Tree> {
        self.trees
            .get(id)
            .cloned()
            .ok_or_else(|| BeeError::new(Reason::MissingObject, format!("tree {id} not imported")))
    }
}

fn wrong_type(oid: &GitOid, want: ObjectType, got: ObjectType) -> BeeError {
    BeeError::invalid(
        Subcode::WrongKind,
        format!(
            "git object {oid} is a {}, expected a {}",
            got.name(),
            want.name()
        ),
    )
}

/// The Meta git extension for a commit: the header bytes after the tree line, then — when the
/// commit has a message — the empty line that ends the headers, followed by the raw message
/// only if it is not valid UTF-8 (a UTF-8 message is Meta's `message` and is not repeated).
/// [`crate::export::imported_commit`] inverts this.
pub fn git_extension(commit: &GitCommit) -> Vec<u8> {
    let mut ext = commit.raw_headers().to_vec();
    if let Some(m) = commit.message() {
        ext.push(b'\n');
        if std::str::from_utf8(m).is_err() {
            ext.extend_from_slice(m);
        }
    }
    ext
}

fn ident_text(id: &Ident) -> String {
    let mut t = String::from_utf8_lossy(&id.name).into_owned();
    t.push_str(" <");
    t.push_str(&String::from_utf8_lossy(&id.email));
    t.push('>');
    t
}

fn ident_parts(commit: &GitCommit, header: &[u8]) -> (String, Timestamp) {
    match commit.header(header) {
        Some(v) => match Ident::parse(&v) {
            Some(id) => (
                ident_text(&id),
                Timestamp::new(id.seconds, id.tz_offset_minutes),
            ),
            None => (
                String::from_utf8_lossy(&v).into_owned(),
                Timestamp::new(0, 0),
            ),
        },
        None => (String::new(), Timestamp::new(0, 0)),
    }
}

impl Importer {
    /// An importer for a repository in `format`. `creator` is the principal recorded as Meta
    /// `creator` (SPEC §4.3; the server's import principal).
    pub fn new(format: ObjectFormat, creator: impl Into<String>) -> Importer {
        Importer {
            format,
            creator: creator.into(),
            limits: Limits::DEFAULT,
            mapping: GitMapping::new(),
            trees: BTreeMap::new(),
            tree_info: BTreeMap::new(),
            blobs: BTreeMap::new(),
            roots: BTreeMap::new(),
        }
    }

    /// Uses repo-specific limits (blob size) instead of the defaults.
    pub fn with_limits(mut self, limits: Limits) -> Importer {
        self.limits = limits;
        self
    }

    /// The object format.
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    /// The mapping built so far (commits, trees and blobs).
    pub fn mapping(&self) -> &GitMapping {
        &self.mapping
    }

    /// Records a commit imported earlier, so its children can be imported incrementally: the
    /// git id, the changeset id and its root tree (whose subtrees must be supplied with
    /// [`Importer::add_known_tree`] for the first child's delta to be computed).
    pub fn add_known_commit(
        &mut self,
        oid: GitOid,
        changeset: ObjectId,
        root: ObjectId,
    ) -> Result<()> {
        self.mapping.insert(oid, changeset)?;
        self.roots.insert(changeset, root);
        Ok(())
    }

    /// Makes a previously stored bee tree available for delta computation.
    pub fn add_known_tree(&mut self, tree: Tree) {
        self.trees.insert(tree.id(), tree);
    }

    fn import_blob(
        &mut self,
        src: &dyn GitObjectSource,
        oid: &GitOid,
        out: &mut Out,
    ) -> Result<BlobInfo> {
        if let Some(info) = self.blobs.get(oid) {
            return Ok(*info);
        }
        let raw = src.git_object(oid)?;
        if raw.kind != ObjectType::Blob {
            return Err(wrong_type(oid, ObjectType::Blob, raw.kind));
        }
        self.limits.check_blob_size(raw.data.len() as u64)?;
        let id = blob_id(&raw.data);
        let info = (id, raw.data.len() as u64);
        self.mapping.insert(*oid, id)?;
        self.blobs.insert(*oid, info);
        out.blobs.push((id, raw.data));
        Ok(info)
    }

    fn import_tree(
        &mut self,
        src: &dyn GitObjectSource,
        oid: &GitOid,
        depth: usize,
        out: &mut Out,
    ) -> Result<TreeInfo> {
        if let Some(info) = self.tree_info.get(oid) {
            return Ok(info.clone());
        }
        if depth as u64 > self.limits.path_depth {
            return Err(BeeError::invalid(
                Subcode::InvalidPath,
                format!(
                    "git tree {oid} nests deeper than {} levels",
                    self.limits.path_depth
                ),
            ));
        }
        let raw = src.git_object(oid)?;
        if raw.kind != ObjectType::Tree {
            return Err(wrong_type(oid, ObjectType::Tree, raw.kind));
        }
        let git = GitTree::parse(&raw.data, self.format)?;
        let mut note = TreeNote::default();
        let mut entries = Vec::with_capacity(git.entries.len());
        let mut child_oids: BTreeMap<Vec<u8>, GitOid> = BTreeMap::new();
        for e in &git.entries {
            let kind = e.kind()?;
            validate_name(&e.name)?;
            if child_oids.insert(e.name.clone(), e.oid).is_some() {
                return Err(BeeError::invalid(
                    Subcode::InvalidTree,
                    format!(
                        "git tree {oid} has two entries named {:?}",
                        String::from_utf8_lossy(&e.name)
                    ),
                ));
            }
            let canonical = kind.canonical_mode() == e.mode.as_slice();
            match kind {
                EntryKind::Tree => {
                    let sub = self.import_tree(src, &e.oid, depth + 1, out)?;
                    if !sub.has_entries {
                        note.hidden.push(HiddenEntry {
                            name: e.name.clone(),
                            mode: e.mode.clone(),
                            note: sub.note,
                        });
                        continue;
                    }
                    entries.push(TreeEntry::tree(e.name.clone(), sub.id));
                }
                EntryKind::Gitlink => {
                    entries.push(TreeEntry::gitlink(e.name.clone(), e.oid.to_foreign()));
                }
                EntryKind::File | EntryKind::Executable | EntryKind::Symlink => {
                    let (id, size) = self.import_blob(src, &e.oid, out)?;
                    entries.push(TreeEntry::blob(e.name.clone(), kind.to_bee(), id, size));
                }
            }
            if !canonical {
                note.modes.insert(e.name.clone(), e.mode.clone());
            }
        }
        note.hidden.sort_by(|a, b| a.name.cmp(&b.name));
        if needs_order(&git.entries) {
            note.order = Some(git.entries.iter().map(|e| e.name.clone()).collect());
        }
        let has_entries = !entries.is_empty();
        let tree = note.build_tree(entries)?;
        // Prove the conversion is lossless before anyone relies on it.
        let format = self.format;
        let mut child = |e: &TreeEntry| -> Result<GitOid> {
            child_oids
                .get(&e.name)
                .copied()
                .ok_or_else(|| integrity("lost a tree entry"))
        };
        let rebuilt = rebuild_git_tree(format, &tree, &note, &mut child)?;
        if rebuilt.serialize() != raw.data {
            return Err(integrity(format!(
                "git tree {oid} does not survive conversion to a bee tree"
            )));
        }
        let id = tree.id();
        let info = TreeInfo {
            id,
            has_entries,
            note,
        };
        if has_entries {
            self.mapping.insert(*oid, id)?;
        }
        if let std::collections::btree_map::Entry::Vacant(slot) = self.trees.entry(id) {
            slot.insert(tree.clone());
            out.trees.push(tree);
        }
        self.tree_info.insert(*oid, info.clone());
        Ok(info)
    }

    /// Imports one commit whose parents are already imported (`MISSING_OBJECT` naming the
    /// first parent that is not). Importing an already imported commit is an error-free
    /// no-op that returns its objects again with empty `trees` and `blobs`.
    pub fn import_commit(
        &mut self,
        src: &dyn GitObjectSource,
        oid: &GitOid,
    ) -> Result<ImportedCommit> {
        let raw = src.git_object(oid)?;
        if raw.kind != ObjectType::Commit {
            return Err(wrong_type(oid, ObjectType::Commit, raw.kind));
        }
        if raw.oid(self.format)? != *oid {
            return Err(integrity(format!("git object bytes do not hash to {oid}")));
        }
        let commit = GitCommit::parse(&raw.data, self.format)?;
        let mut parents: Vec<ObjectId> = Vec::new();
        for p in commit.parents() {
            let id = self.mapping.bee(&p).ok_or_else(|| {
                BeeError::new(
                    Reason::MissingObject,
                    format!("parent commit {p} of {oid} is not imported yet"),
                )
                .with_payload("oid", p.to_hex())
            })?;
            if !parents.contains(&id) {
                parents.push(id);
            }
        }
        let mut out = Out {
            trees: Vec::new(),
            blobs: Vec::new(),
        };
        let root_info = self.import_tree(src, &commit.tree(), 0, &mut out)?;
        let root = root_info.id;
        if !root_info.has_entries {
            // An all-empty root tree is still a valid root; map it so export finds it.
            self.mapping.insert(commit.tree(), root)?;
        }
        let (author, author_time) = ident_parts(&commit, b"author");
        let (committer, committer_time) = ident_parts(&commit, b"committer");
        let has_committer = commit.header(b"committer").is_some();
        let mut meta = Meta::new(
            author,
            author_time,
            self.creator.clone(),
            String::from_utf8_lossy(commit.message().unwrap_or_default()).into_owned(),
        );
        if has_committer {
            meta.committer = Some(committer);
            meta.committer_time = Some(committer_time);
        }
        meta.git = Some(git_extension(&commit));
        meta.validate(&self.limits)?;
        let rebuilt = imported_commit(
            commit.tree(),
            meta.git.as_deref().unwrap_or_default(),
            &meta.message,
        );
        if rebuilt.serialize() != raw.data {
            return Err(integrity(format!(
                "git commit {oid} does not survive conversion to a meta"
            )));
        }
        let first_root = parents.first().map(|p| {
            self.roots
                .get(p)
                .copied()
                .ok_or_else(|| integrity(format!("root of imported changeset {p} unknown")))
        });
        let first_root = first_root.transpose()?;
        let delta = diff_trees(self, first_root.as_ref(), Some(&root))?;
        let changeset = Changeset::new(
            parents,
            root,
            meta.id(),
            import_change_id(oid.as_bytes()),
            delta,
        )?;
        let id = changeset.id();
        self.mapping.insert(*oid, id)?;
        self.roots.insert(id, root);
        Ok(ImportedCommit {
            oid: *oid,
            changeset,
            meta,
            trees: out.trees,
            blobs: out.blobs,
        })
    }

    /// Imports `tips` and every ancestor not yet imported, parents before children, handing
    /// each commit to `sink` as soon as it is converted. Returns the number imported.
    pub fn import_history(
        &mut self,
        src: &dyn GitObjectSource,
        tips: &[GitOid],
        sink: &mut dyn FnMut(ImportedCommit) -> Result<()>,
    ) -> Result<usize> {
        let mut count = 0;
        let mut stack: Vec<(GitOid, bool)> = tips.iter().rev().map(|t| (*t, false)).collect();
        while let Some((oid, ready)) = stack.pop() {
            if self.mapping.bee(&oid).is_some() {
                continue;
            }
            if ready {
                sink(self.import_commit(src, &oid)?)?;
                count += 1;
                continue;
            }
            let raw = src.git_object(&oid)?;
            if raw.kind != ObjectType::Commit {
                return Err(wrong_type(&oid, ObjectType::Commit, raw.kind));
            }
            let commit = GitCommit::parse(&raw.data, self.format)?;
            stack.push((oid, true));
            for p in commit.parents().iter().rev() {
                if self.mapping.bee(p).is_none() {
                    stack.push((*p, false));
                }
            }
        }
        Ok(count)
    }

    /// The bee mode a git mode maps to (exposed for tools that inspect trees).
    pub fn bee_mode(mode: &[u8]) -> Result<Mode> {
        EntryKind::from_mode(mode).map(EntryKind::to_bee)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{BeeObjectSource, Exporter, Representation};
    use crate::tree::GitTreeEntry;
    use std::cell::RefCell;

    /// An in-memory git object store keyed by id.
    #[derive(Default)]
    struct GitStore {
        objects: BTreeMap<GitOid, RawObject>,
    }

    impl GitStore {
        fn put(&mut self, kind: ObjectType, data: Vec<u8>) -> GitOid {
            let raw = RawObject::new(kind, data);
            let oid = raw.oid(ObjectFormat::Sha1).unwrap();
            self.objects.insert(oid, raw);
            oid
        }
        fn get(&self, oid: &GitOid) -> Result<RawObject> {
            self.objects
                .get(oid)
                .cloned()
                .ok_or_else(|| BeeError::new(Reason::MissingObject, oid.to_hex()))
        }
    }

    /// An in-memory bee object store.
    #[derive(Default)]
    struct BeeStore {
        blobs: BTreeMap<ObjectId, Vec<u8>>,
        trees: BTreeMap<ObjectId, Tree>,
        metas: BTreeMap<ObjectId, Meta>,
        changesets: BTreeMap<ObjectId, Changeset>,
    }

    impl BeeStore {
        fn add(&mut self, c: ImportedCommit) {
            self.blobs.extend(c.blobs);
            for t in c.trees {
                self.trees.insert(t.id(), t);
            }
            self.metas.insert(c.meta.id(), c.meta);
            self.changesets.insert(c.changeset.id(), c.changeset);
        }
    }

    fn miss(id: &ObjectId) -> BeeError {
        BeeError::new(Reason::MissingObject, id.to_text())
    }

    impl BeeObjectSource for BeeStore {
        fn blob(&self, id: &ObjectId) -> Result<Vec<u8>> {
            self.blobs.get(id).cloned().ok_or_else(|| miss(id))
        }
        fn tree(&self, id: &ObjectId) -> Result<Tree> {
            self.trees.get(id).cloned().ok_or_else(|| miss(id))
        }
        fn meta(&self, id: &ObjectId) -> Result<Meta> {
            self.metas.get(id).cloned().ok_or_else(|| miss(id))
        }
        fn changeset(&self, id: &ObjectId) -> Result<Changeset> {
            self.changesets.get(id).cloned().ok_or_else(|| miss(id))
        }
    }

    fn tree_bytes(entries: &[(&[u8], &[u8], GitOid)]) -> Vec<u8> {
        GitTree {
            entries: entries
                .iter()
                .map(|(m, n, o)| GitTreeEntry {
                    mode: m.to_vec(),
                    name: n.to_vec(),
                    oid: *o,
                })
                .collect(),
        }
        .serialize()
    }

    #[test]
    fn odd_trees_and_commits_round_trip() {
        let mut g = GitStore::default();
        let a = g.put(ObjectType::Blob, b"a\n".to_vec());
        let b = g.put(ObjectType::Blob, vec![0, 159, 146, 150]);
        let empty = g.put(ObjectType::Tree, Vec::new());
        let only_empty = g.put(ObjectType::Tree, tree_bytes(&[(b"40000", b"e", empty)]));
        let sub = g.put(ObjectType::Tree, tree_bytes(&[(b"100755", b"run", a)]));
        let sub_link = GitOid::Sha1([0x42; 20]);
        // Unsorted, a non-canonical file mode, a zero-padded tree mode, an empty directory
        // holding an empty directory, a gitlink.
        let root = g.put(
            ObjectType::Tree,
            tree_bytes(&[
                (b"100664", b"z", a),
                (b"040000", b"dir", sub),
                (b"100644", b"\xffbin", b),
                (b"40000", b"hollow", only_empty),
                (b"160000", b"mod", sub_link),
                (b"120000", b"link", a),
            ]),
        );
        let c1 = g.put(
            ObjectType::Commit,
            format!(
                "tree {root}\nauthor A <a@x> 1 +0000\ncommitter C <c@x> 2 -0130\n\
                 encoding UTF-8\nx-extra yes\n\nété\n"
            )
            .into_bytes(),
        );
        // A non-UTF-8 message, duplicate parents and a multi-line signature header.
        let mut body = format!(
            "tree {root}\nparent {c1}\nparent {c1}\nauthor A <a@x> 3 +0000\n\
             committer C <c@x> 4 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n sig\n \
             -----END PGP SIGNATURE-----\n\n"
        )
        .into_bytes();
        body.extend_from_slice(b"caf\xe9\n");
        let c2 = g.put(ObjectType::Commit, body);
        let c3 = g.put(
            ObjectType::Commit,
            format!("tree {empty}\nparent {c2}\nauthor <> 0 +0000").into_bytes(),
        );

        let src = |o: &GitOid| g.get(o);
        let mut imp = Importer::new(ObjectFormat::Sha1, "importer");
        let mut store = BeeStore::default();
        let mut seen = Vec::new();
        let n = imp
            .import_history(&src, &[c3], &mut |c| {
                seen.push(c.oid);
                store.add(c);
                Ok(())
            })
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(seen, vec![c1, c2, c3]);
        let cs2 = imp.mapping().bee(&c2).unwrap();
        assert_eq!(store.changesets[&cs2].parents().len(), 1);

        // Export from a fresh exporter reproduces every id, and every object byte for byte.
        let emitted = RefCell::new(BTreeMap::new());
        let mut exp = Exporter::new(ObjectFormat::Sha1);
        let cs3 = imp.mapping().bee(&c3).unwrap();
        let commits = exp
            .export_history(&store, &[cs3], &mut |oid, raw| {
                emitted.borrow_mut().insert(oid, raw);
                Ok(())
            })
            .unwrap();
        let oids: Vec<GitOid> = commits.iter().map(|c| c.oid).collect();
        assert_eq!(oids, vec![c1, c2, c3]);
        assert!(
            commits
                .iter()
                .all(|c| c.representation == Representation::Imported)
        );
        for (oid, raw) in emitted.borrow().iter() {
            assert_eq!(g.objects.get(oid), Some(raw), "{oid}");
        }
        for oid in g.objects.keys().filter(|o| **o != sub_link) {
            assert!(emitted.borrow().contains_key(oid), "missing {oid}");
            assert_eq!(
                exp.object(&store, oid).unwrap().as_ref(),
                g.objects.get(oid)
            );
        }
        assert_eq!(exp.object(&store, &GitOid::Sha1([9; 20])).unwrap(), None);
        // Re-import is a no-op.
        assert_eq!(imp.import_history(&src, &[c3], &mut |_| Ok(())).unwrap(), 0);
    }

    #[test]
    fn rejects_what_bee_cannot_hold() {
        let mut g = GitStore::default();
        let a = g.put(ObjectType::Blob, b"a".to_vec());
        let reserved = g.put(ObjectType::Tree, tree_bytes(&[(b"100644", b".bee", a)]));
        let dup = g.put(
            ObjectType::Tree,
            tree_bytes(&[(b"100644", b"x", a), (b"100644", b"x", a)]),
        );
        let bad_mode = g.put(ObjectType::Tree, tree_bytes(&[(b"100", b"x", a)]));
        let not_tree = g.put(ObjectType::Tree, tree_bytes(&[(b"40000", b"x", a)]));
        let src = |o: &GitOid| g.get(o);
        for (t, sub) in [
            (reserved, Subcode::InvalidName),
            (dup, Subcode::InvalidTree),
            (bad_mode, Subcode::InvalidMode),
            (not_tree, Subcode::WrongKind),
        ] {
            let mut imp = Importer::new(ObjectFormat::Sha1, "");
            let commit = RawObject::new(
                ObjectType::Commit,
                format!("tree {t}\nauthor a <b> 1 +0000\n\nm").into_bytes(),
            );
            let coid = commit.oid(ObjectFormat::Sha1).unwrap();
            let src2 = |o: &GitOid| {
                if *o == coid {
                    Ok(commit.clone())
                } else {
                    src(o)
                }
            };
            let err = imp.import_commit(&src2, &coid).unwrap_err();
            assert!(err.is_invalid(sub), "{err:?}");
        }
        // Missing parent.
        let orphan = RawObject::new(
            ObjectType::Commit,
            format!("tree {}\nparent {a}\n\n", ObjectFormat::Sha1.empty_tree()).into_bytes(),
        );
        let ooid = orphan.oid(ObjectFormat::Sha1).unwrap();
        let mut imp = Importer::new(ObjectFormat::Sha1, "");
        let err = imp
            .import_commit(&|_: &GitOid| Ok(orphan.clone()), &ooid)
            .unwrap_err();
        assert_eq!(err.reason, Reason::MissingObject);
        assert_eq!(Importer::bee_mode(b"100755").unwrap(), Mode::Executable);
    }

    #[test]
    fn rebased_imports_get_new_history() {
        let mut g = GitStore::default();
        let a = g.put(ObjectType::Blob, b"a".to_vec());
        let t1 = g.put(ObjectType::Tree, tree_bytes(&[(b"100644", b"f", a)]));
        let c1 = g.put(
            ObjectType::Commit,
            format!("tree {t1}\nauthor A <a@x> 1 +0000\ncommitter A <a@x> 1 +0000\n\nm\n")
                .into_bytes(),
        );
        let src = |o: &GitOid| g.get(o);
        let mut imp = Importer::new(ObjectFormat::Sha1, "p");
        let mut store = BeeStore::default();
        let ic = imp.import_commit(&src, &c1).unwrap();
        let meta = ic.meta.clone();
        let tree = ic.changeset.root().to_owned();
        store.add(ic);
        // Same meta and ChangeId, but a different (empty) tree: not the original commit.
        let empty_root = Tree::empty();
        store.trees.insert(empty_root.id(), empty_root.clone());
        let moved = Changeset::new(
            vec![],
            empty_root.id(),
            meta.id(),
            import_change_id(c1.as_bytes()),
            vec![],
        )
        .unwrap();
        store.changesets.insert(moved.id(), moved.clone());
        let mut exp = Exporter::new(ObjectFormat::Sha1);
        let out = exp
            .export_changeset(&store, &moved.id(), &mut |_, _| Ok(()))
            .unwrap();
        assert_eq!(out.representation, Representation::NewHistory);
        assert_ne!(out.oid, c1);
        let again = exp
            .export_changeset(&store, &moved.id(), &mut |_, _| Ok(()))
            .unwrap();
        assert_eq!(again.representation, Representation::Known);
        let _ = tree;
    }
}
