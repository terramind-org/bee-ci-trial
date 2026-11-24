//! Apps platform: apps and manifests, installations and installation tokens, the OAuth
//! provider, user-to-server tokens and the marketplace (PRODUCT §4.20). Serves `AppService`.
//!
//! Owns the `apps` surface. Layout:
//!
//! | file | holds |
//! |---|---|
//! | `crypto.rs` | Ed25519 app keys, EdDSA JWTs, PKCE, base64 |
//! | `catalog.rs` | permission catalog (`resource: level` to actions and roles), OAuth scopes |
//! | `registry.rs` | apps, private keys, app JWT authentication |
//! | `installs.rs` | installations, installation tokens, the [`orgs::AccessSource`] |
//! | `hooks.rs` | per-app webhooks kept in sync over [`crate::events::webhooks::Webhooks`] |
//! | `oauth.rs` | OAuth apps, authorization-code and device flows, user-to-server tokens |
//! | `marketplace.rs` | listings, plans, purchases, publishers |
//! | `service.rs` | the `AppService` tonic service |
//!
//! # Design
//!
//! - **Tokens never exceed the installation.** An installation token is a Biscuit minted for
//!   the installation principal with an explicit repo id set (never an unrestricted scope:
//!   for an `ALL` installation the org's current repos are enumerated) and the actions of the
//!   granted permissions, then attenuated to the requested subset. Minted token ids are
//!   tracked, so suspending or deleting an installation (or deleting the app) revokes them;
//!   independently, the [`AppsAccess`] source grants the installation principal roles only on
//!   covered repos while the installation is active, so authorization re-checks the
//!   installation on every request.
//! - **Secrets.** Client secrets and OAuth codes and refresh tokens are stored as SHA-256;
//!   app private keys are never stored (public key and fingerprint only). The webhook secret
//!   is the one exception: it is the HMAC key, so it is stored (never returned) and copied to
//!   the webhook rows the delivery machinery signs with.
//! - **Webhooks** reuse [`crate::events::webhooks::Webhooks`]: an `ALL` installation owns one
//!   org hook, a `SELECTED` one a repo hook per selected repo (so events of other repos are
//!   never delivered). Hooks follow app settings and installation state (see `hooks.rs`).

pub mod catalog;
pub mod crypto;
mod hooks;
mod installs;
mod marketplace;
mod oauth;
mod registry;
mod service;

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{
    App, AppInstallation, AppPrivateKey, MarketplaceListing, MarketplacePlan, MarketplacePublisher,
    MarketplacePurchase, OAuthApp,
};
use prost::Message;

pub use installs::{InstallationToken, TokenRequest};
pub use marketplace::{
    ListingFilter, ListingInput, ListingPatch, ListingView, OrgDomainCheck, PlanInput, PlanPatch,
    PurchaseInput,
};
pub use oauth::{
    AuthorizationOutcome, AuthorizationRequest, DeviceFlowStart, DevicePoll, NewOAuthApp,
    OAuthAppPatch, RevokeTarget,
};
pub use registry::{AppPatch, NewApp};
pub use service::AppsService;

use crate::cx::Cx;
use crate::enterprise::common::{IpPolicy, SpendGate, TokenRevoker};
use crate::error::Result;
use crate::forge::Forge;
use crate::ids::ts;
use crate::module::{EventKind, ForgeModule};
use crate::store::{Db, Index, Migration, MigrationOp, Phase, Surface, TableDef};

/// The apps surface.
pub const SURFACE: Surface = Surface("apps");

macro_rules! table {
    ($name:ident, $table:literal, $uk1:expr, $uk2:expr, [$($idx:expr),*]) => {
        pub(crate) static $name: TableDef = TableDef {
            name: $table,
            surface: SURFACE,
            unique_k1: $uk1,
            unique_k2: $uk2,
            indexes: &[$($idx),*],
        };
    };
}

