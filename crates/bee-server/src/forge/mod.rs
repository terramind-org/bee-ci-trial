//! The Beehive forge, wired into the server (`docs/spec/server.md` §8).
//!
//! [`ForgeWiring::open`] opens the forge store (its own database: SQLite files under
//! `<data>/forge`, or schema `bee_forge` of the metadata Postgres database), builds every
//! forge module over the server's VCS-backed ports ([`adapters`]), and installs the change
//! requests' coalescer-backed lander ([`landing`]). [`ForgeWiring::register`] mounts every
//! forge service on the registry (gRPC, HTTP/JSON and `/mcp` at once), [`http`] serves SCIM,
//! SAML and OAuth over plain HTTP, and [`ForgeWiring::spawn`] starts the workers: outbox relay,
//! webhook delivery, notification fan-out, maintenance, the VCS event bridge and the merge
//! queue.
//!
//! **Failure domain.** The forge is a separate failure domain (SPEC §15.1): the VCS data path
//! only reaches it through [`ForgeLink`], whose every call is bounded by a short deadline and
//! fails open on retryable forge errors (no admission veto, no land inputs, no post-land
//! bookkeeping). When the forge database is down, reads, writes and lands keep working.

pub mod access;
pub mod adapters;
pub mod admission;
pub mod agents;
pub mod compat;
pub mod facts;
pub mod forks;
pub mod http;
pub mod import;
pub mod landing;
pub mod late;
pub mod local_runner;
pub mod mirrors;
pub mod packages;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bee_core::ObjectId;
use bee_forge::agents::service::AgentApi;
use bee_forge::agents::{AgentPorts, AgentWorkers, Agents, AgentsConfig};
use bee_forge::bisect::service::BisectCheckService;
use bee_forge::bisect::{BisectConfig, Bisector};
use bee_forge::changes::service::ChangeServices;
use bee_forge::changes::vcs::HistoryVcs;
use bee_forge::changes::{ChangeHooks, Changes};
use bee_forge::ci::{Ci, CiConfig, CiHooks, CiServices, HistoryCiVcs, StoreCiBlobs};
use bee_forge::community::ports::{HmacSigner, Ports};
use bee_forge::enterprise::apps::{Apps, AppsConfig, AppsService};
use bee_forge::enterprise::billing::{Billing, BillingConfig, BillingService, UsageIngestor};
use bee_forge::enterprise::common::{ScopeResolver, SpendGate, TokenRevoker};
use bee_forge::enterprise::models::{Models, ModelsService};
use bee_forge::enterprise::moderation::{
    Moderation, ModerationConfig, ModerationService, StandingProvider, WriteCheck, WriteKind,
};
use bee_forge::enterprise::network::AdmissionRequest;
use bee_forge::enterprise::service::EnterpriseApi;
use bee_forge::enterprise::{Enterprise, EnterpriseConfig};
use bee_forge::events::bridge::VcsBridge;
use bee_forge::forge::{ForgeConfig, Hooks, Workers};
use bee_forge::security::vcs::HistorySecurityVcs;
use bee_forge::security::{Ports as SecurityPorts, Security, SecurityServices};
use bee_forge::service::ForgeServices;
use bee_forge::store::ForgeStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::work::{AllAgents, WorkHooks};
use bee_forge::{Cx, Forge, ForgeError, TokenIssuer};
use bee_proto::v1::{self, ErrorReason, Principal, PrincipalKind};
use bee_store::RepoId;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::auth::Caller;
use crate::errors::{ApiError, ApiResult};
use crate::logging::{self, Level};
use crate::services::ServiceRegistry;
use crate::state::{AppState, ForgeInputs, LandInputs, RepoCtx};

use self::adapters::{
    ServerActivity, ServerBranches, ServerContent, ServerDirectory, ServerPolicy,
    ServerProvisioner, ServerResolver, ServerTags, ServerTemplates,
};
use self::landing::{QueueListener, ServerLander};
use self::late::{Late, LateChangeSource, LateWorkChanges};

/// Mints forge tokens from the same refreshed ring used by server authentication.
/// The forge is built once per replica, while the shared ring may rotate later.
struct ServerTokenIssuer {
    state: Weak<AppState>,
}

impl ServerTokenIssuer {
    fn new(state: &Arc<AppState>) -> Self {
        Self {
            state: Arc::downgrade(state),
        }
    }
}

impl TokenIssuer for ServerTokenIssuer {
    fn mint(
        &self,
        request: &bee_auth::MintRequest,
        now: u64,
    ) -> bee_forge::Result<bee_auth::Token> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| ForgeError::unavailable("server stopping"))?;
        state
            .ring
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .mint(request, now)
            .map_err(ForgeError::from)
    }
}

/// A forge error as a server error with the same reason, subcode and payload.
pub fn forge_to_api(e: ForgeError) -> ApiError {
    let mut a = ApiError::new(e.reason, e.message);
    a.detail.subcode = e.subcode;
    a.detail.retry_after_ms = e.retry_after_ms;
    a.detail.payload = e.payload.map(|p| *p);
    a
}

/// A server error as a forge error with the same reason, subcode and payload.
pub fn api_to_forge(e: ApiError) -> ForgeError {
    let d = *e.detail;
    let reason = bee_proto::errors::parse_reason(&d.reason).unwrap_or(ErrorReason::Internal);
    let mut f = ForgeError::new(reason, d.message).with_subcode(&d.subcode);
    if let Some(p) = d.payload {
        f = f.with_payload(p);
    }
    f
}

/// A gRPC status (from an in-process call) as a forge error.
pub fn status_to_forge(s: &tonic::Status) -> ForgeError {
    match bee_proto::errors::detail_from_status(s) {
        Some(d) => {
            let reason =
                bee_proto::errors::parse_reason(&d.reason).unwrap_or(ErrorReason::Internal);
            let mut f = ForgeError::new(reason, d.message).with_subcode(&d.subcode);
            if let Some(p) = d.payload {
                f = f.with_payload(p);
            }
            f
        }
        None => ForgeError::new(
            bee_proto::errors::default_reason(s.code()),
            s.message().to_string(),
        ),
    }
}

/// Short alias used by the adapters: an [`ApiError`] as a [`ForgeError`].
pub(crate) fn fe(e: impl Into<ApiError>) -> ForgeError {
    api_to_forge(e.into())
}

/// The forge's database: `forge.database` when set (`sqlite:<dir>` or a `postgres://` URL
/// with `?schema=`), else SQLite under `<data>/forge` in SQLite mode and schema
/// `bee_forge` of the metadata database in Postgres mode.
pub async fn open_store(state: &AppState) -> ApiResult<Arc<dyn ForgeStore>> {
    let cfg = &state.cfg;
    let sql = if !cfg.forge.database.is_empty() {
        SqlConfig::parse(&cfg.forge.database).map_err(forge_to_api)?
    } else if cfg.metadata.backend == "postgres" {
        let schema = if cfg.metadata.postgres_schema.is_empty() {
            "bee_forge".to_string()
        } else {
            format!("{}_forge", cfg.metadata.postgres_schema)
        };
        SqlConfig::postgres(cfg.metadata.postgres_url.clone(), schema)
    } else {
        SqlConfig::sqlite(cfg.server.data_dir.join("forge"))
    };
    let store = SqlStore::open(sql, state.clock.clone())
        .await
        .map_err(forge_to_api)?;
    Ok(Arc::new(store))
}

