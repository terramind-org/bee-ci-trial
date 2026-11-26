//! `RepoService`. Reads need `read`; metadata and topics need `manage_settings` with the
//! maintain role; settings, collaborators, autolinks, custom property values, resolver
//! hook, mirrors, deploy keys, archive, delete and transfer need the admin role.

use bee_policy::Action;
use bee_proto::v1::repo_service_server::RepoService;
use bee_proto::v1::{
    AcceptRepoInvitationRequest, AcceptRepoInvitationResponse, AddCollaboratorRequest,
    AddCollaboratorResponse, AddDeployKeyRequest, ArchiveRepoRequest, Autolink,
    CollaboratorPermission, CreateAutolinkRequest, CreateRepoFromTemplateRequest,
    CreateRepoMirrorRequest, CreateRepoRequest, CreateSocialPreviewUploadRequest,
    CreateSocialPreviewUploadResponse, CustomPropertyDefinition, DeclineRepoInvitationRequest,
    DeclineRepoInvitationResponse, DeleteAutolinkRequest, DeleteAutolinkResponse,
    DeleteCustomPropertyDefinitionRequest, DeleteCustomPropertyDefinitionResponse,
    DeleteDeployKeyRequest, DeleteDeployKeyResponse, DeleteRepoMirrorRequest,
    DeleteRepoMirrorResponse, DeleteRepoRequest, DeleteRepoResponse, DeployKey, ForkRepoRequest,
    GetCollaboratorPermissionRequest, GetContextRequest, GetRepoCustomPropertiesRequest,
    GetRepoCustomPropertiesResponse, GetRepoRequest, GetRepoSettingsRequest,
    GetResolverHookRequest, ListAutolinksRequest, ListAutolinksResponse, ListCollaboratorsRequest,
    ListCollaboratorsResponse, ListCustomPropertyDefinitionsRequest,
    ListCustomPropertyDefinitionsResponse, ListDeletedReposRequest, ListDeletedReposResponse,
    ListDeployKeysRequest, ListDeployKeysResponse, ListForksRequest, ListForksResponse,
    ListRepoInvitationsRequest, ListRepoInvitationsResponse, ListRepoMirrorsRequest,
    ListRepoMirrorsResponse, ListReposRequest, ListReposResponse, RemoveCollaboratorRequest,
    RemoveCollaboratorResponse, Repo, RepoContext, RepoMirror, RepoRole, RepoSettings,
    ResolverHook, RestoreRepoRequest, SetCustomPropertyDefinitionRequest,
    SetRepoCustomPropertiesRequest, SetRepoCustomPropertiesResponse, SetRepoTopicsRequest,
    SetRepoTopicsResponse, SetResolverHookRequest, SyncForkRequest, SyncForkResponse,
    SyncRepoMirrorRequest, TransferRepoRequest, UnarchiveRepoRequest, UpdateRepoRequest,
    UpdateRepoSettingsRequest, Visibility,
};
use tonic::{Request, Response, Status};

use super::{ForgeServices, next, page, reply, unserved};
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::hooks::RepoInit;
use crate::ids::ts;
use crate::orgs::repos::{NewRepo, RepoPatch, SOCIAL_PREVIEW_MAX_BYTES, SOCIAL_PREVIEW_PREFIX};

impl ForgeServices {
    async fn repo_admin(
        &self,
        caller: &Caller,
        repo: &Repo,
        action: &str,
        min: RepoRole,
    ) -> Result<Cx> {
        let auth = self
            .forge
            .authz
            .repo(caller, repo, Action::ManageSettings, &[], None, Some(min))
            .await;
        self.guarded(caller, &repo.org_id, &repo.id, action, auth)
            .await
    }

    fn forks(&self, rpc: &str) -> Result<std::sync::Arc<crate::forks::Forks>> {
        self.forge
            .forks()
            .cloned()
            .ok_or_else(|| unserved(rpc, "the forks module (crate::forks::Forks::attach)"))
    }