// Apps: id = app id; k1 = slug (unique); k2 = client id (unique); n1 = created (ms).
table!(APPS, "forge_apps_apps", true, true, [Index::ScopeN1]);
// Lookups: scope `owner:<owner key>` (id = app id) and scope `principal` (id = app principal
// id, k1 = app id).
table!(INDEX, "forge_apps_index", false, false, [Index::ScopeK1]);
// App public keys: scope = app id; id = key id; k1 = fingerprint (unique per app).
table!(KEYS, "forge_apps_keys", true, false, []);
// Installations: id = installation id; k1 = app id; k2 = target key; n1 = created (ms).
table!(
    INSTALLS,
    "forge_apps_installations",
    false,
    false,
    [Index::ScopeK1, Index::ScopeK2]
);
// Selected repos: scope = installation id; id = repo id; n1 = added (ms).
table!(
    INSTALL_REPOS,
    "forge_apps_installation_repos",
    false,
    false,
    [Index::ScopeN1]
);
// Webhook rows of an installation: scope = installation id; id = log key.
table!(HOOK_LINKS, "forge_apps_hook_links", false, false, []);
// Minted installation tokens: scope = installation id; id = token id; n1 = expiry (ms).
table!(
    TOKENS,
    "forge_apps_tokens",
    false,
    false,
    [Index::ScopeN1, Index::N1]
);
// OAuth apps: id; k1 = client id (unique); k2 = owner key.
table!(
    OAUTH_APPS,
    "forge_apps_oauth_apps",
    true,
    false,
    [Index::ScopeK1, Index::ScopeK2]
);
// Authorization codes: id = SHA-256(code); n1 = expiry (ms).
table!(
    CODES,
    "forge_apps_oauth_codes",
    false,
    false,
    [Index::ScopeN1]
);
// Grants: scope = principal id; id = client id; k1 = client id.
table!(GRANTS, "forge_apps_oauth_grants", false, false, [Index::K1]);
// OAuth access tokens: scope = principal id; id = token id; k1 = client id; k2 = family;
// n1 = expiry (ms).
table!(
    OTOKENS,
    "forge_apps_oauth_tokens",
    false,
    false,
    [Index::ScopeK1, Index::ScopeK2, Index::N1]
);
// Refresh tokens: id = SHA-256(token); k1 = family; k2 = `<principal>/<client>`; n1 = expiry.
table!(
    REFRESH,
    "forge_apps_oauth_refresh",
    false,
    false,
    [Index::ScopeK1, Index::ScopeK2, Index::ScopeN1]
);
// Device flows: id = SHA-256(device code); k1 = user code (unique); n1 = expiry (ms).
table!(
    DEVICE,
    "forge_apps_device_flows",
    true,
    false,
    [Index::ScopeN1]
);
// Marketplace listings: id; k1 = slug (unique); k2 = publisher org id; n1 = created (ms).
table!(
    LISTINGS,
    "forge_apps_listings",
    true,
    false,
    [Index::ScopeK2, Index::ScopeN1]
);
// Plans: scope = listing id; id = plan id; n1 = created (ms).
table!(PLANS, "forge_apps_plans", false, false, [Index::ScopeN1]);
// Purchases: scope = buyer org id; id; k1 = listing id; k2 = plan id; n1 = created (ms).
table!(
    PURCHASES,
    "forge_apps_purchases",
    false,
    false,
    [Index::ScopeK1, Index::K1, Index::K2]
);
// Publisher profiles: id = org id.
table!(PUBLISHERS, "forge_apps_publishers", false, false, []);

