//! Forks on the server (`docs/spec/forks.md`): the [`ForkVcs`] port over the server's VCS
//! stores and write path.
//!
//! A fork's VCS repo is created in the object namespace (VCS org) of its upstream under the
//! name `<name>~<fork id>`, with its own empty root, and given the upstream's history by
//! [`copy_changesets`] (changeset and `object_paths` rows only). Its ref moves append the same
//! `ref.updated` VCS event as `RefService` and wake the forge's event bridge, so CI,
//! webhooks and change requests that follow the fork's refs see them. The server resolves
//! `org/name` of a fork through the forge ([`super::ForgeWiring::fork_full_name`]) because
//! the fork's VCS repo is named in its network's namespace.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use bee_core::{ObjectId, RefName};
use bee_forge::ForgeError;
use bee_forge::forks::{ForkVcs, copy_changesets, vcs_name};
use bee_proto::v1::{self, Ref, Repo};
use bee_store::{Expected, RefStep, RefUpdate, RepoRow};
use serde_json::json;

use super::adapters::repo_ctx;
use super::fe;
use crate::events::new_event;
use crate::state::{AppState, RepoCtx};

type FResult<T> = bee_forge::Result<T>;

fn core_id(id: &v1::ObjectId) -> FResult<ObjectId> {
    ObjectId::from_bytes(&id.raw).map_err(|e| ForgeError::invalid("invalid_object_id", e.message))
}

fn proto_id(id: &ObjectId) -> v1::ObjectId {
    v1::ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

/// [`ForkVcs`] over the server's stores.
pub struct ServerForkVcs {
    state: Weak<AppState>,
}

impl ServerForkVcs {
    /// Over `state`.
    pub fn new(state: &Arc<AppState>) -> ServerForkVcs {
        ServerForkVcs {
            state: Arc::downgrade(state),
        }
    }

    fn state(&self) -> FResult<Arc<AppState>> {
        self.state
            .upgrade()
            .ok_or_else(|| ForgeError::unavailable("server stopping"))
    }

    async fn copy(
        state: &AppState,
        from: &RepoCtx,
        to: &RepoCtx,
        heads: &[ObjectId],
    ) -> FResult<u64> {
        if from.org.id != to.org.id {
            return Err(ForgeError::invalid(
                "not_in_network",
                format!(
                    "{} and {} are not in one fork network",
                    from.full_name, to.full_name
                ),
            ));
        }
        copy_changesets(
            state.meta.as_ref(),
            &state.writer,
            from.repo.id,
            &state.history(to),
            heads,
            state.now_ms(),
        )
        .await
    }
}

#[async_trait]
impl ForkVcs for ServerForkVcs {
    async fn create(&self, upstream: &Repo, fork: &Repo, refs: &[String]) -> FResult<Vec<Ref>> {
        let state = self.state()?;
        let up = repo_ctx(&state, &upstream.id).await?;
        let id = bee_core::Ulid::parse(&fork.id)
            .map_err(|e| ForgeError::invalid("invalid_id", e.message))?;
        if state
            .meta
            .get_repo(bee_store::RepoId(id))
            .await
            .map_err(fe)?
            .is_none()
        {
            let row = RepoRow {
                id: bee_store::RepoId(id),
                org: up.org.id,
                name: vcs_name(fork),
                created_at_ms: state.now_ms(),
                draft_retention_days: up.repo.draft_retention_days,
            };
            crate::server::create_repo(&state, &up.org, &row, false)
                .await
                .map_err(fe)?;
        }
        let dst = repo_ctx(&state, &fork.id).await?;
        let mut out = Vec::new();
        for name in refs {
            let n = RefName::new(name.as_str()).map_err(fe)?;
            let Some(tip) = state
                .meta
                .get_ref(up.repo.id, &n)
                .await
                .map_err(fe)?
                .and_then(|r| r.target)
            else {
                continue;
            };
            Self::copy(&state, &up, &dst, &[tip]).await?;
            let cur = state.meta.get_ref(dst.repo.id, &n).await.map_err(fe)?;
            if cur.as_ref().and_then(|r| r.target) == Some(tip) {
                continue;
            }
            out.push(
                self.move_ref(
                    fork,
                    name,
                    cur.map(|r| r.version).unwrap_or(0),
                    Some(&proto_id(&tip)),
                    "forge",
                )
                .await?,
            );
        }
        Ok(out)
    }

    async fn pull(&self, from: &Repo, to: &Repo, heads: &[v1::ObjectId]) -> FResult<u64> {
        let state = self.state()?;
        let (src, dst) = (
            repo_ctx(&state, &from.id).await?,
            repo_ctx(&state, &to.id).await?,
        );
        let heads: Vec<ObjectId> = heads.iter().map(core_id).collect::<FResult<_>>()?;
        Self::copy(&state, &src, &dst, &heads).await
    }

    async fn refs(&self, repo: &Repo) -> FResult<Vec<Ref>> {
        let state = self.state()?;
        let ctx = repo_ctx(&state, &repo.id).await?;
        Ok(state
            .meta
            .list_refs(ctx.repo.id, "")
            .await
            .map_err(fe)?
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
        target: Option<&v1::ObjectId>,
        principal: &str,
    ) -> FResult<Ref> {
        let state = self.state()?;
        let ctx = repo_ctx(&state, &repo.id).await?;
        let n = RefName::new(name).map_err(fe)?;
        let target = target.map(core_id).transpose()?;
        let cur = state.meta.get_ref(ctx.repo.id, &n).await.map_err(fe)?;
        let expected = match expected {
            0 => match &cur {
                Some(r) if r.target.is_none() => Expected::Version(r.version),
                _ => Expected::Absent,
            },
            v => Expected::Version(v),
        };
        let version_after = match expected {
            Expected::Version(v) => v + 1,
            Expected::Absent => 1,
        };
        let now = state.now_ms();
        let out = state
            .writer
            .update_ref(RefUpdate {
                repo: ctx.repo.id,
                name: n,
                expected,
                steps: vec![RefStep {
                    target,
                    landed: Vec::new(),
                }],
                principal: principal.to_string(),
                at_ms: now,
                events: vec![new_event(
                    "ref.updated",
                    &json!({
                        "ref": name,
                        "old": cur.and_then(|r| r.target).map(|o| o.to_text()),
                        "new": target.map(|t| t.to_text()),
                        "version": version_after,
                        "principal_chain": [principal],
                        "reason": "fork",
                    }),
                    now,
                )],
            })
            .await
            .map_err(fe)?;
        if let Some(w) = state.hub.get() {
            w.touch(&ctx);
        }
        Ok(Ref {
            name: name.to_string(),
            target: target.map(|t| proto_id(&t)),
            version: out.new_version,
        })
    }
}
