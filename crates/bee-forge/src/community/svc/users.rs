//! `UserService`: profiles, status, pins, follows, stars, star lists, emails, settings,
//! login rename, data export and account deletion. Reads need a token that allows `read`,
//! changes `commit`; every call acts on the caller's own account (reads of other users show
//! only what is public). The public reads (profiles, pins, followers, following, stars, star
//! lists, the contribution graph, activity overview and achievements) also answer a caller
//! without credentials, carried by the server's anonymous read token
//! ([`crate::auth::ANONYMOUS_PRINCIPAL`]): it must name the user, and sees only public repos.
//!
//! The contribution graph, activity overview, home feed and achievements are derived from
//! the event logs (`svc/activity.rs`). Served elsewhere (`UNIMPLEMENTED` /
//! `served_elsewhere`): SSH authentication keys and sessions (identity). Signing keys are
//! stored and parsed here (`community/keys.rs`).

use bee_policy::Action;
use bee_proto::v1::ErrorReason;
use bee_proto::v1::user_service_server::UserService;
use bee_proto::v1::{
    AddStarListRepoRequest, AddUserEmailRequest, AddUserSigningKeyRequest, AddUserSshKeyRequest,
    BeginUserAvatarUploadRequest, BeginUserAvatarUploadResponse, ClearUserStatusRequest,
    ClearUserStatusResponse, CreateStarListRequest, DeleteStarListRequest, DeleteStarListResponse,
    DeleteUserAccountRequest, DeleteUserAccountResponse, DeleteUserEmailRequest,
    DeleteUserEmailResponse, DeleteUserSigningKeyRequest, DeleteUserSigningKeyResponse,
    DeleteUserSshKeyRequest, DeleteUserSshKeyResponse, FinalizeUserAvatarUploadRequest,
    FollowRepoRequest, FollowRepoResponse, FollowUserRequest, FollowUserResponse,
    GetStarListRequest, GetUserActivityOverviewRequest, GetUserContributionGraphRequest,
    GetUserDataExportRequest, GetUserRequest, GetUserSettingsRequest, GetViewerRequest,
    IsFollowingRepoRequest, IsFollowingRepoResponse, IsFollowingUserRequest,
    IsFollowingUserResponse, IsRepoStarredRequest, IsRepoStarredResponse, KeyValue,
    ListFollowedReposRequest, ListFollowedReposResponse, ListRepoFollowersRequest,
    ListRepoFollowersResponse, ListRepoStargazersRequest, ListRepoStargazersResponse,
    ListStarListReposRequest, ListStarListReposResponse, ListStarListsRequest,
    ListStarListsResponse, ListStarredReposRequest, ListStarredReposResponse,
    ListUserAchievementsRequest, ListUserAchievementsResponse, ListUserEmailsRequest,
    ListUserEmailsResponse, ListUserFeedEventsRequest, ListUserFeedEventsResponse,
    ListUserFollowersRequest, ListUserFollowersResponse, ListUserFollowingRequest,
    ListUserFollowingResponse, ListUserPinnedItemsRequest, ListUserPinnedItemsResponse,
    ListUserSecurityEventsRequest, ListUserSecurityEventsResponse, ListUserSessionsRequest,
    ListUserSessionsResponse, ListUserSigningKeysRequest, ListUserSigningKeysResponse,
    ListUserSshKeysRequest, ListUserSshKeysResponse, RemoveStarListRepoRequest,
    RenameUserLoginRequest, RepoRef, RequestUserDataExportRequest,
    ResendUserEmailVerificationRequest, ResendUserEmailVerificationResponse,
    ResolveUserCommitEmailsRequest, ResolveUserCommitEmailsResponse, RevokeUserSessionRequest,
    RevokeUserSessionResponse, SetPrimaryUserEmailRequest, SetUserPinnedItemsRequest,
    SetUserStatusRequest, StarGazer, StarList, StarRepoRequest, StarRepoResponse, StarredRepo,
    UnfollowRepoRequest, UnfollowRepoResponse, UnfollowUserRequest, UnfollowUserResponse,
    UnstarRepoRequest, UnstarRepoResponse, UpdateStarListRequest, UpdateUserEmailVisibilityRequest,
    UpdateUserEmailVisibilityResponse, UpdateUserProfileRequest, UpdateUserSettingsRequest,
    UserActivityOverview, UserContributionGraph, UserDataExport, UserEmail, UserPinnedItem,
    UserPinnedItemKind, UserPinnedItemRef, UserProfile, UserSettings, UserSigningKey,
    UserSigningKeyKind, UserSshKey, UserStatus, get_user_request, repo_ref,
};
use tonic::{Request, Response, Status};

