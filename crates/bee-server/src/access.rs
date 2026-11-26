//! The authoritative repo-access decision (SPEC §9.6, `docs/spec/server.md` §4.1).
//!
//! Every entry point that reads or changes a repository — the VCS RPCs through
//! [`AppState::gate`], git smart HTTP and SSH, LFS, the node daemon's access checks, signed
//! URLs, event streams, search, exports and imports — asks [`AppState::authorize_repo`]
//! before anything else. It combines:
//!
//! 1. **The forge's repo record** (visibility `public | internal | private`, `archived`,
//!    `deleted_at`, owning org, full name). A repo the forge keeps no record of (VCS-only
//!    repos: imports not yet adopted, internal namespaces, wiki repos of a deleted parent) is
//!    *unmanaged* and governed by policy alone, as before the forge existed.
//! 2. **Lifecycle.** Deleted (or in a deleted org): `NOT_FOUND` for every action, for every
//!    caller, until restored. Archived: reads as usual, every other action `ARCHIVED`.
//! 3. **Roles.** Every principal of the token's chain needs a forge repo role
//!    (`Orgs::effective_role`: org owner/admin, org base role, direct and outside
//!    collaborators, teams, app installations, fork access, public/internal read) whose
//!    built-in actions contain the action. An agent acts with its own role or that of the
//!    principal it acts for (the next one in its chain), whichever is higher: the humans and
//!    services of the chain decide, so an agent never exceeds its owner and making an agent
//!    an org member never narrows it (token scope and policy narrow agents). A caller that
//!    cannot read a non-public repo gets `NOT_FOUND`, so existence does not leak.
//!
//! The token's scope and the effective policy are checked after this by the gate, as before.
//!
//! **Anonymous read** (`forge.allow_anonymous_public_read`, default on). A request with no
//! credentials to one of [`ANONYMOUS_READ_METHODS`] (and a git clone or fetch over HTTP) is
//! carried by the server's own read-only token for [`ANONYMOUS_PRINCIPAL`], minted here and
//! never handed out, so it passes authentication and lands on the `anonymous` admission
//! tier. Repo access decides it with an **empty principal chain**: `Read` on a public,
//! non-deleted repo and `NOT_FOUND` for everything else; unmanaged repos (no forge record, so
//! no visibility) are `NOT_FOUND` too. Every other RPC without credentials still answers
//! `UNAUTHENTICATED`, and so does every write.
//!
//! **Caching and invalidation.** Repo records and roles are cached for
//! `forge.access_cache_ms` (default 1000 ms), which bounds how long a stale allow can outlive
//! a change made through another server process. Changes made through this process clear the
//! cache before their call returns (the request layer after access-changing forge RPCs, the
//! GitHub compat gateway after writes, SCIM and SAML), and forge events (`repo.*`,
//! `member.*`, `collaborator.*`, `team.*`, installations, sponsorships) clear it as they are
//! relayed. Lookups started before an invalidation never repopulate the cache.
//!
//! **Forge outage.** Authorization fails closed: when the forge cannot answer, the last good
//! record and role are served for `forge.access_outage_grace_ms` (default 5 min) after they
//! were fetched — an invalidated entry is never served — and a caller with no known decision
//! gets `UNAVAILABLE` (retryable).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use bee_core::ObjectId;
use bee_store::OrgId;
use bee_store_blob::SignedUrl;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bee_policy::{Action, Actor, PrincipalKind, builtin_role_actions};
use bee_proto::v1::{self, ErrorReason, RepoRole, Visibility};

use crate::auth::Caller;
use crate::errors::{ApiError, ApiResult};
use crate::state::{AppState, RepoCtx};

/// The forge record of a repo, as access decisions read it.
pub type RepoRecord = std::sync::Arc<v1::Repo>;

/// The principal of the anonymous read token (not a ULID, so no stored principal has it).
pub use bee_forge::auth::ANONYMOUS_PRINCIPAL;

/// Lifetime of the anonymous read token; it is minted again when less than a tenth is left.
const ANONYMOUS_TOKEN_SECS: u64 = 3600;

