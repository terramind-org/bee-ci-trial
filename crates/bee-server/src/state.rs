//! Shared server state and the authorization helpers every RPC uses.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bee_auth::{Budget, BudgetCharge, Request as AuthRequest, VerifiedToken};
use bee_core::{ObjectId, ObjectKind, RefName};
use bee_derived::{Derived, Leases, Registry};
use bee_history::History;
use bee_policy::{Action, Actor, CheckResult, LeaseInfo, Policy, PrincipalId, Review};
use bee_proto::v1::{self, ErrorReason};
use bee_secrets::Scanner;
use bee_store::{
    BlobStore, Clock, MetaStore, MinVersion, OrgRow, ReadFrom, RefRow, RepoId, RepoRow, Writer,
};
use bee_store_blob::{LocalUrlIssuer, SignedUrlIssuer};
use tonic::metadata::MetadataMap;

use crate::admission::Admission;
use crate::audit::{AuditEntry, AuditSink};
use crate::auth::{
    Caller, Ledger, NS_PRINCIPAL, PrincipalRecord, RateLimiter, Revocations, SharedKeyRing,
    TokenCache, Usage,
};
use crate::coalescer::Coalescers;
use crate::config::Config;
use crate::errors::{ApiError, ApiResult};
use crate::events::EventLog;
use crate::kv::Kv;
use crate::metrics::Metrics;
use crate::policy::{PolicyCache, builtin_org_default};

tokio::task_local! {
    /// Effective request deadline and the configured per-lookup forge deadline.
    pub static REQUEST_FORGE_DEADLINES: (Instant, Duration);
}

/// Per-request facts set by the transport layer.
#[derive(Clone, Debug)]
pub struct RequestMeta {
    /// Correlates logs, audit entries and error details.
    pub request_id: String,
    /// When the request must be finished.
    pub deadline: Instant,
    /// The gRPC path.
    pub method: String,
}

impl RequestMeta {
    /// The facts of a request (defaults when it did not pass through the layer).
    pub fn of<T>(req: &tonic::Request<T>) -> RequestMeta {
        req.extensions()
            .get::<RequestMeta>()
            .cloned()
            .unwrap_or_else(|| RequestMeta {
                request_id: crate::util::random_hex(8),
                deadline: Instant::now() + Duration::from_secs(30),
                method: String::new(),
            })
    }

    /// Time left before the deadline.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// A resolved repository.
#[derive(Clone, Debug)]
pub struct RepoCtx {
    /// The org row.
    pub org: OrgRow,
    /// The repo row.
    pub repo: RepoRow,
    /// `org/name`.
    pub full_name: String,
}

impl RepoCtx {
    /// The repo id text.
    pub fn id_text(&self) -> String {
        self.repo.id.0.to_text()
    }
}

/// A land waiting for post-land warming: the repo, the new tip and the landed changesets.
pub type WarmLand = (RepoCtx, ObjectId, Vec<ObjectId>);

/// Repos whose shared history rows are kept at once; past it the map starts over.
const HISTORY_ROWS_REPOS: usize = 256;

/// Forge inputs to land policy (reviews, checks, leases). The forge plugs its own
/// implementation in; without a forge, land policy is evaluated with none of them (so rules
/// that require approvals or checks deny, as they must).
#[async_trait]
pub trait ForgeInputs: Send + Sync {
    /// Locks the organization safety flag through a non-human ref publication.
    async fn fence_agent_landing(
        &self,
        _repo: &RepoCtx,
    ) -> ApiResult<Option<bee_forge::safety::control::LandFence>> {
        Ok(None)
    }

    /// Records the result of a native land for its verified run, if it has one.
    async fn note_safety_land(
        &self,
        _caller: &crate::auth::Caller,
        _repo: &RepoCtx,
        _effect: &str,
        _error: Option<&ApiError>, // Safety needs the typed denial, including the freeze rule.
    ) {
    }

    /// Copies a successful native governing change to Beehive's durable audit outbox.
    async fn note_safety_leash(
        &self,
        _caller: &crate::auth::Caller,
        _action: &str,
        _target: &str,
        _detail: &serde_json::Value,
    ) -> ApiResult<()> {
        Ok(())
    }

    /// Attributes a newly stored native changeset to its verified run, if it has one.
    async fn note_safety_changeset(
        &self,
        _caller: &crate::auth::Caller,
        _repo: &RepoCtx,
        _id: &ObjectId,
    ) {
    }

    /// Refuses a publication that has not reached the merge queue's landing step.
    async fn require_landing_queue_entry(
        &self,
        _repo: &RepoCtx,
        _ref_name: &str,
        _head: &ObjectId,
        _change_request_id: &str,
    ) -> ApiResult<()> {
        Ok(())
    }

    /// Inputs for landing `head` on `ref_name`, optionally for a change request.
    async fn land_inputs(
        &self,
        repo: RepoId,
        ref_name: &str,
        head: &ObjectId,
        change_request_id: &str,
    ) -> ApiResult<LandInputs>;

    /// After a land published changes (`change_request_id` set when it landed on behalf of
    /// a change request, which the forge then records itself). Never fails the land.
    async fn landed(
        &self,
        _caller: &crate::auth::Caller,
        _repo: &RepoCtx,
        _ref_name: &str,
        _resp: &v1::LandResponse,
        _change_request_id: &str,
    ) {
    }

    /// After a land failed with a merge conflict.
    async fn land_conflict(
        &self,
        _repo: &RepoCtx,
        _ref_name: &str,
        _head: &ObjectId,
        _change_request_id: &str,
        _conflicts: &v1::ConflictSet,
    ) {
    }