static M1: Migration = Migration {
    module: "apps",
    version: 1,
    name: "create_apps_tables",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&APPS),
        MigrationOp::CreateTable(&INDEX),
        MigrationOp::CreateTable(&KEYS),
        MigrationOp::CreateTable(&INSTALLS),
        MigrationOp::CreateTable(&INSTALL_REPOS),
        MigrationOp::CreateTable(&HOOK_LINKS),
        MigrationOp::CreateTable(&TOKENS),
        MigrationOp::CreateTable(&OAUTH_APPS),
        MigrationOp::CreateTable(&CODES),
        MigrationOp::CreateTable(&GRANTS),
        MigrationOp::CreateTable(&OTOKENS),
        MigrationOp::CreateTable(&REFRESH),
        MigrationOp::CreateTable(&DEVICE),
        MigrationOp::CreateTable(&LISTINGS),
        MigrationOp::CreateTable(&PLANS),
        MigrationOp::CreateTable(&PURCHASES),
        MigrationOp::CreateTable(&PUBLISHERS),
    ],
};

/// What the apps module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "apps",
    surfaces: &[SURFACE],
    migrations: &[&M1],
    event_kinds: &[
        EventKind::gh(
            "app.installed",
            "An app was installed",
            "installation",
            "created",
        ),
        EventKind::gh(
            "app.uninstalled",
            "An app was uninstalled",
            "installation",
            "deleted",
        ),
        EventKind::gh(
            "app.suspended",
            "An app installation was suspended",
            "installation",
            "suspend",
        ),
        EventKind::gh(
            "app.unsuspended",
            "An app installation was unsuspended",
            "installation",
            "unsuspend",
        ),
        EventKind::gh(
            "marketplace.purchased",
            "A marketplace plan was purchased or changed",
            "marketplace_purchase",
            "purchased",
        ),
        EventKind::gh(
            "marketplace.cancelled",
            "A marketplace purchase was cancelled",
            "marketplace_purchase",
            "cancelled",
        ),
    ],
    audit_prefixes: &["app.", "oauth.", "marketplace."],
};

// ---------- stored records ----------

/// An app and its secret hashes.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct AppRec {
    /// The public record (no private keys; they live in `KEYS`).
    #[prost(message, optional, tag = "1")]
    pub app: Option<App>,
    /// `org:<id>` or `user:<principal id>`.
    #[prost(string, tag = "2")]
    pub owner_key: String,
    /// SHA-256 of the client secret.
    #[prost(string, tag = "3")]
    pub client_secret_hash: String,
    /// The webhook HMAC key (stored because signing needs it; never returned).
    #[prost(string, tag = "4")]
    pub webhook_secret: String,
}

/// One app public key.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct KeyRec {
    /// Summary shown to clients.
    #[prost(message, optional, tag = "1")]
    pub key: Option<AppPrivateKey>,
    /// The 32-byte Ed25519 public key.
    #[prost(bytes = "vec", tag = "2")]
    pub public_key: Vec<u8>,
}

/// An installation.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct InstallRec {
    /// The public record.
    #[prost(message, optional, tag = "1")]
    pub inst: Option<AppInstallation>,
    /// `org:<id>` or `user:<principal id>`.
    #[prost(string, tag = "2")]
    pub target_key: String,
    /// Org id or user principal id.
    #[prost(string, tag = "3")]
    pub target_id: String,
    /// Why it was suspended.
    #[prost(string, tag = "4")]
    pub suspend_reason: String,
}

/// The webhook an installation owns on one log.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct HookLink {
    /// Id of the row in the webhooks surface.
    #[prost(string, tag = "1")]
    pub hook_id: String,
    /// Org id of the hook's owner.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Repo id (empty for the org hook).
    #[prost(string, tag = "3")]
    pub repo_id: String,
    /// Hash of the settings the hook was last written with.
    #[prost(string, tag = "4")]
    pub fingerprint: String,
}

/// A tracked token.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct TokenRec {
    /// Expiry in ms (also the row's `n1`).
    #[prost(uint64, tag = "1")]
    pub expires_ms: u64,
}

/// An OAuth app and its secret hash.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct OAuthAppRec {
    /// The public record.
    #[prost(message, optional, tag = "1")]
    pub app: Option<OAuthApp>,
    /// `org:<id>` or `user:<id>`.
    #[prost(string, tag = "2")]
    pub owner_key: String,
    /// SHA-256 of the client secret.
    #[prost(string, tag = "3")]
    pub client_secret_hash: String,
}

