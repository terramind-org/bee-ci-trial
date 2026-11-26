//! Pushed git commits become changesets with their exact git ids (SPEC §17.4).
//!
//! The conversion follows the importer's rules (`bee_git::Importer` is the reference and the
//! tests compare against it): trees map entry by entry with a git note for what bee trees
//! cannot say, commits become a Meta carrying the git extension and a changeset with the
//! importer ChangeId, and every converted tree and commit is rebuilt and compared with the
//! pushed bytes (`INTEGRITY` if it would not reproduce). Two things differ:
//!
//! - **Known subtrees are not re-read.** A pushed tree may name subtrees and blobs the client
//!   left out of the pack because the repo already has them; their bee ids come from the
//!   repo's `git_mapping` rows (one row read each), so a push costs O(change), not O(tree).
//! - **Every changeset passes the CreateChangeset checks** of the session (path permissions,
//!   push rules, object availability, push protection, budgets) before it is inserted, with
//!   the pusher as Meta `creator` and uploader.
//!
//! Writes follow SPEC §6.2: a commit's new blobs, trees and meta are uploaded, then its
//! changeset row is inserted with `object_paths`, the server's eager rows and the
//! `git_mapping` rows of the commit and of every new tree and blob.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use bee_core::{Limits, ObjectId, ObjectKind, Reason, validate_name};
use bee_git::export::imported_commit;
use bee_git::import::git_extension;
use bee_git::tree::{EntryKind, GitTree, GitTreeEntry, git_order};
use bee_git::{GitCommit, GitOid, GitTag, HiddenEntry, Ident, ObjectType, RawObject, TreeNote};
use bee_object::{Changeset, Meta, Timestamp, Tree, TreeEntry, blob_id, import_change_id};
use bee_store::trees::{fetch_path_trees, object_paths_for};
use bee_store::{DerivedRow, NewChangeset, ReadFrom, UploadRecord};

use crate::backend::{PushContext, RepoSession};
use crate::error::{GitError, GitResult};
use crate::gitmap::{GitView, mapping_rows, rebuild_git_tree};

#[derive(Clone, Debug)]
struct ConvTree {
    /// The bee tree (possibly empty, with a note, when the git tree has no files).
    id: ObjectId,
    /// True when the bee tree has entries.
    has_entries: bool,
    /// The note, needed when this tree is a hidden subtree of another.
    note: TreeNote,
}

/// True when entries are not strictly in git's order, so the note must record their order.
fn needs_order(entries: &[GitTreeEntry]) -> bool {
    let is_dir = |e: &GitTreeEntry| matches!(e.kind(), Ok(EntryKind::Tree));
    entries
        .windows(2)
        .any(|w| git_order(&w[0].name, is_dir(&w[0]), &w[1].name, is_dir(&w[1])) != Ordering::Less)
}

fn ident_parts(commit: &GitCommit, header: &[u8]) -> (String, Timestamp) {
    match commit.header(header) {
        Some(v) => match Ident::parse(&v) {
            Some(id) => {
                let mut t = String::from_utf8_lossy(&id.name).into_owned();
                t.push_str(" <");
                t.push_str(&String::from_utf8_lossy(&id.email));
                t.push('>');
                (t, Timestamp::new(id.seconds, id.tz_offset_minutes))
            }
            None => (
                String::from_utf8_lossy(&v).into_owned(),
                Timestamp::new(0, 0),
            ),
        },
        None => (String::new(), Timestamp::new(0, 0)),
    }
}

/// The Meta of a git commit, as the importer builds it, with `creator` as Meta `creator`.
pub fn commit_meta(commit: &GitCommit, creator: &str) -> GitResult<Meta> {
    let (author, author_time) = ident_parts(commit, b"author");
    let (committer, committer_time) = ident_parts(commit, b"committer");
    let mut meta = Meta::new(
        author,
        author_time,
        creator,
        String::from_utf8_lossy(commit.message().unwrap_or_default()).into_owned(),
    );
    if commit.header(b"committer").is_some() {
        meta.committer = Some(committer);
        meta.committer_time = Some(committer_time);
    }
    meta.git = Some(git_extension(commit));
    meta.validate(&Limits::DEFAULT)?;
    Ok(meta)
}