    /// After a non-protected ref moved (RefService writes and git pushes; `push_options`
    /// are a git push's `-o` values).
    async fn ref_moved(
        &self,
        _repo: &RepoCtx,
        _ref_name: &str,
        _target: Option<&ObjectId>,
        _caller: &crate::auth::Caller,
        _push_options: &[String],
        _via_git: bool,
    ) {
    }

    /// A state-changing call on `repo` was denied (`action` names it, e.g. `vcs.commit`):
    /// the forge appends it to the org's audit log. Never fails the call.
    async fn denied(
        &self,
        _caller: &crate::auth::Caller,
        _repo: &RepoCtx,
        _action: &str,
        _err: &ApiError,
    ) {
    }

    /// Records a completed org-scoped store operation in the forge audit log.
    async fn admin_audit(
        &self,
        _caller: &crate::auth::Caller,
        _org_id: &str,
        _target: &str,
        _action: &str,
        _detail: serde_json::Value,
    ) {
    }

    /// Records a push-protection refusal in the forge audit log without file contents.
    async fn secret_refusal(
        &self,
        _caller: &crate::auth::Caller,
        _repo: &RepoCtx,
        _action: &str,
        _err: &ApiError,
    ) {
    }

    /// Enterprise admission of an authenticated call touching `org_id` (IP allow lists,
    /// deprovisioned principals, SSO and 2FA).
    async fn admit(&self, _caller: &crate::auth::Caller, _org_id: Option<&str>) -> ApiResult<()> {
        Ok(())
    }

    /// Whether the repo's security configuration turns on forge push protection (so callers
    /// read file contents only when it does).
    async fn protects(&self, _repo: &RepoCtx) -> bool {
        false
    }

    /// Forge push protection (secret scanning with the repo's custom patterns and delegated
    /// bypass requests) over `files` (path, bytes, blob id): `SECRET_DETECTED` blocks the
    /// write. Called at CreateChangeset, Land and git push.
    async fn protect(
        &self,
        _caller: &crate::auth::Caller,
        _repo: &RepoCtx,
        _files: &[(String, Vec<u8>, Option<ObjectId>)],
        _bypass_id: Option<&str>,
    ) -> ApiResult<()> {
        Ok(())
    }

    /// The policy groups a principal holds through forge teams (`<org>/<team>`).
    async fn team_groups(&self, _principal: &str) -> Vec<String> {
        Vec::new()
    }

    /// Open code-scanning alerts on the code a land publishes (`head`, and the change
    /// request's head when given) or on `ref_name`, for the code-scanning land rule.
    async fn code_alerts(
        &self,
        _repo: RepoId,
        _ref_name: &str,
        _head: &ObjectId,
        _change_request_id: &str,
    ) -> ApiResult<Vec<bee_policy::CodeAlert>> {
        Ok(Vec::new())
    }

    /// Verifies an armored commit or tag signature over `payload` against the forge's
    /// signing-key registry; `None` without a forge (there is no registry).
    async fn verify_signature(
        &self,
        _payload: &[u8],
        _armored: &[u8],
    ) -> ApiResult<Option<bee_forge::community::keys::SignatureVerdict>> {
        Ok(None)
    }

    /// The forge's record of `repo` for repo-access decisions (`crate::access`): `None` when
    /// the forge keeps none (a VCS-only repo), an error with a retryable reason when the
    /// forge cannot answer. A repo in a soft-deleted org reads as deleted.
    async fn access_record(&self, _repo: &RepoCtx) -> ApiResult<Option<v1::Repo>> {
        Ok(None)
    }

    /// `principal_id`'s effective role on the forge repo `repo` (org roles and base role,
    /// collaborators, teams, installations, fork access, public and internal read).
    async fn access_role(&self, _repo: &v1::Repo, _principal_id: &str) -> ApiResult<v1::RepoRole> {
        Ok(v1::RepoRole::None)
    }

    /// The id of the live forge repo called `full_name` (following rename and transfer
    /// redirects), if the forge knows one (`Ok(None)` when it does not, an error when it
    /// cannot answer).
    async fn repo_by_full_name(&self, _full_name: &str) -> ApiResult<Option<String>> {
        Ok(None)
    }

    /// Owners and admins of an org in the forge, who administer its repos under the
    /// built-in org default policy (empty when unknown).
    async fn org_admins(&self, _org_id: &str) -> Vec<String> {
        Vec::new()
    }