/// Account age from the principal id (a ULID's time) and prior contributions from landed
/// change requests, for moderation's interaction limits and first-time contributor rules.
struct ServerStanding {
    changes: Late<Changes>,
    forge: Late<Forge>,
}

#[async_trait]
impl StandingProvider for ServerStanding {
    async fn account_created_ms(&self, principal_id: &str) -> Option<u64> {
        bee_core::Ulid::parse(principal_id)
            .ok()
            .map(|u| u.timestamp_ms())
    }

    async fn has_contribution(&self, repo_id: &str, principal_id: &str) -> bool {
        let (Some(changes), Some(forge)) = (self.changes.get(), self.forge.get()) else {
            return false;
        };
        let cx = landing::quick_cx();
        let Ok(repo) = forge.orgs.repo_any(&cx, repo_id).await else {
            return false;
        };
        let f = bee_forge::changes::query::ListFilter {
            state: v1::ChangeState::Landed,
            author_id: principal_id.to_string(),
            ..Default::default()
        };
        changes
            .list(&cx, &repo, &f, "", 1)
            .await
            .map(|(v, _)| !v.is_empty())
            .unwrap_or(false)
    }
}

/// Drops the cached org default policy when org membership changes, so forge owners and
/// admins administer the org's repos on the VCS side too.
struct PolicyInvalidator {
    state: Weak<AppState>,
}

#[async_trait]
impl bee_forge::events::EventListener for PolicyInvalidator {
    async fn on_event(&self, event: &v1::Event) {
        if !(event.kind.starts_with("member.") || event.kind == "org.created") {
            return;
        }
        let (Some(state), Ok(id)) = (self.state.upgrade(), bee_core::Ulid::parse(&event.org_id))
        else {
            return;
        };
        state.policies.forget_org(bee_store::OrgId(id));
    }
}

/// Revokes tokens minted by apps (installation and OAuth tokens) in the server's revocation
/// list, so the server and the forge refuse them at once.
struct ServerRevoker {
    state: Weak<AppState>,
}

#[async_trait]
impl TokenRevoker for ServerRevoker {
    async fn revoke(&self, token_id: &str) -> std::result::Result<(), ForgeError> {
        let Some(state) = self.state.upgrade() else {
            return Err(ForgeError::unavailable(
                "token revocation store unavailable",
            ));
        };
        let id = bee_auth::TokenId(token_id.to_string());
        let now = state.now_ms();
        state
            .revocations
            .revoke(state.kv.as_ref(), &id, now)
            .await
            .map(|_| ())
            .map_err(|e| {
                logging::log(
                    Level::Warn,
                    "forge",
                    "revoking an app token failed",
                    &[("error", e.detail.message.clone().into())],
                );
                ForgeError::unavailable(format!("token revocation store: {}", e.detail.message))
            })
    }
}

