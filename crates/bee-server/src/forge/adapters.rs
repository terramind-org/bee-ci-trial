//! The VCS-backed ports the forge needs from the server: the principal directory, repo
//! provisioning, revision resolution, the repo policy, content repos (wiki and gist repos,
//! files of real repos), tags, insights activity, issue templates and issue branches.
//!
//! Every adapter reads and writes the server's own stores (`AppState`), so forge data that
//! refers to VCS objects always names objects that exist. None of them is called by the VCS
//! data path (SPEC §15.1).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bee_core::{ObjectId, RefName, RepoPath};
use bee_forge::changes::vcs::{ChangeVcs, HistoryVcs, NewCommit};
use bee_forge::community::ports::{
    ActivitySource, ChangeActivity, ChangeOutcome, CommitInfo, CommitRequest, ContentVcs,
    FileEntry, GraphEntry, IssueActivity, LogEntry, NewAnnotation, TagAnnotationInfo, TagInfo,
    TagVcs, VcsKind, VcsRepo,
};
use bee_forge::hooks::{
    MemoryDirectory, PolicySource, PrincipalDirectory, RepoInit, RepoProvisioner, RevisionResolver,
};
use bee_forge::work::BranchProvisioner;
use bee_forge::work::{TemplateFile, TemplateFiles, TemplateSource};
use bee_forge::{Cx, ForgeError};
use bee_history::LogOptions;
use bee_proto::v1::{self, ErrorReason, Org, Principal, PrincipalKind, Repo};
use bee_store::{Expected, OrgId, OrgRow, RefUpdate, RepoId, RepoRow};
use serde::{Deserialize, Serialize};

use super::{fe, late::Late};
use crate::auth::{NS_PRINCIPAL, PrincipalRecord};
use crate::state::{AppState, RepoCtx};

type FResult<T> = bee_forge::Result<T>;

/// Server-table namespace of tag annotations (`<repo id>/<tag>` → JSON).
pub const NS_TAG_ANNOTATION: &str = "tag_annotation";
/// The VCS org that holds gist repos (a name the forge's org names cannot take).
pub const GIST_ORG: &str = "-gists";

fn ulid(s: &str) -> FResult<bee_core::Ulid> {
    bee_core::Ulid::parse(s).map_err(|e| ForgeError::invalid("invalid_id", e.message))
}

fn core_id(id: &v1::ObjectId) -> FResult<ObjectId> {
    ObjectId::from_bytes(&id.raw).map_err(|e| ForgeError::invalid("invalid_object_id", e.message))
}

fn proto_id(id: &ObjectId) -> v1::ObjectId {
    v1::ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

fn vcs(e: bee_core::BeeError) -> ForgeError {
    bee_forge::changes::vcs::vcs_err(e)
}

fn kind_of(k: &str) -> PrincipalKind {
    match k {
        "agent" => PrincipalKind::Agent,
        "service" => PrincipalKind::Service,
        _ => PrincipalKind::Human,
    }
}

/// A principal record as the forge sees it.
pub fn principal_of(r: &PrincipalRecord) -> Principal {
    Principal {
        id: r.id.clone(),
        kind: kind_of(&r.kind) as i32,
        display: r.display.clone(),
        owner_id: r.owner.clone(),
        runtime: r.runtime.clone(),
        external_ids: r.external_ids.clone(),
    }
}

/// The forge's view of a VCS repo context.
pub fn repo_proto(ctx: &RepoCtx) -> Repo {
    Repo {
        id: ctx.id_text(),
        org_id: ctx.org.id.0.to_text(),
        name: ctx.repo.name.clone(),
        full_name: ctx.full_name.clone(),
        default_ref: "main".into(),
        ..Default::default()
    }
}

/// Resolves a forge repo to its VCS context.
pub async fn repo_ctx(state: &AppState, repo_id: &str) -> FResult<RepoCtx> {
    state
        .repo(Some(&v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::Id(repo_id.to_string())),
        }))
        .await
        .map_err(fe)
}

// ----- directory -------------------------------------------------------------------------

/// Principals from the server's principal table, with logins = display names, plus the
/// principals the forge learns from verified tokens and registrations.
pub struct ServerDirectory {
    state: Arc<AppState>,
    mem: MemoryDirectory,
}

impl ServerDirectory {
    /// A directory over `state`.
    pub fn new(state: Arc<AppState>) -> ServerDirectory {
        ServerDirectory {
            state,
            mem: MemoryDirectory::new(),
        }
    }

    /// Registers a principal with logins and emails (SCIM, imports, tests).
    pub fn register(&self, p: Principal, logins: Vec<String>, emails: Vec<String>) {
        self.mem.register(p, logins, emails);
    }

    async fn record(&self, id: &str) -> Option<PrincipalRecord> {
        self.state.principal(id).await.ok().flatten()
    }
}

#[async_trait]
impl PrincipalDirectory for ServerDirectory {
    async fn get(&self, id: &str) -> Option<Principal> {
        match self.record(id).await {
            Some(r) => Some(principal_of(&r)),
            None => self.mem.get(id).await,
        }
    }

    async fn by_login(&self, login: &str) -> Option<Principal> {
        if let Some(p) = self.mem.by_login(login).await {
            return Some(p);
        }
        let rows = self
            .state
            .kv
            .scan(NS_PRINCIPAL, b"", None, 10_000)
            .await
            .ok()?;
        rows.into_iter()
            .filter_map(|(_, v)| serde_json::from_slice::<PrincipalRecord>(&v).ok())
            .find(|r| r.display.eq_ignore_ascii_case(login) || r.id == login)
            .map(|r| principal_of(&r))
    }

    async fn identities(&self, id: &str) -> (Vec<String>, Vec<String>) {
        let (mut logins, emails) = self.mem.identities(id).await;
        if let Some(r) = self.record(id).await
            && !logins.iter().any(|l| l.eq_ignore_ascii_case(&r.display))
        {
            logins.push(r.display);
        }
        (logins, emails)
    }

