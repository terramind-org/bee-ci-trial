//! Wiring: one [`Forge`] holds the store, every core module, authentication and the hooks,
//! runs migrations on open, and runs the background workers.
//!
//! # Workers
//!
//! Each worker is its own task with its own bounds, so one cannot starve another:
//!
//! | worker | does | bound |
//! |---|---|---|
//! | outbox relay (one per surface) | outbox → event log / audit chain | one task per surface |
//! | webhook dispatcher + delivery | log events → jobs → HTTP | [`DeliveryConfig::concurrency`] requests |
//! | maintenance | lease expiry, event retention, purge of deleted orgs/repos, project auto-archive, unattached post media | one task |
//! | community | notification fan-out, reminders, deliveries' queue upkeep | [`crate::community::notifications::NotifyConfig::concurrency`] jobs |

use std::sync::{Arc, RwLock};
use std::time::Duration;

use bee_auth::{RevocationList, VerifyingKeys};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::audit::AuditLog;
use crate::auth::{Authenticator, Authz};
use crate::checks::Checks;
use crate::community::backing::WorkIssues;
use crate::community::{
    Community, CommunityConfig, OrgsAccess, Ports as CommunityPorts, sponsors::SponsorAccess,
};
use crate::cx::Cx;
use crate::error::Result;
use crate::events::EventLog;
use crate::events::webhooks::{DeliveryConfig, Webhooks};
use crate::hooks::{
    DetachedProvisioner, HexResolver, MemoryDirectory, PolicySource, PrincipalDirectory,
    RepoProvisioner, RevisionResolver,
};
use crate::ids::{Clock, IdGen};
use crate::leases::{AutoRelease, Leases};
use crate::module::Registry;
use crate::orgs::Orgs;
use crate::outbox::Relay;
use crate::packages::{PackagePorts, Packages, PackagesConfig};
use crate::pages::listener::PagesListener;
use crate::pages::{Pages, PagesConfig, PagesPorts};
use crate::store::{Db, ForgeStore, MigrationReport};
use crate::work::{Work, WorkListener};

/// Forge settings.
#[derive(Clone, Debug)]
pub struct ForgeConfig {
    /// Webhook delivery.
    pub delivery: DeliveryConfig,
    /// Event retention.
    pub event_retention_days: u32,
    /// Event sequence block size.
    pub event_block_size: u64,
    /// Relay poll interval (it also wakes on every outbox commit).
    pub relay_poll: Duration,
    /// Webhook worker poll interval.
    pub webhook_poll: Duration,
    /// Maintenance interval.
    pub maintenance_interval: Duration,
    /// Community modules (discussions, notifications, users, ...).
    pub community: CommunityConfig,
    /// Package registries.
    pub packages: PackagesConfig,
    /// Pages.
    pub pages: PagesConfig,
}

impl Default for ForgeConfig {
    fn default() -> ForgeConfig {
        ForgeConfig {
            delivery: DeliveryConfig::default(),
            event_retention_days: crate::events::DEFAULT_RETENTION_DAYS,
            event_block_size: crate::events::DEFAULT_BLOCK_SIZE,
            relay_poll: Duration::from_secs(1),
            webhook_poll: Duration::from_millis(500),
            maintenance_interval: Duration::from_secs(60),
            community: CommunityConfig::default(),
            packages: PackagesConfig::default(),
            pages: PagesConfig::default(),
        }
    }
}

/// External collaborators of the forge.
pub struct Hooks {
    /// Principal lookups.
    pub directory: Arc<dyn PrincipalDirectory>,
    /// VCS-side provisioning.
    pub provisioner: Arc<dyn RepoProvisioner>,
    /// Revision resolution.
    pub resolver: Arc<dyn RevisionResolver>,
    /// Repo policies.
    pub policy: Option<Arc<dyn PolicySource>>,
    /// What the community module needs: wiki and gist repos, tags, change requests, ...
    pub community: CommunityPorts,
    /// What the package registries need: the blob store, teams, provenance checks.
    pub packages: PackagePorts,
    /// What Pages needs: DNS, ACME, artifacts, the environment gate.
    pub pages: PagesPorts,
}

impl Default for Hooks {
    fn default() -> Hooks {
        Hooks {
            directory: Arc::new(MemoryDirectory::new()),
            provisioner: Arc::new(DetachedProvisioner),
            resolver: Arc::new(HexResolver),
            policy: None,
            community: CommunityPorts::default(),
            packages: PackagePorts::default(),
            pages: PagesPorts::default(),
        }
    }
}

