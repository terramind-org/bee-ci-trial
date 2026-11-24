//! The `blame` derived type (SPEC §8, lazy) and the Blame query (AS-CORE-02).
//!
//! Blame is keyed by a file's history node ([`crate::pathhist`]). A node's value attributes
//! each of its lines to the history node (file version) that introduced it, as runs:
//!
//! ```text
//! {1: [[length, origin node id, origin first line (0-based)], ...]}
//! ```
//!
//! Derivation, parent by parent: the node's content is diffed (`diff3-v1` Myers, via
//! `bee-merge`) against each previous version in parent order; a line matched in a parent
//! takes that parent's attribution (the first parent that matches wins); unmatched lines are
//! attributed to this node. A previous version that is a tombstone does not count, so a file
//! deleted and re-added starts afresh. Redacted content reads as empty. Values are derived
//! lazily: the nearest previous versions with stored values are found and the chain is
//! derived forward iteratively, writing each value back.
//!
//! **Ignore list** (`.git-blame-ignore-revs` semantics): when a node's changeset is ignored,
//! its unmatched lines take the attribution of the first parent's line at the same offset
//! within the changed region, where there is one. Blame with an ignore list is stored under a
//! separate key (derived kind `0x11`, BLAKE3 of the node id and the sorted ignore list).

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use bee_core::{BeeError, ObjectId, ObjectKind, Reason, RepoPath, Result, hex};
use bee_object::{Meta, Value, digest};
use bee_store::DerivedRow;

use crate::codec::{self, Fields, as_array, as_id, as_uint, corrupt};
use crate::derive::{Forward, ensure};
use crate::pathhist::{NodeKind, PathNode};
use crate::repo::History;

/// The derived type name.
pub const BLAME_TYPE: &str = "blame";
/// The derived type version.
pub const BLAME_VERSION: u32 = 1;
/// The derived object kind byte of blame keys with an ignore list (`d111_…`).
pub const BLAME_KEY_KIND: u8 = 0x11;
/// The file holding the default ignore list.
pub const IGNORE_REVS_FILE: &[u8] = b".git-blame-ignore-revs";

/// A run of lines with one origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlameRun {
    /// Number of lines.
    pub len: u64,
    /// The history node that introduced them.
    pub origin: ObjectId,
    /// The first line's 0-based number in that version.
    pub origin_line: u64,
}

/// Encodes blame runs.
pub fn encode_runs(runs: &[BlameRun]) -> Vec<u8> {
    codec::map([(
        1,
        Value::Array(
            runs.iter()
                .map(|r| {
                    Value::Array(vec![
                        Value::Uint(r.len),
                        codec::id(&r.origin),
                        Value::Uint(r.origin_line),
                    ])
                })
                .collect(),
        ),
    )])
    .encode()
}

/// Decodes blame runs.
pub fn decode_runs(bytes: &[u8]) -> Result<Vec<BlameRun>> {
    let f = Fields::decode(BLAME_TYPE, bytes)?;
    let mut out = Vec::new();
    for r in f.opt_array(1)? {
        let r = as_array(BLAME_TYPE, &r)?;
        if r.len() != 3 {
            return Err(corrupt(BLAME_TYPE, "run is not a triple"));
        }
        out.push(BlameRun {
            len: as_uint(BLAME_TYPE, &r[0])?,
            origin: as_id(BLAME_TYPE, &r[1])?,
            origin_line: as_uint(BLAME_TYPE, &r[2])?,
        });
    }
    Ok(out)
}

fn expand(runs: &[BlameRun]) -> Vec<(ObjectId, u64)> {
    let mut out = Vec::new();
    for r in runs {
        for i in 0..r.len {
            out.push((r.origin, r.origin_line + i));
        }
    }
    out
}

fn compress(lines: &[(ObjectId, u64)]) -> Vec<BlameRun> {
    let mut out: Vec<BlameRun> = Vec::new();
    for &(origin, line) in lines {
        if let Some(last) = out.last_mut()
            && last.origin == origin
            && last.origin_line + last.len == line
        {
            last.len += 1;
            continue;
        }
        out.push(BlameRun {
            len: 1,
            origin,
            origin_line: line,
        });
    }
    out
}

/// Matched `(old line, new line)` pairs implied by a list of diff hunks.
fn matched(hunks: &[bee_merge::DiffHunk], old_len: usize, new_len: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let (mut x, mut y) = (0, 0);
    for h in hunks {
        while x < h.old_start && y < h.new_start {
            out.push((x, y));
            x += 1;
            y += 1;
        }
        x = h.old_end();
        y = h.new_end();
    }
    while x < old_len && y < new_len {
        out.push((x, y));
        x += 1;
        y += 1;
    }
    out
}