    fn observe(&self, p: &Principal) {
        self.mem.observe(p);
    }
}

// ----- provisioning ----------------------------------------------------------------------

/// Creates the VCS side of forge orgs and repos: the org row, the repo with its empty root
/// changeset and `main`, initial README and license files, and template copies.
pub struct ServerProvisioner {
    state: Arc<AppState>,
    vcs: Arc<HistoryVcs>,
}

impl ServerProvisioner {
    /// A provisioner over `state`.
    pub fn new(state: Arc<AppState>, vcs: Arc<HistoryVcs>) -> ServerProvisioner {
        ServerProvisioner { state, vcs }
    }

    async fn org_row(&self, org_id: &str) -> FResult<OrgRow> {
        self.state
            .meta
            .get_org(OrgId(ulid(org_id)?))
            .await
            .map_err(vcs)?
            .ok_or_else(|| ForgeError::not_found(format!("org {org_id} on the VCS side")))
    }

    async fn ensure_repo(&self, repo: &Repo) -> FResult<RepoCtx> {
        let id = RepoId(ulid(&repo.id)?);
        let org = self.org_row(&repo.org_id).await?;
        if self.state.meta.get_repo(id).await.map_err(vcs)?.is_none() {
            let row = RepoRow {
                id,
                org: org.id,
                name: repo.name.clone(),
                created_at_ms: self.state.now_ms(),
                draft_retention_days: bee_store::DEFAULT_DRAFT_RETENTION_DAYS,
            };
            crate::server::create_repo(&self.state, &org, &row, true)
                .await
                .map_err(fe)?;
        }
        repo_ctx(&self.state, &repo.id).await
    }

    async fn commit_on_main(
        &self,
        repo: &Repo,
        edits: Vec<(String, Option<Vec<u8>>)>,
        message: &str,
    ) -> FResult<()> {
        let tip = self.vcs.ref_tip(repo, "main").await?.and_then(|r| r.target);
        let head = self
            .vcs
            .create_commit(
                repo,
                NewCommit {
                    parent: tip,
                    edits,
                    message: message.into(),
                    author: format!("Beehive <noreply@{}>", self.state.public_url_host()),
                    creator: "server".into(),
                    chain: vec!["server".into()],
                    change_id: None,
                },
            )
            .await?;
        self.vcs.set_ref(repo, "main", &head).await.map(|_| ())
    }
}

#[async_trait]
impl RepoProvisioner for ServerProvisioner {
    async fn create_org(&self, org: &Org) -> FResult<()> {
        let id = OrgId(ulid(&org.id)?);
        if self.state.meta.get_org(id).await.map_err(vcs)?.is_some() {
            return Ok(());
        }
        self.state
            .meta
            .create_org(OrgRow {
                id,
                name: org.name.clone(),
                created_at_ms: self.state.now_ms(),
            })
            .await
            .map(|_| ())
            .map_err(vcs)
    }

    async fn create_repo(&self, repo: &Repo, init: &RepoInit) -> FResult<()> {
        let r = Repo {
            default_ref: "main".into(),
            ..repo.clone()
        };
        self.ensure_repo(&r).await?;
        let mut edits = Vec::new();
        if init.auto_init {
            edits.push((
                "README.md".to_string(),
                Some(format!("# {}\n", r.name).into_bytes()),
            ));
        }
        if !init.license_template.is_empty() {
            edits.push((
                "LICENSE".to_string(),
                Some(format!("SPDX-License-Identifier: {}\n", init.license_template).into_bytes()),
            ));
        }
        if edits.is_empty() {
            return Ok(());
        }
        self.commit_on_main(&r, edits, "Initial commit").await
    }

    async fn create_from_template(
        &self,
        repo: &Repo,
        template: &Repo,
        _include_all_refs: bool,
    ) -> FResult<()> {
        let r = Repo {
            default_ref: "main".into(),
            ..repo.clone()
        };
        self.ensure_repo(&r).await?;
        let tctx = repo_ctx(&self.state, &template.id).await?;
        let content = ServerContent::new(self.state.clone(), self.vcs.clone());
        let vt = VcsRepo::of_repo(template);
        let Some(tip) = content.head(&vt).await? else {
            return Ok(());
        };
        let history = self.state.history(&tctx);
        let root = *history
            .changeset(&core_id(&tip)?)
            .await
            .map_err(vcs)?
            .root();
        let mut edits = Vec::new();
        for (path, entry) in history.list_leaves(&root, None).await.map_err(vcs)? {
            let bytes = self
                .state
                .blobs
                .get(tctx.org.id, &entry.id)
                .await
                .map_err(vcs)?
                .ok_or_else(|| ForgeError::not_found(format!("blob of {path}")))?;
            edits.push((path.to_string(), Some(bytes.to_vec())));
        }
        if edits.is_empty() {
            return Ok(());
        }
        self.commit_on_main(
            &r,
            edits,
            &format!("Initial commit from template {}", template.full_name),
        )
        .await
    }
}

// ----- revisions and policy ---------------------------------------------------------------

/// Resolves revision expressions with the VCS revision grammar (SPEC §5).
pub struct ServerResolver {
    state: Arc<AppState>,
}

impl ServerResolver {
    /// A resolver over `state`.
    pub fn new(state: Arc<AppState>) -> ServerResolver {
        ServerResolver { state }
    }
}

#[async_trait]
impl RevisionResolver for ServerResolver {
    async fn resolve(&self, repo: &Repo, expr: &str) -> FResult<v1::ObjectId> {
        let ctx = repo_ctx(&self.state, &repo.id).await?;
        let default = if repo.default_ref.is_empty() {
            "main"
        } else {
            repo.default_ref.as_str()
        };
        let on = RefName::new(default).map_err(vcs)?;
        let expr = if expr.is_empty() { default } else { expr };
        let id = self
            .state
            .history(&ctx)
            .resolve_str(expr, Some(&on))
            .await
            .map_err(vcs)?;
        Ok(proto_id(&id))
    }
}