/// Every forge module the server runs.
pub struct ForgeWiring {
    state: Weak<AppState>,
    /// The forge core (store, orgs, leases, checks, events, audit, work, community).
    pub forge: Arc<Forge>,
    /// Change requests.
    pub changes: Arc<Changes>,
    /// Posts on change requests, their media and the Following feed.
    pub posts: Arc<bee_forge::posts::Posts>,
    /// Briefs on change requests: evidence and its signed `/_evidence/...` transfers.
    pub briefs: Arc<bee_forge::briefs::Briefs>,
    /// Teams, roles, enterprise accounts, network controls, SSO, SCIM.
    pub enterprise: Arc<Enterprise>,
    /// Blocks, limits, locks, flood defenses, takedowns.
    pub moderation: Arc<Moderation>,
    /// Usage, budgets, plans, invoices.
    pub billing: Arc<Billing>,
    /// Apps, installations, OAuth, marketplace.
    pub apps: Arc<Apps>,
    /// Model catalog and policy.
    pub models: Arc<Models>,
    /// The principal directory the forge reads.
    pub directory: Arc<ServerDirectory>,
    /// The forge store (tests take it down to prove the failure domain).
    pub store: Arc<dyn ForgeStore>,
    /// GitHub imports this process runs.
    pub github: import::GithubImports,
    /// Workflows and runners.
    pub ci: Arc<Ci>,
    /// Fork networks, sync and fork change requests.
    pub forks: Arc<bee_forge::forks::Forks>,
    /// Post-land bisect and automatic revert.
    pub bisector: Arc<Bisector>,
    /// Dependency graph, alerts, secret and code scanning, advisories.
    pub security: Arc<Security>,
    /// Agents, runs and workspaces.
    pub agents: Arc<Agents>,
    agent_workers: Mutex<Option<AgentWorkers>>,
    /// The package registry protocols (OCI, npm, Maven, NuGet, RubyGems, Cargo).
    pub registry: bee_forge::packages::PackageRegistry,
    ci_worker: Mutex<Option<(tokio::sync::watch::Sender<bool>, JoinHandle<()>)>>,
    bridge: VcsBridge,
    dirty: Mutex<HashSet<(String, String)>>,
    bridge_wake: Notify,
    admitted: Mutex<HashMap<String, Instant>>,
    team_groups: Mutex<HashMap<String, (Instant, Vec<String>)>>,
    /// Versioned cache of repository merge-queue settings mirrored from shared server KV.
    merge_queue_settings: Arc<Mutex<HashMap<String, (u64, bool)>>>,
    queue: Arc<Mutex<BTreeSet<(String, String)>>>,
    queue_wake: Arc<Notify>,
    workers: Mutex<Option<Workers>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl ForgeWiring {
    /// Opens the forge on `store` and builds every module over `state`.
    pub async fn open(
        state: &Arc<AppState>,
        store: Arc<dyn ForgeStore>,
    ) -> ApiResult<Arc<ForgeWiring>> {
        let clock = state.clock.clone();
        let vcs = Arc::new(HistoryVcs::new(state.writer.clone(), clock.clone()));
        let directory = Arc::new(ServerDirectory::new(state.clone()));
        let late_changes: Late<Changes> = Late::new();
        let late_work = Late::new();
        let late_forge: Late<Forge> = Late::new();
        let late_teams: Late<bee_forge::enterprise::teams::Teams> = Late::new();
        let late_ci: Late<Ci> = Late::new();
        let signer_key = blake3::derive_key(
            "bee forge signed urls v1",
            &crate::server::url_key(&state.cfg)?,
        );
        let ports = Ports {
            vcs: Arc::new(ServerContent::new(state.clone(), vcs.clone())),
            tags: Arc::new(ServerTags::new(state.clone())),
            changes: Arc::new(LateChangeSource(late_changes.clone())),
            activity: Arc::new(ServerActivity::new(
                state.clone(),
                late_changes.clone(),
                late_work.clone(),
            )),
            signer: Arc::new(HmacSigner::new(&state.public_url(), &signer_key)),
            ..Ports::default()
        };
        let hooks = Hooks {
            directory: directory.clone(),
            provisioner: Arc::new(ServerProvisioner::new(state.clone(), vcs.clone())),
            resolver: Arc::new(ServerResolver::new(state.clone())),
            policy: Some(Arc::new(ServerPolicy::new(state.clone()))),
            community: ports,
            packages: bee_forge::packages::ports::PackagePorts {
                blobs: Arc::new(packages::ServerPackageBlobs::new(state)),
                signer: Arc::new(HmacSigner::new(&state.public_url(), &signer_key)),
                teams: Arc::new(packages::TeamPorts(late_teams.clone())),
                ..Default::default()
            },
            pages: bee_forge::pages::ports::PagesPorts {
                dns: if state.cfg.forge.pages_memory_dns {
                    Arc::new(bee_forge::pages::ports::MemoryDns::new())
                } else {
                    Arc::new(bee_forge::pages::ports::DetachedDns)
                },
                acme: if state.cfg.forge.pages_memory_dns {
                    Arc::new(bee_forge::pages::ports::MemoryAcme::new(
                        state.clock.clone(),
                    ))
                } else {
                    Arc::new(bee_forge::pages::ports::DetachedAcme)
                },
                artifacts: Arc::new(packages::CiArtifacts(late_ci.clone())),
                gate: Arc::new(packages::EnvironmentGate(late_ci.clone())),
                ..Default::default()
            },
        };
        let keys = state
            .ring
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .verifying_keys();
        let base = state.public_url().trim_end_matches('/').to_string();
        let host = ::http::Uri::try_from(base.as_str())
            .ok()
            .and_then(|uri| uri.host().map(str::to_owned))
            .unwrap_or_else(|| "beehive.invalid".to_string());
        // The strict all-targets clippy gate also checks this server configuration.
        let mut community = bee_forge::community::CommunityConfig {
            base_url: base.clone(),
            ..Default::default()
        };
        community.notify.base_url = base.clone();
        community.notify.reply_domain = format!("reply.{host}");
        community.notify.from_address = format!("notifications@{host}");
        community.users.base_url = base.clone();
        community.users.noreply_domain = format!("users.noreply.{host}");
        let config = ForgeConfig {
            relay_poll: Duration::from_millis(state.cfg.forge.relay_poll_ms.max(10)),
            webhook_poll: Duration::from_millis(state.cfg.forge.webhook_poll_ms.max(10)),
            packages: bee_forge::packages::PackagesConfig {
                base_url: base.clone(),
                ..Default::default()
            },
            community,
            pages: bee_forge::pages::PagesConfig {
                pages_domain: state.cfg.forge.pages_domain.clone(),
                ..Default::default()
            },
            ..ForgeConfig::default()
        };
        let (forge, report) = Forge::open(
            store.clone(),
            clock,
            keys,
            state.revocations.clone(),
            hooks,
            config,
        )
        .await
        .map_err(forge_to_api)?;
        if !report.pending_contract.is_empty() {
            logging::log(
                Level::Info,
                "forge",
                "contract migrations pending",
                &[("count", report.pending_contract.len().into())],
            );
        }
        late_work.set(forge.work.clone());
        late_forge.set(forge.clone());

        let mut enterprise_cfg = EnterpriseConfig::new(&state.public_url());
        enterprise_cfg.two_factor = Arc::new(crate::services::second_factor::KvTwoFactor::new(
            state.kv.clone(),
        ));
        let enterprise = Enterprise::new(forge.clone(), enterprise_cfg);
        enterprise.attach();
        late_teams.set(enterprise.teams.clone());
        let standing = Arc::new(ServerStanding {
            changes: late_changes.clone(),
            forge: late_forge.clone(),
        });
        let moderation = Moderation::new(
            forge.clone(),
            ModerationConfig::new(standing)
                .with_site_admins(state.cfg.forge.site_admins.iter().cloned())
                .with_teams(enterprise.teams.clone()),
        );
        forge.set_write_guard(moderation.clone());
        let billing = Billing::new(
            forge.clone(),
            BillingConfig {
                enabled: state.cfg.forge.billing_enabled,
                invoice_url_key: signer_key.to_vec(),
                invoice_base_url: state.public_url(),
                program_admins: state.cfg.forge.site_admins.iter().cloned().collect(),
                ..BillingConfig::default()
            },
        );
        let resolver: Arc<dyn ScopeResolver> = enterprise.accounts.clone();
        billing.set_scope_resolver(resolver);
        forge
            .events
            .add_listener(UsageIngestor::new(billing.clone()));
        let gate: Arc<dyn SpendGate> = billing.clone();
        let apps = Apps::new(
            forge.clone(),
            AppsConfig::new(
                Arc::new(ServerTokenIssuer::new(state)),
                Arc::new(ServerRevoker {
                    state: Arc::downgrade(state),
                }),
            )
            .with_ip_policy(enterprise.network.clone())
            .with_spend_gate(gate.clone())
            .with_sso(enterprise.network.clone())
            .with_domains(enterprise.network.clone()),
        );
        apps.attach();
        let models = Models::new(forge.clone(), Some(gate));
        let changes = Changes::new(
            &forge,
            ChangeHooks {
                vcs: vcs.clone(),
                lander: Arc::new(ServerLander::new(state)),
                teams: enterprise.teams.clone(),
            },
        );
        changes.attach(&forge);
        changes.set_land_facts(Arc::new(facts::ServerLandFacts::new(state)));
        forge
            .orgs
            .set_mirror_syncer(Arc::new(mirrors::ServerMirrors::new(state)));
        forge
            .orgs
            .set_insecure_mirrors(state.cfg.forge.mirror_allow_insecure);
        forge.set_changes(changes.clone());
        late_changes.set(changes.clone());
        let posts = bee_forge::posts::Posts::new(
            &forge,
            bee_forge::posts::PostPorts {
                blobs: Arc::new(packages::ServerPackageBlobs::new(state)),
                signer: bee_forge::media::MediaSigner::new(
                    &state.public_url(),
                    &blake3::derive_key("bee forge media urls v1", &signer_key),
                ),
            },
        );
        posts.attach(&changes);
        let ci_key = blake3::derive_key("bee ci keys v1", &crate::server::url_key(&state.cfg)?);
        // Actions OIDC tokens and artifact attestations are signed with the server's ES256 key,
        // the one its JWKS publishes, so relying parties and Sigstore tooling can verify them.
        // The key comes from `crate::oidc::key_pkcs8`, which reads `auth.oidc_key_pkcs8_hex`
        // when set, so every replica of a scaled-out server signs with the same key.
        crate::oidc::key(state)?;
        let ci_signing_key = {
            let pkcs8 = crate::oidc::key_pkcs8(state)?;
            bee_forge::ci::ports::EcdsaP256Key::from_pkcs8(&pkcs8).map_err(forge_to_api)?
        };
        let base = state.public_url();
        let ci = Ci::new(
            &forge,
            CiHooks {
                vcs: Arc::new(HistoryCiVcs::new(vcs.clone())),
                blobs: Arc::new(StoreCiBlobs::new(state.blobs.clone())),
                keys: Arc::new(ci_signing_key),
                secret_key: blake3::derive_key("bee ci secrets v1", &ci_key),
                url_key: blake3::derive_key("bee ci urls v1", &ci_key).to_vec(),
                signer: Arc::new(ServerTokenIssuer::new(state)),
                config: CiConfig {
                    server_url: base.clone(),
                    api_url: format!("{base}/api/v3"),
                    graphql_url: format!("{base}/api/graphql"),
                    oidc_issuer: format!("{base}/_services/token"),
                    ..CiConfig::default()
                },
            },
        );
        ci.attach(&forge);
        late_ci.set(ci.clone());
        // Evidence bytes live in the CI blob store; its URLs are signed with a key of their own.
        let briefs = bee_forge::briefs::Briefs::new(
            &forge,
            changes.clone(),
            bee_forge::briefs::BriefPorts {
                blobs: Arc::new(StoreCiBlobs::new(state.blobs.clone())),
                urls: bee_forge::ci::ports::UrlSigner::new(
                    base.clone(),
                    &blake3::derive_key("bee evidence urls v1", &ci_key),
                ),
                artifacts: Some(ci.clone()),
            },
        );
        ci.set_fork_gate(Arc::new(bee_forge::forks::ModerationGate(
            moderation.clone(),
        )));
        let forks = bee_forge::forks::Forks::new(
            &forge,
            Arc::new(forks::ServerForkVcs::new(state)),
            vcs.clone(),
        );
        forks.attach(&forge);
        let bisector = Bisector::new(&forge, ci.clone(), changes.clone(), BisectConfig::default());
        bisector.attach(&forge);
        let security = Security::new(
            &forge,
            Some(changes.clone()),
            SecurityPorts {
                vcs: Arc::new(HistorySecurityVcs::new(state.writer.clone())),
                signatures: Arc::new(facts::ServerSignatures::new(state)),
                ..SecurityPorts::default()
            },
        );
        security.set_sbom_namespace_base(format!("{base}/sbom"));
        security.attach(&forge);
        let agent_repo: Arc<dyn bee_forge::agents::AgentRepo> =
            Arc::new(agents::ServerAgentRepo::new(state, vcs.clone()));
        let runner: Arc<dyn bee_forge::agents::AgentRunner> =
            match state.cfg.forge.agent_runner.as_str() {
                // CommandRunner reads shared cancellation state through this server's forge wiring.
                "command" => Arc::new(agents::CommandRunner::new(
                    &state.cfg.forge.agent_runner_command,
                    &state.public_url(),
                    state.cfg.server.data_dir.join("agent-runs"),
                    state,
                )),
                "fake" => Arc::new(bee_forge::agents::fake::FakeRunner::with_forge(
                    changes.clone(),
                    agent_repo.clone(),
                    forge.leases.clone(),
                )),
                _ => Arc::new(agents::NoRunner),
            };
        let agents_mod = Agents::new(
            forge.clone(),
            changes.clone(),
            AgentPorts {
                runner,
                issuer: Arc::new(agents::ServerRunIssuer::new(state)),
                blobs: Arc::new(agents::ServerAgentBlobs::new(state)),
                repo: agent_repo.clone(),
                workspaces: Arc::new(bee_forge::agents::local::LocalProvider::new(
                    state.cfg.server.data_dir.join("workspaces"),
                    agent_repo,
                )),
            },
            AgentsConfig {
                base_url: state.public_url(),
                watch_debounce: std::time::Duration::from_millis(
                    state.cfg.forge.agent_debounce_ms.max(1),
                ),
                ..AgentsConfig::default()
            },
        );
        // After the work hooks are set: attach installs the agent gate in them.
        agents_mod.attach();
        let agents = agents_mod;
        forge.work.set_hooks(WorkHooks {
            templates: Arc::new(ServerTemplates::new(state.clone(), vcs.clone())),
            branches: Arc::new(ServerBranches::new(state.clone())),
            agents: Arc::new(AllAgents),
            changes: Arc::new(LateWorkChanges(late_changes)),
        });
        let queue = Arc::new(Mutex::new(BTreeSet::new()));
        let queue_wake = Arc::new(Notify::new());
        forge.events.add_listener(Arc::new(QueueListener::new(
            changes.clone(),
            queue.clone(),
            queue_wake.clone(),
        )));
        forge.events.add_listener(Arc::new(PolicyInvalidator {
            state: Arc::downgrade(state),
        }));
        forge
            .events
            .add_listener(Arc::new(access::AccessInvalidator {
                state: Arc::downgrade(state),
            }));
        let bridge = VcsBridge::new(state.meta.clone(), forge.events.clone(), forge.db.clone());
        let registry = bee_forge::packages::PackageRegistry::new(forge.clone());
        Ok(Arc::new(ForgeWiring {
            state: Arc::downgrade(state),
            forge,
            changes,
            posts,
            briefs,
            enterprise,
            moderation,
            billing,
            apps,
            models,
            directory,
            store,
            github: import::GithubImports::default(),
            ci,
            forks,
            bisector,
            security,
            agents,
            agent_workers: Mutex::new(None),
            registry,
            ci_worker: Mutex::new(None),
            bridge,
            dirty: Mutex::new(HashSet::new()),
            bridge_wake: Notify::new(),
            admitted: Mutex::new(HashMap::new()),
            team_groups: Mutex::new(HashMap::new()),
            merge_queue_settings: Arc::new(Mutex::new(HashMap::new())),
            queue,
            queue_wake,
            workers: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
        }))
    }

    pub(crate) fn state(&self) -> Option<Arc<AppState>> {
        self.state.upgrade()
    }

    /// Registers every forge service on both surfaces.
    pub fn register(self: &Arc<Self>, registry: &mut ServiceRegistry) {
        // RepoService owns the settings transaction in bee-forge; this narrow observer
        // persists the committed queue bit/version in server KV before UpdateRepoSettings
        // reports success, so a forge-only outage cannot expose replica-local stale state.
        let base = ForgeServices::new(self.forge.clone()).with_repo_settings_observer(Arc::new(
            landing::MergeQueueSettingsObserver::new(
                &self
                    .state
                    .upgrade()
                    .expect("state is live during registration"),
                self.merge_queue_settings.clone(),
            ),
        ));
        registry
            .add(base.org_service())
            .add(base.repo_service())
            .add(base.lease_service())
            .add(BisectCheckService::new(self.forge.clone(), self.bisector.clone()).server())
            .add(base.event_service())
            .add(v1::audit_service_server::AuditServiceServer::new(
                crate::services::audit_sinks::AuditApi::new(base.clone(), self.clone()),
            ))
            .add(base.issue_service())
            .add(base.project_service())
            .add(base.comment_service());
        let (discussions, wiki, gists, releases, notifications, users, insights, sponsors) =
            base.community_services();
        registry
            .add(discussions)
            .add(wiki)
            .add(gists)
            .add(releases)
            .add(notifications)
            .add(users)
            .add(insights)
            .add(sponsors);
        registry
            .add(ChangeServices::new(self.forge.clone(), self.changes.clone()).change_service());
        registry.add(
            bee_forge::posts::service::PostServices::new(self.forge.clone(), self.posts.clone())
                .post_service(),
        );
        registry.add(
            bee_forge::briefs::service::BriefServices::new(self.forge.clone(), self.briefs.clone())
                .brief_service(),
        );
        let ent = EnterpriseApi::new(self.enterprise.clone());
        registry.add(ent.team_server()).add(ent.enterprise_server());
        registry.add(AppsService::new(self.forge.clone(), self.apps.clone()).server());
        let resolver: Arc<dyn ScopeResolver> = self.enterprise.accounts.clone();
        registry.add(BillingService::server(
            self.billing.clone(),
            resolver,
            base.clone(),
        ));
        registry.add(ModelsService::server(self.models.clone(), base.clone()));
        registry.add(ModerationService::server(self.moderation.clone(), base));
        let ci = CiServices::new(self.forge.clone(), self.ci.clone());
        registry
            .add(ci.workflow_service())
            .add(ci.runner_service())
            .add(ci.secret_service())
            .add(ci.deployment_service());
        registry.add(
            SecurityServices::new(self.forge.clone(), self.security.clone()).security_service(),
        );
        let api = AgentApi::new(self.agents.clone());
        registry.add(api.agent_server()).add(api.workspace_server());
        registry.add(bee_forge::swarms::service::SwarmApi::new(self.agents.clone()).server());
        registry.add(bee_forge::safety::service::SafetyApi::new(self.agents.clone()).server());
        let base = ForgeServices::new(self.forge.clone());
        registry
            .add(base.package_service())
            .add(base.pages_service());
    }

    /// Starts the forge workers and the server-side ones (VCS event bridge, merge queue).
    pub fn spawn(self: &Arc<Self>) {
        let workers = self.forge.spawn_workers();
        *self.ci_worker.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(self.ci.spawn_worker(Duration::from_secs(1)));
        *self.agent_workers.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(self.agents.spawn_workers());
        *self.workers.lock().unwrap_or_else(|p| p.into_inner()) = Some(workers);
        let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(state) = self.state()
            && state.cfg.forge.local_runner.enabled.unwrap_or(false)
        {
            let pending = Arc::new(local_runner::Pending::default());
            self.forge
                .events
                .add_listener(Arc::new(local_runner::QueuedJobs(pending.clone())));
            tasks.push(tokio::spawn(local_runner::run(
                Arc::downgrade(&self.forge),
                Arc::downgrade(&self.ci),
                pending,
                state.cfg.server.data_dir.join("runner"),
            )));
        }
        tasks.push(tokio::spawn(landing::run_queue(
            self.changes.clone(),
            self.forge.clone(),
            self.queue.clone(),
            self.queue_wake.clone(),
        )));
        let me = Arc::downgrade(self);
        tasks.push(tokio::spawn(async move {
            loop {
                let Some(w) = me.upgrade() else { return };
                let every = w
                    .state()
                    .map(|s| s.cfg.forge.bridge_interval_ms)
                    .unwrap_or(500)
                    .max(10);
                let _ =
                    tokio::time::timeout(Duration::from_millis(every), w.bridge_wake.notified())
                        .await;
                w.sync_dirty().await;
                drop(w);
            }
        }));
        let me = Arc::downgrade(self);
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Some(w) = me.upgrade() else { return };
                let _ = w.bisector.tick(&Cx::system()).await;
            }
        }));
        let me = Arc::downgrade(self);
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let Some(w) = me.upgrade() else { return };
                let _ = w.enterprise.maintain().await;
                let _ = w.security.maintain(&Cx::system()).await;
                let _ = w.briefs.maintain(&Cx::system()).await;
            }
        }));
        let me = Arc::downgrade(self);
        tasks.push(tokio::spawn(async move {
            loop {
                let every = match me.upgrade().and_then(|w| w.state()) {
                    Some(s) => s.cfg.forge.mirror_poll_ms.max(50),
                    None => return,
                };
                tokio::time::sleep(Duration::from_millis(every)).await;
                let Some(w) = me.upgrade() else { return };
                w.sync_due_mirrors().await;
            }
        }));
        let every = self
            .state()
            .map(|s| s.cfg.import.github_sync_interval_secs)
            .unwrap_or(0);
        if every > 0 {
            let me = Arc::downgrade(self);
            tasks.push(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(every)).await;
                    let Some(w) = me.upgrade() else { return };
                    w.sync_github().await;
                }
            }));
        }
    }

    /// Stops every worker.
    pub async fn stop(&self) {
        for t in self
            .tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            t.abort();
        }
        let ci = self
            .ci_worker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let agents = self
            .agent_workers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(a) = agents {
            a.stop().await;
        }
        if let Some((stop, task)) = ci {
            let _ = stop.send(true);
            task.abort();
        }
        let w = self
            .workers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(w) = w {
            w.stop().await;
        }
    }

    /// Marks a repo's VCS events for bridging into the forge log.
    pub fn touch(&self, repo: &RepoCtx) {
        if let Ok(mut d) = self.dirty.lock() {
            d.insert((repo.id_text(), Self::bridge_org(repo)));
        }
        self.bridge_wake.notify_one();
    }

    /// The forge org a repo's bridged events belong to: its VCS org, except for forks, whose
    /// VCS repo lives in their network's namespace (empty: [`ForgeWiring::sync_repo`] asks
    /// the forge).
    pub fn bridge_org(repo: &RepoCtx) -> String {
        if repo.repo.name.contains(bee_forge::forks::VCS_NAME_SEP) {
            String::new()
        } else {
            repo.org.id.0.to_text()
        }
    }

    /// The forge's full name of a fork whose VCS repo is `repo_id` (VCS names of forks are
    /// `<name>~<id>` in their network's namespace).
    pub async fn fork_full_name(&self, repo_id: &str) -> Option<String> {
        let cx = landing::quick_cx();
        self.forge
            .orgs
            .repo_any(&cx, repo_id)
            .await
            .ok()
            .filter(|r| !r.fork_parent_id.is_empty())
            .map(|r| r.full_name)
    }

    /// The forge org of a fork whose VCS repo is `repo_id`.
    pub async fn fork_org_id(&self, repo_id: &str) -> Option<String> {
        let cx = landing::quick_cx();
        self.forge
            .orgs
            .repo_any(&cx, repo_id)
            .await
            .ok()
            .filter(|r| !r.fork_parent_id.is_empty())
            .map(|r| r.org_id)
    }

    /// The repo id of the fork the forge calls `full_name`, if it is one.
    pub async fn fork_by_full_name(&self, full_name: &str) -> Option<String> {
        let cx = landing::quick_cx();
        let r = v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::FullName(full_name.to_string())),
        };
        self.forge
            .orgs
            .resolve_repo(&cx, &r)
            .await
            .ok()
            .filter(|r| !r.fork_parent_id.is_empty())
            .map(|r| r.id)
    }

    /// Bridges one repo's VCS events now (EventService reads see every event so far). An
    /// empty `org_id` is looked up in the forge (forks).
    pub async fn sync_repo(&self, repo_id: &str, org_id: &str) {
        let cx = landing::quick_cx();
        let resolved;
        let org_id = if org_id.is_empty() {
            match self.forge.orgs.repo_any(&cx, repo_id).await {
                Ok(r) => {
                    resolved = r.org_id;
                    resolved.as_str()
                }
                Err(_) => {
                    if let Ok(mut d) = self.dirty.lock() {
                        d.insert((repo_id.to_string(), String::new()));
                    }
                    return;
                }
            }
        } else {
            org_id
        };
        loop {
            match self.bridge.sync_repo(&cx, repo_id, org_id).await {
                Ok(n) if n == bee_forge::events::bridge::BATCH => continue,
                Ok(_) => return,
                Err(_) => {
                    // Keep it dirty; the bridge retries after recovery.
                    if let Ok(mut d) = self.dirty.lock() {
                        d.insert((repo_id.to_string(), org_id.to_string()));
                    }
                    return;
                }
            }
        }
    }

    async fn sync_dirty(&self) {
        let batch: Vec<(String, String)> = match self.dirty.lock() {
            Ok(mut d) => d.drain().collect(),
            Err(_) => Vec::new(),
        };
        for (repo, org) in batch {
            self.sync_repo(&repo, &org).await;
        }
    }

    /// Forgets cached admission decisions (after a change to blocks, allow lists, SSO or
    /// SCIM state), so the next request is decided afresh.
    pub fn forget_admissions(&self) {
        if let Ok(mut m) = self.admitted.lock() {
            m.clear();
        }
        if let Ok(mut m) = self.team_groups.lock() {
            m.clear();
        }
    }

    /// The policy groups (`<org>/<team>`) a principal holds through forge teams (F49-041),
    /// cached for `forge.admission_cache_ms`. A forge error yields none.
    pub async fn team_groups(&self, principal: &str) -> Vec<String> {
        let ttl = self
            .state()
            .map(|s| s.cfg.forge.admission_cache_ms)
            .unwrap_or(0);
        if ttl > 0
            && let Ok(m) = self.team_groups.lock()
            && let Some((at, g)) = m.get(principal)
            && at.elapsed() < Duration::from_millis(ttl)
        {
            return g.clone();
        }
        let groups = self
            .enterprise
            .teams
            .policy_groups_of(&landing::quick_cx(), principal)
            .await
            .unwrap_or_default();
        if ttl > 0
            && let Ok(mut m) = self.team_groups.lock()
        {
            if m.len() > 100_000 {
                m.clear();
            }
            m.insert(principal.to_string(), (Instant::now(), groups.clone()));
        }
        groups
    }

    /// Enterprise admission for a request (SPEC: IP allow lists, deprovisioned principals,
    /// SSO and 2FA enforcement): the call's principals, the orgs it touches, the source
    /// address and whether the token is an app's. Retryable forge errors admit (the forge
    /// is a separate failure domain).
    pub async fn admit(
        &self,
        principals: &[String],
        org_ids: &[String],
        ip: &str,
        app_token: bool,
    ) -> ApiResult<()> {
        let key = format!(
            "{}|{}|{ip}|{app_token}",
            principals.join(","),
            org_ids.join(",")
        );
        let ttl = self
            .state()
            .map(|s| s.cfg.forge.admission_cache_ms)
            .unwrap_or(0);
        if ttl > 0
            && let Ok(m) = self.admitted.lock()
            && m.get(&key)
                .is_some_and(|t| t.elapsed() < Duration::from_millis(ttl))
        {
            return Ok(());
        }
        let cx = landing::quick_cx();
        let req = AdmissionRequest {
            principals: principals.to_vec(),
            org_ids: org_ids.to_vec(),
            ip: ip.to_string(),
            app_token,
        };
        match self.enterprise.admit(&cx, &req).await {
            Ok(()) => {
                if ttl > 0
                    && let Ok(mut m) = self.admitted.lock()
                {
                    if m.len() > 100_000 {
                        m.clear();
                    }
                    m.insert(key, Instant::now());
                }
                Ok(())
            }
            Err(e) if e.retryable() => {
                logging::log(
                    Level::Warn,
                    "forge",
                    "admission skipped: forge unavailable",
                    &[("error", e.message.into())],
                );
                Ok(())
            }
            Err(e) => Err(forge_to_api(e)),
        }
    }

    /// Whether a token belongs to an installed app: some service principal of its chain is
    /// an installation (installation tokens) or an app (user-to-server tokens).
    pub async fn app_token(&self, token: &bee_auth::VerifiedToken) -> bool {
        let cx = landing::quick_cx();
        for p in token.chain() {
            if p.kind == bee_policy::PrincipalKind::Service
                && self.apps.get_installation(&cx, &p.id).await.is_ok()
            {
                return true;
            }
        }
        false
    }

    /// Moderation for writes the forge services do not guard themselves (reviews, reactions,
    /// forks).
    pub async fn moderate(
        &self,
        token: &bee_auth::VerifiedToken,
        repo_id: &str,
        org_id: &str,
        kind: WriteKind,
    ) -> ApiResult<()> {
        let cx = landing::token_cx(token);
        match self
            .moderation
            .check_write(&WriteCheck::new(&cx, repo_id, org_id, kind))
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.retryable() => Ok(()),
            Err(e) => Err(forge_to_api(e)),
        }
    }

    /// Billing for metered work: refuses when a budget or spending limit is exhausted, and
    /// records the usage when `record` is set. Storage is metered in GB.
    pub async fn spend(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        bytes: u64,
        record: bool,
    ) -> ApiResult<()> {
        if bytes == 0 {
            return Ok(());
        }
        let cx = landing::caller_cx(caller);
        let org = repo.org.id.0.to_text();
        let gb = bytes as f64 / 1e9;
        let product = v1::BillingProduct::StorageGb;
        let r = if record {
            self.billing
                .record_usage(
                    &cx,
                    bee_forge::enterprise::billing::UsageInput {
                        org_id: org,
                        repo_id: repo.id_text(),
                        principal_id: caller.id().to_string(),
                        product,
                        sku: "storage_gb_month".into(),
                        quantity: gb,
                        unit: "GB".into(),
                        idempotency_key: format!("upload:{}", crate::util::random_hex(12)),
                        at_ms: 0,
                    },
                )
                .await
                .map(|_| ())
        } else {
            self.billing
                .enforce(&cx, &org, &repo.id_text(), product, "storage_gb_month", gb)
                .await
        };
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.retryable() => Ok(()),
            Err(e) => Err(forge_to_api(e)),
        }
    }

    /// Gives the VCS org and repo the forge rows the forge needs (dev bootstrap, imports):
    /// the org under the same id with `owner` as its owner, and the repo under the same id.
    pub async fn adopt(&self, ctx: &RepoCtx, owner: &Principal) -> ApiResult<()> {
        let cx = Cx::system();
        let org_id = ctx.org.id.0.to_text();
        if self.forge.orgs.org_any(&cx, &org_id).await.is_err() {
            match self
                .forge
                .orgs
                .create_org_with_id(&cx, &org_id, &ctx.org.name, "", owner.clone())
                .await
            {
                Ok(_) => {}
                Err(e) if e.reason == ErrorReason::AlreadyExists => {}
                Err(e) => return Err(forge_to_api(e)),
            }
        }
        if self.forge.orgs.repo_any(&cx, &ctx.id_text()).await.is_err() {
            match self
                .forge
                .orgs
                .create_repo(
                    &cx,
                    &org_id,
                    bee_forge::orgs::repos::NewRepo {
                        id: ctx.id_text(),
                        name: ctx.repo.name.clone(),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(_) => {}
                Err(e) if e.reason == ErrorReason::AlreadyExists => {}
                Err(e) => return Err(forge_to_api(e)),
            }
        }
        Ok(())
    }

    /// Makes `principal` a member of the repo's org with `write` on the repo, so it can
    /// review (once; an existing membership is kept as it is).
    pub async fn add_member(&self, ctx: &RepoCtx, principal: &Principal) -> ApiResult<()> {
        let cx = Cx::system();
        let org_id = ctx.org.id.0.to_text();
        let known = self
            .forge
            .orgs
            .member(&cx, &org_id, &principal.id)
            .await
            .map_err(forge_to_api)?;
        if known.is_none() {
            self.forge
                .orgs
                .set_member(&cx, &org_id, principal.clone(), v1::Role::Member)
                .await
                .map_err(forge_to_api)?;
            self.forge
                .orgs
                .add_collaborator(
                    &cx,
                    &ctx.id_text(),
                    principal.clone(),
                    v1::RepoRole::Write,
                    "",
                )
                .await
                .map_err(forge_to_api)?;
        }
        Ok(())
    }
}