/// The RPCs a caller without credentials may make (the read RPCs of the VCS, the reads behind a
/// public repo's tabs, and the public discovery reads of search, repos and user profiles). Each
/// still decides repo access per repo, so a caller without credentials only ever sees public
/// repos. Org membership, agents, principals, tokens and security alerts are not listed.
pub const ANONYMOUS_READ_METHODS: &[&str] = &[
    "/bee.v1.ObjectService/GetObjects",
    "/bee.v1.ObjectService/HasObjects",
    "/bee.v1.ObjectService/ResolveRef",
    "/bee.v1.QueryService/GetChangesets",
    "/bee.v1.QueryService/ReadPath",
    "/bee.v1.QueryService/ListTree",
    "/bee.v1.QueryService/Log",
    "/bee.v1.QueryService/Blame",
    "/bee.v1.QueryService/Diff",
    "/bee.v1.QueryService/IsAncestor",
    "/bee.v1.QueryService/MergeBase",
    "/bee.v1.QueryService/Resolve",
    "/bee.v1.QueryService/GetDerived",
    "/bee.v1.QueryService/ListDerivedTypes",
    "/bee.v1.QueryService/CompareRevisions",
    "/bee.v1.QueryService/FindCopies",
    "/bee.v1.RefService/ListRefs",
    "/bee.v1.RefService/GetRefLog",
    "/bee.v1.RepoService/GetRepo",
    "/bee.v1.RepoService/GetContext",
    // The tabs of a public repo: changes and review, checks, leases, runs, issues, projects,
    // releases, packages, pages and languages. Each decides `Read` on the repo it names (issues
    // `Level::View`, which is repo `Read`), and the event stream re-decides it while it runs.
    "/bee.v1.ChangeService/ListChanges",
    "/bee.v1.ChangeService/GetChange",
    "/bee.v1.ChangeService/GetMergeQueue",
    "/bee.v1.ChangeService/EvaluateLanding",
    "/bee.v1.ChangeService/ListReviews",
    "/bee.v1.ChangeService/ListReviewThreads",
    "/bee.v1.ChangeService/DiffChangeRevisions",
    "/bee.v1.CheckService/ListChecks",
    "/bee.v1.CheckService/ListCheckAnnotations",
    "/bee.v1.LeaseService/ListLeases",
    "/bee.v1.WorkflowService/ListWorkflowRuns",
    "/bee.v1.WorkflowService/GetWorkflowRun",
    "/bee.v1.WorkflowService/ListWorkflowJobs",
    "/bee.v1.WorkflowService/TailWorkflowJobLogs",
    "/bee.v1.IssueService/ListIssues",
    "/bee.v1.IssueService/GetIssueByNumber",
    "/bee.v1.IssueService/ListIssueComments",
    "/bee.v1.IssueService/ListIssueSubIssues",
    "/bee.v1.ProjectService/ListProjects",
    "/bee.v1.ProjectService/ListProjectFields",
    "/bee.v1.ProjectService/ListProjectItems",
    "/bee.v1.ReleaseService/ListReleases",
    "/bee.v1.BriefService/GetBriefChecklist",
    "/bee.v1.BriefService/GetEvidence",
    "/bee.v1.BriefService/ListEvidence",
    "/bee.v1.PackageService/ListPackages",
    "/bee.v1.PackageService/ListPackageVersions",
    "/bee.v1.PagesService/GetPagesSite",
    "/bee.v1.PagesService/ListPagesDeployments",
    "/bee.v1.InsightsService/GetLanguageStats",
    "/bee.v1.EventService/Subscribe",
    // A public repo's posts: permalinks and its Updates tab (PostService checks repo `Read` per
    // post, so a private repo's post answers `NOT_FOUND`).
    "/bee.v1.PostService/GetPost",
    "/bee.v1.PostService/ListRepoPosts",
    "/bee.v1.SearchService/SearchCode",
    "/bee.v1.SearchService/StreamSearchCode",
    "/bee.v1.SearchService/SearchSymbols",
    "/bee.v1.SearchService/GetSymbolDefinitions",
    "/bee.v1.SearchService/GetSymbolReferences",
    "/bee.v1.SearchService/GetDocumentSymbols",
    "/bee.v1.SearchService/SearchIssues",
    "/bee.v1.SearchService/SearchRepos",
    "/bee.v1.SearchService/SearchChangesets",
    "/bee.v1.SearchService/SearchUsers",
    "/bee.v1.SearchService/SearchTopics",
    "/bee.v1.SearchService/SearchDiscussions",
    "/bee.v1.SearchService/SearchPackages",
    "/bee.v1.SearchService/ParseSearchQuery",
    "/bee.v1.SearchService/ListTrendingRepos",
    "/bee.v1.SearchService/ListTrendingDevelopers",
    "/bee.v1.SearchService/ListLeaderboard",
    "/bee.v1.SearchService/GetTopic",
    "/bee.v1.SearchService/ListFeaturedTopics",
    "/bee.v1.SearchService/ListTopicCollections",
    "/bee.v1.SearchService/GetTopicCollection",
    "/bee.v1.UserService/GetUser",
    "/bee.v1.UserService/ListUserPinnedItems",
    "/bee.v1.UserService/ListUserFollowers",
    "/bee.v1.UserService/ListUserFollowing",
    "/bee.v1.UserService/ListStarredRepos",
    "/bee.v1.UserService/ListRepoStargazers",
    "/bee.v1.UserService/GetStarList",
    "/bee.v1.UserService/ListStarLists",
    "/bee.v1.UserService/ListStarListRepos",
    "/bee.v1.UserService/GetUserContributionGraph",
    "/bee.v1.UserService/GetUserActivityOverview",
    "/bee.v1.UserService/ListUserAchievements",
];

/// Whether a caller without credentials may make the RPC at `path` (`/<service>/<method>`).
pub fn anonymous_read_method(path: &str) -> bool {
    ANONYMOUS_READ_METHODS.contains(&path)
}

/// Rank of a repo role (higher grants more).
pub fn role_rank(r: RepoRole) -> u8 {
    match r {
        RepoRole::Unspecified | RepoRole::None => 0,
        RepoRole::Read => 1,
        RepoRole::Triage => 2,
        RepoRole::Write => 3,
        RepoRole::Maintain => 4,
        RepoRole::Admin => 5,
    }
}

/// The policy name of a repo role (`read` .. `admin`; `none` without one).
pub fn role_name(r: RepoRole) -> &'static str {
    match r {
        RepoRole::Read => "read",
        RepoRole::Triage => "triage",
        RepoRole::Write => "write",
        RepoRole::Maintain => "maintain",
        RepoRole::Admin => "admin",
        RepoRole::Unspecified | RepoRole::None => "none",
    }
}

/// The role a principal's `@repo-role/<role>` policy group names (SPEC §9.4); `None` for no
/// role.
pub fn role_group(r: RepoRole) -> Option<&'static str> {
    (role_rank(r) > 0).then(|| role_name(r))
}

/// Whether `role` grants `action` (the built-in role actions of SPEC §9.4).
pub fn role_allows(role: RepoRole, action: Action) -> bool {
    builtin_role_actions(role_name(role)).is_some_and(|a| a.contains(&action))
}