/// The repo's effective policy, as the VCS side computes it (org default overlaid by
/// `.beehive/policy.toml` at the default ref's tip), so forge and coalescer decide alike.
pub struct ServerPolicy {
    state: Arc<AppState>,
}

impl ServerPolicy {
    /// A policy source over `state`.
    pub fn new(state: Arc<AppState>) -> ServerPolicy {
        ServerPolicy { state }
    }
}

#[async_trait]
impl PolicySource for ServerPolicy {
    async fn policy(&self, repo: &Repo) -> FResult<Option<bee_policy::Policy>> {
        let ctx = repo_ctx(&self.state, &repo.id).await?;
        let r = if repo.default_ref.is_empty() {
            "main"
        } else {
            repo.default_ref.as_str()
        };
        let p = self.state.policy(&ctx, r).await.map_err(fe)?;
        Ok(Some((*p).clone()))
    }
}

// ----- content repos -----------------------------------------------------------------------

/// Wiki repos (`<repo>.wiki` siblings in the repo's org), gist repos (repos named by the gist
/// id in the [`GIST_ORG`] org) and the files of real repos.
pub struct ServerContent {
    state: Arc<AppState>,
    vcs: Arc<HistoryVcs>,
}

impl ServerContent {
    /// Content repos over `state`.
    pub fn new(state: Arc<AppState>, vcs: Arc<HistoryVcs>) -> ServerContent {
        ServerContent { state, vcs }
    }

    async fn gist_org(&self, create: bool) -> FResult<Option<OrgRow>> {
        if let Some(o) = self.state.meta.org_by_name(GIST_ORG).await.map_err(vcs)? {
            return Ok(Some(o));
        }
        if !create {
            return Ok(None);
        }
        let row = OrgRow {
            id: OrgId(crate::util::new_ulid(self.state.now_ms()).map_err(fe)?),
            name: GIST_ORG.into(),
            created_at_ms: self.state.now_ms(),
        };
        self.state.meta.create_org(row).await.map_err(vcs)?;
        self.state
            .meta
            .org_by_name(GIST_ORG)
            .await
            .map_err(vcs)?
            .map(Some)
            .ok_or_else(|| ForgeError::unavailable("gist org vanished"))
    }

    /// The VCS org and repo name a content repo lives at.
    async fn location(&self, r: &VcsRepo, create: bool) -> FResult<Option<(OrgRow, String)>> {
        match r.kind {
            VcsKind::Repo => {
                let ctx = repo_ctx(&self.state, &r.id).await?;
                Ok(Some((ctx.org, ctx.repo.name)))
            }
            VcsKind::Wiki => {
                let ctx = repo_ctx(&self.state, &r.id).await?;
                Ok(Some((ctx.org, format!("{}.wiki", ctx.repo.name))))
            }
            VcsKind::Gist => Ok(self
                .gist_org(create)
                .await?
                .map(|o| (o, r.id.to_ascii_lowercase()))),
        }
    }

    /// The repo, created with its root and `main` when `create` is set.
    async fn ctx(&self, r: &VcsRepo, create: bool) -> FResult<Option<RepoCtx>> {
        let Some((org, name)) = self.location(r, create).await? else {
            return Ok(None);
        };
        let row = match self
            .state
            .meta
            .repo_by_name(org.id, &name)
            .await
            .map_err(vcs)?
        {
            Some(row) => row,
            None if create => {
                let row = RepoRow {
                    id: RepoId(crate::util::new_ulid(self.state.now_ms()).map_err(fe)?),
                    org: org.id,
                    name: name.clone(),
                    created_at_ms: self.state.now_ms(),
                    draft_retention_days: bee_store::DEFAULT_DRAFT_RETENTION_DAYS,
                };
                crate::server::create_repo(&self.state, &org, &row, true)
                    .await
                    .map_err(fe)?;
                row
            }
            None => return Ok(None),
        };
        Ok(Some(RepoCtx {
            full_name: format!("{}/{}", org.name, row.name),
            org,
            repo: row,
        }))
    }

    async fn need(&self, r: &VcsRepo) -> FResult<RepoCtx> {
        self.ctx(r, false)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("repository {}", r.name)))
    }

    async fn commit_info(
        &self,
        history: &bee_history::History,
        id: &ObjectId,
        paths: Vec<String>,
        additions: u32,
        deletions: u32,
    ) -> FResult<CommitInfo> {
        let cs = history.changeset(id).await.map_err(vcs)?;
        let meta = history.meta_object(cs.meta()).await.ok().flatten();
        let (author, message, time_ms) = match meta {
            Some(m) => (
                m.creator.clone(),
                m.message.clone(),
                (m.author_time.seconds.max(0) as u64) * 1000,
            ),
            None => (String::new(), String::new(), 0),
        };
        let p = self.principal(&author).await;
        Ok(CommitInfo {
            id: proto_id(id),
            parent: cs.first_parent().map(proto_id),
            author: p,
            message,
            time_ms,
            paths,
            additions,
            deletions,
        })
    }

    async fn principal(&self, id: &str) -> Principal {
        match self.state.principal(id).await.ok().flatten() {
            Some(r) => principal_of(&r),
            None => Principal {
                id: id.to_string(),
                kind: PrincipalKind::Human as i32,
                ..Default::default()
            },
        }
    }
}

