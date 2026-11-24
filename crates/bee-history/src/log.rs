//! Log and path history (PRODUCT §4.1, §4.4).
//!
//! [`History::log`] walks the graph rows from a changeset in generation order (newest
//! first), optionally first-parent only. [`History::path_log`] walks the history nodes of one
//! path instead (a file or a directory), so it touches only the changesets that changed that
//! path, continues through delete and re-add, and reads no trees and no file contents.
//! Both page with opaque tokens that carry the walk's frontier.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use bee_core::{ChangeId, ObjectId, RepoPath, Result};
use bee_object::{Meta, Value};

use crate::codec::{self, as_array, as_id, as_uint};
use crate::pathhist::{NodeKind, PathNode};
use crate::repo::History;

/// Log options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogOptions {
    /// Follow first parents only.
    pub first_parent: bool,
    /// Entries per page (at least 1).
    pub limit: usize,
    /// Continue from a previous page.
    pub page_token: Option<String>,
    /// Only changesets whose time (committer time, else author time) is at or after this
    /// many seconds since the epoch.
    pub since: Option<i64>,
    /// Only changesets whose time is strictly before this.
    pub until: Option<i64>,
    /// Path log only: list the paths each entry changed under the logged path.
    pub changed_paths: bool,
}

impl Default for LogOptions {
    fn default() -> Self {
        LogOptions {
            first_parent: false,
            limit: 100,
            page_token: None,
            since: None,
            until: None,
            changed_paths: false,
        }
    }
}

/// One changeset in a log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// The changeset.
    pub id: ObjectId,
    /// Its parents.
    pub parents: Vec<ObjectId>,
    /// Its generation.
    pub generation: u64,
    /// Its ChangeId.
    pub change_id: ChangeId,
    /// Its meta (author, message, times, provenance); `None` when redacted.
    pub meta: Option<Meta>,
}

/// A page of [`History::log`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogPage {
    /// Entries, newest (highest generation) first.
    pub entries: Vec<LogEntry>,
    /// Pass back to continue; `None` at the end of history.
    pub next_page_token: Option<String>,
}

/// How a changeset changed a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeKind {
    /// The path appeared (new, or re-added after a delete).
    Added,
    /// The path changed in place.
    Modified,
    /// The path was deleted.
    Deleted,
}

impl ChangeKind {
    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
        }
    }
}

/// A leaf path changed by a path-log entry.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChangedPath {
    /// The path.
    pub path: RepoPath,
    /// How it changed.
    pub kind: ChangeKind,
}

/// One entry of a path log: a changeset that changed the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathLogEntry {
    /// The changeset.
    pub changeset: ObjectId,
    /// Its generation.
    pub generation: u64,
    /// The history node of the path at that changeset.
    pub node: ObjectId,
    /// What happened to the logged path itself.
    pub kind: ChangeKind,
    /// The leaf paths changed under the logged path (when requested).
    pub changed: Vec<ChangedPath>,
    /// The changeset's meta; `None` when redacted.
    pub meta: Option<Meta>,
}

/// A page of [`History::path_log`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathLogPage {
    /// Entries, newest first.
    pub entries: Vec<PathLogEntry>,
    /// Pass back to continue; `None` at the end.
    pub next_page_token: Option<String>,
}

const LOG_TOKEN: &str = "lg1_";
const PATH_LOG_TOKEN: &str = "pl1_";

fn encode_frontier(prefix: &str, heap: &BinaryHeap<(u64, ObjectId)>) -> String {
    let mut items: Vec<&(u64, ObjectId)> = heap.iter().collect();
    items.sort();
    let v = Value::Array(
        items
            .into_iter()
            .map(|(g, id)| Value::Array(vec![Value::Uint(*g), codec::id(id)]))
            .collect(),
    );
    codec::token(prefix, &v)
}

