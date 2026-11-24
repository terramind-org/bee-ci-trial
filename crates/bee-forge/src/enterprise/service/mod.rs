//! tonic implementations of `TeamService` and `EnterpriseService` over [`Enterprise`].
//!
//! Same shape as the core services (`docs/spec/forge-core.md` §7): authenticate, validate,
//! resolve and admit (IP allow list, SSO session, 2FA, blocked principals), authorize, call the
//! store with the authorized context, map errors to `ErrorDetail` statuses. Denied state-changing
//! calls are audited.
//!
//! | RPC group | who may |
//! |---|---|
//! | team reads | org members (secret teams: their members and org owners) |
//! | create team | org owners/admins; members when org settings (and the enterprise policy) allow; holders of the custom org permission `manage_teams`; maintainers of the parent team (child teams) |
//! | edit team, members, invitations, review assignment | org owners/admins, `manage_teams` holders, maintainers of the team or an ancestor |
//! | team repo access | repo admins |
//! | IdP group links | org owners/admins |
//! | SSO, SCIM, IP allow lists, domains, 2FA | owners/admins of the org; owners of the enterprise (and, for an org that belongs to one, the enterprise owners) |
//! | enterprise accounts, owners, policies | enterprise owners |

mod enterprise;
mod teams;

use std::sync::Arc;

use bee_policy::Action;
use bee_proto::v1::enterprise_service_server::EnterpriseServiceServer;
use bee_proto::v1::team_service_server::TeamServiceServer;
use bee_proto::v1::{EnterpriseScope, ErrorReason, Org, Role};

use super::Enterprise;
use super::common::{ResolvedScope, ScopeKind, ScopeNeed, ScopeResolver};
use super::network::AdmissionRequest;
use super::teams::Viewer;
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::service::ForgeServices;

/// The `TeamService` and `EnterpriseService` implementation.
#[derive(Clone)]
pub struct EnterpriseApi {
    pub(crate) svc: ForgeServices,
    pub(crate) ent: Arc<Enterprise>,
}

impl EnterpriseApi {
    /// The services over `ent`.
    pub fn new(ent: Arc<Enterprise>) -> EnterpriseApi {
        EnterpriseApi {
            svc: ForgeServices::new(ent.forge.clone()),
            ent,
        }
    }

    /// The `TeamService` server.
    pub fn team_server(&self) -> TeamServiceServer<EnterpriseApi> {
        TeamServiceServer::new(self.clone())
    }

    /// The `EnterpriseService` server.
    pub fn enterprise_server(&self) -> EnterpriseServiceServer<EnterpriseApi> {
        EnterpriseServiceServer::new(self.clone())
    }

    pub(crate) fn forge(&self) -> &Arc<Forge> {
        &self.ent.forge
    }

    /// Resolves an org and admits the caller to it.
    pub(crate) async fn org(&self, caller: &Caller, name: &str) -> Result<Org> {
        let org = self.forge().orgs.get_org(&caller.cx, name).await?;
        self.ent
            .admit_caller(caller, std::slice::from_ref(&org.id), false)
            .await?;
        Ok(org)
    }

    /// Who is looking, for secret team visibility.
    pub(crate) async fn viewer(&self, caller: &Caller, org_id: &str) -> Result<Viewer> {
        let mut owner = true;
        for id in &caller.cx.chain {
            match self.forge().orgs.member(&caller.cx, org_id, id).await? {
                Some(m) if m.role == Role::Owner as i32 => {}
                _ => owner = false,
            }
        }
        Ok(Viewer {
            ids: caller.cx.chain.clone(),
            org_owner: owner,
        })
    }

    /// Any org member may read.
    pub(crate) async fn view(&self, caller: &Caller, org_id: &str) -> Result<Cx> {
        self.forge()
            .authz
            .org(caller, org_id, OrgPerm::ViewMembers)
            .await
    }

    fn token_manages(caller: &Caller) -> Result<()> {
        if caller.token.scope.repos.is_some() || !caller.token.allows_action(Action::ManageSettings)
        {
            return Err(ForgeError::policy_denied(
                "token_scope",
                vec!["manage_settings".into()],
                "the token must be unrestricted by repo and allow `manage_settings`",
            ));
        }
        Ok(())
    }

    /// Owners and admins, or (when `team` is given) holders of `manage_teams` and maintainers
    /// of the team or an ancestor. Denials are audited as `action`.
    pub(crate) async fn team_admin(
        &self,
        caller: &Caller,
        org: &Org,
        team: Option<&str>,
        action: &str,
    ) -> Result<Cx> {
        let auth = self.team_admin_auth(caller, org, team).await;
        self.svc.guarded(caller, &org.id, "", action, auth).await
    }

    pub(crate) async fn team_admin_auth(
        &self,
        caller: &Caller,
        org: &Org,
        team: Option<&str>,
    ) -> Result<Cx> {
        match self
            .forge()
            .authz
            .org(caller, &org.id, OrgPerm::Manage)
            .await
        {
            Ok(cx) => return Ok(cx),
            Err(e) if e.reason == ErrorReason::PermissionDenied => {}
            Err(e) => return Err(e),
        }
        Self::token_manages(caller)?;
        let cx = &caller.cx;
        for id in &cx.chain {
            if self.forge().orgs.member(cx, &org.id, id).await?.is_none() {
                return Err(ForgeError::permission_denied(format!(
                    "{id} is not a member of {}",
                    org.name
                )));
            }
            let custom = self
                .ent
                .roles
                .has_org_permission(cx, &org.id, id, "manage_teams")
                .await?;
            let maintainer = match team {
                Some(t) => self.ent.teams.is_maintainer(cx, &org.id, t, id).await?,
                None => false,
            };
            if !custom && !maintainer {
                return Err(ForgeError::permission_denied(format!(
                    "{id} cannot manage teams in {}",
                    org.name
                )));
            }
        }
        let mut out = cx.clone();
        out.decision = "token:ok team_manager".into();
        Ok(out)
    }

    /// Resolves an `EnterpriseScope`, admits the caller and authorizes `need`. Denials are
    /// audited as `action`.
    pub(crate) async fn scope(
        &self,
        caller: &Caller,
        scope: &Option<EnterpriseScope>,
        need: ScopeNeed,
        action: &str,
    ) -> Result<(ResolvedScope, Cx)> {
        let rs = self.ent.accounts.resolve(&caller.cx, scope).await?;
        let orgs: Vec<String> = match rs.kind {
            ScopeKind::Org => rs.org_ids.clone(),
            ScopeKind::Enterprise => Vec::new(),
        };
        self.ent
            .network
            .admit(
                &caller.cx,
                &AdmissionRequest {
                    principals: caller.cx.chain.clone(),
                    org_ids: orgs,
                    ip: caller.cx.ip.clone(),
                    app_token: false,
                },
            )
            .await?;
        let auth = self.ent.accounts.authorize(caller, &rs, need).await;
        let audit_org = match rs.kind {
            ScopeKind::Org => rs.id.clone(),
            ScopeKind::Enterprise => String::new(),
        };
        let cx = self
            .svc
            .guarded(caller, &audit_org, "", action, auth)
            .await?;
        Ok((rs, cx))
    }
}
