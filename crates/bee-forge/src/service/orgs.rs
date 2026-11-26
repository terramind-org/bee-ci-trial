//! `OrgService`.

use bee_policy::Action;
use bee_proto::v1::org_service_server::OrgService;
use bee_proto::v1::{
    AcceptOrgInvitationRequest, AssignOrgRoleRequest, CancelOrgInvitationRequest,
    CancelOrgInvitationResponse, ConvertToOutsideCollaboratorRequest,
    ConvertToOutsideCollaboratorResponse, CreateOrgInvitationRequest, CreateOrgRequest,
    DeclineOrgInvitationRequest, DeclineOrgInvitationResponse, DeleteOrgRequest, DeleteOrgResponse,
    ErrorReason, GetOrgRequest, GetOrgSettingsRequest, InvitationState, ListMembersRequest,
    ListMembersResponse, ListOrgInvitationsRequest, ListOrgInvitationsResponse, ListOrgsRequest,
    ListOrgsResponse, ListOutsideCollaboratorsRequest, ListOutsideCollaboratorsResponse, Member,
    Org, OrgInvitation, OrgSettings, Principal, RenameOrgRequest, RepoRole, RestoreOrgRequest,
    RevokeOrgRoleRequest, Role, SetMemberRequest, UpdateOrgRequest, UpdateOrgSettingsRequest,
};
use tonic::{Request, Response, Status};

use super::{ForgeServices, next, page, reply};
use crate::auth::{Caller, OrgPerm};
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::orgs::{Caller as InviteCaller, Invitee, OrgPatch};

impl ForgeServices {
    /// A principal from the directory (`NOT_FOUND` if unknown).
    pub(crate) async fn principal(&self, id: &str) -> Result<Principal> {
        if id.is_empty() {
            return Err(ForgeError::missing("principal_id"));
        }
        self.forge
            .hooks
            .directory
            .get(id)
            .await
            .ok_or_else(|| ForgeError::not_found(format!("principal {id}")))
    }

    async fn invite_caller(&self, caller: &Caller) -> InviteCaller {
        let (logins, emails) = self
            .forge
            .hooks
            .directory
            .identities(&caller.cx.actor_id)
            .await;
        let principal = self
            .forge
            .hooks
            .directory
            .get(&caller.cx.actor_id)
            .await
            .unwrap_or_else(|| caller.principal());
        InviteCaller {
            principal,
            logins,
            emails,
        }
    }

    /// `Own` when the change touches an owner, else `Manage`.
    async fn member_perm(
        &self,
        caller: &Caller,
        org_id: &str,
        principal_id: &str,
        role: Role,
    ) -> Result<OrgPerm> {
        let target_is_owner = self
            .forge
            .orgs
            .member(&caller.cx, org_id, principal_id)
            .await?
            .is_some_and(|m| m.role == Role::Owner as i32);
        Ok(if role == Role::Owner || target_is_owner {
            OrgPerm::Own
        } else {
            OrgPerm::Manage
        })
    }

    fn token_reads(caller: &Caller) -> Result<()> {
        if caller.token.allows_action(Action::Read) {
            Ok(())
        } else {
            Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "token does not allow `read`",
            ))
        }
    }
}