fn line_counts(old: Option<&[u8]>, new: Option<&[u8]>) -> (u32, u32) {
    let lines = |b: Option<&[u8]>| -> BTreeMap<Vec<u8>, i64> {
        let mut m = BTreeMap::new();
        for l in b.unwrap_or_default().split(|c| *c == b'\n') {
            if !l.is_empty() {
                *m.entry(l.to_vec()).or_insert(0) += 1;
            }
        }
        m
    };
    let (o, n) = (lines(old), lines(new));
    let mut add = 0i64;
    let mut del = 0i64;
    for (l, c) in &n {
        add += (c - o.get(l).copied().unwrap_or(0)).max(0);
    }
    for (l, c) in &o {
        del += (c - n.get(l).copied().unwrap_or(0)).max(0);
    }
    (add as u32, del as u32)
}

#[async_trait]
impl ContentVcs for ServerContent {
    async fn init(&self, repo: &VcsRepo) -> FResult<()> {
        let ctx = self
            .ctx(repo, true)
            .await?
            .ok_or_else(|| ForgeError::unavailable("content repo not created"))?;
        // A removed repo has no refs left: give it `main` at the root again.
        let main = RefName::new("main").map_err(vcs)?;
        let cur = self
            .state
            .meta
            .get_ref(ctx.repo.id, &main)
            .await
            .map_err(vcs)?;
        if cur.as_ref().is_none_or(|r| r.target.is_none()) {
            let root = bee_object::Changeset::empty_root().id();
            let expected = match cur {
                Some(r) => Expected::Version(r.version),
                None => Expected::Absent,
            };
            self.state
                .writer
                .update_ref(RefUpdate::set(
                    ctx.repo.id,
                    main,
                    expected,
                    Some(root),
                    "forge",
                    self.state.now_ms(),
                ))
                .await
                .map_err(vcs)?;
        }
        Ok(())
    }

    async fn remove(&self, repo: &VcsRepo) -> FResult<()> {
        let Some(ctx) = self.ctx(repo, false).await? else {
            return Ok(());
        };
        for r in self
            .state
            .meta
            .list_refs(ctx.repo.id, "")
            .await
            .map_err(vcs)?
        {
            if r.target.is_none() {
                continue;
            }
            self.state
                .writer
                .update_ref(RefUpdate::set(
                    ctx.repo.id,
                    r.name,
                    Expected::Version(r.version),
                    None,
                    "forge",
                    self.state.now_ms(),
                ))
                .await
                .map_err(vcs)?;
        }
        Ok(())
    }