fn decode_frontier(prefix: &str, text: &str) -> Result<BinaryHeap<(u64, ObjectId)>> {
    let v = codec::parse_token(prefix, text)?;
    let bad = |_| codec::bad_token(text);
    let mut heap = BinaryHeap::new();
    for item in as_array("token", &v).map_err(bad)? {
        let pair = as_array("token", &item).map_err(bad)?;
        if pair.len() != 2 {
            return Err(codec::bad_token(text));
        }
        heap.push((
            as_uint("token", &pair[0]).map_err(bad)?,
            as_id("token", &pair[1]).map_err(bad)?,
        ));
    }
    Ok(heap)
}

fn in_window(meta: &Option<Meta>, opts: &LogOptions) -> bool {
    if opts.since.is_none() && opts.until.is_none() {
        return true;
    }
    let Some(m) = meta else { return false };
    let t = m.committer_time.unwrap_or(m.author_time).seconds;
    opts.since.is_none_or(|s| t >= s) && opts.until.is_none_or(|u| t < u)
}

fn change_kind(node: &PathNode, parent: Option<&PathNode>) -> ChangeKind {
    if !node.kind.is_live() {
        return ChangeKind::Deleted;
    }
    match parent {
        Some(p) if p.kind.is_live() && p.kind.is_dir() == node.kind.is_dir() => {
            ChangeKind::Modified
        }
        _ => ChangeKind::Added,
    }
}

impl History {
    async fn meta_of(&self, changeset: &ObjectId) -> Result<(ChangeId, Option<Meta>)> {
        let cs = self.changeset(changeset).await?;
        Ok((*cs.change_id(), self.meta_object(cs.meta()).await?))
    }

    /// The log from `from`, newest first.
    pub async fn log(&self, from: &ObjectId, opts: &LogOptions) -> Result<LogPage> {
        let limit = opts.limit.max(1);
        let mut heap = match &opts.page_token {
            Some(t) => decode_frontier(LOG_TOKEN, t)?,
            None => {
                let n = self.graph_node(from).await?;
                BinaryHeap::from([(n.generation, *from)])
            }
        };
        let mut queued: BTreeMap<ObjectId, ()> = heap.iter().map(|(_, id)| (*id, ())).collect();
        let mut entries = Vec::new();
        while entries.len() < limit {
            // The next commits in walk order, as many as the page still needs (the walk does
            // not depend on metas), then their metas in one read, then the window filter.
            let mut batch: Vec<(u64, ObjectId, Vec<ObjectId>)> = Vec::new();
            while batch.len() < limit - entries.len() {
                let Some((g, id)) = heap.pop() else { break };
                queued.remove(&id);
                let node = self.graph_node(&id).await?;
                let parents: &[ObjectId] = if opts.first_parent {
                    &node.parents[..node.parents.len().min(1)]
                } else {
                    &node.parents
                };
                for p in parents {
                    if queued.insert(*p, ()).is_none() {
                        let np = self.graph_node(p).await?;
                        heap.push((np.generation, *p));
                    }
                }
                batch.push((g, id, node.parents));
            }
            if batch.is_empty() {
                break;
            }
            let mut metas = Vec::with_capacity(batch.len());
            for (_, id, _) in &batch {
                metas.push(*self.changeset(id).await?.meta());
            }
            self.prefetch_metas(&metas).await?;
            for (g, id, parents) in batch {
                let (change_id, meta) = self.meta_of(&id).await?;
                if in_window(&meta, opts) {
                    entries.push(LogEntry {
                        id,
                        parents,
                        generation: g,
                        change_id,
                        meta,
                    });
                }
            }
        }
        let next_page_token = (!heap.is_empty()).then(|| encode_frontier(LOG_TOKEN, &heap));
        Ok(LogPage {
            entries,
            next_page_token,
        })
    }

