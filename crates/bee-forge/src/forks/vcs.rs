//! What forks need from the VCS ([`ForkVcs`]) and the reference implementation over a
//! [`bee_store::Writer`] ([`HistoryForkVcs`]).
//!
//! A fork's VCS repo lives in the object namespace (the VCS org) of its network's root, so
//! the objects every member shares are stored once and never copied (SPEC §6.1). What a fork
//! gets is its own changeset rows and `object_paths` rows (SPEC §9.3), so content is readable
//! through a repo only when one of that repo's changesets references it: a fork never makes
//! content that exists only in the fork readable through its parent, or the reverse.
//! [`copy_changesets`] is the one routine that moves history between members; it rebuilds each
//! changeset from its parents, meta, ChangeId and delta, which yields the same id and writes
//! no object bytes that are not already stored.

use std::collections::BTreeSet;

use async_trait::async_trait;
use bee_core::{ObjectId as CoreId, RefName, Ulid};
use bee_history::History;
use bee_proto::v1::{ObjectId, Ref, Repo};
use bee_store::{Expected, MetaStore, OrgId, ReadFrom, RefUpdate, RepoId, RepoRow, Writer};

use crate::changes::vcs::vcs_err;
use crate::error::{ForgeError, Result};
use crate::ids::Clock;

/// Separator between a fork's name and its id in its VCS repo name (VCS names are unique per
/// namespace, and a network can hold several forks with the same forge name).
pub const VCS_NAME_SEP: char = '~';

/// The VCS repo name of a fork: `<name>~<id in lower case>`.
pub fn vcs_name(fork: &Repo) -> String {
    format!(
        "{}{VCS_NAME_SEP}{}",
        fork.name,
        fork.id.to_ascii_lowercase()
    )
}

/// What forks need from the VCS.
#[async_trait]
pub trait ForkVcs: Send + Sync {
    /// Creates `fork`'s VCS repo in `upstream`'s object namespace and gives it the history
    /// of the listed `refs` of `upstream` (changeset and `object_paths` rows only; no object
    /// is copied). Returns the refs created, with their tips. Idempotent by fork id.
    async fn create(&self, upstream: &Repo, fork: &Repo, refs: &[String]) -> Result<Vec<Ref>>;
    /// Copies the changesets reachable from `heads` that `to` lacks from `from` into `to`
    /// (both in one namespace, `INVALID_ARGUMENT` otherwise). Returns how many were copied.
    async fn pull(&self, from: &Repo, to: &Repo, heads: &[ObjectId]) -> Result<u64>;
    /// Live refs of a repo (deleted refs omitted), sorted by name.
    async fn refs(&self, repo: &Repo) -> Result<Vec<Ref>>;
    /// Moves `name` to `target` (`None` deletes) if its version is `expected` (0 = absent),
    /// through the write path (ref events, CI, webhooks), attributed to `principal`.
    async fn move_ref(
        &self,
        repo: &Repo,
        name: &str,
        expected: u64,
        target: Option<&ObjectId>,
        principal: &str,
    ) -> Result<Ref>;
}

fn ulid(s: &str) -> Result<Ulid> {
    Ulid::parse(s).map_err(|e| ForgeError::invalid("invalid_id", e.message))
}

pub(crate) fn core_id(id: &ObjectId) -> Result<CoreId> {
    CoreId::from_bytes(&id.raw).map_err(|e| ForgeError::invalid("invalid_object_id", e.message))
}