    /// Private and internal repos fork only when their org lets members fork them, or for
    /// the org's owners and admins.
    async fn may_fork_private(&self, cx: &Cx, upstream: &Repo) -> Result<bool> {
        if self
            .forge
            .orgs
            .settings(cx, &upstream.org_id)
            .await?
            .members_can_fork_private_repos
        {
            return Ok(true);
        }
        let member = self
            .forge
            .orgs
            .member(cx, &upstream.org_id, &cx.actor_id)
            .await?;
        Ok(member.is_some_and(|m| {
            m.role == bee_proto::v1::Role::Owner as i32
                || m.role == bee_proto::v1::Role::Admin as i32
        }))
    }

    /// The caller's personal namespace: the org named after their first login.
    async fn personal_org(&self, caller: &Caller) -> Result<String> {
        let (logins, _) = self
            .forge
            .hooks
            .directory
            .identities(&caller.cx.actor_id)
            .await;
        for login in logins {
            if let Ok(o) = self.forge.orgs.get_org(&caller.cx, &login).await {
                return Ok(o.id);
            }
        }
        Err(ForgeError::invalid(
            "no_personal_org",
            "the caller has no personal namespace; name the destination org",
        ))
    }

    async fn repo_read(&self, caller: &Caller, repo: &Repo) -> Result<Cx> {
        self.forge
            .authz
            .repo(caller, repo, Action::Read, &[], None, None)
            .await
    }

    async fn check_create(
        &self,
        caller: &Caller,
        org_id: &str,
        visibility: Visibility,
    ) -> Result<Cx> {
        let auth = self
            .forge
            .authz
            .org(caller, org_id, OrgPerm::CreateRepo)
            .await;
        let cx = self
            .guarded(caller, org_id, "", "repo.create", auth)
            .await?;
        if visibility == Visibility::Public {
            let member = self.forge.orgs.member(&cx, org_id, &cx.actor_id).await?;
            let privileged = member.is_some_and(|m| {
                m.role == bee_proto::v1::Role::Owner as i32
                    || m.role == bee_proto::v1::Role::Admin as i32
            });
            if !privileged
                && !self
                    .forge
                    .orgs
                    .settings(&cx, org_id)
                    .await?
                    .members_can_create_public_repos
            {
                return Err(ForgeError::permission_denied(
                    "members of this org may not create public repos",
                ));
            }
        }
        Ok(cx)
    }
}

#[tonic::async_trait]
impl RepoService for ForgeServices {
    async fn create_repo(&self, req: Request<CreateRepoRequest>) -> Result<Response<Repo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let visibility = Visibility::try_from(r.visibility).unwrap_or(Visibility::Private);
            let cx = self.check_create(&caller, &org_id, visibility).await?;
            crate::orgs::validate_name(&r.name)?;
            let id = self.forge.db.ids().ulid()?;
            let draft = Repo {
                id: id.clone(),
                org_id: org_id.clone(),
                name: r.name.clone(),
                visibility: visibility as i32,
                ..Default::default()
            };
            let init = RepoInit {
                auto_init: r.auto_init,
                license_template: r.license_template,
            };
            // Repository provisioning establishes the initial named ref.
            let fence =
                crate::safety::control::fence_nonhuman(&self.forge.db, &cx, &org_id).await?;
            self.forge
                .hooks
                .provisioner
                .create_repo(&draft, &init)
                .await?;
            drop(fence);
            self.forge
                .orgs
                .create_repo(
                    &cx,
                    &org_id,
                    NewRepo {
                        id,
                        name: r.name,
                        visibility,
                        description: r.description,
                        is_template: r.is_template,
                        topics: r.topics,
                        homepage: r.homepage,
                        template_repo_id: String::new(),
                        fork_parent_id: String::new(),
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_repo(&self, req: Request<GetRepoRequest>) -> Result<Response<Repo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            self.repo_read(&caller, &repo).await?;
            Ok(repo)
        }
        .await;
        reply(&caller, out)
    }

