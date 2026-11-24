//! `ModerationService`: the 30 RPCs over [`Moderation`].
//!
//! # Who may do what
//!
//! | caller | may |
//! |---|---|
//! | anyone who can read a repo | report its content |
//! | org owners and admins, org moderators (`Role::Moderator` or listed here, directly or through a team), repo maintainers (`manage_settings`), site admins | hide comments, lock conversations, block users in the org, set interaction limits, resolve reports and read the repo and org queues, set first-time and flood settings, release throttles |
//! | org owners and admins | add and remove moderators |
//! | site admins (and org owners for their own repos) | create, list and reinstate takedowns |
//! | a user | manage their own personal block list and their own user-scope limit |
//!
//! Every principal of the delegation chain must qualify, as everywhere in the forge, and the
//! token must allow `triage` (moderator path) or `manage_settings` (maintainer path). Denied
//! state-changing calls are audited through
//! [`ForgeServices::guarded`](crate::service::ForgeServices).

use std::sync::Arc;

use bee_auth::Request as ScopeRequest;
use bee_policy::Action;
use bee_proto::v1::moderation_service_server::{
    ModerationService as ModerationRpc, ModerationServiceServer,
};
use bee_proto::v1::{
    AddModeratorRequest, BlockEntry, BlockScope, BlockUserRequest, CheckUserBlockedRequest,
    CheckUserBlockedResponse, Comment, CreateTakedownRequest, GetContentReportRequest,
    GetFirstTimeContributorSettingsRequest, GetFloodLimitsRequest, GetInteractionLimitRequest,
    GetModerationCommunityProfileRequest, GetTakedownRequest, InteractionLimit,
    InteractionLimitLevel, InteractionLimitScope, ListBlockedUsersRequest,
    ListBlockedUsersResponse, ListCodesOfConductRequest, ListCodesOfConductResponse,
    ListContentReportsRequest, ListContentReportsResponse, ListModeratorsRequest,
    ListModeratorsResponse, ListTakedownsRequest, ListTakedownsResponse,
    ListThrottledPrincipalsRequest, ListThrottledPrincipalsResponse, LockConversationRequest,
    MinimizeCommentRequest, ModerationCommunityProfile, ModerationFirstTimeContributorSettings,
    ModerationFloodLimits, ModerationLock, ModerationModerator, ModerationReport,
    ModerationReportState, ReinstateTakedownRequest, ReleaseThrottledPrincipalRequest,
    ReleaseThrottledPrincipalResponse, RemoveInteractionLimitRequest,
    RemoveInteractionLimitResponse, RemoveModeratorRequest, RemoveModeratorResponse, Repo,
    ReportContentRequest, ResolveContentReportRequest, Role,
    SetFirstTimeContributorSettingsRequest, SetFloodLimitsRequest, SetInteractionLimitRequest,
    SubjectKind, SubjectRef, Takedown, UnblockUserRequest, UnblockUserResponse,
    UnlockConversationRequest, UnlockConversationResponse, UnminimizeCommentRequest,
    add_moderator_request, block_scope, interaction_limit_scope, list_content_reports_request,
    moderation_moderator, remove_moderator_request,
};
use tonic::{Request, Response, Status};

use super::blocks::{BlockTarget, NewBlock};
use super::comments::CommentRef;
use super::contributors::{SettingsPatch, SettingsTarget};
use super::flood::FloodPatch;
use super::limits::{LimitTarget, NewLimit};
use super::moderators::ModeratorRef;
use super::reports::{NewReport, ReportQueue};
use super::takedowns::NewTakedown;
use super::{Moderation, subject_in};
use crate::auth::{Caller, OrgPerm, authorize_repo_scope};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::service::{ForgeServices, next, page, reply, unserved};

/// `ModerationService` over a [`Moderation`] and the forge services' helpers.
#[derive(Clone)]
pub struct ModerationService {
    m: Arc<Moderation>,
    s: ForgeServices,
}

impl ModerationService {
    /// The service over `m`; `s` supplies authentication and the shared helpers.
    pub fn new(m: Arc<Moderation>, s: ForgeServices) -> ModerationService {
        ModerationService { m, s }
    }

    /// The tonic server for registration (`routes.add_service(...)`).
    pub fn server(
        m: Arc<Moderation>,
        s: ForgeServices,
    ) -> ModerationServiceServer<ModerationService> {
        ModerationServiceServer::new(ModerationService::new(m, s))
    }

    fn now_secs(&self) -> u64 {
        self.s.forge().db.now_ms() / 1000
    }

