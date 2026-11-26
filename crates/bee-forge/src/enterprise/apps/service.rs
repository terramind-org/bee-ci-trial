//! `AppService`: the tonic service for apps, installations, OAuth and the marketplace.
//!
//! Authorization (SPEC §7 pattern: authenticate, validate, authorize, audit denials through
//! `guarded`, map errors with `reply`):
//!
//! | RPC group | who |
//! |---|---|
//! | create/update/delete apps, keys, OAuth apps, listings, plans, verification | owner org: owner, admin or app manager; user-owned: that user |
//! | install, repo changes | target org: owner, admin or app manager (user account: that user); non-public apps only by the app's owner |
//! | get/list installations | target org members, the app's owner, or the app itself (JWT) |
//! | suspend, unsuspend, uninstall | target managers, the app's owner, or the app itself |
//! | `CreateInstallationToken` | the app (`authorization: Bearer <JWT>`, recognized by its two dots) or an org owner/admin/app manager with a normal Biscuit |
//! | consent, grants | the signed-in user (or, for another principal's grants, an owner/admin of an org they belong to) |
//! | `ExchangeOAuthCode`, `RefreshOAuthToken`, `StartOAuthDeviceFlow`, `PollOAuthDeviceFlow` | no bearer: the client proves itself with its secret, PKCE verifier or device code |
//! | purchases | buyer org: owner, admin, app manager or billing manager |
//!
//! Management calls need a token that is not restricted to repos and allows `manage_settings`,
//! so an installation token can never manage apps.

use std::sync::Arc;

use bee_policy::Action;
use bee_proto::v1::app_service_server::{AppService, AppServiceServer};
use bee_proto::v1::*;
use tonic::{Request, Response, Status};

use super::catalog;
use super::marketplace::{
    ListingFilter, ListingInput, ListingPatch, ListingView, PlanInput, PlanPatch, PurchaseInput,
};
use super::oauth::{AuthorizationRequest, NewOAuthApp, OAuthAppPatch, RevokeTarget};
use super::registry::AppPatch;
use super::{Account, AccountKind, Apps, TokenRequest};
use crate::auth::Caller;
use crate::cx::Cx;
use crate::enterprise::common::{ScopeNeed, authorize_org};
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::ts;
use crate::service::{ForgeServices, next, page, reply};

/// The `AppService` implementation.
#[derive(Clone)]
pub struct AppsService {
    svc: ForgeServices,
    apps: Arc<Apps>,
}

