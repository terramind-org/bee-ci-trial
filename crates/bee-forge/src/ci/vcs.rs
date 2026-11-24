//! What CI reads from the VCS, and the implementation over `bee-history`.
//!
//! CI never touches the VCS stores directly: workflow discovery, changed paths for path
//! filters, commit messages for `[skip ci]`, and job checkouts all go through [`CiVcs`]. The
//! dependency points forge → VCS only (SPEC §15.1). [`HistoryCiVcs`] implements it over the
//! changes module's [`HistoryVcs`].

use std::sync::Arc;

use async_trait::async_trait;
use bee_core::{Mode, ObjectId as CoreId};
use bee_proto::v1::{ObjectId, Repo};

use crate::changes::vcs::ChangeVcs;
use crate::changes::{HistoryVcs, vcs::vcs_err};
use crate::error::{ForgeError, Result};

/// Most files one checkout returns.
pub const MAX_CHECKOUT_FILES: usize = 100_000;

/// One file of a checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeFile {
    /// Repo-relative path.
    pub path: String,
    /// Contents.
    pub bytes: Vec<u8>,
    /// Executable bit.
    pub executable: bool,
}

/// What CI needs from the VCS.
#[async_trait]
pub trait CiVcs: Send + Sync {
    /// The tip of a ref; `None` when absent or deleted.
    async fn ref_tip(&self, repo: &Repo, name: &str) -> Result<Option<ObjectId>>;
    /// A file's bytes at a changeset; `None` when absent.
    async fn read_file(&self, repo: &Repo, at: &ObjectId, path: &str) -> Result<Option<Vec<u8>>>;
    /// A file's blob id at a changeset; `None` when absent.
    async fn file_id(&self, repo: &Repo, at: &ObjectId, path: &str) -> Result<Option<ObjectId>>;
    /// Names of the files directly in `dir`.
    async fn list_dir(&self, repo: &Repo, at: &ObjectId, dir: &str) -> Result<Vec<String>>;
    /// Paths that differ between `from` and `to` (every file of `to` when `from` is `None`).
    async fn changed_paths(
        &self,
        repo: &Repo,
        from: Option<&ObjectId>,
        to: &ObjectId,
    ) -> Result<Vec<String>>;
    /// A changeset's message.
    async fn message(&self, repo: &Repo, id: &ObjectId) -> Result<String>;
    /// Whether `a` is an ancestor of (or equal to) `b`.
    async fn is_ancestor(&self, repo: &Repo, a: &ObjectId, b: &ObjectId) -> Result<bool>;
    /// First-parent changesets from `to` back to (not including) `from`, at most `limit + 1`.
    async fn count_between(
        &self,
        repo: &Repo,
        from: Option<&ObjectId>,
        to: &ObjectId,
        limit: u64,
    ) -> Result<u64>;
    /// Every file of a changeset.
    async fn checkout(&self, repo: &Repo, at: &ObjectId) -> Result<Vec<TreeFile>>;
    /// The first parent of a changeset (`None` for a root). Ports that cannot walk history
    /// answer UNIMPLEMENTED; green status then only looks at the tip.
    async fn first_parent(&self, repo: &Repo, id: &ObjectId) -> Result<Option<ObjectId>> {
        let _ = (repo, id);
        Err(ForgeError::unimplemented(
            "this VCS port cannot walk history",
        ))
    }
    /// Points the non-protected ref `name` at `target` with a plain compare-and-swap, only if
    /// it currently targets `expected` (`None` = absent). `false` when the ref moved meanwhile.
    /// CI writes `green/*` (SPEC §5) through this; ports that cannot write refs answer
    /// UNIMPLEMENTED and `green/*` is then only computed on demand.
    async fn set_ref_if(
        &self,
        repo: &Repo,
        name: &str,
        expected: Option<&ObjectId>,
        target: &ObjectId,
    ) -> Result<bool> {
        let _ = (repo, name, expected, target);
        Err(ForgeError::unimplemented("this VCS port cannot write refs"))
    }
}

/// [`CiVcs`] over `bee-history`.
pub struct HistoryCiVcs {
    inner: Arc<HistoryVcs>,
}

impl HistoryCiVcs {
    /// Over the changes module's VCS handle.
    pub fn new(inner: Arc<HistoryVcs>) -> HistoryCiVcs {
        HistoryCiVcs { inner }
    }
}