    /// The role that makes `id` staff of the org: `owner`, `admin` or `moderator`.
    async fn org_staff(&self, cx: &Cx, org_id: &str, id: &str) -> Result<Option<&'static str>> {
        let orgs = &self.s.forge().orgs;
        match orgs
            .member(cx, org_id, id)
            .await?
            .map(|m| Role::try_from(m.role).unwrap_or(Role::Member))
        {
            Some(Role::Owner) => return Ok(Some("owner")),
            Some(Role::Admin) => return Ok(Some("admin")),
            _ => {}
        }
        Ok(self
            .m
            .is_moderator(cx, org_id, id)
            .await?
            .then_some("moderator"))
    }

    /// Authorizes org-level moderation: site admins, or every principal of the chain being an
    /// owner, admin or moderator of the org.
    async fn staff_org(&self, caller: &Caller, org_id: &str) -> Result<Cx> {
        let mut cx = caller.cx.clone();
        if self.m.is_site_admin(&cx) {
            cx.decision = "site_admin".into();
            return Ok(cx);
        }
        if caller.token.scope.repos.is_some() || !caller.token.allows_action(Action::Triage) {
            return Err(ForgeError::policy_denied(
                "token_scope",
                vec!["triage".into()],
                "org moderation needs a token that allows `triage` on every repo",
            ));
        }
        let mut decision = vec!["token:ok".to_string()];
        for id in caller.cx.chain.clone() {
            let Some(role) = self.org_staff(&caller.cx, org_id, &id).await? else {
                return Err(ForgeError::permission_denied(format!(
                    "{id} is not a moderator of {org_id}"
                )));
            };
            decision.push(format!("{id}:moderation:{role}"));
        }
        cx.decision = decision.join(" ");
        Ok(cx)
    }

    /// Authorizes moderation of one repo: org staff (see [`Self::staff_org`], with the token
    /// checked against the repo) or repo maintainers (`manage_settings`).
    async fn staff_repo(&self, caller: &Caller, repo: &Repo) -> Result<Cx> {
        if self.m.is_site_admin(&caller.cx) {
            let mut cx = caller.cx.clone();
            cx.decision = "site_admin".into();
            return Ok(cx);
        }
        let token_ok = authorize_repo_scope(
            &caller.token,
            repo,
            |key| ScopeRequest::new(key, Action::Triage),
            self.now_secs(),
        )
        .map_err(ForgeError::from);
        if token_ok.is_ok() {
            let mut decision = vec!["token:ok".to_string()];
            let mut all = true;
            for id in caller.cx.chain.clone() {
                match self.org_staff(&caller.cx, &repo.org_id, &id).await? {
                    Some(role) => decision.push(format!("{id}:moderation:{role}")),
                    None => {
                        all = false;
                        break;
                    }
                }
            }
            if all {
                let mut cx = caller.cx.clone();
                cx.decision = decision.join(" ");
                return Ok(cx);
            }
        }
        self.s
            .forge()
            .authz
            .repo(caller, repo, Action::ManageSettings, &[], None, None)
            .await
    }

    /// Org owners and admins (or site admins).
    async fn manage_org(&self, caller: &Caller, org_id: &str) -> Result<Cx> {
        if self.m.is_site_admin(&caller.cx) {
            let mut cx = caller.cx.clone();
            cx.decision = "site_admin".into();
            return Ok(cx);
        }
        self.s
            .forge()
            .authz
            .org(caller, org_id, OrgPerm::Manage)
            .await
    }

    /// Site admins, or org owners of `org_id`.
    async fn takedown_auth(&self, caller: &Caller, org_id: &str) -> Result<Cx> {
        if self.m.is_site_admin(&caller.cx) {
            let mut cx = caller.cx.clone();
            cx.decision = "site_admin".into();
            return Ok(cx);
        }
        self.s.forge().authz.org(caller, org_id, OrgPerm::Own).await
    }

    fn require_site_admin(&self, caller: &Caller) -> Result<Cx> {
        if self.m.is_site_admin(&caller.cx) {
            let mut cx = caller.cx.clone();
            cx.decision = "site_admin".into();
            Ok(cx)
        } else {
            Err(ForgeError::permission_denied(
                "this needs a site administrator",
            ))
        }
    }

    /// The user whose personal lists the caller manages: the delegation root.
    fn personal_id(&self, caller: &Caller) -> Result<String> {
        if !caller.token.allows_action(Action::Read) {
            return Err(ForgeError::permission_denied("token does not allow `read`"));
        }
        Ok(caller.cx.root().to_string())
    }

    /// Resolves a block scope; `write` audits a denial as `action`.
    async fn block_target(
        &self,
        caller: &Caller,
        scope: &Option<BlockScope>,
        write: Option<&str>,
    ) -> Result<(BlockTarget, Cx)> {
        match scope.as_ref().and_then(|s| s.scope.as_ref()) {
            Some(block_scope::Scope::Org(name)) => {
                let org_id = self.s.org_id(caller, name).await?;
                let auth = self.staff_org(caller, &org_id).await;
                let cx = match write {
                    Some(a) => self.s.guarded(caller, &org_id, "", a, auth).await?,
                    None => auth?,
                };
                Ok((BlockTarget::Org(org_id), cx))
            }
            Some(block_scope::Scope::Personal(true)) => {
                let id = self.personal_id(caller)?;
                let mut cx = caller.cx.clone();
                cx.decision = "personal".into();
                Ok((BlockTarget::Personal(id), cx))
            }
            _ => Err(ForgeError::missing("scope")),
        }
    }

    /// The block entry with the org shown by name.
    async fn block_view(&self, cx: &Cx, mut e: BlockEntry) -> Result<BlockEntry> {
        if let Some(BlockScope {
            scope: Some(block_scope::Scope::Org(id)),
        }) = &e.scope
        {
            let org = self.s.forge().orgs.org_any(cx, id).await?;
            e.scope = Some(BlockScope {
                scope: Some(block_scope::Scope::Org(org.name)),
            });
        }
        Ok(e)
    }

    /// Resolves an interaction-limit scope and authorizes it (`write` = state-changing).
    async fn limit_target(
        &self,
        caller: &Caller,
        scope: &Option<InteractionLimitScope>,
        write: Option<&str>,
    ) -> Result<(LimitTarget, String, Cx, Option<Repo>)> {
        match scope.as_ref().and_then(|s| s.scope.as_ref()) {
            Some(interaction_limit_scope::Scope::Repo(r)) => {
                let repo = self.s.repo(caller, &Some(r.clone())).await?;
                let cx = match write {
                    Some(a) => {
                        let auth = self.staff_repo(caller, &repo).await;
                        self.s
                            .guarded(caller, &repo.org_id, &repo.id, a, auth)
                            .await?
                    }
                    None => {
                        self.s
                            .forge()
                            .authz
                            .repo(caller, &repo, Action::Read, &[], None, None)
                            .await?
                    }
                };
                Ok((
                    LimitTarget::Repo(repo.id.clone()),
                    repo.org_id.clone(),
                    cx,
                    Some(repo),
                ))
            }
            Some(interaction_limit_scope::Scope::Org(name)) => {
                let org_id = self.s.org_id(caller, name).await?;
                let cx = match write {
                    Some(a) => {
                        let auth = self.staff_org(caller, &org_id).await;
                        self.s.guarded(caller, &org_id, "", a, auth).await?
                    }
                    None => {
                        self.s
                            .forge()
                            .authz
                            .org(caller, &org_id, OrgPerm::ViewMembers)
                            .await?
                    }
                };
                Ok((LimitTarget::Org(org_id.clone()), org_id, cx, None))
            }
            Some(interaction_limit_scope::Scope::UserId(id)) => {
                if id.is_empty() {
                    return Err(ForgeError::missing("user_id"));
                }
                let me = self.personal_id(caller)?;
                if *id != me && *id != caller.cx.actor_id && !self.m.is_site_admin(&caller.cx) {
                    return Err(ForgeError::permission_denied(
                        "a user limit is set by that user",
                    ));
                }
                let mut cx = caller.cx.clone();
                cx.decision = "personal".into();
                Ok((LimitTarget::User(id.clone()), String::new(), cx, None))
            }
            None => Err(ForgeError::missing("scope")),
        }
    }

    /// A comment from the comment source (`UNIMPLEMENTED` without one).
    async fn comment_of(&self, caller: &Caller, id: &str) -> Result<Comment> {
        let Some(src) = &self.m.cfg.comments else {
            return Err(ForgeError::unimplemented(
                "comments are not available to moderation on this deployment",
            )
            .with_subcode("no_comment_source"));
        };
        if id.is_empty() {
            return Err(ForgeError::missing("comment_id"));
        }
        src.comment(&caller.cx, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("comment {id}")))
    }

    /// The comment, its repo and the authorized context for moderating it.
    async fn comment_ctx(
        &self,
        caller: &Caller,
        comment_id: &str,
        action: &str,
    ) -> Result<(Cx, CommentRef)> {
        let comment = self.comment_of(caller, comment_id).await?;
        let repo = self
            .s
            .repo(
                caller,
                &comment.subject.as_ref().and_then(|s| s.repo.clone()),
            )
            .await?;
        let auth = self.staff_repo(caller, &repo).await;
        let cx = self
            .s
            .guarded(caller, &repo.org_id, &repo.id, action, auth)
            .await?;
        Ok((
            cx,
            CommentRef {
                comment_id: comment.id.clone(),
                org_id: repo.org_id.clone(),
                repo_id: repo.id.clone(),
                subject: subject_in(&repo.id, SubjectKind::Comment, &comment.id),
            },
        ))
    }

    /// Repo of a lockable or takedown subject.
    async fn subject_repo(&self, caller: &Caller, subject: &Option<SubjectRef>) -> Result<Repo> {
        let s = subject
            .as_ref()
            .ok_or_else(|| ForgeError::missing("subject"))?;
        match &s.repo {
            Some(_) => self.s.repo(caller, &s.repo).await,
            None => Err(ForgeError::missing("subject.repo")),
        }
    }

    /// Org name for display.
    async fn org_name(&self, cx: &Cx, org_id: &str) -> Result<String> {
        Ok(self.s.forge().orgs.org_any(cx, org_id).await?.name)
    }
}