pub(crate) fn proto_id(id: &CoreId) -> ObjectId {
    ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

/// The VCS org (object namespace) a forge repo's history lives in: its org, or for a fork
/// the namespace recorded on its VCS repo row (the network root's org).
pub async fn vcs_namespace(meta: &dyn MetaStore, repo: &Repo) -> Result<OrgId> {
    if repo.fork_parent_id.is_empty() {
        return Ok(OrgId(ulid(&repo.org_id)?));
    }
    let row = meta
        .get_repo(RepoId(ulid(&repo.id)?))
        .await
        .map_err(vcs_err)?
        .ok_or_else(|| ForgeError::not_found(format!("VCS repo of {}", repo.full_name)))?;
    Ok(row.org)
}

/// Copies into `dst` every changeset reachable from `heads` in `src` that `dst` does not
/// have, parents first. Each changeset is rebuilt from its parents, meta, ChangeId and delta
/// in `dst` (same namespace, so every object it names is already stored) and must come out
/// with its original id. Returns how many were copied.
pub async fn copy_changesets(
    meta: &dyn MetaStore,
    writer: &Writer,
    src: RepoId,
    dst: &History,
    heads: &[CoreId],
    now_ms: u64,
) -> Result<u64> {
    // Iterative post-order walk: a changeset is copied after all of its parents.
    let mut done: BTreeSet<CoreId> = BTreeSet::new();
    let mut stack: Vec<(CoreId, bool)> = heads.iter().map(|h| (*h, false)).collect();
    let mut copied = 0u64;
    while let Some((id, expanded)) = stack.pop() {
        if done.contains(&id) {
            continue;
        }
        if !expanded
            && meta
                .get_changeset(dst.repo(), &id, ReadFrom::Primary)
                .await
                .map_err(vcs_err)?
                .is_some()
        {
            done.insert(id);
            continue;
        }
        let row = meta
            .get_changeset(src, &id, ReadFrom::Primary)
            .await
            .map_err(vcs_err)?
            .ok_or_else(|| ForgeError::not_found(format!("changeset {id}")))?;
        let cs = row.changeset;
        if !expanded {
            stack.push((id, true));
            for p in cs.parents() {
                if !done.contains(p) {
                    stack.push((*p, false));
                }
            }
            continue;
        }
        let built = writer
            .build(
                dst.org(),
                dst.repo(),
                cs.parents().to_vec(),
                *cs.meta(),
                *cs.change_id(),
                cs.delta().to_vec(),
            )
            .await
            .map_err(vcs_err)?;
        if built.id() != id {
            return Err(ForgeError::internal(format!(
                "changeset {id} rebuilt as {} while copying between fork network members",
                built.id()
            )));
        }
        dst.store_built(&built, now_ms).await.map_err(vcs_err)?;
        done.insert(id);
        copied += 1;
    }
    Ok(copied)
}

/// Whether `name` is a ref a new fork copies: branches and tags, not change request heads
/// (`pull/`), git-only refs (`git/`) or other principals' workspaces.
pub fn forkable_ref(name: &str) -> bool {
    let Ok(r) = RefName::new(name) else {
        return false;
    };
    !(name.starts_with("pull/") || name.starts_with("git/") || r.workspace_owner().is_some())
}

/// [`ForkVcs`] over a [`Writer`]: rows straight in the metadata store and plain ref
/// compare-and-swaps that emit no VCS events (forge tests and tools; the server installs an
/// implementation whose ref moves go through its write path).
pub struct HistoryForkVcs {
    writer: Writer,
    clock: std::sync::Arc<dyn Clock>,
}

impl HistoryForkVcs {
    /// Over `writer`, timing rows with `clock`.
    pub fn new(writer: Writer, clock: std::sync::Arc<dyn Clock>) -> HistoryForkVcs {
        HistoryForkVcs { writer, clock }
    }

    async fn row(&self, repo: &Repo) -> Result<RepoRow> {
        self.writer
            .meta()
            .get_repo(RepoId(ulid(&repo.id)?))
            .await
            .map_err(vcs_err)?
            .ok_or_else(|| ForgeError::not_found(format!("VCS repo of {}", repo.full_name)))
    }

    async fn tip(&self, repo: RepoId, name: &str) -> Result<Option<(CoreId, u64)>> {
        let n = RefName::new(name).map_err(|e| ForgeError::invalid("invalid_ref", e.message))?;
        Ok(self
            .writer
            .meta()
            .get_ref(repo, &n)
            .await
            .map_err(vcs_err)?
            .and_then(|r| r.target.map(|t| (t, r.version))))
    }
}

#[async_trait]
impl ForkVcs for HistoryForkVcs {
    async fn create(&self, upstream: &Repo, fork: &Repo, refs: &[String]) -> Result<Vec<Ref>> {
        let up = self.row(upstream).await?;
        let now = self.clock.now_ms();
        let row = RepoRow {
            id: RepoId(ulid(&fork.id)?),
            org: up.org,
            name: vcs_name(fork),
            created_at_ms: now,
            draft_retention_days: bee_store::DEFAULT_DRAFT_RETENTION_DAYS,
        };
        self.writer
            .meta()
            .create_repo(row.clone())
            .await
            .map_err(vcs_err)?;
        let dst = History::new(self.writer.clone(), row.org, row.id);
        let mut out = Vec::new();
        for name in refs {
            let Some((tip, _)) = self.tip(up.id, name).await? else {
                continue;
            };
            copy_changesets(
                self.writer.meta().as_ref(),
                &self.writer,
                up.id,
                &dst,
                &[tip],
                now,
            )
            .await?;
            let cur = self.tip(row.id, name).await?;
            if cur.is_some_and(|(t, _)| t == tip) {
                continue;
            }
            out.push(
                self.move_ref(
                    fork,
                    name,
                    cur.map(|(_, v)| v).unwrap_or(0),
                    Some(&proto_id(&tip)),
                    "forge",
                )
                .await?,
            );
        }
        Ok(out)
    }

    async fn pull(&self, from: &Repo, to: &Repo, heads: &[ObjectId]) -> Result<u64> {
        let (src, dst) = (self.row(from).await?, self.row(to).await?);
        if src.org != dst.org {
            return Err(ForgeError::invalid(
                "not_in_network",
                format!(
                    "{} and {} are not in one fork network",
                    from.full_name, to.full_name
                ),
            ));
        }
        let heads: Vec<CoreId> = heads.iter().map(core_id).collect::<Result<_>>()?;
        let h = History::new(self.writer.clone(), dst.org, dst.id);
        copy_changesets(
            self.writer.meta().as_ref(),
            &self.writer,
            src.id,
            &h,
            &heads,
            self.clock.now_ms(),
        )
        .await
    }

    async fn refs(&self, repo: &Repo) -> Result<Vec<Ref>> {
        let row = self.row(repo).await?;
        Ok(self
            .writer
            .meta()
            .list_refs(row.id, "")
            .await
            .map_err(vcs_err)?
            .into_iter()
            .filter_map(|r| {
                r.target.map(|t| Ref {
                    name: r.name.as_str().to_string(),
                    target: Some(proto_id(&t)),
                    version: r.version,
                })
            })
            .collect())
    }

    async fn move_ref(
        &self,
        repo: &Repo,
        name: &str,
        expected: u64,
        target: Option<&ObjectId>,
        principal: &str,
    ) -> Result<Ref> {
        let row = self.row(repo).await?;
        let n = RefName::new(name).map_err(|e| ForgeError::invalid("invalid_ref", e.message))?;
        let target = target.map(core_id).transpose()?;
        let expected = match expected {
            // Absent: no row, or a deleted ref (which keeps its numbering).
            0 => match self
                .writer
                .meta()
                .get_ref(row.id, &n)
                .await
                .map_err(vcs_err)?
            {
                Some(r) if r.target.is_none() => Expected::Version(r.version),
                _ => Expected::Absent,
            },
            v => Expected::Version(v),
        };
        let out = self
            .writer
            .update_ref(RefUpdate::set(
                row.id,
                n,
                expected,
                target,
                principal,
                self.clock.now_ms(),
            ))
            .await
            .map_err(vcs_err)?;
        Ok(Ref {
            name: name.to_string(),
            target: target.map(|t| proto_id(&t)),
            version: out.new_version,
        })
    }
}
