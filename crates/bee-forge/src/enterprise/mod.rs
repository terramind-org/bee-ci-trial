//! Org enterprise features: teams, custom roles, enterprise accounts, SSO and SCIM, apps and
//! the marketplace, billing, models and moderation (PRODUCT §4.20, §4.21). Serves
//! `TeamService`, `AppService`, `EnterpriseService`, `BillingService`, `ModerationService`
//! and `ModelService`. The design is in `docs/spec/orgs-apps.md`.
//!
//! # Layout
//!
//! Each domain is a forge module with its own surface (failure domain), tables, migrations,
//! event kinds and audit prefixes:
//!
//! | module | surface | holds |
//! |---|---|---|
//! | [`MODULE`] (this one) | `enterprise` | teams, custom roles, enterprise accounts, IP allow lists, domains, 2FA, SAML SSO, SCIM |
//! | [`apps`] | `apps` | apps, installations, OAuth, marketplace |
//! | [`billing`] | `billing` | plans, usage, budgets, invoices, cost centers |
//! | [`models`] | `models` | model catalog, provider keys, prompt index, evals |
//! | [`moderation`] | `moderation` | blocks, reports, limits, locks, flood defenses, takedowns |
//!
//! Teams and apps grant repo access by implementing [`crate::orgs::AccessSource`] and
//! registering with [`crate::orgs::Orgs::add_access_source`]. Cross-domain calls go through the
//! traits in [`common`].
//!
//! # Wiring
//!
//! [`Enterprise::new`] builds the identity side (teams, roles, accounts, network controls, SSO,
//! SCIM) over an opened [`crate::Forge`]; [`Enterprise::attach`] registers its access source and
//! event listeners; [`Enterprise::register`] adds the `TeamService` and `EnterpriseService`
//! servers to a tonic router. The server must call [`Enterprise::admit`] for every
//! authenticated request (IP allow lists, SSO sessions, 2FA, blocked principals).

// Store methods take their request fields flat, like the orgs module.
#![allow(clippy::too_many_arguments)]

pub mod accounts;
pub mod apps;
pub mod billing;
pub mod common;
pub mod integration;
pub mod models;
pub mod moderation;
pub mod network;
pub mod roles;
pub mod saml;
pub mod scim;
pub mod scim_filter;
pub mod service;
pub mod sso;
pub mod sync;
pub mod teams;

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::Event;

use crate::auth::Caller;
use crate::cx::Cx;
use crate::error::Result;
use crate::events::EventListener;
use crate::forge::Forge;
use crate::module::{EventKind, ForgeModule};
use crate::store::Surface;

use self::accounts::Accounts;
use self::common::ScopeResolver;
use self::network::{
    AdmissionRequest, DnsResolver, MemoryTwoFactor, Network, StaticResolver, TwoFactorDirectory,
};
use self::roles::Roles;
use self::scim::Scim;
use self::sso::Sso;
use self::teams::{TeamAccess, Teams};

/// The enterprise surface.
pub const SURFACE: Surface = Surface("enterprise");

/// What the enterprise module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "enterprise",
    surfaces: &[SURFACE],
    migrations: &[
        &teams::M_TEAMS,
        &roles::M_ROLES,
        &accounts::M_ACCOUNTS,
        &network::M_NETWORK,
        &sso::M_SSO,
        &scim::M_SCIM,
    ],
    event_kinds: &[
        EventKind::gh("team.created", "A team was created", "team", "created"),
        EventKind::gh(
            "team.member_added",
            "A member joined a team",
            "membership",
            "added",
        ),
        EventKind::new("team.updated", "A team was changed"),
        EventKind::new("team.deleted", "A team was deleted"),
        EventKind::new("team.member_role_changed", "A team member's role changed"),
        EventKind::new("team.member_removed", "A member left a team"),
        EventKind::new("team.invitation_created", "A team invitation was created"),
        EventKind::new(
            "team.invitation_cancelled",
            "A team invitation was cancelled",
        ),
        EventKind::new("team.repo_granted", "A team was granted access to a repo"),
        EventKind::new("team.repo_revoked", "A team's repo access was removed"),
        EventKind::new(
            "team.review_assignment_updated",
            "A team's review assignment settings changed",
        ),
        EventKind::new("team.group_linked", "An IdP group was linked to a team"),
        EventKind::new(
            "team.group_unlinked",
            "An IdP group was unlinked from a team",
        ),
        EventKind::new(
            "team.synced",
            "A team's membership was synced from its IdP groups",
        ),
        EventKind::new("role.created", "A custom role was created"),
        EventKind::new("role.updated", "A custom role was changed"),
        EventKind::new("role.deleted", "A custom role was deleted"),
        EventKind::new("role.assigned", "A custom org role was assigned"),
        EventKind::new("role.unassigned", "A custom org role was unassigned"),
        EventKind::new("enterprise.org_added", "An org joined an enterprise"),
        EventKind::new("enterprise.org_removed", "An org left an enterprise"),
        EventKind::new("audit_sink.failing", "An audit sink stopped delivering"),
    ],
    audit_prefixes: &[
        "team.",
        "role.",
        "enterprise.",
        "audit_sink.",
        "audit_export.",
    ],
};