    async fn fork(&self, from: &VcsRepo, to: &VcsRepo) -> FResult<()> {
        let src = self.need(from).await?;
        let dst = self
            .ctx(to, true)
            .await?
            .ok_or_else(|| ForgeError::unavailable("fork target not created"))?;
        let Some(head) = self.head(from).await? else {
            return Ok(());
        };
        let head = core_id(&head)?;
        let sh = self.state.history(&src);
        let mut order = Vec::new();
        let mut token = None;
        loop {
            let page = sh
                .log(
                    &head,
                    &LogOptions {
                        limit: 1000,
                        page_token: token.clone(),
                        ..LogOptions::default()
                    },
                )
                .await
                .map_err(vcs)?;
            order.extend(page.entries.into_iter().map(|e| e.id));
            match page.next_page_token {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        order.reverse();
        let dh = self.state.history(&dst);
        let now = self.state.now_ms();
        for id in order {
            let cs = sh.changeset(&id).await.map_err(vcs)?;
            if cs.parents().is_empty() {
                continue;
            }
            if src.org.id != dst.org.id {
                return Err(ForgeError::invalid(
                    "cross_org_fork",
                    "content repos fork within one org",
                ));
            }
            let built = self
                .state
                .writer
                .build(
                    dst.org.id,
                    dst.repo.id,
                    cs.parents().to_vec(),
                    *cs.meta(),
                    *cs.change_id(),
                    cs.delta().to_vec(),
                )
                .await
                .map_err(vcs)?;
            dh.store_built(&built, now).await.map_err(vcs)?;
        }
        let main = RefName::new("main").map_err(vcs)?;
        let cur = self
            .state
            .meta
            .get_ref(dst.repo.id, &main)
            .await
            .map_err(vcs)?;
        self.state
            .writer
            .update_ref(RefUpdate::set(
                dst.repo.id,
                main,
                cur.map_or(Expected::Absent, |r| Expected::Version(r.version)),
                Some(head),
                "forge",
                now,
            ))
            .await
            .map_err(vcs)?;
        Ok(())
    }

    async fn head(&self, repo: &VcsRepo) -> FResult<Option<v1::ObjectId>> {
        let Some(ctx) = self.ctx(repo, false).await? else {
            return Ok(None);
        };
        let main = RefName::new("main").map_err(vcs)?;
        let tip = self
            .state
            .meta
            .get_ref(ctx.repo.id, &main)
            .await
            .map_err(vcs)?
            .and_then(|r| r.target);
        // The empty root is an empty repo.
        Ok(tip
            .filter(|t| *t != bee_object::Changeset::empty_root().id())
            .map(|t| proto_id(&t)))
    }

    async fn resolve(&self, repo: &VcsRepo, expr: &str) -> FResult<v1::ObjectId> {
        let ctx = self.need(repo).await?;
        let main = RefName::new("main").map_err(vcs)?;
        let expr = if expr.is_empty() { "main" } else { expr };
        let id = self
            .state
            .history(&ctx)
            .resolve_str(expr, Some(&main))
            .await
            .map_err(vcs)?;
        Ok(proto_id(&id))
    }

    async fn tree(&self, repo: &VcsRepo, at: &v1::ObjectId) -> FResult<Vec<FileEntry>> {
        let ctx = self.need(repo).await?;
        let h = self.state.history(&ctx);
        let root = *h.changeset(&core_id(at)?).await.map_err(vcs)?.root();
        Ok(h.list_leaves(&root, None)
            .await
            .map_err(vcs)?
            .into_iter()
            .map(|(p, e)| FileEntry {
                path: p.to_string(),
                blob: proto_id(&e.id),
                size: e.size,
            })
            .collect())
    }

    async fn tree_id(&self, repo: &VcsRepo, at: &v1::ObjectId) -> FResult<v1::ObjectId> {
        let ctx = self.need(repo).await?;
        let h = self.state.history(&ctx);
        Ok(proto_id(
            h.changeset(&core_id(at)?).await.map_err(vcs)?.root(),
        ))
    }

    async fn read(&self, repo: &VcsRepo, blob: &v1::ObjectId) -> FResult<Vec<u8>> {
        let ctx = self.need(repo).await?;
        self.state
            .blobs
            .get(ctx.org.id, &core_id(blob)?)
            .await
            .map_err(vcs)?
            .map(|b| b.to_vec())
            .ok_or_else(|| ForgeError::not_found("blob"))
    }

    async fn commit(&self, repo: &VcsRepo, req: CommitRequest) -> FResult<CommitInfo> {
        let ctx = self
            .ctx(repo, true)
            .await?
            .ok_or_else(|| ForgeError::unavailable("content repo not created"))?;
        let rp = repo_proto(&ctx);
        let main = RefName::new("main").map_err(vcs)?;
        let cur = self
            .state
            .meta
            .get_ref(ctx.repo.id, &main)
            .await
            .map_err(vcs)?;
        let tip = cur.as_ref().and_then(|r| r.target);
        let visible = tip
            .filter(|t| *t != bee_object::Changeset::empty_root().id())
            .map(|t| proto_id(&t));
        if req.expected_head.is_some() && req.expected_head != visible {
            return Err(ForgeError::new(
                ErrorReason::VersionMismatch,
                "the repository moved since it was read",
            ));
        }
        let mut adds = 0;
        let mut dels = 0;
        let mut paths = Vec::new();
        let mut edits = Vec::new();
        for c in req.changes {
            let old = match &visible {
                Some(t) => self.vcs.read_file(&rp, t, &c.path).await?,
                None => None,
            };
            let (a, d) = line_counts(old.as_deref(), c.content.as_deref());
            adds += a;
            dels += d;
            paths.push(c.path.clone());
            edits.push((c.path, c.content));
        }
        let display = if req.author.display.is_empty() {
            req.author.id.clone()
        } else {
            req.author.display.clone()
        };
        let head = self
            .vcs
            .create_commit(
                &rp,
                NewCommit {
                    parent: tip.map(|t| proto_id(&t)),
                    edits,
                    message: req.message.clone(),
                    author: display,
                    creator: req.author.id.clone(),
                    chain: vec![req.author.id.clone()],
                    change_id: None,
                },
            )
            .await?;
        let head_core = core_id(&head)?;
        self.state
            .writer
            .update_ref(RefUpdate::set(
                ctx.repo.id,
                main,
                cur.map_or(Expected::Absent, |r| Expected::Version(r.version)),
                Some(head_core),
                req.author.id.clone(),
                self.state.now_ms(),
            ))
            .await
            .map_err(vcs)?;
        let h = self.state.history(&ctx);
        let mut info = self.commit_info(&h, &head_core, paths, adds, dels).await?;
        info.author = req.author;
        if info.time_ms == 0 {
            info.time_ms = req.time_ms;
        }
        Ok(info)
    }

    async fn log(
        &self,
        repo: &VcsRepo,
        at: &v1::ObjectId,
        path: Option<&str>,
        limit: usize,
    ) -> FResult<Vec<CommitInfo>> {
        let ctx = self.need(repo).await?;
        let h = self.state.history(&ctx);
        let from = core_id(at)?;
        let opts = LogOptions {
            limit: limit.max(1),
            changed_paths: true,
            ..LogOptions::default()
        };
        let mut out = Vec::new();
        match path.filter(|p| !p.is_empty()) {
            Some(p) => {
                let rp = RepoPath::new(p).map_err(vcs)?;
                let page = h.path_log(&from, Some(&rp), &opts).await.map_err(vcs)?;
                for e in page.entries {
                    let paths = e.changed.iter().map(|c| c.path.to_string()).collect();
                    out.push(self.commit_info(&h, &e.changeset, paths, 0, 0).await?);
                }
            }
            None => {
                let page = h.log(&from, &opts).await.map_err(vcs)?;
                for e in page.entries {
                    if e.parents.is_empty() {
                        continue;
                    }
                    let cs = h.changeset(&e.id).await.map_err(vcs)?;
                    let paths = cs.delta().iter().map(|d| d.path.to_string()).collect();
                    out.push(self.commit_info(&h, &e.id, paths, 0, 0).await?);
                }
            }
        }
        Ok(out)
    }

    async fn is_ancestor(
        &self,
        repo: &VcsRepo,
        ancestor: &v1::ObjectId,
        of: &v1::ObjectId,
    ) -> FResult<bool> {
        let ctx = self.need(repo).await?;
        self.state
            .history(&ctx)
            .is_ancestor(&core_id(ancestor)?, &core_id(of)?)
            .await
            .map_err(vcs)
    }
}

// ----- tags ---------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoredAnnotation {
    tagger: String,
    message: String,
    signature_hex: String,
    time_ms: u64,
}

/// GitHub's verification reason name (`valid`, `unsigned`, `unknown_key`, `bad_signature`,
/// …) for a signature verdict.
pub fn reason_name(r: v1::SecuritySignatureReason) -> String {
    r.as_str_name()
        .trim_start_matches("SECURITY_SIGNATURE_REASON_")
        .to_ascii_lowercase()
}

/// The stored annotation of a tag created through the forge: its message and armored
/// signature (empty when unsigned).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredTagAnnotation {
    /// The message, which the signature covers.
    pub message: String,
    /// The armored signature.
    pub signature: Vec<u8>,
}

