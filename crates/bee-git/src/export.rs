//! bee → git (SPEC §17.4).
//!
//! **Trees.** A bee tree becomes a git tree with one entry per bee entry: blobs by their
//! git blob id, subtrees recursively, gitlinks by the git id inside their foreign id. Modes
//! are git's canonical spellings unless the tree's git note ([`crate::note`]) overrides them;
//! the note's hidden entries (empty directories) are added back; entries are in git order
//! (directories compare as if suffixed with `/`) unless the note records another order.
//!
//! **Imported commits.** When the Meta carries the git extension (key 64) the commit is
//! `tree <root>\n` followed by the extension bytes, with Meta's message substituted where the
//! extension leaves it out (see [`crate::import::git_extension`]). That reproduces the
//! original commit — and its id and signature — exactly. It is used only when the result is
//! provably the original: its id must give back the changeset's ChangeId under the importer
//! rule (SPEC §3.3) and its parents must be the git ids of the changeset's parents. A
//! changeset that was amended or rebased in bee falls through to the new-history form.
//!
//! **New history.** Everything else gets one deterministic representation:
//!
//! ```text
//! tree <root>
//! parent <git id of each bee parent, in order>
//! author <sanitized author> <author seconds> <±hhmm>
//! committer <sanitized committer, or the author> <committer time, or the author time>
//!
//! <message, trailing whitespace removed>
//!
//! Change-Id: k_<32 hex>
//! ```
//!
//! See [`sanitize_ident`] and [`change_id_trailer`] for the exact rules. Negative times are
//! written as 0 (git rejects negative dates); offsets are clamped to ±99:59.

use std::collections::BTreeMap;

use bee_core::{BeeError, ChangeId, Mode, ObjectId, ObjectKind, Reason, Result};
use bee_object::{Changeset, Meta, Tree, import_change_id};

use crate::commit::{GitCommit, Ident, split_body, split_headers};
use crate::mapping::GitMapping;
use crate::note::{TreeNote, tree_oid};
use crate::object::{ObjectType, RawObject};
use crate::oid::{GitOid, ObjectFormat};
use crate::tree::{EntryKind, GitTree, GitTreeEntry};

/// Where the exporter reads bee objects.
pub trait BeeObjectSource {
    /// Blob bytes.
    fn blob(&self, id: &ObjectId) -> Result<Vec<u8>>;
    /// A tree.
    fn tree(&self, id: &ObjectId) -> Result<Tree>;
    /// A meta.
    fn meta(&self, id: &ObjectId) -> Result<Meta>;
    /// A changeset.
    fn changeset(&self, id: &ObjectId) -> Result<Changeset>;
}

/// Which representation a commit got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Representation {
    /// The original imported commit, rebuilt from Meta's git extension.
    Imported,
    /// The deterministic new-history form.
    NewHistory,
    /// Already in the mapping; nothing was rebuilt.
    Known,
}

/// The result of exporting one changeset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExportedCommit {
    /// The bee changeset.
    pub changeset: ObjectId,
    /// Its git commit id.
    pub oid: GitOid,
    /// How the commit was produced.
    pub representation: Representation,
}

/// The sink new git objects are handed to: `(id, object)`.
pub type ObjectSink<'a> = dyn FnMut(GitOid, RawObject) -> Result<()> + 'a;

/// Converts bee objects to git objects, remembering every id it has produced.
#[derive(Clone, Debug)]
pub struct Exporter {
    format: ObjectFormat,
    mapping: GitMapping,
    hidden: BTreeMap<GitOid, RawObject>,
}

fn missing(what: &str, id: &ObjectId) -> BeeError {
    BeeError::new(
        Reason::MissingObject,
        format!("{what} {id} has not been exported to git yet"),
    )
    .with_payload("id", id.to_text())
}

impl Exporter {
    /// An exporter with an empty mapping.
    pub fn new(format: ObjectFormat) -> Exporter {
        Exporter::with_mapping(format, GitMapping::new())
    }

    /// An exporter that starts from a known mapping (objects in it are not re-emitted).
    pub fn with_mapping(format: ObjectFormat, mapping: GitMapping) -> Exporter {
        Exporter {
            format,
            mapping,
            hidden: BTreeMap::new(),
        }
    }

    /// The object format.
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    /// The mapping built so far.
    pub fn mapping(&self) -> &GitMapping {
        &self.mapping
    }