/// A pending authorization code.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct CodeRec {
    /// Client the code was issued to.
    #[prost(string, tag = "1")]
    pub client_id: String,
    /// The authorizing user.
    #[prost(string, tag = "2")]
    pub principal_id: String,
    /// The redirect URI used to ask.
    #[prost(string, tag = "3")]
    pub redirect_uri: String,
    /// Granted scopes.
    #[prost(string, repeated, tag = "4")]
    pub scopes: Vec<String>,
    /// PKCE S256 challenge.
    #[prost(string, tag = "5")]
    pub challenge: String,
    /// Set once exchanged.
    #[prost(bool, tag = "6")]
    pub used: bool,
    /// Token family issued from this code.
    #[prost(string, tag = "7")]
    pub family: String,
    /// OpenID Connect nonce of the authorization request.
    #[prost(string, tag = "8")]
    pub nonce: String,
}

/// A refresh token.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct RefreshRec {
    /// Client.
    #[prost(string, tag = "1")]
    pub client_id: String,
    /// User.
    #[prost(string, tag = "2")]
    pub principal_id: String,
    /// Token family.
    #[prost(string, tag = "3")]
    pub family: String,
    /// Scopes of the grant.
    #[prost(string, repeated, tag = "4")]
    pub scopes: Vec<String>,
    /// Already exchanged (a second use is reuse).
    #[prost(bool, tag = "5")]
    pub rotated: bool,
    /// Issued to a client that proved itself without a secret (PKCE or device flow).
    #[prost(bool, tag = "6")]
    pub public_client: bool,
}

/// A device flow.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct DeviceRec {
    /// Client.
    #[prost(string, tag = "1")]
    pub client_id: String,
    /// Requested scopes.
    #[prost(string, repeated, tag = "2")]
    pub scopes: Vec<String>,
    /// `OAuthDeviceFlowStatus` as a number.
    #[prost(int32, tag = "3")]
    pub status: i32,
    /// The approving user.
    #[prost(string, tag = "4")]
    pub principal_id: String,
    /// Time of the last poll (ms).
    #[prost(uint64, tag = "5")]
    pub last_poll_ms: u64,
    /// Current minimum polling interval.
    #[prost(uint32, tag = "6")]
    pub interval_secs: u32,
}

/// A marketplace listing; plans live in `PLANS`.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ListingRec {
    /// The public record without plans.
    #[prost(message, optional, tag = "1")]
    pub listing: Option<MarketplaceListing>,
    /// Publisher org id.
    #[prost(string, tag = "2")]
    pub publisher_org_id: String,
    /// New purchases so far (plan changes do not count).
    #[prost(uint64, tag = "3")]
    pub purchase_count: u64,
    /// The reviewer's note.
    #[prost(string, tag = "4")]
    pub review_note: String,
}

/// A plan record.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct PlanRec {
    /// The public record.
    #[prost(message, optional, tag = "1")]
    pub plan: Option<MarketplacePlan>,
    /// Creation time (ms), for stable ordering.
    #[prost(uint64, tag = "2")]
    pub created_ms: u64,
}

/// A purchase record.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct PurchaseRec {
    /// The public record (`org` holds the buyer org id until views resolve the name).
    #[prost(message, optional, tag = "1")]
    pub purchase: Option<MarketplacePurchase>,
}

/// A publisher profile.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct PublisherRec {
    /// The public record (`org` holds the org id until views resolve the name).
    #[prost(message, optional, tag = "1")]
    pub publisher: Option<MarketplacePublisher>,
}

// ---------- hooks other domains implement ----------

/// Asks the SSO domain whether a user may use orgs that enforce SAML. The parent implements
/// it; without one, SSO is never required.
#[async_trait]
pub trait SsoGate: Send + Sync {
    /// `SSO_REQUIRED` when an org in `org_ids` enforces SAML and `principal_id` has no
    /// active SSO session for it.
    async fn check_sso(&self, cx: &Cx, principal_id: &str, org_ids: &[String]) -> Result<()>;
}

