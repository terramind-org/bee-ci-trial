//! Forks (SPEC §11.1, `docs/spec/forks.md`): fork networks that share one object namespace,
//! fork sync, change requests from a fork to its upstream, maintainers' edits, and the
//! visibility rules of private forks. Serves `RepoService.ForkRepo`, `ListForks` and
//! `SyncFork` through [`crate::service`].
//!
//! # Layout
//!
//! | file | owns |
//! |---|---|
//! | `mod.rs` | the module declaration, [`Forks`] (create, list, network, sync, cross-fork change request plumbing), [`ForkAccess`] |
//! | [`vcs`] | what forks need from the VCS ([`ForkVcs`]), the [`HistoryForkVcs`] reference implementation and [`copy_changesets`] |
//! | [`gate`] | [`ModerationGate`]: CI approval of fork runs from enterprise moderation's first-time contributor rules |
//!
//! # Storage
//!
//! The `forks` surface holds the network ([`FORKS`]: one row per fork with its parent and the
//! network root) and the change requests that follow a fork's ref ([`FORK_CHANGES`]). The
//! fork's repo row itself is an ordinary orgs row with `fork_parent_id` set.

pub mod gate;
pub mod vcs;

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use bee_proto::v1::{
    ChangeState, Event, ObjectId, Ref, Repo, RepoRef, RepoRole, SubjectKind, Visibility, event,
    repo_ref,
};
use serde_json::json;

pub use gate::ModerationGate;
pub use vcs::{
    ForkVcs, HistoryForkVcs, VCS_NAME_SEP, copy_changesets, forkable_ref, vcs_name, vcs_namespace,
};

use crate::audit::AuditDraft;
use crate::changes::vcs::{ChangeVcs, Rebased, merge_conflict};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::events::EventListener;
use crate::forge::Forge;
use crate::ids::{hex, ts};
use crate::module::{EventKind, ForgeModule};
use crate::orgs::repos::{NewRepo, RepoPatch};
use crate::orgs::{AccessSource, Orgs};
use crate::store::{
    Cursor, Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, Surface, TableDef,
};

/// The forks surface.
pub const SURFACE: Surface = Surface("forks");

/// Fork network membership: global scope, id the fork's repo id, `k1` the parent repo id,
/// `k2` the network root's repo id, `n1` the creation time (ms). Body [`ForkRecord`].
pub static FORKS: TableDef = TableDef {
    name: "forge_forks",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::K1, Index::K2],
};

/// Change requests whose head follows a fork's ref: scope the fork's repo id, id the change
/// request id, `k1` the fork ref. Body [`ForkChange`].
pub static FORK_CHANGES: TableDef = TableDef {
    name: "forge_fork_changes",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

static M1: Migration = Migration {
    module: "forks",
    version: 1,
    name: "create_forks",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&FORKS),
        MigrationOp::CreateTable(&FORK_CHANGES),
    ],
};

/// What the forks module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "forks",
    surfaces: &[SURFACE],
    migrations: &[&M1],
    event_kinds: &[
        EventKind::gh("repo.forked", "A repo was forked", "fork", ""),
        EventKind::new(
            "fork.synced",
            "A fork's ref was brought up to date with its upstream",
        ),
    ],
    audit_prefixes: &["fork."],
};

/// A fork's place in its network.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ForkRecord {
    /// The fork's repo id.
    #[prost(string, tag = "1")]
    pub fork_id: String,
    /// The repo it was forked from.
    #[prost(string, tag = "2")]
    pub parent_id: String,
    /// The network root (the first repo of the chain of parents).
    #[prost(string, tag = "3")]
    pub root_id: String,
    /// Who forked it.
    #[prost(string, tag = "4")]
    pub created_by: String,
    /// When (ms).
    #[prost(uint64, tag = "5")]
    pub created_at_ms: u64,
}

/// A change request whose head follows a fork's ref.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ForkChange {
    /// The repo the change request targets.
    #[prost(string, tag = "1")]
    pub target_repo_id: String,
    /// The change request.
    #[prost(string, tag = "2")]
    pub change_request_id: String,
    /// The fork ref it follows.
    #[prost(string, tag = "3")]
    pub head_ref: String,
}