/// The assembled forge.
pub struct Forge {
    /// Store handle.
    pub db: Db,
    /// Registered modules.
    pub registry: Arc<Registry>,
    /// Orgs and repos.
    pub orgs: Arc<Orgs>,
    /// Leases.
    pub leases: Arc<Leases>,
    /// Checks.
    pub checks: Arc<Checks>,
    /// Issues, planning objects and projects.
    pub work: Arc<Work>,
    /// Event logs.
    pub events: Arc<EventLog>,
    /// Webhooks.
    pub webhooks: Arc<Webhooks>,
    /// Audit log.
    pub audit: Arc<AuditLog>,
    /// Discussions, wiki, gists, releases, notifications, users, insights, sponsors.
    pub community: Arc<Community>,
    /// Package registries.
    pub packages: Arc<Packages>,
    /// Pages.
    pub pages: Arc<Pages>,
    /// Outbox relay.
    pub relay: Arc<Relay>,
    /// Token verification.
    pub authn: Authenticator,
    /// Authorization.
    pub authz: Authz,
    /// External collaborators.
    pub hooks: Hooks,
    /// Settings.
    pub config: ForgeConfig,
    write_guard: RwLock<Option<Arc<dyn crate::hooks::WriteGuard>>>,
    changes: std::sync::OnceLock<Arc<crate::changes::Changes>>,
    forks: std::sync::OnceLock<Arc<crate::forks::Forks>>,
}

impl Forge {
    /// Opens the forge on `store` (running migrations) and returns it with the migration
    /// report.
    pub async fn open(
        store: Arc<dyn ForgeStore>,
        clock: Arc<dyn Clock>,
        keys: VerifyingKeys,
        revocations: Arc<dyn RevocationList + Send + Sync>,
        hooks: Hooks,
        config: ForgeConfig,
    ) -> Result<(Arc<Forge>, MigrationReport)> {
        let registry = Arc::new(Registry::core()?);
        let report = store.migrate(&registry.migrations()).await?;
        let ids = Arc::new(IdGen::new(clock.clone()));
        let db = Db::new(store, ids.clone());
        let events = Arc::new(
            EventLog::new(db.clone())
                .with_retention_days(config.event_retention_days)
                .with_block_size(config.event_block_size),
        );
        let audit = Arc::new(AuditLog::new(db.clone()));
        let orgs = Arc::new(Orgs::new(db.clone()));
        let leases = Arc::new(Leases::new(db.clone()));
        events.add_listener(Arc::new(AutoRelease::new(leases.clone())));
        let work = Arc::new(Work::new(db.clone()));
        let mut ports = hooks.community.clone();
        if ports.issues.is_detached() {
            ports.issues = Arc::new(WorkIssues(work.clone()));
        }
        let community = Arc::new(Community::new(
            db.clone(),
            ports,
            hooks.directory.clone(),
            hooks.resolver.clone(),
            Arc::new(OrgsAccess(orgs.clone())),
            config.community.clone(),
        ));
        orgs.add_access_source(Arc::new(SponsorAccess(community.sponsors.clone())));
        events.add_listener(Arc::new(crate::community::users::AutoFollow::new(
            community.users.clone(),
            orgs.clone(),
        )));
        let checks = Arc::new(Checks::new(db.clone()));
        events.add_listener(Arc::new(WorkListener::new(work.clone(), orgs.clone())));
        let webhooks = Arc::new(Webhooks::new(
            db.clone(),
            events.clone(),
            registry.clone(),
            config.delivery,
        ));
        let relay = Arc::new(Relay::new(
            db.clone(),
            events.clone(),
            audit.clone(),
            registry.surfaces(),
        ));
        let packages = Arc::new(Packages::new(
            db.clone(),
            hooks.packages.clone(),
            orgs.clone(),
            hooks.directory.clone(),
            Authz::new(
                orgs.clone(),
                audit.clone(),
                hooks.policy.clone(),
                clock.clone(),
            ),
            config.packages.clone(),
        ));
        let pages = Arc::new(Pages::new(
            db.clone(),
            orgs.clone(),
            Authz::new(
                orgs.clone(),
                audit.clone(),
                hooks.policy.clone(),
                clock.clone(),
            ),
            hooks.community.vcs.clone(),
            hooks.packages.blobs.clone(),
            hooks.pages.clone(),
            config.pages.clone(),
        ));
        events.add_listener(Arc::new(PagesListener::new(pages.clone())));
        let authn = Authenticator::new(keys, revocations, ids, hooks.directory.clone());
        let authz = Authz::new(orgs.clone(), audit.clone(), hooks.policy.clone(), clock);
        Ok((
            Arc::new(Forge {
                db,
                registry,
                orgs,
                leases,
                checks,
                work,
                events,
                webhooks,
                audit,
                community,
                packages,
                pages,
                relay,
                authn,
                authz,
                hooks,
                config,
                write_guard: RwLock::new(None),
                changes: std::sync::OnceLock::new(),
                forks: std::sync::OnceLock::new(),
            }),
            report,
        ))
    }