/// Asks the enterprise domain about verified domains and 2FA enforcement.
#[async_trait]
pub trait DomainCheck: Send + Sync {
    /// Whether `domain` is a verified domain of the org.
    async fn is_verified(&self, cx: &Cx, org_id: &str, domain: &str) -> Result<bool>;
    /// Whether the org enforces two-factor authentication for its members.
    async fn two_factor_required(&self, cx: &Cx, org_id: &str) -> Result<bool>;
}

/// Marketplace settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarketplaceConfig {
    /// Publish at once when a publisher submits a listing (self-hosted default). When false,
    /// listings wait in `PENDING_REVIEW` for [`Apps::review_listing`].
    pub auto_approve: bool,
}

impl Default for MarketplaceConfig {
    fn default() -> MarketplaceConfig {
        MarketplaceConfig { auto_approve: true }
    }
}

/// Settings and collaborators of the apps platform.
#[derive(Clone)]
pub struct AppsConfig {
    /// Keys that sign access tokens (the same ring the server verifies with).
    pub keys: Arc<dyn crate::TokenIssuer>,
    /// Revokes token ids (the server's revocation list).
    pub revoker: Arc<dyn TokenRevoker>,
    /// IP allow lists consulted when an installation token is minted.
    pub ip_policy: Option<Arc<dyn IpPolicy>>,
    /// Billing gate consulted for paid marketplace purchases.
    pub spend_gate: Option<Arc<dyn SpendGate>>,
    /// SSO gate consulted when a user authorizes an OAuth client.
    pub sso: Option<Arc<dyn SsoGate>>,
    /// Verified-domain and 2FA lookups for publisher verification (default: the org's
    /// `verified_domains`, 2FA assumed enforced).
    pub domains: Option<Arc<dyn DomainCheck>>,
    /// Marketplace settings.
    pub marketplace: MarketplaceConfig,
    /// OAuth access token lifetime in seconds (default 8 hours).
    pub access_token_ttl_secs: u64,
    /// OAuth refresh token lifetime in seconds (default 180 days).
    pub refresh_token_ttl_secs: u64,
    /// Where users type a device code.
    pub verification_uri: String,
}

impl AppsConfig {
    /// A config with the required collaborators and defaults for the rest.
    pub fn new(keys: Arc<dyn crate::TokenIssuer>, revoker: Arc<dyn TokenRevoker>) -> AppsConfig {
        AppsConfig {
            keys,
            revoker,
            ip_policy: None,
            spend_gate: None,
            sso: None,
            domains: None,
            marketplace: MarketplaceConfig::default(),
            access_token_ttl_secs: 8 * 3600,
            refresh_token_ttl_secs: 180 * 24 * 3600,
            verification_uri: "https://beehive.example/login/device".into(),
        }
    }

    /// Sets the IP policy.
    pub fn with_ip_policy(mut self, p: Arc<dyn IpPolicy>) -> AppsConfig {
        self.ip_policy = Some(p);
        self
    }

    /// Sets the spend gate.
    pub fn with_spend_gate(mut self, g: Arc<dyn SpendGate>) -> AppsConfig {
        self.spend_gate = Some(g);
        self
    }

    /// Sets the SSO gate.
    pub fn with_sso(mut self, g: Arc<dyn SsoGate>) -> AppsConfig {
        self.sso = Some(g);
        self
    }

    /// Sets the domain check.
    pub fn with_domains(mut self, d: Arc<dyn DomainCheck>) -> AppsConfig {
        self.domains = Some(d);
        self
    }

    /// Sets the marketplace settings.
    pub fn with_marketplace(mut self, m: MarketplaceConfig) -> AppsConfig {
        self.marketplace = m;
        self
    }
}