/// Who is calling: a Biscuit holder, or an app with a JWT.
struct Auth {
    cx: Cx,
    caller: Option<Caller>,
    app: Option<App>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InstAccess {
    View,
    Admin,
    Repos,
    Token,
}

fn md<T>(req: &Request<T>, key: &str) -> String {
    req.metadata()
        .get(key)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn done<T>(cx: &Cx, r: Result<T>) -> std::result::Result<Response<T>, Status> {
    r.map(Response::new)
        .map_err(|e| e.to_status(&cx.request_id))
}

fn ref_id(id: &str) -> RepoRef {
    RepoRef {
        repo: Some(repo_ref::Repo::Id(id.to_string())),
    }
}

impl AppsService {
    /// A service over `forge` and its apps store.
    pub fn new(forge: Arc<Forge>, apps: Arc<Apps>) -> AppsService {
        AppsService {
            svc: ForgeServices::new(forge),
            apps,
        }
    }

    /// The tonic server for `AppService`.
    pub fn server(&self) -> AppServiceServer<AppsService> {
        AppServiceServer::new(self.clone())
    }

    /// Adds the service to a router builder.
    pub fn register(&self, routes: &mut tonic::service::RoutesBuilder) {
        routes.add_service(self.server());
    }

    fn forge(&self) -> &Arc<Forge> {
        self.svc.forge()
    }

    // ---------- authentication ----------

    /// Biscuit or app JWT.
    async fn who<T>(&self, req: &Request<T>) -> std::result::Result<Auth, Status> {
        let rid = md(req, "x-request-id");
        let bearer = md(req, "authorization");
        let token = bearer
            .strip_prefix("Bearer ")
            .or_else(|| bearer.strip_prefix("bearer "))
            .unwrap_or("")
            .trim()
            .to_string();
        if super::crypto::looks_like_jwt(&token) {
            let app = self
                .apps
                .authenticate_app_jwt(&token)
                .await
                .map_err(|e| e.to_status(&rid))?;
            let mut cx = Cx::new(app.principal_id.clone(), PrincipalKind::Service);
            cx.request_id = if rid.is_empty() {
                self.forge().db.ids().ulid().map_err(|e| e.to_status(""))?
            } else {
                rid
            };
            cx.ip = match md(req, "x-forwarded-for") {
                s if !s.is_empty() => s.split(',').next().unwrap_or("").trim().to_string(),
                _ => req
                    .remote_addr()
                    .map(|a| a.ip().to_string())
                    .unwrap_or_default(),
            };
            cx.user_agent = md(req, "user-agent");
            cx.decision = format!("app-jwt:{}", app.slug);
            return Ok(Auth {
                cx,
                caller: None,
                app: Some(app),
            });
        }
        let caller = self.svc.caller(req)?;
        Ok(Auth {
            cx: caller.cx.clone(),
            caller: Some(caller),
            app: None,
        })
    }

    /// A context for the unauthenticated OAuth endpoints.
    fn public_cx<T>(&self, req: &Request<T>) -> Cx {
        let mut cx = Cx::new("oauth-client", PrincipalKind::Service);
        let rid = md(req, "x-request-id");
        cx.request_id = if rid.is_empty() {
            self.forge().db.ids().ulid().unwrap_or_default()
        } else {
            rid
        };
        cx.ip = match md(req, "x-forwarded-for") {
            s if !s.is_empty() => s.split(',').next().unwrap_or("").trim().to_string(),
            _ => req
                .remote_addr()
                .map(|a| a.ip().to_string())
                .unwrap_or_default(),
        };
        cx.user_agent = md(req, "user-agent");
        cx.decision = "public oauth endpoint".into();
        cx
    }

    // ---------- authorization helpers ----------

    fn token_may_manage(caller: &Caller) -> Result<()> {
        if caller.token.scope.repos.is_some() {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "app management needs a token not restricted to repos",
            ));
        }
        if !caller.token.allows_action(Action::ManageSettings) {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "token does not allow `manage_settings`",
            ));
        }
        Ok(())
    }

    /// The signed-in human acting for themselves (no delegation).
    fn require_user(caller: &Caller) -> Result<String> {
        if caller.cx.actor_kind != PrincipalKind::Human || caller.cx.chain.len() != 1 {
            return Err(ForgeError::permission_denied(
                "this call needs the signed-in user, not a delegated or service token",
            ));
        }
        Ok(caller.cx.actor_id.clone())
    }

    /// Org owner, admin or app manager.
    async fn manage_org(&self, caller: &Caller, org_id: &str) -> Result<Cx> {
        Self::token_may_manage(caller)?;
        let mut decision = vec!["token:ok".to_string()];
        for id in caller.cx.chain.clone() {
            let role = self
                .forge()
                .orgs
                .member(&caller.cx, org_id, &id)
                .await?
                .map(|m| Role::try_from(m.role).unwrap_or(Role::Member));
            if !matches!(role, Some(Role::Owner | Role::Admin | Role::AppManager)) {
                return Err(ForgeError::permission_denied(format!(
                    "{id} is not an owner, admin or app manager of {org_id}"
                )));
            }
            decision.push(format!(
                "{id}:org:{}",
                role.map(|r| r.as_str_name()).unwrap_or("none")
            ));
        }
        let mut cx = caller.cx.clone();
        cx.decision = decision.join(" ");
        Ok(cx)
    }

    /// Manages the thing owned by an owner key (`org:<id>` / `user:<id>`).
    async fn manage_owner(&self, caller: &Caller, owner_key: &str) -> Result<Cx> {
        if let Some(org) = owner_key.strip_prefix("org:") {
            self.manage_org(caller, org).await
        } else if let Some(user) = owner_key.strip_prefix("user:") {
            Self::token_may_manage(caller)?;
            if Self::require_user(caller)? != user {
                return Err(ForgeError::permission_denied("not your account"));
            }
            let mut cx = caller.cx.clone();
            cx.decision = "token:ok owner".into();
            Ok(cx)
        } else {
            Err(ForgeError::internal(format!("bad owner key {owner_key}")))
        }
    }

    fn org_of_key(key: &str) -> String {
        key.strip_prefix("org:").unwrap_or("").to_string()
    }

    /// `guarded` for an owner key.
    async fn guard_owner(&self, caller: &Caller, owner_key: &str, action: &str) -> Result<Cx> {
        let auth = self.manage_owner(caller, owner_key).await;
        self.svc
            .guarded(caller, &Self::org_of_key(owner_key), "", action, auth)
            .await
    }

    /// An empty `org` names the caller's own user account.
    async fn account_of(&self, caller: &Caller, org: &str) -> Result<Account> {
        if org.is_empty() {
            let user = Self::require_user(caller)?;
            Ok(self.apps.user_account(&user).await)
        } else {
            self.org_account(caller, org).await
        }
    }

    async fn org_account(&self, caller: &Caller, org: &str) -> Result<Account> {
        let o = self.forge().orgs.get_org(&caller.cx, org).await?;
        Ok(Account::org(&o.id, &o.name))
    }

    async fn repos_of(&self, cx: &Cx, refs: &[RepoRef]) -> Result<Vec<Repo>> {
        let mut out = Vec::new();
        for r in refs {
            if r.repo.is_none() {
                return Err(ForgeError::missing("repo"));
            }
            out.push(self.forge().orgs.resolve_repo(cx, r).await?);
        }
        Ok(out)
    }

    async fn installation_access(
        &self,
        auth: &Auth,
        id: &str,
        need: InstAccess,
        action: &str,
    ) -> Result<Cx> {
        let (target_key, app_id) = self.apps.installation_target(&auth.cx, id).await?;
        if let Some(app) = &auth.app {
            return if app.id == app_id && need != InstAccess::Repos {
                Ok(auth.cx.clone())
            } else {
                Err(ForgeError::not_found(format!("installation {id}")))
            };
        }
        let Some(caller) = auth.caller.as_ref() else {
            return Err(ForgeError::internal("no caller"));
        };
        let org_id = Self::org_of_key(&target_key);
        let target = if need == InstAccess::View {
            if org_id.is_empty() {
                self.manage_owner(caller, &target_key).await
            } else {
                self.forge()
                    .authz
                    .org(caller, &org_id, crate::auth::OrgPerm::ViewMembers)
                    .await
            }
        } else {
            self.manage_owner(caller, &target_key).await
        };
        if let Ok(cx) = target {
            return Ok(cx);
        }
        if matches!(need, InstAccess::View | InstAccess::Admin)
            && let Ok(owner) = self.apps.app_owner_key(&auth.cx, &app_id).await
            && let Ok(cx) = self.manage_owner(caller, &owner).await
        {
            return Ok(cx);
        }
        self.svc
            .guarded(
                caller,
                &org_id,
                "",
                action,
                Err(ForgeError::permission_denied(format!(
                    "no access to installation {id}"
                ))),
            )
            .await
    }

    async fn publisher_org(
        &self,
        caller: &Caller,
        listing: &str,
        action: &str,
    ) -> Result<(Cx, String)> {
        let org_id = self.apps.listing_publisher(&caller.cx, listing).await?;
        let auth = self.manage_org(caller, &org_id).await;
        let cx = self.svc.guarded(caller, &org_id, "", action, auth).await?;
        Ok((cx, org_id))
    }

    async fn buyer_cx(&self, caller: &Caller, org_id: &str, action: &str) -> Result<Cx> {
        let auth = match self.manage_org(caller, org_id).await {
            Ok(cx) => Ok(cx),
            Err(_) => authorize_org(self.forge(), caller, org_id, ScopeNeed::Billing).await,
        };
        self.svc.guarded(caller, org_id, "", action, auth).await
    }

    /// Whether the caller is an owner, admin or app manager of the org of `owner_key` (or the
    /// user themselves).
    async fn can_manage(&self, caller: &Caller, owner_key: &str) -> bool {
        self.manage_owner(caller, owner_key).await.is_ok()
    }

    /// A user's id for a grant call: the caller, or a managed user's id for an owner/admin of
    /// an org the user belongs to.
    async fn grant_subject(
        &self,
        caller: &Caller,
        principal_id: &str,
        action: &str,
    ) -> Result<(Cx, String)> {
        let me = Self::require_user(caller)?;
        if principal_id.is_empty() || principal_id == me {
            return Ok((caller.cx.clone(), me));
        }
        Self::token_may_manage(caller)?;
        let (orgs, _) = self
            .forge()
            .orgs
            .orgs_of(&caller.cx, principal_id, "", 500)
            .await?;
        let mut last = ForgeError::permission_denied("not a user you manage");
        let mut org_for_audit = String::new();
        for o in orgs {
            org_for_audit = o.id.clone();
            match self
                .forge()
                .authz
                .org(caller, &o.id, crate::auth::OrgPerm::Manage)
                .await
            {
                Ok(cx) => return Ok((cx, principal_id.to_string())),
                Err(e) => last = e,
            }
        }
        self.svc
            .guarded(caller, &org_for_audit, "", action, Err(last))
            .await
            .map(|cx| (cx, principal_id.to_string()))
    }
}

