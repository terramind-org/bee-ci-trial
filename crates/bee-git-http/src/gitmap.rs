//! Git ids and git bytes of bee objects, read through the stores (SPEC §17.4).
//!
//! The representation rules are the ones `bee_git::Exporter` implements (the reference); this
//! module applies them over the async stores so a fetch reads only what it needs:
//!
//! - an object whose id is already recorded in the repo's `git_mapping` rows (every imported
//!   commit, tree and blob, and everything the gateway has served or received before) costs one
//!   row read;
//! - anything else is rebuilt (a blob is hashed, a tree rebuilt from its children's git ids and
//!   its git note, a changeset turned into its imported or new-history commit) and the new ids
//!   are written back as `git_mapping` rows, so the deterministic git representation of new
//!   history is derived once and cached (SPEC §8).
//!
//! The differential tests in this module check the rules against `bee_git::Exporter`.

use std::collections::{BTreeMap, BTreeSet};

use bee_core::{Mode, ObjectId, ObjectKind, Reason};
use bee_git::export::{imported_commit, new_history_commit};
use bee_git::tree::{EntryKind, GitTree, GitTreeEntry};
use bee_git::{GitCommit, GitOid, ObjectFormat, ObjectType, RawObject, TreeNote};
use bee_history::{BlobContent, History};
use bee_object::{Changeset, Meta, Tree, TreeEntry, import_change_id};
use bee_store::DerivedRow;

use crate::error::{GitError, GitResult};

/// Rebuilds the git tree of a bee tree from its children's git ids (keyed by entry name),
/// applying the tree's git note: non-canonical modes, hidden empty directories and a recorded
/// entry order.
pub fn rebuild_git_tree(
    format: ObjectFormat,
    tree: &Tree,
    note: &TreeNote,
    child: &dyn Fn(&TreeEntry) -> GitResult<GitOid>,
) -> GitResult<GitTree> {
    note.check_against(tree)?;
    let mut entries = Vec::with_capacity(tree.len() + note.hidden.len());
    for e in tree.entries() {
        let mode = match note.modes.get(&e.name) {
            Some(m) => m.clone(),
            None => EntryKind::from_bee(e.mode).canonical_mode().to_vec(),
        };
        entries.push(GitTreeEntry {
            mode,
            name: e.name.clone(),
            oid: child(e)?,
        });
    }
    for h in &note.hidden {
        let sub = h.note.hidden_git_tree(format)?;
        entries.push(GitTreeEntry {
            mode: h.mode.clone(),
            name: h.name.clone(),
            oid: format.hash_object(ObjectType::Tree, &sub.serialize())?,
        });
    }
    Ok(note.arrange(entries)?)
}

/// Every git tree object a note's hidden (empty) directories need.
pub fn hidden_objects(
    format: ObjectFormat,
    note: &TreeNote,
) -> GitResult<Vec<(GitOid, RawObject)>> {
    let mut out = Vec::new();
    for h in &note.hidden {
        out.extend(hidden_objects(format, &h.note)?);
        let t = h.note.hidden_git_tree(format)?;
        let raw = RawObject::new(ObjectType::Tree, t.serialize());
        out.push((raw.oid(format)?, raw));
    }
    Ok(out)
}

/// The leading `parent` lines of a commit, duplicates dropped; `None` if one does not parse.
fn leading_parents(format: ObjectFormat, c: &GitCommit) -> Option<Vec<GitOid>> {
    let mut out: Vec<GitOid> = Vec::new();
    for (name, value) in c.headers() {
        if name != b"parent" {
            break;
        }
        let p = format.parse_hex(&value).ok()?;
        if !out.contains(&p) {
            out.push(p);
        }
    }
    Some(out)
}

/// The git commit of a changeset: the imported original when it provably is one (its id gives
/// back the changeset's ChangeId under the importer rule and its parents are the parents' git
/// ids), else the deterministic new-history commit.
pub fn commit_for(
    format: ObjectFormat,
    cs: &Changeset,
    meta: &Meta,
    parents: &[GitOid],
    root: GitOid,
) -> GitResult<GitCommit> {
    if let Some(ext) = &meta.git {
        let c = imported_commit(root, ext, &meta.message);
        let oid = format.hash_object(ObjectType::Commit, &c.serialize())?;
        if import_change_id(oid.as_bytes()) == *cs.change_id()
            && leading_parents(format, &c).as_deref() == Some(parents)
        {
            return Ok(c);
        }
    }
    Ok(new_history_commit(root, parents, meta, cs.change_id()))
}

/// The two `git_mapping` rows of a pair.
pub fn mapping_rows(bee: ObjectId, git: &GitOid) -> Vec<DerivedRow> {
    bee_import::git_mapping_rows(bee, git).to_vec()
}

