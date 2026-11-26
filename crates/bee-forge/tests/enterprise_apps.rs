//! The apps platform: apps and manifests, installations and installation tokens, webhooks
//! per app, the OAuth provider (code and device flows, user-to-server tokens) and the
//! marketplace. Store behaviour runs on memory and SQLite (and Postgres when `BEE_TEST_PG`
//! is set); the service permission matrix runs through the tonic traits.

use crate::common;
use crate::enterprise_support as support;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use bee_auth::{Attenuation, KeyRing, Scope, SigningKey, Verifier};
use bee_forge::Cx;
use bee_forge::enterprise::apps::crypto::{pkce_challenge, sign_app_jwt};
use bee_forge::enterprise::apps::{
    Account, AppPatch, Apps, AppsConfig, AppsService, AuthorizationRequest, DomainCheck,
    ListingFilter, ListingInput, MarketplaceConfig, NewApp, NewOAuthApp, OAuthAppPatch, PlanInput,
    PlanPatch, PurchaseInput, RevokeTarget, SsoGate, TokenRequest, catalog,
};
use bee_forge::enterprise::common::{IpPolicy, SpendGate, TokenRevoker};
use bee_forge::error::{ForgeError, Result as FResult};
use bee_forge::events::delivery::verify_signature;
use bee_forge::ids::{Clock, ManualClock};
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlBackend, SqlConfig, SqlStore};
use bee_policy::Action;
use bee_proto::v1::app_service_server::AppService;
use bee_proto::v1::*;

use common::{reason, req};

static SCHEMA: AtomicU64 = AtomicU64::new(0);
const T0: u64 = 1_767_225_600_000; // 2026-01-01

#[derive(Default)]
struct TestRevocationState {
    attempts: usize,
    failures_remaining: usize,
    fail_all: bool,
    failed_ids: BTreeSet<String>,
    revoked: BTreeSet<String>,
}

#[derive(Clone, Default)]
struct TestRevocationHandle(Arc<Mutex<TestRevocationState>>);

impl TestRevocationHandle {
    fn attempts(&self) -> usize {
        self.0.lock().unwrap().attempts
    }

    fn is_revoked(&self, id: &str) -> bool {
        self.0.lock().unwrap().revoked.contains(id)
    }
}

#[async_trait]
impl TokenRevoker for TestRevocationHandle {
    async fn revoke(&self, token_id: &str) -> FResult<()> {
        let mut state = self.0.lock().unwrap();
        state.attempts += 1;
        if state.fail_all || state.failures_remaining > 0 {
            state.failures_remaining = state.failures_remaining.saturating_sub(1);
            state.failed_ids.insert(token_id.to_string());
            return Err(ForgeError::unavailable(
                "test revocation store write failed",
            ));
        }
        state.revoked.insert(token_id.to_string());
        Ok(())
    }
}

async fn world_with_revoker(revoker: Arc<dyn TokenRevoker>) -> World {
    world_with(Backend::Memory, move |mut cfg| {
        cfg.revoker = revoker;
        cfg
    })
    .await
    .expect("memory world")
}

async fn install_with_token(w: &World) -> (AppInstallation, String) {
    let app = new_app(w, "Revocation test", false, &[("contents", "read")]).await;
    let install = install_selected(w, &app, &[&w.repos[0]]).await;
    let token = token_for(w, &install, &[], &[]).await;
    (install, token.token_id)
}

#[derive(Clone, Copy, Debug)]
enum Backend {
    Memory,
    Sqlite,
    Postgres,
}

struct World {
    env: common::Env,
    clock: Arc<ManualClock>,
    apps: Arc<Apps>,
    svc: AppsService,
    ring: Arc<KeyRing>,
    org: Org,
    repos: Vec<Repo>,
    _dir: Option<tempfile::TempDir>,
}

fn ring() -> Arc<KeyRing> {
    // The same key the test environment verifies with.
    Arc::new(KeyRing::new(
        SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap(),
    ))
}

async fn world(b: Backend) -> Option<World> {
    world_with(b, |c| c).await
}

async fn world_with(b: Backend, tweak: impl FnOnce(AppsConfig) -> AppsConfig) -> Option<World> {
    let clock = Arc::new(ManualClock::new(T0));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let mut dir = None;
    let store: Arc<dyn ForgeStore> = match b {
        Backend::Memory => Arc::new(MemoryStore::new()),
        Backend::Sqlite => {
            let d = tempfile::tempdir().unwrap();
            let s = SqlStore::open(SqlConfig::sqlite(d.path()), dyn_clock.clone())
                .await
                .unwrap();
            dir = Some(d);
            Arc::new(s)
        }
        Backend::Postgres => {
            let Ok(url) = std::env::var("BEE_TEST_PG") else {
                eprintln!("BEE_TEST_PG not set; skipping Postgres");
                return None;
            };
            let schema = format!(
                "apps_test_{}_{}",
                std::process::id(),
                SCHEMA.fetch_add(1, Ordering::SeqCst)
            );
            let mut cfg = SqlConfig::parse(&url).unwrap();
            if let SqlBackend::Postgres { schema: s, .. } = &mut cfg.backend {
                *s = schema;
            }
            cfg.default_pool_size = 3;
            Arc::new(SqlStore::open(cfg, dyn_clock.clone()).await.unwrap())
        }
    };
    let env = common::env_with(store, dyn_clock).await;
    env.directory.register(
        Principal {
            id: "appmgr".into(),
            kind: PrincipalKind::Human as i32,
            display: "appmgr".into(),
            ..Default::default()
        },
        vec!["appmgr-login".into()],
        vec![],
    );
    let org = support::make_org(&env, "acme").await;
    for (who, role) in [
        ("admin", Role::Admin),
        ("appmgr", Role::AppManager),
        ("writer", Role::Member),
        ("reader", Role::Member),
    ] {
        support::add_member(&env, &org, who, role).await;
    }
    let mut repos = Vec::new();
    for n in ["r1", "r2", "r3"] {
        repos.push(support::make_repo(&env, &org, n).await);
    }
    let ring = ring();
    let cfg = tweak(AppsConfig::new(ring.clone(), env.revocations.clone()));
    let apps = Apps::new(env.forge.clone(), cfg);
    apps.attach();
    let svc = AppsService::new(env.forge.clone(), apps.clone());
    Some(World {
        env,
        clock,
        apps,
        svc,
        ring,
        org,
        repos,
        _dir: dir,
    })
}