/// The server's handle on the forge: unset until the forge opens (and forever when it is
/// disabled). Every VCS-path hook goes through it and is a no-op while unset.
#[derive(Default)]
pub struct ForgeLink {
    wiring: OnceLock<Arc<ForgeWiring>>,
}

impl ForgeLink {
    /// An unset link.
    pub fn new() -> ForgeLink {
        ForgeLink::default()
    }

    /// Installs the opened forge.
    pub fn set(&self, w: Arc<ForgeWiring>) {
        let _ = self.wiring.set(w);
    }

    /// The forge, once open.
    pub fn get(&self) -> Option<&Arc<ForgeWiring>> {
        self.wiring.get()
    }

    /// Whether a ref must move through Land because ordinary policy protects it or because
    /// the merge queue is required.  The latter is deliberately shared by native ref writes
    /// and the git gateway so neither can bypass the publication gate.
    pub async fn queue_or_policy_protected(
        &self,
        state: &AppState,
        repo: &RepoCtx,
        ref_name: &str,
    ) -> ApiResult<bool> {
        let policy = state.policy(repo, ref_name).await?;
        if policy.is_protected(ref_name) {
            return Ok(true);
        }
        let Some(wiring) = self.get() else {
            return crate::services::policy::merge_queue_required(state, repo, ref_name).await;
        };
        wiring.merge_queue_required(state, repo, ref_name).await
    }