/// The annotation of tag `name` (without `tags/`) in a repo, if one was stored.
pub async fn tag_annotation(
    state: &AppState,
    ctx: &RepoCtx,
    name: &str,
) -> crate::errors::ApiResult<Option<StoredTagAnnotation>> {
    let key = format!("{}/{}", ctx.id_text(), name);
    Ok(state
        .kv
        .get(NS_TAG_ANNOTATION, key.as_bytes())
        .await?
        .and_then(|b| serde_json::from_slice::<StoredAnnotation>(&b).ok())
        .map(|a| StoredTagAnnotation {
            message: a.message,
            signature: bee_core::hex::decode(&a.signature_hex).unwrap_or_default(),
        }))
}

/// Tags as `tags/<name>` refs; annotations of tags created here are kept in the server
/// tables.
pub struct ServerTags {
    state: Arc<AppState>,
}

impl ServerTags {
    /// Tags over `state`.
    pub fn new(state: Arc<AppState>) -> ServerTags {
        ServerTags { state }
    }

    async fn info(&self, ctx: &RepoCtx, row: &bee_store::RefRow) -> Option<TagInfo> {
        let target = row.target?;
        let name = row.name.as_str().strip_prefix("tags/")?.to_string();
        let key = format!("{}/{}", ctx.id_text(), name);
        let annotation = self
            .state
            .kv
            .get(NS_TAG_ANNOTATION, key.as_bytes())
            .await
            .ok()
            .flatten()
            .and_then(|b| serde_json::from_slice::<StoredAnnotation>(&b).ok());
        let annotation = match annotation {
            None => None,
            Some(a) => {
                let signature = bee_core::hex::decode(&a.signature_hex).unwrap_or_default();
                // The signature of an annotation made here covers its message.
                let (verified, reason) = if signature.is_empty() {
                    (false, "unsigned".to_string())
                } else {
                    match self
                        .state
                        .forge
                        .verify_signature(a.message.as_bytes(), &signature)
                        .await
                    {
                        Ok(Some(v)) => (v.verified(), reason_name(v.reason)),
                        _ => (false, "unknown_key".to_string()),
                    }
                };
                Some(TagAnnotationInfo {
                    id: proto_id(&target),
                    tagger: Principal {
                        id: a.tagger.clone(),
                        kind: PrincipalKind::Human as i32,
                        ..Default::default()
                    },
                    time_ms: a.time_ms,
                    message: a.message,
                    signature,
                    verified,
                    reason,
                })
            }
        };
        Some(TagInfo {
            name,
            target: proto_id(&target),
            ref_version: row.version,
            annotation,
            protected: false,
        })
    }
}

#[async_trait]
impl TagVcs for ServerTags {
    async fn list(&self, repo_id: &str, prefix: &str) -> FResult<Vec<TagInfo>> {
        let ctx = repo_ctx(&self.state, repo_id).await?;
        let rows = self
            .state
            .meta
            .list_refs(ctx.repo.id, &format!("tags/{prefix}"))
            .await
            .map_err(vcs)?;
        let mut out = Vec::new();
        for r in rows {
            if let Some(t) = self.info(&ctx, &r).await {
                out.push(t);
            }
        }
        Ok(out)
    }

    async fn get(&self, repo_id: &str, name: &str) -> FResult<Option<TagInfo>> {
        let ctx = repo_ctx(&self.state, repo_id).await?;
        let rn = RefName::new(format!("tags/{name}")).map_err(vcs)?;
        Ok(
            match self
                .state
                .meta
                .get_ref(ctx.repo.id, &rn)
                .await
                .map_err(vcs)?
            {
                Some(r) => self.info(&ctx, &r).await,
                None => None,
            },
        )
    }

    async fn create(
        &self,
        repo_id: &str,
        name: &str,
        target: &v1::ObjectId,
        annotation: Option<NewAnnotation>,
    ) -> FResult<TagInfo> {
        let ctx = repo_ctx(&self.state, repo_id).await?;
        let rn = RefName::new(format!("tags/{name}")).map_err(vcs)?;
        let cur = self
            .state
            .meta
            .get_ref(ctx.repo.id, &rn)
            .await
            .map_err(vcs)?;
        if cur.as_ref().is_some_and(|r| r.target.is_some()) {
            return Err(ForgeError::already_exists(format!("tag {name}")));
        }
        let t = core_id(target)?;
        let principal = annotation
            .as_ref()
            .map(|a| a.tagger.id.clone())
            .unwrap_or_else(|| "forge".into());
        let mut up = RefUpdate::set(
            ctx.repo.id,
            rn.clone(),
            cur.as_ref()
                .map_or(Expected::Absent, |r| Expected::Version(r.version)),
            Some(t),
            principal.clone(),
            self.state.now_ms(),
        );
        up.events.push(crate::events::new_event(
            "ref.updated",
            &serde_json::json!({
                "ref": rn.as_str(), "old": "", "new": t.to_text(),
                "version": cur.as_ref().map_or(1, |r| r.version + 1),
                "principal_chain": [principal],
            }),
            self.state.now_ms(),
        ));
        let done = self.state.writer.update_ref(up).await.map_err(vcs)?;
        if let Some(a) = &annotation {
            let stored = StoredAnnotation {
                tagger: a.tagger.id.clone(),
                message: a.message.clone(),
                signature_hex: bee_core::hex::encode(&a.signature),
                time_ms: a.time_ms,
            };
            self.state
                .kv
                .put(
                    NS_TAG_ANNOTATION,
                    format!("{}/{}", ctx.id_text(), name).as_bytes(),
                    serde_json::to_vec(&stored).unwrap_or_default(),
                )
                .await
                .map_err(fe)?;
        }
        let row = bee_store::RefRow {
            repo: ctx.repo.id,
            name: rn,
            target: Some(t),
            version: done.new_version,
        };
        self.info(&ctx, &row)
            .await
            .ok_or_else(|| ForgeError::internal("tag vanished"))
    }