/// One blame step: attributes `content`'s lines given each live previous version's content
/// and attribution (in parent order). Pure.
pub fn blame_step(
    node: ObjectId,
    content: &[u8],
    parents: &[(Vec<u8>, Vec<BlameRun>)],
    ignored: bool,
) -> Vec<BlameRun> {
    let lines = bee_merge::split_lines(content);
    let mut origins: Vec<Option<(ObjectId, u64)>> = vec![None; lines.len()];
    let mut first_hunks = None;
    for (i, (pcontent, pruns)) in parents.iter().enumerate() {
        let plines = bee_merge::split_lines(pcontent);
        let pattr = expand(pruns);
        let hunks = bee_merge::diff_lines(&plines, &lines);
        for (x, y) in matched(&hunks, plines.len(), lines.len()) {
            if origins[y].is_none()
                && let Some(a) = pattr.get(x)
            {
                origins[y] = Some(*a);
            }
        }
        if i == 0 {
            first_hunks = Some((hunks, pattr));
        }
    }
    if ignored && let Some((hunks, pattr)) = first_hunks {
        for h in hunks {
            for k in 0..h.new_len.min(h.old_len) {
                let y = h.new_start + k;
                if origins[y].is_none()
                    && let Some(a) = pattr.get(h.old_start + k)
                {
                    origins[y] = Some(*a);
                }
            }
        }
    }
    let lines: Vec<(ObjectId, u64)> = origins
        .into_iter()
        .enumerate()
        .map(|(j, o)| o.unwrap_or((node, j as u64)))
        .collect();
    compress(&lines)
}

/// Blame options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlameOptions {
    /// Changesets whose changes are looked through (`.git-blame-ignore-revs`).
    pub ignore_revs: Vec<ObjectId>,
}

/// A run of lines in a blame result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameRange {
    /// First line (0-based) in the blamed file.
    pub start: usize,
    /// Number of lines.
    pub len: usize,
    /// The changeset that introduced them.
    pub changeset: ObjectId,
    /// The file's path in that changeset.
    pub path: RepoPath,
    /// The first line's 0-based number there.
    pub origin_start: usize,
}

/// One line of a blame result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameLine {
    /// 1-based line number in the blamed file.
    pub line: usize,
    /// The changeset that introduced it.
    pub changeset: ObjectId,
    /// Its 1-based line number in that changeset's version.
    pub origin_line: usize,
}

/// The result of [`History::blame`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameResult {
    /// The blamed path.
    pub path: RepoPath,
    /// The file's history node at the blamed changeset.
    pub node: ObjectId,
    /// Runs covering every line, in order.
    pub ranges: Vec<BlameRange>,
    /// The meta of every changeset named in `ranges` (who, or which agent, made it; its
    /// provenance); `None` when redacted.
    pub metas: BTreeMap<ObjectId, Option<Meta>>,
}

impl BlameResult {
    /// Number of lines blamed.
    pub fn line_count(&self) -> usize {
        self.ranges.iter().map(|r| r.len).sum()
    }

    /// Every line's attribution.
    pub fn lines(&self) -> Vec<BlameLine> {
        let mut out = Vec::new();
        for r in &self.ranges {
            for i in 0..r.len {
                out.push(BlameLine {
                    line: r.start + i + 1,
                    changeset: r.changeset,
                    origin_line: r.origin_start + i + 1,
                });
            }
        }
        out
    }

    /// The attribution of a 1-based line.
    pub fn line(&self, line: usize) -> Option<&BlameRange> {
        let i = line.checked_sub(1)?;
        self.ranges
            .iter()
            .find(|r| r.start <= i && i < r.start + r.len)
    }
}

/// The blame key of a node under an ignore list (the node id itself when the list is empty).
pub fn blame_key(node: &ObjectId, ignore: &BTreeSet<ObjectId>) -> ObjectId {
    if ignore.is_empty() {
        return *node;
    }
    let mut input = b"swarm-blame\0".to_vec();
    input.extend_from_slice(node.as_bytes());
    for id in ignore {
        input.extend_from_slice(id.as_bytes());
    }
    ObjectId::new(ObjectKind::Derived(BLAME_KEY_KIND), digest(&input))
}