    /// Billing of `bytes` of new storage in `repo`: a check before (`record = false`) and
    /// the usage record after (`record = true`).
    async fn spend(
        &self,
        _caller: &crate::auth::Caller,
        _repo: &RepoCtx,
        _bytes: u64,
        _record: bool,
    ) -> ApiResult<()> {
        Ok(())
    }
}

/// What the forge knows about a land.
#[derive(Clone, Debug, Default)]
pub struct LandInputs {
    /// Reviews, chronological.
    pub reviews: Vec<Review>,
    /// Check results for the head.
    pub checks: Vec<CheckResult>,
    /// Live leases on the ref.
    pub leases: Vec<LeaseInfo>,
    /// Who pushed the head revision.
    pub last_pusher: Option<PrincipalId>,
    /// Enforced org-required workflows for the head.
    pub required_workflows: Vec<bee_policy::WorkflowGate>,
}

/// The default: no forge data.
pub struct NoForge;

#[async_trait]
impl ForgeInputs for NoForge {
    async fn land_inputs(
        &self,
        _: RepoId,
        _: &str,
        _: &ObjectId,
        _: &str,
    ) -> ApiResult<LandInputs> {
        Ok(LandInputs::default())
    }
}

/// Everything the services share.
pub struct AppState {
    /// Configuration.
    pub cfg: Config,
    /// Metadata store.
    pub meta: Arc<dyn MetaStore>,
    /// Blob store.
    pub blobs: Arc<dyn BlobStore>,
    /// Write-ordering protocol.
    pub writer: Writer,
    /// Derived types.
    pub registry: Arc<Registry>,
    /// Derivation leases shared by every request.
    pub derive_leases: Arc<Leases>,
    /// Changesets and graph nodes read so far, per repo, shared by every request's
    /// [`History`] handle (both are immutable under their id).
    pub history_rows: Mutex<BTreeMap<RepoId, Arc<bee_history::SharedRows>>>,
    /// Lands waiting for post-land warming, per repo. A repo is present while its one warming
    /// task runs; lands arriving meanwhile queue here and the task takes them in turn.
    pub warm_queue: Mutex<BTreeMap<RepoId, Vec<WarmLand>>>,
    /// Server-owned tables.
    pub kv: Arc<dyn Kv>,
    /// Token signing keys.
    pub ring: SharedKeyRing,
    /// Revocations.
    pub revocations: Arc<Revocations>,
    /// Verified-token cache.
    pub tokens: TokenCache,
    /// Budgets and quotas.
    pub ledger: Ledger,
    /// Request-rate quotas.
    pub limiter: RateLimiter,
    /// JWKS documents of the external OIDC identity provider.
    pub jwks: crate::login::JwksCache,
    /// The ES256 key that signs id_tokens when this server is an OIDC provider.
    pub oidc_key: std::sync::OnceLock<Arc<crate::oidc::OidcKey>>,
    /// The git gateway the listener serves, once the server has mounted it: post-land warming
    /// fills its pack cache (`land.warm_pack`).
    pub git_gateway: std::sync::OnceLock<Arc<bee_git_http::GitGateway>>,
    /// Effective policies.
    pub policies: PolicyCache,
    /// Audit log.
    pub audit: Arc<dyn AuditSink>,
    /// Non-ref events.
    pub events: EventLog,
    /// Metrics.
    pub metrics: Arc<Metrics>,
    /// Admission control.
    pub admission: Admission,
    /// Injected clock.
    pub clock: Arc<dyn Clock>,
    /// Signed URL issuer (SPEC §10.5), when enabled.
    pub url_issuer: Option<Arc<dyn SignedUrlIssuer>>,
    /// The local issuer, whose route `/blob/...` this server serves.
    pub local_urls: Option<Arc<LocalUrlIssuer>>,
    /// Secret scanner for push protection.
    pub scanner: Scanner,
    /// Land coalescers.
    pub coalescers: Coalescers,
    /// Forge land inputs and VCS-path hooks.
    pub forge: Arc<dyn ForgeInputs>,
    /// The built-in forge, once open (unset when `forge.enabled` is false).
    pub hub: Arc<crate::forge::ForgeLink>,
    /// The public base URL (set once the listener is bound).
    pub public_url: RwLock<String>,
    /// This process's id (coalescer owner identity).
    pub instance_id: String,
    /// Signing keys known to this server (the ring itself cannot be listed).
    pub keys: RwLock<Vec<KeyInfo>>,
    /// Rendered API rulesets per repo (`services::policy`).
    pub rulesets: crate::services::policy::RulesetCache,
    /// Recent RPC spans and metric samples (`services::ops`).
    pub ops: crate::services::ops::OpsState,
    /// OpenTelemetry span export, when `tracing.otlp_endpoint` is set.
    pub otel: Option<Arc<crate::otel::Exporter>>,
    /// Cached repo-access decision inputs (`crate::access`).
    pub access: crate::access::AccessCache,
    /// Last known ref versions, served marked stale during a metadata outage (SPEC §15.1).
    pub last_refs: crate::stale::LastKnownRefs,
    started_ms: u64,
    repos: Mutex<HashMap<String, (RepoCtx, Instant)>>,
}

/// What the server reports about one signing key.
#[derive(Clone, Debug)]
pub struct KeyInfo {
    /// Key id.
    pub id: u32,
    /// Ed25519 public key bytes.
    pub public_key: Vec<u8>,
    /// When it was added.
    pub created_ms: u64,
    /// When it was retired (0 = active).
    pub retired_ms: u64,
}

/// Builder inputs for [`AppState`].
pub struct StateParts {
    /// Configuration.
    pub cfg: Config,
    /// Metadata store.
    pub meta: Arc<dyn MetaStore>,
    /// Blob store.
    pub blobs: Arc<dyn BlobStore>,
    /// Server-owned tables.
    pub kv: Arc<dyn Kv>,
    /// Keys.
    pub ring: SharedKeyRing,
    /// Clock.
    pub clock: Arc<dyn Clock>,
    /// Signed URL issuer.
    pub url_issuer: Option<Arc<dyn SignedUrlIssuer>>,
    /// Local URL issuer.
    pub local_urls: Option<Arc<LocalUrlIssuer>>,
    /// Audit sink.
    pub audit: Arc<dyn AuditSink>,
    /// Metrics.
    pub metrics: Arc<Metrics>,
    /// Forge inputs.
    pub forge: Arc<dyn ForgeInputs>,
    /// The built-in forge link.
    pub hub: Arc<crate::forge::ForgeLink>,
    /// Signing key metadata.
    pub keys: Vec<KeyInfo>,
}

impl AppState {
    /// Assembles the state.
    pub fn new(mut p: StateParts) -> ApiResult<Arc<AppState>> {
        p.blobs = Arc::new(crate::integrity::IntegrityWatch::new(
            p.blobs,
            p.metrics.clone(),
            p.clock.clone(),
        ));
        let writer = Writer::new(p.meta.clone(), p.blobs.clone());
        let limiter = RateLimiter::new(
            p.cfg.admission.per_principal_rps,
            p.cfg.admission.per_root_rps,
        );
        let admission = Admission::new(&p.cfg.admission);
        let instance_id = crate::util::random_hex(8);
        let otel = (!p.cfg.tracing.otlp_endpoint.is_empty()).then(|| {
            Arc::new(crate::otel::Exporter::new(
                &p.cfg.tracing.otlp_endpoint,
                &p.cfg.tracing.service_name,
                &instance_id,
                &p.cfg.server.cell,
            ))
        });
        Ok(Arc::new(AppState {
            otel,
            events: EventLog::new(p.meta.clone(), p.metrics.clone()),
            coalescers: Coalescers::default(),
            instance_id,
            writer,
            registry: Arc::new(Registry::standard()),
            derive_leases: Arc::new(Leases::new()),
            history_rows: Mutex::new(BTreeMap::new()),
            warm_queue: Mutex::new(BTreeMap::new()),
            revocations: Arc::new(Revocations::default()),
            tokens: TokenCache::default(),
            ledger: Ledger::default(),
            limiter,
            jwks: crate::login::JwksCache::default(),
            oidc_key: std::sync::OnceLock::new(),
            git_gateway: std::sync::OnceLock::new(),
            policies: PolicyCache::default(),
            admission,
            scanner: Scanner::builtin(),
            public_url: RwLock::new(String::new()),
            repos: Mutex::new(HashMap::new()),
            keys: RwLock::new(p.keys),
            rulesets: crate::services::policy::RulesetCache::default(),
            ops: crate::services::ops::OpsState::default(),
            access: crate::access::AccessCache::default(),
            last_refs: crate::stale::LastKnownRefs::default(),
            started_ms: p.clock.now_ms(),
            cfg: p.cfg,
            meta: p.meta,
            blobs: p.blobs,
            kv: p.kv,
            ring: p.ring,
            clock: p.clock,
            url_issuer: p.url_issuer,
            local_urls: p.local_urls,
            audit: p.audit,
            metrics: p.metrics,
            forge: p.forge,
            hub: p.hub,
        }))
    }