    async fn list_repos(
        &self,
        req: Request<ListReposRequest>,
    ) -> Result<Response<ListReposResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let (after, size) = page(&r.page);
            let (all, token) = self
                .forge
                .orgs
                .list_repos(&caller.cx, &org_id, &after, size)
                .await?;
            let mut repos = Vec::new();
            for repo in all {
                if self.repo_read(&caller, &repo).await.is_ok() {
                    repos.push(repo);
                }
            }
            Ok(ListReposResponse {
                repos,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_repo(&self, req: Request<UpdateRepoRequest>) -> Result<Response<Repo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let visibility = Visibility::try_from(r.visibility).unwrap_or(Visibility::Unspecified);
            let min = if visibility != Visibility::Unspecified {
                RepoRole::Admin
            } else {
                RepoRole::Maintain
            };
            let cx = self.repo_admin(&caller, &repo, "repo.update", min).await?;
            if !repo.fork_parent_id.is_empty()
                && visibility != Visibility::Unspecified
                && visibility as i32 != repo.visibility
            {
                return Err(ForgeError::invalid(
                    "fork_visibility",
                    "a fork's visibility follows its upstream and cannot be changed",
                ));
            }
            let patch = RepoPatch {
                name: (!r.name.is_empty()).then_some(r.name),
                visibility: Some(visibility),
                description: (!r.description.is_empty()).then_some(r.description),
                homepage: r.homepage,
                default_ref: r.default_ref,
                is_template: r.is_template,
            };
            self.forge
                .orgs
                .update_repo(&cx, &repo.id, patch, r.expected_version)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_context(
        &self,
        req: Request<GetContextRequest>,
    ) -> Result<Response<RepoContext>, Status> {
        if let Some(changes) = self.forge().changes() {
            return crate::changes::service::ChangeServices::new(
                self.forge().clone(),
                changes.clone(),
            )
            .get_context(req)
            .await;
        }
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "RepoService.GetContext",
                "changes (repo context needs open change requests and owners)",
            )),
        )
    }

    async fn delete_repo(
        &self,
        req: Request<DeleteRepoRequest>,
    ) -> Result<Response<DeleteRepoResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "repo.delete", RepoRole::Admin)
                .await?;
            let until = self
                .forge
                .orgs
                .delete_repo(&cx, &repo.id, &r.confirm_full_name)
                .await?;
            Ok(DeleteRepoResponse {
                restorable_until: Some(ts(until)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn restore_repo(
        &self,
        req: Request<RestoreRepoRequest>,
    ) -> Result<Response<Repo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.forge.orgs.repo_any(&caller.cx, &r.repo_id).await?;
            let auth = self
                .forge
                .authz
                .org(&caller, &repo.org_id, OrgPerm::Manage)
                .await;
            let cx = self
                .guarded(&caller, &repo.org_id, &repo.id, "repo.restore", auth)
                .await?;
            self.forge.orgs.restore_repo(&cx, &repo.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_deleted_repos(
        &self,
        req: Request<ListDeletedReposRequest>,
    ) -> Result<Response<ListDeletedReposResponse>, Status> {
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
            let (repos, token) = self
                .forge
                .orgs
                .list_deleted_repos(&cx, &org_id, &after, size)
                .await?;
            Ok(ListDeletedReposResponse {
                repos,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn archive_repo(
        &self,
        req: Request<ArchiveRepoRequest>,
    ) -> Result<Response<Repo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "repo.archive", RepoRole::Admin)
                .await?;
            self.forge.orgs.archive_repo(&cx, &repo.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn unarchive_repo(
        &self,
        req: Request<UnarchiveRepoRequest>,
    ) -> Result<Response<Repo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "repo.unarchive", RepoRole::Admin)
                .await?;
            self.forge.orgs.unarchive_repo(&cx, &repo.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn transfer_repo(
        &self,
        req: Request<TransferRepoRequest>,
    ) -> Result<Response<Repo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "repo.transfer", RepoRole::Admin)
                .await?;
            let new_org = self.org_id(&caller, &r.new_org).await?;
            let auth = self
                .forge
                .authz
                .org(&caller, &new_org, OrgPerm::CreateRepo)
                .await;
            self.guarded(&caller, &new_org, &repo.id, "repo.transfer", auth)
                .await?;
            if !r.team_ids.is_empty() {
                return Err(unserved("TransferRepo team_ids", "enterprise (teams)"));
            }
            self.forge
                .orgs
                .transfer_repo(&cx, &repo.id, &new_org, &r.new_name)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_repo_from_template(
        &self,
        req: Request<CreateRepoFromTemplateRequest>,
    ) -> Result<Response<Repo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let template = self.repo(&caller, &r.template).await?;
            self.repo_read(&caller, &template).await?;
            if !template.is_template {
                return Err(ForgeError::invalid_state(format!(
                    "{} is not a template repo",
                    template.full_name
                )));
            }
            let org_id = self.org_id(&caller, &r.org).await?;
            let visibility = Visibility::try_from(r.visibility).unwrap_or(Visibility::Private);
            let cx = self.check_create(&caller, &org_id, visibility).await?;
            crate::orgs::validate_name(&r.name)?;
            let id = self.forge.db.ids().ulid()?;
            let draft = Repo {
                id: id.clone(),
                org_id: org_id.clone(),
                name: r.name.clone(),
                visibility: visibility as i32,
                template_repo_id: template.id.clone(),
                ..Default::default()
            };
            // This provisioner copies refs directly rather than calling RefService.
            let fence =
                crate::safety::control::fence_nonhuman(&self.forge.db, &cx, &org_id).await?;
            self.forge
                .hooks
                .provisioner
                .create_from_template(&draft, &template, r.include_all_refs)
                .await?;
            drop(fence);
            self.forge
                .orgs
                .create_repo(
                    &cx,
                    &org_id,
                    NewRepo {
                        id,
                        name: r.name,
                        visibility,
                        description: r.description,
                        is_template: false,
                        topics: template.topics.clone(),
                        homepage: String::new(),
                        template_repo_id: template.id,
                        fork_parent_id: String::new(),
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn fork_repo(&self, req: Request<ForkRepoRequest>) -> Result<Response<Repo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let forks = self.forks("RepoService.ForkRepo")?;
            let upstream = self.repo(&caller, &r.repo).await?;
            let rcx = self.repo_read(&caller, &upstream).await?;
            if !self
                .forge
                .orgs
                .repo_settings(&rcx, &upstream.id)
                .await?
                .allow_forking
            {
                return Err(ForgeError::permission_denied(format!(
                    "forking is disabled for {}",
                    upstream.full_name
                )));
            }
            let public = upstream.visibility == Visibility::Public as i32;
            if !public && !self.may_fork_private(&rcx, &upstream).await? {
                return Err(ForgeError::permission_denied(format!(
                    "members of the org of {} may not fork its private repos",
                    upstream.full_name
                )));
            }
            let org_id = if r.org.is_empty() {
                self.personal_org(&caller).await?
            } else {
                self.org_id(&caller, &r.org).await?
            };
            let visibility = if public {
                Visibility::Public
            } else {
                Visibility::Private
            };
            let cx = self.check_create(&caller, &org_id, visibility).await?;
            forks
                .fork(&cx, &upstream, &org_id, &r.name, r.default_ref_only)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_forks(
        &self,
        req: Request<ListForksRequest>,
    ) -> Result<Response<ListForksResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let forks = self.forks("RepoService.ListForks")?;
            let upstream = self.repo(&caller, &r.repo).await?;
            let cx = self.repo_read(&caller, &upstream).await?;
            let (after, size) = page(&r.page);
            let (all, token) = forks.list(&cx, &upstream.id, &after, size).await?;
            let mut visible = Vec::new();
            for f in all {
                if self.repo_read(&caller, &f).await.is_ok() {
                    visible.push(f);
                }
            }
            Ok(ListForksResponse {
                forks: visible,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn sync_fork(
        &self,
        req: Request<SyncForkRequest>,
    ) -> Result<Response<SyncForkResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let forks = self.forks("RepoService.SyncFork")?;
            let fork = self.repo(&caller, &r.repo).await?;
            let upstream = forks.parent(&caller.cx, &fork).await?;
            self.repo_read(&caller, &upstream).await?;
            let ref_name = if r.r#ref.is_empty() {
                fork.default_ref.clone()
            } else {
                r.r#ref.clone()
            };
            let auth = self
                .forge
                .authz
                .repo(&caller, &fork, Action::Commit, &[], Some(&ref_name), None)
                .await;
            let cx = self
                .guarded(&caller, &fork.org_id, &fork.id, "fork.sync", auth)
                .await?;
            if fork.archived {
                return Err(ForgeError::archived(&fork.full_name));
            }
            let s = forks.sync(&cx, &fork, &ref_name).await?;
            Ok(SyncForkResponse {
                r#ref: Some(s.r#ref),
                changesets_pulled: s.pulled,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_repo_topics(
        &self,
        req: Request<SetRepoTopicsRequest>,
    ) -> Result<Response<SetRepoTopicsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "repo.topics_update", RepoRole::Maintain)
                .await?;
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            Ok(SetRepoTopicsResponse {
                topics: self.forge.orgs.set_topics(&cx, &repo.id, r.topics).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_repo_settings(
        &self,
        req: Request<GetRepoSettingsRequest>,
    ) -> Result<Response<RepoSettings>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.repo_read(&caller, &repo).await?;
            self.forge.orgs.repo_settings(&cx, &repo.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_repo_settings(
        &self,
        req: Request<UpdateRepoSettingsRequest>,
    ) -> Result<Response<RepoSettings>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "repo.settings_update", RepoRole::Admin)
                .await?;
            let expected = r.expected_version;
            let settings = self
                .forge
                .orgs
                .update_repo_settings(&cx, &repo.id, expected, move |s| {
                    if r.set_allowed_land_methods {
                        if r.allowed_land_methods.is_empty() {
                            return Err(ForgeError::invalid(
                                "invalid_setting",
                                "at least one land method must be allowed",
                            ));
                        }
                        s.allowed_land_methods = r.allowed_land_methods;
                    }
                    macro_rules! set {
                        ($($f:ident),*) => { $( if let Some(v) = r.$f { s.$f = v; } )* };
                    }
                    set!(
                        default_land_method,
                        squash_message_template,
                        allow_auto_merge,
                        merge_queue_required,
                        suggest_update_base,
                        delete_workspace_ref_on_land,
                        has_issues,
                        has_projects,
                        has_wiki,
                        has_discussions,
                        allow_forking,
                        web_commit_signoff_required,
                        draft_retention_days,
                        cold_after_days,
                        push_auto_change,
                        git_enabled,
                        land_attempt_budget,
                        auto_revert,
                        merge_queue_check_combined
                    );
                    if s.draft_retention_days == 0 {
                        return Err(ForgeError::invalid(
                            "invalid_setting",
                            "draft_retention_days must be at least 1",
                        ));
                    }
                    Ok(())
                })
                .await?;
            if let Some(observer) = &self.repo_settings_observer {
                observer.updated(&settings).await?;
            }
            Ok(settings)
        }
        .await;
        reply(&caller, out)
    }

    async fn create_autolink(
        &self,
        req: Request<CreateAutolinkRequest>,
    ) -> Result<Response<Autolink>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "autolink.create", RepoRole::Admin)
                .await?;
            self.forge
                .orgs
                .create_autolink(
                    &cx,
                    &repo.id,
                    &r.key_prefix,
                    &r.url_template,
                    r.alphanumeric,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_autolinks(
        &self,
        req: Request<ListAutolinksRequest>,
    ) -> Result<Response<ListAutolinksResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.repo_read(&caller, &repo).await?;
            Ok(ListAutolinksResponse {
                autolinks: self.forge.orgs.autolinks(&cx, &repo.id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_autolink(
        &self,
        req: Request<DeleteAutolinkRequest>,
    ) -> Result<Response<DeleteAutolinkResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "autolink.delete", RepoRole::Admin)
                .await?;
            self.forge
                .orgs
                .delete_autolink(&cx, &repo.id, &r.id)
                .await?;
            Ok(DeleteAutolinkResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_custom_property_definitions(
        &self,
        req: Request<ListCustomPropertyDefinitionsRequest>,
    ) -> Result<Response<ListCustomPropertyDefinitionsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let cx = self
                .forge
                .authz
                .org(&caller, &org_id, OrgPerm::ViewMembers)
                .await?;
            Ok(ListCustomPropertyDefinitionsResponse {
                definitions: self.forge.orgs.property_defs(&cx, &org_id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_custom_property_definition(
        &self,
        req: Request<SetCustomPropertyDefinitionRequest>,
    ) -> Result<Response<CustomPropertyDefinition>, Status> {
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
                .guarded(&caller, &org_id, "", "custom_property.set", auth)
                .await?;
            let def = r
                .definition
                .ok_or_else(|| ForgeError::missing("definition"))?;
            self.forge.orgs.set_property_def(&cx, &org_id, def).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_custom_property_definition(
        &self,
        req: Request<DeleteCustomPropertyDefinitionRequest>,
    ) -> Result<Response<DeleteCustomPropertyDefinitionResponse>, Status> {
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
                .guarded(&caller, &org_id, "", "custom_property.delete", auth)
                .await?;
            self.forge
                .orgs
                .delete_property_def(&cx, &org_id, &r.name)
                .await?;
            Ok(DeleteCustomPropertyDefinitionResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn get_repo_custom_properties(
        &self,
        req: Request<GetRepoCustomPropertiesRequest>,
    ) -> Result<Response<GetRepoCustomPropertiesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.repo_read(&caller, &repo).await?;
            Ok(GetRepoCustomPropertiesResponse {
                properties: self.forge.orgs.repo_properties(&cx, &repo.id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_repo_custom_properties(
        &self,
        req: Request<SetRepoCustomPropertiesRequest>,
    ) -> Result<Response<SetRepoCustomPropertiesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "repo.properties_update", RepoRole::Admin)
                .await?;
            Ok(SetRepoCustomPropertiesResponse {
                properties: self
                    .forge
                    .orgs
                    .set_repo_properties(&cx, &repo.id, r.properties)
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn add_collaborator(
        &self,
        req: Request<AddCollaboratorRequest>,
    ) -> Result<Response<AddCollaboratorResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "collaborator.add", RepoRole::Admin)
                .await?;
            let principal = if !r.principal_id.is_empty() {
                self.principal(&r.principal_id).await?
            } else if !r.invitee_login.is_empty() {
                self.forge
                    .hooks
                    .directory
                    .by_login(&r.invitee_login)
                    .await
                    .ok_or_else(|| ForgeError::not_found(format!("login {}", r.invitee_login)))?
            } else {
                return Err(ForgeError::missing("principal_id"));
            };
            let role = RepoRole::try_from(r.role).unwrap_or(RepoRole::Unspecified);
            let (collaborator, invitation) = self
                .forge
                .orgs
                .add_collaborator(&cx, &repo.id, principal, role, &r.custom_role_id)
                .await?;
            Ok(AddCollaboratorResponse {
                collaborator,
                invitation,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_collaborator(
        &self,
        req: Request<RemoveCollaboratorRequest>,
    ) -> Result<Response<RemoveCollaboratorResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "collaborator.remove", RepoRole::Admin)
                .await?;
            self.forge
                .orgs
                .remove_collaborator(&cx, &repo.id, &r.principal_id)
                .await?;
            Ok(RemoveCollaboratorResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_collaborators(
        &self,
        req: Request<ListCollaboratorsRequest>,
    ) -> Result<Response<ListCollaboratorsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.repo_read(&caller, &repo).await?;
            let (after, size) = page(&r.page);
            let rest: Vec<_> = self
                .forge
                .orgs
                .collaborators(&cx, &repo.id, r.direct_only)
                .await?
                .into_iter()
                .filter(|c| {
                    c.principal
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
            Ok(ListCollaboratorsResponse {
                collaborators,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_collaborator_permission(
        &self,
        req: Request<GetCollaboratorPermissionRequest>,
    ) -> Result<Response<CollaboratorPermission>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.repo_read(&caller, &repo).await?;
            let who = if r.principal_id.is_empty() {
                caller.cx.actor_id.clone()
            } else {
                r.principal_id
            };
            self.forge.orgs.effective_role(&cx, &repo, &who).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_repo_invitations(
        &self,
        req: Request<ListRepoInvitationsRequest>,
    ) -> Result<Response<ListRepoInvitationsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let invitations = if r.repo.as_ref().is_some_and(|x| x.repo.is_some()) {
                let repo = self.repo(&caller, &r.repo).await?;
                let cx = self
                    .repo_admin(&caller, &repo, "repo_invitation.list", RepoRole::Admin)
                    .await?;
                self.forge
                    .orgs
                    .repo_invitations(&cx, Some(&repo.id), "")
                    .await?
            } else {
                self.forge
                    .orgs
                    .repo_invitations(&caller.cx, None, &caller.cx.actor_id)
                    .await?
            };
            let (after, size) = page(&r.page);
            let rest: Vec<_> = invitations
                .into_iter()
                .filter(|i| i.id.as_str() > after.as_str())
                .collect();
            let more = rest.len() > size;
            let invitations: Vec<_> = rest.into_iter().take(size).collect();
            let token = if more {
                invitations.last().map(|i| i.id.clone()).unwrap_or_default()
            } else {
                String::new()
            };
            Ok(ListRepoInvitationsResponse {
                invitations,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn accept_repo_invitation(
        &self,
        req: Request<AcceptRepoInvitationRequest>,
    ) -> Result<Response<AcceptRepoInvitationResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let mut cx = caller.cx.clone();
            cx.decision = "invitee accepts own invitation".into();
            let p = self
                .forge
                .hooks
                .directory
                .get(&cx.actor_id)
                .await
                .unwrap_or_else(|| caller.principal());
            let collaborator = self
                .forge
                .orgs
                .answer_repo_invitation(&cx, &r.invitation_id, &p, true)
                .await?;
            Ok(AcceptRepoInvitationResponse { collaborator })
        }
        .await;
        reply(&caller, out)
    }

    async fn decline_repo_invitation(
        &self,
        req: Request<DeclineRepoInvitationRequest>,
    ) -> Result<Response<DeclineRepoInvitationResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let mut cx = caller.cx.clone();
            cx.decision = "invitee declines own invitation".into();
            let p = caller.principal();
            self.forge
                .orgs
                .answer_repo_invitation(&cx, &r.invitation_id, &p, false)
                .await?;
            Ok(DeclineRepoInvitationResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn set_resolver_hook(
        &self,
        req: Request<SetResolverHookRequest>,
    ) -> Result<Response<ResolverHook>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "resolver_hook.set", RepoRole::Admin)
                .await?;
            let hook = ResolverHook {
                repo_id: repo.id.clone(),
                url: r.url,
                agent_principal_id: r.agent_principal_id,
                auto_land: r.auto_land,
                timeout_seconds: r.timeout_seconds,
            };
            self.forge
                .orgs
                .set_resolver(&cx, &repo.id, hook, &r.secret)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_resolver_hook(
        &self,
        req: Request<GetResolverHookRequest>,
    ) -> Result<Response<ResolverHook>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.repo_read(&caller, &repo).await?;
            self.forge.orgs.resolver(&cx, &repo.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_repo_mirror(
        &self,
        req: Request<CreateRepoMirrorRequest>,
    ) -> Result<Response<RepoMirror>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "mirror.create", RepoRole::Admin)
                .await?;
            let m = RepoMirror {
                remote_url: r.remote_url,
                direction: r.direction,
                ref_patterns: r.ref_patterns,
                interval_seconds: r.interval_seconds,
                ..Default::default()
            };
            self.forge
                .orgs
                .create_mirror(&cx, &repo.id, m, &r.credential)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_repo_mirrors(
        &self,
        req: Request<ListRepoMirrorsRequest>,
    ) -> Result<Response<ListRepoMirrorsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "mirror.list", RepoRole::Admin)
                .await?;
            Ok(ListRepoMirrorsResponse {
                mirrors: self.forge.orgs.mirrors(&cx, &repo.id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_repo_mirror(
        &self,
        req: Request<DeleteRepoMirrorRequest>,
    ) -> Result<Response<DeleteRepoMirrorResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "mirror.delete", RepoRole::Admin)
                .await?;
            self.forge.orgs.delete_mirror(&cx, &repo.id, &r.id).await?;
            Ok(DeleteRepoMirrorResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn sync_repo_mirror(
        &self,
        req: Request<SyncRepoMirrorRequest>,
    ) -> Result<Response<RepoMirror>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "mirror.sync", RepoRole::Admin)
                .await?;
            // The mirror must exist here before the worker runs.
            self.forge.orgs.mirror_record(&cx, &repo.id, &r.id).await?;
            match self.forge.orgs.mirror_syncer() {
                Some(s) => s.sync(&cx, &repo, &r.id).await,
                None => Err(unserved(
                    "RepoService.SyncRepoMirror",
                    "git gateway (mirror sync worker)",
                )),
            }
        }
        .await;
        reply(&caller, out)
    }

    async fn add_deploy_key(
        &self,
        req: Request<AddDeployKeyRequest>,
    ) -> Result<Response<DeployKey>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "deploy_key.create", RepoRole::Admin)
                .await?;
            self.forge
                .orgs
                .add_deploy_key(&cx, &repo.id, &r.title, &r.public_key, r.read_only)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_deploy_keys(
        &self,
        req: Request<ListDeployKeysRequest>,
    ) -> Result<Response<ListDeployKeysResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "deploy_key.list", RepoRole::Admin)
                .await?;
            Ok(ListDeployKeysResponse {
                keys: self.forge.orgs.deploy_keys(&cx, &repo.id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_deploy_key(
        &self,
        req: Request<DeleteDeployKeyRequest>,
    ) -> Result<Response<DeleteDeployKeyResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_admin(&caller, &repo, "deploy_key.delete", RepoRole::Admin)
                .await?;
            self.forge
                .orgs
                .delete_deploy_key(&cx, &repo.id, &r.id)
                .await?;
            Ok(DeleteDeployKeyResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn create_social_preview_upload(
        &self,
        req: Request<CreateSocialPreviewUploadRequest>,
    ) -> Result<Response<CreateSocialPreviewUploadResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            self.repo_admin(&caller, &repo, "repo.social_preview", RepoRole::Maintain)
                .await?;
            if !["image/png", "image/jpeg"].contains(&r.content_type.as_str()) {
                return Err(ForgeError::invalid(
                    "bad_content_type",
                    "a social preview is image/png or image/jpeg",
                ));
            }
            if r.size_bytes == 0 || r.size_bytes > SOCIAL_PREVIEW_MAX_BYTES {
                return Err(ForgeError::too_large(
                    "social_preview_bytes",
                    SOCIAL_PREVIEW_MAX_BYTES,
                    r.size_bytes,
                ));
            }
            // The image becomes the repo's preview when its bytes arrive at the signed URL
            // (the server stores them and sets `social_preview_url`).
            let key = format!(
                "{SOCIAL_PREVIEW_PREFIX}{}/{}",
                repo.id,
                self.forge.db.ids().ulid()?
            );
            let signed = self.forge.community.ports.signer.upload(
                &key,
                r.size_bytes,
                &r.content_type,
                self.forge.db.now_ms(),
            );
            Ok(CreateSocialPreviewUploadResponse {
                upload_url: signed.url,
                upload_url_expires_unix: (signed.expires_ms / 1000) as i64,
            })
        }
        .await;
        reply(&caller, out)
    }
}