/// The result of a sync.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncOutcome {
    /// The fork ref after the sync.
    pub r#ref: Ref,
    /// Changesets copied from the upstream into the fork.
    pub pulled: u64,
}

/// Forks over the forge's orgs and the VCS ports.
pub struct Forks {
    db: Db,
    orgs: Arc<Orgs>,
    vcs: Arc<dyn ForkVcs>,
    history: Arc<dyn ChangeVcs>,
}

fn rref(id: &str) -> RepoRef {
    RepoRef {
        repo: Some(repo_ref::Repo::Id(id.to_string())),
    }
}

fn is_public(repo: &Repo) -> bool {
    repo.visibility == Visibility::Public as i32
}

fn default_ref(repo: &Repo) -> String {
    if repo.default_ref.is_empty() {
        "main".to_string()
    } else {
        repo.default_ref.clone()
    }
}

impl Forks {
    /// Forks over `forge`'s orgs, creating and copying through `vcs` and reading history
    /// (stacks, rebases, ref tips) through `history`.
    pub fn new(forge: &Forge, vcs: Arc<dyn ForkVcs>, history: Arc<dyn ChangeVcs>) -> Arc<Forks> {
        Arc::new(Forks {
            db: forge.db.clone(),
            orgs: forge.orgs.clone(),
            vcs,
            history,
        })
    }

    /// Installs the module in `forge`: the fork RPCs, read access to private forks for the
    /// upstream's maintainers ([`ForkAccess`]) and change requests that follow pushes to
    /// their fork ref.
    pub fn attach(self: &Arc<Self>, forge: &Arc<Forge>) {
        forge.set_forks(self.clone());
        forge
            .orgs
            .add_access_source(Arc::new(ForkAccess(Arc::downgrade(&forge.orgs))));
        forge.events.add_listener(Arc::new(FollowForkPushes {
            forks: Arc::downgrade(self),
            forge: Arc::downgrade(forge),
        }));
    }

    /// The VCS port.
    pub fn vcs(&self) -> &Arc<dyn ForkVcs> {
        &self.vcs
    }