    /// When this process started.
    pub fn started_ms(&self) -> u64 {
        self.started_ms
    }

    /// The signing keys.
    pub fn key_infos(&self) -> Vec<KeyInfo> {
        self.keys.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Now, from the injected clock.
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The public base URL.
    pub fn public_url(&self) -> String {
        self.public_url
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The host in the resolved public URL, without a port.
    pub fn public_url_host(&self) -> String {
        ::http::Uri::try_from(self.public_url().as_str())
            .ok()
            .and_then(|uri| uri.host().map(str::to_owned))
            .unwrap_or_else(|| "beehive.invalid".to_string())
    }

    // ----- identity ----------------------------------------------------------------

    /// The bearer token of a request, if any.
    pub fn bearer(md: &MetadataMap) -> Option<String> {
        let v = md.get("authorization")?.to_str().ok()?;
        let t = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))?
            .trim();
        (!t.is_empty()).then(|| t.to_string())
    }

    /// A principal record.
    pub async fn principal(&self, id: &str) -> ApiResult<Option<PrincipalRecord>> {
        Ok(self
            .kv
            .get(NS_PRINCIPAL, id.as_bytes())
            .await?
            .and_then(|b| serde_json::from_slice(&b).ok()))
    }

    /// Authenticates a token (verification, revocation, expiry, deactivation).
    pub async fn verify_token(
        &self,
        token: &str,
    ) -> ApiResult<(VerifiedToken, Option<PrincipalRecord>)> {
        let now = self.now_ms() / 1000;
        let v = self
            .tokens
            .verify(&self.ring, &self.revocations, token, now)?;
        let record = self.principal(&v.principal.id).await?;
        for p in v.chain() {
            let r = if p.id == v.principal.id {
                record.clone()
            } else {
                self.principal(&p.id).await?
            };
            if r.as_ref().is_some_and(|r| r.deactivated) {
                return Err(ApiError::unauthenticated(format!(
                    "principal {} is deactivated",
                    p.id
                )));
            }
        }
        Ok((v, record))
    }

    /// A principal's external policy groups (SPEC §9.4 `@org/team`, F49-041): the groups
    /// stored on its record (GitHub team sync at login) plus the forge teams it belongs to.
    pub async fn groups_of(&self, principal: &str, stored: Vec<String>) -> Vec<String> {
        let mut groups = stored;
        for g in self.forge.team_groups(principal).await {
            if !groups.contains(&g) {
                groups.push(g);
            }
        }
        // `repo-role/*` carries the forge repo role and is computed per repo by
        // `AppState::policy_actors` only; a stored group or a team of an org named
        // `repo-role` must not pose as one.
        groups.retain(|g| !bee_policy::is_repo_role_group(g));
        groups.sort();
        groups
    }

    /// Charges `extra` more rate-limit tokens to the caller's principal and delegation root
    /// (bulk reads, SPEC §15.4); `RESOURCE_EXHAUSTED` with `retry-after` when either bucket
    /// cannot pay.
    pub fn charge_rate(&self, caller: &Caller, extra: u64) -> ApiResult<()> {
        if extra == 0 {
            return Ok(());
        }
        let v = &caller.token;
        if let Err((quota, wait)) =
            self.limiter
                .take_n(&v.principal.id, &v.delegation_root().id, extra)
        {
            self.metrics.shed("bulk", quota);
            return Err(ApiError::throttled(
                format!("rate quota {quota} exceeded by a bulk read"),
                wait,
                quota,
            ));
        }
        Ok(())
    }

