//! ReadPath and ListTree (PRODUCT §4.1 "history without a clone"): read a file or directory
//! at a changeset, and list a tree, optionally recursively, in pages.

use std::cmp::Ordering;

use bee_core::{BeeError, Mode, ObjectId, Reason, RepoPath, Result};
use bee_object::{TreeEntry, Value};

use crate::codec;
use crate::repo::{BlobContent, History, child};

/// One entry of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeItem {
    /// Full path.
    pub path: RepoPath,
    /// Mode ([`Mode::Tree`] for directories).
    pub mode: Mode,
    /// Object id.
    pub id: ObjectId,
    /// Size (0 for directories and gitlinks).
    pub size: u64,
}

/// What a path holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathContent {
    /// A file, executable or symlink with its content (redacted content is reported as such).
    File {
        /// The entry.
        item: TreeItem,
        /// The bytes.
        content: BlobContent,
    },
    /// A submodule pointer.
    Gitlink {
        /// The entry; its id is the foreign git id.
        item: TreeItem,
    },
    /// A directory and its direct entries.
    Dir {
        /// The directory's tree id.
        id: ObjectId,
        /// Its entries, in tree order.
        entries: Vec<TreeItem>,
    },
}

/// One page of [`History::list_tree`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListTreePage {
    /// Entries in pre-order (a directory before its contents, siblings by name).
    pub items: Vec<TreeItem>,
    /// Pass back to continue; `None` on the last page.
    pub next_page_token: Option<String>,
}

const TOKEN_PREFIX: &str = "lt1_";

fn item(dir: Option<&RepoPath>, e: &TreeEntry) -> Result<TreeItem> {
    Ok(TreeItem {
        path: child(dir, &e.name)?,
        mode: e.mode,
        id: e.id,
        size: e.size,
    })
}

/// Pre-order comparison: component by component, a prefix first.
pub fn cmp_components(a: &RepoPath, b: &RepoPath) -> Ordering {
    a.components().cmp(b.components())
}

fn not_found(path: Option<&RepoPath>) -> BeeError {
    let p = path.map_or(String::new(), |p| p.to_string());
    BeeError::new(Reason::NotFound, format!("path {p:?} not found")).with_payload("path", p)
}

impl History {
    /// The tree id at a directory path (`None` = root); `NOT_FOUND` when absent or a file.
    async fn dir_at(&self, changeset: &ObjectId, path: Option<&RepoPath>) -> Result<ObjectId> {
        let root = *self.changeset(changeset).await?.root();
        let Some(p) = path else { return Ok(root) };
        match self.entry_at(&root, p).await? {
            Some(e) if e.mode == Mode::Tree => Ok(e.id),
            _ => Err(not_found(path)),
        }
    }

    /// ReadPath: the file or directory at `path` (`None` = root) in a changeset.
    pub async fn read_path(
        &self,
        changeset: &ObjectId,
        path: Option<&RepoPath>,
    ) -> Result<PathContent> {
        let root = *self.changeset(changeset).await?.root();
        let entry = match path {
            None => None,
            Some(p) => Some(
                self.entry_at(&root, p)
                    .await?
                    .ok_or_else(|| not_found(path))?,
            ),
        };
        match entry {
            Some(e) if e.mode != Mode::Tree => {
                let item = TreeItem {
                    path: path.cloned().expect("a leaf has a path"),
                    mode: e.mode,
                    id: e.id,
                    size: e.size,
                };
                if e.mode == Mode::Gitlink {
                    return Ok(PathContent::Gitlink { item });
                }
                let content = self.blob(&e.id).await?;
                Ok(PathContent::File { item, content })
            }
            other => {
                let id = other.map_or(root, |e| e.id);
                let tree = self.tree(&id).await?;
                let entries = tree
                    .entries()
                    .iter()
                    .map(|e| item(path, e))
                    .collect::<Result<_>>()?;
                Ok(PathContent::Dir { id, entries })
            }
        }
    }

    /// ListTree: the entries under the directory `path` (`None` = root), recursively or not,
    /// at most `limit` per page (at least 1), resuming after `page_token`.
    pub async fn list_tree(
        &self,
        changeset: &ObjectId,
        path: Option<&RepoPath>,
        recursive: bool,
        limit: usize,
        page_token: Option<&str>,
    ) -> Result<ListTreePage> {
        let limit = limit.max(1);
        let after = match page_token {
            Some(t) => {
                let v = codec::parse_token(TOKEN_PREFIX, t)?;
                let bytes = codec::as_bytes("page token", &v).map_err(|_| codec::bad_token(t))?;
                Some(RepoPath::new(bytes).map_err(|_| codec::bad_token(t))?)
            }
            None => None,
        };
        let dir = self.dir_at(changeset, path).await?;
        let mut items: Vec<TreeItem> = Vec::new();
        let mut stack: Vec<(Option<RepoPath>, Vec<TreeEntry>, usize)> =
            vec![(path.cloned(), self.tree(&dir).await?.into_entries(), 0)];
        'walk: while let Some(top) = stack.last_mut() {
            if top.2 >= top.1.len() {
                stack.pop();
                continue;
            }
            let e = top.1[top.2].clone();
            top.2 += 1;
            let it = item(top.0.as_ref(), &e)?;
            let descend = recursive && e.mode == Mode::Tree;
            if let Some(a) = &after
                && cmp_components(&it.path, a) != Ordering::Greater
            {
                // A page can end on a directory: its entries come on the next page.
                if descend && (it.path == *a || it.path.is_ancestor_of(a)) {
                    let entries = self.tree(&e.id).await?.into_entries();
                    stack.push((Some(it.path), entries, 0));
                }
                continue;
            }
            items.push(it.clone());
            if items.len() > limit {
                break 'walk;
            }
            if descend {
                let entries = self.tree(&e.id).await?.into_entries();
                stack.push((Some(it.path), entries, 0));
            }
        }
        let next_page_token = if items.len() > limit {
            items.truncate(limit);
            items
                .last()
                .map(|i| codec::token(TOKEN_PREFIX, &Value::Bytes(i.path.as_bytes().to_vec())))
        } else {
            None
        };
        Ok(ListTreePage {
            items,
            next_page_token,
        })
    }
}