    async fn delete(&self, repo_id: &str, name: &str, expected_version: u64) -> FResult<()> {
        let ctx = repo_ctx(&self.state, repo_id).await?;
        let rn = RefName::new(format!("tags/{name}")).map_err(vcs)?;
        let cur = self
            .state
            .meta
            .get_ref(ctx.repo.id, &rn)
            .await
            .map_err(vcs)?
            .filter(|r| r.target.is_some())
            .ok_or_else(|| ForgeError::not_found(format!("tag {name}")))?;
        let v = if expected_version == 0 {
            cur.version
        } else {
            expected_version
        };
        self.state
            .writer
            .update_ref(RefUpdate::set(
                ctx.repo.id,
                rn,
                Expected::Version(v),
                None,
                "forge",
                self.state.now_ms(),
            ))
            .await
            .map_err(vcs)?;
        let _ = self
            .state
            .kv
            .apply(vec![crate::kv::KvOp::delete(
                NS_TAG_ANNOTATION,
                format!("{}/{}", ctx.id_text(), name),
            )])
            .await;
        Ok(())
    }
}

// ----- insights activity --------------------------------------------------------------------

/// History from the VCS; change requests and issues from the forge modules.
pub struct ServerActivity {
    state: Arc<AppState>,
    changes: Late<bee_forge::changes::Changes>,
    work: Late<bee_forge::work::Work>,
}

impl ServerActivity {
    /// Activity over `state`, the changes module (installed later) and the work module.
    pub fn new(
        state: Arc<AppState>,
        changes: Late<bee_forge::changes::Changes>,
        work: Late<bee_forge::work::Work>,
    ) -> ServerActivity {
        ServerActivity {
            state,
            changes,
            work,
        }
    }
}

fn ms_of(t: &Option<pbjson_types::Timestamp>) -> u64 {
    t.as_ref()
        .map(|t| (t.seconds.max(0) as u64) * 1000 + (t.nanos.max(0) as u64) / 1_000_000)
        .unwrap_or(0)
}

#[async_trait]
impl ActivitySource for ServerActivity {
    async fn log(
        &self,
        repo: &Repo,
        ref_name: &str,
        since_ms: u64,
        until_ms: u64,
        limit: usize,
    ) -> FResult<Vec<LogEntry>> {
        let ctx = repo_ctx(&self.state, &repo.id).await?;
        let h = self.state.history(&ctx);
        let name = if ref_name.is_empty() {
            "main"
        } else {
            ref_name
        };
        let rn = RefName::new(name).map_err(vcs)?;
        let Some(tip) = self
            .state
            .meta
            .get_ref(ctx.repo.id, &rn)
            .await
            .map_err(vcs)?
            .and_then(|r| r.target)
        else {
            return Ok(Vec::new());
        };
        let page = h
            .log(
                &tip,
                &LogOptions {
                    limit: limit.max(1),
                    since: (since_ms > 0).then_some((since_ms / 1000) as i64),
                    until: (until_ms > 0).then_some(until_ms.div_ceil(1000) as i64),
                    ..LogOptions::default()
                },
            )
            .await
            .map_err(vcs)?;
        let mut out = Vec::new();
        for e in page.entries {
            if e.parents.is_empty() {
                continue;
            }
            let cs = h.changeset(&e.id).await.map_err(vcs)?;
            let (author, summary, time_ms, cost) = match &e.meta {
                Some(m) => {
                    let cost = m
                        .provenance
                        .as_ref()
                        .and_then(|p| p.cost.as_ref())
                        .map(|c| {
                            (
                                c.input_tokens.unwrap_or(0),
                                c.output_tokens.unwrap_or(0),
                                c.usd_micros.unwrap_or(0),
                            )
                        })
                        .unwrap_or((0, 0, 0));
                    (
                        m.creator.clone(),
                        m.message.lines().next().unwrap_or("").to_string(),
                        (m.author_time.seconds.max(0) as u64) * 1000,
                        cost,
                    )
                }
                None => (String::new(), String::new(), 0, (0, 0, 0)),
            };
            let mut p = Principal {
                id: author.clone(),
                kind: PrincipalKind::Human as i32,
                ..Default::default()
            };
            if let Some(r) = self.state.principal(&author).await.ok().flatten() {
                p = principal_of(&r);
            }
            let additions: u64 = cs
                .delta()
                .iter()
                .map(|d| match d.op {
                    bee_object::DeltaOp::Put { .. } => 1,
                    bee_object::DeltaOp::Delete => 0,
                })
                .sum();
            out.push(LogEntry {
                changeset: proto_id(&e.id),
                parents: e.parents.iter().map(proto_id).collect(),
                author: p,
                placeholder_email: String::new(),
                time_ms,
                additions,
                deletions: cs.delta().len() as u64 - additions,
                files_changed: cs.delta().len() as u32,
                summary,
                refs: Vec::new(),
                cost,
                reverted: false,
            });
        }
        Ok(out)
    }

    async fn change_requests(
        &self,
        repo: &Repo,
        since_ms: u64,
        until_ms: u64,
    ) -> FResult<Vec<ChangeActivity>> {
        let Some(changes) = self.changes.get() else {
            return Ok(Vec::new());
        };
        let cx = Cx::system();
        let (crs, _) = changes
            .list(
                &cx,
                repo,
                &bee_forge::changes::query::ListFilter::default(),
                "",
                bee_forge::changes::store::MAX_EXAMINE,
            )
            .await?;
        let until = if until_ms == 0 { u64::MAX } else { until_ms };
        Ok(crs
            .into_iter()
            .filter_map(|cr| {
                let opened = ms_of(&cr.created_at);
                let ended = ms_of(&cr.landed_at).max(ms_of(&cr.closed_at));
                let in_window = |t: u64| t >= since_ms && t < until;
                if !in_window(opened) && !(ended > 0 && in_window(ended)) {
                    return None;
                }
                let outcome = match v1::ChangeState::try_from(cr.state) {
                    Ok(v1::ChangeState::Landed) => ChangeOutcome::Landed,
                    Ok(v1::ChangeState::Closed) => ChangeOutcome::Closed,
                    _ => ChangeOutcome::Open,
                };
                Some(ChangeActivity {
                    number: cr.number,
                    author: cr.author.clone().unwrap_or_default(),
                    outcome,
                    opened_ms: opened,
                    ended_ms: ended,
                    review_rounds: cr.revisions.len().saturating_sub(1) as u32,
                })
            })
            .collect())
    }