use crate::auth::{ANONYMOUS_PRINCIPAL, Caller};
use crate::community::users::{ProfilePatch, SettingsPatch};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::service::{ForgeServices, next, page, reply, unserved};

fn elsewhere(caller: &Caller, rpc: &str, owner: &str) -> Status {
    unserved(rpc, owner).to_status(&caller.cx.request_id)
}

fn name_ref(full: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::FullName(full.to_string())),
    })
}

impl ForgeServices {
    /// The principal id of a login (empty = the caller). A caller without credentials (the
    /// anonymous read token) has no account of its own, so it must name one.
    pub(crate) async fn uid(&self, cx: &Cx, login: &str) -> Result<String> {
        if login.is_empty() {
            if cx.actor_id == ANONYMOUS_PRINCIPAL {
                return Err(ForgeError::new(
                    ErrorReason::Unauthenticated,
                    "sign in to read your own account, or name a login",
                ));
            }
            return Ok(cx.actor_id.clone());
        }
        Ok(self.community().users.resolve_login(cx, login).await?.0)
    }

    /// A full profile: stored fields plus gist count, sponsorable flag, org badges and the
    /// profile README repo.
    async fn full_profile(&self, caller: &Caller, cx: &Cx, pid: &str) -> Result<UserProfile> {
        let mut p = self
            .community()
            .users
            .profile(cx, pid, &caller.cx.actor_id)
            .await?;
        p.public_gist_count = self
            .community()
            .gists
            .public_count(cx, pid)
            .await
            .unwrap_or(0);
        if let Ok((orgs, _)) = self.fg().orgs.orgs_of(cx, pid, "", 50).await {
            p.orgs = orgs.into_iter().map(|o| o.name).collect();
        }
        let owner = crate::community::sponsors::Owner::user(
            &p.principal.clone().unwrap_or_default(),
            &p.login,
        );
        p.sponsorable = self
            .community()
            .sponsors
            .profile(cx, &owner, false)
            .await
            .map(|s| s.enabled)
            .unwrap_or(false);
        if let Ok(r) = self
            .fg()
            .orgs
            .resolve_repo(
                cx,
                &bee_proto::v1::RepoRef {
                    repo: Some(repo_ref::Repo::FullName(format!("{0}/{0}", p.login))),
                },
            )
            .await
            && self
                .fg()
                .authz
                .repo(caller, &r, Action::Read, &[], None, None)
                .await
                .is_ok()
        {
            p.readme_repo = name_ref(&r.full_name);
        }
        Ok(p)
    }

    async fn summaries(&self, cx: &Cx, ids: Vec<String>) -> Vec<bee_proto::v1::UserSummary> {
        let mut out = Vec::new();
        for id in ids {
            if let Some(s) = self.community().users.summary(cx, &id).await {
                out.push(s);
            }
        }
        out
    }

    async fn starred_view(
        &self,
        caller: &Caller,
        cx: &Cx,
        repo_id: &str,
        at_ms: u64,
    ) -> Option<StarredRepo> {
        let repo = self
            .fg()
            .orgs
            .repo_any(cx, repo_id)
            .await
            .ok()
            .filter(|r| r.deleted_at.is_none())?;
        self.fg()
            .authz
            .repo(caller, &repo, Action::Read, &[], None, None)
            .await
            .ok()?;
        Some(StarredRepo {
            repo: name_ref(&repo.full_name),
            full_name: repo.full_name.clone(),
            description: repo.description.clone(),
            language: repo.primary_language.clone(),
            star_count: self
                .community()
                .users
                .star_count(cx, repo_id)
                .await
                .unwrap_or(0),
            starred_at: Some(ts(at_ms)),
        })
    }
}

