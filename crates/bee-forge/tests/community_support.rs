//! Shared setup for the community integration tests (included with `#[path]`; it has no
//! tests of its own).
#![allow(dead_code, unused_imports, missing_docs)]

use crate::common;
use std::sync::Arc;

use async_trait::async_trait;
use bee_auth::{InMemoryRevocations, KeyRing, SigningKey};
use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::community::Ports;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bee_forge::community::ports::{
    DependencyQuery, DependencySource, MemoryActivity, MemoryChanges, MemoryIssues, MemoryVcs,
    SponsorPayments, VcsRepo,
};
use bee_forge::error::Result;
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, RevisionResolver};
use bee_forge::ids::{Clock, ManualClock};
use bee_forge::service::ForgeServices;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_proto::v1::org_service_server::OrgService;
use bee_proto::v1::repo_service_server::RepoService;
use bee_proto::v1::*;

pub use common::{actions, reason, req};

/// Start of the manual clock: 2026-10-18 12:00 UTC (a Sunday).
pub const T0: u64 = 1_792_324_800_000;

/// Resolves revisions through the in-memory VCS.
pub struct VcsResolver(pub Arc<MemoryVcs>);

#[async_trait]
impl RevisionResolver for VcsResolver {
    async fn resolve(&self, repo: &Repo, expr: &str) -> Result<ObjectId> {
        use bee_forge::community::ports::ContentVcs;
        self.0.resolve(&VcsRepo::of_repo(repo), expr).await
    }
}

/// Org dependencies a test sets.
#[derive(Default)]
pub struct StaticDeps(pub Mutex<Vec<InsightsOrgDependency>>);

#[async_trait]
impl DependencySource for StaticDeps {
    async fn org_dependencies(
        &self,
        _org_id: &str,
        q: &DependencyQuery,
    ) -> Result<Vec<InsightsOrgDependency>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|d| q.ecosystem.is_empty() || d.ecosystem == q.ecosystem)
            .filter(|d| q.query.is_empty() || d.package_name.contains(&q.query))
            .filter(|d| !q.vulnerable_only || d.open_advisories > 0)
            .cloned()
            .collect())
    }
}

/// Payments that either activate at once or hand out a checkout URL.
#[derive(Default)]
pub struct TogglePayments {
    pub pending: AtomicBool,
    pub cancelled: Mutex<Vec<String>>,
}

#[async_trait]
impl SponsorPayments for TogglePayments {
    async fn checkout(&self, s: &Sponsorship) -> Result<Option<String>> {
        Ok(self
            .pending
            .load(Ordering::SeqCst)
            .then(|| format!("https://pay.example/checkout/{}", s.id)))
    }

    async fn cancel(&self, s: &Sponsorship) -> Result<()> {
        self.cancelled.lock().unwrap().push(s.id.clone());
        Ok(())
    }
}

/// The in-memory ports of a forge.
pub struct Parts {
    pub env: common::Env,
    pub vcs: Arc<MemoryVcs>,
    pub changes: Arc<MemoryChanges>,
    pub activity: Arc<MemoryActivity>,
    pub issues: Arc<MemoryIssues>,
    pub deps: Arc<StaticDeps>,
    pub pay: Arc<TogglePayments>,
}

/// A forge with in-memory ports, an org `acme` and two repos.
pub struct World {
    pub env: common::Env,
    pub s: ForgeServices,
    pub vcs: Arc<MemoryVcs>,
    pub changes: Arc<MemoryChanges>,
    pub activity: Arc<MemoryActivity>,
    pub issues: Arc<MemoryIssues>,
    pub deps: Arc<StaticDeps>,
    pub pay: Arc<TogglePayments>,
    pub clock: Arc<ManualClock>,
    /// `acme/app`, private.
    pub app: Repo,
    /// `acme/open`, public.
    pub open: Repo,
}

pub fn rref(full: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::FullName(full.into())),
    })
}

pub fn rid(id: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(id.into())),
    })
}

pub fn human(id: &str) -> Principal {
    Principal {
        id: id.into(),
        kind: PrincipalKind::Human as i32,
        display: id.into(),
        ..Default::default()
    }
}

pub fn agent(id: &str) -> Principal {
    Principal {
        id: id.into(),
        kind: PrincipalKind::Agent as i32,
        display: id.into(),
        ..Default::default()
    }
}