/// A git tree object with the hidden trees its note implies.
pub struct TreeObject {
    /// The tree's git id.
    pub oid: GitOid,
    /// The tree.
    pub raw: RawObject,
    /// Hidden empty-directory trees under it (recursively), with their ids.
    pub hidden: Vec<(GitOid, RawObject)>,
}

/// Git ids of one repo's bee objects, memoized for a request and persisted as
/// `git_mapping` rows by [`GitView::flush`].
pub struct GitView<'a> {
    history: &'a History,
    format: ObjectFormat,
    git: BTreeMap<ObjectId, GitOid>,
    bee: BTreeMap<GitOid, ObjectId>,
    pending: Vec<DerivedRow>,
}

fn redacted(id: &ObjectId) -> GitError {
    GitError::new(
        Reason::Redacted,
        format!("{id} is redacted and cannot be served over git"),
    )
}

impl<'a> GitView<'a> {
    /// A view over a repo's history in `format`.
    pub fn new(history: &'a History, format: ObjectFormat) -> GitView<'a> {
        GitView {
            history,
            format,
            git: BTreeMap::new(),
            bee: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    /// The object format.
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    /// The history.
    pub fn history(&self) -> &'a History {
        self.history
    }

    /// Records a pair known to the caller. `persist` queues its `git_mapping` rows.
    pub fn remember(&mut self, bee: ObjectId, git: GitOid, persist: bool) {
        if persist && !self.git.contains_key(&bee) {
            self.pending.extend(mapping_rows(bee, &git));
        }
        self.git.insert(bee, git);
        self.bee.entry(git).or_insert(bee);
    }

    /// Writes the rows for ids derived by this view (insert-or-ignore).
    pub async fn flush(&mut self) -> GitResult<()> {
        let rows = std::mem::take(&mut self.pending);
        self.history.put_derived(rows).await?;
        Ok(())
    }

    /// The bee object a git id names in this repo, if any.
    pub async fn bee_of(&mut self, oid: &GitOid) -> GitResult<Option<ObjectId>> {
        if let Some(id) = self.bee.get(oid) {
            return Ok(Some(*id));
        }
        let found =
            bee_import::lookup_bee_id(self.history.meta(), self.history.repo(), oid).await?;
        if let Some(id) = found {
            self.bee.insert(*oid, id);
            self.git.entry(id).or_insert(*oid);
        }
        Ok(found)
    }

    /// The recorded git id of a bee object, without deriving it.
    pub async fn known(&mut self, id: &ObjectId) -> GitResult<Option<GitOid>> {
        if let Some(g) = self.git.get(id) {
            return Ok(Some(*g));
        }
        let found = bee_import::lookup_git_id(self.history.meta(), self.history.repo(), id).await?;
        if let Some(g) = found {
            if g.format() != self.format {
                return Err(GitError::new(
                    Reason::Integrity,
                    format!("git_mapping row of {id} is not in the repo's object format"),
                ));
            }
            self.git.insert(*id, g);
            self.bee.entry(g).or_insert(*id);
        }
        Ok(found)
    }

    /// The git id of a changeset, tree or blob.
    pub async fn git_of(&mut self, id: &ObjectId) -> GitResult<GitOid> {
        match id.kind() {
            ObjectKind::Blob => self.blob_oid(id).await,
            ObjectKind::Tree => self.tree_oid(id).await,
            ObjectKind::Changeset => self.commit_oid(id).await,
            other => Err(GitError::invalid(format!(
                "{id} is a {other:?}; only changesets, trees and blobs have git ids"
            ))),
        }
    }

    /// Blob bytes.
    pub async fn blob_bytes(&self, id: &ObjectId) -> GitResult<Vec<u8>> {
        match self.history.blob(id).await? {
            BlobContent::Bytes(b) => Ok(b),
            BlobContent::Redacted => Err(redacted(id)),
        }
    }

    /// The git id of a blob.
    pub async fn blob_oid(&mut self, id: &ObjectId) -> GitResult<GitOid> {
        if let Some(g) = self.known(id).await? {
            return Ok(g);
        }
        let bytes = self.blob_bytes(id).await?;
        let oid = self.format.hash_object(ObjectType::Blob, &bytes)?;
        self.remember(*id, oid, true);
        Ok(oid)
    }

    /// The git id of a tree, rebuilding only the subtrees whose ids are not recorded.
    pub async fn tree_oid(&mut self, id: &ObjectId) -> GitResult<GitOid> {
        if let Some(g) = self.known(id).await? {
            return Ok(g);
        }
        // Post-order over the unrecorded subtrees, iteratively.
        let mut stack: Vec<(ObjectId, bool)> = vec![(*id, false)];
        while let Some((t, ready)) = stack.pop() {
            if ready {
                if self.git.contains_key(&t) {
                    continue;
                }
                let obj = self.tree_object(&t).await?;
                self.remember(t, obj.oid, true);
                continue;
            }
            if self.known(&t).await?.is_some() {
                continue;
            }
            stack.push((t, true));
            let tree = self.history.tree(&t).await?;
            for e in tree.entries() {
                if e.mode == Mode::Tree && !self.git.contains_key(&e.id) {
                    stack.push((e.id, false));
                }
            }
        }
        self.git
            .get(id)
            .copied()
            .ok_or_else(|| GitError::internal(format!("tree {id} was not exported")))
    }

    /// The git tree object of a bee tree (children's ids derived as needed).
    pub async fn tree_object(&mut self, id: &ObjectId) -> GitResult<TreeObject> {
        let tree = self.history.tree(id).await?;
        let note = TreeNote::from_tree(&tree)?;
        let mut child: BTreeMap<Vec<u8>, GitOid> = BTreeMap::new();
        for e in tree.entries() {
            let oid = match e.mode {
                Mode::Tree => Box::pin(self.tree_oid(&e.id)).await?,
                Mode::Gitlink => GitOid::from_foreign(&e.id, self.format)?,
                _ => self.blob_oid(&e.id).await?,
            };
            child.insert(e.name.clone(), oid);
        }
        let git = rebuild_git_tree(self.format, &tree, &note, &|e| {
            child
                .get(&e.name)
                .copied()
                .ok_or_else(|| GitError::internal("lost a tree entry"))
        })?;
        let raw = RawObject::new(ObjectType::Tree, git.serialize());
        let oid = raw.oid(self.format)?;
        Ok(TreeObject {
            oid,
            raw,
            hidden: hidden_objects(self.format, &note)?,
        })
    }

    /// The git id of a changeset, deriving unrecorded ancestors parents first.
    pub async fn commit_oid(&mut self, id: &ObjectId) -> GitResult<GitOid> {
        if let Some(g) = self.known(id).await? {
            return Ok(g);
        }
        let mut stack: Vec<(ObjectId, bool)> = vec![(*id, false)];
        let mut expanded: BTreeSet<ObjectId> = BTreeSet::new();
        while let Some((c, ready)) = stack.pop() {
            if ready {
                if self.git.contains_key(&c) {
                    continue;
                }
                let (oid, _) = self.commit_object(&c).await?;
                self.remember(c, oid, true);
                continue;
            }
            if self.known(&c).await?.is_some() || !expanded.insert(c) {
                continue;
            }
            stack.push((c, true));
            let cs = self.history.changeset(&c).await?;
            for p in cs.parents().iter().rev() {
                if !self.git.contains_key(p) {
                    stack.push((*p, false));
                }
            }
        }
        self.git
            .get(id)
            .copied()
            .ok_or_else(|| GitError::internal(format!("changeset {id} was not exported")))
    }

    /// The git commit object of a changeset whose parents' ids are recorded or derivable.
    pub async fn commit_object(&mut self, id: &ObjectId) -> GitResult<(GitOid, RawObject)> {
        let cs = self.history.changeset(id).await?;
        let mut parents = Vec::with_capacity(cs.parents().len());
        for p in cs.parents() {
            parents.push(Box::pin(self.commit_oid(p)).await?);
        }
        let root = self.tree_oid(cs.root()).await?;
        let meta = self
            .history
            .meta_object(cs.meta())
            .await?
            .ok_or_else(|| redacted(cs.meta()))?;
        let commit = commit_for(self.format, &cs, &meta, &parents, root)?;
        let raw = RawObject::new(ObjectType::Commit, commit.serialize());
        Ok((raw.oid(self.format)?, raw))
    }
}

/// The object format of a repo: the format of the first recorded git id among `tips`
/// (changesets the refs point at), SHA-1 when none is recorded.
pub async fn detect_format(history: &History, tips: &[ObjectId]) -> GitResult<ObjectFormat> {
    for t in tips {
        if let Some(g) = bee_import::lookup_git_id(history.meta(), history.repo(), t).await? {
            return Ok(g.format());
        }
    }
    Ok(ObjectFormat::Sha1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::ChangeId;
    use bee_git::{BeeObjectSource, Exporter};
    use bee_object::Timestamp;
    use std::collections::BTreeMap as Map;

    struct Src {
        trees: Map<ObjectId, Tree>,
        blobs: Map<ObjectId, Vec<u8>>,
    }

    impl BeeObjectSource for Src {
        fn blob(&self, id: &ObjectId) -> bee_core::Result<Vec<u8>> {
            Ok(self.blobs[id].clone())
        }
        fn tree(&self, id: &ObjectId) -> bee_core::Result<Tree> {
            Ok(self.trees[id].clone())
        }
        fn meta(&self, _: &ObjectId) -> bee_core::Result<Meta> {
            unreachable!()
        }
        fn changeset(&self, _: &ObjectId) -> bee_core::Result<Changeset> {
            unreachable!()
        }
    }

    #[test]
    fn tree_rebuild_matches_the_exporter() {
        let f = bee_object::blob_id(b"f");
        let x = bee_object::blob_id(b"x");
        let sub = Tree::new(vec![TreeEntry::blob(b"x".to_vec(), Mode::Executable, x, 1)]).unwrap();
        let mut note = TreeNote::default();
        note.modes.insert(b"a".to_vec(), b"100664".to_vec());
        note.hidden.push(bee_git::HiddenEntry {
            name: b"empty".to_vec(),
            mode: b"40000".to_vec(),
            note: TreeNote::default(),
        });
        let root = note
            .build_tree(vec![
                TreeEntry::blob(b"a".to_vec(), Mode::File, f, 1),
                TreeEntry::tree(b"a-dir".to_vec(), sub.id()),
                TreeEntry::blob(b"l".to_vec(), Mode::Symlink, x, 1),
            ])
            .unwrap();
        let src = Src {
            trees: [(sub.id(), sub.clone()), (root.id(), root.clone())].into(),
            blobs: [(f, b"f".to_vec()), (x, b"x".to_vec())].into(),
        };
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let mut exp = Exporter::new(format);
            let mut emitted: Map<GitOid, RawObject> = Map::new();
            let want = exp
                .tree(&src, &root.id(), &mut |o, r| {
                    emitted.insert(o, r);
                    Ok(())
                })
                .unwrap();
            let ids: Map<ObjectId, GitOid> = exp.mapping().iter().map(|(s, g)| (*s, *g)).collect();
            let rnote = TreeNote::from_tree(&root).unwrap();
            let git = rebuild_git_tree(format, &root, &rnote, &|e| Ok(ids[&e.id])).unwrap();
            let raw = RawObject::new(ObjectType::Tree, git.serialize());
            assert_eq!(raw.oid(format).unwrap(), want);
            for (oid, raw) in hidden_objects(format, &rnote).unwrap() {
                assert_eq!(emitted.get(&oid), Some(&raw), "hidden tree emitted");
            }
        }
    }

    #[test]
    fn commit_rules_match_the_exporter() {
        let format = ObjectFormat::Sha1;
        let root = format.empty_tree();
        // A new-history commit.
        let meta = Meta::new("Ada <ada@x>", Timestamp::new(5, 60), "p", "subject\n");
        let cs = Changeset::new(
            Vec::new(),
            bee_object::Tree::empty().id(),
            meta.id(),
            ChangeId::new([3; 16]),
            Vec::new(),
        )
        .unwrap();
        let c = commit_for(format, &cs, &meta, &[], root).unwrap();
        assert_eq!(c, new_history_commit(root, &[], &meta, cs.change_id()));
        // An imported commit keeps its bytes only when its ChangeId matches.
        let raw = format!("tree {root}\nauthor A <a> 1 +0000\ncommitter A <a> 1 +0000\n\nm\n");
        let oid = format
            .hash_object(ObjectType::Commit, raw.as_bytes())
            .unwrap();
        let parsed = GitCommit::parse(raw.as_bytes(), format).unwrap();
        let mut imeta = Meta::new("A <a>", Timestamp::new(1, 0), "p", "m\n");
        imeta.git = Some(bee_git::import::git_extension(&parsed));
        let good = Changeset::new(
            Vec::new(),
            bee_object::Tree::empty().id(),
            imeta.id(),
            import_change_id(oid.as_bytes()),
            Vec::new(),
        )
        .unwrap();
        let c = commit_for(format, &good, &imeta, &[], root).unwrap();
        assert_eq!(c.serialize(), raw.as_bytes());
        let bad = Changeset::new(
            Vec::new(),
            bee_object::Tree::empty().id(),
            imeta.id(),
            ChangeId::new([9; 16]),
            Vec::new(),
        )
        .unwrap();
        let c = commit_for(format, &bad, &imeta, &[], root).unwrap();
        assert_ne!(c.serialize(), raw.as_bytes());
        // A parent the extension does not name means new history.
        let p = GitOid::Sha1([1; 20]);
        let c = commit_for(format, &good, &imeta, &[p], root).unwrap();
        assert_eq!(c.parents(), vec![p]);
    }
}