fn moderator_of(org: &str, p: &Option<add_moderator_request::Moderator>) -> Result<ModeratorRef> {
    let _ = org;
    match p {
        Some(add_moderator_request::Moderator::PrincipalId(id)) => {
            Ok(ModeratorRef::Principal(id.clone()))
        }
        Some(add_moderator_request::Moderator::TeamId(id)) => Ok(ModeratorRef::Team(id.clone())),
        None => Err(ForgeError::missing("moderator")),
    }
}

#[tonic::async_trait]
impl ModerationRpc for ModerationService {
    // ---- Blocks ----

    async fn block_user(
        &self,
        req: Request<BlockUserRequest>,
    ) -> std::result::Result<Response<BlockEntry>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            if r.principal_id.is_empty() {
                return Err(ForgeError::missing("principal_id"));
            }
            let (target, cx) = self
                .block_target(&caller, &r.scope, Some("moderation.block"))
                .await?;
            let blocked = self.m.principal(&r.principal_id).await;
            if let BlockTarget::Org(org_id) = &target
                && self.org_staff(&cx, org_id, &r.principal_id).await? == Some("owner")
            {
                return Err(ForgeError::invalid_state("an org owner cannot be blocked"));
            }
            let entry = self
                .m
                .block(
                    &cx,
                    &target,
                    NewBlock {
                        blocked,
                        reason: r.reason.clone(),
                        note: r.note.clone(),
                        duration_secs: r.duration_seconds,
                        source: r.source.clone(),
                    },
                )
                .await?;
            if r.hide_content {
                self.m
                    .hide_user_content(&cx, &target, &r.principal_id)
                    .await?;
            }
            self.block_view(&cx, entry).await
        }
        .await;
        reply(&caller, out)
    }

    async fn unblock_user(
        &self,
        req: Request<UnblockUserRequest>,
    ) -> std::result::Result<Response<UnblockUserResponse>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (target, cx) = self
                .block_target(&caller, &r.scope, Some("moderation.unblock"))
                .await?;
            self.m.unblock(&cx, &target, &r.principal_id).await?;
            Ok(UnblockUserResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_blocked_users(
        &self,
        req: Request<ListBlockedUsersRequest>,
    ) -> std::result::Result<Response<ListBlockedUsersResponse>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (target, cx) = self.block_target(&caller, &r.scope, None).await?;
            let (after, size) = page(&r.page);
            let (rows, token) = self.m.list_blocks(&cx, &target, &after, size).await?;
            let mut blocks = Vec::new();
            for b in rows {
                blocks.push(self.block_view(&cx, b).await?);
            }
            Ok(ListBlockedUsersResponse {
                blocks,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn check_user_blocked(
        &self,
        req: Request<CheckUserBlockedRequest>,
    ) -> std::result::Result<Response<CheckUserBlockedResponse>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (target, cx) = self.block_target(&caller, &r.scope, None).await?;
            let found = self.m.blocked(&cx, &target, &r.principal_id).await?;
            Ok(match found {
                Some(b) => CheckUserBlockedResponse {
                    blocked: true,
                    block: Some(self.block_view(&cx, b).await?),
                },
                None => CheckUserBlockedResponse {
                    blocked: false,
                    block: None,
                },
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---- Reports ----

    async fn report_content(
        &self,
        req: Request<ReportContentRequest>,
    ) -> std::result::Result<Response<ModerationReport>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let mut subject = r
                .subject
                .clone()
                .ok_or_else(|| ForgeError::missing("subject"))?;
            let mut author = None;
            if subject.kind == SubjectKind::Comment as i32
                && let Some(src) = &self.m.cfg.comments
                && let Some(c) = src.comment(&caller.cx, &subject.id).await?
            {
                author = c.author.clone();
                if subject.repo.is_none() {
                    subject.repo = c.subject.and_then(|s| s.repo);
                }
            }
            let (repo, cx) = match &subject.repo {
                Some(_) => {
                    let repo = self.s.repo(&caller, &subject.repo).await?;
                    // Reporters need only read access.
                    let cx = self
                        .s
                        .forge()
                        .authz
                        .repo(&caller, &repo, Action::Read, &[], None, None)
                        .await?;
                    (Some(repo), cx)
                }
                None => {
                    if !caller.token.allows_action(Action::Read) {
                        return Err(ForgeError::permission_denied("token does not allow `read`"));
                    }
                    (None, caller.cx.clone())
                }
            };
            self.m
                .create_report(
                    &cx,
                    NewReport {
                        subject,
                        category: bee_proto::v1::ModerationReportCategory::try_from(r.category)
                            .unwrap_or(bee_proto::v1::ModerationReportCategory::Unspecified),
                        audience: bee_proto::v1::ModerationReportAudience::try_from(r.audience)
                            .unwrap_or(bee_proto::v1::ModerationReportAudience::Unspecified),
                        body: r.body.clone(),
                        org_id: repo.as_ref().map(|p| p.org_id.clone()),
                        repo_id: repo.as_ref().map(|p| p.id.clone()),
                        content_author: author,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_content_reports(
        &self,
        req: Request<ListContentReportsRequest>,
    ) -> std::result::Result<Response<ListContentReportsResponse>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (queue, cx) = match &r.queue {
                Some(list_content_reports_request::Queue::Repo(rr)) => {
                    let repo = self.s.repo(&caller, &Some(rr.clone())).await?;
                    let cx = self.staff_repo(&caller, &repo).await?;
                    (ReportQueue::Repo(repo.id), cx)
                }
                Some(list_content_reports_request::Queue::Org(name)) => {
                    let org_id = self.s.org_id(&caller, name).await?;
                    let cx = self.staff_org(&caller, &org_id).await?;
                    (ReportQueue::Org(org_id), cx)
                }
                Some(list_content_reports_request::Queue::Site(true)) => {
                    (ReportQueue::Site, self.require_site_admin(&caller)?)
                }
                _ => return Err(ForgeError::missing("queue")),
            };
            let (after, size) = page(&r.page);
            let (reports, token) = self
                .m
                .list_reports(
                    &cx,
                    &queue,
                    ModerationReportState::try_from(r.state)
                        .unwrap_or(ModerationReportState::Unspecified),
                    bee_proto::v1::ModerationReportCategory::try_from(r.category)
                        .unwrap_or(bee_proto::v1::ModerationReportCategory::Unspecified),
                    &after,
                    size,
                )
                .await?;
            Ok(ListContentReportsResponse {
                reports,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_content_report(
        &self,
        req: Request<GetContentReportRequest>,
    ) -> std::result::Result<Response<ModerationReport>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let stored = self.m.report(&caller.cx, &r.id).await?;
            let own = stored
                .report
                .reporter
                .as_ref()
                .is_some_and(|p| p.id == caller.cx.actor_id);
            if !own {
                self.report_staff(&caller, &stored).await?;
            }
            Ok(stored.report)
        }
        .await;
        reply(&caller, out)
    }

    async fn resolve_content_report(
        &self,
        req: Request<ResolveContentReportRequest>,
    ) -> std::result::Result<Response<ModerationReport>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let stored = self.m.report(&caller.cx, &r.id).await?;
            let auth = self.report_staff(&caller, &stored).await;
            let cx = self
                .s
                .guarded(
                    &caller,
                    stored.org_id.as_deref().unwrap_or(""),
                    stored.repo_id.as_deref().unwrap_or(""),
                    "moderation.report_resolve",
                    auth,
                )
                .await?;
            self.m
                .resolve_report(
                    &cx,
                    &r.id,
                    ModerationReportState::try_from(r.state)
                        .unwrap_or(ModerationReportState::Unspecified),
                    bee_proto::v1::ModerationReportAction::try_from(r.action)
                        .unwrap_or(bee_proto::v1::ModerationReportAction::Unspecified),
                    &r.note,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    // ---- Interaction limits ----

    async fn get_interaction_limit(
        &self,
        req: Request<GetInteractionLimitRequest>,
    ) -> std::result::Result<Response<InteractionLimit>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (target, _, cx, repo) = self.limit_target(&caller, &r.scope, None).await?;
            let found = match (&target, &repo) {
                (LimitTarget::Repo(id), Some(repo)) => {
                    self.m.effective_limit(&cx, id, &repo.org_id, None).await?
                }
                _ => self.m.limit(&cx, &target).await?,
            };
            Ok(match found {
                Some(mut l) => {
                    l.scope = r.scope.clone();
                    l
                }
                None => InteractionLimit {
                    scope: r.scope.clone(),
                    level: InteractionLimitLevel::Unspecified as i32,
                    ..InteractionLimit::default()
                },
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_interaction_limit(
        &self,
        req: Request<SetInteractionLimitRequest>,
    ) -> std::result::Result<Response<InteractionLimit>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (target, audit_org, cx, _) = self
                .limit_target(&caller, &r.scope, Some("moderation.limit_set"))
                .await?;
            let mut l = self
                .m
                .set_limit(
                    &cx,
                    &target,
                    &audit_org,
                    NewLimit {
                        level: InteractionLimitLevel::try_from(r.level)
                            .unwrap_or(InteractionLimitLevel::Unspecified),
                        duration_secs: r.duration_seconds,
                        applies_to_agents: r.applies_to_agents,
                    },
                )
                .await?;
            l.scope = r.scope.clone();
            Ok(l)
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_interaction_limit(
        &self,
        req: Request<RemoveInteractionLimitRequest>,
    ) -> std::result::Result<Response<RemoveInteractionLimitResponse>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (target, audit_org, cx, _) = self
                .limit_target(&caller, &r.scope, Some("moderation.limit_remove"))
                .await?;
            self.m.remove_limit(&cx, &target, &audit_org).await?;
            Ok(RemoveInteractionLimitResponse {})
        }
        .await;
        reply(&caller, out)
    }

    // ---- Comments and conversations ----

    async fn minimize_comment(
        &self,
        req: Request<MinimizeCommentRequest>,
    ) -> std::result::Result<Response<Comment>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, c) = self
                .comment_ctx(&caller, &r.comment_id, "moderation.minimize")
                .await?;
            self.m
                .minimize(
                    &cx,
                    &c,
                    bee_proto::v1::MinimizedReason::try_from(r.reason)
                        .unwrap_or(bee_proto::v1::MinimizedReason::Unspecified),
                    "rpc",
                )
                .await?;
            self.m.comment_view(&cx, &c).await
        }
        .await;
        reply(&caller, out)
    }

    async fn unminimize_comment(
        &self,
        req: Request<UnminimizeCommentRequest>,
    ) -> std::result::Result<Response<Comment>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, c) = self
                .comment_ctx(&caller, &r.comment_id, "moderation.unminimize")
                .await?;
            self.m.unminimize(&cx, &c).await?;
            self.m.comment_view(&cx, &c).await
        }
        .await;
        reply(&caller, out)
    }

    async fn lock_conversation(
        &self,
        req: Request<LockConversationRequest>,
    ) -> std::result::Result<Response<ModerationLock>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.subject_repo(&caller, &r.subject).await?;
            let auth = self.staff_repo(&caller, &repo).await;
            let cx = self
                .s
                .guarded(&caller, &repo.org_id, &repo.id, "moderation.lock", auth)
                .await?;
            let subject = r.subject.clone().unwrap_or_default();
            self.m
                .lock_conversation(
                    &cx,
                    &repo.org_id,
                    &repo.id,
                    &subject,
                    bee_proto::v1::ModerationLockReason::try_from(r.reason)
                        .unwrap_or(bee_proto::v1::ModerationLockReason::Unspecified),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn unlock_conversation(
        &self,
        req: Request<UnlockConversationRequest>,
    ) -> std::result::Result<Response<UnlockConversationResponse>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.subject_repo(&caller, &r.subject).await?;
            let auth = self.staff_repo(&caller, &repo).await;
            let cx = self
                .s
                .guarded(&caller, &repo.org_id, &repo.id, "moderation.unlock", auth)
                .await?;
            let subject = r.subject.clone().unwrap_or_default();
            self.m
                .unlock_conversation(&cx, &repo.org_id, &repo.id, &subject)
                .await?;
            Ok(UnlockConversationResponse {})
        }
        .await;
        reply(&caller, out)
    }

    // ---- Moderators ----

    async fn add_moderator(
        &self,
        req: Request<AddModeratorRequest>,
    ) -> std::result::Result<Response<ModerationModerator>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.s.org_id(&caller, &r.org).await?;
            let who = moderator_of(&r.org, &r.moderator)?;
            let auth = self.manage_org(&caller, &org_id).await;
            let cx = self
                .s
                .guarded(&caller, &org_id, "", "moderation.moderator_add", auth)
                .await?;
            let mut m = self.m.add_moderator(&cx, &org_id, &who).await?;
            m.org = self.org_name(&cx, &org_id).await?;
            Ok(m)
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_moderator(
        &self,
        req: Request<RemoveModeratorRequest>,
    ) -> std::result::Result<Response<RemoveModeratorResponse>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.s.org_id(&caller, &r.org).await?;
            let who = match &r.moderator {
                Some(remove_moderator_request::Moderator::PrincipalId(id)) => {
                    ModeratorRef::Principal(id.clone())
                }
                Some(remove_moderator_request::Moderator::TeamId(id)) => {
                    ModeratorRef::Team(id.clone())
                }
                None => return Err(ForgeError::missing("moderator")),
            };
            let auth = self.manage_org(&caller, &org_id).await;
            let cx = self
                .s
                .guarded(&caller, &org_id, "", "moderation.moderator_remove", auth)
                .await?;
            self.m.remove_moderator(&cx, &org_id, &who).await?;
            Ok(RemoveModeratorResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_moderators(
        &self,
        req: Request<ListModeratorsRequest>,
    ) -> std::result::Result<Response<ListModeratorsResponse>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.s.org_id(&caller, &r.org).await?;
            let cx = if self.m.is_site_admin(&caller.cx) {
                caller.cx.clone()
            } else {
                self.s
                    .forge()
                    .authz
                    .org(&caller, &org_id, OrgPerm::ViewMembers)
                    .await?
            };
            let name = self.org_name(&cx, &org_id).await?;
            let (after, size) = page(&r.page);
            let (rows, token) = self.m.list_moderators(&cx, &org_id, &after, size).await?;
            let mut moderators: Vec<ModerationModerator> = rows
                .into_iter()
                .map(|mut m| {
                    m.org = name.clone();
                    m
                })
                .collect();
            if token.is_empty() {
                // Members with the moderator role are moderators too; they are listed with
                // the last page.
                let listed: Vec<String> = moderators
                    .iter()
                    .filter_map(|m| match &m.moderator {
                        Some(moderation_moderator::Moderator::Principal(p)) => Some(p.id.clone()),
                        _ => None,
                    })
                    .collect();
                for p in self.m.role_moderators(&cx, &org_id).await? {
                    if !listed.contains(&p.id) {
                        moderators.push(ModerationModerator {
                            org: name.clone(),
                            moderator: Some(moderation_moderator::Moderator::Principal(p)),
                            added_by: None,
                            added_at: None,
                        });
                    }
                }
            }
            Ok(ListModeratorsResponse {
                moderators,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---- First-time contributors ----

    async fn get_first_time_contributor_settings(
        &self,
        req: Request<GetFirstTimeContributorSettingsRequest>,
    ) -> std::result::Result<Response<ModerationFirstTimeContributorSettings>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            if r.repo.is_some() {
                let repo = self.s.repo(&caller, &r.repo).await?;
                let cx = self
                    .s
                    .forge()
                    .authz
                    .repo(&caller, &repo, Action::Read, &[], None, None)
                    .await?;
                self.m
                    .first_time_settings(&cx, &repo.id, &repo.org_id)
                    .await
            } else {
                let org_id = self.s.org_id(&caller, &r.org).await?;
                let cx = self
                    .s
                    .forge()
                    .authz
                    .org(&caller, &org_id, OrgPerm::ViewMembers)
                    .await?;
                let mut s = self.m.org_first_time_settings(&cx, &org_id).await?;
                s.org = self.org_name(&cx, &org_id).await?;
                Ok(s)
            }
        }
        .await;
        reply(&caller, out)
    }

    async fn set_first_time_contributor_settings(
        &self,
        req: Request<SetFirstTimeContributorSettingsRequest>,
    ) -> std::result::Result<Response<ModerationFirstTimeContributorSettings>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let patch = SettingsPatch {
                run_approval: r
                    .run_approval
                    .and_then(|v| bee_proto::v1::ModerationRunApproval::try_from(v).ok()),
                label_first_contributions: r.label_first_contributions,
                label: r.label.clone(),
                hold_change_requests: r.hold_change_requests,
                treat_new_agents_as_first_time: r.treat_new_agents_as_first_time,
            };
            if r.repo.is_some() {
                let repo = self.s.repo(&caller, &r.repo).await?;
                let auth = self.staff_repo(&caller, &repo).await;
                let cx = self
                    .s
                    .guarded(
                        &caller,
                        &repo.org_id,
                        &repo.id,
                        "moderation.first_time_settings_set",
                        auth,
                    )
                    .await?;
                self.m
                    .set_first_time_settings(
                        &cx,
                        &SettingsTarget::Repo {
                            repo_id: repo.id.clone(),
                            org_id: repo.org_id.clone(),
                        },
                        patch,
                    )
                    .await
            } else {
                let org_id = self.s.org_id(&caller, &r.org).await?;
                let auth = self.staff_org(&caller, &org_id).await;
                let cx = self
                    .s
                    .guarded(
                        &caller,
                        &org_id,
                        "",
                        "moderation.first_time_settings_set",
                        auth,
                    )
                    .await?;
                let mut s = self
                    .m
                    .set_first_time_settings(&cx, &SettingsTarget::Org(org_id.clone()), patch)
                    .await?;
                s.org = self.org_name(&cx, &org_id).await?;
                Ok(s)
            }
        }
        .await;
        reply(&caller, out)
    }

    // ---- Community profile ----

    async fn get_moderation_community_profile(
        &self,
        req: Request<GetModerationCommunityProfileRequest>,
    ) -> std::result::Result<Response<ModerationCommunityProfile>, Status> {
        let caller = self.s.caller(&req)?;
        reply(
            &caller,
            Err(unserved("GetModerationCommunityProfile", "community")),
        )
    }

    async fn list_codes_of_conduct(
        &self,
        req: Request<ListCodesOfConductRequest>,
    ) -> std::result::Result<Response<ListCodesOfConductResponse>, Status> {
        let caller = self.s.caller(&req)?;
        reply(
            &caller,
            Ok(ListCodesOfConductResponse {
                codes_of_conduct: super::codes::templates(),
            }),
        )
    }

    // ---- Agent-flood defenses ----

    async fn get_flood_limits(
        &self,
        req: Request<GetFloodLimitsRequest>,
    ) -> std::result::Result<Response<ModerationFloodLimits>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.s.repo(&caller, &r.repo).await?;
            let cx = self.staff_repo(&caller, &repo).await?;
            self.m.flood_limits(&cx, &repo.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_flood_limits(
        &self,
        req: Request<SetFloodLimitsRequest>,
    ) -> std::result::Result<Response<ModerationFloodLimits>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.s.repo(&caller, &r.repo).await?;
            let auth = self.staff_repo(&caller, &repo).await;
            let cx = self
                .s
                .guarded(
                    &caller,
                    &repo.org_id,
                    &repo.id,
                    "moderation.flood_limits_set",
                    auth,
                )
                .await?;
            self.m
                .set_flood_limits(
                    &cx,
                    &repo.org_id,
                    &repo.id,
                    FloodPatch {
                        max_open_change_requests_per_principal: r
                            .max_open_change_requests_per_principal,
                        max_open_change_requests_per_owner: r.max_open_change_requests_per_owner,
                        max_new_change_requests_per_hour_per_owner: r
                            .max_new_change_requests_per_hour_per_owner,
                        max_new_comments_per_hour_per_owner: r.max_new_comments_per_hour_per_owner,
                        max_new_issues_per_hour_per_owner: r.max_new_issues_per_hour_per_owner,
                        min_reputation: r.min_reputation,
                        exempt_collaborators: r.exempt_collaborators,
                        throttle_duration_seconds: r.throttle_duration_seconds,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_throttled_principals(
        &self,
        req: Request<ListThrottledPrincipalsRequest>,
    ) -> std::result::Result<Response<ListThrottledPrincipalsResponse>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.s.repo(&caller, &r.repo).await?;
            let cx = self.staff_repo(&caller, &repo).await?;
            let (after, size) = page(&r.page);
            let (principals, token) = self
                .m
                .throttled_principals(&cx, &repo.id, &after, size)
                .await?;
            Ok(ListThrottledPrincipalsResponse {
                principals,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn release_throttled_principal(
        &self,
        req: Request<ReleaseThrottledPrincipalRequest>,
    ) -> std::result::Result<Response<ReleaseThrottledPrincipalResponse>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.s.repo(&caller, &r.repo).await?;
            let auth = self.staff_repo(&caller, &repo).await;
            let cx = self
                .s
                .guarded(
                    &caller,
                    &repo.org_id,
                    &repo.id,
                    "moderation.throttle_release",
                    auth,
                )
                .await?;
            self.m
                .release_throttle(&cx, &repo.org_id, &repo.id, &r.principal_id)
                .await?;
            Ok(ReleaseThrottledPrincipalResponse {})
        }
        .await;
        reply(&caller, out)
    }

    // ---- Takedowns ----

    async fn create_takedown(
        &self,
        req: Request<CreateTakedownRequest>,
    ) -> std::result::Result<Response<Takedown>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.subject_repo(&caller, &r.subject).await?;
            let auth = self.takedown_auth(&caller, &repo.org_id).await;
            let cx = self
                .s
                .guarded(
                    &caller,
                    &repo.org_id,
                    &repo.id,
                    "moderation.takedown_create",
                    auth,
                )
                .await?;
            let subject = r.subject.clone().unwrap_or_default();
            self.m
                .create_takedown(
                    &cx,
                    &repo.org_id,
                    &repo.id,
                    &subject,
                    NewTakedown {
                        kind: bee_proto::v1::TakedownKind::try_from(r.kind)
                            .unwrap_or(bee_proto::v1::TakedownKind::Unspecified),
                        notice_text: r.notice_text.clone(),
                        notice_url: r.notice_url.clone(),
                        complainant: r.complainant.clone(),
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_takedowns(
        &self,
        req: Request<ListTakedownsRequest>,
    ) -> std::result::Result<Response<ListTakedownsResponse>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo_id, cx) = if r.repo.is_some() {
                let repo = self.s.repo(&caller, &r.repo).await?;
                let cx = self.takedown_auth(&caller, &repo.org_id).await?;
                (Some(repo.id), cx)
            } else {
                (None, self.require_site_admin(&caller)?)
            };
            let (after, size) = page(&r.page);
            let (takedowns, token) = self
                .m
                .list_takedowns(
                    &cx,
                    repo_id.as_deref(),
                    bee_proto::v1::TakedownState::try_from(r.state)
                        .unwrap_or(bee_proto::v1::TakedownState::Unspecified),
                    bee_proto::v1::TakedownKind::try_from(r.kind)
                        .unwrap_or(bee_proto::v1::TakedownKind::Unspecified),
                    &after,
                    size,
                )
                .await?;
            Ok(ListTakedownsResponse {
                takedowns,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_takedown(
        &self,
        req: Request<GetTakedownRequest>,
    ) -> std::result::Result<Response<Takedown>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let t = self.m.takedown(&caller.cx, &r.id).await?;
            self.takedown_org(&caller, &t, false).await?;
            Ok(t)
        }
        .await;
        reply(&caller, out)
    }

    async fn reinstate_takedown(
        &self,
        req: Request<ReinstateTakedownRequest>,
    ) -> std::result::Result<Response<Takedown>, Status> {
        let caller = self.s.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let t = self.m.takedown(&caller.cx, &r.id).await?;
            let (cx, org_id) = self.takedown_org(&caller, &t, true).await?;
            self.m
                .reinstate_takedown(&cx, &org_id, &r.id, &r.reason)
                .await
        }
        .await;
        reply(&caller, out)
    }
}

impl ModerationService {
    /// Authorizes the staff of the queue a report was filed in.
    async fn report_staff(
        &self,
        caller: &Caller,
        stored: &super::reports::StoredReport,
    ) -> Result<Cx> {
        match (&stored.repo_id, &stored.org_id) {
            (Some(repo_id), _) => {
                let repo = self.s.forge().orgs.repo_any(&caller.cx, repo_id).await?;
                self.staff_repo(caller, &repo).await
            }
            (None, Some(org_id)) => self.staff_org(caller, org_id).await,
            (None, None) => self.require_site_admin(caller),
        }
    }

    /// Authorizes a takedown's org owners and site admins; returns the context and org id.
    async fn takedown_org(
        &self,
        caller: &Caller,
        t: &Takedown,
        write: bool,
    ) -> Result<(Cx, String)> {
        let repo_id = t
            .subject
            .as_ref()
            .and_then(super::subject_repo_id)
            .unwrap_or_default()
            .to_string();
        let repo = self.s.forge().orgs.repo_any(&caller.cx, &repo_id).await?;
        let auth = self.takedown_auth(caller, &repo.org_id).await;
        let cx = if write {
            self.s
                .guarded(
                    caller,
                    &repo.org_id,
                    &repo.id,
                    "moderation.takedown_reinstate",
                    auth,
                )
                .await?
        } else {
            auth?
        };
        Ok((cx, repo.org_id))
    }
}