    async fn issues(
        &self,
        repo: &Repo,
        since_ms: u64,
        until_ms: u64,
    ) -> FResult<Vec<IssueActivity>> {
        let cx = Cx::system();
        let Some(work) = self.work.get() else {
            return Ok(Vec::new());
        };
        let issues = work.issues_in_window(&cx, &repo.id).await?;
        let until = if until_ms == 0 { u64::MAX } else { until_ms };
        Ok(issues
            .into_iter()
            .filter(|(_, o, c)| (*o >= since_ms && *o < until) || (*c >= since_ms && *c < until))
            .map(|(number, opened_ms, closed_ms)| IssueActivity {
                number,
                opened_ms,
                closed_ms,
            })
            .collect())
    }

    async fn graph(
        &self,
        repo: &Repo,
        _include_forks: bool,
        refs: &[String],
        since_ms: u64,
        until_ms: u64,
        limit: usize,
    ) -> FResult<Vec<GraphEntry>> {
        let names: Vec<String> = if refs.is_empty() {
            vec![repo.default_ref.clone()]
        } else {
            refs.to_vec()
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for name in names {
            for mut e in self.log(repo, &name, since_ms, until_ms, limit).await? {
                if !seen.insert(e.changeset.raw.clone()) {
                    continue;
                }
                e.refs = vec![name.clone()];
                out.push(GraphEntry {
                    entry: e,
                    repo_id: repo.id.clone(),
                    repo_full_name: repo.full_name.clone(),
                });
            }
        }
        out.sort_by_key(|a| std::cmp::Reverse(a.entry.time_ms));
        out.truncate(limit.max(1));
        Ok(out)
    }
}

// ----- issue templates and branches ---------------------------------------------------------

/// Issue templates from the default ref, read through the VCS.
pub struct ServerTemplates {
    state: Arc<AppState>,
    vcs: Arc<HistoryVcs>,
}

impl ServerTemplates {
    /// Templates over `state`.
    pub fn new(state: Arc<AppState>, vcs: Arc<HistoryVcs>) -> ServerTemplates {
        ServerTemplates { state, vcs }
    }
}

#[async_trait]
impl TemplateSource for ServerTemplates {
    async fn read_dir(&self, repo: &Repo, dir: &str) -> FResult<Option<TemplateFiles>> {
        let ctx = repo_ctx(&self.state, &repo.id).await?;
        let rp = repo_proto(&ctx);
        let Some(tip) = self
            .vcs
            .ref_tip(&rp, &rp.default_ref)
            .await?
            .and_then(|r| r.target)
        else {
            return Ok(None);
        };
        let names = self.vcs.list_dir(&rp, &tip, dir).await?;
        if names.is_empty() {
            return Ok(None);
        }
        let mut files = Vec::new();
        for name in names {
            let path = format!("{}/{name}", dir.trim_end_matches('/'));
            if let Some(content) = self.vcs.read_file(&rp, &tip, &path).await? {
                files.push(TemplateFile { path, content });
            }
        }
        Ok(Some(TemplateFiles {
            revision: bee_forge::ids::hex(&tip.raw),
            files,
        }))
    }
}

/// Issue branches: a ref created from the base ref's tip with a ref-log entry.
pub struct ServerBranches {
    state: Arc<AppState>,
}

impl ServerBranches {
    /// Branches over `state`.
    pub fn new(state: Arc<AppState>) -> ServerBranches {
        ServerBranches { state }
    }
}

#[async_trait]
impl BranchProvisioner for ServerBranches {
    async fn create_branch(
        &self,
        repo: &Repo,
        ref_name: &str,
        base_ref: &str,
        workspace: bool,
    ) -> FResult<String> {
        let ctx = repo_ctx(&self.state, &repo.id).await?;
        let base = if base_ref.is_empty() {
            repo.default_ref.as_str()
        } else {
            base_ref
        };
        let base = RefName::new(if base.is_empty() { "main" } else { base }).map_err(vcs)?;
        let name = RefName::new(ref_name).map_err(vcs)?;
        let tip = self
            .state
            .meta
            .get_ref(ctx.repo.id, &base)
            .await
            .map_err(vcs)?
            .and_then(|r| r.target)
            .ok_or_else(|| ForgeError::not_found(format!("ref {}", base.as_str())))?;
        let cur = self
            .state
            .meta
            .get_ref(ctx.repo.id, &name)
            .await
            .map_err(vcs)?;
        if cur.as_ref().is_some_and(|r| r.target.is_some()) {
            return Err(ForgeError::already_exists(format!("ref {ref_name}")));
        }
        let mut up = RefUpdate::set(
            ctx.repo.id,
            name.clone(),
            cur.as_ref()
                .map_or(Expected::Absent, |r| Expected::Version(r.version)),
            Some(tip),
            "forge",
            self.state.now_ms(),
        );
        up.events.push(crate::events::new_event(
            "ref.updated",
            &serde_json::json!({
                "ref": name.as_str(), "old": "", "new": tip.to_text(),
                "version": cur.as_ref().map_or(1, |r| r.version + 1),
            }),
            self.state.now_ms(),
        ));
        self.state.writer.update_ref(up).await.map_err(vcs)?;
        Ok(if workspace {
            format!("{}:{}", ctx.id_text(), name.as_str())
        } else {
            String::new()
        })
    }
}