    /// The authenticated caller of a request, after rate quotas.
    pub async fn caller(&self, md: &MetadataMap) -> ApiResult<Caller> {
        let token = Self::bearer(md).ok_or_else(|| {
            ApiError::unauthenticated("this call needs `Authorization: Bearer <token>`")
        })?;
        let (v, record) = self.verify_token(&token).await?;
        let ip = md
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .unwrap_or_default()
            .trim()
            .to_string();
        // The anonymous read token (`crate::access`): only while the server allows it, and
        // rate-limited per client address rather than as one shared principal.
        let anonymous = v.principal.id == crate::access::ANONYMOUS_PRINCIPAL;
        if anonymous && !self.cfg.forge.allow_anonymous_public_read {
            return Err(ApiError::unauthenticated(
                "this call needs `Authorization: Bearer <token>`",
            ));
        }
        let (bucket, root) = if anonymous {
            let k = format!("{}:{ip}", crate::access::ANONYMOUS_PRINCIPAL);
            (k.clone(), k)
        } else {
            (v.principal.id.clone(), v.delegation_root().id.clone())
        };
        if let Err((quota, wait)) = self.limiter.take(&bucket, &root) {
            self.metrics.shed("interactive", quota);
            return Err(ApiError::throttled(
                format!("rate quota {quota} exceeded"),
                wait,
                quota,
            ));
        }
        let mut actors: Vec<Actor> = Vec::new();
        for p in v.chain() {
            let stored = if p.id == v.principal.id {
                record
                    .as_ref()
                    .map(|r| r.groups.clone())
                    .unwrap_or_default()
            } else {
                self.principal(&p.id)
                    .await?
                    .map(|r| r.groups)
                    .unwrap_or_default()
            };
            let groups = self.groups_of(&p.id, stored).await;
            actors.push(p.to_actor(groups, Vec::new()));
        }
        Ok(Caller {
            token: v,
            token_text: token,
            record,
            actors,
            ip,
            anonymous,
            repo_actors: Default::default(),
        })
    }

    // ----- repos -------------------------------------------------------------------

    /// Resolves a repo reference (`NOT_FOUND` when unknown). With the forge running, a name
    /// resolves through the forge first (so renames and transfers, with their redirects, and
    /// forks, whose VCS repo lives in their network's namespace, resolve to the repo's id), a
    /// VCS name the forge has since given to nothing resolves to nothing, and the full name
    /// is the forge's. Resolutions are reused for `forge.access_cache_ms` and dropped by
    /// [`AppState::forget_access`].
    pub async fn repo(&self, r: Option<&v1::RepoRef>) -> ApiResult<RepoCtx> {
        let r = r
            .and_then(|r| r.repo.as_ref())
            .ok_or_else(|| ApiError::invalid("missing_field", "repo is required"))?;
        let key = match r {
            v1::repo_ref::Repo::Id(id) => format!("id:{}", id.to_ascii_uppercase()),
            v1::repo_ref::Repo::FullName(n) => format!("name:{n}"),
        };
        let forge_on = self.hub.get().is_some();
        let ttl = Duration::from_millis(self.cfg.forge.access_cache_ms);
        if let Some((c, at)) = self
            .repos
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            && (!forge_on || at.elapsed() < ttl)
        {
            return Ok(c.clone());
        }
        let not_found = || ApiError::not_found("repository not found");
        let mut asked_name: Option<&str> = None;
        let row = match r {
            v1::repo_ref::Repo::Id(id) => {
                let ulid = bee_core::Ulid::parse(id).map_err(ApiError::from)?;
                self.meta
                    .get_repo(RepoId(ulid))
                    .await?
                    .ok_or_else(not_found)?
            }
            v1::repo_ref::Repo::FullName(n) => {
                let (o, name) = n.split_once('/').ok_or_else(|| {
                    ApiError::invalid("invalid_field", "repo full_name must be `org/name`")
                })?;
                let mut found = None;
                if forge_on
                    && let Ok(Some(id)) = self.forge.repo_by_full_name(n).await
                    && let Ok(u) = bee_core::Ulid::parse(&id)
                {
                    found = self.meta.get_repo(RepoId(u)).await?;
                }
                if found.is_none() {
                    asked_name = Some(n.as_str());
                    found = match self.meta.org_by_name(o).await? {
                        Some(org) => self.meta.repo_by_name(org.id, name).await?,
                        None => None,
                    };
                }
                found.ok_or_else(not_found)?
            }
        };
        let org = self.meta.get_org(row.org).await?.ok_or_else(not_found)?;
        let mut ctx = RepoCtx {
            full_name: format!("{}/{}", org.name, row.name),
            org,
            repo: row,
        };
        if forge_on
            && let Ok(Some(rec)) = self.repo_record(&ctx).await
            && rec.id == ctx.id_text()
        {
            // A VCS name of a repo the forge now calls something else (a rename or transfer
            // whose redirect has expired) names nothing.
            if asked_name.is_some_and(|n| !rec.full_name.eq_ignore_ascii_case(n)) {
                return Err(not_found());
            }
            if !rec.full_name.is_empty() {
                ctx.full_name = rec.full_name.clone();
            }
        }
        let mut m = self.repos.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() > 100_000 {
            m.clear();
        }
        m.insert(key, (ctx.clone(), Instant::now()));
        Ok(ctx)
    }

