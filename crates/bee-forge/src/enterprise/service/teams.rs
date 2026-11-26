//! `TeamService`.

use bee_policy::Action;
use bee_proto::v1::team_service_server::TeamService;
use bee_proto::v1::{
    AddTeamMemberRequest, AddTeamMemberResponse, CancelTeamInvitationRequest,
    CancelTeamInvitationResponse, CreateTeamRequest, DeleteTeamRequest, DeleteTeamResponse,
    EnterprisePolicyToggle, GetTeamMembershipRequest, GetTeamRequest,
    GetTeamReviewAssignmentRequest, GrantTeamRepoAccessRequest, LinkTeamExternalGroupRequest,
    ListChildTeamsRequest, ListChildTeamsResponse, ListPrincipalTeamsRequest,
    ListPrincipalTeamsResponse, ListRepoTeamsRequest, ListRepoTeamsResponse,
    ListTeamExternalGroupsRequest, ListTeamExternalGroupsResponse, ListTeamInvitationsRequest,
    ListTeamInvitationsResponse, ListTeamMembersRequest, ListTeamMembersResponse,
    ListTeamReposRequest, ListTeamReposResponse, ListTeamsRequest, ListTeamsResponse,
    RemoveTeamMemberRequest, RemoveTeamMemberResponse, RepoRole, RevokeTeamRepoAccessRequest,
    RevokeTeamRepoAccessResponse, Team, TeamExternalGroup, TeamMember, TeamMemberRole,
    TeamRepoAccess, TeamReviewAssignment, UnlinkTeamExternalGroupRequest,
    UnlinkTeamExternalGroupResponse, UpdateTeamRequest, UpdateTeamReviewAssignmentRequest,
    add_team_member_request, add_team_member_response,
};
use tonic::{Request, Response, Status};

use super::EnterpriseApi;
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::enterprise::common::{page_args, page_response};
use crate::enterprise::teams::{AddOutcome, Invitee, NewTeam, ReviewPatch, TeamPatch};
use crate::error::{ForgeError, Result};
use crate::service::reply;

impl EnterpriseApi {
    /// Who may create a team (see the module table in `service/mod.rs`).
    async fn create_auth(
        &self,
        caller: &Caller,
        org: &bee_proto::v1::Org,
        parent: &str,
    ) -> Result<Cx> {
        use bee_proto::v1::ErrorReason;
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
        let cx = self
            .forge()
            .authz
            .org(caller, &org.id, OrgPerm::ViewMembers)
            .await?;
        if caller.token.scope.repos.is_some() || !caller.token.allows_action(Action::ManageSettings)
        {
            return Err(ForgeError::policy_denied(
                "token_scope",
                vec!["manage_settings".into()],
                "the token must allow `manage_settings`",
            ));
        }
        let settings = self.forge().orgs.settings(&caller.cx, &org.id).await?;
        let policy = self
            .ent
            .accounts
            .policies_of_org(&caller.cx, &org.id)
            .await?;
        let allowed_by_settings = match policy.map(|p| p.members_can_create_teams) {
            Some(t) if t == EnterprisePolicyToggle::Disabled as i32 => false,
            Some(t) if t == EnterprisePolicyToggle::Allowed as i32 => true,
            _ => settings.members_can_create_teams,
        };
        for id in &caller.cx.chain {
            let mut ok = allowed_by_settings && parent.is_empty();
            if !ok && !parent.is_empty() {
                ok = self
                    .ent
                    .teams
                    .is_maintainer(&caller.cx, &org.id, parent, id)
                    .await?;
            }
            if !ok {
                ok = self
                    .ent
                    .roles
                    .has_org_permission(&caller.cx, &org.id, id, "manage_teams")
                    .await?;
            }
            if !ok {
                return Err(ForgeError::permission_denied(format!(
                    "{id} may not create teams in {}",
                    org.name
                )));
            }
        }
        Ok(cx)
    }

    async fn repo_for_team(
        &self,
        caller: &Caller,
        org: &bee_proto::v1::Org,
        repo: &Option<bee_proto::v1::RepoRef>,
        min: RepoRole,
        action: &str,
    ) -> Result<(bee_proto::v1::Repo, Cx)> {
        let repo = self.svc.repo(caller, repo).await?;
        if repo.org_id != org.id {
            return Err(ForgeError::invalid(
                "repo_not_in_org",
                format!("{} does not belong to {}", repo.full_name, org.name),
            ));
        }
        let auth = self
            .forge()
            .authz
            .repo(caller, &repo, Action::ManageSettings, &[], None, Some(min))
            .await;
        let cx = self
            .svc
            .guarded(caller, &org.id, &repo.id, action, auth)
            .await?;
        Ok((repo, cx))
    }
}