#[tonic::async_trait]
impl OrgService for ForgeServices {
    async fn create_org(&self, req: Request<CreateOrgRequest>) -> Result<Response<Org>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            if caller.token.scope.repos.is_some()
                || !caller.token.allows_action(Action::ManageSettings)
            {
                return Err(ForgeError::new(
                    ErrorReason::PolicyDenied,
                    "creating an org needs an unrestricted token that allows `manage_settings`",
                ));
            }
            let mut cx = caller.cx.clone();
            cx.decision = "token:ok any principal may create an org".into();
            let creator = self
                .forge
                .hooks
                .directory
                .get(&cx.actor_id)
                .await
                .unwrap_or_else(|| caller.principal());
            let org = self
                .forge
                .orgs
                .create_org(&cx, &r.name, &r.display, creator)
                .await?;
            self.forge.hooks.provisioner.create_org(&org).await?;
            Ok(org)
        }
        .await;
        reply(&caller, out)
    }

    async fn get_org(&self, req: Request<GetOrgRequest>) -> Result<Response<Org>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            Self::token_reads(&caller)?;
            self.forge.orgs.get_org(&caller.cx, &r.name_or_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_members(
        &self,
        req: Request<ListMembersRequest>,
    ) -> Result<Response<ListMembersResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            Self::token_reads(&caller)?;
            let org_id = self.org_id(&caller, &r.org).await?;
            let is_member = self
                .forge
                .authz
                .org(&caller, &org_id, OrgPerm::ViewMembers)
                .await
                .is_ok();
            let (after, size) = page(&r.page);
            let (members, token) = self
                .forge
                .orgs
                .members(&caller.cx, &org_id, &after, size)
                .await?;
            let members = members
                .into_iter()
                .filter(|m| is_member || m.public_membership)
                .collect();
            Ok(ListMembersResponse {
                members,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_member(&self, req: Request<SetMemberRequest>) -> Result<Response<Member>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let role = Role::try_from(r.role).unwrap_or(Role::Unspecified);
            let perm = self
                .member_perm(&caller, &org_id, &r.principal_id, role)
                .await?;
            let auth = self.forge.authz.org(&caller, &org_id, perm).await;
            let cx = self
                .guarded(&caller, &org_id, "", "member.set", auth)
                .await?;
            let principal = match self
                .forge
                .orgs
                .member(&cx, &org_id, &r.principal_id)
                .await?
            {
                Some(m) if role == Role::Unspecified => m.principal.unwrap_or_default(),
                _ => self.principal(&r.principal_id).await?,
            };
            Ok(self
                .forge
                .orgs
                .set_member(&cx, &org_id, principal.clone(), role)
                .await?
                .unwrap_or(Member {
                    principal: Some(principal),
                    ..Default::default()
                }))
        }
        .await;
        reply(&caller, out)
    }

    async fn list_orgs(
        &self,
        req: Request<ListOrgsRequest>,
    ) -> Result<Response<ListOrgsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            Self::token_reads(&caller)?;
            let who = if r.principal_id.is_empty() {
                caller.cx.actor_id.clone()
            } else {
                r.principal_id.clone()
            };
            let (after, size) = page(&r.page);
            let (orgs, token) = self
                .forge
                .orgs
                .orgs_of(&caller.cx, &who, &after, size)
                .await?;
            let mut visible = Vec::new();
            for o in orgs {
                let public = who == caller.cx.actor_id
                    || self
                        .forge
                        .orgs
                        .member(&caller.cx, &o.id, &who)
                        .await?
                        .is_some_and(|m| m.public_membership)
                    || self
                        .forge
                        .orgs
                        .member(&caller.cx, &o.id, &caller.cx.actor_id)
                        .await?
                        .is_some();
                if public {
                    visible.push(o);
                }
            }
            Ok(ListOrgsResponse {
                orgs: visible,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_org(&self, req: Request<UpdateOrgRequest>) -> Result<Response<Org>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let auth = self
                .forge
                .authz
                .org(&caller, &org_id, OrgPerm::Manage)
                .await;
            let cx = self
                .guarded(&caller, &org_id, "", "org.update", auth)
                .await?;
            let patch = OrgPatch {
                display: r.display,
                description: r.description,
                website: r.website,
                email: r.email,
                location: r.location,
            };
            self.forge
                .orgs
                .update_org(&cx, &org_id, patch, r.expected_version)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn rename_org(&self, req: Request<RenameOrgRequest>) -> Result<Response<Org>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let auth = self.forge.authz.org(&caller, &org_id, OrgPerm::Own).await;
            let cx = self
                .guarded(&caller, &org_id, "", "org.rename", auth)
                .await?;
            self.forge.orgs.rename_org(&cx, &org_id, &r.new_name).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_org(
        &self,
        req: Request<DeleteOrgRequest>,
    ) -> Result<Response<DeleteOrgResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let auth = self.forge.authz.org(&caller, &org_id, OrgPerm::Own).await;
            let cx = self
                .guarded(&caller, &org_id, "", "org.delete", auth)
                .await?;
            let until = self
                .forge
                .orgs
                .delete_org(&cx, &org_id, &r.confirm_name)
                .await?;
            Ok(DeleteOrgResponse {
                restorable_until: Some(ts(until)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn restore_org(&self, req: Request<RestoreOrgRequest>) -> Result<Response<Org>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.forge.orgs.org_any(&caller.cx, &r.org_id).await?;
            let auth = self.forge.authz.org(&caller, &org.id, OrgPerm::Own).await;
            let cx = self
                .guarded(&caller, &org.id, "", "org.restore", auth)
                .await?;
            self.forge.orgs.restore_org(&cx, &org.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_org_settings(
        &self,
        req: Request<GetOrgSettingsRequest>,
    ) -> Result<Response<OrgSettings>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let cx = self
                .forge
                .authz
                .org(&caller, &org_id, OrgPerm::ViewMembers)
                .await?;
            self.forge.orgs.settings(&cx, &org_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_org_settings(
        &self,
        req: Request<UpdateOrgSettingsRequest>,
    ) -> Result<Response<OrgSettings>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let auth = self
                .forge
                .authz
                .org(&caller, &org_id, OrgPerm::Manage)
                .await;
            let cx = self
                .guarded(&caller, &org_id, "", "org.settings_update", auth)
                .await?;
            if let Some(b) = r.base_repo_role
                && RepoRole::try_from(b).unwrap_or(RepoRole::Unspecified) == RepoRole::Unspecified
            {
                return Err(ForgeError::invalid(
                    "invalid_setting",
                    "base_repo_role must be set to a role",
                ));
            }
            let expected = r.expected_version;
            self.forge
                .orgs
                .update_settings(&cx, &org_id, expected, move |s| {
                    if let Some(v) = r.base_repo_role {
                        s.base_repo_role = v;
                    }
                    if let Some(v) = r.members_can_create_repos {
                        s.members_can_create_repos = v;
                    }
                    if let Some(v) = r.members_can_create_public_repos {
                        s.members_can_create_public_repos = v;
                    }
                    if let Some(v) = r.members_can_fork_private_repos {
                        s.members_can_fork_private_repos = v;
                    }
                    if let Some(v) = r.members_can_delete_repos {
                        s.members_can_delete_repos = v;
                    }
                    if let Some(v) = r.members_can_invite_outside_collaborators {
                        s.members_can_invite_outside_collaborators = v;
                    }
                    if let Some(v) = r.members_can_create_teams {
                        s.members_can_create_teams = v;
                    }
                    if let Some(v) = r.web_commit_signoff_required {
                        s.web_commit_signoff_required = v;
                    }
                    if let Some(v) = r.readme_repo_id {
                        s.readme_repo_id = v;
                    }
                    if r.set_pinned_repo_ids {
                        s.pinned_repo_ids = r.pinned_repo_ids;
                    }
                    if let Some(v) = r.max_open_changes_per_agent {
                        s.max_open_changes_per_agent = v;
                    }
                })
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_org_invitation(
        &self,
        req: Request<CreateOrgInvitationRequest>,
    ) -> Result<Response<OrgInvitation>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let role = Role::try_from(r.role).unwrap_or(Role::Member);
            let perm = if role == Role::Owner {
                OrgPerm::Own
            } else {
                OrgPerm::Manage
            };
            let auth = self.forge.authz.org(&caller, &org_id, perm).await;
            let cx = self
                .guarded(&caller, &org_id, "", "org_invitation.create", auth)
                .await?;
            let principal = if !r.invitee_principal_id.is_empty() {
                Some(self.principal(&r.invitee_principal_id).await?)
            } else if !r.invitee_login.is_empty() {
                self.forge.hooks.directory.by_login(&r.invitee_login).await
            } else {
                None
            };
            let invitee = Invitee {
                principal,
                login: r.invitee_login,
                email: r.invitee_email,
            };
            self.forge
                .orgs
                .invite(&cx, &org_id, invitee, role, r.team_ids)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_org_invitations(
        &self,
        req: Request<ListOrgInvitationsRequest>,
    ) -> Result<Response<ListOrgInvitationsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let cx = self
                .forge
                .authz
                .org(&caller, &org_id, OrgPerm::Manage)
                .await?;
            let (after, size) = page(&r.page);
            let state = InvitationState::try_from(r.state).unwrap_or(InvitationState::Unspecified);
            let (invitations, token) = self
                .forge
                .orgs
                .invitations(&cx, &org_id, state, &after, size)
                .await?;
            Ok(ListOrgInvitationsResponse {
                invitations,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn cancel_org_invitation(
        &self,
        req: Request<CancelOrgInvitationRequest>,
    ) -> Result<Response<CancelOrgInvitationResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let auth = self
                .forge
                .authz
                .org(&caller, &org_id, OrgPerm::Manage)
                .await;
            let cx = self
                .guarded(&caller, &org_id, "", "org_invitation.cancel", auth)
                .await?;
            self.forge
                .orgs
                .cancel_invitation(&cx, &org_id, &r.invitation_id)
                .await?;
            Ok(CancelOrgInvitationResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn accept_org_invitation(
        &self,
        req: Request<AcceptOrgInvitationRequest>,
    ) -> Result<Response<Member>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            Self::token_reads(&caller)?;
            let mut cx = caller.cx.clone();
            cx.decision = "invitee accepts own invitation".into();
            let who = self.invite_caller(&caller).await;
            self.forge
                .orgs
                .accept_invitation(&cx, &r.invitation_id, &who)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn decline_org_invitation(
        &self,
        req: Request<DeclineOrgInvitationRequest>,
    ) -> Result<Response<DeclineOrgInvitationResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            Self::token_reads(&caller)?;
            let mut cx = caller.cx.clone();
            cx.decision = "invitee declines own invitation".into();
            let who = self.invite_caller(&caller).await;
            self.forge
                .orgs
                .decline_invitation(&cx, &r.invitation_id, &who)
                .await?;
            Ok(DeclineOrgInvitationResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn assign_org_role(
        &self,
        req: Request<AssignOrgRoleRequest>,
    ) -> Result<Response<Member>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let role = Role::try_from(r.role).unwrap_or(Role::Unspecified);
            let perm = self
                .member_perm(&caller, &org_id, &r.principal_id, role)
                .await?;
            let auth = self.forge.authz.org(&caller, &org_id, perm).await;
            let cx = self
                .guarded(&caller, &org_id, "", "member.role_assign", auth)
                .await?;
            self.forge
                .orgs
                .assign_role(&cx, &org_id, &r.principal_id, role, &r.custom_role_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn revoke_org_role(
        &self,
        req: Request<RevokeOrgRoleRequest>,
    ) -> Result<Response<Member>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let auth = self
                .forge
                .authz
                .org(&caller, &org_id, OrgPerm::Manage)
                .await;
            let cx = self
                .guarded(&caller, &org_id, "", "member.role_revoke", auth)
                .await?;
            self.forge
                .orgs
                .revoke_role(&cx, &org_id, &r.principal_id, &r.custom_role_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_outside_collaborators(
        &self,
        req: Request<ListOutsideCollaboratorsRequest>,
    ) -> Result<Response<ListOutsideCollaboratorsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let cx = self
                .forge
                .authz
                .org(&caller, &org_id, OrgPerm::Manage)
                .await?;
            let (after, size) = page(&r.page);
            let all = self.forge.orgs.outside_collaborators(&cx, &org_id).await?;
            let rest: Vec<_> = all
                .into_iter()
                .filter(|o| {
                    o.principal
                        .as_ref()
                        .is_some_and(|p| p.id.as_str() > after.as_str())
                })
                .collect();
            let more = rest.len() > size;
            let collaborators: Vec<_> = rest.into_iter().take(size).collect();
            let token = if more {
                collaborators
                    .last()
                    .and_then(|c| c.principal.as_ref())
                    .map(|p| p.id.clone())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            Ok(ListOutsideCollaboratorsResponse {
                collaborators,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn convert_to_outside_collaborator(
        &self,
        req: Request<ConvertToOutsideCollaboratorRequest>,
    ) -> Result<Response<ConvertToOutsideCollaboratorResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let perm = self
                .member_perm(&caller, &org_id, &r.principal_id, Role::Member)
                .await?;
            let auth = self.forge.authz.org(&caller, &org_id, perm).await;
            let cx = self
                .guarded(&caller, &org_id, "", "member.convert_to_outside", auth)
                .await?;
            let c = self
                .forge
                .orgs
                .convert_to_outside(&cx, &org_id, &r.principal_id)
                .await?;
            Ok(ConvertToOutsideCollaboratorResponse {
                collaborator: Some(c),
            })
        }
        .await;
        reply(&caller, out)
    }
}
