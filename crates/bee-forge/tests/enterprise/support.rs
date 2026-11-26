//! Helpers shared by the `enterprise_*` integration tests. The including test file declares
//! `mod common;` (the forge test environment) and
//! `#[path = "enterprise/support.rs"] mod support;`.
#![allow(dead_code)]

use std::sync::Arc;

use bee_forge::Cx;
use bee_forge::audit::org_scope;
use bee_forge::enterprise::network::{MemoryTwoFactor, StaticResolver};
use bee_forge::enterprise::{Enterprise, EnterpriseConfig};
use bee_forge::ids::{Clock, ManualClock};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::{AuditLogFilter, Org, PrincipalKind, Repo, Role, Visibility};

use crate::common;

/// 2026-11-01T00:00:00Z in milliseconds: the start of every world's manual clock.
pub const NOW_MS: u64 = 1_793_491_200_000;

/// Which store backend a world runs on.
#[derive(Clone, Copy, Debug)]
pub enum Backend {
    /// The in-process store.
    Memory,
    /// SQLite files in a temporary directory.
    Sqlite,
}

/// A forge with the identity side attached, on a manual clock.
pub struct World {
    /// The forge test environment (tokens, directory).
    pub env: common::Env,
    /// The identity side.
    pub ent: Arc<Enterprise>,
    /// The DNS table domain checks resolve against.
    pub dns: Arc<StaticResolver>,
    /// Who has two-factor authentication.
    pub twofa: Arc<MemoryTwoFactor>,
    /// The clock.
    pub clock: Arc<ManualClock>,
    _dir: Option<tempfile::TempDir>,
}

impl World {
    /// The manual clock's time in milliseconds.
    pub fn clock_ms(&self) -> u64 {
        self.clock.now_ms()
    }
}

/// Builds a world on `backend`.
pub async fn world(backend: Backend) -> World {
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let (store, dir): (Arc<dyn ForgeStore>, Option<tempfile::TempDir>) = match backend {
        Backend::Memory => (Arc::new(MemoryStore::new()), None),
        Backend::Sqlite => {
            let dir = tempfile::tempdir().expect("tempdir");
            let c: Arc<dyn Clock> = clock.clone();
            let store = SqlStore::open(SqlConfig::sqlite(dir.path()), c)
                .await
                .expect("open sqlite");
            (Arc::new(store), Some(dir))
        }
    };
    let c: Arc<dyn Clock> = clock.clone();
    let env = common::env_with(store, c).await;
    let dns = Arc::new(StaticResolver::new());
    let twofa = Arc::new(MemoryTwoFactor::new());
    let ent = Enterprise::new(
        env.forge.clone(),
        EnterpriseConfig {
            base_url: "https://hub.example.com".into(),
            dns: dns.clone(),
            two_factor: twofa.clone(),
        },
    );
    ent.attach();
    World {
        env,
        ent,
        dns,
        twofa,
        clock,
        _dir: dir,
    }
}

/// The actions recorded in an audit chain (`org:<id>` or `enterprise:<id>`), oldest first,
/// after draining the outboxes.
pub async fn audit_actions(w: &World, scope: &str) -> Vec<String> {
    w.env.forge.flush().await.expect("flush");
    let scope = scope
        .strip_prefix("org:")
        .map(org_scope)
        .unwrap_or_else(|| scope.to_string());
    let (entries, _) = w
        .env
        .forge
        .audit
        .query(
            &cx("owner"),
            &scope,
            &AuditLogFilter::default(),
            true,
            "",
            500,
        )
        .await
        .expect("audit query");
    entries.into_iter().map(|e| e.action).collect()
}

/// Runs an `async fn $name(World)` flow on both backends as tests `$name::memory` and
/// `$name::sqlite`.
#[macro_export]
macro_rules! on_both_backends {
    ($name:ident) => {
        mod $name {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn memory() {
                super::$name(support::world(support::Backend::Memory).await).await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn sqlite() {
                super::$name(support::world(support::Backend::Sqlite).await).await
            }
        }
    };
}

/// A context for a registered test principal (`owner`, `admin`, `writer`, ...).
pub fn cx(id: &str) -> Cx {
    let kind = match id {
        "ci" => PrincipalKind::Service,
        "agent" => PrincipalKind::Agent,
        _ => PrincipalKind::Human,
    };
    Cx::new(id, kind)
}

/// Creates an org owned by `owner`.
pub async fn make_org(env: &common::Env, name: &str) -> Org {
    let owner = env
        .forge
        .hooks
        .directory
        .get("owner")
        .await
        .expect("owner registered");
    env.forge
        .orgs
        .create_org(&cx("owner"), name, "", owner)
        .await
        .expect("create org")
}

/// Creates a private repo in `org`.
pub async fn make_repo(env: &common::Env, org: &Org, name: &str) -> Repo {
    make_repo_vis(env, org, name, Visibility::Private).await
}

/// Creates a repo with a visibility.
pub async fn make_repo_vis(env: &common::Env, org: &Org, name: &str, v: Visibility) -> Repo {
    let id = env.forge.db.ids().ulid().expect("id");
    env.forge
        .orgs
        .create_repo(
            &cx("owner"),
            &org.id,
            NewRepo {
                id,
                name: name.into(),
                visibility: v,
                ..NewRepo::default()
            },
        )
        .await
        .expect("create repo")
}

/// Adds `principal_id` to `org` with `role`.
pub async fn add_member(env: &common::Env, org: &Org, principal_id: &str, role: Role) {
    let p = env
        .forge
        .hooks
        .directory
        .get(principal_id)
        .await
        .unwrap_or_else(|| panic!("{principal_id} not registered"));
    env.forge
        .orgs
        .set_member(&cx("owner"), &org.id, p, role)
        .await
        .expect("set member");
}