/// [`Forward`] over blame values of one file history under one ignore list.
pub struct BlameRows<'a> {
    history: &'a History,
    ignore: BTreeSet<ObjectId>,
    nodes: Mutex<BTreeMap<ObjectId, ObjectId>>,
}

impl<'a> BlameRows<'a> {
    /// Blame rows under `ignore`.
    pub fn new(history: &'a History, ignore: BTreeSet<ObjectId>) -> BlameRows<'a> {
        BlameRows {
            history,
            ignore,
            nodes: Mutex::new(BTreeMap::new()),
        }
    }

    /// The key for `node`, remembering which node it stands for.
    pub fn key_for(&self, node: &ObjectId) -> ObjectId {
        let key = blame_key(node, &self.ignore);
        self.nodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, *node);
        key
    }

    fn node_of(&self, key: &ObjectId) -> Result<ObjectId> {
        self.nodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .copied()
            .ok_or_else(|| BeeError::new(Reason::Internal, format!("unknown blame key {key}")))
    }

    async fn content(&self, node: &PathNode) -> Result<Vec<u8>> {
        let e = node
            .entry
            .ok_or_else(|| corrupt("path_history node", "file node without an entry"))?;
        Ok(self
            .history
            .leaf_content(e.mode, &e.id)
            .await?
            .bytes_or_empty()
            .to_vec())
    }
}

#[async_trait]
impl Forward for BlameRows<'_> {
    fn what(&self) -> &str {
        BLAME_TYPE
    }

    async fn stored(&self, key: &ObjectId) -> Result<Option<Vec<u8>>> {
        self.history.derived(key, BLAME_TYPE, BLAME_VERSION).await
    }

    async fn deps(&self, key: &ObjectId) -> Result<Vec<ObjectId>> {
        let node = self.history.path_node(&self.node_of(key)?).await?;
        let mut out = Vec::new();
        for p in &node.parents {
            if self.history.path_node(p).await?.kind == NodeKind::File {
                out.push(self.key_for(p));
            }
        }
        Ok(out)
    }

    async fn derive(&self, key: &ObjectId, deps: &[(ObjectId, Vec<u8>)]) -> Result<Vec<u8>> {
        let id = self.node_of(key)?;
        let node = self.history.path_node(&id).await?;
        let content = self.content(&node).await?;
        let mut parents = Vec::new();
        for (k, v) in deps {
            let pn = self.history.path_node(&self.node_of(k)?).await?;
            parents.push((self.content(&pn).await?, decode_runs(v)?));
        }
        let ignored = self.ignore.contains(&node.changeset);
        Ok(encode_runs(&blame_step(id, &content, &parents, ignored)))
    }

    async fn store(&self, key: &ObjectId, value: &[u8]) -> Result<()> {
        self.history
            .put_derived(vec![DerivedRow {
                key: *key,
                type_name: BLAME_TYPE.into(),
                version: BLAME_VERSION,
                value: value.to_vec(),
            }])
            .await
    }
}

impl History {
    /// The blame runs of a file history node (deriving lazily).
    pub async fn blame_runs(
        &self,
        node: &ObjectId,
        ignore: &BTreeSet<ObjectId>,
    ) -> Result<Vec<BlameRun>> {
        let rows = BlameRows::new(self, ignore.clone());
        let key = rows.key_for(node);
        decode_runs(&ensure(&rows, &key).await?)
    }

    /// Blame of the file at `path` in `changeset`: which changeset introduced each line, and
    /// its meta (author, creator, provenance). `NOT_FOUND` unless the path is a file there.
    pub async fn blame(
        &self,
        changeset: &ObjectId,
        path: &RepoPath,
        opts: &BlameOptions,
    ) -> Result<BlameResult> {
        let not_file = || {
            BeeError::new(
                Reason::NotFound,
                format!("{path} is not a file in {changeset}"),
            )
            .with_payload("path", path.to_string())
        };
        let (id, node) = self
            .node_at(changeset, Some(path))
            .await?
            .ok_or_else(not_file)?;
        if node.kind != NodeKind::File {
            return Err(not_file());
        }
        let ignore: BTreeSet<ObjectId> = opts.ignore_revs.iter().copied().collect();
        let runs = self.blame_runs(&id, &ignore).await?;
        let mut origins: BTreeMap<ObjectId, (ObjectId, RepoPath)> = BTreeMap::new();
        let mut metas = BTreeMap::new();
        let mut ranges = Vec::new();
        let mut start = 0usize;
        for r in runs {
            if let Entry::Vacant(v) = origins.entry(r.origin) {
                let on = self.path_node(&r.origin).await?;
                let p = on
                    .path
                    .clone()
                    .ok_or_else(|| corrupt("path_history node", "file node without a path"))?;
                v.insert((on.changeset, p));
            }
            let (cs, p) = origins[&r.origin].clone();
            if let Entry::Vacant(v) = metas.entry(cs) {
                let meta_id = *self.changeset(&cs).await?.meta();
                v.insert(self.meta_object(&meta_id).await?);
            }
            ranges.push(BlameRange {
                start,
                len: r.len as usize,
                changeset: cs,
                path: p,
                origin_start: r.origin_line as usize,
            });
            start += r.len as usize;
        }
        Ok(BlameResult {
            path: path.clone(),
            node: id,
            ranges,
            metas,
        })
    }