/// The record's lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// Live and writable.
    Active,
    /// Read-only.
    Archived,
    /// Soft-deleted (or in a soft-deleted org), restorable for the restore window.
    Deleted,
}

/// The lifecycle of a forge record.
pub fn lifecycle(r: &v1::Repo) -> Lifecycle {
    if r.deleted_at.is_some() {
        Lifecycle::Deleted
    } else if r.archived {
        Lifecycle::Archived
    } else {
        Lifecycle::Active
    }
}

/// The visibility of a forge record (unspecified reads as private).
pub fn visibility(r: &v1::Repo) -> Visibility {
    match Visibility::try_from(r.visibility).unwrap_or(Visibility::Private) {
        Visibility::Unspecified => Visibility::Private,
        v => v,
    }
}

/// Applies agent inheritance to the roles of a chain (acting principal first): an agent takes
/// the role of the principal it acts for when that is higher than its own.
pub fn inherit_agent_roles(kinds: &[PrincipalKind], roles: &mut [RepoRole]) {
    for i in (0..roles.len().saturating_sub(1)).rev() {
        if kinds.get(i) == Some(&PrincipalKind::Agent)
            && role_rank(roles[i + 1]) > role_rank(roles[i])
        {
            roles[i] = roles[i + 1];
        }
    }
}

/// The pure decision over a managed repo: the record, the chain's principal ids and their
/// (inherited) roles, and the action. Returns the weakest role of the chain on success.
/// An empty chain (a caller without credentials) reads a public repo when `anonymous_read`
/// (`forge.allow_anonymous_public_read`) is on, and is `NOT_FOUND` for everything else.
pub fn decide(
    record: &v1::Repo,
    chain: &[String],
    roles: &[RepoRole],
    action: Action,
    anonymous_read: bool,
) -> ApiResult<RepoRole> {
    let not_found = || ApiError::not_found("repository not found");
    if lifecycle(record) == Lifecycle::Deleted {
        return Err(not_found());
    }
    let public = visibility(record) == Visibility::Public;
    if chain.is_empty() {
        return if anonymous_read && public && action == Action::Read {
            Ok(RepoRole::Read)
        } else {
            Err(not_found())
        };
    }
    let mut weakest = RepoRole::Admin;
    let mut weakest_id = "";
    for (id, role) in chain.iter().zip(roles) {
        let role = if public && role_rank(*role) == 0 {
            RepoRole::Read
        } else {
            *role
        };
        if role_rank(role) < role_rank(weakest) {
            weakest = role;
            weakest_id = id;
        }
    }
    if !role_allows(weakest, Action::Read) {
        return Err(not_found());
    }
    if action != Action::Read && lifecycle(record) == Lifecycle::Archived {
        return Err(ApiError::new(
            ErrorReason::Archived,
            format!(
                "{} is archived and read-only; `{}` is refused",
                record.full_name,
                action.as_str()
            ),
        ));
    }
    if !role_allows(weakest, action) {
        return Err(ApiError::denied(format!(
            "{weakest_id} has role {} on {}, which does not allow `{}`",
            role_name(weakest),
            record.full_name,
            action.as_str()
        )));
    }
    Ok(weakest)
}

/// The grant a locally served signed URL carries: who it was issued to and for which repos'
/// content.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlGrant {
    /// Repo ids.
    #[serde(rename = "r", default)]
    pub repos: Vec<String>,
    /// The principal chain (id, kind), acting principal first.
    #[serde(rename = "c", default)]
    pub chain: Vec<(String, String)>,
}

/// What [`AppState::authorize_repo`] decided.
#[derive(Clone, Debug)]
pub struct RepoAccess {
    /// The forge record (`None` for an unmanaged repo).
    pub record: Option<RepoRecord>,
    /// The weakest role of the chain (`None` for an unmanaged repo).
    pub role: Option<RepoRole>,
}

impl RepoAccess {
    /// The org that owns the repo: the forge's (which differs from the VCS org for forks and
    /// transferred repos), else the VCS org.
    pub fn owner_org_id(&self, repo: &RepoCtx) -> String {
        match &self.record {
            Some(r) if !r.org_id.is_empty() => r.org_id.clone(),
            _ => repo.org.id.0.to_text(),
        }
    }
}

struct Entry<T> {
    value: T,
    at: Instant,
}

/// Cached forge records and roles with generation-checked invalidation.
#[derive(Default)]
pub struct AccessCache {
    generation: AtomicU64,
    records: Mutex<HashMap<String, Entry<Option<RepoRecord>>>>,
    roles: Mutex<HashMap<(String, String), Entry<RepoRole>>>,
    grant_key: std::sync::OnceLock<[u8; 32]>,
    /// The anonymous read token and its expiry (unix seconds).
    anonymous: Mutex<Option<(String, u64)>>,
}

/// How long cached entries are fresh, and how long they may be served during an outage.
#[derive(Clone, Copy, Debug)]
pub struct CacheBounds {
    /// Freshness.
    pub ttl: Duration,
    /// Outage grace.
    pub grace: Duration,
}

enum Cached<T> {
    Fresh(T),
    Stale(T, Instant),
    Missing,
}

fn lookup<K: Eq + Hash, T: Clone>(
    map: &Mutex<HashMap<K, Entry<T>>>,
    key: &K,
    ttl: Duration,
) -> Cached<T> {
    let m = map.lock().unwrap_or_else(|p| p.into_inner());
    match m.get(key) {
        Some(e) if e.at.elapsed() < ttl => Cached::Fresh(e.value.clone()),
        Some(e) => Cached::Stale(e.value.clone(), e.at),
        None => Cached::Missing,
    }
}