    /// Enforces the durable merge-queue publication gate when the forge is enabled.
    pub async fn require_landing_queue_entry(
        &self,
        repo: &RepoCtx,
        ref_name: &str,
        head: &ObjectId,
        change_request_id: &str,
    ) -> ApiResult<()> {
        if let Some(wiring) = self.get() {
            wiring
                .require_landing_queue_entry(repo, ref_name, head, change_request_id)
                .await
        } else {
            Ok(())
        }
    }

    /// Records an administrative store operation in the forge audit log, which is the log
    /// exposed by `AuditService`.
    pub async fn admin_audit(
        &self,
        caller: &Caller,
        org_id: &str,
        target: &str,
        action: &str,
        detail: serde_json::Value,
    ) {
        let Some(w) = self.get() else { return };
        let cx = landing::caller_cx(caller);
        let entry = bee_forge::audit::AuditDraft::new(action, org_id)
            .target_id(target)
            .after(&detail.to_string())
            .into_entry(&cx, w.forge.db.now_ms(), String::new());
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            w.forge.audit.append(&cx, entry),
        )
        .await;
    }

    /// Records a push-protection refusal without retaining file content or a matching line.
    pub async fn secret_refusal(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        action: &str,
        err: &ApiError,
    ) {
        if err.detail.reason != "SECRET_DETECTED" {
            return;
        }
        let Some(w) = self.get() else { return };
        let findings = match &err.detail.payload {
            Some(v1::error_detail::Payload::PushProtection(block)) => block
                .findings
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "path": String::from_utf8_lossy(&f.path),
                        "rule": f.pattern_id,
                    })
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let cx = landing::caller_cx(caller);
        let entry = bee_forge::audit::AuditDraft::new(action, &repo.org.id.0.to_text())
            .repo_id(&repo.id_text())
            .result("SECRET_DETECTED")
            .after(&serde_json::json!({ "findings": findings }).to_string())
            .into_entry(&cx, w.forge.db.now_ms(), String::new());
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            w.forge.audit.append(&cx, entry),
        )
        .await;
    }
}