/// Settings of the identity side.
#[derive(Clone)]
pub struct EnterpriseConfig {
    /// Public URL of this deployment (SP entity ids, ACS and SCIM URLs are built from it).
    pub base_url: String,
    /// Resolver for domain verification challenges.
    pub dns: Arc<dyn DnsResolver>,
    /// Source of two-factor facts.
    pub two_factor: Arc<dyn TwoFactorDirectory>,
}

impl EnterpriseConfig {
    /// A config with an empty static resolver and a directory where nobody has two-factor
    /// (tests and `--dev`; the server supplies real ones).
    pub fn new(base_url: &str) -> EnterpriseConfig {
        EnterpriseConfig {
            base_url: base_url.to_string(),
            dns: Arc::new(StaticResolver::new()),
            two_factor: Arc::new(MemoryTwoFactor::new()),
        }
    }
}

/// The identity side of the enterprise features over one forge.
pub struct Enterprise {
    /// The forge.
    pub forge: Arc<Forge>,
    /// Custom roles.
    pub roles: Arc<Roles>,
    /// Teams.
    pub teams: Arc<Teams>,
    /// Enterprise accounts (also the full [`ScopeResolver`]).
    pub accounts: Arc<Accounts>,
    /// IP allow lists, domains, 2FA and admission.
    pub network: Arc<Network>,
    /// SAML SSO.
    pub sso: Arc<Sso>,
    /// SCIM provisioning.
    pub scim: Arc<Scim>,
}

impl Enterprise {
    /// Builds the identity side over `forge`.
    pub fn new(forge: Arc<Forge>, cfg: EnterpriseConfig) -> Arc<Enterprise> {
        let roles = Arc::new(Roles::new(forge.clone()));
        let teams = Arc::new(Teams::new(forge.clone()));
        let accounts = Arc::new(Accounts::new(forge.clone()));
        let network = Arc::new(Network::new(
            forge.clone(),
            accounts.clone(),
            cfg.dns.clone(),
            cfg.two_factor.clone(),
        ));
        let sso = Arc::new(Sso::new(forge.clone(), &cfg.base_url));
        let resolver: Arc<dyn ScopeResolver> = accounts.clone();
        let scim = Arc::new(Scim::new(
            forge.clone(),
            resolver,
            accounts.clone(),
            sso.clone(),
            teams.clone(),
            &cfg.base_url,
        ));
        Arc::new(Enterprise {
            forge,
            roles,
            teams,
            accounts,
            network,
            sso,
            scim,
        })
    }

    /// Registers the team access source (so [`crate::orgs::Orgs::effective_role`] reports team
    /// grants) and the org membership listener (members who leave an org leave its teams;
    /// members who join take up their team invitations).
    pub fn attach(self: &Arc<Self>) {
        self.forge
            .orgs
            .add_access_source(Arc::new(TeamAccess::new(&self.forge)));
        self.forge
            .events
            .add_listener(Arc::new(MembershipListener { ent: self.clone() }));
    }

    /// Admits or refuses a request. See [`network::Network::admit`] for the checks; the server
    /// calls this once per authenticated request with every org the request touches.
    pub async fn admit(&self, cx: &Cx, req: &AdmissionRequest) -> Result<()> {
        self.network.admit(cx, req).await
    }

    /// [`Enterprise::admit`] for a verified caller touching `org_ids`.
    pub async fn admit_caller(
        &self,
        caller: &Caller,
        org_ids: &[String],
        app_token: bool,
    ) -> Result<()> {
        self.network
            .admit(
                &caller.cx,
                &AdmissionRequest {
                    principals: caller.cx.chain.clone(),
                    org_ids: org_ids.to_vec(),
                    ip: caller.cx.ip.clone(),
                    app_token,
                },
            )
            .await
    }

    /// Housekeeping: expired SSO requests, sessions and replay-cache entries. Call from the
    /// maintenance worker.
    pub async fn maintain(&self) -> Result<usize> {
        self.sso.sweep(&Cx::system()).await
    }

    /// Adds the `TeamService` and `EnterpriseService` servers to a tonic router builder.
    pub fn register(self: &Arc<Self>, routes: &mut tonic::service::RoutesBuilder) {
        let api = service::EnterpriseApi::new(self.clone());
        routes.add_service(api.team_server());
        routes.add_service(api.enterprise_server());
    }
}

/// Keeps team membership consistent with org membership.
struct MembershipListener {
    ent: Arc<Enterprise>,
}

#[async_trait]
impl EventListener for MembershipListener {
    async fn on_event(&self, event: &Event) {
        if event.org_id.is_empty()
            || !matches!(event.kind.as_str(), "member.removed" | "member.added")
        {
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&event.payload_json) else {
            return;
        };
        let Some(pid) = v.get("principal_id").and_then(|p| p.as_str()) else {
            return;
        };
        let cx = Cx::system();
        if event.kind == "member.removed" {
            let _ = self
                .ent
                .teams
                .remove_principal_everywhere(&cx, std::slice::from_ref(&event.org_id), pid)
                .await;
        } else if let Ok(org) = self.ent.forge.orgs.org_any(&cx, &event.org_id).await {
            let (_, emails) = self.ent.forge.hooks.directory.identities(pid).await;
            let _ = self
                .ent
                .teams
                .resolve_pending(&cx, &org, pid, &emails)
                .await;
        }
    }
}