    /// Consumes the exporter, returning its mapping.
    pub fn into_mapping(self) -> GitMapping {
        self.mapping
    }

    /// The git id of a bee blob, emitting the blob if it is new.
    pub fn blob(
        &mut self,
        src: &dyn BeeObjectSource,
        id: &ObjectId,
        sink: &mut ObjectSink<'_>,
    ) -> Result<GitOid> {
        if let Some(g) = self.mapping.git(id) {
            return Ok(g);
        }
        id.expect_kind(ObjectKind::Blob)?;
        let data = src.blob(id)?;
        let raw = RawObject::new(ObjectType::Blob, data);
        let oid = raw.oid(self.format)?;
        self.mapping.insert(oid, *id)?;
        sink(oid, raw)?;
        Ok(oid)
    }

    fn git_tree(
        &mut self,
        src: &dyn BeeObjectSource,
        tree: &Tree,
        sink: &mut ObjectSink<'_>,
    ) -> Result<GitTree> {
        let format = self.format;
        let note = TreeNote::from_tree(tree)?;
        let hidden = hidden_objects(format, &note)?;
        let mut child = |e: &bee_object::TreeEntry| -> Result<GitOid> {
            match e.mode {
                Mode::Tree => self.tree(src, &e.id, sink),
                Mode::Gitlink => GitOid::from_foreign(&e.id, format),
                _ => self.blob(src, &e.id, sink),
            }
        };
        let git = rebuild_git_tree(format, tree, &note, &mut child)?;
        for (oid, raw) in hidden {
            if let std::collections::btree_map::Entry::Vacant(slot) = self.hidden.entry(oid) {
                slot.insert(raw.clone());
                sink(oid, raw)?;
            }
        }
        Ok(git)
    }

    /// The git id of a bee tree, emitting it and everything under it that is new.
    pub fn tree(
        &mut self,
        src: &dyn BeeObjectSource,
        id: &ObjectId,
        sink: &mut ObjectSink<'_>,
    ) -> Result<GitOid> {
        if let Some(g) = self.mapping.git(id) {
            return Ok(g);
        }
        id.expect_kind(ObjectKind::Tree)?;
        let tree = src.tree(id)?;
        let git = self.git_tree(src, &tree, sink)?;
        let raw = RawObject::new(ObjectType::Tree, git.serialize());
        let oid = raw.oid(self.format)?;
        self.mapping.insert(oid, *id)?;
        sink(oid, raw)?;
        Ok(oid)
    }

    /// Exports one changeset whose parents are already exported (`MISSING_OBJECT` naming the
    /// first parent that is not). Its tree and commit are emitted if new.
    pub fn export_changeset(
        &mut self,
        src: &dyn BeeObjectSource,
        id: &ObjectId,
        sink: &mut ObjectSink<'_>,
    ) -> Result<ExportedCommit> {
        if let Some(oid) = self.mapping.git(id) {
            return Ok(ExportedCommit {
                changeset: *id,
                oid,
                representation: Representation::Known,
            });
        }
        let (commit, representation) = self.build_commit(src, id, sink)?;
        let raw = RawObject::new(ObjectType::Commit, commit.serialize());
        let oid = raw.oid(self.format)?;
        self.mapping.insert(oid, *id)?;
        sink(oid, raw)?;
        Ok(ExportedCommit {
            changeset: *id,
            oid,
            representation,
        })
    }

    fn build_commit(
        &mut self,
        src: &dyn BeeObjectSource,
        id: &ObjectId,
        sink: &mut ObjectSink<'_>,
    ) -> Result<(GitCommit, Representation)> {
        id.expect_kind(ObjectKind::Changeset)?;
        let cs = src.changeset(id)?;
        let parents = cs
            .parents()
            .iter()
            .map(|p| {
                self.mapping
                    .git(p)
                    .ok_or_else(|| missing("parent changeset", p))
            })
            .collect::<Result<Vec<_>>>()?;
        let root = self.tree(src, cs.root(), sink)?;
        let meta = src.meta(cs.meta())?;
        commit_for(self.format, &cs, &meta, &parents, root)
    }