fn core_id(id: &ObjectId) -> Result<CoreId> {
    CoreId::from_bytes(&id.raw).map_err(|e| ForgeError::invalid("bad_id", e.message))
}

#[async_trait]
impl CiVcs for HistoryCiVcs {
    async fn ref_tip(&self, repo: &Repo, name: &str) -> Result<Option<ObjectId>> {
        Ok(self.inner.ref_tip(repo, name).await?.and_then(|r| r.target))
    }

    async fn read_file(&self, repo: &Repo, at: &ObjectId, path: &str) -> Result<Option<Vec<u8>>> {
        self.inner.read_file(repo, at, path).await
    }

    async fn file_id(&self, repo: &Repo, at: &ObjectId, path: &str) -> Result<Option<ObjectId>> {
        self.inner.file_id(repo, at, path).await
    }

    async fn list_dir(&self, repo: &Repo, at: &ObjectId, dir: &str) -> Result<Vec<String>> {
        self.inner.list_dir(repo, at, dir).await
    }

    async fn changed_paths(
        &self,
        repo: &Repo,
        from: Option<&ObjectId>,
        to: &ObjectId,
    ) -> Result<Vec<String>> {
        match from {
            Some(f) => Ok(self
                .inner
                .diff(repo, f, to, &[], false, 0)
                .await?
                .into_iter()
                .map(|d| String::from_utf8_lossy(&d.path).into_owned())
                .collect()),
            None => Ok(self
                .checkout(repo, to)
                .await?
                .into_iter()
                .map(|f| f.path)
                .collect()),
        }
    }

    async fn message(&self, repo: &Repo, id: &ObjectId) -> Result<String> {
        Ok(self.inner.commit(repo, id).await?.message)
    }

    async fn is_ancestor(&self, repo: &Repo, a: &ObjectId, b: &ObjectId) -> Result<bool> {
        self.inner.is_ancestor(repo, a, b).await
    }

    async fn count_between(
        &self,
        repo: &Repo,
        from: Option<&ObjectId>,
        to: &ObjectId,
        limit: u64,
    ) -> Result<u64> {
        let mut n = 0;
        let mut cur = Some(to.clone());
        while let Some(c) = cur {
            if from.is_some_and(|f| *f == c) || n > limit {
                break;
            }
            n += 1;
            cur = self.inner.commit(repo, &c).await?.parents.first().cloned();
        }
        Ok(n)
    }

    async fn first_parent(&self, repo: &Repo, id: &ObjectId) -> Result<Option<ObjectId>> {
        Ok(self.inner.commit(repo, id).await?.parents.first().cloned())
    }

    async fn set_ref_if(
        &self,
        repo: &Repo,
        name: &str,
        expected: Option<&ObjectId>,
        target: &ObjectId,
    ) -> Result<bool> {
        self.inner.set_ref_if(repo, name, expected, target).await
    }

    async fn checkout(&self, repo: &Repo, at: &ObjectId) -> Result<Vec<TreeFile>> {
        let h = self.inner.history(repo).await?;
        let root = *h.changeset(&core_id(at)?).await.map_err(vcs_err)?.root();
        let mut out = Vec::new();
        let mut stack = vec![(String::new(), root)];
        while let Some((prefix, tree)) = stack.pop() {
            let t = h.tree(&tree).await.map_err(vcs_err)?;
            for e in t.entries() {
                let name = String::from_utf8_lossy(&e.name).into_owned();
                let path = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                match e.mode {
                    Mode::Tree => stack.push((path, e.id)),
                    Mode::File | Mode::Executable | Mode::Symlink => {
                        if out.len() >= MAX_CHECKOUT_FILES {
                            return Err(ForgeError::too_large(
                                "checkout_files",
                                MAX_CHECKOUT_FILES as u64,
                                out.len() as u64 + 1,
                            ));
                        }
                        let bytes = match h.blob(&e.id).await.map_err(vcs_err)? {
                            bee_history::BlobContent::Bytes(b) => b,
                            bee_history::BlobContent::Redacted => Vec::new(),
                        };
                        out.push(TreeFile {
                            path,
                            bytes,
                            executable: e.mode == Mode::Executable,
                        });
                    }
                    Mode::Gitlink => {}
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
}