fn wrong_type(oid: &GitOid, want: ObjectType, got: ObjectType) -> GitError {
    GitError::invalid(format!(
        "git object {oid} is a {}, expected a {}",
        got.name(),
        want.name()
    ))
}

fn missing(what: &str, oid: &GitOid) -> GitError {
    GitError::new(
        Reason::MissingObject,
        format!("{what} {oid} is neither in the pushed pack nor in the repo"),
    )
}

fn integrity(message: String) -> GitError {
    GitError::new(Reason::Integrity, message)
}

/// Converts and stores the commits of one push.
pub struct Ingest<'s, 'v, 'a> {
    session: &'s dyn RepoSession,
    view: &'v mut GitView<'a>,
    objects: BTreeMap<GitOid, RawObject>,
    trees: BTreeMap<GitOid, ConvTree>,
    blobs: BTreeMap<GitOid, (ObjectId, u64)>,
    /// Objects waiting for the next upload, and their ids (deduplicated per push).
    pending: Vec<(ObjectId, Vec<u8>)>,
    staged: BTreeSet<ObjectId>,
    /// `git_mapping` rows of new trees and blobs, written with the next changeset.
    rows: Vec<DerivedRow>,
    at_ms: u64,
    push: PushContext,
    /// Commits converted or refused in this push.
    done: BTreeMap<GitOid, GitResult<ObjectId>>,
}

