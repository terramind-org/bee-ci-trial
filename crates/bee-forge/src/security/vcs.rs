//! [`HistorySecurityVcs`]: the [`SecurityVcs`] port over `bee-history` (reads only).

use async_trait::async_trait;
use bee_core::{Mode, ObjectId as CoreId, RefName, Ulid};
use bee_history::{BlobContent, History, LogOptions};
use bee_object::DeltaOp;
use bee_proto::v1::{ObjectId, Repo};
use bee_store::{RepoId, Writer};

use super::ports::{AddedBlob, SecurityVcs, TreeFile};
use crate::changes::vcs::vcs_err;
use crate::error::{ForgeError, Result};

/// Largest blob read for scanning.
const MAX_BLOB: u64 = 4 << 20;

/// Reads repo content through a [`Writer`] (the history layer's store handle).
pub struct HistorySecurityVcs {
    writer: Writer,
}

impl HistorySecurityVcs {
    /// Over `writer`.
    pub fn new(writer: Writer) -> HistorySecurityVcs {
        HistorySecurityVcs { writer }
    }

    async fn history(&self, repo: &Repo) -> Result<History> {
        let org = crate::forks::vcs_namespace(self.writer.meta().as_ref(), repo).await?;
        let id = Ulid::parse(&repo.id).map_err(|e| ForgeError::invalid("invalid_id", e.message))?;
        Ok(History::new(self.writer.clone(), org, RepoId(id)))
    }
}

fn core_id(id: &ObjectId) -> Result<CoreId> {
    CoreId::from_bytes(&id.raw).map_err(|e| ForgeError::invalid("invalid_object_id", e.message))
}

fn proto_id(id: &CoreId) -> ObjectId {
    ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

#[async_trait]
impl SecurityVcs for HistorySecurityVcs {
    async fn resolve(&self, repo: &Repo, expr: &str) -> Result<ObjectId> {
        let h = self.history(repo).await?;
        let expr = if expr.is_empty() {
            &repo.default_ref
        } else {
            expr
        };
        if let Some(raw) = crate::ids::unhex(expr)
            && !raw.is_empty()
            && let Ok(id) = CoreId::from_bytes(&raw)
            && h.has_changeset(&id).await.map_err(vcs_err)?
        {
            return Ok(proto_id(&id));
        }
        let on_ref = RefName::new(&repo.default_ref).ok();
        let id = h
            .resolve_str(expr, on_ref.as_ref())
            .await
            .map_err(vcs_err)?;
        Ok(proto_id(&id))
    }

    async fn list_files(&self, repo: &Repo, at: &ObjectId) -> Result<Vec<TreeFile>> {
        let h = self.history(repo).await?;
        let root = *h.changeset(&core_id(at)?).await.map_err(vcs_err)?.root();
        let leaves = h.list_leaves(&root, None).await.map_err(vcs_err)?;
        Ok(leaves
            .into_iter()
            .filter(|(_, e)| matches!(e.mode, Mode::File | Mode::Executable))
            .map(|(p, e)| TreeFile {
                path: p.to_string(),
                size: e.size,
            })
            .collect())
    }

    async fn read_file(&self, repo: &Repo, at: &ObjectId, path: &str) -> Result<Option<Vec<u8>>> {
        let h = self.history(repo).await?;
        let root = *h.changeset(&core_id(at)?).await.map_err(vcs_err)?.root();
        let rp = bee_core::RepoPath::new(path.as_bytes().to_vec())
            .map_err(|e| ForgeError::invalid("invalid_path", e.message))?;
        let Some(e) = h.entry_at(&root, &rp).await.map_err(vcs_err)? else {
            return Ok(None);
        };
        if !matches!(e.mode, Mode::File | Mode::Executable) {
            return Ok(None);
        }
        match h.blob(&e.id).await.map_err(vcs_err)? {
            BlobContent::Bytes(b) => Ok(Some(b)),
            BlobContent::Redacted => Ok(None),
        }
    }

    async fn added_blobs(&self, repo: &Repo, at: &ObjectId) -> Result<Vec<AddedBlob>> {
        let h = self.history(repo).await?;
        let cs = h.changeset(&core_id(at)?).await.map_err(vcs_err)?;
        let new_root = *cs.root();
        let old_root = match cs.parents().first() {
            Some(p) => Some(*h.changeset(p).await.map_err(vcs_err)?.root()),
            None => None,
        };
        let delta = h
            .diff_trees(old_root.as_ref(), Some(&new_root), None)
            .await
            .map_err(vcs_err)?;
        let mut out = Vec::new();
        for d in delta {
            let DeltaOp::Put { mode, id, size } = d.op else {
                continue;
            };
            if !matches!(mode, Mode::File | Mode::Executable) || size > MAX_BLOB {
                continue;
            }
            if let BlobContent::Bytes(bytes) = h.blob(&id).await.map_err(vcs_err)? {
                out.push(AddedBlob {
                    path: d.path.to_string(),
                    blob: proto_id(&id),
                    bytes,
                });
            }
        }
        Ok(out)
    }

    async fn changed_paths(
        &self,
        repo: &Repo,
        from: &ObjectId,
        to: &ObjectId,
    ) -> Result<Option<Vec<String>>> {
        let h = self.history(repo).await?;
        let old = *h.changeset(&core_id(from)?).await.map_err(vcs_err)?.root();
        let new = *h.changeset(&core_id(to)?).await.map_err(vcs_err)?.root();
        // Equal subtrees are skipped unread, so this costs the size of the difference.
        let delta = h
            .diff_trees(Some(&old), Some(&new), None)
            .await
            .map_err(vcs_err)?;
        Ok(Some(
            delta.into_iter().map(|d| d.path.to_string()).collect(),
        ))
    }

    async fn history(&self, repo: &Repo, tip: &ObjectId, limit: usize) -> Result<Vec<ObjectId>> {
        let h = self.history(repo).await?;
        let mut out = Vec::new();
        let mut token = None;
        while out.len() < limit {
            let page = h
                .log(
                    &core_id(tip)?,
                    &LogOptions {
                        limit: (limit - out.len()).min(500),
                        page_token: token.clone(),
                        ..LogOptions::default()
                    },
                )
                .await
                .map_err(vcs_err)?;
            out.extend(page.entries.iter().map(|e| proto_id(&e.id)));
            match page.next_page_token {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        Ok(out)
    }
}