/// Opens a forge over `store` with the in-memory ports and `clock`.
pub async fn forge_on(store: Arc<dyn ForgeStore>, clock: Arc<ManualClock>) -> Parts {
    let ring = KeyRing::new(SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let revocations = Arc::new(InMemoryRevocations::new());
    let directory = Arc::new(MemoryDirectory::new());
    let vcs = Arc::new(MemoryVcs::new());
    let changes = Arc::new(MemoryChanges::new(vcs.clone()));
    let activity = Arc::new(MemoryActivity::new());
    let issues = Arc::new(MemoryIssues::new());
    let deps = Arc::new(StaticDeps::default());
    let pay = Arc::new(TogglePayments::default());
    let ports = Ports {
        vcs: vcs.clone(),
        tags: vcs.clone(),
        changes: changes.clone(),
        activity: activity.clone(),
        issues: issues.clone(),
        deps: deps.clone(),
        payments: pay.clone(),
        ..Ports::default()
    };
    let hooks = Hooks {
        directory: directory.clone(),
        resolver: Arc::new(VcsResolver(vcs.clone())),
        community: ports,
        ..Hooks::default()
    };
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let (forge, _) = Forge::open(
        store,
        clock_dyn.clone(),
        ring.verifying_keys(),
        revocations.clone(),
        hooks,
        common::test_config(),
    )
    .await
    .expect("open forge");
    for (id, kind) in [
        ("owner", PrincipalKind::Human),
        ("admin", PrincipalKind::Human),
        ("writer", PrincipalKind::Human),
        ("triager", PrincipalKind::Human),
        ("reader", PrincipalKind::Human),
        ("outsider", PrincipalKind::Human),
        ("ci", PrincipalKind::Service),
        ("agent", PrincipalKind::Agent),
    ] {
        directory.register(
            Principal {
                id: id.into(),
                kind: kind as i32,
                display: id.into(),
                ..Default::default()
            },
            vec![format!("{id}-login")],
            vec![format!("{id}@example.com")],
        );
    }
    let env = common::Env {
        forge,
        ring,
        revocations,
        directory,
        clock: clock_dyn,
    };
    Parts {
        env,
        vcs,
        changes,
        activity,
        issues,
        deps,
        pay,
    }
}

pub async fn world() -> World {
    world_on(Arc::new(MemoryStore::new())).await
}

pub async fn world_on(store: Arc<dyn ForgeStore>) -> World {
    let clock = Arc::new(ManualClock::new(T0));
    let Parts {
        env,
        vcs,
        changes,
        activity,
        issues,
        deps,
        pay,
    } = forge_on(store, clock.clone()).await;
    let s = ForgeServices::new(env.forge.clone());
    let owner = env.full("owner");
    s.create_org(req(
        &owner,
        CreateOrgRequest {
            name: "acme".into(),
            display: "Acme".into(),
        },
    ))
    .await
    .unwrap();
    let mk = |name: &str, vis: Visibility| {
        s.create_repo(req(
            &owner,
            CreateRepoRequest {
                org: "acme".into(),
                name: name.into(),
                visibility: vis as i32,
                description: format!("the {name} repo"),
                ..Default::default()
            },
        ))
    };
    let app = mk("app", Visibility::Private).await.unwrap().into_inner();
    let open = mk("open", Visibility::Public).await.unwrap().into_inner();
    for (who, role) in [
        ("admin", Role::Admin),
        ("writer", Role::Member),
        ("triager", Role::Member),
        ("reader", Role::Member),
        ("ci", Role::Member),
        ("agent", Role::Member),
    ] {
        s.set_member(req(
            &owner,
            SetMemberRequest {
                org: "acme".into(),
                principal_id: who.into(),
                role: role as i32,
            },
        ))
        .await
        .unwrap();
    }
    for repo in ["acme/app", "acme/open"] {
        for (who, role) in [
            ("writer", RepoRole::Write),
            ("triager", RepoRole::Triage),
            ("agent", RepoRole::Write),
        ] {
            s.add_collaborator(req(
                &owner,
                AddCollaboratorRequest {
                    repo: rref(repo),
                    principal_id: who.into(),
                    role: role as i32,
                    ..Default::default()
                },
            ))
            .await
            .unwrap();
        }
    }
    World {
        env,
        s,
        vcs,
        changes,
        activity,
        issues,
        deps,
        pay,
        clock,
        app,
        open,
    }
}

impl World {
    pub fn tok(&self, who: &str) -> String {
        self.env.full(who)
    }

    /// Drains outboxes, derives notifications and delivers webhooks.
    pub async fn flush(&self) {
        self.env.forge.flush().await.unwrap();
    }

    pub fn cx(&self, who: &str) -> Cx {
        Cx::new(who, PrincipalKind::Human)
    }

    pub fn vr(&self, repo: &Repo) -> VcsRepo {
        VcsRepo::of_repo(repo)
    }

    /// Turns discussions on for a repo.
    pub async fn enable_discussions(&self, repo: &Repo) {
        self.env
            .forge
            .orgs
            .update_repo_settings(&Cx::system(), &repo.id, 0, |s| {
                s.has_discussions = true;
                Ok(())
            })
            .await
            .unwrap();
    }

    /// The kinds of events in an org's log, oldest first.
    pub async fn event_kinds(&self) -> Vec<String> {
        let org = self
            .env
            .forge
            .orgs
            .get_org(&Cx::system(), "acme")
            .await
            .unwrap();
        let (events, _) = self
            .env
            .forge
            .events
            .read(
                &Cx::system(),
                &bee_forge::events::org_log(&org.id),
                0,
                &[],
                1000,
            )
            .await
            .unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }

    /// Principals the in-memory directory knows by id.
    pub async fn principal(&self, id: &str) -> Principal {
        use bee_forge::hooks::PrincipalDirectory;
        self.env.directory.get(id).await.unwrap()
    }
}