    /// A fork's network record; `None` for repos that are not forks.
    pub async fn record(&self, cx: &Cx, repo_id: &str) -> Result<Option<ForkRecord>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .get_msg::<ForkRecord>(&FORKS, "", repo_id)
            .await?
            .map(|(r, _)| r))
    }

    /// The id of the network root `repo` belongs to (itself unless it is a fork).
    pub async fn root_of(&self, cx: &Cx, repo: &Repo) -> Result<String> {
        if repo.fork_parent_id.is_empty() {
            return Ok(repo.id.clone());
        }
        Ok(self
            .record(cx, &repo.id)
            .await?
            .map(|r| r.root_id)
            .unwrap_or_else(|| repo.fork_parent_id.clone()))
    }

    /// Whether `a` and `b` are members of one fork network.
    pub async fn same_network(&self, cx: &Cx, a: &Repo, b: &Repo) -> Result<bool> {
        Ok(a.id == b.id || self.root_of(cx, a).await? == self.root_of(cx, b).await?)
    }

    /// Forks `upstream` into org `org_id` as `name` (empty = the upstream's name). The fork
    /// shares the upstream's object namespace; it gets the history of the upstream's default
    /// ref (`default_ref_only`) or of every branch and tag. Public repos fork public, private
    /// and internal ones private. Asking again for a fork the org already has returns it.
    /// The caller has authorized the read of `upstream` and repo creation in `org_id`.
    pub async fn fork(
        &self,
        cx: &Cx,
        upstream: &Repo,
        org_id: &str,
        name: &str,
        default_ref_only: bool,
    ) -> Result<Repo> {
        for existing in self.list_all(cx, &upstream.id).await? {
            if existing.org_id == org_id {
                return Ok(existing);
            }
        }
        let name = if name.is_empty() {
            upstream.name.clone()
        } else {
            name.to_string()
        };
        crate::orgs::validate_name(&name)?;
        let org = self.orgs.org_any(cx, org_id).await?;
        if self
            .orgs
            .resolve_repo(
                cx,
                &RepoRef {
                    repo: Some(repo_ref::Repo::FullName(format!("{}/{name}", org.name))),
                },
            )
            .await
            .is_ok()
        {
            return Err(ForgeError::already_exists(format!(
                "repo {}/{name}",
                org.name
            )));
        }
        let visibility = if is_public(upstream) {
            Visibility::Public
        } else {
            Visibility::Private
        };
        let id = self.db.ids().ulid()?;
        let base_ref = default_ref(upstream);
        let draft = Repo {
            id: id.clone(),
            org_id: org_id.to_string(),
            name: name.clone(),
            full_name: format!("{}/{name}", org.name),
            visibility: visibility as i32,
            default_ref: base_ref.clone(),
            fork_parent_id: upstream.id.clone(),
            ..Default::default()
        };
        let refs: Vec<String> = if default_ref_only {
            vec![base_ref.clone()]
        } else {
            self.vcs
                .refs(upstream)
                .await?
                .into_iter()
                .map(|r| r.name)
                .filter(|n| forkable_ref(n))
                .collect()
        };
        // The fork port publishes refs without going through RefService.
        let fence = crate::safety::control::fence_nonhuman(&self.db, cx, org_id).await?;
        self.vcs.create(upstream, &draft, &refs).await?;
        drop(fence);
        let mut repo = self
            .orgs
            .create_repo(
                cx,
                org_id,
                NewRepo {
                    id: id.clone(),
                    name,
                    visibility,
                    description: upstream.description.clone(),
                    is_template: false,
                    topics: upstream.topics.clone(),
                    homepage: upstream.homepage.clone(),
                    template_repo_id: String::new(),
                    fork_parent_id: upstream.id.clone(),
                },
            )
            .await?;
        if repo.default_ref != base_ref {
            repo = self
                .orgs
                .update_repo(
                    cx,
                    &id,
                    RepoPatch {
                        default_ref: Some(base_ref),
                        ..Default::default()
                    },
                    0,
                )
                .await?;
        }
        let root = self.root_of(cx, upstream).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let rec = ForkRecord {
            fork_id: id.clone(),
            parent_id: upstream.id.clone(),
            root_id: root.clone(),
            created_by: cx.actor_id.clone(),
            created_at_ms: now,
        };
        tx.insert(
            &FORKS,
            &Row::new("", id.clone())
                .k1(upstream.id.clone())
                .k2(root)
                .n1(now as i64)
                .msg(&rec),
        )
        .await?;
        let payload = json!({
            "forkee": {"id": repo.id, "full_name": repo.full_name, "visibility": visibility.as_str_name()},
            "refs": refs,
        })
        .to_string();
        tx.emit_event(Event {
            kind: "repo.forked".into(),
            repo_id: upstream.id.clone(),
            org_id: upstream.org_id.clone(),
            time: Some(ts(now)),
            payload_json: payload.clone(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new("fork.create", &upstream.org_id)
                .repo_id(&upstream.id)
                .target(SubjectKind::Repo, &repo.id)
                .after(&payload),
        )
        .await?;
        tx.commit().await?;
        Ok(repo)
    }

    async fn rows(&self, cx: &Cx, key: Key, after: &str, limit: usize) -> Result<Vec<Row>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let cursor = (!after.is_empty()).then(|| Cursor {
            n1: 0,
            scope: String::new(),
            id: after.to_string(),
        });
        tx.scan(&FORKS, &Scan::scope("", limit).key(key).after(cursor))
            .await
    }

    /// Live direct forks of `upstream_id`, by fork id, after `after`, at most `limit`, with
    /// the next page token (empty on the last page). Deleted forks are skipped.
    pub async fn list(
        &self,
        cx: &Cx,
        upstream_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Repo>, String)> {
        let rows = self
            .rows(cx, Key::K1(upstream_id.to_string()), after, limit + 1)
            .await?;
        let more = rows.len() > limit;
        let mut out = Vec::new();
        let mut last = String::new();
        for row in rows.into_iter().take(limit) {
            last = row.id.clone();
            if let Ok(r) = self.orgs.resolve_repo(cx, &rref(&row.id)).await {
                out.push(r);
            }
        }
        Ok((out, if more { last } else { String::new() }))
    }

    async fn list_all(&self, cx: &Cx, upstream_id: &str) -> Result<Vec<Repo>> {
        let mut out = Vec::new();
        let mut after = String::new();
        loop {
            let (page, next) = self.list(cx, upstream_id, &after, 500).await?;
            out.extend(page);
            if next.is_empty() {
                return Ok(out);
            }
            after = next;
        }
    }

    /// Every live member of `repo`'s fork network, the root first (when live), then forks by
    /// id. Visibility filtering is the caller's.
    pub async fn network(&self, cx: &Cx, repo: &Repo) -> Result<Vec<Repo>> {
        let root = self.root_of(cx, repo).await?;
        let mut out = Vec::new();
        if let Ok(r) = self.orgs.resolve_repo(cx, &rref(&root)).await {
            out.push(r);
        }
        let mut after = String::new();
        loop {
            let rows = self.rows(cx, Key::K2(root.clone()), &after, 501).await?;
            let more = rows.len() > 500;
            for row in rows.iter().take(500) {
                after = row.id.clone();
                if let Ok(r) = self.orgs.resolve_repo(cx, &rref(&row.id)).await {
                    out.push(r);
                }
            }
            if !more {
                return Ok(out);
            }
        }
    }

    /// The live parent of a fork (`INVALID_ARGUMENT` for a repo that is not a fork,
    /// `INVALID_STATE` when the parent was deleted).
    pub async fn parent(&self, cx: &Cx, fork: &Repo) -> Result<Repo> {
        if fork.fork_parent_id.is_empty() {
            return Err(ForgeError::invalid(
                "not_a_fork",
                format!("{} is not a fork", fork.full_name),
            ));
        }
        self.orgs
            .resolve_repo(cx, &rref(&fork.fork_parent_id))
            .await
            .map_err(|_| {
                ForgeError::invalid_state(format!(
                    "the upstream of {} was deleted; the fork keeps its history",
                    fork.full_name
                ))
            })
    }

    /// Brings fork ref `ref_name` (empty = the fork's default ref) up to date with the
    /// upstream's ref of the same name (the upstream's default ref for the fork's default
    /// ref). A fork ref behind the upstream fast-forwards; one with changesets of its own is
    /// rebased onto the upstream tip (`MERGE_CONFLICT` with the conflicts when that fails);
    /// one already containing the upstream tip is left alone.
    pub async fn sync(&self, cx: &Cx, fork: &Repo, ref_name: &str) -> Result<SyncOutcome> {
        let upstream = self.parent(cx, fork).await?;
        let fork_ref = if ref_name.is_empty() {
            default_ref(fork)
        } else {
            ref_name.to_string()
        };
        let up_ref = if fork_ref == default_ref(fork) {
            default_ref(&upstream)
        } else {
            fork_ref.clone()
        };
        let up_tip = self
            .history
            .ref_tip(&upstream, &up_ref)
            .await?
            .and_then(|r| r.target)
            .ok_or_else(|| {
                ForgeError::not_found(format!("ref {up_ref} in {}", upstream.full_name))
            })?;
        let pulled = self
            .vcs
            .pull(&upstream, fork, std::slice::from_ref(&up_tip))
            .await?;
        let cur = self.history.ref_tip(fork, &fork_ref).await?;
        let (old, version) = match &cur {
            Some(r) => (r.target.clone(), r.version),
            None => (None, 0),
        };
        let new = match &old {
            None => Some(up_tip.clone()),
            Some(o) if *o == up_tip => None,
            Some(o) if self.history.is_ancestor(fork, o, &up_tip).await? => Some(up_tip.clone()),
            Some(o) if self.history.is_ancestor(fork, &up_tip, o).await? => None,
            Some(o) => {
                let stack = self.history.stack(fork, o, &up_tip).await?;
                match self
                    .history
                    .rebase_stack(fork, &stack.members, &up_tip)
                    .await?
                {
                    Rebased::Head(h) => Some(h),
                    Rebased::Conflict(set) => return Err(merge_conflict(set)),
                }
            }
        };
        let r = match new {
            Some(target) => {
                let fence =
                    crate::safety::control::fence_nonhuman(&self.db, cx, &fork.org_id).await?;
                let r = self
                    .vcs
                    .move_ref(fork, &fork_ref, version, Some(&target), &cx.actor_id)
                    .await?;
                drop(fence);
                let mut tx = self.db.begin(SURFACE, cx).await?;
                let payload = json!({
                    "ref": fork_ref,
                    "upstream": upstream.full_name,
                    "upstream_ref": up_ref,
                    "old": old.as_ref().map(|o| hex(&o.raw)),
                    "new": hex(&target.raw),
                    "pulled": pulled,
                })
                .to_string();
                tx.emit_event(Event {
                    kind: "fork.synced".into(),
                    repo_id: fork.id.clone(),
                    org_id: fork.org_id.clone(),
                    payload_json: payload.clone(),
                    ..Default::default()
                })
                .await?;
                tx.audit(
                    AuditDraft::new("fork.sync", &fork.org_id)
                        .repo_id(&fork.id)
                        .target(SubjectKind::Repo, &fork.id)
                        .after(&payload),
                )
                .await?;
                tx.commit().await?;
                r
            }
            None => cur.unwrap_or_else(|| Ref {
                name: fork_ref.clone(),
                ..Default::default()
            }),
        };
        Ok(SyncOutcome { r#ref: r, pulled })
    }

    /// Copies the stack of `head` from `head_repo` into `target` so a change request in
    /// `target` can review, check and land it (both must be members of one fork network).
    pub async fn pull_head(
        &self,
        cx: &Cx,
        target: &Repo,
        head_repo: &Repo,
        head: &ObjectId,
    ) -> Result<u64> {
        if !self.same_network(cx, target, head_repo).await? {
            return Err(ForgeError::invalid(
                "not_in_network",
                format!(
                    "{} is not in the fork network of {}",
                    head_repo.full_name, target.full_name
                ),
            ));
        }
        self.vcs
            .pull(head_repo, target, std::slice::from_ref(head))
            .await
    }

    /// Records that change request `cr_id` in `target_repo_id` follows `head_ref` of fork
    /// `fork_id`: pushes to that ref revise it.
    pub async fn follow(
        &self,
        cx: &Cx,
        fork_id: &str,
        head_ref: &str,
        target_repo_id: &str,
        cr_id: &str,
    ) -> Result<()> {
        if head_ref.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.put(
            &FORK_CHANGES,
            &Row::new(fork_id, cr_id).k1(head_ref).msg(&ForkChange {
                target_repo_id: target_repo_id.to_string(),
                change_request_id: cr_id.to_string(),
                head_ref: head_ref.to_string(),
            }),
        )
        .await?;
        tx.commit().await
    }

    async fn unfollow(&self, cx: &Cx, fork_id: &str, cr_id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.delete(&FORK_CHANGES, fork_id, cr_id).await?;
        tx.commit().await
    }

    /// After a maintainer revised fork change request `cr` in `target` to `head`: copies the
    /// new stack into the fork and moves the fork's head ref to it, so the contributor's
    /// branch carries the maintainer's edit.
    pub async fn write_back(
        &self,
        cx: &Cx,
        target: &Repo,
        fork: &Repo,
        head_ref: &str,
        head: &ObjectId,
    ) -> Result<()> {
        if head_ref.is_empty() {
            return Ok(());
        }
        self.vcs
            .pull(target, fork, std::slice::from_ref(head))
            .await?;
        let cur = self.history.ref_tip(fork, head_ref).await?;
        if cur.as_ref().and_then(|r| r.target.as_ref()) == Some(head) {
            return Ok(());
        }
        let _fence = crate::safety::control::fence_nonhuman(&self.db, cx, &fork.org_id).await?;
        self.vcs
            .move_ref(
                fork,
                head_ref,
                cur.map(|r| r.version).unwrap_or(0),
                Some(head),
                &cx.actor_id,
            )
            .await
            .map(|_| ())
    }

    async fn on_fork_push(&self, forge: &Forge, ev: &Event, r: &Ref) -> Result<()> {
        let fork_id = &ev.repo_id;
        let Some(target) = r.target.clone().filter(|t| !t.raw.is_empty()) else {
            return Ok(());
        };
        let cx = Cx::system();
        let rows = {
            let mut tx = self.db.begin(SURFACE, &cx).await?;
            tx.scan_msgs::<ForkChange>(
                &FORK_CHANGES,
                &Scan::scope(fork_id, 100).key(Key::K1(r.name.clone())),
            )
            .await?
        };
        if rows.is_empty() {
            return Ok(());
        }
        let Some(changes) = forge.changes() else {
            return Ok(());
        };
        let fork = self.orgs.repo_any(&cx, fork_id).await?;
        for (fc, _) in rows {
            let Ok(repo) = self.orgs.resolve_repo(&cx, &rref(&fc.target_repo_id)).await else {
                self.unfollow(&cx, fork_id, &fc.change_request_id).await?;
                continue;
            };
            let cr = changes.get(&cx, &repo, &fc.change_request_id).await?;
            if cr.state != ChangeState::Open as i32 {
                self.unfollow(&cx, fork_id, &fc.change_request_id).await?;
                continue;
            }
            let head = cr.revisions.last().and_then(|v| v.head.clone());
            if head.as_ref() == Some(&target) {
                continue;
            }
            self.pull_head(&cx, &repo, &fork, &target).await?;
            // Safety and revision history must see the authenticated pusher, even when
            // a different principal opened the pull request from the fork.
            let pusher = changes.principal(&ev.principal).await;
            let acx = Cx::new(ev.principal.clone(), pusher.kind()).with_chain(
                std::iter::once(ev.principal.clone())
                    .chain(ev.on_behalf_of.clone())
                    .collect(),
            );
            changes.revise(&acx, &repo, &cr.id, &target).await?;
        }
        Ok(())
    }
}

/// Read access to private forks for the maintainers and admins of the fork's parent, as
/// GitHub gives the upstream's owners visibility into the private forks of their repos.
pub struct ForkAccess(Weak<Orgs>);

#[async_trait]
impl AccessSource for ForkAccess {
    async fn grants(
        &self,
        cx: &Cx,
        repo: &Repo,
        principal_id: &str,
    ) -> Result<Vec<(RepoRole, String)>> {
        if repo.fork_parent_id.is_empty() || is_public(repo) {
            return Ok(Vec::new());
        }
        let Some(orgs) = self.0.upgrade() else {
            return Ok(Vec::new());
        };
        let Ok(parent) = orgs.resolve_repo(cx, &rref(&repo.fork_parent_id)).await else {
            return Ok(Vec::new());
        };
        let role = orgs.effective_role(cx, &parent, principal_id).await?;
        let r = RepoRole::try_from(role.role).unwrap_or(RepoRole::None);
        Ok(if matches!(r, RepoRole::Maintain | RepoRole::Admin) {
            vec![(RepoRole::Read, format!("fork-network:{}", parent.full_name))]
        } else {
            Vec::new()
        })
    }
}

/// Revises the change requests that follow a fork ref when that ref moves.
struct FollowForkPushes {
    forks: Weak<Forks>,
    forge: Weak<Forge>,
}

#[async_trait]
impl EventListener for FollowForkPushes {
    async fn on_event(&self, e: &Event) {
        if e.kind != "ref.updated" {
            return;
        }
        let Some(event::Body::RefUpdated(r)) = &e.body else {
            return;
        };
        let (Some(forks), Some(forge)) = (self.forks.upgrade(), self.forge.upgrade()) else {
            return;
        };
        // Failures leave the change request at its last revision; the next push retries.
        let _ = forks.on_fork_push(&forge, e, r).await;
    }
}