#[async_trait]
impl ForgeInputs for ForgeLink {
    async fn fence_agent_landing(
        &self,
        repo: &RepoCtx,
    ) -> ApiResult<Option<bee_forge::safety::control::LandFence>> {
        let Some(wiring) = self.get() else {
            return Ok(None);
        };
        bee_forge::safety::control::fence_landing(
            &wiring.forge.db,
            &landing::quick_cx(),
            &repo.org.id.0.to_text(),
        )
        .await
        .map(Some)
        .map_err(forge_to_api)
    }

    async fn require_landing_queue_entry(
        &self,
        repo: &RepoCtx,
        ref_name: &str,
        head: &ObjectId,
        change_request_id: &str,
    ) -> ApiResult<()> {
        ForgeLink::require_landing_queue_entry(self, repo, ref_name, head, change_request_id).await
    }

    async fn note_safety_land(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        effect: &str,
        error: Option<&ApiError>,
    ) {
        let Some(wiring) = self.get() else { return };
        let cx = landing::caller_cx(caller).with_deadline(landing::quick_cx().deadline);
        let Some(repo) = wiring.forge_repo(&cx, &repo.id_text()).await else {
            return;
        };
        let error = error.cloned().map(api_to_forge);
        if let Err(error) = bee_forge::safety::record_landing_attempt(
            &wiring.forge,
            &cx,
            &repo,
            &caller.token,
            effect,
            error.as_ref(),
        )
        .await
        {
            logging::log(
                Level::Warn,
                "forge",
                "recording the run's landing attempt failed",
                &[("reason", error.reason_name().into())],
            );
        }
    }