    /// The history of one path (a file or a directory; `None` = the whole tree) from `from`,
    /// newest first: every changeset that changed it, through deletes and re-adds. Reads only
    /// history nodes (plus each entry's changeset and meta), never trees or file contents.
    /// A path that never existed gives an empty page.
    pub async fn path_log(
        &self,
        from: &ObjectId,
        path: Option<&RepoPath>,
        opts: &LogOptions,
    ) -> Result<PathLogPage> {
        let limit = opts.limit.max(1);
        let mut nodes: BTreeMap<ObjectId, PathNode> = BTreeMap::new();
        let mut heap = match &opts.page_token {
            Some(t) => decode_frontier(PATH_LOG_TOKEN, t)?,
            None => match self.node_at(from, path).await? {
                Some((id, n)) => {
                    let h = BinaryHeap::from([(n.generation, id)]);
                    nodes.insert(id, n);
                    h
                }
                None => BinaryHeap::new(),
            },
        };
        let mut queued: BTreeMap<ObjectId, ()> = heap.iter().map(|(_, id)| (*id, ())).collect();
        let mut entries = Vec::new();
        while entries.len() < limit {
            let Some((g, id)) = heap.pop() else { break };
            queued.remove(&id);
            let node = match nodes.remove(&id) {
                Some(n) => n,
                None => self.path_node(&id).await?,
            };
            let followed: &[ObjectId] = if opts.first_parent {
                &node.parents[..node.parents.len().min(1)]
            } else {
                &node.parents
            };
            let mut first_parent: Option<PathNode> = None;
            for (i, p) in node.parents.iter().enumerate() {
                let follow = followed.contains(p);
                if i > 0 && !follow {
                    continue;
                }
                let pn = match nodes.get(p) {
                    Some(n) => n.clone(),
                    None => self.path_node(p).await?,
                };
                if i == 0 {
                    first_parent = Some(pn.clone());
                }
                if follow && queued.insert(*p, ()).is_none() {
                    heap.push((pn.generation, *p));
                    nodes.insert(*p, pn);
                }
            }
            let (_, meta) = self.meta_of(&node.changeset).await?;
            if !in_window(&meta, opts) {
                continue;
            }
            let changed = if opts.changed_paths {
                self.changed_under(&node, first_parent.as_ref()).await?
            } else {
                Vec::new()
            };
            entries.push(PathLogEntry {
                changeset: node.changeset,
                generation: g,
                node: id,
                kind: change_kind(&node, first_parent.as_ref()),
                changed,
                meta,
            });
        }
        let next_page_token = (!heap.is_empty()).then(|| encode_frontier(PATH_LOG_TOKEN, &heap));
        Ok(PathLogPage {
            entries,
            next_page_token,
        })
    }

    /// The leaf paths that differ between a node and its previous node, found by comparing
    /// child lists (history nodes only; no trees, no contents).
    async fn changed_under(
        &self,
        node: &PathNode,
        previous: Option<&PathNode>,
    ) -> Result<Vec<ChangedPath>> {
        let mut out = Vec::new();
        let mut stack: Vec<(Option<PathNode>, Option<PathNode>)> =
            vec![(Some(node.clone()), previous.cloned())];
        while let Some((new, old)) = stack.pop() {
            let is_file =
                |n: &Option<PathNode>| n.as_ref().is_some_and(|n| n.kind == NodeKind::File);
            let (new_file, old_file) = (is_file(&new), is_file(&old));
            let path = new
                .as_ref()
                .and_then(|n| n.path.clone())
                .or_else(|| old.as_ref().and_then(|n| n.path.clone()));
            let kind = match (old_file, new_file) {
                (true, true) => Some(ChangeKind::Modified),
                (false, true) => Some(ChangeKind::Added),
                (true, false) => Some(ChangeKind::Deleted),
                (false, false) => None,
            };
            if let (Some(kind), Some(path)) = (kind, path) {
                out.push(ChangedPath { path, kind });
            }
            let children = |n: &Option<PathNode>| -> BTreeMap<Vec<u8>, ObjectId> {
                n.as_ref()
                    .filter(|n| n.kind.is_dir())
                    .map(|n| {
                        n.children
                            .iter()
                            .map(|c| (c.name.clone(), c.node))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let (nc, oc) = (children(&new), children(&old));
            let names: BTreeSet<&Vec<u8>> = nc.keys().chain(oc.keys()).collect();
            for name in names {
                let (a, b) = (nc.get(name), oc.get(name));
                if a == b {
                    continue;
                }
                let an = match a {
                    Some(id) => Some(self.path_node(id).await?),
                    None => None,
                };
                let bn = match b {
                    Some(id) => Some(self.path_node(id).await?),
                    None => None,
                };
                stack.push((an, bn));
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}