/// Whose an app or installation is: an org or a user account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// Org or user.
    pub kind: AccountKind,
    /// Org id, or the user's principal id.
    pub id: String,
    /// Org name, or the user's login.
    pub name: String,
}

/// Kind of [`Account`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountKind {
    /// An org.
    Org,
    /// A user.
    User,
}

impl Account {
    /// An org account.
    pub fn org(id: &str, name: &str) -> Account {
        Account {
            kind: AccountKind::Org,
            id: id.into(),
            name: name.into(),
        }
    }

    /// A user account.
    pub fn user(id: &str, login: &str) -> Account {
        Account {
            kind: AccountKind::User,
            id: id.into(),
            name: login.into(),
        }
    }

    /// `org:<id>` or `user:<id>`.
    pub fn key(&self) -> String {
        match self.kind {
            AccountKind::Org => format!("org:{}", self.id),
            AccountKind::User => format!("user:{}", self.id),
        }
    }

    /// The org id, empty for a user (audit and events of user accounts go to the system log).
    pub fn org_id(&self) -> &str {
        match self.kind {
            AccountKind::Org => &self.id,
            AccountKind::User => "",
        }
    }
}

/// The apps store facade: apps, installations, OAuth and the marketplace.
pub struct Apps {
    pub(crate) forge: Arc<Forge>,
    pub(crate) db: Db,
    pub(crate) cfg: AppsConfig,
}

impl Apps {
    /// The apps store over `forge`. Call [`Apps::attach`] once to register the access source.
    pub fn new(forge: Arc<Forge>, cfg: AppsConfig) -> Arc<Apps> {
        Arc::new(Apps {
            db: forge.db.clone(),
            forge,
            cfg,
        })
    }

    /// Registers the [`AppsAccess`] source with the orgs module, so installation principals
    /// (and app principals acting for a user) hold repo roles on the repos they cover.
    pub fn attach(&self) {
        self.forge
            .orgs
            .add_access_source(Arc::new(AppsAccess::new(self.db.clone())));
    }

    pub(crate) fn now_ms(&self) -> u64 {
        self.db.now_ms()
    }

    pub(crate) fn now_secs(&self) -> u64 {
        self.db.now_ms() / 1000
    }

    pub(crate) fn rand_hex(&self, bytes: usize) -> Result<String> {
        self.db.ids().random_hex(bytes)
    }

    /// Resolves an org name or a user login to an [`Account`] (orgs win).
    pub async fn resolve_account(&self, cx: &Cx, name: &str) -> Result<Account> {
        match self.forge.orgs.get_org(cx, name).await {
            Ok(o) => Ok(Account::org(&o.id, &o.name)),
            Err(e) if e.reason == bee_proto::v1::ErrorReason::NotFound => {
                match self.forge.hooks.directory.by_login(name).await {
                    Some(p) => Ok(Account::user(&p.id, name)),
                    None => Err(crate::error::ForgeError::not_found(format!(
                        "account {name}"
                    ))),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// The user account of a principal id (login from the directory, else the id).
    pub async fn user_account(&self, principal_id: &str) -> Account {
        let (logins, _) = self.forge.hooks.directory.identities(principal_id).await;
        let login = logins
            .into_iter()
            .next()
            .unwrap_or_else(|| principal_id.to_string());
        Account::user(principal_id, &login)
    }

    /// Display name of the account behind an account key.
    pub(crate) async fn account_name(&self, cx: &Cx, key: &str) -> String {
        if let Some(id) = key.strip_prefix("org:") {
            self.forge
                .orgs
                .org_any(cx, id)
                .await
                .map(|o| o.name)
                .unwrap_or_else(|_| id.to_string())
        } else if let Some(id) = key.strip_prefix("user:") {
            self.user_account(id).await.name
        } else {
            key.to_string()
        }
    }
}

pub use installs::AppsAccess;

/// Timestamp helper for views.
pub(crate) fn tsm(ms: u64) -> Option<pbjson_types::Timestamp> {
    Some(ts(ms))
}