impl<'s, 'v, 'a> Ingest<'s, 'v, 'a> {
    /// An ingest of the objects of one pushed pack.
    pub fn new(
        session: &'s dyn RepoSession,
        view: &'v mut GitView<'a>,
        objects: BTreeMap<GitOid, RawObject>,
        push: PushContext,
        at_ms: u64,
    ) -> Ingest<'s, 'v, 'a> {
        Ingest {
            session,
            view,
            objects,
            trees: BTreeMap::new(),
            blobs: BTreeMap::new(),
            pending: Vec::new(),
            staged: BTreeSet::new(),
            rows: Vec::new(),
            at_ms,
            push,
            done: BTreeMap::new(),
        }
    }

    fn format(&self) -> bee_git::ObjectFormat {
        self.view.format()
    }

    /// The git view the ingest records its ids in.
    pub fn view_mut(&mut self) -> &mut GitView<'a> {
        self.view
    }

    /// A pushed object, or `None` when it is not in the pack.
    pub fn object(&self, oid: &GitOid) -> Option<&RawObject> {
        self.objects.get(oid)
    }

    /// Follows annotated tags from `oid` to a commit. Returns the commit and the tag objects
    /// passed on the way (outermost first). Tags not in the pack come from the repo's
    /// `git_tag` records.
    pub async fn peel(&mut self, oid: &GitOid) -> GitResult<(GitOid, Vec<(GitOid, Vec<u8>)>)> {
        let mut cur = *oid;
        let mut tags = Vec::new();
        for _ in 0..16 {
            let raw = match self.objects.get(&cur) {
                Some(r) => r.clone(),
                None => {
                    let h = self.view.history();
                    match bee_import::lookup_git_tag(h.meta(), h.repo(), &cur).await? {
                        Some(bytes) => RawObject::new(ObjectType::Tag, bytes),
                        None => return Ok((cur, tags)),
                    }
                }
            };
            match raw.kind {
                ObjectType::Commit => return Ok((cur, tags)),
                ObjectType::Tag => {
                    let tag = GitTag::parse(&raw.data, self.format())?;
                    tags.push((cur, raw.data));
                    cur = tag.object();
                }
                other => {
                    return Err(GitError::invalid(format!(
                        "{oid} points at a {}; refs carry commits (or tags of commits)",
                        other.name()
                    )));
                }
            }
        }
        Err(GitError::invalid(format!(
            "tag chain from {oid} is too deep"
        )))
    }

    /// Converts and stores every commit reachable from `tip` that the repo does not have,
    /// parents first. Returns the tip's changeset.
    pub async fn commit(&mut self, tip: &GitOid) -> GitResult<ObjectId> {
        if let Some(r) = self.done.get(tip) {
            return r.clone();
        }
        // Parents-first order over the commits the repo lacks.
        let mut order: Vec<GitOid> = Vec::new();
        let mut stack: Vec<(GitOid, bool)> = vec![(*tip, false)];
        let mut seen: BTreeSet<GitOid> = BTreeSet::new();
        while let Some((c, ready)) = stack.pop() {
            if ready {
                order.push(c);
                continue;
            }
            if self.done.contains_key(&c) || !seen.insert(c) {
                continue;
            }
            if let Some(id) = self.view.bee_of(&c).await?
                && id.kind() == ObjectKind::Changeset
            {
                self.done.insert(c, Ok(id));
                continue;
            }
            let Some(raw) = self.objects.get(&c) else {
                self.done.insert(c, Err(missing("commit", &c)));
                continue;
            };
            if raw.kind != ObjectType::Commit {
                self.done
                    .insert(c, Err(wrong_type(&c, ObjectType::Commit, raw.kind)));
                continue;
            }
            let commit = GitCommit::parse(&raw.data, self.format())?;
            stack.push((c, true));
            for p in commit.parents().iter().rev() {
                stack.push((*p, false));
            }
        }
        for c in order {
            if self.done.contains_key(&c) {
                continue;
            }
            let r = self.convert_commit(&c).await;
            if let Err(e) = &r
                && matches!(
                    e.reason.as_str(),
                    "UNAVAILABLE" | "INTERNAL" | "DEADLINE_EXCEEDED"
                )
            {
                return Err(e.clone());
            }
            self.done.insert(c, r);
        }
        self.done
            .get(tip)
            .cloned()
            .unwrap_or_else(|| Err(missing("commit", tip)))
    }

    async fn convert_commit(&mut self, oid: &GitOid) -> GitResult<ObjectId> {
        let format = self.format();
        let raw = self
            .objects
            .get(oid)
            .cloned()
            .ok_or_else(|| missing("commit", oid))?;
        let commit = GitCommit::parse(&raw.data, format)?;
        let mut parents: Vec<ObjectId> = Vec::new();
        for p in commit.parents() {
            let id = match self.done.get(&p) {
                Some(Ok(id)) => *id,
                Some(Err(e)) => {
                    return Err(GitError::named(
                        e.reason.clone(),
                        format!("parent {p} of {oid} was refused: {}", e.message),
                    ));
                }
                None => match self.view.bee_of(&p).await? {
                    Some(id) if id.kind() == ObjectKind::Changeset => id,
                    _ => return Err(missing("parent commit", &p)),
                },
            };
            if !parents.contains(&id) {
                parents.push(id);
            }
        }
        let root = self.convert_tree(&commit.tree()).await?;
        let meta = commit_meta(&commit, self.session.principal())?;
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
        self.stage(meta.id(), meta.encode());
        self.upload().await?;
        let history = self.view.history();
        let first_root = match parents.first() {
            Some(p) => Some(*history.changeset(p).await?.root()),
            None => None,
        };
        let delta = history
            .diff_trees(first_root.as_ref(), Some(&root.id), None)
            .await?;
        let changeset = Changeset::new(
            parents,
            root.id,
            meta.id(),
            import_change_id(oid.as_bytes()),
            delta,
        )?;
        let id = changeset.id();
        let mut rows = std::mem::take(&mut self.rows);
        rows.extend(mapping_rows(id, oid));
        if history.has_changeset(&id).await? {
            history.put_derived(rows).await?;
        } else {
            self.session.check_commit(&changeset, &self.push).await?;
            let paths: Vec<_> = changeset.delta().iter().map(|e| e.path.clone()).collect();
            let source = fetch_path_trees(
                history.meta(),
                history.blobs(),
                history.org(),
                Some(changeset.root()),
                &paths,
            )
            .await?;
            let object_paths = object_paths_for(&source, &changeset)?;
            let mut derived = history.eager().rows(history, id, &changeset).await?;
            derived.extend(rows);
            history
                .writer()
                .insert_changeset(NewChangeset {
                    repo: history.repo(),
                    changeset,
                    object_paths,
                    derived,
                    at_ms: self.at_ms,
                })
                .await?;
            self.session.committed(&id).await?;
        }
        self.view.remember(id, *oid, false);
        Ok(id)
    }

    fn stage(&mut self, id: ObjectId, bytes: Vec<u8>) {
        if self.staged.insert(id) {
            self.pending.push((id, bytes));
        }
    }

    async fn upload(&mut self) -> GitResult<()> {
        let objects = std::mem::take(&mut self.pending);
        if objects.is_empty() {
            return Ok(());
        }
        let history = self.view.history();
        history
            .writer()
            .upload(
                history.org(),
                objects,
                Some(UploadRecord {
                    repo: Some(history.repo()),
                    principal: self.session.principal().to_string(),
                }),
                self.at_ms,
            )
            .await?;
        Ok(())
    }

    async fn known_tree(&mut self, oid: &GitOid) -> GitResult<ConvTree> {
        if let Some(id) = self.view.bee_of(oid).await?
            && id.kind() == ObjectKind::Tree
        {
            // An all-empty root is recorded too; its note matters when it is nested.
            let tree = self.view.history().tree(&id).await?;
            return Ok(ConvTree {
                id,
                has_entries: !tree.is_empty(),
                note: TreeNote::from_tree(&tree)?,
            });
        }
        if *oid == self.format().empty_tree() {
            let tree = Tree::empty();
            self.stage(tree.id(), tree.encode());
            return Ok(ConvTree {
                id: tree.id(),
                has_entries: false,
                note: TreeNote::default(),
            });
        }
        Err(missing("tree", oid))
    }

    async fn convert_tree(&mut self, root: &GitOid) -> GitResult<ConvTree> {
        let limits = Limits::DEFAULT;
        let mut stack: Vec<(GitOid, u64, bool)> = vec![(*root, 0, false)];
        while let Some((oid, depth, ready)) = stack.pop() {
            if self.trees.contains_key(&oid) {
                continue;
            }
            if ready {
                let info = self.convert_one(&oid).await?;
                self.trees.insert(oid, info);
                continue;
            }
            if depth > limits.path_depth {
                return Err(GitError::invalid(format!(
                    "git tree {oid} nests deeper than {} levels",
                    limits.path_depth
                )));
            }
            match self.objects.get(&oid) {
                Some(raw) => {
                    if raw.kind != ObjectType::Tree {
                        return Err(wrong_type(&oid, ObjectType::Tree, raw.kind));
                    }
                    let git = GitTree::parse(&raw.data, self.format())?;
                    stack.push((oid, depth, true));
                    for e in &git.entries {
                        if matches!(e.kind()?, EntryKind::Tree) && !self.trees.contains_key(&e.oid)
                        {
                            stack.push((e.oid, depth + 1, false));
                        }
                    }
                }
                None => {
                    let info = self.known_tree(&oid).await?;
                    self.trees.insert(oid, info);
                }
            }
        }
        self.trees
            .get(root)
            .cloned()
            .ok_or_else(|| GitError::internal("root tree was not converted"))
    }

    async fn blob(&mut self, oid: &GitOid) -> GitResult<(ObjectId, u64)> {
        if let Some(b) = self.blobs.get(oid) {
            return Ok(*b);
        }
        let info = match self.objects.get(oid) {
            Some(raw) => {
                if raw.kind != ObjectType::Blob {
                    return Err(wrong_type(oid, ObjectType::Blob, raw.kind));
                }
                let size = raw.data.len() as u64;
                if size > self.session.max_blob_bytes() {
                    return Err(GitError::new(
                        Reason::TooLarge,
                        format!(
                            "blob {oid} is {size} bytes; the repo accepts at most {} (use Git LFS)",
                            self.session.max_blob_bytes()
                        ),
                    ));
                }
                let id = blob_id(&raw.data);
                let data = raw.data.clone();
                self.stage(id, data);
                self.rows.extend(mapping_rows(id, oid));
                self.view.remember(id, *oid, false);
                (id, size)
            }
            None => {
                let id = match self.view.bee_of(oid).await? {
                    Some(id) if id.kind() == ObjectKind::Blob => id,
                    _ => return Err(missing("blob", oid)),
                };
                let h = self.view.history();
                let row = h
                    .meta()
                    .get_object(h.org(), &id, ReadFrom::Primary)
                    .await?
                    .ok_or_else(|| missing("blob", oid))?;
                (id, row.size)
            }
        };
        self.blobs.insert(*oid, info);
        Ok(info)
    }

    /// Converts one pushed git tree whose subtrees are converted already.
    async fn convert_one(&mut self, oid: &GitOid) -> GitResult<ConvTree> {
        let format = self.format();
        let raw = self
            .objects
            .get(oid)
            .cloned()
            .ok_or_else(|| missing("tree", oid))?;
        let git = GitTree::parse(&raw.data, format)?;
        let mut note = TreeNote::default();
        let mut entries = Vec::with_capacity(git.entries.len());
        let mut child_oids: BTreeMap<Vec<u8>, GitOid> = BTreeMap::new();
        for e in &git.entries {
            let kind = e.kind()?;
            validate_name(&e.name)?;
            if child_oids.insert(e.name.clone(), e.oid).is_some() {
                return Err(GitError::invalid(format!(
                    "git tree {oid} has two entries named {:?}",
                    String::from_utf8_lossy(&e.name)
                )));
            }
            let canonical = kind.canonical_mode() == e.mode.as_slice();
            match kind {
                EntryKind::Tree => {
                    let sub = self
                        .trees
                        .get(&e.oid)
                        .cloned()
                        .ok_or_else(|| GitError::internal("subtree was not converted"))?;
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
                    let (id, size) = self.blob(&e.oid).await?;
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
        let rebuilt = rebuild_git_tree(format, &tree, &note, &|e| {
            child_oids
                .get(&e.name)
                .copied()
                .ok_or_else(|| integrity("lost a tree entry".into()))
        })?;
        if rebuilt.serialize() != raw.data {
            return Err(integrity(format!(
                "git tree {oid} does not survive conversion to a bee tree"
            )));
        }
        let id = tree.id();
        if has_entries {
            self.rows.extend(mapping_rows(id, oid));
            self.view.remember(id, *oid, false);
        }
        self.stage(id, tree.encode());
        Ok(ConvTree {
            id,
            has_entries,
            note,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_matches_the_importer() {
        let format = bee_git::ObjectFormat::Sha1;
        let raw = format!(
            "tree {}\nauthor Ada <ada@x> 10 +0130\ncommitter Bob <b@x> 11 -0100\nextra h\n\nmsg\n",
            format.empty_tree()
        );
        let oid = format
            .hash_object(ObjectType::Commit, raw.as_bytes())
            .unwrap();
        let commit = GitCommit::parse(raw.as_bytes(), format).unwrap();
        let mine = commit_meta(&commit, "p1").unwrap();
        let src = |o: &GitOid| -> bee_core::Result<RawObject> {
            if *o == oid {
                Ok(RawObject::new(ObjectType::Commit, raw.as_bytes().to_vec()))
            } else {
                Ok(RawObject::new(ObjectType::Tree, Vec::new()))
            }
        };
        let mut importer = bee_git::Importer::new(format, "p1");
        let theirs = importer.import_commit(&src, &oid).unwrap();
        assert_eq!(mine, theirs.meta);
        assert_eq!(mine.author, "Ada <ada@x>");
        assert_eq!(mine.committer.as_deref(), Some("Bob <b@x>"));
    }

    #[test]
    fn order_detection() {
        let e = |name: &str, kind| {
            GitTreeEntry::new(kind, name.as_bytes().to_vec(), GitOid::Sha1([1; 20]))
        };
        assert!(!needs_order(&[
            e("a", EntryKind::File),
            e("b", EntryKind::File)
        ]));
        assert!(needs_order(&[
            e("b", EntryKind::File),
            e("a", EntryKind::File)
        ]));
        // `a.b` sorts before the directory `a` (compared as `a/`).
        assert!(!needs_order(&[
            e("a.b", EntryKind::File),
            e("a", EntryKind::Tree)
        ]));
        assert!(needs_order(&[
            e("a", EntryKind::Tree),
            e("a.b", EntryKind::File)
        ]));
    }
}