fn app_patch(r: UpdateAppRequest) -> Result<AppPatch> {
    Ok(AppPatch {
        name: r.name,
        description: r.description,
        homepage_url: r.homepage_url,
        callback_urls: r.replace_callback_urls.then_some(r.callback_urls),
        setup_url: r.setup_url,
        request_oauth_on_install: r.request_oauth_on_install,
        webhook_url: r.webhook_url,
        webhook_active: r.webhook_active,
        webhook_secret: r.webhook_secret,
        permissions: if r.replace_permissions {
            Some(catalog::from_proto(&r.permissions, "invalid_permission")?)
        } else {
            None
        },
        events: r.replace_events.then_some(r.events),
        public: r.public,
    })
}

fn tokens_ts(ms: u64) -> Option<pbjson_types::Timestamp> {
    Some(ts(ms))
}

fn listing_of(v: ListingView) -> MarketplaceListing {
    v.listing
}

#[tonic::async_trait]
impl AppService for AppsService {
    // ---------- apps ----------

    async fn create_app_from_manifest(
        &self,
        req: Request<CreateAppFromManifestRequest>,
    ) -> std::result::Result<Response<CreateAppFromManifestResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            if r.manifest_json.trim().is_empty() {
                return Err(ForgeError::invalid("bad_manifest", "the manifest is empty"));
            }
            let owner = self.account_of(&caller, &r.org).await?;
            let cx = self
                .guard_owner(&caller, &owner.key(), "app.create")
                .await?;
            let n = self.apps.create_app(&cx, &owner, &r.manifest_json).await?;
            Ok(CreateAppFromManifestResponse {
                app: Some(n.app),
                client_secret: n.client_secret,
                webhook_secret: n.webhook_secret,
                private_key_pem: n.private_key_pem,
                private_key_id: n.private_key_id,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_app(
        &self,
        req: Request<GetAppRequest>,
    ) -> std::result::Result<Response<App>, Status> {
        let auth = self.who(&req).await?;
        let r = req.into_inner();
        let out = async {
            let app = self.apps.get_app(&auth.cx, &r.app).await?;
            if let Some(me) = &auth.app {
                return if me.id == app.id {
                    Ok(app)
                } else {
                    Err(ForgeError::not_found(format!("app {}", r.app)))
                };
            }
            let Some(caller) = &auth.caller else {
                return Err(ForgeError::internal("no caller"));
            };
            if !app.public {
                let key = self.apps.app_owner_key(&auth.cx, &app.id).await?;
                if !self.can_manage(caller, &key).await {
                    return Err(ForgeError::not_found(format!("app {}", r.app)));
                }
            }
            Ok(app)
        }
        .await;
        done(&auth.cx, out)
    }

    async fn update_app(
        &self,
        req: Request<UpdateAppRequest>,
    ) -> std::result::Result<Response<App>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let key = self.apps.app_owner_key(&caller.cx, &r.app).await?;
            let cx = self.guard_owner(&caller, &key, "app.update").await?;
            let target = r.app.clone();
            self.apps.update_app(&cx, &target, app_patch(r)?).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_app(
        &self,
        req: Request<DeleteAppRequest>,
    ) -> std::result::Result<Response<DeleteAppResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let key = self.apps.app_owner_key(&caller.cx, &r.app).await?;
            let cx = self.guard_owner(&caller, &key, "app.delete").await?;
            self.apps.delete_app(&cx, &r.app).await?;
            Ok(DeleteAppResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_apps(
        &self,
        req: Request<ListAppsRequest>,
    ) -> std::result::Result<Response<ListAppsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let owner = if r.owner.is_empty() {
                self.account_of(&caller, "").await?
            } else {
                self.apps.resolve_account(&caller.cx, &r.owner).await?
            };
            let manage = self.can_manage(&caller, &owner.key()).await;
            let (token, limit) = page(&r.page);
            let (mut apps, nx) = self
                .apps
                .list_apps(&caller.cx, &owner, &token, limit)
                .await?;
            if !manage {
                apps.retain(|a| a.public);
            }
            Ok(ListAppsResponse {
                apps,
                page: next(nx),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_app_private_key(
        &self,
        req: Request<CreateAppPrivateKeyRequest>,
    ) -> std::result::Result<Response<CreateAppPrivateKeyResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let key = self.apps.app_owner_key(&caller.cx, &r.app).await?;
            let cx = self.guard_owner(&caller, &key, "app.key_create").await?;
            let (k, pem) = self.apps.create_private_key(&cx, &r.app).await?;
            Ok(CreateAppPrivateKeyResponse {
                key: Some(k),
                private_key_pem: pem,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_app_private_key(
        &self,
        req: Request<DeleteAppPrivateKeyRequest>,
    ) -> std::result::Result<Response<DeleteAppPrivateKeyResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let key = self.apps.app_owner_key(&caller.cx, &r.app).await?;
            let cx = self.guard_owner(&caller, &key, "app.key_delete").await?;
            self.apps.delete_private_key(&cx, &r.app, &r.key_id).await?;
            Ok(DeleteAppPrivateKeyResponse {})
        }
        .await;
        reply(&caller, out)
    }

    // ---------- installations ----------

    async fn create_app_installation(
        &self,
        req: Request<CreateAppInstallationRequest>,
    ) -> std::result::Result<Response<AppInstallation>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let app = self.apps.get_app(&caller.cx, &r.app).await?;
            let target = self.account_of(&caller, &r.org).await?;
            let cx = self
                .guard_owner(&caller, &target.key(), "app.install")
                .await?;
            if !app.public {
                let key = self.apps.app_owner_key(&caller.cx, &app.id).await?;
                if self.manage_owner(&caller, &key).await.is_err() {
                    // Do not reveal private apps to other accounts.
                    return Err(ForgeError::not_found(format!("app {}", r.app)));
                }
            }
            let repos = self.repos_of(&cx, &r.repos).await?;
            let selection = AppRepositorySelection::try_from(r.repository_selection)
                .unwrap_or(AppRepositorySelection::Unspecified);
            self.apps
                .create_installation(&cx, &app.id, &target, selection, &repos)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_app_installation(
        &self,
        req: Request<GetAppInstallationRequest>,
    ) -> std::result::Result<Response<AppInstallation>, Status> {
        let auth = self.who(&req).await?;
        let r = req.into_inner();
        let out = async {
            let cx = self
                .installation_access(
                    &auth,
                    &r.installation_id,
                    InstAccess::View,
                    "app.installation_get",
                )
                .await?;
            self.apps.get_installation(&cx, &r.installation_id).await
        }
        .await;
        done(&auth.cx, out)
    }

    async fn list_app_installations(
        &self,
        req: Request<ListAppInstallationsRequest>,
    ) -> std::result::Result<Response<ListAppInstallationsResponse>, Status> {
        let auth = self.who(&req).await?;
        let r = req.into_inner();
        let out = async {
            let (token, limit) = page(&r.page);
            let (installations, nx) = match r.filter {
                Some(list_app_installations_request::Filter::App(app)) => {
                    let a = self.apps.get_app(&auth.cx, &app).await?;
                    match (&auth.app, &auth.caller) {
                        (Some(me), _) if me.id == a.id => {}
                        (Some(_), _) => return Err(ForgeError::not_found(format!("app {app}"))),
                        (None, Some(caller)) => {
                            let key = self.apps.app_owner_key(&auth.cx, &a.id).await?;
                            self.guard_owner(caller, &key, "app.installations_list")
                                .await?;
                        }
                        (None, None) => return Err(ForgeError::internal("no caller")),
                    }
                    self.apps
                        .list_installations_of_app(&auth.cx, &a.id, &token, limit)
                        .await?
                }
                Some(list_app_installations_request::Filter::Account(name)) => {
                    let Some(caller) = &auth.caller else {
                        return Err(ForgeError::permission_denied(
                            "an app lists its own installations by app",
                        ));
                    };
                    let account = self.apps.resolve_account(&caller.cx, &name).await?;
                    match account.kind {
                        AccountKind::Org => {
                            self.forge()
                                .authz
                                .org(caller, &account.id, crate::auth::OrgPerm::ViewMembers)
                                .await?;
                        }
                        AccountKind::User => {
                            if Self::require_user(caller)? != account.id {
                                return Err(ForgeError::permission_denied("not your account"));
                            }
                        }
                    }
                    self.apps
                        .list_installations_of_account(&caller.cx, &account, &token, limit)
                        .await?
                }
                None => return Err(ForgeError::missing("filter")),
            };
            Ok(ListAppInstallationsResponse {
                installations,
                page: next(nx),
            })
        }
        .await;
        done(&auth.cx, out)
    }

    async fn suspend_app_installation(
        &self,
        req: Request<SuspendAppInstallationRequest>,
    ) -> std::result::Result<Response<AppInstallation>, Status> {
        let auth = self.who(&req).await?;
        let r = req.into_inner();
        let out = async {
            let cx = self
                .installation_access(&auth, &r.installation_id, InstAccess::Admin, "app.suspend")
                .await?;
            self.apps
                .suspend_installation(&cx, &r.installation_id, &r.reason)
                .await
        }
        .await;
        done(&auth.cx, out)
    }

    async fn unsuspend_app_installation(
        &self,
        req: Request<UnsuspendAppInstallationRequest>,
    ) -> std::result::Result<Response<AppInstallation>, Status> {
        let auth = self.who(&req).await?;
        let r = req.into_inner();
        let out = async {
            let cx = self
                .installation_access(
                    &auth,
                    &r.installation_id,
                    InstAccess::Admin,
                    "app.unsuspend",
                )
                .await?;
            self.apps
                .unsuspend_installation(&cx, &r.installation_id)
                .await
        }
        .await;
        done(&auth.cx, out)
    }

    async fn delete_app_installation(
        &self,
        req: Request<DeleteAppInstallationRequest>,
    ) -> std::result::Result<Response<DeleteAppInstallationResponse>, Status> {
        let auth = self.who(&req).await?;
        let r = req.into_inner();
        let out = async {
            let cx = self
                .installation_access(
                    &auth,
                    &r.installation_id,
                    InstAccess::Admin,
                    "app.uninstall",
                )
                .await?;
            self.apps
                .delete_installation(&cx, &r.installation_id)
                .await?;
            Ok(DeleteAppInstallationResponse {})
        }
        .await;
        done(&auth.cx, out)
    }

    async fn add_app_installation_repos(
        &self,
        req: Request<AddAppInstallationReposRequest>,
    ) -> std::result::Result<Response<AddAppInstallationReposResponse>, Status> {
        let auth = self.who(&req).await?;
        let r = req.into_inner();
        let out = async {
            let cx = self
                .installation_access(
                    &auth,
                    &r.installation_id,
                    InstAccess::Repos,
                    "app.installation_repos_add",
                )
                .await?;
            let repos = self.repos_of(&cx, &r.repos).await?;
            let n = self
                .apps
                .add_installation_repos(&cx, &r.installation_id, &repos)
                .await?;
            Ok(AddAppInstallationReposResponse { repo_count: n })
        }
        .await;
        done(&auth.cx, out)
    }

    async fn remove_app_installation_repos(
        &self,
        req: Request<RemoveAppInstallationReposRequest>,
    ) -> std::result::Result<Response<RemoveAppInstallationReposResponse>, Status> {
        let auth = self.who(&req).await?;
        let r = req.into_inner();
        let out = async {
            let cx = self
                .installation_access(
                    &auth,
                    &r.installation_id,
                    InstAccess::Repos,
                    "app.installation_repos_remove",
                )
                .await?;
            let repos = self.repos_of(&cx, &r.repos).await?;
            let n = self
                .apps
                .remove_installation_repos(&cx, &r.installation_id, &repos)
                .await?;
            Ok(RemoveAppInstallationReposResponse { repo_count: n })
        }
        .await;
        done(&auth.cx, out)
    }

    async fn list_app_installation_repos(
        &self,
        req: Request<ListAppInstallationReposRequest>,
    ) -> std::result::Result<Response<ListAppInstallationReposResponse>, Status> {
        let auth = self.who(&req).await?;
        let r = req.into_inner();
        let out = async {
            let cx = self
                .installation_access(
                    &auth,
                    &r.installation_id,
                    InstAccess::View,
                    "app.installation_repos_list",
                )
                .await?;
            let (token, limit) = page(&r.page);
            let (ids, nx) = self
                .apps
                .list_installation_repos(&cx, &r.installation_id, &token, limit)
                .await?;
            Ok(ListAppInstallationReposResponse {
                repos: ids.iter().map(|i| ref_id(i)).collect(),
                page: next(nx),
            })
        }
        .await;
        done(&auth.cx, out)
    }

    async fn create_installation_token(
        &self,
        req: Request<CreateInstallationTokenRequest>,
    ) -> std::result::Result<Response<CreateInstallationTokenResponse>, Status> {
        let auth = self.who(&req).await?;
        let r = req.into_inner();
        let out = async {
            let cx = self
                .installation_access(
                    &auth,
                    &r.installation_id,
                    InstAccess::Token,
                    "app.token_create",
                )
                .await?;
            let repos = self.repos_of(&cx, &r.repos).await?;
            let t = self
                .apps
                .create_installation_token(
                    &cx,
                    &r.installation_id,
                    TokenRequest {
                        repo_ids: repos.into_iter().map(|x| x.id).collect(),
                        permissions: r.permissions,
                        ttl_seconds: r.ttl_seconds,
                    },
                )
                .await?;
            Ok(CreateInstallationTokenResponse {
                token: t.token,
                token_id: t.token_id,
                expires_at: tokens_ts(t.expires_at_ms),
                repos: t.repo_ids.iter().map(|i| ref_id(i)).collect(),
                permissions: t.permissions,
            })
        }
        .await;
        done(&auth.cx, out)
    }

    // ---------- OAuth apps ----------

    async fn create_o_auth_app(
        &self,
        req: Request<CreateOAuthAppRequest>,
    ) -> std::result::Result<Response<CreateOAuthAppResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let owner = self.account_of(&caller, &r.org).await?;
            let cx = self
                .guard_owner(&caller, &owner.key(), "oauth.app_create")
                .await?;
            let (app, secret) = self
                .apps
                .create_oauth_app(
                    &cx,
                    &owner,
                    NewOAuthApp {
                        name: r.name,
                        description: r.description,
                        homepage_url: r.homepage_url,
                        callback_urls: r.callback_urls,
                        device_flow_enabled: r.device_flow_enabled,
                    },
                )
                .await?;
            Ok(CreateOAuthAppResponse {
                app: Some(app),
                client_secret: secret,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_o_auth_app(
        &self,
        req: Request<GetOAuthAppRequest>,
    ) -> std::result::Result<Response<OAuthApp>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = self.apps.get_oauth_app(&caller.cx, &r.oauth_app).await;
        reply(&caller, out)
    }

    async fn update_o_auth_app(
        &self,
        req: Request<UpdateOAuthAppRequest>,
    ) -> std::result::Result<Response<UpdateOAuthAppResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let key = self
                .apps
                .oauth_app_owner_key(&caller.cx, &r.oauth_app)
                .await?;
            let cx = self.guard_owner(&caller, &key, "oauth.app_update").await?;
            let (app, secret) = self
                .apps
                .update_oauth_app(
                    &cx,
                    &r.oauth_app,
                    OAuthAppPatch {
                        name: r.name,
                        description: r.description,
                        homepage_url: r.homepage_url,
                        callback_urls: r.replace_callback_urls.then_some(r.callback_urls),
                        device_flow_enabled: r.device_flow_enabled,
                        reset_client_secret: r.reset_client_secret,
                    },
                )
                .await?;
            Ok(UpdateOAuthAppResponse {
                app: Some(app),
                client_secret: secret.unwrap_or_default(),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_o_auth_app(
        &self,
        req: Request<DeleteOAuthAppRequest>,
    ) -> std::result::Result<Response<DeleteOAuthAppResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let key = self
                .apps
                .oauth_app_owner_key(&caller.cx, &r.oauth_app)
                .await?;
            let cx = self.guard_owner(&caller, &key, "oauth.app_delete").await?;
            self.apps.delete_oauth_app(&cx, &r.oauth_app).await?;
            Ok(DeleteOAuthAppResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_o_auth_apps(
        &self,
        req: Request<ListOAuthAppsRequest>,
    ) -> std::result::Result<Response<ListOAuthAppsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let owner = if r.owner.is_empty() {
                self.account_of(&caller, "").await?
            } else {
                self.apps.resolve_account(&caller.cx, &r.owner).await?
            };
            let cx = self
                .guard_owner(&caller, &owner.key(), "oauth.app_list")
                .await?;
            let (token, limit) = page(&r.page);
            let (apps, nx) = self
                .apps
                .list_oauth_apps(&cx, &owner, &token, limit)
                .await?;
            Ok(ListOAuthAppsResponse {
                apps,
                page: next(nx),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------- OAuth flows ----------

    async fn create_o_auth_authorization(
        &self,
        req: Request<CreateOAuthAuthorizationRequest>,
    ) -> std::result::Result<Response<CreateOAuthAuthorizationResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let user = Self::require_user(&caller)?;
            if !caller.token.allows_action(Action::Read) {
                return Err(ForgeError::new(
                    ErrorReason::PolicyDenied,
                    "token does not allow `read`",
                ));
            }
            let mut cx = caller.cx.clone();
            cx.decision = format!("token:ok user:{user}");
            let (client_id, user_code) = match r.flow {
                Some(create_o_auth_authorization_request::Flow::ClientId(c)) => (c, String::new()),
                Some(create_o_auth_authorization_request::Flow::UserCode(c)) => (String::new(), c),
                None => return Err(ForgeError::missing("client_id or user_code")),
            };
            let o = self
                .apps
                .create_authorization(
                    &cx,
                    AuthorizationRequest {
                        client_id,
                        user_code,
                        redirect_uri: r.redirect_uri,
                        scopes: r.scopes,
                        state: r.state,
                        code_challenge: r.code_challenge,
                        code_challenge_method: r.code_challenge_method,
                        deny: r.deny,
                        nonce: String::new(),
                    },
                )
                .await?;
            Ok(CreateOAuthAuthorizationResponse {
                code: o.code,
                redirect_url: o.redirect_url,
                code_expires_at: (o.code_expires_ms > 0).then(|| ts(o.code_expires_ms)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn exchange_o_auth_code(
        &self,
        req: Request<ExchangeOAuthCodeRequest>,
    ) -> std::result::Result<Response<ExchangeOAuthCodeResponse>, Status> {
        let cx = self.public_cx(&req);
        let r = req.into_inner();
        let out = self
            .apps
            .exchange_code(
                &cx,
                &r.client_id,
                &r.client_secret,
                &r.code,
                &r.redirect_uri,
                &r.code_verifier,
            )
            .await
            .map(|t| ExchangeOAuthCodeResponse { tokens: Some(t) });
        done(&cx, out)
    }

    async fn refresh_o_auth_token(
        &self,
        req: Request<RefreshOAuthTokenRequest>,
    ) -> std::result::Result<Response<RefreshOAuthTokenResponse>, Status> {
        let cx = self.public_cx(&req);
        let r = req.into_inner();
        let out = self
            .apps
            .refresh_token(&cx, &r.client_id, &r.client_secret, &r.refresh_token)
            .await
            .map(|t| RefreshOAuthTokenResponse { tokens: Some(t) });
        done(&cx, out)
    }

    async fn start_o_auth_device_flow(
        &self,
        req: Request<StartOAuthDeviceFlowRequest>,
    ) -> std::result::Result<Response<StartOAuthDeviceFlowResponse>, Status> {
        let cx = self.public_cx(&req);
        let r = req.into_inner();
        let out = self
            .apps
            .start_device_flow(&cx, &r.client_id, &r.scopes)
            .await
            .map(|d| StartOAuthDeviceFlowResponse {
                device_code: d.device_code,
                user_code: d.user_code,
                verification_uri: d.verification_uri,
                verification_uri_complete: d.verification_uri_complete,
                expires_at: tokens_ts(d.expires_ms),
                interval_seconds: d.interval_secs,
            });
        done(&cx, out)
    }

    async fn poll_o_auth_device_flow(
        &self,
        req: Request<PollOAuthDeviceFlowRequest>,
    ) -> std::result::Result<Response<PollOAuthDeviceFlowResponse>, Status> {
        let cx = self.public_cx(&req);
        let r = req.into_inner();
        let out = self
            .apps
            .poll_device_flow(&cx, &r.client_id, &r.device_code)
            .await
            .map(|p| PollOAuthDeviceFlowResponse {
                status: p.status as i32,
                tokens: p.tokens,
                interval_seconds: p.interval_secs,
            });
        done(&cx, out)
    }

    async fn revoke_o_auth_grant(
        &self,
        req: Request<RevokeOAuthGrantRequest>,
    ) -> std::result::Result<Response<RevokeOAuthGrantResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, user) = self
                .grant_subject(&caller, &r.principal_id, "oauth.revoke")
                .await?;
            let target = match r.target {
                Some(revoke_o_auth_grant_request::Target::ClientId(c)) => RevokeTarget::Client(c),
                Some(revoke_o_auth_grant_request::Target::TokenId(t)) => RevokeTarget::Token(t),
                None => return Err(ForgeError::missing("client_id or token_id")),
            };
            self.apps.revoke_oauth_grant(&cx, &user, target).await?;
            Ok(RevokeOAuthGrantResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_o_auth_authorized_apps(
        &self,
        req: Request<ListOAuthAuthorizedAppsRequest>,
    ) -> std::result::Result<Response<ListOAuthAuthorizedAppsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, user) = self
                .grant_subject(&caller, &r.principal_id, "oauth.grants_list")
                .await?;
            let (token, limit) = page(&r.page);
            let (grants, nx) = self
                .apps
                .list_oauth_grants(&cx, &user, &token, limit)
                .await?;
            Ok(ListOAuthAuthorizedAppsResponse {
                grants,
                page: next(nx),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------- marketplace ----------

    async fn create_marketplace_listing(
        &self,
        req: Request<CreateMarketplaceListingRequest>,
    ) -> std::result::Result<Response<MarketplaceListing>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            if r.org.is_empty() {
                return Err(ForgeError::missing("org"));
            }
            let org = self.org_account(&caller, &r.org).await?;
            let auth = self.manage_org(&caller, &org.id).await;
            let cx = self
                .svc
                .guarded(&caller, &org.id, "", "marketplace.listing_create", auth)
                .await?;
            let mut action_repo_id = String::new();
            if let Some(rr) = &r.action_repo
                && rr.repo.is_some()
            {
                let repo = self.forge().orgs.resolve_repo(&cx, rr).await?;
                if repo.org_id != org.id {
                    return Err(ForgeError::invalid(
                        "repo_not_in_account",
                        "the action repo must belong to the publisher org",
                    ));
                }
                action_repo_id = repo.id;
            }
            let v = self
                .apps
                .create_listing(
                    &cx,
                    &org,
                    ListingInput {
                        listing_type: MarketplaceListingType::try_from(r.r#type)
                            .unwrap_or(MarketplaceListingType::Unspecified),
                        slug: r.slug,
                        name: r.name,
                        short_description: r.short_description,
                        full_description: r.full_description,
                        categories: r.categories,
                        app_id: r.app_id,
                        action_repo_id,
                        action_path: r.action_path,
                        agent_id: r.agent_id,
                        logo_url: r.logo_url,
                        support_url: r.support_url,
                        privacy_policy_url: r.privacy_policy_url,
                        terms_url: r.terms_url,
                    },
                )
                .await?;
            Ok(listing_of(v))
        }
        .await;
        reply(&caller, out)
    }

    async fn update_marketplace_listing(
        &self,
        req: Request<UpdateMarketplaceListingRequest>,
    ) -> std::result::Result<Response<MarketplaceListing>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .publisher_org(&caller, &r.listing, "marketplace.listing_update")
                .await?;
            let v = self
                .apps
                .update_listing(
                    &cx,
                    &r.listing,
                    ListingPatch {
                        name: r.name,
                        short_description: r.short_description,
                        full_description: r.full_description,
                        categories: r.replace_categories.then_some(r.categories),
                        logo_url: r.logo_url,
                        screenshot_urls: r.replace_screenshot_urls.then_some(r.screenshot_urls),
                        support_url: r.support_url,
                        privacy_policy_url: r.privacy_policy_url,
                        terms_url: r.terms_url,
                    },
                )
                .await?;
            Ok(listing_of(v))
        }
        .await;
        reply(&caller, out)
    }

    async fn publish_marketplace_listing(
        &self,
        req: Request<PublishMarketplaceListingRequest>,
    ) -> std::result::Result<Response<MarketplaceListing>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .publisher_org(
                    &caller,
                    &r.listing,
                    if r.unpublish {
                        "marketplace.unpublish"
                    } else {
                        "marketplace.submit"
                    },
                )
                .await?;
            Ok(listing_of(
                self.apps
                    .publish_listing(&cx, &r.listing, r.unpublish)
                    .await?,
            ))
        }
        .await;
        reply(&caller, out)
    }

    async fn get_marketplace_listing(
        &self,
        req: Request<GetMarketplaceListingRequest>,
    ) -> std::result::Result<Response<MarketplaceListing>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let v = self.apps.get_listing(&caller.cx, &r.listing).await?;
            if v.listing.state != MarketplaceListingState::Published as i32
                && self.manage_org(&caller, &v.publisher_org_id).await.is_err()
            {
                return Err(ForgeError::not_found(format!("listing {}", r.listing)));
            }
            Ok(listing_of(v))
        }
        .await;
        reply(&caller, out)
    }

    async fn list_marketplace_listings(
        &self,
        req: Request<ListMarketplaceListingsRequest>,
    ) -> std::result::Result<Response<ListMarketplaceListingsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let mut f = ListingFilter {
                query: r.query,
                listing_type: MarketplaceListingType::try_from(r.r#type)
                    .unwrap_or(MarketplaceListingType::Unspecified),
                category: r.category,
                verified_only: r.verified_only,
                free_only: r.free_only,
                sort: r.sort,
                ..ListingFilter::default()
            };
            if !r.publisher.is_empty() {
                let org = self.forge().orgs.get_org(&caller.cx, &r.publisher).await?;
                f.publisher_org_id = org.id.clone();
                if self.manage_org(&caller, &org.id).await.is_ok() {
                    f.include_unpublished_of = org.id;
                }
            }
            let (token, limit) = page(&r.page);
            let (views, nx) = self
                .apps
                .list_listings(&caller.cx, &f, &token, limit)
                .await?;
            Ok(ListMarketplaceListingsResponse {
                listings: views.into_iter().map(listing_of).collect(),
                page: next(nx),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_marketplace_plan(
        &self,
        req: Request<CreateMarketplacePlanRequest>,
    ) -> std::result::Result<Response<MarketplacePlan>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .publisher_org(&caller, &r.listing, "marketplace.plan_create")
                .await?;
            self.apps
                .create_plan(
                    &cx,
                    &r.listing,
                    PlanInput {
                        name: r.name,
                        description: r.description,
                        pricing_model: MarketplacePricingModel::try_from(r.pricing_model)
                            .unwrap_or(MarketplacePricingModel::Unspecified),
                        monthly_price_micro_usd: r.monthly_price_micro_usd,
                        yearly_price_micro_usd: r.yearly_price_micro_usd,
                        unit_name: r.unit_name,
                        free_trial_days: r.free_trial_days,
                        bullets: r.bullets,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_marketplace_plan(
        &self,
        req: Request<UpdateMarketplacePlanRequest>,
    ) -> std::result::Result<Response<MarketplacePlan>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .publisher_org(&caller, &r.listing, "marketplace.plan_update")
                .await?;
            self.apps
                .update_plan(
                    &cx,
                    &r.listing,
                    &r.plan_id,
                    PlanPatch {
                        name: r.name,
                        description: r.description,
                        monthly_price_micro_usd: r.monthly_price_micro_usd,
                        yearly_price_micro_usd: r.yearly_price_micro_usd,
                        free_trial_days: r.free_trial_days,
                        bullets: r.replace_bullets.then_some(r.bullets),
                        retired: r.retired,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn purchase_marketplace_plan(
        &self,
        req: Request<PurchaseMarketplacePlanRequest>,
    ) -> std::result::Result<Response<MarketplacePurchase>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_account(&caller, &r.org).await?;
            let cx = self
                .buyer_cx(&caller, &org.id, "marketplace.purchase")
                .await?;
            self.apps
                .purchase_plan(
                    &cx,
                    &org,
                    &r.listing,
                    PurchaseInput {
                        plan_id: r.plan_id,
                        unit_count: r.unit_count,
                        billing_cycle: MarketplaceBillingCycle::try_from(r.billing_cycle)
                            .unwrap_or(MarketplaceBillingCycle::Unspecified),
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn cancel_marketplace_purchase(
        &self,
        req: Request<CancelMarketplacePurchaseRequest>,
    ) -> std::result::Result<Response<MarketplacePurchase>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_account(&caller, &r.org).await?;
            let cx = self
                .buyer_cx(&caller, &org.id, "marketplace.purchase_cancel")
                .await?;
            self.apps
                .cancel_purchase(&cx, &org.id, &r.purchase_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_marketplace_purchases(
        &self,
        req: Request<ListMarketplacePurchasesRequest>,
    ) -> std::result::Result<Response<ListMarketplacePurchasesResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (token, limit) = page(&r.page);
            let (purchases, nx) = match r.filter {
                Some(list_marketplace_purchases_request::Filter::Org(o)) => {
                    let org = self.org_account(&caller, &o).await?;
                    let cx = self
                        .buyer_cx(&caller, &org.id, "marketplace.purchases_list")
                        .await?;
                    self.apps
                        .list_purchases_of_org(&cx, &org.id, r.include_cancelled, &token, limit)
                        .await?
                }
                Some(list_marketplace_purchases_request::Filter::Listing(l)) => {
                    let (cx, _) = self
                        .publisher_org(&caller, &l, "marketplace.purchasers_list")
                        .await?;
                    self.apps
                        .list_purchases_of_listing(&cx, &l, r.include_cancelled, &token, limit)
                        .await?
                }
                None => return Err(ForgeError::missing("filter")),
            };
            Ok(ListMarketplacePurchasesResponse {
                purchases,
                page: next(nx),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_marketplace_publisher(
        &self,
        req: Request<GetMarketplacePublisherRequest>,
    ) -> std::result::Result<Response<MarketplacePublisher>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_account(&caller, &r.org).await?;
            self.apps.get_publisher(&caller.cx, &org).await
        }
        .await;
        reply(&caller, out)
    }

    async fn request_marketplace_publisher_verification(
        &self,
        req: Request<RequestMarketplacePublisherVerificationRequest>,
    ) -> std::result::Result<Response<MarketplacePublisher>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_account(&caller, &r.org).await?;
            let auth = self.manage_org(&caller, &org.id).await;
            let cx = self
                .svc
                .guarded(
                    &caller,
                    &org.id,
                    "",
                    "marketplace.publisher_verify_request",
                    auth,
                )
                .await?;
            self.apps
                .request_publisher_verification(&cx, &org, &r.domain, &r.support_email)
                .await
        }
        .await;
        reply(&caller, out)
    }
}