#[tonic::async_trait]
impl UserService for ForgeServices {
    async fn get_user(
        &self,
        req: Request<GetUserRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let pid = match r.user {
                Some(get_user_request::User::Login(l)) => {
                    self.community().users.resolve_login(&cx, &l).await?.0
                }
                Some(get_user_request::User::Id(i)) => i,
                None => return Err(ForgeError::missing("user")),
            };
            self.full_profile(&caller, &cx, &pid).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_viewer(
        &self,
        req: Request<GetViewerRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let caller = self.caller(&req)?;
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            self.full_profile(&caller, &cx, &cx.actor_id.clone()).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_user_profile(
        &self,
        req: Request<UpdateUserProfileRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .users
                .update_profile(
                    &cx,
                    ProfilePatch {
                        name: r.name,
                        bio: r.bio,
                        pronouns: r.pronouns,
                        company: r.company,
                        location: r.location,
                        website: r.website,
                        social: r.social_accounts,
                        replace_social: r.replace_social_accounts,
                        public_email: r.public_email,
                        hireable: r.hireable,
                    },
                )
                .await?;
            self.full_profile(&caller, &cx, &cx.actor_id.clone()).await
        }
        .await;
        reply(&caller, out)
    }

    async fn begin_user_avatar_upload(
        &self,
        req: Request<BeginUserAvatarUploadRequest>,
    ) -> Result<Response<BeginUserAvatarUploadResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            let (upload_id, url) = self
                .community()
                .users
                .begin_avatar(&cx, &r.content_type, r.size)
                .await?;
            Ok(BeginUserAvatarUploadResponse {
                upload_id,
                upload_url: url.url,
                upload_headers: url
                    .headers
                    .into_iter()
                    .map(|(key, value)| KeyValue { key, value })
                    .collect(),
                expires_at: Some(ts(url.expires_ms)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn finalize_user_avatar_upload(
        &self,
        req: Request<FinalizeUserAvatarUploadRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .users
                .finalize_avatar(&cx, &r.upload_id, &r.sha256)
                .await?;
            self.full_profile(&caller, &cx, &cx.actor_id.clone()).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_user_status(
        &self,
        req: Request<SetUserStatusRequest>,
    ) -> Result<Response<UserStatus>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            if !r.org.is_empty() {
                let o = self.fg().orgs.get_org(&cx, &r.org).await?;
                self.fg()
                    .authz
                    .org(&caller, &o.id, crate::auth::OrgPerm::ViewMembers)
                    .await?;
            }
            self.community()
                .users
                .set_status(
                    &cx,
                    UserStatus {
                        message: r.message,
                        emoji: r.emoji,
                        busy: r.busy,
                        expires_at: r.expires_at,
                        org: r.org,
                        updated_at: None,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn clear_user_status(
        &self,
        req: Request<ClearUserStatusRequest>,
    ) -> Result<Response<ClearUserStatusResponse>, Status> {
        let caller = self.caller(&req)?;
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community().users.clear_status(&cx).await?;
            Ok(ClearUserStatusResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn set_user_pinned_items(
        &self,
        req: Request<SetUserPinnedItemsRequest>,
    ) -> Result<Response<ListUserPinnedItemsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            if r.items.len() > crate::community::users::MAX_PINNED {
                return Err(ForgeError::too_large(
                    "pinned_items",
                    crate::community::users::MAX_PINNED as u64,
                    r.items.len() as u64,
                ));
            }
            let mut refs = Vec::new();
            for i in r.items {
                match UserPinnedItemKind::try_from(i.kind).unwrap_or_default() {
                    UserPinnedItemKind::Repo => {
                        let by_id = RepoRef {
                            repo: Some(repo_ref::Repo::Id(i.id.clone())),
                        };
                        let repo = match self.fg().orgs.resolve_repo(&cx, &by_id).await {
                            Ok(r) => r,
                            Err(_) => {
                                self.fg()
                                    .orgs
                                    .resolve_repo(
                                        &cx,
                                        &RepoRef {
                                            repo: Some(repo_ref::Repo::FullName(i.id.clone())),
                                        },
                                    )
                                    .await?
                            }
                        };
                        self.fg()
                            .authz
                            .repo(&caller, &repo, Action::Read, &[], None, None)
                            .await?;
                        refs.push(UserPinnedItemRef {
                            kind: i.kind,
                            id: repo.id,
                        });
                    }
                    UserPinnedItemKind::Gist => {
                        let owner = self.community().gists.owner(&cx, &i.id).await?;
                        if owner.id != cx.actor_id {
                            return Err(ForgeError::invalid(
                                "not_your_gist",
                                "only your own gists can be pinned",
                            ));
                        }
                        refs.push(i);
                    }
                    UserPinnedItemKind::Unspecified => {
                        return Err(ForgeError::invalid(
                            "invalid_pin",
                            "pinned items need a kind",
                        ));
                    }
                }
            }
            self.community().users.set_pinned(&cx, refs).await?;
            Ok(ListUserPinnedItemsResponse {
                items: self
                    .pinned_items(&caller, &cx, &cx.actor_id.clone())
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_user_pinned_items(
        &self,
        req: Request<ListUserPinnedItemsRequest>,
    ) -> Result<Response<ListUserPinnedItemsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let pid = self.uid(&cx, &r.login).await?;
            Ok(ListUserPinnedItemsResponse {
                items: self.pinned_items(&caller, &cx, &pid).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn follow_user(
        &self,
        req: Request<FollowUserRequest>,
    ) -> Result<Response<FollowUserResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            let pid = self.community().users.resolve_login(&cx, &r.login).await?.0;
            self.community().users.follow(&cx, &pid).await?;
            Ok(FollowUserResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn unfollow_user(
        &self,
        req: Request<UnfollowUserRequest>,
    ) -> Result<Response<UnfollowUserResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            let pid = self.community().users.resolve_login(&cx, &r.login).await?.0;
            self.community().users.unfollow(&cx, &pid).await?;
            Ok(UnfollowUserResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_user_followers(
        &self,
        req: Request<ListUserFollowersRequest>,
    ) -> Result<Response<ListUserFollowersResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let pid = self.uid(&cx, &r.login).await?;
            let (token, limit) = page(&r.page);
            let (ids, nt) = self
                .community()
                .users
                .followers(&cx, &pid, &token, limit)
                .await?;
            Ok(ListUserFollowersResponse {
                users: self.summaries(&cx, ids).await,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_user_following(
        &self,
        req: Request<ListUserFollowingRequest>,
    ) -> Result<Response<ListUserFollowingResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let pid = self.uid(&cx, &r.login).await?;
            let (token, limit) = page(&r.page);
            let (ids, nt) = self
                .community()
                .users
                .following(&cx, &pid, &token, limit)
                .await?;
            Ok(ListUserFollowingResponse {
                users: self.summaries(&cx, ids).await,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn is_following_user(
        &self,
        req: Request<IsFollowingUserRequest>,
    ) -> Result<Response<IsFollowingUserResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let follower = self.uid(&cx, &r.login).await?;
            let target = self
                .community()
                .users
                .resolve_login(&cx, &r.target_login)
                .await?
                .0;
            Ok(IsFollowingUserResponse {
                following: self
                    .community()
                    .users
                    .is_following(&cx, &follower, &target)
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn star_repo(
        &self,
        req: Request<StarRepoRequest>,
    ) -> Result<Response<StarRepoResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            if !caller.token.allows_action(Action::Commit) {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::PolicyDenied,
                    "token does not allow `commit`",
                ));
            }
            self.community().users.star(&cx, &repo).await?;
            Ok(StarRepoResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn unstar_repo(
        &self,
        req: Request<UnstarRepoRequest>,
    ) -> Result<Response<UnstarRepoResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            let repo = self.repo(&caller, &r.repo).await?;
            self.community().users.unstar(&cx, &repo).await?;
            Ok(UnstarRepoResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn is_repo_starred(
        &self,
        req: Request<IsRepoStarredRequest>,
    ) -> Result<Response<IsRepoStarredResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let (starred, star_list_ids) = self.community().users.is_starred(&cx, &repo.id).await?;
            Ok(IsRepoStarredResponse {
                starred,
                star_list_ids,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_starred_repos(
        &self,
        req: Request<ListStarredReposRequest>,
    ) -> Result<Response<ListStarredReposResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let pid = self.uid(&cx, &r.login).await?;
            let (token, limit) = page(&r.page);
            let (rows, nt) = self
                .community()
                .users
                .starred(&cx, &pid, &token, limit)
                .await?;
            let mut repos = Vec::new();
            for (id, at) in rows {
                if let Some(v) = self.starred_view(&caller, &cx, &id, at).await
                    && (r.language.is_empty() || v.language.eq_ignore_ascii_case(&r.language))
                {
                    repos.push(v);
                }
            }
            Ok(ListStarredReposResponse {
                repos,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_repo_stargazers(
        &self,
        req: Request<ListRepoStargazersRequest>,
    ) -> Result<Response<ListRepoStargazersResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let (token, limit) = page(&r.page);
            let (rows, total_count, nt) = self
                .community()
                .users
                .stargazers(&cx, &repo.id, &token, limit)
                .await?;
            let mut stargazers = Vec::new();
            for (id, at) in rows {
                if let Some(user) = self.community().users.summary(&cx, &id).await {
                    stargazers.push(StarGazer {
                        user: Some(user),
                        starred_at: Some(ts(at)),
                    });
                }
            }
            Ok(ListRepoStargazersResponse {
                stargazers,
                total_count,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_star_list(
        &self,
        req: Request<CreateStarListRequest>,
    ) -> Result<Response<StarList>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .users
                .create_star_list(&cx, &r.name, &r.description, r.private)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_star_list(
        &self,
        req: Request<GetStarListRequest>,
    ) -> Result<Response<StarList>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            self.community().users.star_list(&cx, &r.star_list_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_star_lists(
        &self,
        req: Request<ListStarListsRequest>,
    ) -> Result<Response<ListStarListsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let pid = self.uid(&cx, &r.login).await?;
            let (token, limit) = page(&r.page);
            let (star_lists, nt) = self
                .community()
                .users
                .star_lists(&cx, &pid, &token, limit)
                .await?;
            Ok(ListStarListsResponse {
                star_lists,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_star_list(
        &self,
        req: Request<UpdateStarListRequest>,
    ) -> Result<Response<StarList>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .users
                .update_star_list(
                    &cx,
                    &r.star_list_id,
                    r.name.as_deref(),
                    r.description.as_deref(),
                    r.private,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_star_list(
        &self,
        req: Request<DeleteStarListRequest>,
    ) -> Result<Response<DeleteStarListResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .users
                .delete_star_list(&cx, &r.star_list_id)
                .await?;
            Ok(DeleteStarListResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn add_star_list_repo(
        &self,
        req: Request<AddStarListRepoRequest>,
    ) -> Result<Response<StarList>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            if !caller.token.allows_action(Action::Commit) {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::PolicyDenied,
                    "token does not allow `commit`",
                ));
            }
            self.community()
                .users
                .add_to_star_list(&cx, &r.star_list_id, &repo)
                .await?;
            self.community().users.star_list(&cx, &r.star_list_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_star_list_repo(
        &self,
        req: Request<RemoveStarListRepoRequest>,
    ) -> Result<Response<StarList>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            let repo = self.repo(&caller, &r.repo).await?;
            self.community()
                .users
                .remove_from_star_list(&cx, &r.star_list_id, &repo.id)
                .await?;
            self.community().users.star_list(&cx, &r.star_list_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_star_list_repos(
        &self,
        req: Request<ListStarListReposRequest>,
    ) -> Result<Response<ListStarListReposResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let (token, limit) = page(&r.page);
            let (rows, nt) = self
                .community()
                .users
                .star_list_repos(&cx, &r.star_list_id, &token, limit)
                .await?;
            let mut repos = Vec::new();
            for (id, at) in rows {
                if let Some(v) = self.starred_view(&caller, &cx, &id, at).await {
                    repos.push(v);
                }
            }
            Ok(ListStarListReposResponse {
                repos,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_user_contribution_graph(
        &self,
        req: Request<GetUserContributionGraphRequest>,
    ) -> Result<Response<UserContributionGraph>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = self.contribution_graph(&caller, r).await;
        reply(&caller, out)
    }

    async fn get_user_activity_overview(
        &self,
        req: Request<GetUserActivityOverviewRequest>,
    ) -> Result<Response<UserActivityOverview>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = self.activity_overview(&caller, r).await;
        reply(&caller, out)
    }

    async fn add_user_email(
        &self,
        req: Request<AddUserEmailRequest>,
    ) -> Result<Response<UserEmail>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community().users.add_email(&cx, &r.email).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_user_email(
        &self,
        req: Request<DeleteUserEmailRequest>,
    ) -> Result<Response<DeleteUserEmailResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community().users.delete_email(&cx, &r.email).await?;
            Ok(DeleteUserEmailResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_user_emails(
        &self,
        req: Request<ListUserEmailsRequest>,
    ) -> Result<Response<ListUserEmailsResponse>, Status> {
        let caller = self.caller(&req)?;
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let (emails, noreply_address, keep_private, block_private_pushes) =
                self.community().users.emails(&cx).await?;
            Ok(ListUserEmailsResponse {
                emails,
                noreply_address,
                keep_private,
                block_private_pushes,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_primary_user_email(
        &self,
        req: Request<SetPrimaryUserEmailRequest>,
    ) -> Result<Response<UserEmail>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .users
                .set_primary_email(&cx, &r.email)
                .await?;
            let (emails, ..) = self.community().users.emails(&cx).await?;
            let want = r.email.trim().to_lowercase();
            emails
                .into_iter()
                .find(|e| e.email == want)
                .ok_or_else(|| ForgeError::not_found(format!("email {want}")))
        }
        .await;
        reply(&caller, out)
    }

    async fn update_user_email_visibility(
        &self,
        req: Request<UpdateUserEmailVisibilityRequest>,
    ) -> Result<Response<UpdateUserEmailVisibilityResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .users
                .update_email_visibility(
                    &cx,
                    r.keep_private,
                    r.block_private_pushes,
                    &r.public_emails,
                    r.replace_public_emails,
                )
                .await?;
            let (emails, _, keep_private, block_private_pushes) =
                self.community().users.emails(&cx).await?;
            Ok(UpdateUserEmailVisibilityResponse {
                emails,
                keep_private,
                block_private_pushes,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn resend_user_email_verification(
        &self,
        req: Request<ResendUserEmailVerificationRequest>,
    ) -> Result<Response<ResendUserEmailVerificationResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            let at = self
                .community()
                .users
                .resend_verification(&cx, &r.email)
                .await?;
            Ok(ResendUserEmailVerificationResponse {
                sent_at: Some(ts(at)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn resolve_user_commit_emails(
        &self,
        req: Request<ResolveUserCommitEmailsRequest>,
    ) -> Result<Response<ResolveUserCommitEmailsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            Ok(ResolveUserCommitEmailsResponse {
                mappings: self
                    .community()
                    .users
                    .resolve_commit_emails(&cx, &r.emails)
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn add_user_ssh_key(
        &self,
        req: Request<AddUserSshKeyRequest>,
    ) -> Result<Response<UserSshKey>, Status> {
        let caller = self.caller(&req)?;
        Err(elsewhere(&caller, "UserService.AddUserSshKey", "identity"))
    }

    async fn list_user_ssh_keys(
        &self,
        req: Request<ListUserSshKeysRequest>,
    ) -> Result<Response<ListUserSshKeysResponse>, Status> {
        let caller = self.caller(&req)?;
        Err(elsewhere(
            &caller,
            "UserService.ListUserSshKeys",
            "identity",
        ))
    }

    async fn delete_user_ssh_key(
        &self,
        req: Request<DeleteUserSshKeyRequest>,
    ) -> Result<Response<DeleteUserSshKeyResponse>, Status> {
        let caller = self.caller(&req)?;
        Err(elsewhere(
            &caller,
            "UserService.DeleteUserSshKey",
            "identity",
        ))
    }

    async fn add_user_signing_key(
        &self,
        req: Request<AddUserSigningKeyRequest>,
    ) -> Result<Response<UserSigningKey>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            let (emails, _, _, _) = self.community().users.emails(&cx).await?;
            let verified: Vec<String> = emails
                .into_iter()
                .filter(|e| e.verified)
                .map(|e| e.email)
                .collect();
            let kind = UserSigningKeyKind::try_from(r.kind).unwrap_or_default();
            self.community()
                .keys
                .add(&cx, kind, &r.title, &r.public_key, &verified)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_user_signing_keys(
        &self,
        req: Request<ListUserSigningKeysRequest>,
    ) -> Result<Response<ListUserSigningKeysResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let pid = self.uid(&cx, &r.login).await?;
            let kind = UserSigningKeyKind::try_from(r.kind).unwrap_or_default();
            let keys = self.community().keys.list(&cx, &pid, kind).await?;
            let (token, limit) = page(&r.page);
            let (keys, nt) = crate::community::common::page_vec(keys, &token, limit)?;
            Ok(ListUserSigningKeysResponse {
                keys,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_user_signing_key(
        &self,
        req: Request<DeleteUserSigningKeyRequest>,
    ) -> Result<Response<DeleteUserSigningKeyResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community().keys.delete(&cx, &r.key_id).await?;
            Ok(DeleteUserSigningKeyResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn get_user_settings(
        &self,
        req: Request<GetUserSettingsRequest>,
    ) -> Result<Response<UserSettings>, Status> {
        let caller = self.caller(&req)?;
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            self.community().users.settings(&cx).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_user_settings(
        &self,
        req: Request<UpdateUserSettingsRequest>,
    ) -> Result<Response<UserSettings>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .users
                .update_settings(
                    &cx,
                    SettingsPatch {
                        theme: r.theme,
                        dark_theme: r.dark_theme,
                        light_theme: r.light_theme,
                        time_zone: r.time_zone,
                        locale: r.locale,
                        accessibility: r.accessibility,
                        keyboard_shortcuts: r.keyboard_shortcuts,
                        keyboard_shortcuts_enabled: r.keyboard_shortcuts_enabled,
                        editor_indent: r.editor_indent,
                        diff_view: r.diff_view,
                        show_private_contributions: r.show_private_contributions,
                        vigilant_mode: r.vigilant_mode,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_user_sessions(
        &self,
        req: Request<ListUserSessionsRequest>,
    ) -> Result<Response<ListUserSessionsResponse>, Status> {
        let caller = self.caller(&req)?;
        Err(elsewhere(
            &caller,
            "UserService.ListUserSessions",
            "identity",
        ))
    }

    async fn revoke_user_session(
        &self,
        req: Request<RevokeUserSessionRequest>,
    ) -> Result<Response<RevokeUserSessionResponse>, Status> {
        let caller = self.caller(&req)?;
        Err(elsewhere(
            &caller,
            "UserService.RevokeUserSession",
            "identity",
        ))
    }

    async fn list_user_security_events(
        &self,
        req: Request<ListUserSecurityEventsRequest>,
    ) -> Result<Response<ListUserSecurityEventsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let (token, limit) = page(&r.page);
            let since = r.since.as_ref().map(ts_ms).unwrap_or(0);
            let (events, nt) = self
                .community()
                .users
                .security_events(&cx, &r.action_prefix, since, &token, limit)
                .await?;
            Ok(ListUserSecurityEventsResponse {
                events,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn rename_user_login(
        &self,
        req: Request<RenameUserLoginRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            crate::community::users::validate_login(&r.new_login)?;
            // Logins share a namespace with orgs and with the directory.
            if self.fg().orgs.get_org(&cx, &r.new_login).await.is_ok() {
                return Err(ForgeError::already_exists(format!("login {}", r.new_login)));
            }
            if let Some(p) = self.fg().hooks.directory.by_login(&r.new_login).await
                && p.id != cx.actor_id
            {
                return Err(ForgeError::already_exists(format!("login {}", r.new_login)));
            }
            self.community()
                .users
                .rename_login(&cx, &r.new_login)
                .await?;
            self.full_profile(&caller, &cx, &cx.actor_id.clone()).await
        }
        .await;
        reply(&caller, out)
    }

    async fn request_user_data_export(
        &self,
        req: Request<RequestUserDataExportRequest>,
    ) -> Result<Response<UserDataExport>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .users
                .request_export(&cx, r.include_repos, r.include_attachments)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_user_data_export(
        &self,
        req: Request<GetUserDataExportRequest>,
    ) -> Result<Response<UserDataExport>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            self.community().users.export(&cx, &r.export_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_user_account(
        &self,
        req: Request<DeleteUserAccountRequest>,
    ) -> Result<Response<DeleteUserAccountResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            // An account that is the only owner of an org cannot vanish and orphan it.
            let (orgs, _) = self.fg().orgs.orgs_of(&cx, &cx.actor_id, "", 500).await?;
            for o in orgs {
                let (members, _) = self.fg().orgs.members(&cx, &o.id, "", 1000).await?;
                let owners: Vec<_> = members
                    .iter()
                    .filter(|m| m.role == bee_proto::v1::Role::Owner as i32)
                    .collect();
                if owners.len() == 1
                    && owners[0]
                        .principal
                        .as_ref()
                        .is_some_and(|p| p.id == cx.actor_id)
                {
                    return Err(ForgeError::invalid_state(format!(
                        "you are the only owner of {}; transfer ownership first",
                        o.name
                    ))
                    .with_subcode("sole_org_owner"));
                }
            }
            let purge = self
                .community()
                .users
                .delete_account(&cx, &r.confirm_login, &r.reason)
                .await?;
            Ok(DeleteUserAccountResponse {
                purge_after: Some(ts(purge)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_user_feed_events(
        &self,
        req: Request<ListUserFeedEventsRequest>,
    ) -> Result<Response<ListUserFeedEventsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = self.home_feed(&caller, r).await;
        reply(&caller, out)
    }

    async fn list_user_achievements(
        &self,
        req: Request<ListUserAchievementsRequest>,
    ) -> Result<Response<ListUserAchievementsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = self.user_achievements(&caller, r).await;
        reply(&caller, out)
    }

    async fn follow_repo(
        &self,
        req: Request<FollowRepoRequest>,
    ) -> Result<Response<FollowRepoResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            self.account_write(&caller)?;
            self.community().users.follow_repo(&cx, &repo).await?;
            Ok(FollowRepoResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn unfollow_repo(
        &self,
        req: Request<UnfollowRepoRequest>,
    ) -> Result<Response<UnfollowRepoResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            // Like unstarring: a follower who lost access to the repo can still let go of it.
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.account_write(&caller)?;
            let repo = self.repo(&caller, &r.repo).await?;
            self.community().users.unfollow_repo(&cx, &repo).await?;
            Ok(UnfollowRepoResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn is_following_repo(
        &self,
        req: Request<IsFollowingRepoRequest>,
    ) -> Result<Response<IsFollowingRepoResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            Ok(IsFollowingRepoResponse {
                following: !caller.is_anonymous()
                    && self
                        .community()
                        .users
                        .is_following_repo(&cx, &caller.cx.actor_id, &repo.id)
                        .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_followed_repos(
        &self,
        req: Request<ListFollowedReposRequest>,
    ) -> Result<Response<ListFollowedReposResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let pid = self.follower_id(&cx, &r.user).await?;
            let (token, limit) = page(&r.page);
            let (rows, nt) = self
                .community()
                .users
                .followed_repos(&cx, &pid, &token, limit)
                .await?;
            let mut repos = Vec::new();
            for (id, _) in rows {
                let rr = RepoRef {
                    repo: Some(repo_ref::Repo::Id(id)),
                };
                // Only live repos the caller can read.
                let Ok(repo) = self.fg().orgs.resolve_repo(&cx, &rr).await else {
                    continue;
                };
                if self.can(&caller, &repo, Action::Read).await {
                    repos.push(repo);
                }
            }
            Ok(ListFollowedReposResponse {
                repos,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_repo_followers(
        &self,
        req: Request<ListRepoFollowersRequest>,
    ) -> Result<Response<ListRepoFollowersResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let (token, limit) = page(&r.page);
            let (rows, _, nt) = self
                .community()
                .users
                .repo_followers(&cx, &repo.id, &token, limit)
                .await?;
            let mut followers = Vec::with_capacity(rows.len());
            for (id, _) in rows {
                followers.push(
                    self.fg()
                        .hooks
                        .directory
                        .get(&id)
                        .await
                        .unwrap_or_else(|| crate::community::common::principal_ref(&id)),
                );
            }
            Ok(ListRepoFollowersResponse {
                followers,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }
}

impl ForgeServices {
    /// A write to the caller's own account (follows): signed in, with a token that allows
    /// `commit`. Agents follow with their own tokens.
    fn account_write(&self, caller: &Caller) -> Result<()> {
        if caller.is_anonymous() {
            return Err(ForgeError::new(
                ErrorReason::Unauthenticated,
                "sign in to follow repos",
            ));
        }
        if !caller.token.allows_action(Action::Commit) {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "token does not allow `commit`",
            ));
        }
        Ok(())
    }

    /// A follower named by login or principal id (agents are named by id); empty = the
    /// caller.
    async fn follower_id(&self, cx: &Cx, user: &str) -> Result<String> {
        if user.is_empty() {
            return self.uid(cx, "").await;
        }
        match self.community().users.resolve_login(cx, user).await {
            Ok((id, _, _)) => Ok(id),
            Err(e) if e.reason == ErrorReason::NotFound => {
                match self.fg().hooks.directory.get(user).await {
                    Some(p) => Ok(p.id),
                    None => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }
}

impl ForgeServices {
    /// Pinned items of a user, hydrated; items the caller cannot see are left out.
    async fn pinned_items(
        &self,
        caller: &Caller,
        cx: &Cx,
        pid: &str,
    ) -> Result<Vec<UserPinnedItem>> {
        let refs = self.community().users.pinned(cx, pid).await?;
        let mut out = Vec::new();
        for i in refs {
            match UserPinnedItemKind::try_from(i.kind).unwrap_or_default() {
                UserPinnedItemKind::Repo => {
                    let Ok(repo) = self.fg().orgs.repo_any(cx, &i.id).await else {
                        continue;
                    };
                    if repo.deleted_at.is_some()
                        || self
                            .fg()
                            .authz
                            .repo(caller, &repo, Action::Read, &[], None, None)
                            .await
                            .is_err()
                    {
                        continue;
                    }
                    out.push(UserPinnedItem {
                        kind: i.kind,
                        id: repo.id.clone(),
                        title: repo.full_name.clone(),
                        description: repo.description.clone(),
                        language: repo.primary_language.clone(),
                        star_count: self
                            .community()
                            .users
                            .star_count(cx, &repo.id)
                            .await
                            .unwrap_or(0),
                        fork_count: repo.forks,
                    });
                }
                UserPinnedItemKind::Gist => {
                    let Ok(g) = self.community().gists.get(cx, &i.id, None, 0).await else {
                        continue;
                    };
                    if !g.public
                        && g.owner.as_ref().map(|o| o.id.as_str())
                            != Some(caller.cx.actor_id.as_str())
                    {
                        continue;
                    }
                    out.push(UserPinnedItem {
                        kind: i.kind,
                        id: g.id.clone(),
                        title: if g.description.is_empty() {
                            g.files
                                .first()
                                .map(|f| f.filename.clone())
                                .unwrap_or_default()
                        } else {
                            g.description.clone()
                        },
                        description: g.description.clone(),
                        language: g
                            .files
                            .first()
                            .map(|f| f.language.clone())
                            .unwrap_or_default(),
                        star_count: u64::from(g.star_count),
                        fork_count: u64::from(g.fork_count),
                    });
                }
                UserPinnedItemKind::Unspecified => {}
            }
        }
        Ok(out)
    }
}