    async fn note_safety_leash(
        &self,
        caller: &Caller,
        action: &str,
        target: &str,
        detail: &serde_json::Value,
    ) -> ApiResult<()> {
        let Some(wiring) = self.get() else {
            return Ok(());
        };
        bee_forge::safety::record_native_leash(
            &wiring.agents,
            &landing::caller_cx(caller),
            action,
            target,
            detail,
        )
        .await
        .map_err(forge_to_api)
    }

    async fn note_safety_changeset(&self, caller: &Caller, repo: &RepoCtx, id: &ObjectId) {
        let Some(wiring) = self.get() else { return };
        let cx = landing::caller_cx(caller).with_deadline(landing::quick_cx().deadline);
        let Some(repo) = wiring.forge_repo(&cx, &repo.id_text()).await else {
            return;
        };
        if let Err(error) = bee_forge::safety::record_changeset(
            &wiring.forge,
            &cx,
            &repo,
            &caller.token,
            &crate::convert::id_to(id),
        )
        .await
        {
            logging::log(
                Level::Warn,
                "forge",
                "recording the run's changeset failed",
                &[("reason", error.reason_name().into())],
            );
        }
    }

    async fn admin_audit(
        &self,
        caller: &Caller,
        org_id: &str,
        target: &str,
        action: &str,
        detail: serde_json::Value,
    ) {
        ForgeLink::admin_audit(self, caller, org_id, target, action, detail).await;
    }

    async fn secret_refusal(&self, caller: &Caller, repo: &RepoCtx, action: &str, err: &ApiError) {
        ForgeLink::secret_refusal(self, caller, repo, action, err).await;
    }