fn member_role(v: i32) -> TeamMemberRole {
    TeamMemberRole::try_from(v).unwrap_or(TeamMemberRole::Unspecified)
}

#[tonic::async_trait]
impl TeamService for EnterpriseApi {
    async fn create_team(
        &self,
        req: Request<CreateTeamRequest>,
    ) -> std::result::Result<Response<Team>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let auth = self.create_auth(&caller, &org, &r.parent_team).await;
            let cx = self
                .svc
                .guarded(&caller, &org.id, "", "team.create", auth)
                .await?;
            let mut maintainers = r.maintainer_ids.clone();
            if maintainers.is_empty() {
                maintainers.push(cx.root().to_string());
            }
            self.ent
                .teams
                .create(
                    &cx,
                    &org,
                    NewTeam {
                        slug: r.slug,
                        name: r.name,
                        description: r.description,
                        privacy: bee_proto::v1::TeamPrivacy::try_from(r.privacy)
                            .unwrap_or_default(),
                        parent: r.parent_team,
                        notifications_enabled: r.notifications_enabled,
                        maintainers,
                        no_maintainers: false,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_team(
        &self,
        req: Request<GetTeamRequest>,
    ) -> std::result::Result<Response<Team>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self.view(&caller, &org.id).await?;
            let v = self.viewer(&caller, &org.id).await?;
            self.ent.teams.get(&cx, &org, &r.team, &v).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_team(
        &self,
        req: Request<UpdateTeamRequest>,
    ) -> std::result::Result<Response<Team>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self
                .team_admin(&caller, &org, Some(&r.team), "team.update")
                .await?;
            self.ent
                .teams
                .update(
                    &cx,
                    &org,
                    &r.team,
                    TeamPatch {
                        slug: r.slug,
                        name: r.name,
                        description: r.description,
                        privacy: r
                            .privacy
                            .and_then(|p| bee_proto::v1::TeamPrivacy::try_from(p).ok()),
                        parent: r.parent_team,
                        notifications_enabled: r.notifications_enabled,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_team(
        &self,
        req: Request<DeleteTeamRequest>,
    ) -> std::result::Result<Response<DeleteTeamResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self
                .team_admin(&caller, &org, Some(&r.team), "team.delete")
                .await?;
            self.ent.teams.delete(&cx, &org, &r.team).await?;
            Ok(DeleteTeamResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_teams(
        &self,
        req: Request<ListTeamsRequest>,
    ) -> std::result::Result<Response<ListTeamsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self.view(&caller, &org.id).await?;
            let v = self.viewer(&caller, &org.id).await?;
            let (after, limit) = page_args(&r.page);
            let (teams, next) = self
                .ent
                .teams
                .list(&cx, &org, &r.query, r.top_level_only, &v, &after, limit)
                .await?;
            Ok(ListTeamsResponse {
                teams,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_child_teams(
        &self,
        req: Request<ListChildTeamsRequest>,
    ) -> std::result::Result<Response<ListChildTeamsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self.view(&caller, &org.id).await?;
            let v = self.viewer(&caller, &org.id).await?;
            let (after, limit) = page_args(&r.page);
            let (teams, next) = self
                .ent
                .teams
                .children(&cx, &org, &r.team, r.recursive, &v, &after, limit)
                .await?;
            Ok(ListChildTeamsResponse {
                teams,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_principal_teams(
        &self,
        req: Request<ListPrincipalTeamsRequest>,
    ) -> std::result::Result<Response<ListPrincipalTeamsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self.view(&caller, &org.id).await?;
            let v = self.viewer(&caller, &org.id).await?;
            let who = if r.principal_id.is_empty() {
                cx.root().to_string()
            } else {
                r.principal_id.clone()
            };
            let (after, limit) = page_args(&r.page);
            let (teams, next) = self
                .ent
                .teams
                .principal_teams(&cx, &org, &who, r.include_inherited, &v, &after, limit)
                .await?;
            Ok(ListPrincipalTeamsResponse {
                teams,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn add_team_member(
        &self,
        req: Request<AddTeamMemberRequest>,
    ) -> std::result::Result<Response<AddTeamMemberResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self
                .team_admin(&caller, &org, Some(&r.team), "team.member_add")
                .await?;
            let who = match r.invitee {
                Some(add_team_member_request::Invitee::PrincipalId(p)) if !p.is_empty() => {
                    Invitee::Principal(p)
                }
                Some(add_team_member_request::Invitee::Email(e)) if !e.is_empty() => {
                    Invitee::Email(e)
                }
                _ => return Err(ForgeError::missing("invitee")),
            };
            let res = match self
                .ent
                .teams
                .add_member(&cx, &org, &r.team, who, member_role(r.role), None)
                .await?
            {
                AddOutcome::Member(m) => add_team_member_response::Result::Member(m),
                AddOutcome::Invitation(i) => add_team_member_response::Result::Invitation(i),
            };
            Ok(AddTeamMemberResponse { result: Some(res) })
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_team_member(
        &self,
        req: Request<RemoveTeamMemberRequest>,
    ) -> std::result::Result<Response<RemoveTeamMemberResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self
                .team_admin(&caller, &org, Some(&r.team), "team.member_remove")
                .await?;
            self.ent
                .teams
                .remove_member(&cx, &org, &r.team, &r.principal_id, false)
                .await?;
            Ok(RemoveTeamMemberResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_team_members(
        &self,
        req: Request<ListTeamMembersRequest>,
    ) -> std::result::Result<Response<ListTeamMembersResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self.view(&caller, &org.id).await?;
            let v = self.viewer(&caller, &org.id).await?;
            let (after, limit) = page_args(&r.page);
            let (members, next) = self
                .ent
                .teams
                .members(
                    &cx,
                    &org,
                    &r.team,
                    member_role(r.role),
                    r.include_child_teams,
                    &v,
                    &after,
                    limit,
                )
                .await?;
            Ok(ListTeamMembersResponse {
                members,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_team_membership(
        &self,
        req: Request<GetTeamMembershipRequest>,
    ) -> std::result::Result<Response<TeamMember>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self.view(&caller, &org.id).await?;
            let v = self.viewer(&caller, &org.id).await?;
            // Secret teams answer NOT_FOUND to those who cannot see them.
            self.ent.teams.get(&cx, &org, &r.team, &v).await?;
            let who = if r.principal_id.is_empty() {
                cx.root().to_string()
            } else {
                r.principal_id.clone()
            };
            self.ent.teams.membership(&cx, &org, &r.team, &who).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_team_invitations(
        &self,
        req: Request<ListTeamInvitationsRequest>,
    ) -> std::result::Result<Response<ListTeamInvitationsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self.team_admin_auth(&caller, &org, Some(&r.team)).await?;
            let (after, limit) = page_args(&r.page);
            let (invitations, next) = self
                .ent
                .teams
                .invitations(&cx, &org, &r.team, &after, limit)
                .await?;
            Ok(ListTeamInvitationsResponse {
                invitations,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn cancel_team_invitation(
        &self,
        req: Request<CancelTeamInvitationRequest>,
    ) -> std::result::Result<Response<CancelTeamInvitationResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self
                .team_admin(&caller, &org, Some(&r.team), "team.invitation_cancel")
                .await?;
            self.ent
                .teams
                .cancel_invitation(&cx, &org, &r.team, &r.invitation_id)
                .await?;
            Ok(CancelTeamInvitationResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn grant_team_repo_access(
        &self,
        req: Request<GrantTeamRepoAccessRequest>,
    ) -> std::result::Result<Response<TeamRepoAccess>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let (repo, cx) = self
                .repo_for_team(&caller, &org, &r.repo, RepoRole::Admin, "team.repo_grant")
                .await?;
            self.ent
                .teams
                .grant_repo(&cx, &org, &r.team, &repo, &r.role)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn revoke_team_repo_access(
        &self,
        req: Request<RevokeTeamRepoAccessRequest>,
    ) -> std::result::Result<Response<RevokeTeamRepoAccessResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let (repo, cx) = self
                .repo_for_team(&caller, &org, &r.repo, RepoRole::Admin, "team.repo_revoke")
                .await?;
            self.ent
                .teams
                .revoke_repo(&cx, &org, &r.team, &repo)
                .await?;
            Ok(RevokeTeamRepoAccessResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_team_repos(
        &self,
        req: Request<ListTeamReposRequest>,
    ) -> std::result::Result<Response<ListTeamReposResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self.view(&caller, &org.id).await?;
            let v = self.viewer(&caller, &org.id).await?;
            self.ent.teams.get(&cx, &org, &r.team, &v).await?;
            let (after, limit) = page_args(&r.page);
            let (repos, next) = self
                .ent
                .teams
                .team_repos(&cx, &org, &r.team, r.include_inherited, &after, limit)
                .await?;
            Ok(ListTeamReposResponse {
                repos,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_repo_teams(
        &self,
        req: Request<ListRepoTeamsRequest>,
    ) -> std::result::Result<Response<ListRepoTeamsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.svc.repo(&caller, &r.repo).await?;
            self.ent
                .admit_caller(&caller, std::slice::from_ref(&repo.org_id), false)
                .await?;
            let cx = self
                .forge()
                .authz
                .repo(
                    &caller,
                    &repo,
                    Action::ManageSettings,
                    &[],
                    None,
                    Some(RepoRole::Maintain),
                )
                .await?;
            let (after, limit) = page_args(&r.page);
            let (grants, next) = self
                .ent
                .teams
                .repo_teams(&cx, &repo.id, &after, limit)
                .await?;
            Ok(ListRepoTeamsResponse {
                grants,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_team_review_assignment(
        &self,
        req: Request<GetTeamReviewAssignmentRequest>,
    ) -> std::result::Result<Response<TeamReviewAssignment>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self.view(&caller, &org.id).await?;
            let v = self.viewer(&caller, &org.id).await?;
            self.ent.teams.get(&cx, &org, &r.team, &v).await?;
            self.ent.teams.review_assignment(&cx, &org, &r.team).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_team_review_assignment(
        &self,
        req: Request<UpdateTeamReviewAssignmentRequest>,
    ) -> std::result::Result<Response<TeamReviewAssignment>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self
                .team_admin(
                    &caller,
                    &org,
                    Some(&r.team),
                    "team.review_assignment_update",
                )
                .await?;
            self.ent
                .teams
                .update_review_assignment(
                    &cx,
                    &org,
                    &r.team,
                    ReviewPatch {
                        enabled: r.enabled,
                        algorithm: r
                            .algorithm
                            .and_then(|a| bee_proto::v1::TeamReviewAlgorithm::try_from(a).ok()),
                        member_count: r.member_count,
                        excluded: r.replace_excluded.then(|| r.excluded_member_ids.clone()),
                        notify_team: r.notify_team,
                        skip_agents: r.skip_agents,
                        count_child_team_members: r.count_child_team_members,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn link_team_external_group(
        &self,
        req: Request<LinkTeamExternalGroupRequest>,
    ) -> std::result::Result<Response<TeamExternalGroup>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let auth = self
                .forge()
                .authz
                .org(&caller, &org.id, OrgPerm::Manage)
                .await;
            let cx = self
                .svc
                .guarded(&caller, &org.id, "", "team.group_link", auth)
                .await?;
            let g = self
                .ent
                .teams
                .link_group(&cx, &org, &r.team, &r.provider, &r.group_id, &r.group_id)
                .await?;
            let team_id = self.ent.teams.resolve_id(&cx, &org.id, &r.team).await?;
            crate::enterprise::sync::resync_team(
                self.forge(),
                &self.ent.teams,
                &cx,
                &org.id,
                &team_id,
            )
            .await?;
            let (groups, _) = self
                .ent
                .teams
                .linked_groups(&cx, &org, &r.team, "", 100)
                .await?;
            Ok(groups
                .into_iter()
                .find(|x| x.provider == g.provider && x.group_id == g.group_id)
                .unwrap_or(g))
        }
        .await;
        reply(&caller, out)
    }

    async fn unlink_team_external_group(
        &self,
        req: Request<UnlinkTeamExternalGroupRequest>,
    ) -> std::result::Result<Response<UnlinkTeamExternalGroupResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let auth = self
                .forge()
                .authz
                .org(&caller, &org.id, OrgPerm::Manage)
                .await;
            let cx = self
                .svc
                .guarded(&caller, &org.id, "", "team.group_unlink", auth)
                .await?;
            self.ent
                .teams
                .unlink_group(&cx, &org, &r.team, &r.provider, &r.group_id)
                .await?;
            Ok(UnlinkTeamExternalGroupResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_team_external_groups(
        &self,
        req: Request<ListTeamExternalGroupsRequest>,
    ) -> std::result::Result<Response<ListTeamExternalGroupsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org(&caller, &r.org).await?;
            let cx = self.team_admin_auth(&caller, &org, Some(&r.team)).await?;
            let (after, limit) = page_args(&r.page);
            let (groups, next) = self
                .ent
                .teams
                .linked_groups(&cx, &org, &r.team, &after, limit)
                .await?;
            Ok(ListTeamExternalGroupsResponse {
                groups,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }
}