    /// Exports `tips` and every ancestor not yet exported, parents before children, and
    /// returns the commits in that order.
    pub fn export_history(
        &mut self,
        src: &dyn BeeObjectSource,
        tips: &[ObjectId],
        sink: &mut ObjectSink<'_>,
    ) -> Result<Vec<ExportedCommit>> {
        let mut out = Vec::new();
        let mut stack: Vec<(ObjectId, bool)> = tips.iter().rev().map(|t| (*t, false)).collect();
        while let Some((id, ready)) = stack.pop() {
            if self.mapping.git(&id).is_some() {
                continue;
            }
            if ready {
                out.push(self.export_changeset(src, &id, sink)?);
                continue;
            }
            let cs = src.changeset(&id)?;
            stack.push((id, true));
            for p in cs.parents().iter().rev() {
                if self.mapping.git(p).is_none() {
                    stack.push((*p, false));
                }
            }
        }
        Ok(out)
    }

    /// The git object with id `oid`, rebuilt from bee objects through the mapping (or an
    /// empty-directory tree recorded in a note). `None` when the id is unknown.
    pub fn object(&mut self, src: &dyn BeeObjectSource, oid: &GitOid) -> Result<Option<RawObject>> {
        if let Some(raw) = self.hidden.get(oid) {
            return Ok(Some(raw.clone()));
        }
        let Some(id) = self.mapping.bee(oid) else {
            return Ok(None);
        };
        let mut ignore = |_: GitOid, _: RawObject| Ok(());
        let raw = match id.kind() {
            ObjectKind::Blob => RawObject::new(ObjectType::Blob, src.blob(&id)?),
            ObjectKind::Tree => {
                let tree = src.tree(&id)?;
                RawObject::new(
                    ObjectType::Tree,
                    self.git_tree(src, &tree, &mut ignore)?.serialize(),
                )
            }
            _ => {
                let (c, _) = self.build_commit(src, &id, &mut ignore)?;
                RawObject::new(ObjectType::Commit, c.serialize())
            }
        };
        Ok(Some(raw))
    }
}

/// Every git tree object a note's hidden entries need, keyed by id.
fn hidden_objects(format: ObjectFormat, note: &TreeNote) -> Result<Vec<(GitOid, RawObject)>> {
    let mut out = Vec::new();
    for h in &note.hidden {
        out.extend(hidden_objects(format, &h.note)?);
        let t = h.note.hidden_git_tree(format)?;
        let raw = RawObject::new(ObjectType::Tree, t.serialize());
        out.push((raw.oid(format)?, raw));
    }
    Ok(out)
}

/// Rebuilds the git tree of a bee tree given each child's git id.
pub(crate) fn rebuild_git_tree(
    format: ObjectFormat,
    tree: &Tree,
    note: &TreeNote,
    child: &mut dyn FnMut(&bee_object::TreeEntry) -> Result<GitOid>,
) -> Result<GitTree> {
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
        entries.push(GitTreeEntry {
            mode: h.mode.clone(),
            name: h.name.clone(),
            oid: tree_oid(format, &h.note.hidden_git_tree(format)?)?,
        });
    }
    note.arrange(entries)
}

/// The commit for a changeset: the imported original when provably exact, else new history.
fn commit_for(
    format: ObjectFormat,
    cs: &Changeset,
    meta: &Meta,
    parents: &[GitOid],
    root: GitOid,
) -> Result<(GitCommit, Representation)> {
    if let Some(ext) = &meta.git {
        let c = imported_commit(root, ext, &meta.message);
        let oid = format.hash_object(ObjectType::Commit, &c.serialize())?;
        if import_change_id(oid.as_bytes()) == *cs.change_id()
            && extension_parents(format, ext).as_deref() == Some(parents)
        {
            return Ok((c, Representation::Imported));
        }
    }
    Ok((
        new_history_commit(root, parents, meta, cs.change_id()),
        Representation::NewHistory,
    ))
}