    async fn land_inputs(
        &self,
        repo: RepoId,
        ref_name: &str,
        head: &ObjectId,
        change_request_id: &str,
    ) -> ApiResult<LandInputs> {
        Ok(match self.get() {
            Some(w) => {
                w.land_inputs(&repo.0.to_text(), ref_name, head, change_request_id)
                    .await
            }
            None => LandInputs::default(),
        })
    }

    async fn landed(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        ref_name: &str,
        resp: &v1::LandResponse,
        change_request_id: &str,
    ) {
        if let Some(w) = self.get() {
            w.landed(caller, repo, ref_name, resp, change_request_id)
                .await;
        }
    }

    async fn land_conflict(
        &self,
        repo: &RepoCtx,
        ref_name: &str,
        head: &ObjectId,
        change_request_id: &str,
        conflicts: &v1::ConflictSet,
    ) {
        if let Some(w) = self.get() {
            w.land_conflict(repo, ref_name, head, change_request_id, conflicts)
                .await;
        }
    }

    async fn ref_moved(
        &self,
        repo: &RepoCtx,
        ref_name: &str,
        target: Option<&ObjectId>,
        caller: &Caller,
        push_options: &[String],
        via_git: bool,
    ) {
        if let Some(w) = self.get() {
            w.ref_moved(repo, ref_name, target, caller, push_options, via_git)
                .await;
        }
    }

    async fn admit(&self, caller: &Caller, org_id: Option<&str>) -> ApiResult<()> {
        let Some(w) = self.get() else { return Ok(()) };
        let orgs: Vec<String> = org_id.map(|o| vec![o.to_string()]).unwrap_or_default();
        let app = w.app_token(&caller.token).await;
        w.admit(&caller.chain_ids(), &orgs, &caller.ip, app).await
    }

    async fn protects(&self, repo: &RepoCtx) -> bool {
        let Some(w) = self.get() else { return false };
        let cx = landing::quick_cx();
        match w.forge_repo(&cx, &repo.id_text()).await {
            Some(r) => w
                .security
                .config_of(&cx, &r)
                .await
                .is_ok_and(|c| c.secret_scanning_push_protection),
            None => false,
        }
    }

    async fn protect(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        files: &[(String, Vec<u8>, Option<ObjectId>)],
        bypass_id: Option<&str>,
    ) -> ApiResult<()> {
        let Some(w) = self.get() else { return Ok(()) };
        let cx = landing::caller_cx(caller);
        let Some(r) = w.forge_repo(&cx, &repo.id_text()).await else {
            return Ok(());
        };
        let files: Vec<(String, Vec<u8>, Option<v1::ObjectId>)> = files
            .iter()
            .map(|(p, b, id)| {
                (
                    p.clone(),
                    b.clone(),
                    id.map(|i| v1::ObjectId {
                        raw: i.as_bytes().to_vec(),
                    }),
                )
            })
            .collect();
        match w.security.protect(&cx, &r, &files, bypass_id).await {
            Ok(_) => Ok(()),
            Err(e) if e.retryable() => Ok(()),
            Err(e) => {
                let err = forge_to_api(e);
                self.secret_refusal(caller, repo, "vcs.write", &err).await;
                Err(err)
            }
        }
    }

    async fn denied(&self, caller: &Caller, repo: &RepoCtx, action: &str, err: &ApiError) {
        let Some(w) = self.get() else {
            return;
        };
        let mut cx = landing::caller_cx(caller);
        cx.decision = format!("denied: {}", err.detail.message);
        let entry = bee_forge::audit::AuditDraft::new(action, &repo.org.id.0.to_text())
            .repo_id(&repo.id_text())
            .result(&err.detail.reason)
            .into_entry(&cx, w.forge.db.now_ms(), String::new());
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            w.forge.audit.append(&cx, entry),
        )
        .await;
    }

    async fn team_groups(&self, principal: &str) -> Vec<String> {
        match self.get() {
            Some(w) => w.team_groups(principal).await,
            None => Vec::new(),
        }
    }

    async fn code_alerts(
        &self,
        repo: RepoId,
        ref_name: &str,
        head: &ObjectId,
        change_request_id: &str,
    ) -> ApiResult<Vec<bee_policy::CodeAlert>> {
        let Some(w) = self.get() else {
            return Ok(Vec::new());
        };
        let cx = landing::quick_cx();
        let Some(r) = w.forge_repo(&cx, &repo.0.to_text()).await else {
            return Ok(Vec::new());
        };
        let mut heads = vec![head.as_bytes().to_vec()];
        if !change_request_id.is_empty()
            && let Ok(cr) = w.changes.get(&cx, &r, change_request_id).await
            && let Some(h) = cr.revisions.last().and_then(|rv| rv.head.as_ref())
            && h.raw != heads[0]
        {
            heads.push(h.raw.clone());
        }
        w.land_code_alerts(&cx, &r, ref_name, &heads).await
    }

    async fn verify_signature(
        &self,
        payload: &[u8],
        armored: &[u8],
    ) -> ApiResult<Option<bee_forge::community::keys::SignatureVerdict>> {
        let Some(w) = self.get() else {
            return Ok(None);
        };
        let now = w.forge.db.now_ms();
        w.forge
            .community
            .keys
            .verify_signature(&landing::quick_cx(), payload, armored, now)
            .await
            .map(Some)
            .map_err(forge_to_api)
    }

    async fn access_record(&self, repo: &RepoCtx) -> ApiResult<Option<v1::Repo>> {
        match self.get() {
            Some(w) => w.access_record(repo).await,
            None => Ok(None),
        }
    }

    async fn access_role(&self, repo: &v1::Repo, principal_id: &str) -> ApiResult<v1::RepoRole> {
        match self.get() {
            Some(w) => w.access_role(repo, principal_id).await,
            None => Ok(v1::RepoRole::None),
        }
    }

    async fn repo_by_full_name(&self, full_name: &str) -> ApiResult<Option<String>> {
        match self.get() {
            Some(w) => w.repo_by_full_name(full_name).await,
            None => Ok(None),
        }
    }

    async fn org_admins(&self, org_id: &str) -> Vec<String> {
        let Some(w) = self.get() else {
            return Vec::new();
        };
        let cx = landing::quick_cx();
        let mut out = Vec::new();
        let mut after = String::new();
        loop {
            let Ok((page, next)) = w.forge.orgs.members(&cx, org_id, &after, 500).await else {
                return out;
            };
            for m in page {
                let admin = matches!(
                    v1::Role::try_from(m.role),
                    Ok(v1::Role::Owner) | Ok(v1::Role::Admin)
                );
                if admin && let Some(p) = m.principal {
                    out.push(p.id);
                }
            }
            if next.is_empty() {
                return out;
            }
            after = next;
        }
    }

    async fn spend(
        &self,
        caller: &Caller,
        repo: &RepoCtx,
        bytes: u64,
        record: bool,
    ) -> ApiResult<()> {
        match self.get() {
            Some(w) => w.spend(caller, repo, bytes, record).await,
            None => Ok(()),
        }
    }
}

/// A principal of the server's table as a forge principal (dev bootstrap owner).
pub fn owner_principal(id: &str, display: &str) -> Principal {
    Principal {
        id: id.to_string(),
        kind: PrincipalKind::Human as i32,
        display: display.to_string(),
        ..Default::default()
    }
}