impl AccessCache {
    /// The current generation (bumped by every invalidation).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Drops every cached record and role.
    pub fn invalidate_all(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.records
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        self.roles.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    /// Drops the cached record and roles of one repo (by id).
    pub fn invalidate_repo(&self, repo_id: &str) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.records
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(repo_id);
        self.roles
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(r, _), _| r != repo_id);
    }

    fn store<K: Eq + Hash, T>(
        &self,
        map: &Mutex<HashMap<K, Entry<T>>>,
        generation: u64,
        key: K,
        value: T,
    ) {
        let mut m = map.lock().unwrap_or_else(|p| p.into_inner());
        // A lookup that started before an invalidation must not bring the old answer back.
        if self.generation() != generation {
            return;
        }
        if m.len() > 100_000 {
            m.clear();
        }
        m.insert(
            key,
            Entry {
                value,
                at: Instant::now(),
            },
        );
    }
}

fn outage(what: &str) -> ApiError {
    ApiError::new(
        ErrorReason::Unavailable,
        format!("repository access cannot be decided: the forge is unavailable ({what})"),
    )
}

impl AppState {
    fn access_bounds(&self) -> CacheBounds {
        CacheBounds {
            ttl: Duration::from_millis(self.cfg.forge.access_cache_ms),
            grace: Duration::from_millis(self.cfg.forge.access_outage_grace_ms),
        }
    }

    /// The forge record of `repo` (`None` when unmanaged), cached.
    pub async fn repo_record(&self, repo: &RepoCtx) -> ApiResult<Option<RepoRecord>> {
        if self.hub.get().is_none() {
            return Ok(None);
        }
        let b = self.access_bounds();
        let key = repo.id_text();
        let stale = match lookup(&self.access.records, &key, b.ttl) {
            Cached::Fresh(v) => return Ok(v),
            Cached::Stale(v, at) => Some((v, at)),
            Cached::Missing => None,
        };
        let generation = self.access.generation();
        match self.forge.access_record(repo).await {
            Ok(v) => {
                let v = v.map(std::sync::Arc::new);
                self.access
                    .store(&self.access.records, generation, key, v.clone());
                Ok(v)
            }
            Err(e) if e.detail.retryable => match stale {
                Some((v, at)) if at.elapsed() < b.grace => Ok(v),
                _ => Err(outage(&e.detail.message)),
            },
            Err(e) => Err(e),
        }
    }

    /// `principal_id`'s forge role on the managed repo `record`, cached.
    pub async fn repo_role(&self, record: &v1::Repo, principal_id: &str) -> ApiResult<RepoRole> {
        let b = self.access_bounds();
        let key = (record.id.clone(), principal_id.to_string());
        let stale = match lookup(&self.access.roles, &key, b.ttl) {
            Cached::Fresh(v) => return Ok(v),
            Cached::Stale(v, at) => Some((v, at)),
            Cached::Missing => None,
        };
        let generation = self.access.generation();
        match self.forge.access_role(record, principal_id).await {
            Ok(v) => {
                self.access.store(&self.access.roles, generation, key, v);
                Ok(v)
            }
            Err(e) if e.detail.retryable => match stale {
                Some((v, at)) if at.elapsed() < b.grace => Ok(v),
                _ => Err(outage(&e.detail.message)),
            },
            Err(e) => Err(e),
        }
    }

    /// The roles of a principal chain (acting first) on a managed repo, with agent
    /// inheritance applied.
    pub async fn chain_roles(
        &self,
        record: &v1::Repo,
        chain: &[(String, PrincipalKind)],
    ) -> ApiResult<Vec<RepoRole>> {
        let mut roles = Vec::with_capacity(chain.len());
        for (id, _) in chain {
            roles.push(self.repo_role(record, id).await?);
        }
        let kinds: Vec<PrincipalKind> = chain.iter().map(|(_, k)| *k).collect();
        inherit_agent_roles(&kinds, &mut roles);
        Ok(roles)
    }

    /// Decides whether a principal chain (acting first) may do `action` on `repo`: the forge
    /// record's lifecycle and visibility and every principal's role. Token scope and policy
    /// are not part of it (the gate adds them).
    pub async fn authorize_chain(
        &self,
        chain: &[(String, PrincipalKind)],
        repo: &RepoCtx,
        action: Action,
    ) -> ApiResult<RepoAccess> {
        let Some(record) = self.repo_record(repo).await? else {
            return Ok(RepoAccess {
                record: None,
                role: None,
            });
        };
        let ids: Vec<String> = chain.iter().map(|(id, _)| id.clone()).collect();
        let anonymous_read = self.cfg.forge.allow_anonymous_public_read;
        if lifecycle(&record) == Lifecycle::Deleted {
            decide(&record, &ids, &[], action, anonymous_read)?;
        }
        let roles = if action == Action::Read && visibility(&record) == Visibility::Public {
            // Every authenticated principal reads a public repo.
            vec![RepoRole::Read; ids.len()]
        } else {
            self.chain_roles(&record, chain).await?
        };
        let role = decide(&record, &ids, &roles, action, anonymous_read)?;
        Ok(RepoAccess {
            record: Some(record),
            role: Some(role),
        })
    }