/// The parents named by an extension's leading `parent` lines, duplicates dropped (bee
/// parents are unique). `None` if one does not parse.
fn extension_parents(format: ObjectFormat, ext: &[u8]) -> Option<Vec<GitOid>> {
    let (headers, _) = split_body(ext);
    let mut out: Vec<GitOid> = Vec::new();
    for (name, value) in split_headers(&headers) {
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

/// Rebuilds an imported commit from its root tree id, the Meta git extension and Meta's
/// message.
pub fn imported_commit(root: GitOid, ext: &[u8], message: &str) -> GitCommit {
    let (headers, msg) = split_body(ext);
    let message = match msg {
        None => None,
        Some(m) if !m.is_empty() => Some(m),
        Some(_) => Some(message.as_bytes().to_vec()),
    };
    GitCommit::from_parts(root, headers, message)
}

/// The new-history commit for a changeset (see the module docs).
pub fn new_history_commit(
    root: GitOid,
    parents: &[GitOid],
    meta: &Meta,
    change_id: &ChangeId,
) -> GitCommit {
    let (an, ae) = sanitize_ident(&meta.author);
    let author = Ident {
        name: an,
        email: ae,
        seconds: meta.author_time.seconds.max(0),
        tz_offset_minutes: meta.author_time.tz_offset_minutes,
    };
    let (cn, ce) = sanitize_ident(meta.committer.as_deref().unwrap_or(&meta.author));
    let ct = meta.committer_time.unwrap_or(meta.author_time);
    let committer = Ident {
        name: cn,
        email: ce,
        seconds: ct.seconds.max(0),
        tz_offset_minutes: ct.tz_offset_minutes,
    };
    let message = change_id_trailer(&meta.message, change_id);
    GitCommit::new(root, parents, &author, &committer, message.as_bytes())
}

/// Bytes git strips from the ends of identity parts.
fn is_crud(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | ',' | ':' | ';' | '"' | '\'' | '\\')
}

fn clean_part(s: &str) -> String {
    let mapped: String = s
        .chars()
        .filter(|&c| c != '<' && c != '>')
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = mapped.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.trim_matches(is_crud).to_string()
}

/// Splits Meta's author (or committer) text into a git name and email that every git version
/// accepts (`git fsck` clean):
///
/// 1. If the text, trimmed, ends with `>` and contains `<`, the email is between the last `<`
///    and that final `>`, and the name is everything before that `<`. Otherwise the whole
///    text is the name and the email is empty.
/// 2. In both parts, `<` and `>` are removed, control characters become spaces, runs of
///    whitespace collapse to one space, and whitespace and `.,:;"'\` are trimmed from both
///    ends.
/// 3. An empty name becomes `unknown`. An empty email stays empty (`<>`).
pub fn sanitize_ident(text: &str) -> (Vec<u8>, Vec<u8>) {
    let t = text.trim();
    let (name, email) = match (t.strip_suffix('>'), t.rfind('<')) {
        (Some(body), Some(lt)) if lt < body.len() => (&t[..lt], &body[lt + 1..]),
        _ => (t, ""),
    };
    let mut name = clean_part(name);
    if name.is_empty() {
        name = "unknown".to_string();
    }
    (name.into_bytes(), clean_part(email).into_bytes())
}

fn is_trailer_line(line: &str) -> bool {
    match line.split_once(": ") {
        Some((token, value)) => {
            !token.is_empty()
                && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !value.trim().is_empty()
        }
        None => false,
    }
}

/// Appends the `Change-Id: k_<hex>` trailer to a message:
///
/// 1. Trailing whitespace (newlines included) is removed.
/// 2. An empty message becomes just the trailer line.
/// 3. If the last line already is this exact trailer, it is not added again.
/// 4. If the message has more than one paragraph and its last paragraph consists only of
///    `Token: value` trailer lines, the trailer joins that paragraph.
/// 5. Otherwise the trailer becomes a new paragraph after an empty line.
///
/// The result always ends with one newline.
pub fn change_id_trailer(message: &str, change_id: &ChangeId) -> String {
    let trailer = format!("Change-Id: {change_id}");
    let m = message.trim_end();
    if m.is_empty() {
        return format!("{trailer}\n");
    }
    if m.lines().last() == Some(trailer.as_str()) {
        return format!("{m}\n");
    }
    let joins_block = match m.rsplit_once("\n\n") {
        Some((_, last)) => last.lines().all(is_trailer_line),
        None => false,
    };
    if joins_block {
        format!("{m}\n{trailer}\n")
    } else {
        format!("{m}\n\n{trailer}\n")
    }
}

/// The ChangeId named by the last `Change-Id: k_<hex>` line of a message's final paragraph,
/// if any. Receive-pack uses it to keep a change's identity across git round trips.
pub fn parse_change_id_trailer(message: &[u8]) -> Option<ChangeId> {
    let text = String::from_utf8_lossy(message);
    let m = text.trim_end();
    let last = m.rsplit_once("\n\n").map_or(m, |(_, l)| l);
    last.lines()
        .rev()
        .find_map(|l| ChangeId::parse(l.strip_prefix("Change-Id: ")?.trim()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_object::Timestamp;

    #[test]
    fn sanitizing() {
        let s = |t: &str| {
            let (n, e) = sanitize_ident(t);
            (String::from_utf8(n).unwrap(), String::from_utf8(e).unwrap())
        };
        assert_eq!(
            s("Ada Lovelace <ada@example.com>"),
            ("Ada Lovelace".into(), "ada@example.com".into())
        );
        assert_eq!(
            s("agent:claude/run-7"),
            ("agent:claude/run-7".into(), String::new())
        );
        assert_eq!(s("  \"Bob\"  "), ("Bob".into(), String::new()));
        assert_eq!(s("Eve\n<x> <e@x>"), ("Eve x".into(), "e@x".into()));
        assert_eq!(s("<only@email>"), ("unknown".into(), "only@email".into()));
        assert_eq!(s(""), ("unknown".into(), String::new()));
        assert_eq!(s("a>b"), ("ab".into(), String::new()));
        assert_eq!(s("x <a\tb>"), ("x".into(), "a b".into()));
        assert_eq!(s("..."), ("unknown".into(), String::new()));
    }

    #[test]
    fn trailers() {
        let id = ChangeId::new([0xab; 16]);
        let t = format!("Change-Id: {id}");
        assert_eq!(change_id_trailer("", &id), format!("{t}\n"));
        assert_eq!(
            change_id_trailer("fix bug\n\n", &id),
            format!("fix bug\n\n{t}\n")
        );
        assert_eq!(
            change_id_trailer("fix: thing", &id),
            format!("fix: thing\n\n{t}\n")
        );
        assert_eq!(
            change_id_trailer("subj\n\nbody\n\nSigned-off-by: A <a@x>\n", &id),
            format!("subj\n\nbody\n\nSigned-off-by: A <a@x>\n{t}\n")
        );
        assert_eq!(
            change_id_trailer(&format!("subj\n\n{t}\n"), &id),
            format!("subj\n\n{t}\n")
        );
        assert_eq!(
            change_id_trailer("subj\n\nnot a trailer line\n", &id),
            format!("subj\n\nnot a trailer line\n\n{t}\n")
        );
        let msg = change_id_trailer("subj\n\nbody", &id);
        assert_eq!(parse_change_id_trailer(msg.as_bytes()), Some(id));
        assert_eq!(parse_change_id_trailer(b"subj\n\nChange-Id: k_zz\n"), None);
        assert_eq!(parse_change_id_trailer(b"plain"), None);
    }

    #[test]
    fn new_history_shape() {
        let mut meta = Meta::new(
            "Ada <ada@x>",
            Timestamp::new(-5, 90),
            "p",
            "subject\n\nbody\n",
        );
        let id = ChangeId::new([1; 16]);
        let root = ObjectFormat::Sha1.empty_tree();
        let parent = GitOid::Sha1([7; 20]);
        let c = new_history_commit(root, &[parent], &meta, &id);
        let text = String::from_utf8(c.serialize()).unwrap();
        assert_eq!(
            text,
            format!(
                "tree {root}\nparent {parent}\nauthor Ada <ada@x> 0 +0130\n\
                 committer Ada <ada@x> 0 +0130\n\nsubject\n\nbody\n\nChange-Id: {id}\n"
            )
        );
        meta.committer = Some("Bot".into());
        meta.committer_time = Some(Timestamp::new(10, -60));
        let c = new_history_commit(root, &[], &meta, &id);
        assert_eq!(c.header(b"committer").unwrap(), b"Bot <> 10 -0100".to_vec());
    }

    #[test]
    fn imported_commit_message_rules() {
        let root = ObjectFormat::Sha1.empty_tree();
        let c = imported_commit(root, b"author a <b> 1 +0000\n\n", "hello\n");
        assert_eq!(c.message().unwrap(), b"hello\n");
        let c = imported_commit(root, b"author a <b> 1 +0000\n\n\xff\xfe", "lossy");
        assert_eq!(c.message().unwrap(), b"\xff\xfe");
        let c = imported_commit(root, b"author a <b> 1 +0000", "ignored");
        assert_eq!(c.message(), None);
        assert_eq!(
            extension_parents(ObjectFormat::Sha1, b"parent zz\n\n"),
            None
        );
        let p = "1".repeat(40);
        let ext = format!("parent {p}\nparent {p}\nauthor x\n\n");
        assert_eq!(
            extension_parents(ObjectFormat::Sha1, ext.as_bytes()).unwrap(),
            vec![GitOid::Sha1([0x11; 20])]
        );
    }
}