/// Declares one test per backend running `$f`.
macro_rules! on_backends {
    ($name:ident, $f:ident) => {
        mod $name {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn memory() {
                if let Some(w) = world(Backend::Memory).await {
                    $f(w).await;
                }
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn sqlite() {
                if let Some(w) = world(Backend::Sqlite).await {
                    $f(w).await;
                }
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn postgres() {
                if let Some(w) = world(Backend::Postgres).await {
                    $f(w).await;
                }
            }
        }
    };
}

// ---------- helpers ----------

fn owner_cx() -> Cx {
    support::cx("owner")
}

fn acme(w: &World) -> Account {
    Account::org(&w.org.id, &w.org.name)
}

fn manifest(
    name: &str,
    public: bool,
    hook: Option<&str>,
    perms: &[(&str, &str)],
    events: &[&str],
) -> String {
    let perms: serde_json::Map<String, serde_json::Value> = perms
        .iter()
        .map(|(r, l)| (r.to_string(), serde_json::Value::String(l.to_string())))
        .collect();
    let mut m = serde_json::json!({
        "name": name,
        "url": "https://example.com/app",
        "public": public,
        "default_permissions": perms,
        "default_events": events,
        "callback_urls": ["https://example.com/callback"],
    });
    if let Some(h) = hook {
        m["hook_attributes"] = serde_json::json!({ "url": h, "active": true });
    }
    m.to_string()
}

async fn new_app(w: &World, name: &str, public: bool, perms: &[(&str, &str)]) -> NewApp {
    w.apps
        .create_app(
            &owner_cx(),
            &acme(w),
            &manifest(name, public, None, perms, &[]),
        )
        .await
        .unwrap()
}

fn now_secs(w: &World) -> u64 {
    w.clock.now_ms() / 1000
}

fn jwt(w: &World, n: &NewApp) -> String {
    sign_app_jwt(
        &n.private_key_pem,
        &n.private_key_id,
        &n.app.client_id,
        now_secs(w),
        now_secs(w) + 300,
    )
    .unwrap()
}

fn rid(r: &Repo) -> RepoRef {
    RepoRef {
        repo: Some(repo_ref::Repo::Id(r.id.clone())),
    }
}

fn caller_of(w: &World, token: &str) -> bee_forge::auth::Caller {
    w.env.forge.authn.authenticate(&req(token, ())).unwrap()
}

fn verify(w: &World, token: &str) -> Result<bee_auth::VerifiedToken, bee_auth::AuthError> {
    let keys = w.ring.verifying_keys();
    Verifier::new(&keys, w.env.revocations.as_ref()).verify(token, now_secs(w))
}

fn scope_req(repo: &Repo, a: Action) -> bee_auth::Request {
    bee_auth::Request::new(repo.id.clone(), a)
}

fn subcode<T: std::fmt::Debug>(r: Result<tonic::Response<T>, tonic::Status>) -> String {
    let st = r.expect_err("expected failure");
    bee_proto::errors::detail_from_status(&st)
        .map(|d| d.subcode)
        .unwrap_or_default()
}

async fn install_selected(w: &World, n: &NewApp, repos: &[&Repo]) -> AppInstallation {
    let repos: Vec<Repo> = repos.iter().map(|r| (*r).clone()).collect();
    w.apps
        .create_installation(
            &owner_cx(),
            &n.app.id,
            &acme(w),
            AppRepositorySelection::Selected,
            &repos,
        )
        .await
        .unwrap()
}

async fn token_for(
    w: &World,
    inst: &AppInstallation,
    repos: &[&Repo],
    perms: &[(&str, Level)],
) -> bee_forge::enterprise::apps::InstallationToken {
    w.apps
        .create_installation_token(
            &owner_cx(),
            &inst.id,
            TokenRequest {
                repo_ids: repos.iter().map(|r| r.id.clone()).collect(),
                permissions: perms
                    .iter()
                    .map(|(r, l)| AppPermission {
                        resource: r.to_string(),
                        level: l.to_proto() as i32,
                    })
                    .collect(),
                ttl_seconds: 0,
            },
        )
        .await
        .unwrap()
}

use catalog::Level;

// ---------- manifests and apps ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn suspend_returns_unavailable_and_keeps_install_active_when_revocation_writes_fail() {
    let handle = TestRevocationHandle::default();
    handle.0.lock().unwrap().fail_all = true;
    let w = world_with_revoker(Arc::new(handle.clone())).await;
    let (install, _) = install_with_token(&w).await;

    let error = w
        .apps
        .suspend_installation(&owner_cx(), &install.id, "investigating")
        .await
        .unwrap_err();

    assert_eq!(error.reason, ErrorReason::Unavailable);
    assert_eq!(handle.attempts(), 3, "initial write plus two retries");
    let current = w
        .apps
        .get_installation(&owner_cx(), &install.id)
        .await
        .unwrap();
    assert!(
        !current.suspended,
        "failed revocation must roll back suspension"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn suspend_retries_a_transient_revocation_write_failure() {
    let handle = TestRevocationHandle::default();
    handle.0.lock().unwrap().failures_remaining = 1;
    let w = world_with_revoker(Arc::new(handle.clone())).await;
    let (install, _) = install_with_token(&w).await;

    let suspended = w
        .apps
        .suspend_installation(&owner_cx(), &install.id, "investigating")
        .await
        .unwrap();

    assert!(suspended.suspended);
    let state = handle.0.lock().unwrap();
    assert_eq!(
        state.attempts, 3,
        "one token is retried and another succeeds"
    );
    assert_eq!(state.failed_ids.len(), 1, "exactly one write fails");
    assert!(
        state.failed_ids.iter().all(|id| state.revoked.contains(id)),
        "the failed token write must be retried successfully"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn successful_suspend_has_written_revocation_visible_through_second_handle() {
    let writer = TestRevocationHandle::default();
    let second_handle = writer.clone();
    let w = world_with_revoker(Arc::new(writer)).await;
    let (install, token_id) = install_with_token(&w).await;

    w.apps
        .suspend_installation(&owner_cx(), &install.id, "investigating")
        .await
        .expect("suspend succeeds after persisting revocation");

    assert!(
        second_handle.is_revoked(&token_id),
        "the second shared-store handle must see the revocation before success returns"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_manifests_are_rejected_with_bad_manifest() {
    let w = world(Backend::Memory).await.unwrap();
    let org = acme(&w);
    let owner = w.env.full("owner");
    let bad = |m: serde_json::Value| m.to_string();
    let cases = [
        "not json".to_string(),
        "[1,2]".to_string(),
        bad(serde_json::json!({ "url": "https://x.example" })),
        bad(serde_json::json!({ "name": "x", "url": "ftp://x.example" })),
        bad(serde_json::json!({ "name": "x", "url": "https://x.example", "surprise": 1 })),
        bad(
            serde_json::json!({ "name": "x", "url": "https://x.example", "default_permissions": { "nope": "read" } }),
        ),
        bad(
            serde_json::json!({ "name": "x", "url": "https://x.example", "default_permissions": { "contents": "owner" } }),
        ),
        bad(
            serde_json::json!({ "name": "x", "url": "https://x.example", "default_events": ["nope.never"], "hook_attributes": { "url": "https://h.example/x" } }),
        ),
        bad(
            serde_json::json!({ "name": "x", "url": "https://x.example", "default_events": ["lease.*"] }),
        ),
        bad(
            serde_json::json!({ "name": "x", "url": "https://x.example", "hook_attributes": { "active": true } }),
        ),
        bad(serde_json::json!({ "name": "!!!", "url": "https://x.example" })),
    ];
    for m in cases {
        let e = w.apps.create_app(&owner_cx(), &org, &m).await.unwrap_err();
        assert_eq!(e.reason, ErrorReason::InvalidArgument, "{m}");
        assert_eq!(e.subcode, "bad_manifest", "{m}");
        // The same through the service.
        let r = w
            .svc
            .create_app_from_manifest(req(
                &owner,
                CreateAppFromManifestRequest {
                    org: "acme".into(),
                    manifest_json: m.clone(),
                },
            ))
            .await;
        assert_eq!(subcode(r), "bad_manifest", "{m}");
    }
    // A good one, and slug uniqueness.
    let ok = manifest(
        "Triage Bot",
        true,
        Some("https://hooks.example/x"),
        &[("contents", "read"), ("issues", "write")],
        &["lease.*", "check.updated"],
    );
    let n = w.apps.create_app(&owner_cx(), &org, &ok).await.unwrap();
    assert_eq!(n.app.slug, "triage-bot");
    assert!(n.app.client_id.starts_with("Iv1."));
    assert!(n.client_secret.starts_with("cs_"));
    assert!(n.private_key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
    // metadata:read is always requested.
    assert!(
        n.app
            .permissions
            .iter()
            .any(|p| p.resource == "metadata" && p.level == AppPermissionLevel::Read as i32)
    );
    let dup = w.apps.create_app(&owner_cx(), &org, &ok).await.unwrap_err();
    assert_eq!(dup.reason, ErrorReason::AlreadyExists);
}

async fn app_lifecycle(w: World) {
    let n = new_app(&w, "Lifecycle", true, &[("contents", "read")]).await;
    let cx = owner_cx();
    // Secrets are shown once: the stored app never carries them.
    let got = w.apps.get_app(&cx, &n.app.slug).await.unwrap();
    assert_eq!(got.id, n.app.id);
    assert_eq!(got.private_keys.len(), 1);
    assert_eq!(got.private_keys[0].id, n.private_key_id);
    assert!(!got.private_keys[0].fingerprint.is_empty());
    assert_eq!(got.owner, "acme");
    // By client id and id as well.
    assert_eq!(
        w.apps.get_app(&cx, &n.app.client_id).await.unwrap().id,
        n.app.id
    );
    assert_eq!(w.apps.get_app(&cx, &n.app.id).await.unwrap().id, n.app.id);

    // Keys: a second one, then the first can go, the last cannot.
    let (k2, pem2) = w.apps.create_private_key(&cx, &n.app.id).await.unwrap();
    assert!(pem2.contains("PRIVATE KEY"));
    w.apps
        .delete_private_key(&cx, &n.app.id, &n.private_key_id)
        .await
        .unwrap();
    let e = w
        .apps
        .delete_private_key(&cx, &n.app.id, &k2.id)
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InvalidState);

    // JWT authentication: the deleted key no longer works, the new one does (by client id and
    // by app id).
    let old = sign_app_jwt(
        &n.private_key_pem,
        &n.private_key_id,
        &n.app.client_id,
        now_secs(&w),
        now_secs(&w) + 60,
    )
    .unwrap();
    assert_eq!(
        w.apps.authenticate_app_jwt(&old).await.unwrap_err().reason,
        ErrorReason::Unauthenticated
    );
    let good = sign_app_jwt(
        &pem2,
        &k2.id,
        &n.app.client_id,
        now_secs(&w),
        now_secs(&w) + 60,
    )
    .unwrap();
    assert_eq!(
        w.apps.authenticate_app_jwt(&good).await.unwrap().id,
        n.app.id
    );
    let by_id = sign_app_jwt(&pem2, &k2.id, &n.app.id, now_secs(&w), now_secs(&w) + 60).unwrap();
    assert_eq!(
        w.apps.authenticate_app_jwt(&by_id).await.unwrap().id,
        n.app.id
    );
    // Lifetime above ten minutes, expired, someone else's key, garbage.
    let long = sign_app_jwt(&pem2, &k2.id, &n.app.id, now_secs(&w), now_secs(&w) + 601).unwrap();
    assert!(w.apps.authenticate_app_jwt(&long).await.is_err());
    let other = new_app(&w, "Other", true, &[]).await;
    let forged = sign_app_jwt(
        &other.private_key_pem,
        &k2.id,
        &n.app.id,
        now_secs(&w),
        now_secs(&w) + 60,
    )
    .unwrap();
    assert!(w.apps.authenticate_app_jwt(&forged).await.is_err());
    assert!(w.apps.authenticate_app_jwt("a.b.c").await.is_err());
    w.clock.advance(61_000);
    assert_eq!(
        w.apps.authenticate_app_jwt(&good).await.unwrap_err().reason,
        ErrorReason::TokenExpired
    );

    // Update: settings change at once.
    let u = w
        .apps
        .update_app(
            &cx,
            &n.app.slug,
            AppPatch {
                description: Some("does things".into()),
                public: Some(false),
                ..AppPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(u.description, "does things");
    assert!(!u.public);
    assert!(
        w.apps
            .update_app(
                &cx,
                &n.app.slug,
                AppPatch {
                    homepage_url: Some("javascript:alert(1)".into()),
                    ..AppPatch::default()
                },
            )
            .await
            .is_err()
    );

    // Listing by owner.
    let (list, _) = w.apps.list_apps(&cx, &acme(&w), "", 50).await.unwrap();
    assert_eq!(list.len(), 2);

    // Delete removes it for good.
    w.apps.delete_app(&cx, &n.app.id).await.unwrap();
    assert_eq!(
        w.apps.get_app(&cx, &n.app.id).await.unwrap_err().reason,
        ErrorReason::NotFound
    );
    assert_eq!(
        w.apps
            .list_apps(&cx, &acme(&w), "", 50)
            .await
            .unwrap()
            .0
            .len(),
        1
    );
}
on_backends!(app_lifecycle_backends, app_lifecycle);

async fn permission_updates(w: World) {
    let cx = owner_cx();
    let n = new_app(
        &w,
        "Perms",
        true,
        &[("contents", "read"), ("issues", "write")],
    )
    .await;
    let inst = install_selected(&w, &n, &[&w.repos[0]]).await;
    assert!(!inst.permissions_update_pending);

    // Raising permissions leaves the installation on its old grant.
    w.apps
        .update_app(
            &cx,
            &n.app.id,
            AppPatch {
                permissions: Some(
                    [
                        ("contents".to_string(), Level::Write),
                        ("issues".to_string(), Level::Write),
                    ]
                    .into_iter()
                    .collect(),
                ),
                ..AppPatch::default()
            },
        )
        .await
        .unwrap();
    let after = w.apps.get_installation(&cx, &inst.id).await.unwrap();
    assert!(after.permissions_update_pending);
    let contents = |i: &AppInstallation| {
        i.permissions
            .iter()
            .find(|p| p.resource == "contents")
            .map(|p| p.level)
    };
    assert_eq!(contents(&after), Some(AppPermissionLevel::Read as i32));
    // The installation principal is still only a reader.
    let role = w
        .env
        .forge
        .orgs
        .effective_role(&cx, &w.repos[0], &inst.principal_id)
        .await
        .unwrap();
    assert_eq!(role.role, RepoRole::Triage as i32); // issues:write needs triage
    // A token cannot ask for the new level.
    let denied = w
        .apps
        .create_installation_token(
            &cx,
            &inst.id,
            TokenRequest {
                permissions: vec![AppPermission {
                    resource: "contents".into(),
                    level: AppPermissionLevel::Write as i32,
                }],
                ..TokenRequest::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(denied.reason, ErrorReason::PermissionDenied);

    // An installer accepts.
    let accepted = w
        .apps
        .accept_installation_permissions(&cx, &inst.id)
        .await
        .unwrap();
    assert!(!accepted.permissions_update_pending);
    assert_eq!(contents(&accepted), Some(AppPermissionLevel::Write as i32));
    let role = w
        .env
        .forge
        .orgs
        .effective_role(&cx, &w.repos[0], &inst.principal_id)
        .await
        .unwrap();
    assert_eq!(role.role, RepoRole::Write as i32);

    // Lowering applies at once.
    w.apps
        .update_app(
            &cx,
            &n.app.id,
            AppPatch {
                permissions: Some([("issues".to_string(), Level::Read)].into_iter().collect()),
                ..AppPatch::default()
            },
        )
        .await
        .unwrap();
    let low = w.apps.get_installation(&cx, &inst.id).await.unwrap();
    assert!(!low.permissions_update_pending);
    assert_eq!(contents(&low), None);
    let role = w
        .env
        .forge
        .orgs
        .effective_role(&cx, &w.repos[0], &inst.principal_id)
        .await
        .unwrap();
    assert_eq!(role.role, RepoRole::Read as i32);
}
on_backends!(permission_updates_backends, permission_updates);

// ---------- installation tokens: an app on two repos never touches a third ----------

async fn org_events(w: &World) -> Vec<String> {
    w.env.forge.flush().await.unwrap();
    let (events, _) = w
        .env
        .forge
        .events
        .read(
            &owner_cx(),
            &bee_forge::events::org_log(&w.org.id),
            1,
            &[],
            1000,
        )
        .await
        .unwrap();
    events.into_iter().map(|e| e.kind).collect()
}

async fn audit_actions(w: &World) -> Vec<String> {
    w.env.forge.flush().await.unwrap();
    let (entries, _) = w
        .env
        .forge
        .audit
        .query(
            &owner_cx(),
            &bee_forge::audit::org_scope(&w.org.id),
            &AuditLogFilter::default(),
            true,
            "",
            500,
        )
        .await
        .unwrap();
    entries.into_iter().map(|e| e.action).collect()
}

async fn install_scoping(w: World) {
    let cx = owner_cx();
    let n = new_app(
        &w,
        "Scoped",
        true,
        &[("contents", "write"), ("issues", "read")],
    )
    .await;
    let (r1, r2, r3) = (&w.repos[0], &w.repos[1], &w.repos[2]);
    let inst = install_selected(&w, &n, &[r1, r2]).await;
    let tok = token_for(&w, &inst, &[], &[]).await;
    assert_eq!(tok.repo_ids.len(), 2);

    // Level 1: the token's scope, checked by the verifier.
    let vt = verify(&w, &tok.token).unwrap();
    assert_eq!(vt.principal.id, inst.principal_id);
    assert!(
        vt.scope.repos.is_some(),
        "a token never has unrestricted repos"
    );
    let now = now_secs(&w);
    for r in [r1, r2] {
        for a in [Action::Read, Action::Commit, Action::Land, Action::Lease] {
            vt.authorize(&scope_req(r, a), now).unwrap();
        }
    }
    for a in Action::ALL {
        assert!(
            vt.authorize(&scope_req(r3, a), now).is_err(),
            "third repo: {a}"
        );
    }
    assert!(
        vt.authorize(&scope_req(r1, Action::ManageSettings), now)
            .is_err()
    );
    assert!(
        vt.authorize(&scope_req(r1, Action::ForceUpdate), now)
            .is_err()
    );

    // Level 2: the forge's authorization with a Caller built from the token.
    let caller = caller_of(&w, &tok.token);
    let authz = &w.env.forge.authz;
    for r in [r1, r2] {
        authz
            .repo(&caller, r, Action::Commit, &[], None, None)
            .await
            .unwrap();
        authz
            .repo(&caller, r, Action::Read, &[], None, None)
            .await
            .unwrap();
    }
    for a in [Action::Read, Action::Commit] {
        assert!(authz.repo(&caller, r3, a, &[], None, None).await.is_err());
    }
    assert!(
        authz
            .repo(&caller, r1, Action::ManageSettings, &[], None, None)
            .await
            .is_err()
    );

    // Level 3: the access source behind the roles.
    let role = |r: &Repo| {
        let orgs = w.env.forge.orgs.clone();
        let (cx, r, p) = (owner_cx(), r.clone(), inst.principal_id.clone());
        async move { orgs.effective_role(&cx, &r, &p).await.unwrap() }
    };
    let p1 = role(r1).await;
    assert_eq!(p1.role, RepoRole::Write as i32);
    assert!(
        p1.sources.iter().any(|s| s == "app:scoped"),
        "{:?}",
        p1.sources
    );
    assert_eq!(role(r2).await.role, RepoRole::Write as i32);
    assert_eq!(role(r3).await.role, RepoRole::None as i32);

    // A token cannot exceed the installation grant, and can be narrowed.
    let denied = |repos: Vec<String>, perms: Vec<(&'static str, Level)>| {
        let apps = w.apps.clone();
        let id = inst.id.clone();
        async move {
            apps.create_installation_token(
                &owner_cx(),
                &id,
                TokenRequest {
                    repo_ids: repos,
                    permissions: perms
                        .into_iter()
                        .map(|(r, l)| AppPermission {
                            resource: r.into(),
                            level: l.to_proto() as i32,
                        })
                        .collect(),
                    ttl_seconds: 0,
                },
            )
            .await
            .unwrap_err()
        }
    };
    assert_eq!(
        denied(vec![r3.id.clone()], vec![]).await.reason,
        ErrorReason::PermissionDenied
    );
    assert_eq!(
        denied(vec![], vec![("administration", Level::Admin)])
            .await
            .reason,
        ErrorReason::PermissionDenied
    );
    assert_eq!(
        denied(vec![], vec![("contents", Level::Admin)])
            .await
            .reason,
        ErrorReason::PermissionDenied
    );
    assert_eq!(
        denied(vec![], vec![("issues", Level::Write)]).await.reason,
        ErrorReason::PermissionDenied
    );
    assert_eq!(
        denied(vec![], vec![("nonsense", Level::Read)]).await.reason,
        ErrorReason::InvalidArgument
    );
    assert_eq!(
        denied(vec![r1.id.clone(), r3.id.clone()], vec![])
            .await
            .reason,
        ErrorReason::PermissionDenied
    );

    let narrow = token_for(&w, &inst, &[r1], &[("contents", Level::Read)]).await;
    assert_eq!(narrow.repo_ids, vec![r1.id.clone()]);
    let vn = verify(&w, &narrow.token).unwrap();
    vn.authorize(&scope_req(r1, Action::Read), now).unwrap();
    assert!(vn.authorize(&scope_req(r1, Action::Commit), now).is_err());
    assert!(vn.authorize(&scope_req(r2, Action::Read), now).is_err());
    // Attenuating offline can only narrow further, never widen.
    let wider = bee_auth::attenuate(
        &narrow.token,
        &Attenuation {
            scope: Scope::new()
                .repos([r1.id.as_str(), r2.id.as_str(), r3.id.as_str()])
                .unwrap()
                .actions(Action::ALL),
            act_as: None,
        },
    )
    .unwrap();
    let vw = verify(&w, &wider.text).unwrap();
    for (r, a) in [(r2, Action::Read), (r3, Action::Read), (r1, Action::Commit)] {
        assert!(vw.authorize(&scope_req(r, a), now).is_err());
    }

    // Adding a repo: new tokens cover it, the old token's scope still does not; removing a
    // repo: the access source denies it even though the old token's scope lists it.
    assert_eq!(
        w.apps
            .add_installation_repos(&cx, &inst.id, std::slice::from_ref(r3))
            .await
            .unwrap(),
        3
    );
    assert!(vt.authorize(&scope_req(r3, Action::Read), now).is_err());
    let tok2 = token_for(&w, &inst, &[], &[]).await;
    assert_eq!(tok2.repo_ids.len(), 3);
    let c2 = caller_of(&w, &tok2.token);
    authz
        .repo(&c2, r3, Action::Commit, &[], None, None)
        .await
        .unwrap();
    assert_eq!(
        w.apps
            .remove_installation_repos(&cx, &inst.id, std::slice::from_ref(r2))
            .await
            .unwrap(),
        2
    );
    assert_eq!(role(r2).await.role, RepoRole::None as i32);
    vt.authorize(&scope_req(r2, Action::Commit), now).unwrap(); // the token itself still lists it
    let err = authz
        .repo(&caller, r2, Action::Commit, &[], None, None)
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::NotFound, "{err}");
    let (listed, _) = w
        .apps
        .list_installation_repos(&cx, &inst.id, "", 50)
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);

    // Suspension: tokens die, nothing is granted, new tokens are refused, and it lifts.
    let sus = w
        .apps
        .suspend_installation(&cx, &inst.id, "investigating")
        .await
        .unwrap();
    assert!(sus.suspended && sus.suspended_by.is_some());
    for t in [&tok.token, &tok2.token, &narrow.token] {
        let e = w.env.forge.authn.authenticate(&req(t, ())).err().unwrap();
        assert_eq!(e.reason, ErrorReason::TokenRevoked);
        assert_eq!(
            verify(&w, t).unwrap_err().reason,
            bee_auth::AuthReason::TokenRevoked
        );
    }
    assert_eq!(role(r1).await.role, RepoRole::None as i32);
    assert_eq!(
        w.apps
            .create_installation_token(&cx, &inst.id, TokenRequest::default())
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert_eq!(
        w.apps
            .suspend_installation(&cx, &inst.id, "")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    let un = w.apps.unsuspend_installation(&cx, &inst.id).await.unwrap();
    assert!(!un.suspended);
    assert_eq!(role(r1).await.role, RepoRole::Write as i32);
    assert!(
        w.env
            .forge
            .authn
            .authenticate(&req(&tok.token, ()))
            .is_err(),
        "revoked stays revoked"
    );
    let live = token_for(&w, &inst, &[], &[]).await;
    verify(&w, &live.token).unwrap();

    // Uninstalling revokes everything derived from the installation.
    w.apps.delete_installation(&cx, &inst.id).await.unwrap();
    assert_eq!(
        verify(&w, &live.token).unwrap_err().reason,
        bee_auth::AuthReason::TokenRevoked
    );
    assert_eq!(role(r1).await.role, RepoRole::None as i32);
    assert_eq!(
        w.apps
            .get_installation(&cx, &inst.id)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    assert_eq!(
        w.apps
            .get_app(&cx, &n.app.id)
            .await
            .unwrap()
            .installation_count,
        0
    );

    // Events and audit.
    let kinds = org_events(&w).await;
    for k in [
        "app.installed",
        "app.suspended",
        "app.unsuspended",
        "app.uninstalled",
    ] {
        assert!(kinds.iter().any(|x| x == k), "{k} in {kinds:?}");
    }
    let actions = audit_actions(&w).await;
    for a in [
        "app.create",
        "app.install",
        "app.token_create",
        "app.installation_repos_add",
        "app.installation_repos_remove",
        "app.suspend",
        "app.unsuspend",
        "app.uninstall",
    ] {
        assert!(actions.iter().any(|x| x == a), "{a} in {actions:?}");
    }
}
on_backends!(install_scoping_backends, install_scoping);

async fn all_installation(w: World) {
    let cx = owner_cx();
    let n = new_app(&w, "Everywhere", true, &[("contents", "write")]).await;
    let inst = w
        .apps
        .create_installation(&cx, &n.app.id, &acme(&w), AppRepositorySelection::All, &[])
        .await
        .unwrap();
    let tok = token_for(&w, &inst, &[], &[]).await;
    let vt = verify(&w, &tok.token).unwrap();
    let ids: Vec<String> = vt
        .scope
        .repos
        .iter()
        .flatten()
        .map(|p| format!("{p:?}"))
        .collect();
    assert_eq!(ids.len(), 3, "ALL is enumerated, never unrestricted");
    assert_eq!(tok.repo_ids.len(), 3);

    // A repo created later: the access source covers it at once, tokens minted before do not.
    let r4 = support::make_repo(&w.env, &w.org, "r4").await;
    let role = w
        .env
        .forge
        .orgs
        .effective_role(&cx, &r4, &inst.principal_id)
        .await
        .unwrap();
    assert_eq!(role.role, RepoRole::Write as i32);
    let now = now_secs(&w);
    assert!(vt.authorize(&scope_req(&r4, Action::Read), now).is_err());
    let tok2 = token_for(&w, &inst, &[], &[]).await;
    assert_eq!(tok2.repo_ids.len(), 4);
    verify(&w, &tok2.token)
        .unwrap()
        .authorize(&scope_req(&r4, Action::Commit), now)
        .unwrap();
    assert_eq!(
        w.apps
            .list_installation_repos(&cx, &inst.id, "", 50)
            .await
            .unwrap()
            .0
            .len(),
        4
    );

    // Repo lists only apply to SELECTED installations.
    assert_eq!(
        w.apps
            .add_installation_repos(&cx, &inst.id, std::slice::from_ref(&r4))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert_eq!(
        w.apps
            .create_installation(&cx, &n.app.id, &acme(&w), AppRepositorySelection::All, &[])
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );

    // Expiry: default and maximum one hour.
    let exp = verify(&w, &tok2.token).unwrap().scope.expires_at;
    assert_eq!(exp, Some(now + 3600));
    assert_eq!(tok2.expires_at_ms, (now + 3600) * 1000);
    let long = w
        .apps
        .create_installation_token(
            &cx,
            &inst.id,
            TokenRequest {
                ttl_seconds: 3601,
                ..TokenRequest::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(long.reason, ErrorReason::InvalidArgument);
    let short = w
        .apps
        .create_installation_token(
            &cx,
            &inst.id,
            TokenRequest {
                ttl_seconds: 60,
                ..TokenRequest::default()
            },
        )
        .await
        .unwrap();
    w.clock.advance(61_000);
    assert_eq!(
        verify(&w, &short.token).unwrap_err().reason,
        bee_auth::AuthReason::TokenExpired
    );
    verify(&w, &tok2.token).unwrap();
    w.clock.advance(3600 * 1000);
    assert_eq!(
        verify(&w, &tok2.token).unwrap_err().reason,
        bee_auth::AuthReason::TokenExpired
    );
}
on_backends!(all_installation_backends, all_installation);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn installs_validate_their_targets_and_repos() {
    let w = world(Backend::Memory).await.unwrap();
    let cx = owner_cx();
    let n = new_app(&w, "Targets", true, &[("contents", "read")]).await;
    // Repos of another org cannot be selected.
    let outsider = w.env.forge.hooks.directory.get("outsider").await.unwrap();
    let other = w
        .env
        .forge
        .orgs
        .create_org(&support::cx("outsider"), "other", "", outsider)
        .await
        .unwrap();
    let foreign = support::make_repo(&w.env, &other, "o1").await;
    let e = w
        .apps
        .create_installation(
            &cx,
            &n.app.id,
            &acme(&w),
            AppRepositorySelection::Selected,
            std::slice::from_ref(&foreign),
        )
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "repo_not_in_account");
    assert_eq!(
        w.apps
            .create_installation(
                &cx,
                &n.app.id,
                &acme(&w),
                AppRepositorySelection::Unspecified,
                &[],
            )
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidArgument
    );
    // A user account installation covers no repos and still mints (empty) tokens.
    let user = w.apps.user_account("writer").await;
    assert_eq!(user.name, "writer-login");
    let inst = w
        .apps
        .create_installation(
            &support::cx("writer"),
            &n.app.id,
            &user,
            AppRepositorySelection::All,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(inst.target, "writer-login");
    assert_eq!(inst.target_kind, AppInstallationTargetKind::User as i32);
    let tok = token_for(&w, &inst, &[], &[]).await;
    assert!(tok.repo_ids.is_empty());
    let vt = verify(&w, &tok.token).unwrap();
    assert!(
        vt.authorize(&scope_req(&w.repos[0], Action::Read), now_secs(&w))
            .is_err()
    );
    // Listing by account and by app.
    let (by_acct, _) = w
        .apps
        .list_installations_of_account(&cx, &user, "", 10)
        .await
        .unwrap();
    assert_eq!(by_acct.len(), 1);
    let (by_app, _) = w
        .apps
        .list_installations_of_app(&cx, &n.app.id, "", 10)
        .await
        .unwrap();
    assert_eq!(by_app.len(), 1);
    assert_eq!(
        w.apps
            .get_app(&cx, &n.app.id)
            .await
            .unwrap()
            .installation_count,
        1
    );
    // Deleting the app deletes its installations and revokes their tokens.
    w.apps.delete_app(&cx, &n.app.id).await.unwrap();
    assert_eq!(
        verify(&w, &tok.token).unwrap_err().reason,
        bee_auth::AuthReason::TokenRevoked
    );
    assert_eq!(
        w.apps
            .get_installation(&cx, &inst.id)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
}

// ---------- webhooks per app ----------

#[derive(Default)]
struct Receiver {
    got: Vec<(BTreeMap<String, String>, String)>,
}

type Shared = Arc<Mutex<Receiver>>;

async fn hook(State(s): State<Shared>, headers: HeaderMap, body: String) -> StatusCode {
    let h = headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    s.lock().unwrap().got.push((h, body));
    StatusCode::OK
}

async fn receiver() -> (String, Shared) {
    let shared: Shared = Arc::default();
    let app = Router::new()
        .route("/hook", post(hook))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/hook"), shared)
}

fn lease_req(r: &Repo, path: &str) -> bee_forge::leases::AcquireRequest {
    bee_forge::leases::AcquireRequest {
        repo_id: r.id.clone(),
        org_id: r.org_id.clone(),
        ref_name: "main".into(),
        paths: vec![path.as_bytes().to_vec()],
        mode: LeaseMode::Advisory,
        ..Default::default()
    }
}

/// Acquires a lease one second after the previous step and delivers what is due.
async fn lease_and_flush(w: &World, r: &Repo, path: &str) {
    w.clock.advance(1000);
    w.env
        .forge
        .leases
        .acquire(&owner_cx(), lease_req(r, path))
        .await
        .unwrap();
    w.env.forge.flush().await.unwrap();
}

fn deliveries(rx: &Shared) -> Vec<(BTreeMap<String, String>, String)> {
    rx.lock().unwrap().got.clone()
}

async fn event_subscriber_app(w: World) {
    let (url, rx) = receiver().await;
    let cx = owner_cx();
    let n = w
        .apps
        .create_app(
            &cx,
            &acme(&w),
            &manifest(
                "Chat Notifier",
                true,
                Some(&url),
                &[("contents", "read")],
                &["lease.*"],
            ),
        )
        .await
        .unwrap();
    let (r1, r2) = (&w.repos[0], &w.repos[1]);
    w.clock.advance(1000);
    let inst = install_selected(&w, &n, &[r1]).await;

    // Events of the selected repo are delivered, signed with the app's webhook secret; events
    // of a repo the app was not given are not.
    lease_and_flush(&w, r1, "a").await;
    lease_and_flush(&w, r2, "a").await;
    let got = deliveries(&rx);
    assert_eq!(got.len(), 1, "{got:?}");
    let (headers, body) = &got[0];
    assert_eq!(
        headers.get("x-bee-event").map(String::as_str),
        Some("lease.acquired")
    );
    let sig = headers.get("x-bee-signature-256").expect("signature");
    assert!(verify_signature(&n.webhook_secret, body.as_bytes(), sig));
    assert!(!verify_signature("another-secret", body.as_bytes(), sig));
    assert!(body.contains(&r1.id) && !body.contains(&r2.id));

    // A suspended installation gets nothing; lifting the suspension resumes delivery.
    w.apps
        .suspend_installation(&cx, &inst.id, "")
        .await
        .unwrap();
    lease_and_flush(&w, r1, "b").await;
    assert_eq!(deliveries(&rx).len(), 1);
    w.clock.advance(1000);
    w.apps.unsuspend_installation(&cx, &inst.id).await.unwrap();
    lease_and_flush(&w, r1, "c").await;
    assert_eq!(deliveries(&rx).len(), 2);

    // Adding a repo subscribes to it too; removing one stops its events.
    w.clock.advance(1000);
    w.apps
        .add_installation_repos(&cx, &inst.id, std::slice::from_ref(r2))
        .await
        .unwrap();
    lease_and_flush(&w, r2, "b").await;
    assert_eq!(deliveries(&rx).len(), 3);
    w.clock.advance(1000);
    w.apps
        .remove_installation_repos(&cx, &inst.id, std::slice::from_ref(r2))
        .await
        .unwrap();
    lease_and_flush(&w, r2, "c").await;
    assert_eq!(deliveries(&rx).len(), 3);

    // The app's event list and secret apply to every installation at once.
    w.clock.advance(1000);
    w.apps
        .update_app(
            &cx,
            &n.app.id,
            AppPatch {
                events: Some(vec!["ref.*".into()]),
                ..AppPatch::default()
            },
        )
        .await
        .unwrap();
    lease_and_flush(&w, r1, "d").await;
    assert_eq!(deliveries(&rx).len(), 3, "no longer subscribed to leases");
    w.clock.advance(1000);
    w.apps
        .update_app(
            &cx,
            &n.app.id,
            AppPatch {
                events: Some(vec!["lease.acquired".into()]),
                webhook_secret: Some("rotated-secret-1".into()),
                ..AppPatch::default()
            },
        )
        .await
        .unwrap();
    lease_and_flush(&w, r1, "e").await;
    let got = deliveries(&rx);
    assert_eq!(got.len(), 4);
    let (h, b) = &got[3];
    let sig = h.get("x-bee-signature-256").unwrap();
    assert!(verify_signature("rotated-secret-1", b.as_bytes(), sig));
    assert!(!verify_signature(&n.webhook_secret, b.as_bytes(), sig));

    // Switching the webhook off stops delivery; uninstalling does too.
    w.clock.advance(1000);
    w.apps
        .update_app(
            &cx,
            &n.app.id,
            AppPatch {
                webhook_active: Some(false),
                ..AppPatch::default()
            },
        )
        .await
        .unwrap();
    lease_and_flush(&w, r1, "f").await;
    assert_eq!(deliveries(&rx).len(), 4);
    w.clock.advance(1000);
    w.apps
        .update_app(
            &cx,
            &n.app.id,
            AppPatch {
                webhook_active: Some(true),
                ..AppPatch::default()
            },
        )
        .await
        .unwrap();
    lease_and_flush(&w, r1, "g").await;
    assert_eq!(deliveries(&rx).len(), 5);
    w.apps.delete_installation(&cx, &inst.id).await.unwrap();
    lease_and_flush(&w, r1, "h").await;
    assert_eq!(deliveries(&rx).len(), 5);

    // An ALL installation sees every repo of the org.
    w.clock.advance(1000);
    let all = w
        .apps
        .create_installation(&cx, &n.app.id, &acme(&w), AppRepositorySelection::All, &[])
        .await
        .unwrap();
    lease_and_flush(&w, &w.repos[2], "x").await;
    assert_eq!(deliveries(&rx).len(), 6);
    w.apps.delete_app(&cx, &n.app.id).await.unwrap();
    assert_eq!(
        w.apps
            .get_installation(&cx, &all.id)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    lease_and_flush(&w, &w.repos[2], "y").await;
    assert_eq!(
        deliveries(&rx).len(),
        6,
        "deleting the app removes its hooks"
    );
}
on_backends!(event_subscriber_app_backends, event_subscriber_app);

// ---------- OAuth provider ----------

const CB: &str = "https://cli.example/cb";

async fn oauth_app(w: &World, device: bool) -> (OAuthApp, String) {
    w.apps
        .create_oauth_app(
            &owner_cx(),
            &acme(w),
            NewOAuthApp {
                name: "CLI".into(),
                description: "a command line".into(),
                homepage_url: "https://cli.example".into(),
                callback_urls: vec![CB.into()],
                device_flow_enabled: device,
            },
        )
        .await
        .unwrap()
}

fn ask(client_id: &str, scopes: &[&str], challenge: &str) -> AuthorizationRequest {
    AuthorizationRequest {
        client_id: client_id.into(),
        redirect_uri: CB.into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        state: "xyz".into(),
        code_challenge: challenge.into(),
        code_challenge_method: if challenge.is_empty() { "" } else { "S256" }.into(),
        ..Default::default()
    }
}

async fn oauth_code_flow(w: World) {
    let (app, secret) = oauth_app(&w, false).await;
    let user = support::cx("writer");
    let apps = &w.apps;
    let ucx = &user;
    let verifier = "v".repeat(60);
    let challenge = pkce_challenge(&verifier);
    let exchange = |code: &str, secret: &str, redirect: &str, verifier: &str| {
        let apps = apps.clone();
        let (cid, code, secret, redirect, verifier) = (
            app.client_id.clone(),
            code.to_string(),
            secret.to_string(),
            redirect.to_string(),
            verifier.to_string(),
        );
        async move {
            apps.exchange_code(&Cx::system(), &cid, &secret, &code, &redirect, &verifier)
                .await
        }
    };

    // The redirect URI must match exactly; PKCE needs S256; scopes must exist.
    for bad in [
        format!("{CB}/"),
        "https://evil.example/cb".to_string(),
        String::new(),
    ] {
        let mut r = ask(&app.client_id, &["repo"], &challenge);
        r.redirect_uri = bad;
        assert_eq!(
            apps.create_authorization(ucx, r).await.unwrap_err().subcode,
            "redirect_uri_mismatch"
        );
    }
    let mut plain = ask(&app.client_id, &["repo"], &challenge);
    plain.code_challenge_method = "plain".into();
    assert_eq!(
        apps.create_authorization(ucx, plain)
            .await
            .unwrap_err()
            .subcode,
        "bad_code_challenge"
    );
    assert_eq!(
        apps.create_authorization(ucx, ask(&app.client_id, &["root"], &challenge))
            .await
            .unwrap_err()
            .subcode,
        "unknown_scope"
    );
    assert_eq!(
        apps.create_authorization(ucx, ask("Ov1.nope", &["repo"], &challenge))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::Unauthenticated
    );

    // Denying redirects with access_denied and never issues a code.
    let mut deny = ask(&app.client_id, &["repo"], &challenge);
    deny.deny = true;
    let d = apps.create_authorization(ucx, deny).await.unwrap();
    assert_eq!(
        d.redirect_url,
        format!("{CB}?error=access_denied&state=xyz")
    );
    assert!(d.code.is_empty());
    assert!(
        apps.list_oauth_grants(ucx, "writer", "", 10)
            .await
            .unwrap()
            .0
            .is_empty()
    );

    // A wrong PKCE verifier fails and burns the code.
    let a = apps
        .create_authorization(ucx, ask(&app.client_id, &["repo"], &challenge))
        .await
        .unwrap();
    assert_eq!(a.redirect_url, format!("{CB}?code={}&state=xyz", a.code));
    assert_eq!(a.code_expires_ms, w.clock.now_ms() + 600_000);
    let e = exchange(&a.code, "", CB, &"w".repeat(60))
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "invalid_grant");
    assert_eq!(
        exchange(&a.code, "", CB, &verifier)
            .await
            .unwrap_err()
            .subcode,
        "invalid_grant"
    );

    // A different redirect_uri at the exchange fails too.
    let a = apps
        .create_authorization(ucx, ask(&app.client_id, &["repo"], &challenge))
        .await
        .unwrap();
    assert_eq!(
        exchange(&a.code, "", "https://cli.example/other", &verifier)
            .await
            .unwrap_err()
            .subcode,
        "invalid_grant"
    );

    // The right verifier works (a public client: no secret).
    let t = exchange(&a.code, "", CB, &verifier).await.unwrap();
    assert_eq!(t.token_type, "bearer");
    assert_eq!(
        t.client_kind,
        OAuthClientKind::OauthClientKindOauthApp as i32
    );
    assert_eq!(t.scopes, vec!["repo".to_string()]);
    assert!(t.refresh_token.starts_with("rt_"));
    let vt = verify(&w, &t.access_token).unwrap();
    assert_eq!(vt.principal.id, "writer");
    let now = now_secs(&w);
    for act in [Action::Read, Action::Commit, Action::Land] {
        vt.authorize(&scope_req(&w.repos[0], act), now).unwrap();
    }
    assert!(
        vt.authorize(&scope_req(&w.repos[0], Action::ManageSettings), now)
            .is_err()
    );
    // The token acts as the user: the scope allows Commit, the user's own role (a plain
    // member) does not.
    let caller = caller_of(&w, &t.access_token);
    w.env
        .forge
        .authz
        .repo(&caller, &w.repos[0], Action::Read, &[], None, None)
        .await
        .unwrap();
    assert_eq!(
        w.env
            .forge
            .authz
            .repo(&caller, &w.repos[0], Action::Commit, &[], None, None)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied
    );

    // Replaying the code revokes what it produced.
    let e = exchange(&a.code, "", CB, &verifier).await.unwrap_err();
    assert_eq!(e.subcode, "invalid_grant");
    assert_eq!(
        verify(&w, &t.access_token).unwrap_err().reason,
        bee_auth::AuthReason::TokenRevoked
    );
    let e = apps
        .refresh_token(&Cx::system(), &app.client_id, "", &t.refresh_token)
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "invalid_grant");

    // Confidential clients: a secret is required without PKCE, and a wrong one is refused.
    let a = apps
        .create_authorization(ucx, ask(&app.client_id, &["repo:read"], ""))
        .await
        .unwrap();
    assert_eq!(
        exchange(&a.code, "", CB, "").await.unwrap_err().reason,
        ErrorReason::Unauthenticated
    );
    assert_eq!(
        exchange(&a.code, "nope", CB, "").await.unwrap_err().subcode,
        "invalid_client"
    );
    let t1 = exchange(&a.code, &secret, CB, "").await.unwrap();
    assert_eq!(t1.scopes, vec!["repo:read".to_string()]);
    verify(&w, &t1.access_token).unwrap();

    // Codes expire after ten minutes.
    let a = apps
        .create_authorization(ucx, ask(&app.client_id, &["repo"], &challenge))
        .await
        .unwrap();
    w.clock.advance(601_000);
    assert_eq!(
        exchange(&a.code, "", CB, &verifier)
            .await
            .unwrap_err()
            .subcode,
        "invalid_grant"
    );

    // Refresh tokens rotate on use, and reusing a spent one revokes the whole family.
    let t1 = {
        let a = apps
            .create_authorization(ucx, ask(&app.client_id, &["repo"], ""))
            .await
            .unwrap();
        exchange(&a.code, &secret, CB, "").await.unwrap()
    };
    assert_eq!(
        apps.refresh_token(&Cx::system(), &app.client_id, "", &t1.refresh_token)
            .await
            .unwrap_err()
            .subcode,
        "invalid_client",
        "a confidential client must present its secret"
    );
    let t2 = apps
        .refresh_token(&Cx::system(), &app.client_id, &secret, &t1.refresh_token)
        .await
        .unwrap();
    assert_ne!(t2.token_id, t1.token_id);
    assert_ne!(t2.refresh_token, t1.refresh_token);
    verify(&w, &t2.access_token).unwrap();
    let reuse = apps
        .refresh_token(&Cx::system(), &app.client_id, &secret, &t1.refresh_token)
        .await
        .unwrap_err();
    assert_eq!(reuse.reason, ErrorReason::TokenRevoked);
    assert_eq!(reuse.subcode, "refresh_token_reused");
    for t in [&t1.access_token, &t2.access_token] {
        assert_eq!(
            verify(&w, t).unwrap_err().reason,
            bee_auth::AuthReason::TokenRevoked
        );
    }
    assert_eq!(
        apps.refresh_token(&Cx::system(), &app.client_id, &secret, &t2.refresh_token)
            .await
            .unwrap_err()
            .subcode,
        "invalid_grant"
    );

    // Grants: list, revoke one token (it cannot be refreshed any more), revoke the grant.
    let (grants, _) = apps.list_oauth_grants(ucx, "writer", "", 10).await.unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].client_id, app.client_id);
    assert_eq!(grants[0].client_name, "CLI");
    assert_eq!(grants[0].owner, "acme");
    assert_eq!(
        grants[0].client_kind,
        OAuthClientKind::OauthClientKindOauthApp as i32
    );
    let t3 = {
        let a = apps
            .create_authorization(ucx, ask(&app.client_id, &["repo"], ""))
            .await
            .unwrap();
        exchange(&a.code, &secret, CB, "").await.unwrap()
    };
    let t4 = {
        let a = apps
            .create_authorization(ucx, ask(&app.client_id, &["repo"], ""))
            .await
            .unwrap();
        exchange(&a.code, &secret, CB, "").await.unwrap()
    };
    apps.revoke_oauth_grant(ucx, "writer", RevokeTarget::Token(t3.token_id.clone()))
        .await
        .unwrap();
    assert_eq!(
        verify(&w, &t3.access_token).unwrap_err().reason,
        bee_auth::AuthReason::TokenRevoked
    );
    verify(&w, &t4.access_token).unwrap();
    assert!(
        apps.refresh_token(&Cx::system(), &app.client_id, &secret, &t3.refresh_token)
            .await
            .is_err()
    );
    assert_eq!(
        apps.revoke_oauth_grant(ucx, "writer", RevokeTarget::Token("nope".into()))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    apps.revoke_oauth_grant(ucx, "writer", RevokeTarget::Client(app.client_id.clone()))
        .await
        .unwrap();
    assert_eq!(
        verify(&w, &t4.access_token).unwrap_err().reason,
        bee_auth::AuthReason::TokenRevoked
    );
    assert!(
        apps.list_oauth_grants(ucx, "writer", "", 10)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert_eq!(
        apps.revoke_oauth_grant(ucx, "writer", RevokeTarget::Client(app.client_id.clone()))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );

    // Secret reset: the old secret stops working.
    let a = apps
        .create_authorization(ucx, ask(&app.client_id, &["repo"], ""))
        .await
        .unwrap();
    let (updated, fresh) = apps
        .update_oauth_app(
            &owner_cx(),
            &app.client_id,
            OAuthAppPatch {
                name: Some("CLI 2".into()),
                reset_client_secret: true,
                ..OAuthAppPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "CLI 2");
    let fresh = fresh.expect("a new secret");
    assert_eq!(
        exchange(&a.code, &secret, CB, "")
            .await
            .unwrap_err()
            .subcode,
        "invalid_client"
    );
    let t5 = exchange(&a.code, &fresh, CB, "").await.unwrap();
    let (same, none) = apps
        .update_oauth_app(&owner_cx(), &app.id, OAuthAppPatch::default())
        .await
        .unwrap();
    assert_eq!(same.name, "CLI 2");
    assert!(none.is_none());

    // Deleting the OAuth app revokes every token it holds.
    apps.delete_oauth_app(&owner_cx(), &app.client_id)
        .await
        .unwrap();
    assert_eq!(
        verify(&w, &t5.access_token).unwrap_err().reason,
        bee_auth::AuthReason::TokenRevoked
    );
    assert_eq!(
        apps.get_oauth_app(&owner_cx(), &app.client_id)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    let actions = audit_actions(&w).await;
    assert!(actions.iter().any(|a| a == "oauth.app_create"));
}
on_backends!(oauth_code_flow_backends, oauth_code_flow);

async fn oauth_device_flow(w: World) {
    let (app, secret) = oauth_app(&w, false).await;
    let apps = &w.apps;
    let sys = Cx::system();
    // The flag must be on.
    assert_eq!(
        apps.start_device_flow(&sys, &app.client_id, &[])
            .await
            .unwrap_err()
            .subcode,
        "device_flow_disabled"
    );
    apps.update_oauth_app(
        &owner_cx(),
        &app.client_id,
        OAuthAppPatch {
            device_flow_enabled: Some(true),
            ..OAuthAppPatch::default()
        },
    )
    .await
    .unwrap();
    let start = apps
        .start_device_flow(&sys, &app.client_id, &["repo".into()])
        .await
        .unwrap();
    let shape = start.user_code.as_bytes();
    assert_eq!(start.user_code.len(), 9, "{}", start.user_code);
    assert_eq!(shape[4], b'-');
    assert!(
        start
            .user_code
            .chars()
            .filter(|c| *c != '-')
            .all(|c| c.is_ascii_uppercase())
    );
    assert_eq!(start.interval_secs, 5);
    assert_eq!(start.expires_ms, w.clock.now_ms() + 15 * 60 * 1000);
    assert!(start.verification_uri_complete.contains(&start.user_code));
    let poll = |code: &str| {
        let (apps, cid, code) = (apps.clone(), app.client_id.clone(), code.to_string());
        async move {
            apps.poll_device_flow(&Cx::system(), &cid, &code)
                .await
                .unwrap()
        }
    };
    use bee_proto::v1::OAuthDeviceFlowStatus as S;

    // Pending, then SLOW_DOWN when polled before the interval is up (and the interval grows).
    let p = poll(&start.device_code).await;
    assert_eq!(p.status, S::OauthDeviceFlowStatusPending);
    assert_eq!(p.interval_secs, 5);
    w.clock.advance(2000);
    let p = poll(&start.device_code).await;
    assert_eq!(p.status, S::OauthDeviceFlowStatusSlowDown);
    assert_eq!(p.interval_secs, 10);
    w.clock.advance(6000);
    assert_eq!(
        poll(&start.device_code).await.status,
        S::OauthDeviceFlowStatusSlowDown
    );
    w.clock.advance(16_000);
    assert_eq!(
        poll(&start.device_code).await.status,
        S::OauthDeviceFlowStatusPending
    );

    // Someone else's client id cannot poll it.
    let (other, _) = apps
        .create_oauth_app(
            &owner_cx(),
            &acme(&w),
            NewOAuthApp {
                name: "Other".into(),
                device_flow_enabled: true,
                ..NewOAuthApp::default()
            },
        )
        .await
        .unwrap();
    assert!(
        apps.poll_device_flow(&sys, &other.client_id, &start.device_code)
            .await
            .is_err()
    );

    // Approve with a typed (lower-case, hyphen-less) code.
    let typed = start.user_code.replace('-', "").to_lowercase();
    let approve = |code: &str, deny: bool| {
        let (apps, code) = (apps.clone(), code.to_string());
        async move {
            apps.create_authorization(
                &support::cx("writer"),
                AuthorizationRequest {
                    user_code: code,
                    deny,
                    ..Default::default()
                },
            )
            .await
        }
    };
    assert_eq!(
        approve("ZZZZ-ZZZZ", false).await.unwrap_err().reason,
        ErrorReason::NotFound
    );
    approve(&typed, false).await.unwrap();
    assert_eq!(
        approve(&typed, false).await.unwrap_err().reason,
        ErrorReason::InvalidState
    );
    w.clock.advance(6000);
    let p = poll(&start.device_code).await;
    assert_eq!(p.status, S::OauthDeviceFlowStatusApproved);
    let t = p.tokens.expect("tokens");
    assert_eq!(verify(&w, &t.access_token).unwrap().principal.id, "writer");
    assert_eq!(t.scopes, vec!["repo".to_string()]);
    // Tokens are handed out once.
    assert!(
        apps.poll_device_flow(&sys, &app.client_id, &start.device_code)
            .await
            .is_err()
    );
    // A device-flow refresh token works without the client secret.
    let r = apps
        .refresh_token(&sys, &app.client_id, "", &t.refresh_token)
        .await
        .unwrap();
    verify(&w, &r.access_token).unwrap();
    let _ = secret;

    // Denied.
    let s2 = apps
        .start_device_flow(&sys, &app.client_id, &[])
        .await
        .unwrap();
    approve(&s2.user_code, true).await.unwrap();
    w.clock.advance(6000);
    let p = poll(&s2.device_code).await;
    assert_eq!(p.status, S::OauthDeviceFlowStatusDenied);
    assert!(p.tokens.is_none());

    // Expired after fifteen minutes.
    let s3 = apps
        .start_device_flow(&sys, &app.client_id, &[])
        .await
        .unwrap();
    w.clock.advance(15 * 60 * 1000);
    assert_eq!(
        poll(&s3.device_code).await.status,
        S::OauthDeviceFlowStatusExpired
    );
    assert_eq!(
        approve(&s3.user_code, false).await.unwrap_err().subcode,
        "expired"
    );
    assert!(apps.sweep_expired(&sys).await.unwrap() > 0);
}
on_backends!(oauth_device_flow_backends, oauth_device_flow);

struct NeedsSso;

#[async_trait]
impl SsoGate for NeedsSso {
    async fn check_sso(&self, _cx: &Cx, principal_id: &str, org_ids: &[String]) -> FResult<()> {
        if principal_id == "writer" && !org_ids.is_empty() {
            Err(ForgeError::new(
                ErrorReason::SsoRequired,
                "sign in with SSO",
            ))
        } else {
            Ok(())
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oauth_authorization_can_require_sso() {
    let w = world_with(Backend::Memory, |c| c.with_sso(Arc::new(NeedsSso)))
        .await
        .unwrap();
    let (app, _) = oauth_app(&w, true).await;
    let writer = support::cx("writer");
    let e = w
        .apps
        .create_authorization(&writer, ask(&app.client_id, &["repo"], ""))
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::SsoRequired);
    // A user outside the SSO org is fine, and a denial needs no SSO session.
    w.apps
        .create_authorization(&support::cx("outsider"), ask(&app.client_id, &["repo"], ""))
        .await
        .unwrap();
    let mut deny = ask(&app.client_id, &["repo"], "");
    deny.deny = true;
    w.apps.create_authorization(&writer, deny).await.unwrap();
    // The device flow checks too.
    let s = w
        .apps
        .start_device_flow(&Cx::system(), &app.client_id, &[])
        .await
        .unwrap();
    let e = w
        .apps
        .create_authorization(
            &writer,
            AuthorizationRequest {
                user_code: s.user_code,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::SsoRequired);
}

// ---------- user-to-server tokens ----------

async fn user_to_server(w: World) {
    let cx = owner_cx();
    let n = new_app(&w, "Acts For Users", true, &[("contents", "write")]).await;
    let (r1, r2, r3) = (&w.repos[0], &w.repos[1], &w.repos[2]);
    // Installed on two repos of acme, and on everything in another org the user is not in.
    install_selected(&w, &n, &[r1, r2]).await;
    let outsider = w.env.forge.hooks.directory.get("outsider").await.unwrap();
    let beta = w
        .env
        .forge
        .orgs
        .create_org(&support::cx("outsider"), "beta", "", outsider)
        .await
        .unwrap();
    let b1 = support::make_repo(&w.env, &beta, "b1").await;
    let beta_inst = w
        .apps
        .create_installation(
            &support::cx("outsider"),
            &n.app.id,
            &Account::org(&beta.id, &beta.name),
            AppRepositorySelection::All,
            &[],
        )
        .await
        .unwrap();
    // The user: a plain member of acme, with write on r1 only.
    let writer = w.env.forge.hooks.directory.get("writer").await.unwrap();
    w.env
        .forge
        .orgs
        .add_collaborator(&cx, &r1.id, writer, RepoRole::Write, "")
        .await
        .unwrap();

    let user = support::cx("writer");
    let callback = "https://example.com/callback";
    let verifier = "u".repeat(50);
    let a = w
        .apps
        .create_authorization(
            &user,
            AuthorizationRequest {
                client_id: n.app.client_id.clone(),
                redirect_uri: callback.into(),
                code_challenge: pkce_challenge(&verifier),
                code_challenge_method: "S256".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let t = w
        .apps
        .exchange_code(
            &Cx::system(),
            &n.app.client_id,
            "",
            &a.code,
            callback,
            &verifier,
        )
        .await
        .unwrap();
    assert_eq!(t.client_kind, OAuthClientKind::OauthClientKindApp as i32);
    assert!(
        t.scopes.contains(&"contents:write".to_string()),
        "{:?}",
        t.scopes
    );

    // The token is the app acting for the user: chain [app principal, user].
    let vt = verify(&w, &t.access_token).unwrap();
    assert_eq!(vt.principal.id, n.app.principal_id);
    assert_eq!(
        vt.on_behalf_of
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec!["writer"]
    );
    // Level 1: its repos are exactly those of the installations on orgs the user belongs to.
    let now = now_secs(&w);
    for r in [r1, r2] {
        vt.authorize(&scope_req(r, Action::Commit), now).unwrap();
    }
    for r in [r3, &b1] {
        assert!(vt.authorize(&scope_req(r, Action::Read), now).is_err());
    }
    // Level 2: both principals of the chain must hold a role, so it is an intersection.
    let caller = caller_of(&w, &t.access_token);
    let authz = &w.env.forge.authz;
    authz
        .repo(&caller, r1, Action::Commit, &[], None, None)
        .await
        .unwrap(); // app write ∩ user write
    authz
        .repo(&caller, r2, Action::Read, &[], None, None)
        .await
        .unwrap();
    assert_eq!(
        authz
            .repo(&caller, r2, Action::Commit, &[], None, None)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied,
        "the user can only read r2"
    );
    for r in [r3, &b1] {
        assert!(
            authz
                .repo(&caller, r, Action::Read, &[], None, None)
                .await
                .is_err()
        );
    }
    // Level 3: the access sources. The app principal holds write on its installed repos,
    // including beta's; the user holds nothing in beta, and the token's repo list is what
    // keeps the app out of it.
    let role = |r: &Repo, p: &str| {
        let orgs = w.env.forge.orgs.clone();
        let (r, p) = (r.clone(), p.to_string());
        async move {
            orgs.effective_role(&Cx::system(), &r, &p)
                .await
                .unwrap()
                .role
        }
    };
    assert_eq!(role(r1, &n.app.principal_id).await, RepoRole::Write as i32);
    assert_eq!(role(r3, &n.app.principal_id).await, RepoRole::None as i32);
    assert_eq!(role(&b1, &n.app.principal_id).await, RepoRole::Write as i32);
    assert_eq!(role(&b1, "writer").await, RepoRole::None as i32);

    // Refresh recomputes: suspending the acme installation leaves the app nothing for this user.
    let acme_inst = w
        .apps
        .list_installations_of_account(&cx, &acme(&w), "", 10)
        .await
        .unwrap()
        .0
        .remove(0);
    w.apps
        .suspend_installation(&cx, &acme_inst.id, "")
        .await
        .unwrap();
    assert_eq!(role(r1, &n.app.principal_id).await, RepoRole::None as i32);
    let t2 = w
        .apps
        .refresh_token(
            &Cx::system(),
            &n.app.client_id,
            &n.client_secret,
            &t.refresh_token,
        )
        .await
        .unwrap();
    let v2 = verify(&w, &t2.access_token).unwrap();
    assert!(v2.authorize(&scope_req(r1, Action::Read), now).is_err());
    assert!(v2.authorize(&scope_req(&b1, Action::Read), now).is_err());
    // Deleting the app revokes its user tokens and grants.
    w.apps.delete_app(&cx, &n.app.id).await.unwrap();
    assert_eq!(
        verify(&w, &t2.access_token).unwrap_err().reason,
        bee_auth::AuthReason::TokenRevoked
    );
    assert!(
        w.apps
            .list_oauth_grants(&user, "writer", "", 10)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    let _ = beta_inst;
}
on_backends!(user_to_server_backends, user_to_server);

// ---------- marketplace ----------

struct Domains {
    verified: Mutex<Vec<String>>,
    two_factor: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl DomainCheck for Domains {
    async fn is_verified(&self, _cx: &Cx, _org: &str, domain: &str) -> FResult<bool> {
        Ok(self.verified.lock().unwrap().iter().any(|d| d == domain))
    }

    async fn two_factor_required(&self, _cx: &Cx, _org: &str) -> FResult<bool> {
        Ok(self.two_factor.load(Ordering::SeqCst))
    }
}

struct Gate {
    limit: AtomicU64,
    calls: AtomicU64,
}

#[async_trait]
impl SpendGate for Gate {
    async fn check_spend(
        &self,
        _cx: &Cx,
        _org_id: &str,
        _repo_id: &str,
        product: BillingProduct,
        add_micro_usd: u64,
    ) -> FResult<()> {
        assert_eq!(product, BillingProduct::Marketplace);
        self.calls.fetch_add(1, Ordering::SeqCst);
        if add_micro_usd > self.limit.load(Ordering::SeqCst) {
            Err(ForgeError::new(
                ErrorReason::QuotaExceeded,
                "budget would be passed",
            ))
        } else {
            Ok(())
        }
    }
}

fn domains() -> Arc<Domains> {
    Arc::new(Domains {
        verified: Mutex::new(vec![]),
        two_factor: std::sync::atomic::AtomicBool::new(false),
    })
}

fn gate() -> Arc<Gate> {
    Arc::new(Gate {
        limit: AtomicU64::new(10_000_000),
        calls: AtomicU64::new(0),
    })
}

fn listing_in(ty: MarketplaceListingType, slug: &str, name: &str) -> ListingInput {
    ListingInput {
        listing_type: ty,
        slug: slug.into(),
        name: name.into(),
        short_description: format!("{name} short"),
        full_description: format!("{name} long text about triage"),
        categories: vec!["ci".into()],
        ..ListingInput::default()
    }
}

fn plan_in(name: &str, model: MarketplacePricingModel, monthly: u64, yearly: u64) -> PlanInput {
    PlanInput {
        name: name.into(),
        pricing_model: model,
        monthly_price_micro_usd: monthly,
        yearly_price_micro_usd: yearly,
        unit_name: if model == MarketplacePricingModel::PerUnit {
            "seat".into()
        } else {
            String::new()
        },
        ..PlanInput::default()
    }
}

fn slugs(v: &[bee_forge::enterprise::apps::ListingView]) -> Vec<String> {
    v.iter().map(|l| l.listing.slug.clone()).collect()
}

async fn marketplace_lifecycle(w: World, domains: Arc<Domains>, gate: Arc<Gate>) {
    use MarketplaceListingType as T;
    use MarketplacePricingModel as P;
    let cx = owner_cx();
    let org = acme(&w);
    let apps = &w.apps;
    let n = new_app(&w, "Market App", true, &[("contents", "read")]).await;

    // Validation.
    let mut bad = listing_in(T::App, "Bad Slug", "x");
    bad.app_id = n.app.id.clone();
    assert_eq!(
        apps.create_listing(&cx, &org, bad)
            .await
            .unwrap_err()
            .subcode,
        "invalid_slug"
    );
    assert_eq!(
        apps.create_listing(&cx, &org, listing_in(T::App, "no-app", "x"))
            .await
            .unwrap_err()
            .subcode,
        "missing_field"
    );
    let user_app = apps
        .create_app(
            &cx,
            &Account::user("owner", "owner-login"),
            &manifest("Personal", true, None, &[], &[]),
        )
        .await
        .unwrap();
    let mut foreign = listing_in(T::App, "foreign", "x");
    foreign.app_id = user_app.app.id.clone();
    assert_eq!(
        apps.create_listing(&cx, &org, foreign)
            .await
            .unwrap_err()
            .subcode,
        "app_not_owned"
    );
    let mut cats = listing_in(T::Agent, "cats", "x");
    cats.agent_id = "a".into();
    cats.categories = (0..6).map(|i| format!("c{i}")).collect();
    assert_eq!(
        apps.create_listing(&cx, &org, cats.clone())
            .await
            .unwrap_err()
            .subcode,
        "too_many_categories"
    );
    cats.categories = vec!["Not Valid!".into()];
    assert_eq!(
        apps.create_listing(&cx, &org, cats)
            .await
            .unwrap_err()
            .subcode,
        "invalid_category"
    );

    // Drafts of the three kinds.
    let mut app_l = listing_in(T::App, "market-app", "Market App");
    app_l.app_id = n.app.id.clone();
    let l_app = apps.create_listing(&cx, &org, app_l.clone()).await.unwrap();
    assert_eq!(l_app.listing.state, MarketplaceListingState::Draft as i32);
    assert_eq!(l_app.listing.publisher, "acme");
    assert_eq!(
        apps.create_listing(&cx, &org, app_l)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );
    let mut act = listing_in(T::Action, "ci-action", "CI Action");
    act.action_repo_id = w.repos[0].id.clone();
    let l_act = apps.create_listing(&cx, &org, act).await.unwrap();
    assert_eq!(l_act.listing.action_path, "action.yml");
    assert!(l_act.listing.action_repo.is_some());
    let mut ag = listing_in(T::Agent, "review-agent", "Review Agent");
    ag.agent_id = "agent-def-1".into();
    let l_agent = apps.create_listing(&cx, &org, ag).await.unwrap();

    // Plans: validation, then one free, one with a trial, per-seat, and a paid-only one.
    for (p, sub) in [
        (plan_in("f", P::Free, 5, 0), "bad_price"),
        (plan_in("f", P::Flat, 0, 0), "bad_price"),
        (plan_in("f", P::Unspecified, 0, 0), "missing_field"),
        (
            PlanInput {
                unit_name: String::new(),
                ..plan_in("u", P::PerUnit, 5, 0)
            },
            "missing_field",
        ),
        (
            PlanInput {
                free_trial_days: 91,
                ..plan_in("t", P::Flat, 5, 0)
            },
            "bad_trial",
        ),
    ] {
        assert_eq!(
            apps.create_plan(&cx, "market-app", p)
                .await
                .unwrap_err()
                .subcode,
            sub
        );
    }
    let free = apps
        .create_plan(&cx, "market-app", plan_in("Free", P::Free, 0, 0))
        .await
        .unwrap();
    let pro = apps
        .create_plan(
            &cx,
            &l_app.listing.id,
            PlanInput {
                free_trial_days: 14,
                bullets: vec!["fast".into()],
                ..plan_in("Pro", P::Flat, 5_000_000, 50_000_000)
            },
        )
        .await
        .unwrap();
    let seats = apps
        .create_plan(
            &cx,
            "market-app",
            plan_in("Seats", P::PerUnit, 2_000_000, 0),
        )
        .await
        .unwrap();
    let paid_only = apps
        .create_plan(&cx, "ci-action", plan_in("Paid", P::Flat, 1_000_000, 0))
        .await
        .unwrap();

    // Drafts are invisible to a search; the publisher sees them.
    let everyone = ListingFilter::default();
    assert!(
        apps.list_listings(&cx, &everyone, "", 50)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    let mine = ListingFilter {
        include_unpublished_of: w.org.id.clone(),
        ..ListingFilter::default()
    };
    assert_eq!(
        apps.list_listings(&cx, &mine, "", 50)
            .await
            .unwrap()
            .0
            .len(),
        3
    );
    // Purchases need a published listing.
    assert_eq!(
        apps.purchase_plan(
            &cx,
            &org,
            "market-app",
            PurchaseInput {
                plan_id: free.id.clone(),
                ..Default::default()
            }
        )
        .await
        .unwrap_err()
        .reason,
        ErrorReason::NotFound
    );

    // Publish (auto-approved), one second apart.
    for slug in ["market-app", "ci-action", "review-agent"] {
        w.clock.advance(1000);
        let p = apps.publish_listing(&cx, slug, false).await.unwrap();
        assert_eq!(p.listing.state, MarketplaceListingState::Published as i32);
        assert!(p.listing.published_at.is_some());
        assert_eq!(
            apps.publish_listing(&cx, slug, false)
                .await
                .unwrap_err()
                .reason,
            ErrorReason::InvalidState
        );
    }

    // Search: type, category, text, publisher, free.
    let list = |f: ListingFilter| {
        let apps = apps.clone();
        async move { apps.list_listings(&owner_cx(), &f, "", 50).await.unwrap().0 }
    };
    assert_eq!(list(everyone.clone()).await.len(), 3);
    assert_eq!(
        slugs(
            &list(ListingFilter {
                listing_type: T::Action,
                ..everyone.clone()
            })
            .await
        ),
        vec!["ci-action"]
    );
    assert_eq!(
        list(ListingFilter {
            category: "ci".into(),
            ..everyone.clone()
        })
        .await
        .len(),
        3
    );
    assert!(
        list(ListingFilter {
            category: "nope".into(),
            ..everyone.clone()
        })
        .await
        .is_empty()
    );
    assert_eq!(
        list(ListingFilter {
            query: "TRIAGE".into(),
            ..everyone.clone()
        })
        .await
        .len(),
        3
    );
    assert_eq!(
        slugs(
            &list(ListingFilter {
                query: "market".into(),
                ..everyone.clone()
            })
            .await
        ),
        vec!["market-app"]
    );
    assert_eq!(
        slugs(
            &list(ListingFilter {
                query: "review agent".into(),
                ..everyone.clone()
            })
            .await
        ),
        vec!["review-agent"]
    );
    assert!(
        list(ListingFilter {
            query: "zzz".into(),
            ..everyone.clone()
        })
        .await
        .is_empty()
    );
    assert_eq!(
        list(ListingFilter {
            publisher_org_id: w.org.id.clone(),
            ..everyone.clone()
        })
        .await
        .len(),
        3
    );
    assert!(
        list(ListingFilter {
            publisher_org_id: "nobody".into(),
            ..everyone.clone()
        })
        .await
        .is_empty()
    );
    let free_only = list(ListingFilter {
        free_only: true,
        ..everyone.clone()
    })
    .await;
    assert_eq!(
        slugs(&free_only),
        vec!["market-app", "review-agent"],
        "the paid-only action is excluded"
    );
    assert!(
        list(ListingFilter {
            verified_only: true,
            ..everyone.clone()
        })
        .await
        .is_empty()
    );
    // Sorting and paging.
    let by = |sort: &str| ListingFilter {
        sort: sort.into(),
        ..everyone.clone()
    };
    assert_eq!(
        slugs(&list(by("name")).await),
        vec!["ci-action", "market-app", "review-agent"]
    );
    assert_eq!(
        slugs(&list(by("recent")).await),
        vec!["review-agent", "ci-action", "market-app"]
    );
    assert_eq!(
        apps.list_listings(&cx, &by("bogus"), "", 10)
            .await
            .unwrap_err()
            .subcode,
        "bad_sort"
    );
    let (p1, next) = apps.list_listings(&cx, &by("name"), "", 2).await.unwrap();
    assert_eq!(p1.len(), 2);
    assert!(!next.is_empty());
    let (p2, end) = apps
        .list_listings(&cx, &by("name"), &next, 2)
        .await
        .unwrap();
    assert_eq!(slugs(&p2), vec!["review-agent"]);
    assert!(end.is_empty());

    // Purchases. A free plan has no billing date.
    let buy = |buyer: Account,
               who: Cx,
               listing: &str,
               plan: &str,
               units: u32,
               cycle: MarketplaceBillingCycle| {
        let apps = apps.clone();
        let (listing, plan) = (listing.to_string(), plan.to_string());
        async move {
            apps.purchase_plan(
                &who,
                &buyer,
                &listing,
                PurchaseInput {
                    plan_id: plan,
                    unit_count: units,
                    billing_cycle: cycle,
                },
            )
            .await
        }
    };
    use MarketplaceBillingCycle as C;
    let p_free = buy(
        org.clone(),
        cx.clone(),
        "market-app",
        &free.id,
        0,
        C::Unspecified,
    )
    .await
    .unwrap();
    assert_eq!(p_free.org, "acme");
    assert!(p_free.next_billing_at.is_none() && !p_free.on_free_trial);
    // A paid plan runs through the spend gate; a hard stop is QUOTA_EXCEEDED.
    gate.limit.store(500_000, Ordering::SeqCst);
    assert_eq!(
        buy(
            org.clone(),
            cx.clone(),
            "ci-action",
            &paid_only.id,
            0,
            C::Monthly
        )
        .await
        .unwrap_err()
        .reason,
        ErrorReason::QuotaExceeded
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    gate.limit.store(10_000_000, Ordering::SeqCst);
    let now = w.clock.now_ms();
    let p_act = buy(
        org.clone(),
        cx.clone(),
        "ci-action",
        &paid_only.id,
        0,
        C::Unspecified,
    )
    .await
    .unwrap();
    assert_eq!(p_act.billing_cycle, C::Monthly as i32);
    assert_eq!(
        p_act.next_billing_at.map(|t| bee_forge::ids::ts_ms(&t)),
        Some(now + 30 * 86_400_000)
    );
    assert_eq!(p_act.unit_count, 1);
    // Per-unit plans need units; a cycle the plan does not price is refused.
    assert_eq!(
        buy(
            org.clone(),
            cx.clone(),
            "market-app",
            &seats.id,
            0,
            C::Monthly
        )
        .await
        .unwrap_err()
        .subcode,
        "invalid_units"
    );
    assert_eq!(
        buy(
            org.clone(),
            cx.clone(),
            "market-app",
            &seats.id,
            3,
            C::Yearly
        )
        .await
        .unwrap_err()
        .subcode,
        "cycle_not_offered"
    );
    // Buying again on the same listing changes the plan (same purchase, billing starts).
    let p_seats = buy(
        org.clone(),
        cx.clone(),
        "market-app",
        &seats.id,
        3,
        C::Monthly,
    )
    .await
    .unwrap();
    assert_eq!(p_seats.id, p_free.id);
    assert_eq!(p_seats.plan_id, seats.id);
    assert_eq!(p_seats.unit_count, 3);
    assert!(p_seats.next_billing_at.is_some());
    // A first purchase with a trial bills at the end of the trial and skips the gate.
    let outsider = w.env.forge.hooks.directory.get("outsider").await.unwrap();
    let beta = w
        .env
        .forge
        .orgs
        .create_org(&support::cx("outsider"), "beta", "", outsider)
        .await
        .unwrap();
    let beta_acct = Account::org(&beta.id, &beta.name);
    let calls = gate.calls.load(Ordering::SeqCst);
    let now = w.clock.now_ms();
    let p_trial = buy(
        beta_acct.clone(),
        support::cx("outsider"),
        "market-app",
        &pro.id,
        0,
        C::Monthly,
    )
    .await
    .unwrap();
    assert!(p_trial.on_free_trial);
    let trial_end = now + 14 * 86_400_000;
    assert_eq!(
        p_trial
            .free_trial_ends_at
            .map(|t| bee_forge::ids::ts_ms(&t)),
        Some(trial_end)
    );
    assert_eq!(
        p_trial.next_billing_at.map(|t| bee_forge::ids::ts_ms(&t)),
        Some(trial_end)
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), calls);

    // install_count: purchases (plan changes do not count) plus the app's installations.
    assert_eq!(
        apps.get_listing(&cx, "market-app")
            .await
            .unwrap()
            .listing
            .install_count,
        2
    );
    install_selected(&w, &n, &[&w.repos[0]]).await;
    assert_eq!(
        apps.get_listing(&cx, "market-app")
            .await
            .unwrap()
            .listing
            .install_count,
        3
    );
    assert_eq!(
        slugs(&list(by("popularity")).await),
        vec!["market-app", "ci-action", "review-agent"]
    );

    // A plan with purchases can only be retired.
    let e = apps
        .update_plan(
            &cx,
            "ci-action",
            &paid_only.id,
            PlanPatch {
                monthly_price_micro_usd: Some(2_000_000),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.reason, ErrorReason::InvalidState);
    apps.update_plan(
        &cx,
        "ci-action",
        &paid_only.id,
        PlanPatch {
            monthly_price_micro_usd: Some(1_000_000),
            name: Some("Paid plan".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let unused = apps
        .create_plan(&cx, "ci-action", plan_in("Unused", P::Flat, 3_000_000, 0))
        .await
        .unwrap();
    let repriced = apps
        .update_plan(
            &cx,
            "ci-action",
            &unused.id,
            PlanPatch {
                monthly_price_micro_usd: Some(4_000_000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(repriced.monthly_price_micro_usd, 4_000_000);
    let retired = apps
        .update_plan(
            &cx,
            "ci-action",
            &paid_only.id,
            PlanPatch {
                retired: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(retired.retired);
    assert_eq!(
        buy(
            beta_acct.clone(),
            support::cx("outsider"),
            "ci-action",
            &paid_only.id,
            0,
            C::Monthly
        )
        .await
        .unwrap_err()
        .subcode,
        "plan_retired"
    );

    // Purchases: the buyer's and the publisher's views.
    let (mine, _) = apps
        .list_purchases_of_org(&cx, &w.org.id, false, "", 10)
        .await
        .unwrap();
    assert_eq!(mine.len(), 2);
    let (theirs, _) = apps
        .list_purchases_of_listing(&cx, "market-app", false, "", 10)
        .await
        .unwrap();
    let mut orgs: Vec<String> = theirs.iter().map(|p| p.org.clone()).collect();
    orgs.sort();
    assert_eq!(orgs, vec!["acme".to_string(), "beta".to_string()]);

    // Cancelling ends the purchase at the end of the cycle.
    let c = apps
        .cancel_purchase(&cx, &w.org.id, &p_act.id)
        .await
        .unwrap();
    assert!(c.pending_cancellation && c.cancelled_at.is_none());
    assert_eq!(
        apps.cancel_purchase(&cx, &w.org.id, &p_act.id)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert_eq!(
        apps.list_purchases_of_org(&cx, &w.org.id, false, "", 10)
            .await
            .unwrap()
            .0
            .len(),
        2
    );
    w.clock.advance(31 * 86_400_000);
    let (live, _) = apps
        .list_purchases_of_org(&cx, &w.org.id, false, "", 10)
        .await
        .unwrap();
    assert_eq!(live.len(), 1);
    let (all, _) = apps
        .list_purchases_of_org(&cx, &w.org.id, true, "", 10)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    let ended = all.iter().find(|p| p.id == p_act.id).unwrap();
    assert!(ended.cancelled_at.is_some() && !ended.pending_cancellation);
    assert_eq!(
        apps.cancel_purchase(&cx, &w.org.id, &p_act.id)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    // The trial is over by now.
    let (b, _) = apps
        .list_purchases_of_org(&cx, &beta.id, false, "", 10)
        .await
        .unwrap();
    assert!(!b[0].on_free_trial);
    // A free purchase cancels at once.
    let gone = apps
        .cancel_purchase(&cx, &w.org.id, &p_seats.id)
        .await
        .unwrap();
    assert!(gone.pending_cancellation || gone.cancelled_at.is_some());

    // Unpublishing hides the listing from search; purchases continue; it can come back.
    let un = apps.publish_listing(&cx, "market-app", true).await.unwrap();
    assert_eq!(
        un.listing.state,
        MarketplaceListingState::Unpublished as i32
    );
    assert_eq!(
        apps.publish_listing(&cx, "market-app", true)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert_eq!(
        slugs(&list(by("name")).await),
        vec!["ci-action", "review-agent"]
    );
    assert_eq!(
        apps.list_purchases_of_listing(&cx, "market-app", true, "", 10)
            .await
            .unwrap()
            .0
            .len(),
        2
    );
    assert_eq!(
        buy(
            beta_acct.clone(),
            support::cx("outsider"),
            "market-app",
            &pro.id,
            0,
            C::Monthly
        )
        .await
        .unwrap_err()
        .reason,
        ErrorReason::NotFound
    );
    assert_eq!(
        apps.publish_listing(&cx, "market-app", false)
            .await
            .unwrap()
            .listing
            .state,
        MarketplaceListingState::Published as i32
    );

    // Publisher verification needs a verified domain and enforced 2FA.
    let pb = apps.get_publisher(&cx, &org).await.unwrap();
    assert_eq!(
        pb.verification,
        MarketplacePublisherVerification::Unverified as i32
    );
    assert_eq!(pb.listing_count, 3);
    assert_eq!(
        apps.request_publisher_verification(&cx, &org, "acme.example", "help@acme.example")
            .await
            .unwrap_err()
            .subcode,
        "domain_not_verified"
    );
    domains.verified.lock().unwrap().push("acme.example".into());
    assert_eq!(
        apps.request_publisher_verification(&cx, &org, "acme.example", "help@acme.example")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::TwoFactorRequired
    );
    domains.two_factor.store(true, Ordering::SeqCst);
    assert_eq!(
        apps.request_publisher_verification(&cx, &org, "acme.example", "not an email")
            .await
            .unwrap_err()
            .subcode,
        "bad_email"
    );
    let v = apps
        .request_publisher_verification(&cx, &org, "ACME.example", "help@acme.example")
        .await
        .unwrap();
    assert_eq!(
        v.verification,
        MarketplacePublisherVerification::Verified as i32
    );
    assert_eq!(v.verified_domain, "acme.example");
    assert!(v.verified_at.is_some());
    assert_eq!(
        apps.request_publisher_verification(&cx, &org, "acme.example", "")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert!(
        apps.get_listing(&cx, "market-app")
            .await
            .unwrap()
            .listing
            .verified_publisher
    );
    assert_eq!(
        list(ListingFilter {
            verified_only: true,
            ..everyone.clone()
        })
        .await
        .len(),
        3
    );

    // Every change is an audit entry in the publisher's chain.
    let actions = audit_actions(&w).await;
    for a in [
        "marketplace.listing_create",
        "marketplace.plan_create",
        "marketplace.submit",
        "marketplace.unpublish",
        "marketplace.purchase",
        "marketplace.purchase_change",
        "marketplace.purchase_cancel",
        "marketplace.plan_update",
        "marketplace.publisher_verify_request",
    ] {
        assert!(actions.iter().any(|x| x == a), "{a} in {actions:?}");
    }
    let _ = l_agent;
}

macro_rules! market_tests {
    ($($name:ident => $backend:expr),*) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let (d, g) = (domains(), gate());
            let (d2, g2) = (d.clone(), g.clone());
            let Some(w) = world_with($backend, move |c| c.with_domains(d2).with_spend_gate(g2)).await else { return };
            marketplace_lifecycle(w, d, g).await;
        }
    )*};
}
market_tests!(
    marketplace_lifecycle_memory => Backend::Memory,
    marketplace_lifecycle_sqlite => Backend::Sqlite,
    marketplace_lifecycle_postgres => Backend::Postgres
);

async fn marketplace_review(w: World, domains: Arc<Domains>) {
    use MarketplaceListingState as S;
    let cx = owner_cx();
    let org = acme(&w);
    let apps = &w.apps;
    let mut ag = listing_in(MarketplaceListingType::Agent, "reviewed", "Reviewed");
    ag.agent_id = "def".into();
    apps.create_listing(&cx, &org, ag).await.unwrap();
    let state = |v: bee_forge::enterprise::apps::ListingView| v.listing.state;

    // Without auto-approval, submitting waits for a site admin.
    assert_eq!(
        state(apps.publish_listing(&cx, "reviewed", false).await.unwrap()),
        S::PendingReview as i32
    );
    assert_eq!(
        apps.publish_listing(&cx, "reviewed", false)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert_eq!(
        apps.publish_listing(&cx, "reviewed", true)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert!(
        apps.list_listings(&cx, &ListingFilter::default(), "", 10)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert_eq!(
        state(
            apps.review_listing(&cx, "reviewed", false, "needs a logo")
                .await
                .unwrap()
        ),
        S::Rejected as i32
    );
    assert_eq!(
        apps.review_listing(&cx, "reviewed", true, "")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    assert!(
        apps.list_listings(&cx, &ListingFilter::default(), "", 10)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    // A rejected listing can be fixed and submitted again.
    assert_eq!(
        state(apps.publish_listing(&cx, "reviewed", false).await.unwrap()),
        S::PendingReview as i32
    );
    let ok = apps
        .review_listing(&cx, "reviewed", true, "looks good")
        .await
        .unwrap();
    assert_eq!(ok.listing.state, S::Published as i32);
    assert!(ok.listing.published_at.is_some());
    assert_eq!(
        apps.list_listings(&cx, &ListingFilter::default(), "", 10)
            .await
            .unwrap()
            .0
            .len(),
        1
    );
    assert_eq!(
        state(apps.publish_listing(&cx, "reviewed", true).await.unwrap()),
        S::Unpublished as i32
    );
    assert_eq!(
        state(apps.publish_listing(&cx, "reviewed", false).await.unwrap()),
        S::PendingReview as i32
    );
    // Never-submitted listings cannot be reviewed.
    let mut other = listing_in(MarketplaceListingType::Agent, "other", "Other");
    other.agent_id = "d2".into();
    apps.create_listing(&cx, &org, other).await.unwrap();
    assert_eq!(
        apps.review_listing(&cx, "other", true, "")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );

    // Publisher verification waits for review too.
    domains.verified.lock().unwrap().push("acme.example".into());
    domains.two_factor.store(true, Ordering::SeqCst);
    let p = apps
        .request_publisher_verification(&cx, &org, "acme.example", "")
        .await
        .unwrap();
    assert_eq!(
        p.verification,
        MarketplacePublisherVerification::Pending as i32
    );
    assert_eq!(
        apps.review_publisher(&cx, &org, false)
            .await
            .unwrap()
            .verification,
        MarketplacePublisherVerification::Rejected as i32
    );
    assert_eq!(
        apps.review_publisher(&cx, &org, true)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    apps.request_publisher_verification(&cx, &org, "acme.example", "")
        .await
        .unwrap();
    assert_eq!(
        apps.review_publisher(&cx, &org, true)
            .await
            .unwrap()
            .verification,
        MarketplacePublisherVerification::Verified as i32
    );
}

macro_rules! review_tests {
    ($($name:ident => $backend:expr),*) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let d = domains();
            let d2 = d.clone();
            let Some(w) = world_with($backend, move |c| {
                c.with_domains(d2).with_marketplace(MarketplaceConfig { auto_approve: false })
            })
            .await
            else {
                return;
            };
            marketplace_review(w, d).await;
        }
    )*};
}
review_tests!(
    marketplace_review_memory => Backend::Memory,
    marketplace_review_sqlite => Backend::Sqlite,
    marketplace_review_postgres => Backend::Postgres
);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn marketplace_service_permissions() {
    let w = world(Backend::Memory).await.unwrap();
    let s = &w.svc;
    let tok = |who: &str| w.env.full(who);
    let n = new_app(&w, "Svc App", true, &[]).await;
    let create = |who: &str, slug: &str| {
        let r = req(
            &tok(who),
            CreateMarketplaceListingRequest {
                org: "acme".into(),
                r#type: MarketplaceListingType::App as i32,
                slug: slug.into(),
                name: "Svc App".into(),
                short_description: "short".into(),
                app_id: n.app.id.clone(),
                ..Default::default()
            },
        );
        async move { s.create_marketplace_listing(r).await }
    };
    assert_eq!(
        reason(create("writer", "svc-app").await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(create("outsider", "svc-app").await),
        "PERMISSION_DENIED"
    );
    let l = create("appmgr", "svc-app").await.unwrap().into_inner();
    assert_eq!(l.state, MarketplaceListingState::Draft as i32);
    // A draft is invisible to everyone but the publisher's managers.
    let get = |who: &str| {
        let r = req(
            &tok(who),
            GetMarketplaceListingRequest {
                listing: "svc-app".into(),
            },
        );
        async move { s.get_marketplace_listing(r).await }
    };
    get("admin").await.unwrap();
    assert_eq!(reason(get("writer").await), "NOT_FOUND");
    assert_eq!(reason(get("outsider").await), "NOT_FOUND");
    // Plans and publishing are the publisher's.
    let plan = |who: &str| {
        let r = req(
            &tok(who),
            CreateMarketplacePlanRequest {
                listing: "svc-app".into(),
                name: "Free".into(),
                pricing_model: MarketplacePricingModel::Free as i32,
                ..Default::default()
            },
        );
        async move { s.create_marketplace_plan(r).await }
    };
    assert_eq!(reason(plan("writer").await), "PERMISSION_DENIED");
    let free = plan("owner").await.unwrap().into_inner();
    let publish = |who: &str, unpublish: bool| {
        let r = req(
            &tok(who),
            PublishMarketplaceListingRequest {
                listing: "svc-app".into(),
                unpublish,
            },
        );
        async move { s.publish_marketplace_listing(r).await }
    };
    assert_eq!(reason(publish("reader", false).await), "PERMISSION_DENIED");
    let p = publish("admin", false).await.unwrap().into_inner();
    assert_eq!(p.state, MarketplaceListingState::Published as i32);
    assert_eq!(p.plans.len(), 1);
    assert_eq!(p.publisher, "acme");
    get("outsider").await.unwrap();
    // Searching shows published listings to anyone, and a publisher's drafts to its managers.
    let search = |who: &str, publisher: &str| {
        let r = req(
            &tok(who),
            ListMarketplaceListingsRequest {
                publisher: publisher.into(),
                ..Default::default()
            },
        );
        async move {
            s.list_marketplace_listings(r)
                .await
                .unwrap()
                .into_inner()
                .listings
        }
    };
    assert_eq!(search("outsider", "").await.len(), 1);
    // Purchasing: the buyer org's managers; billing is theirs to read.
    let purchase = |who: &str, org: &str| {
        let r = req(
            &tok(who),
            PurchaseMarketplacePlanRequest {
                org: org.into(),
                listing: "svc-app".into(),
                plan_id: free.id.clone(),
                ..Default::default()
            },
        );
        async move { s.purchase_marketplace_plan(r).await }
    };
    assert_eq!(
        reason(purchase("writer", "acme").await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(purchase("outsider", "acme").await),
        "PERMISSION_DENIED"
    );
    let bought = purchase("admin", "acme").await.unwrap().into_inner();
    assert_eq!(bought.org, "acme");
    let list_p = |who: &str| {
        let r = req(
            &tok(who),
            ListMarketplacePurchasesRequest {
                filter: Some(list_marketplace_purchases_request::Filter::Org(
                    "acme".into(),
                )),
                ..Default::default()
            },
        );
        async move { s.list_marketplace_purchases(r).await }
    };
    assert_eq!(
        list_p("owner").await.unwrap().into_inner().purchases.len(),
        1
    );
    assert_eq!(reason(list_p("writer").await), "PERMISSION_DENIED");
    let by_listing = |who: &str| {
        let r = req(
            &tok(who),
            ListMarketplacePurchasesRequest {
                filter: Some(list_marketplace_purchases_request::Filter::Listing(
                    "svc-app".into(),
                )),
                ..Default::default()
            },
        );
        async move { s.list_marketplace_purchases(r).await }
    };
    assert_eq!(
        by_listing("appmgr")
            .await
            .unwrap()
            .into_inner()
            .purchases
            .len(),
        1
    );
    assert_eq!(reason(by_listing("outsider").await), "PERMISSION_DENIED");
    let cancel = |who: &str| {
        let r = req(
            &tok(who),
            CancelMarketplacePurchaseRequest {
                org: "acme".into(),
                purchase_id: bought.id.clone(),
            },
        );
        async move { s.cancel_marketplace_purchase(r).await }
    };
    assert_eq!(reason(cancel("writer").await), "PERMISSION_DENIED");
    assert!(
        cancel("admin")
            .await
            .unwrap()
            .into_inner()
            .cancelled_at
            .is_some()
    );
    // Publisher profile and verification.
    let pubr = s
        .get_marketplace_publisher(req(
            &tok("outsider"),
            GetMarketplacePublisherRequest { org: "acme".into() },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        pubr.verification,
        MarketplacePublisherVerification::Unverified as i32
    );
    assert_eq!(pubr.listing_count, 1);
    let verify_req = |who: &str| {
        let r = req(
            &tok(who),
            RequestMarketplacePublisherVerificationRequest {
                org: "acme".into(),
                domain: "acme.example".into(),
                support_email: "help@acme.example".into(),
            },
        );
        async move { s.request_marketplace_publisher_verification(r).await }
    };
    assert_eq!(reason(verify_req("writer").await), "PERMISSION_DENIED");
    assert_eq!(subcode(verify_req("owner").await), "domain_not_verified");
    // Updating the listing is the publisher's too.
    let upd = |who: &str| {
        let r = req(
            &tok(who),
            UpdateMarketplaceListingRequest {
                listing: "svc-app".into(),
                short_description: Some("new".into()),
                ..Default::default()
            },
        );
        async move { s.update_marketplace_listing(r).await }
    };
    assert_eq!(reason(upd("writer").await), "PERMISSION_DENIED");
    assert_eq!(
        upd("owner").await.unwrap().into_inner().short_description,
        "new"
    );
    let planu = |who: &str| {
        let r = req(
            &tok(who),
            UpdateMarketplacePlanRequest {
                listing: "svc-app".into(),
                plan_id: free.id.clone(),
                retired: Some(true),
                ..Default::default()
            },
        );
        async move { s.update_marketplace_plan(r).await }
    };
    assert_eq!(reason(planu("reader").await), "PERMISSION_DENIED");
    assert!(planu("owner").await.unwrap().into_inner().retired);
}

// ---------- service: permission matrix ----------

struct DenyIp;

#[async_trait]
impl IpPolicy for DenyIp {
    async fn check_ip(&self, _cx: &Cx, _org: &str, ip: &str, app_token: bool) -> FResult<()> {
        assert!(app_token);
        if ip == "203.0.113.9" {
            Err(ForgeError::new(
                ErrorReason::IpNotAllowed,
                "address not allowed",
            ))
        } else {
            Ok(())
        }
    }
}

async fn audit_results(w: &World, action: &str) -> Vec<String> {
    w.env.forge.flush().await.unwrap();
    let (entries, _) = w
        .env
        .forge
        .audit
        .query(
            &owner_cx(),
            &bee_forge::audit::org_scope(&w.org.id),
            &AuditLogFilter::default(),
            true,
            "",
            500,
        )
        .await
        .unwrap();
    entries
        .into_iter()
        .filter(|e| e.action == action)
        .map(|e| e.result)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_permission_matrix() {
    let w = world_with(Backend::Memory, |c| c.with_ip_policy(Arc::new(DenyIp)))
        .await
        .unwrap();
    let s = &w.svc;
    let tok = |who: &str| w.env.full(who);
    let create = |who: &str, name: &str, public: bool| {
        let r = req(
            &tok(who),
            CreateAppFromManifestRequest {
                org: "acme".into(),
                manifest_json: manifest(name, public, None, &[("contents", "read")], &[]),
            },
        );
        async move { s.create_app_from_manifest(r).await }
    };

    // Who may create apps in the org: owners, admins and app managers.
    for (who, ok) in [
        ("owner", true),
        ("admin", true),
        ("appmgr", true),
        ("writer", false),
        ("reader", false),
        ("outsider", false),
    ] {
        let r = create(who, &format!("app {who}"), true).await;
        if ok {
            assert_eq!(r.unwrap().into_inner().app.unwrap().owner, "acme", "{who}");
        } else {
            assert_eq!(reason(r), "PERMISSION_DENIED", "{who}");
        }
    }
    // The denial of a state-changing call is audited.
    assert!(
        audit_results(&w, "app.create")
            .await
            .iter()
            .any(|r| r == "PERMISSION_DENIED"),
        "denials are audited"
    );
    // An unauthenticated call is refused.
    let anon = tonic::Request::new(GetAppRequest {
        app: "app-owner".into(),
    });
    assert_eq!(reason(s.get_app(anon).await), "UNAUTHENTICATED");

    // A private app is invisible to everyone but its owner's managers.
    let private = create("owner", "Secret Sauce", false)
        .await
        .unwrap()
        .into_inner();
    let slug = private.app.unwrap().slug;
    let get = |who: &str| {
        let r = req(&tok(who), GetAppRequest { app: slug.clone() });
        async move { s.get_app(r).await }
    };
    get("owner").await.unwrap();
    get("appmgr").await.unwrap();
    assert_eq!(reason(get("writer").await), "NOT_FOUND");
    assert_eq!(reason(get("outsider").await), "NOT_FOUND");
    // Listing: outsiders see public apps only.
    let list = |who: &str| {
        let r = req(
            &tok(who),
            ListAppsRequest {
                owner: "acme".into(),
                page: None,
            },
        );
        async move { s.list_apps(r).await.unwrap().into_inner().apps }
    };
    assert!(list("admin").await.iter().any(|a| !a.public));
    assert!(list("outsider").await.iter().all(|a| a.public));

    // Updates and deletes follow the same rule.
    let update = |who: &str| {
        let r = req(
            &tok(who),
            UpdateAppRequest {
                app: slug.clone(),
                description: Some("x".into()),
                ..Default::default()
            },
        );
        async move { s.update_app(r).await }
    };
    assert_eq!(reason(update("writer").await), "PERMISSION_DENIED");
    assert_eq!(reason(update("outsider").await), "PERMISSION_DENIED");
    update("appmgr").await.unwrap();

    // Installing: the target org's owners, admins and app managers; private apps only by their
    // own owner.
    let n = create("owner", "Installable", true)
        .await
        .unwrap()
        .into_inner();
    let app = n.app.unwrap();
    let install = |who: &str, app: &str, org: &str, repos: Vec<RepoRef>| {
        let r = req(
            &tok(who),
            CreateAppInstallationRequest {
                app: app.into(),
                org: org.into(),
                repository_selection: AppRepositorySelection::Selected as i32,
                repos,
            },
        );
        async move { s.create_app_installation(r).await }
    };
    let both = vec![rid(&w.repos[0]), rid(&w.repos[1])];
    assert_eq!(
        reason(install("writer", &app.slug, "acme", both.clone()).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(install("outsider", &app.slug, "acme", both.clone()).await),
        "PERMISSION_DENIED"
    );
    let inst = install("appmgr", &app.slug, "acme", both.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(inst.target, "acme");
    assert_eq!(
        inst.repository_selection,
        AppRepositorySelection::Selected as i32
    );
    let ids = s
        .list_app_installation_repos(req(
            &tok("admin"),
            ListAppInstallationReposRequest {
                installation_id: inst.id.clone(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .repos;
    assert_eq!(ids.len(), 2);
    // The private app cannot be installed by another org's owner (it is not even visible).
    let outsider = w.env.forge.hooks.directory.get("outsider").await.unwrap();
    w.env
        .forge
        .orgs
        .create_org(&support::cx("outsider"), "other", "", outsider)
        .await
        .unwrap();
    assert_eq!(
        reason(install("outsider", &slug, "other", vec![]).await),
        "NOT_FOUND"
    );
    let other_inst = {
        let r = req(
            &tok("outsider"),
            CreateAppInstallationRequest {
                app: app.slug.clone(),
                org: "other".into(),
                repository_selection: AppRepositorySelection::All as i32,
                repos: vec![],
            },
        );
        s.create_app_installation(r).await.unwrap().into_inner()
    };
    // Repos of another org are refused.
    let foreign = support::make_repo(
        &w.env,
        &w.env
            .forge
            .orgs
            .get_org(&owner_cx(), "other")
            .await
            .unwrap(),
        "o1",
    )
    .await;
    assert_eq!(
        subcode(install("admin", &app.slug, "acme", vec![rid(&foreign)]).await),
        "repo_not_in_account"
    );
    drop(other_inst);

    // Reading installations: org members, the app's owner and the app itself.
    let nn = {
        // The installable app in the store for JWT signing: create a second app we hold keys for.
        w.apps
            .create_app(
                &owner_cx(),
                &acme(&w),
                &manifest("Keyed", true, None, &[("contents", "write")], &[]),
            )
            .await
            .unwrap()
    };
    let keyed_inst = w
        .apps
        .create_installation(
            &owner_cx(),
            &nn.app.id,
            &acme(&w),
            AppRepositorySelection::Selected,
            &[w.repos[0].clone(), w.repos[1].clone()],
        )
        .await
        .unwrap();
    let get_inst = |bearer: &str, id: &str| {
        let r = req(
            bearer,
            GetAppInstallationRequest {
                installation_id: id.into(),
            },
        );
        async move { s.get_app_installation(r).await }
    };
    get_inst(&tok("writer"), &keyed_inst.id).await.unwrap();
    get_inst(&tok("owner"), &keyed_inst.id).await.unwrap();
    assert_eq!(
        reason(get_inst(&tok("outsider"), &keyed_inst.id).await),
        "PERMISSION_DENIED"
    );
    get_inst(&jwt(&w, &nn), &keyed_inst.id).await.unwrap();
    let stranger = new_app(&w, "Stranger", true, &[]).await;
    assert_eq!(
        reason(get_inst(&jwt(&w, &stranger), &keyed_inst.id).await),
        "NOT_FOUND"
    );
    let list_inst = |bearer: &str, filter: list_app_installations_request::Filter| {
        let r = req(
            bearer,
            ListAppInstallationsRequest {
                filter: Some(filter),
                page: None,
            },
        );
        async move { s.list_app_installations(r).await }
    };
    use list_app_installations_request::Filter;
    assert_eq!(
        list_inst(&tok("reader"), Filter::Account("acme".into()))
            .await
            .unwrap()
            .into_inner()
            .installations
            .len(),
        2
    );
    assert_eq!(
        reason(list_inst(&tok("outsider"), Filter::Account("acme".into())).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        list_inst(&jwt(&w, &nn), Filter::App(nn.app.slug.clone()))
            .await
            .unwrap()
            .into_inner()
            .installations
            .len(),
        1
    );
    assert_eq!(
        reason(list_inst(&jwt(&w, &stranger), Filter::App(nn.app.slug.clone())).await),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(list_inst(&tok("writer"), Filter::App(nn.app.slug.clone())).await),
        "PERMISSION_DENIED"
    );

    // Installation tokens: the app (JWT) or an org owner/admin/app manager; nobody else.
    let mint = |bearer: &str, id: &str, repos: Vec<RepoRef>| {
        let r = req(
            bearer,
            CreateInstallationTokenRequest {
                installation_id: id.into(),
                repos,
                permissions: vec![],
                ttl_seconds: 600,
            },
        );
        async move { s.create_installation_token(r).await }
    };
    let by_jwt = mint(&jwt(&w, &nn), &keyed_inst.id, vec![rid(&w.repos[0])])
        .await
        .unwrap()
        .into_inner();
    assert_eq!(by_jwt.repos.len(), 1);
    let vt = verify(&w, &by_jwt.token).unwrap();
    assert_eq!(vt.principal.id, keyed_inst.principal_id);
    vt.authorize(&scope_req(&w.repos[0], Action::Commit), now_secs(&w))
        .unwrap();
    assert!(
        vt.authorize(&scope_req(&w.repos[1], Action::Commit), now_secs(&w))
            .is_err()
    );
    mint(&tok("admin"), &keyed_inst.id, vec![]).await.unwrap();
    mint(&tok("appmgr"), &keyed_inst.id, vec![]).await.unwrap();
    assert_eq!(
        reason(mint(&tok("writer"), &keyed_inst.id, vec![]).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(mint(&tok("outsider"), &keyed_inst.id, vec![]).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(mint(&jwt(&w, &stranger), &keyed_inst.id, vec![]).await),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(mint(&jwt(&w, &nn), &keyed_inst.id, vec![rid(&w.repos[2])]).await),
        "PERMISSION_DENIED",
        "a third repo is refused"
    );
    // A JWT lifetime above ten minutes is refused; an installation token is not a JWT and
    // cannot mint more tokens or manage apps.
    let long = sign_app_jwt(
        &nn.private_key_pem,
        &nn.private_key_id,
        &nn.app.client_id,
        now_secs(&w),
        now_secs(&w) + 900,
    )
    .unwrap();
    assert_eq!(
        reason(mint(&long, &keyed_inst.id, vec![]).await),
        "UNAUTHENTICATED"
    );
    assert_eq!(
        reason(mint(&by_jwt.token, &keyed_inst.id, vec![]).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(
            s.create_app_from_manifest(req(
                &by_jwt.token,
                CreateAppFromManifestRequest {
                    org: "acme".into(),
                    manifest_json: manifest("Escalation", true, None, &[], &[]),
                },
            ))
            .await
        ),
        "POLICY_DENIED"
    );
    // The org's IP allow list can refuse an app.
    let mut blocked = req(
        &jwt(&w, &nn),
        CreateInstallationTokenRequest {
            installation_id: keyed_inst.id.clone(),
            ..Default::default()
        },
    );
    blocked
        .metadata_mut()
        .insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
    assert_eq!(
        reason(s.create_installation_token(blocked).await),
        "IP_NOT_ALLOWED"
    );

    // Suspension: target managers, the app's owner and the app itself; members cannot.
    let suspend = |bearer: &str, id: &str| {
        let r = req(
            bearer,
            SuspendAppInstallationRequest {
                installation_id: id.into(),
                reason: "x".into(),
            },
        );
        async move { s.suspend_app_installation(r).await }
    };
    let unsuspend = |bearer: &str, id: &str| {
        let r = req(
            bearer,
            UnsuspendAppInstallationRequest {
                installation_id: id.into(),
            },
        );
        async move { s.unsuspend_app_installation(r).await }
    };
    assert_eq!(
        reason(suspend(&tok("writer"), &keyed_inst.id).await),
        "PERMISSION_DENIED"
    );
    assert!(
        suspend(&tok("appmgr"), &keyed_inst.id)
            .await
            .unwrap()
            .into_inner()
            .suspended
    );
    assert_eq!(
        reason(mint(&jwt(&w, &nn), &keyed_inst.id, vec![]).await),
        "INVALID_STATE"
    );
    assert!(
        !unsuspend(&jwt(&w, &nn), &keyed_inst.id)
            .await
            .unwrap()
            .into_inner()
            .suspended
    );
    // Repo changes are for the target's managers only (not even the app).
    let add = |bearer: &str, id: &str| {
        let r = req(
            bearer,
            AddAppInstallationReposRequest {
                installation_id: id.into(),
                repos: vec![rid(&w.repos[2])],
            },
        );
        async move { s.add_app_installation_repos(r).await }
    };
    assert_eq!(
        reason(add(&jwt(&w, &nn), &keyed_inst.id).await),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(add(&tok("writer"), &keyed_inst.id).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        add(&tok("admin"), &keyed_inst.id)
            .await
            .unwrap()
            .into_inner()
            .repo_count,
        3
    );
    let remove = req(
        &tok("admin"),
        RemoveAppInstallationReposRequest {
            installation_id: keyed_inst.id.clone(),
            repos: vec![rid(&w.repos[2])],
        },
    );
    assert_eq!(
        s.remove_app_installation_repos(remove)
            .await
            .unwrap()
            .into_inner()
            .repo_count,
        2
    );
    // Uninstall.
    let del = |bearer: &str, id: &str| {
        let r = req(
            bearer,
            DeleteAppInstallationRequest {
                installation_id: id.into(),
            },
        );
        async move { s.delete_app_installation(r).await }
    };
    assert_eq!(
        reason(del(&tok("reader"), &keyed_inst.id).await),
        "PERMISSION_DENIED"
    );
    del(&tok("admin"), &keyed_inst.id).await.unwrap();
    assert_eq!(
        reason(get_inst(&tok("admin"), &keyed_inst.id).await),
        "NOT_FOUND"
    );

    // Keys and deletion are the owner side.
    let key_req = |who: &str| {
        let r = req(
            &tok(who),
            CreateAppPrivateKeyRequest {
                app: app.slug.clone(),
            },
        );
        async move { s.create_app_private_key(r).await }
    };
    assert_eq!(reason(key_req("writer").await), "PERMISSION_DENIED");
    let k = key_req("admin").await.unwrap().into_inner();
    assert!(k.private_key_pem.contains("PRIVATE KEY"));
    let delete = |who: &str| {
        let r = req(
            &tok(who),
            DeleteAppRequest {
                app: app.slug.clone(),
            },
        );
        async move { s.delete_app(r).await }
    };
    assert_eq!(reason(delete("writer").await), "PERMISSION_DENIED");
    delete("appmgr").await.unwrap();
    assert_eq!(
        reason(
            s.get_app(req(
                &tok("owner"),
                GetAppRequest {
                    app: app.slug.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
}