    /// The authoritative repo-access decision for a caller (see the module docs).
    pub async fn authorize_repo(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        action: Action,
    ) -> ApiResult<RepoAccess> {
        if caller.anonymous {
            // No credentials: the empty chain. A repo the forge keeps no record of has no
            // visibility, so it is never public.
            let record = self
                .repo_record(repo)
                .await?
                .ok_or_else(|| ApiError::not_found("repository not found"))?;
            let role = decide(
                &record,
                &[],
                &[],
                action,
                self.cfg.forge.allow_anonymous_public_read,
            )?;
            return Ok(RepoAccess {
                record: Some(record),
                role: Some(role),
            });
        }
        let mut chain: Vec<(String, PrincipalKind)> = caller
            .token
            .chain()
            .into_iter()
            .map(|p| (p.id, p.kind))
            .collect();
        // An agent token that names no delegator still acts for the agent's recorded owner
        // (SPEC §9.1: every agent has one).
        if let Some((last, PrincipalKind::Agent)) = chain.last().cloned() {
            let owners = self.principal_chain(&last).await?;
            chain.extend(owners.into_iter().skip(1));
        }
        self.authorize_chain(&chain, repo, action).await
    }

    /// `actor` as the policy of a repo sees it (SPEC §9.4): on a managed repo (`record` set) it
    /// carries `@repo-role/<role>` naming its own forge role on the repo, without agent
    /// inheritance (repo access inherits, §9.6; policy narrows agents); with no role, or on a
    /// repo the forge keeps no record of, no such group.
    pub async fn actor_on_repo(&self, record: Option<&v1::Repo>, actor: Actor) -> ApiResult<Actor> {
        let role = match record {
            Some(r) => role_group(self.repo_role(r, actor.id.as_str()).await?),
            None => None,
        };
        Ok(actor.with_repo_role(role))
    }

    /// The caller's chain as the policy of `repo` sees it: [`Caller::actors`] in the same
    /// order with the same ids, each with its `@repo-role/<role>` group
    /// ([`AppState::actor_on_repo`]). Resolved once per repo and request (the role lookups
    /// share the access cache), so path checks later in the request can use
    /// [`AppState::known_policy_actors`].
    pub async fn policy_actors(&self, caller: &Caller, repo: &RepoCtx) -> ApiResult<Arc<[Actor]>> {
        if let Some(a) = caller.resolved_actors(repo.repo.id) {
            return Ok(a);
        }
        let record = if caller.anonymous {
            None
        } else {
            self.repo_record(repo).await?
        };
        let mut out = Vec::with_capacity(caller.actors.len());
        for a in &caller.actors {
            out.push(self.actor_on_repo(record.as_deref(), a.clone()).await?);
        }
        let out: Arc<[Actor]> = out.into();
        caller.remember_actors(repo.repo.id, out.clone());
        Ok(out)
    }

    /// The caller's policy actors for `repo` where no lookup can be awaited (path filters):
    /// those [`AppState::policy_actors`] resolved earlier in the request (every gate does),
    /// else the bare chain, which holds no repo-role group and so is never allowed more.
    pub fn known_policy_actors(&self, caller: &Caller, repo: bee_store::RepoId) -> Arc<[Actor]> {
        caller
            .resolved_actors(repo)
            .unwrap_or_else(|| caller.actors.clone().into())
    }

    /// The gate's first step: a deleted repo is `NOT_FOUND`; then enterprise admission for the
    /// org that owns the repo (blocked and deprovisioned principals, IP allow lists, SSO and
    /// 2FA, so a member is told to sign in rather than told the repo does not exist); then
    /// [`AppState::authorize_repo`].
    pub async fn admit_and_authorize(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        action: Action,
    ) -> ApiResult<RepoAccess> {
        let record = self.repo_record(repo).await?;
        if let Some(r) = &record
            && lifecycle(r) == Lifecycle::Deleted
        {
            return Err(ApiError::not_found("repository not found"));
        }
        let owner = RepoAccess { record, role: None }.owner_org_id(repo);
        self.forge.admit(caller, Some(&owner)).await?;
        self.authorize_repo(caller, repo, action).await
    }

    /// Whether the caller may read `repo` (for filtering lists and search results; any
    /// refusal, including an outage, reads as no).
    pub async fn can_read_repo(&self, caller: &Caller, repo: &RepoCtx) -> bool {
        self.authorize_repo(caller, repo, Action::Read)
            .await
            .is_ok()
    }

    /// The principal chain a stored principal acts in, acting principal first: an agent is
    /// followed by its owner (and theirs), as agent tokens are minted on its owner's behalf.
    /// For principals that signed URLs name, where no token is at hand. A deactivated
    /// principal anywhere in the chain is refused.
    pub async fn principal_chain(&self, id: &str) -> ApiResult<Vec<(String, PrincipalKind)>> {
        let mut out: Vec<(String, PrincipalKind)> = Vec::new();
        let mut next = id.to_string();
        while !next.is_empty() && out.len() < 8 && !out.iter().any(|(p, _)| *p == next) {
            let rec = self.principal(&next).await?;
            if rec.as_ref().is_some_and(|r| r.deactivated) {
                return Err(ApiError::unauthenticated(format!(
                    "principal {next} is deactivated"
                )));
            }
            let kind = rec
                .as_ref()
                .map(|r| r.policy_kind())
                .unwrap_or(PrincipalKind::Human);
            out.push((next.clone(), kind));
            next = match (&rec, kind) {
                (Some(r), PrincipalKind::Agent) => r.owner.clone(),
                _ => String::new(),
            };
        }
        Ok(out)
    }

    fn grant_key(&self) -> ApiResult<[u8; 32]> {
        if let Some(k) = self.access.grant_key.get() {
            return Ok(*k);
        }
        let k = blake3::derive_key("bee blob url grant v1", &crate::server::url_key(&self.cfg)?);
        Ok(*self.access.grant_key.get_or_init(|| k))
    }