    /// Installs the change-request module, so services of the core that delegate to it
    /// (`RepoService.GetContext`) can reach it. Only the first call takes effect.
    pub fn set_changes(&self, changes: Arc<crate::changes::Changes>) {
        let _ = self.changes.set(changes);
    }

    /// The installed change-request module, if any.
    pub fn changes(&self) -> Option<&Arc<crate::changes::Changes>> {
        self.changes.get()
    }

    /// Installs the forks module, so `RepoService.ForkRepo`, `ListForks`, `SyncFork` and fork
    /// change requests are served. Only the first call takes effect.
    pub fn set_forks(&self, forks: Arc<crate::forks::Forks>) {
        let _ = self.forks.set(forks);
    }

    /// The installed forks module, if any.
    pub fn forks(&self) -> Option<&Arc<crate::forks::Forks>> {
        self.forks.get()
    }

    /// Installs the moderation guard consulted by [`Forge::guard_write`].
    pub fn set_write_guard(&self, guard: Arc<dyn crate::hooks::WriteGuard>) {
        if let Ok(mut g) = self.write_guard.write() {
            *g = Some(guard);
        }
    }

    /// Asks the moderation guard (if installed) whether a write may proceed.
    pub async fn guard_write(
        &self,
        cx: &Cx,
        repo: &bee_proto::v1::Repo,
        kind: crate::hooks::GuardedWrite,
        subject: Option<bee_proto::v1::SubjectRef>,
    ) -> Result<()> {
        let guard = self.write_guard.read().ok().and_then(|g| g.clone());
        match guard {
            Some(g) => g.check(cx, repo, kind, subject).await,
            None => Ok(()),
        }
    }

    /// Drains every outbox, dispatches webhook jobs and delivers the due ones once
    /// (deterministic progress for tests and `--dev` tooling).
    pub async fn flush(&self) -> Result<()> {
        let cx = Cx::system();
        self.relay.drain_all(&cx).await?;
        self.community.flush(&cx, &self.events).await?;
        self.relay.drain_all(&cx).await?;
        self.webhooks.dispatch_once(&cx).await?;
        self.webhooks.deliver_due(&cx).await?;
        Ok(())
    }

    /// One maintenance pass: expired leases, event retention, purge of deleted rows.
    pub async fn maintain(&self) -> Result<()> {
        let cx = Cx::system();
        self.leases.sweep_expired(&cx).await?;
        self.events.prune(&cx).await?;
        self.orgs.purge_expired(&cx).await?;
        self.community.maintain(&cx).await?;
        self.work.sweep_projects(&cx).await?;
        self.packages.maintain(&cx).await?;
        self.pages.maintain(&cx).await?;
        if let Some(posts) = self.changes().and_then(|c| c.posts()) {
            posts.maintain(&cx).await?;
        }
        Ok(())
    }

    /// Starts the background workers; dropping or stopping the handle ends them.
    pub fn spawn_workers(self: &Arc<Self>) -> Workers {
        let (stop_tx, stop) = watch::channel(false);
        let mut tasks = self
            .relay
            .clone()
            .spawn(self.config.relay_poll, stop.clone());
        let me = self.clone();
        let mut s = stop.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                if *s.borrow() {
                    return;
                }
                let cx = Cx::system();
                let _ = me.webhooks.dispatch_once(&cx).await;
                let _ = me.webhooks.deliver_due(&cx).await;
                tokio::select! {
                    _ = tokio::time::sleep(me.config.webhook_poll) => {}
                    _ = s.changed() => {}
                }
            }
        }));
        let me = self.clone();
        let mut s = stop.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                if *s.borrow() {
                    return;
                }
                let _ = me.community.flush(&Cx::system(), &me.events).await;
                tokio::select! {
                    _ = tokio::time::sleep(me.config.webhook_poll) => {}
                    _ = s.changed() => {}
                }
            }
        }));
        let me = self.clone();
        let mut s = stop;
        tasks.push(tokio::spawn(async move {
            loop {
                if *s.borrow() {
                    return;
                }
                let _ = me.maintain().await;
                tokio::select! {
                    _ = tokio::time::sleep(me.config.maintenance_interval) => {}
                    _ = s.changed() => {}
                }
            }
        }));
        Workers {
            stop: stop_tx,
            tasks,
        }
    }
}

/// Running background workers.
pub struct Workers {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl Workers {
    /// Signals every worker to stop and waits for them.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        for t in self.tasks {
            let _ = t.await;
        }
    }
}