    /// Forgets every cached repo resolution.
    pub fn forget_repo_names(&self) {
        self.repos.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    /// A history handle for a repo, writing the registry's eager rows.
    pub fn history(&self, repo: &RepoCtx) -> History {
        let history = History::new(self.writer.clone(), repo.org.id, repo.repo.id)
            .with_eager(self.registry.clone());
        if !self.cfg.server.share_history_rows {
            return history;
        }
        let rows = {
            let mut all = self.history_rows.lock().unwrap_or_else(|p| p.into_inner());
            if all.len() >= HISTORY_ROWS_REPOS && !all.contains_key(&repo.repo.id) {
                all.clear();
            }
            all.entry(repo.repo.id).or_default().clone()
        };
        history.with_shared_rows(rows)
    }

    /// A history handle that reads replicas at or past `min` (SPEC §6.3).
    pub fn history_min(&self, repo: &RepoCtx, min: Option<MinVersion>) -> History {
        self.history(repo).with_min_version(min)
    }

    /// The derived-data engine for a repo.
    pub fn derived(&self, repo: &RepoCtx) -> Derived {
        Derived::new(Arc::new(self.history(repo)), self.registry.clone())
            .with_leases(self.derive_leases.clone())
    }

    // ----- policy ------------------------------------------------------------------

    /// The org default policy text.
    pub async fn org_text(&self, org: &OrgRow) -> ApiResult<String> {
        // The admin list it embeds can change through another server process sharing the
        // store, whose invalidation this one never sees: reuse the text for at most
        // `forge.access_cache_ms`, the bound repo access already has (docs/scaling.md).
        let max_age = std::time::Duration::from_millis(self.cfg.forge.access_cache_ms);
        if let Some(t) = self.policies.org_text(org.id, max_age) {
            return Ok(t);
        }
        let text = if self.cfg.policy.org_default_file.is_empty() {
            let prefix = format!("{}/", org.id.0.to_text());
            let rows = self
                .kv
                .scan(crate::auth::NS_ORG_ADMIN, prefix.as_bytes(), None, 1000)
                .await?;
            let mut admins: Vec<String> = rows
                .into_iter()
                .map(|(k, _)| String::from_utf8_lossy(&k[prefix.len()..]).into_owned())
                .collect();
            admins.extend(self.forge.org_admins(&org.id.0.to_text()).await);
            admins.sort();
            admins.dedup();
            builtin_org_default(&admins)
        } else {
            std::fs::read_to_string(&self.cfg.policy.org_default_file).map_err(|e| {
                ApiError::new(
                    ErrorReason::Internal,
                    format!("cannot read org policy: {e}"),
                )
            })?
        };
        self.policies.set_org_text(org.id, text.clone());
        Ok(text)
    }

    /// The org whose default policy governs `repo`: the org that owns it in the forge, which
    /// differs from its VCS org for a fork (whose VCS repo lives in its network's namespace)
    /// and for a transferred repo (whose VCS repo stays in the namespace it was created in);
    /// the VCS org for a repo the forge keeps no record of.
    pub async fn policy_org(&self, repo: &RepoCtx) -> ApiResult<OrgRow> {
        if let Ok(Some(rec)) = self.repo_record(repo).await
            && rec.org_id != repo.org.id.0.to_text()
            && let Ok(u) = bee_core::Ulid::parse(&rec.org_id)
            && let Some(o) = self.meta.get_org(bee_store::OrgId(u)).await?
        {
            return Ok(o);
        }
        Ok(repo.org.clone())
    }

    /// The effective policy for `ref_name` at its current tip (repo-wide decisions use
    /// `main`).
    pub async fn policy(&self, repo: &RepoCtx, ref_name: &str) -> ApiResult<Arc<Policy>> {
        let name = RefName::new(ref_name).map_err(ApiError::from)?;
        let mut tip = self
            .read_ref(repo.repo.id, &name)
            .await?
            .0
            .and_then(|r| r.target);
        if tip.is_none() && ref_name != "main" {
            // A ref that does not exist yet is governed by the repo's main policy.
            let main = RefName::new("main").map_err(ApiError::from)?;
            tip = self
                .read_ref(repo.repo.id, &main)
                .await?
                .0
                .and_then(|r| r.target);
        }
        self.policy_at(repo, ref_name, tip.as_ref()).await
    }

    /// Reads a ref from the primary (and remembers it). While the primary is unavailable,
    /// answers with the last version this process read, if at most
    /// `metadata.stale_read_max_ms` old, and `true` for stale (SPEC §15.1); otherwise the
    /// read fails as the store did.
    pub async fn read_ref(
        &self,
        repo: RepoId,
        name: &RefName,
    ) -> ApiResult<(Option<RefRow>, bool)> {
        let (read, from_memory) = crate::outage_cache::SERVED_STALE
            .scope(std::cell::Cell::new(false), async {
                let r = self.meta.get_ref(repo, name).await;
                (r, crate::outage_cache::SERVED_STALE.with(|c| c.get()))
            })
            .await;
        match read {
            // Answered by the outage cache: the primary is down and the row is stale.
            Ok(row) if from_memory => Ok((row, true)),
            Ok(row) => {
                if let Some(r) = &row {
                    self.last_refs.record(repo, r, None, self.now_ms());
                }
                Ok((row, false))
            }
            Err(e) if e.reason == bee_core::Reason::Unavailable => {
                let max = self.cfg.metadata.stale_read_max_ms;
                match (max > 0)
                    .then(|| self.last_refs.stale(repo, name, self.now_ms(), max))
                    .flatten()
                {
                    Some((row, _)) => Ok((Some(row), true)),
                    None => Err(e.into()),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The effective policy for `ref_name` at a known tip.
    pub async fn policy_at(
        &self,
        repo: &RepoCtx,
        ref_name: &str,
        tip: Option<&ObjectId>,
    ) -> ApiResult<Arc<Policy>> {
        let org_text = crate::services::policy::effective_org_text(self, repo).await?;
        let history = self.history(repo);
        let policy = self
            .policies
            .get(&history, &org_text, ref_name, tip)
            .await?;
        if !policy.has_property_conditions() {
            return Ok(policy);
        }
        // Rulesets limited by repo custom properties (F416-020) apply only when the repo's
        // current values match; property changes take effect without a policy change.
        let props = crate::services::policy::repo_property_values(self, repo).await;
        Ok(Arc::new(policy.for_repo_properties(&props)))
    }

    /// The policy epoch and redaction epoch of a repo (SPEC §9.5).
    pub async fn epochs(&self, repo: &RepoCtx, policy: &Policy) -> ApiResult<(u64, u64)> {
        use bee_auth::RevocationList;
        let policy_epoch = bee_policy::repo_epoch(policy.epoch(), self.revocations.counter());
        Ok((policy_epoch, self.redaction_epoch(repo).await?))
    }

    /// The redaction epoch: the store's counter plus quarantine-phase bumps.
    /// While the metadata primary is unavailable, the store's part is the last one read if
    /// it is within `metadata.stale_read_max_ms` (SPEC §15.1: ref reads carry these epochs).
    pub async fn redaction_epoch(&self, repo: &RepoCtx) -> ApiResult<u64> {
        let id = repo.repo.id;
        let store = match self.meta.redaction_epoch(id).await {
            Ok(e) => {
                self.last_refs.record_epoch(id, e, self.now_ms());
                e
            }
            Err(e) if e.reason == bee_core::Reason::Unavailable => {
                let max = self.cfg.metadata.stale_read_max_ms;
                match (max > 0)
                    .then(|| self.last_refs.stale_epoch(id, self.now_ms(), max))
                    .flatten()
                {
                    Some(v) => v,
                    None => return Err(e.into()),
                }
            }
            Err(e) => return Err(e.into()),
        };
        let bumps =
            crate::auth::read_counter(self.kv.as_ref(), &format!("redaction/{}", repo.id_text()))
                .await?;
        Ok(store + bumps)
    }

    /// Puts the epoch trailers on a response.
    pub async fn with_epochs<T>(
        &self,
        repo: &RepoCtx,
        policy: &Policy,
        mut resp: tonic::Response<T>,
    ) -> ApiResult<tonic::Response<T>> {
        let (p, r) = self.epochs(repo, policy).await?;
        if let Ok(v) = p.to_string().parse() {
            resp.metadata_mut().insert("bee-policy-epoch", v);
        }
        if let Ok(v) = r.to_string().parse() {
            resp.metadata_mut().insert("bee-redaction-epoch", v);
        }
        Ok(resp)
    }

    // ----- authorization -------------------------------------------------------------

    /// The token half of the repo gate: `req` (built for the full name) on `repo`, with the
    /// repo named by full name or by id under SPEC §9.2's one rule
    /// ([`bee_auth::VerifiedToken::authorize_repo`]), as the forge and git surfaces do.
    pub(crate) fn token_check(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        req: AuthRequest,
    ) -> ApiResult<()> {
        let now = self.now_ms() / 1000;
        caller
            .token
            .authorize_repo(
                &repo.full_name,
                &repo.id_text(),
                |key| {
                    let mut r = req.clone();
                    r.repo = key;
                    r
                },
                now,
            )
            .map_err(ApiError::from)
    }

    /// Repo-level gate: the token allows `action` on the repo (and ref), and every principal
    /// in the chain holds a role granting it. Returns the policy used.
    /// Denials of state-changing actions are appended to the forge audit log (`vcs.<action>`,
    /// `result` = the reason) so `AuditService` shows them.
    pub async fn gate(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        action: Action,
        ref_name: Option<&str>,
    ) -> ApiResult<Arc<Policy>> {
        let out = self.gate_checked(caller, repo, action, ref_name).await;
        if let Err(e) = &out
            && action != Action::Read
            && matches!(
                e.detail.reason.as_str(),
                "PERMISSION_DENIED" | "POLICY_DENIED"
            )
        {
            self.forge
                .denied(caller, repo, &format!("vcs.{}", action.as_str()), e)
                .await;
        }
        out
    }

    async fn gate_checked(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        action: Action,
        ref_name: Option<&str>,
    ) -> ApiResult<Arc<Policy>> {
        self.admit_and_authorize(caller, repo, action).await?;
        // Every principal of the chain is checked with its forge repo role as
        // `@repo-role/<role>` (SPEC §9.4); later path checks of the request reuse these.
        let actors = self.policy_actors(caller, repo).await?;
        let mut req = AuthRequest::new(repo.full_name.clone(), action);
        if let Some(r) = ref_name {
            req = req.on_ref(r);
        }
        self.token_check(caller, repo, req)?;
        let policy = self
            .policy(repo, ref_name.filter(|r| !r.is_empty()).unwrap_or("main"))
            .await?;
        for a in actors.iter() {
            if !policy.actions_of(a).contains(&action) {
                return Err(ApiError::policy(
                    ErrorReason::PolicyDenied,
                    "members",
                    vec![format!("role:{}", action.as_str())],
                    format!("{} holds no role granting `{}`", a.id, action.as_str()),
                ));
            }
        }
        Ok(policy)
    }

    /// The token half of [`AppState::gate`] only (the policy decision is made by the caller,
    /// for example with ref rules). Returns the policy of `ref_name` (or `main`).
    pub async fn gate_token_only(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        action: Action,
        ref_name: Option<&str>,
    ) -> ApiResult<Arc<Policy>> {
        self.admit_and_authorize(caller, repo, action).await?;
        // The caller's policy decision uses `policy_actors` (resolved here for the request).
        self.policy_actors(caller, repo).await?;
        let mut req = AuthRequest::new(repo.full_name.clone(), action);
        if let Some(r) = ref_name {
            req = req.on_ref(r);
        }
        self.token_check(caller, repo, req)?;
        self.policy(repo, ref_name.filter(|r| !r.is_empty()).unwrap_or("main"))
            .await
    }

    /// Whether the caller may do `action` on `path` of `repo` (token scope and policy, the
    /// chain as [`AppState::known_policy_actors`]), for filtering.
    pub fn path_allowed(
        &self,
        caller: &Caller,
        repo: RepoId,
        policy: &Policy,
        action: Action,
        path: &str,
    ) -> bool {
        let req = AuthRequest::new(String::new(), action).paths([path.to_string()]);
        let mut scope = caller.token.scope.clone();
        scope.repos = None;
        scope.check(&req).is_ok()
            && policy
                .check_access_chain(&self.known_policy_actors(caller, repo), action, path)
                .allowed
    }

    /// [`AppState::path_allowed`] for many paths of one request: the token scope and the
    /// policy's view of the chain are prepared once. `actors` is the caller's chain as the
    /// repo's policy sees it ([`AppState::known_policy_actors`]).
    pub fn path_filter<'a>(
        &self,
        caller: &'a Caller,
        actors: &'a [Actor],
        policy: &'a Policy,
        action: Action,
    ) -> impl Fn(&str) -> bool + 'a {
        let mut scope = caller.token.scope.clone();
        scope.repos = None;
        let token_ok = scope
            .check(&AuthRequest::new(String::new(), action))
            .is_ok();
        let access = policy.access_filter(actors, action);
        move |path: &str| token_ok && scope.allows_path(action, path) && access.allows(path)
    }

    /// Requires `action` on every path (token Datalog and scope, then policy).
    pub fn require_paths(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        policy: &Policy,
        action: Action,
        paths: &[String],
        ref_name: Option<&str>,
    ) -> ApiResult<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut req = AuthRequest::new(repo.full_name.clone(), action).paths(paths.iter().cloned());
        if let Some(r) = ref_name {
            req = req.on_ref(r);
        }
        self.token_check(caller, repo, req)?;
        let actors = self.known_policy_actors(caller, repo.repo.id);
        for p in paths {
            let d = policy.check_access_chain(&actors, action, p);
            if !d.allowed {
                return Err(ApiError::policy(
                    ErrorReason::PolicyDenied,
                    &d.rule,
                    vec![format!("access:{}:{p}", action.as_str())],
                    d.detail,
                ));
            }
        }
        Ok(())
    }

    /// Whether an object is readable through `repo` (SPEC §9.3): some `object_paths` row at a
    /// readable path, or an upload by the caller that nothing references yet.
    pub async fn object_readable(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        policy: &Policy,
        id: &ObjectId,
    ) -> ApiResult<bool> {
        match id.kind() {
            ObjectKind::Changeset => Ok(self
                .meta
                .get_changeset(repo.repo.id, id, ReadFrom::Primary)
                .await?
                .is_some()),
            ObjectKind::Blob | ObjectKind::Tree => {
                let paths = self
                    .meta
                    .object_paths(repo.repo.id, id, ReadFrom::Primary)
                    .await?;
                for p in &paths {
                    let ok = match p {
                        None => true,
                        Some(p) => {
                            let p = p.to_string();
                            self.path_allowed(caller, repo.repo.id, policy, Action::Read, &p)
                        }
                    };
                    if ok {
                        return Ok(true);
                    }
                }
                if !paths.is_empty() {
                    return Ok(false);
                }
                self.uploaded_by(caller, repo, id, false).await
            }
            ObjectKind::Meta => self.uploaded_by(caller, repo, id, true).await,
            _ => Ok(false),
        }
    }

    async fn uploaded_by(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        id: &ObjectId,
        any_in_repo: bool,
    ) -> ApiResult<bool> {
        let ups = self.meta.get_uploads(repo.org.id, id).await?;
        Ok(ups
            .iter()
            .any(|u| u.principal == caller.id() || (any_in_repo && u.repo == Some(repo.repo.id))))
    }

    // ----- budgets -------------------------------------------------------------------

    /// Charges usage to every budgeted token block and to the principal and delegation-root
    /// quotas.
    pub async fn charge(&self, caller: &Caller, usage: Usage) -> ApiResult<()> {
        let a = &self.cfg.admission;
        let nz = |v: u64| (v > 0).then_some(v);
        let root = Budget {
            max_changesets: nz(a.root_max_changesets),
            max_landed: nz(a.root_max_landed),
            max_upload_bytes: nz(a.root_max_upload_bytes),
        };
        let mut charges: Vec<BudgetCharge> = caller.token.budget_charges();
        charges.extend(caller.token.quota_charges(Budget::default(), root));
        self.ledger.charge(self.kv.as_ref(), &charges, usage).await
    }

    // ----- audit -------------------------------------------------------------------

    /// Writes an audit entry for a state change.
    #[allow(clippy::too_many_arguments)]
    pub async fn audit(
        &self,
        caller: &Caller,
        meta: &RequestMeta,
        repo: Option<&RepoCtx>,
        target: &str,
        decision: &str,
        detail: serde_json::Value,
    ) -> ApiResult<u64> {
        let seq = self
            .audit
            .record(AuditEntry {
                at_ms: self.now_ms(),
                chain: caller.chain_ids(),
                action: meta.method.clone(),
                repo: repo.map(RepoCtx::id_text).unwrap_or_default(),
                target: target.to_string(),
                decision: decision.to_string(),
                detail: detail.clone(),
                request_id: meta.request_id.clone(),
                ..AuditEntry::default()
            })
            .await?;
        if bee_forge::safety::leash_audit_action(decision) {
            self.forge
                .note_safety_leash(caller, decision, target, &detail)
                .await?;
        }
        Ok(seq)
    }
}