    /// The ignore list in `.git-blame-ignore-revs` at the root of `changeset`: one revision per
    /// line (`#` starts a comment); full or abbreviated git commit ids and changeset ids or
    /// prefixes. Entries that do not resolve are skipped. Empty when there is no such file.
    pub async fn blame_ignore_revs(&self, changeset: &ObjectId) -> Result<Vec<ObjectId>> {
        let root = *self.changeset(changeset).await?.root();
        let path = RepoPath::new(IGNORE_REVS_FILE)?;
        let Some(e) = self.entry_at(&root, &path).await? else {
            return Ok(Vec::new());
        };
        if !e.mode.is_blob() {
            return Ok(Vec::new());
        }
        let content = self.blob(&e.id).await?;
        let text = String::from_utf8_lossy(content.bytes_or_empty()).into_owned();
        let mut out = BTreeSet::new();
        for line in text.lines() {
            let token = line.split('#').next().unwrap_or("").trim();
            if token.is_empty() {
                continue;
            }
            let resolved = if hex::is_lower_hex(token) {
                self.resolve_git(token).await
            } else {
                self.resolve_str(token, None).await
            };
            if let Ok(id) = resolved {
                out.insert(id);
            }
        }
        Ok(out.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(b: u8) -> ObjectId {
        ObjectId::new(ObjectKind::Derived(0x10), [b; 32])
    }

    #[test]
    fn step_attributes_matched_lines_to_parents() {
        let v1 = blame_step(n(1), b"a\nb\n", &[], false);
        assert_eq!(
            v1,
            vec![BlameRun {
                len: 2,
                origin: n(1),
                origin_line: 0
            }]
        );
        let v2 = blame_step(
            n(2),
            b"a\nB\nb\n",
            &[(b"a\nb\n".to_vec(), v1.clone())],
            false,
        );
        assert_eq!(expand(&v2), vec![(n(1), 0), (n(2), 1), (n(1), 1)]);
        // Ignored: a replaced line takes the parent's line at the same offset.
        let v3 = blame_step(
            n(3),
            b"a\nX\nb\n",
            &[(b"a\nB\nb\n".to_vec(), v2.clone())],
            true,
        );
        assert_eq!(expand(&v3), vec![(n(1), 0), (n(2), 1), (n(1), 1)]);
        let v3n = blame_step(
            n(3),
            b"a\nX\nb\n",
            &[(b"a\nB\nb\n".to_vec(), v2.clone())],
            false,
        );
        assert_eq!(expand(&v3n)[1], (n(3), 1));
        // Merge: the second parent supplies lines the first lacks.
        let other = blame_step(n(4), b"z\n", &[], false);
        let m = blame_step(
            n(5),
            b"a\nB\nb\nz\n",
            &[(b"a\nB\nb\n".to_vec(), v2), (b"z\n".to_vec(), other)],
            false,
        );
        assert_eq!(expand(&m)[3], (n(4), 0));
        assert_eq!(decode_runs(&encode_runs(&m)).unwrap(), m);
        assert!(blame_step(n(6), b"", &[], false).is_empty());
    }

    #[test]
    fn keys_depend_on_the_ignore_list() {
        let none = BTreeSet::new();
        assert_eq!(blame_key(&n(1), &none), n(1));
        let some: BTreeSet<ObjectId> = [ObjectId::new(ObjectKind::Changeset, [9; 32])].into();
        let k = blame_key(&n(1), &some);
        assert_eq!(k.kind(), ObjectKind::Derived(BLAME_KEY_KIND));
        assert_ne!(k, blame_key(&n(2), &some));
    }
}