    /// Issues a signed URL for an object (SPEC §10.5) on behalf of `caller` (none for org-level
    /// artifacts) for content of `repos`. URLs this server serves itself (`/blob/...`) carry a
    /// grant (`g`, the repos and the caller's principal chain, and `gs`, its MAC bound to the
    /// URL's signature), and redeeming one decides repo access again
    /// ([`AppState::redeem_url_grant`]); URLs of an external store are bounded by
    /// `signed_url_ttl` alone.
    pub async fn issue_url(
        &self,
        org: OrgId,
        id: &ObjectId,
        ttl: Duration,
        caller: Option<&Caller>,
        repos: &[&RepoCtx],
    ) -> ApiResult<SignedUrl> {
        let issuer = self.url_issuer.as_ref().ok_or_else(|| {
            ApiError::new(
                ErrorReason::Unimplemented,
                "signed URLs are disabled on this server (blobs.signed_urls)",
            )
        })?;
        let mut url = issuer.issue(org, id, ttl).await?;
        if self.local_urls.is_some() {
            let grant = UrlGrant {
                repos: repos.iter().map(|r| r.id_text()).collect(),
                chain: caller
                    .filter(|c| !c.anonymous)
                    .map(|c| {
                        c.token
                            .chain()
                            .into_iter()
                            .map(|p| (p.id, p.kind.as_str().to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            let g = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&grant).unwrap_or_default());
            let sig = url
                .url
                .split(['?', '&'])
                .find_map(|kv| kv.strip_prefix("sig="))
                .unwrap_or_default()
                .to_string();
            let gs = blake3::keyed_hash(&self.grant_key()?, format!("{sig}\n{g}").as_bytes());
            url.url = format!("{}&g={g}&gs={}", url.url, gs.to_hex());
        }
        Ok(url)
    }

    /// Checks the grant of a locally served signed URL whose signature `sig` verified: its MAC,
    /// then repo access of its principal chain to each of its repos, decided now (a repo
    /// deleted, made private or out of the principal's reach since issue refuses it).
    pub async fn redeem_url_grant(
        &self,
        sig: &str,
        g: Option<&str>,
        gs: Option<&str>,
    ) -> ApiResult<()> {
        let (Some(g), Some(gs)) = (g, gs) else {
            return Err(ApiError::unauthenticated("signed URL carries no grant"));
        };
        let want = blake3::keyed_hash(&self.grant_key()?, format!("{sig}\n{g}").as_bytes());
        let got = blake3::Hash::from_hex(gs)
            .map_err(|_| ApiError::unauthenticated("signed URL grant is malformed"))?;
        if want != got {
            return Err(ApiError::unauthenticated(
                "signed URL grant does not match its signature",
            ));
        }
        let grant: UrlGrant = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(g)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(|| ApiError::unauthenticated("signed URL grant is malformed"))?;
        let chain: Vec<(String, PrincipalKind)> = grant
            .chain
            .iter()
            .map(|(id, k)| {
                (
                    id.clone(),
                    PrincipalKind::parse(k).unwrap_or(PrincipalKind::Human),
                )
            })
            .collect();
        for id in &grant.repos {
            let r = v1::RepoRef {
                repo: Some(v1::repo_ref::Repo::Id(id.clone())),
            };
            let repo = self.repo(Some(&r)).await?;
            self.authorize_chain(&chain, &repo, Action::Read).await?;
        }
        Ok(())
    }

    /// The server's anonymous read token (see the module docs): read-only, for
    /// [`ANONYMOUS_PRINCIPAL`], minted on first use and again before it expires. `None` while
    /// `forge.allow_anonymous_public_read` is off. Never returned to a client.
    pub fn anonymous_token(&self) -> ApiResult<Option<String>> {
        if !self.cfg.forge.allow_anonymous_public_read {
            return Ok(None);
        }
        let now = self.now_ms() / 1000;
        let mut slot = self
            .access
            .anonymous
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some((text, exp)) = slot.as_ref()
            && *exp > now + ANONYMOUS_TOKEN_SECS / 10
        {
            return Ok(Some(text.clone()));
        }
        let exp = now + ANONYMOUS_TOKEN_SECS;
        let scope = bee_auth::Scope::new()
            .actions([Action::Read])
            .expires_at(exp)
            .session("anonymous");
        let token = self
            .ring
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .mint(
                &bee_auth::MintRequest {
                    principal: bee_auth::Principal::human(ANONYMOUS_PRINCIPAL, "anonymous"),
                    on_behalf_of: Vec::new(),
                    scope,
                },
                now,
            )
            .map_err(ApiError::from)?;
        *slot = Some((token.text.clone(), exp));
        Ok(Some(token.text))
    }

    /// Forgets the anonymous read token (after the signing keys change).
    pub fn forget_anonymous_token(&self) {
        *self
            .access
            .anonymous
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// Drops every cached access decision and repo resolution (after a change to repo
    /// records, memberships, teams, collaborators or installations).
    pub fn forget_access(&self) {
        self.access.invalidate_all();
        self.forget_repo_names();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(vis: Visibility, archived: bool, deleted: bool) -> v1::Repo {
        v1::Repo {
            id: "R".into(),
            org_id: "O".into(),
            full_name: "o/r".into(),
            visibility: vis as i32,
            archived,
            deleted_at: deleted.then(pbjson_types::Timestamp::default),
            ..Default::default()
        }
    }

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("p{i}")).collect()
    }

    #[test]
    fn private_repos_hide_from_principals_without_read() {
        let r = rec(Visibility::Private, false, false);
        let e = decide(&r, &ids(1), &[RepoRole::None], Action::Read, true).unwrap_err();
        assert_eq!(e.detail.reason, "NOT_FOUND");
        let e = decide(&r, &ids(1), &[RepoRole::None], Action::Commit, true).unwrap_err();
        assert_eq!(e.detail.reason, "NOT_FOUND");
        assert_eq!(
            decide(&r, &ids(1), &[RepoRole::Read], Action::Read, true).unwrap(),
            RepoRole::Read
        );
        let e = decide(&r, &ids(1), &[RepoRole::Read], Action::Commit, true).unwrap_err();
        assert_eq!(e.detail.reason, "PERMISSION_DENIED");
        assert!(decide(&r, &ids(1), &[RepoRole::Write], Action::Land, true).is_ok());
        let e = decide(&r, &ids(1), &[RepoRole::Write], Action::Redact, true).unwrap_err();
        assert_eq!(e.detail.reason, "PERMISSION_DENIED");
        assert!(decide(&r, &ids(1), &[RepoRole::Admin], Action::Redact, true).is_ok());
    }

    #[test]
    fn public_repos_read_for_everyone_but_write_by_role() {
        let r = rec(Visibility::Public, false, false);
        assert!(decide(&r, &ids(1), &[RepoRole::None], Action::Read, true).is_ok());
        let e = decide(&r, &ids(1), &[RepoRole::None], Action::Commit, true).unwrap_err();
        assert_eq!(e.detail.reason, "PERMISSION_DENIED");
    }

    #[test]
    fn archived_repos_read_but_refuse_every_write() {
        let r = rec(Visibility::Private, true, false);
        assert!(decide(&r, &ids(1), &[RepoRole::Admin], Action::Read, true).is_ok());
        for a in [
            Action::Commit,
            Action::Land,
            Action::ManageRefs,
            Action::Lease,
            Action::Redact,
        ] {
            let e = decide(&r, &ids(1), &[RepoRole::Admin], a, true).unwrap_err();
            assert_eq!(e.detail.reason, "ARCHIVED", "{a:?}");
        }
        // Existence is still hidden from callers who cannot read it.
        let e = decide(&r, &ids(1), &[RepoRole::None], Action::Commit, true).unwrap_err();
        assert_eq!(e.detail.reason, "NOT_FOUND");
    }

    #[test]
    fn deleted_repos_are_not_found_for_everyone() {
        let r = rec(Visibility::Public, false, true);
        for a in [Action::Read, Action::Commit, Action::ManageSettings] {
            let e = decide(&r, &ids(1), &[RepoRole::Admin], a, true).unwrap_err();
            assert_eq!(e.detail.reason, "NOT_FOUND");
        }
    }

    #[test]
    fn the_weakest_principal_of_the_chain_decides() {
        let r = rec(Visibility::Private, false, false);
        let e = decide(
            &r,
            &ids(2),
            &[RepoRole::Admin, RepoRole::Read],
            Action::Commit,
            true,
        )
        .unwrap_err();
        assert_eq!(e.detail.reason, "PERMISSION_DENIED");
        assert!(e.detail.message.starts_with("p1 "));
        let e = decide(
            &r,
            &ids(2),
            &[RepoRole::Admin, RepoRole::None],
            Action::Read,
            true,
        )
        .unwrap_err();
        assert_eq!(e.detail.reason, "NOT_FOUND");
        assert!(decide(&r, &[], &[], Action::Read, true).is_err());
    }

    #[test]
    fn an_empty_chain_reads_public_repos_and_nothing_else() {
        let public = rec(Visibility::Public, false, false);
        assert_eq!(
            decide(&public, &[], &[], Action::Read, true).unwrap(),
            RepoRole::Read
        );
        // Archived public repos read as usual.
        let archived = rec(Visibility::Public, true, false);
        assert!(decide(&archived, &[], &[], Action::Read, true).is_ok());
        // Every other action is refused, exactly as before anonymous read existed.
        for a in [
            Action::Commit,
            Action::Land,
            Action::ManageRefs,
            Action::Lease,
            Action::Redact,
            Action::ManageSettings,
        ] {
            let e = decide(&public, &[], &[], a, true).unwrap_err();
            assert_eq!(e.detail.reason, "NOT_FOUND", "{a:?}");
        }
        for vis in [
            Visibility::Private,
            Visibility::Internal,
            Visibility::Unspecified,
        ] {
            let r = rec(vis, false, false);
            let e = decide(&r, &[], &[], Action::Read, true).unwrap_err();
            assert_eq!(e.detail.reason, "NOT_FOUND", "{vis:?}");
        }
        let deleted = rec(Visibility::Public, false, true);
        let e = decide(&deleted, &[], &[], Action::Read, true).unwrap_err();
        assert_eq!(e.detail.reason, "NOT_FOUND");
    }

    #[test]
    fn anonymous_read_off_hides_public_repos_from_an_empty_chain() {
        let public = rec(Visibility::Public, false, false);
        let e = decide(&public, &[], &[], Action::Read, false).unwrap_err();
        assert_eq!(e.detail.reason, "NOT_FOUND");
        let e = decide(&public, &[], &[], Action::Commit, false).unwrap_err();
        assert_eq!(e.detail.reason, "NOT_FOUND");
        // Signed-in principals are unaffected by the switch.
        assert!(decide(&public, &ids(1), &[RepoRole::None], Action::Read, false).is_ok());
    }

    #[test]
    fn anonymous_methods_are_reads_only() {
        assert!(anonymous_read_method("/bee.v1.QueryService/Log"));
        assert!(anonymous_read_method(
            "/bee.v1.SearchService/ListLeaderboard"
        ));
        for m in [
            "/bee.v1.WriteService/Land",
            "/bee.v1.WriteService/UploadObjects",
            "/bee.v1.RefService/UpdateRef",
            "/bee.v1.UserService/FollowUser",
            "/bee.v1.UserService/StarRepo",
            "/bee.v1.UserService/GetViewer",
            "/bee.v1.SearchService/CreateSavedSearch",
            "/bee.v1.SearchService/ListSavedSearches",
            "/bee.v1.AuthService/MintToken",
            // The repo tabs' reads that stay closed to signed-out visitors: org membership,
            // agents, tokens, principals (they carry external ids), security alerts (Write).
            "/bee.v1.AgentService/ListAgentPrincipals",
            "/bee.v1.AgentService/ListAgentWorkItems",
            "/bee.v1.OrgService/ListMembers",
            "/bee.v1.AuthService/ListTokens",
            "/bee.v1.AuthService/GetPrincipal",
            "/bee.v1.SecurityService/ListDependencyAlerts",
            "/bee.v1.SecurityService/ListSecretScanningAlerts",
            // Event log paging and the writes beside the newly open reads.
            "/bee.v1.ChangeService/CreateChange",
            "/bee.v1.ChangeService/EnqueueChange",
            "/bee.v1.LeaseService/AcquireLease",
            "/bee.v1.IssueService/CreateIssue",
            "/bee.v1.WorkflowService/RerunWorkflowRun",
            // Following, liking, reposting and writing a post need an account.
            "/bee.v1.PostService/CreatePost",
            "/bee.v1.PostService/ListFollowingFeed",
            "/bee.v1.PostService/LikePost",
            "/bee.v1.PostService/RepostPost",
            "/bee.v1.UserService/FollowRepo",
        ] {
            assert!(!anonymous_read_method(m), "{m}");
        }
        // The tabs of a public repo are open.
        for m in [
            "/bee.v1.RepoService/GetContext",
            "/bee.v1.ChangeService/GetChange",
            "/bee.v1.WorkflowService/TailWorkflowJobLogs",
            "/bee.v1.IssueService/GetIssueByNumber",
            "/bee.v1.EventService/Subscribe",
            "/bee.v1.PostService/GetPost",
            "/bee.v1.PostService/ListRepoPosts",
        ] {
            assert!(anonymous_read_method(m), "{m}");
        }
        // Every listed method names a served RPC.
        let served: std::collections::BTreeSet<&str> =
            bee_proto::methods::METHODS.iter().map(|m| m.path).collect();
        for m in ANONYMOUS_READ_METHODS {
            assert!(served.contains(m), "{m} is not an RPC");
        }
    }

    #[test]
    fn repo_role_groups_name_a_role_or_nothing() {
        assert_eq!(role_group(RepoRole::Admin), Some("admin"));
        assert_eq!(role_group(RepoRole::Maintain), Some("maintain"));
        assert_eq!(role_group(RepoRole::Read), Some("read"));
        assert_eq!(role_group(RepoRole::None), None);
        assert_eq!(role_group(RepoRole::Unspecified), None);
    }

    #[test]
    fn agents_act_with_the_higher_of_their_role_and_their_owners() {
        use PrincipalKind::{Agent, Human};
        let mut roles = vec![RepoRole::None, RepoRole::None, RepoRole::Write];
        inherit_agent_roles(&[Agent, Agent, Human], &mut roles);
        assert_eq!(roles, vec![RepoRole::Write; 3]);
        // An agent made an org member (base role read) still acts with its owner's role.
        let mut roles = vec![RepoRole::Read, RepoRole::Admin];
        inherit_agent_roles(&[Agent, Human], &mut roles);
        assert_eq!(roles, vec![RepoRole::Admin, RepoRole::Admin]);
        // An agent's own higher role never lifts the chain: its owner still decides.
        let mut roles = vec![RepoRole::Admin, RepoRole::None];
        inherit_agent_roles(&[Agent, Human], &mut roles);
        assert_eq!(roles, vec![RepoRole::Admin, RepoRole::None]);
        let r = v1::Repo {
            visibility: Visibility::Private as i32,
            ..Default::default()
        };
        let e = decide(&r, &ids(2), &roles, Action::Read, true).unwrap_err();
        assert_eq!(e.detail.reason, "NOT_FOUND");
        // A human never inherits.
        let mut roles = vec![RepoRole::None, RepoRole::Admin];
        inherit_agent_roles(&[Human, Human], &mut roles);
        assert_eq!(roles, vec![RepoRole::None, RepoRole::Admin]);
    }

    #[test]
    fn invalidation_wins_over_a_lookup_that_started_before_it() {
        let c = AccessCache::default();
        let g = c.generation();
        c.invalidate_repo("R");
        c.store(&c.roles, g, ("R".into(), "p".into()), RepoRole::Admin);
        assert!(matches!(
            lookup(
                &c.roles,
                &("R".to_string(), "p".to_string()),
                Duration::from_secs(1)
            ),
            Cached::Missing
        ));
        let g = c.generation();
        c.store(&c.roles, g, ("R".into(), "p".into()), RepoRole::Admin);
        assert!(matches!(
            lookup(
                &c.roles,
                &("R".to_string(), "p".to_string()),
                Duration::from_secs(1)
            ),
            Cached::Fresh(RepoRole::Admin)
        ));
        c.invalidate_all();
        assert!(matches!(
            lookup(
                &c.roles,
                &("R".to_string(), "p".to_string()),
                Duration::from_secs(1)
            ),
            Cached::Missing
        ));
    }
}
