//! `DiscussionService`.

use bee_policy::Action;
use bee_proto::v1::discussion_service_server::DiscussionService;
use bee_proto::v1::{
    AddDiscussionLabelsRequest, AddDiscussionUpvoteRequest, AddDiscussionUpvoteResponse,
    CloseDiscussionRequest, ConvertDiscussionToIssueRequest, ConvertDiscussionToIssueResponse,
    ConvertIssueToDiscussionRequest, CreateDiscussionCategoryRequest,
    CreateDiscussionCommentRequest, CreateDiscussionRequest, DeleteDiscussionCategoryRequest,
    DeleteDiscussionCategoryResponse, DeleteDiscussionRequest, DeleteDiscussionResponse,
    Discussion, DiscussionCategory, DiscussionComment, DiscussionPoll, DiscussionStateReason,
    GetDiscussionByNumberRequest, GetDiscussionPollRequest, GetDiscussionRequest,
    ListDiscussionCategoriesRequest, ListDiscussionCategoriesResponse,
    ListDiscussionCommentRepliesRequest, ListDiscussionCommentRepliesResponse,
    ListDiscussionCommentsRequest, ListDiscussionCommentsResponse, ListDiscussionsRequest,
    ListDiscussionsResponse, ListPinnedDiscussionsRequest, ListPinnedDiscussionsResponse,
    LockDiscussionRequest, MarkDiscussionAnswerRequest, PageRequest, PinDiscussionRequest,
    RemoveDiscussionLabelsRequest, RemoveDiscussionUpvoteRequest, RemoveDiscussionUpvoteResponse,
    ReopenDiscussionRequest, Repo, RepoRef, RepoRole, SetDiscussionLabelsRequest,
    UnlockDiscussionRequest, UnmarkDiscussionAnswerRequest, UnpinDiscussionRequest,
    UpdateDiscussionCategoryRequest, UpdateDiscussionRequest, VoteDiscussionPollRequest,
};
use tonic::{Request, Response, Status};

use super::writable;
use crate::auth::{Caller, OrgPerm};
use crate::community::discussions::{
    Discussions, Home, ListFilter, NewDiscussion, UpdateDiscussion, compose_form_body,
    parse_discussion_id, parse_form,
};
use crate::community::ports::{VcsRepo, read_file};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::service::{ForgeServices, next, page, reply};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Read,
    Participate,
    Moderate,
    Admin,
}

struct HomeCtx {
    home: Home,
    repo: Option<Repo>,
}

impl ForgeServices {
    async fn dhome(&self, caller: &Caller, repo: &Option<RepoRef>, org: &str) -> Result<HomeCtx> {
        if !org.is_empty() {
            let o = self.fg().orgs.get_org(&caller.cx, org).await?;
            return Ok(HomeCtx {
                home: Home::of_org(&o.id, &o.name),
                repo: None,
            });
        }
        let r = self.repo(caller, repo).await?;
        Ok(HomeCtx {
            home: Home::of_repo(&r),
            repo: Some(r),
        })
    }

    async fn dhome_of_key(&self, caller: &Caller, key: &str) -> Result<HomeCtx> {
        if let Ok(r) = self.fg().orgs.repo_any(&caller.cx, key).await
            && r.deleted_at.is_none()
        {
            return Ok(HomeCtx {
                home: Home::of_repo(&r),
                repo: Some(r),
            });
        }
        let o = self.fg().orgs.org_any(&caller.cx, key).await?;
        if o.deleted_at.is_some() {
            return Err(ForgeError::not_found(format!("discussion home {key}")));
        }
        Ok(HomeCtx {
            home: Home::of_org(&o.id, &o.name),
            repo: None,
        })
    }

    async fn dhome_of_discussion(&self, caller: &Caller, id: &str) -> Result<(HomeCtx, u64)> {
        let (key, number) = parse_discussion_id(id)?;
        Ok((self.dhome_of_key(caller, &key).await?, number))
    }

    async fn dauth(&self, caller: &Caller, h: &HomeCtx, level: Level, audit: &str) -> Result<Cx> {
        let write = level != Level::Read;
        match &h.repo {
            Some(repo) => {
                let (action, min) = match level {
                    Level::Read | Level::Participate => (Action::Read, None),
                    Level::Moderate => (Action::Triage, None),
                    Level::Admin => (Action::ManageSettings, Some(RepoRole::Maintain)),
                };
                let cx = self
                    .repo_cx(caller, repo, action, min, write.then_some(audit))
                    .await?;
                let settings = self.fg().orgs.repo_settings(&cx, &repo.id).await?;
                if !settings.has_discussions {
                    return Err(ForgeError::invalid_state(
                        "discussions are not enabled for this repo",
                    )
                    .with_subcode("discussions_disabled"));
                }
                if write {
                    writable(repo)?;
                }
                Ok(cx)
            }
            None => {
                let perm = if matches!(level, Level::Moderate | Level::Admin) {
                    OrgPerm::Manage
                } else {
                    OrgPerm::ViewMembers
                };
                let auth = self.fg().authz.org(caller, &h.home.org_id, perm).await;
                if write {
                    self.guarded(caller, &h.home.org_id, "", audit, auth).await
                } else {
                    auth
                }
            }
        }
    }

    async fn dmod(&self, caller: &Caller, h: &HomeCtx) -> bool {
        match &h.repo {
            Some(r) => self.can(caller, r, Action::Triage).await,
            None => self
                .fg()
                .authz
                .org(caller, &h.home.org_id, OrgPerm::Manage)
                .await
                .is_ok(),
        }
    }

    async fn dassoc(&self, cx: &Cx, h: &HomeCtx) -> String {
        match &h.repo {
            Some(r) => match self.fg().orgs.effective_role(cx, r, &cx.actor_id).await {
                Ok(p) => match RepoRole::try_from(p.role).unwrap_or(RepoRole::None) {
                    RepoRole::Admin => "OWNER",
                    RepoRole::Write | RepoRole::Maintain | RepoRole::Triage => "MEMBER",
                    _ => "NONE",
                },
                Err(_) => "NONE",
            },
            None => "MEMBER",
        }
        .to_string()
    }

    /// The form of a category on the repo's default ref, if the repo has one.
    async fn dform(
        &self,
        repo: &Repo,
        slug: &str,
    ) -> Option<Vec<crate::community::discussions::FormField>> {
        let vr = VcsRepo::of_repo(repo);
        let vcs = self.community().ports.vcs.clone();
        let at = vcs.resolve(&vr, "").await.ok()?;
        for path in [
            format!(".github/DISCUSSION_TEMPLATE/{slug}.yml"),
            format!(".github/DISCUSSION_TEMPLATE/{slug}.yaml"),
        ] {
            if let Ok(Some((_, bytes))) = read_file(vcs.as_ref(), &vr, &at, &path).await
                && let Ok(fields) = parse_form(&String::from_utf8_lossy(&bytes))
            {
                return Some(fields);
            }
        }
        None
    }

    fn disc(&self) -> &Discussions {
        &self.community().discussions
    }

    /// For `CommentService`: authorizes an action on a discussion comment. `write` needs
    /// participation (audited as `audit` when denied); the answer carries the authorized
    /// context, the discussion's home and whether the caller moderates it.
    pub(crate) async fn discussion_comment_access(
        &self,
        caller: &Caller,
        comment_id: &str,
        write: bool,
        audit: &str,
    ) -> Result<(Cx, Home, bool)> {
        let (key, _) = Discussions::comment_home(comment_id)?;
        let h = self.dhome_of_key(caller, &key).await?;
        let level = if write {
            Level::Participate
        } else {
            Level::Read
        };
        let cx = self.dauth(caller, &h, level, audit).await?;
        let moderator = self.dmod(caller, &h).await;
        Ok((cx, h.home, moderator))
    }

    /// For `CommentService`: authorizes an action on a discussion (by discussion id).
    pub(crate) async fn discussion_access(
        &self,
        caller: &Caller,
        discussion_id: &str,
        write: bool,
        audit: &str,
    ) -> Result<(Cx, Home)> {
        let (h, _) = self.dhome_of_discussion(caller, discussion_id).await?;
        let level = if write {
            Level::Participate
        } else {
            Level::Read
        };
        let cx = self.dauth(caller, &h, level, audit).await?;
        Ok((cx, h.home))
    }

    /// The discussion store, for `CommentService`.
    pub(crate) fn discussions_store(&self) -> &Discussions {
        self.disc()
    }
}

fn pg(p: &Option<PageRequest>) -> (String, usize) {
    page(p)
}

#[tonic::async_trait]
impl DiscussionService for ForgeServices {
    async fn create_discussion_category(
        &self,
        req: Request<CreateDiscussionCategoryRequest>,
    ) -> Result<Response<DiscussionCategory>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Admin, "discussion.category_create")
                .await?;
            let format =
                bee_proto::v1::DiscussionCategoryFormat::try_from(r.format).unwrap_or_default();
            self.disc()
                .create_category(
                    &cx,
                    &h.home,
                    &r.name,
                    &r.emoji,
                    &r.description,
                    format,
                    &r.section,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_discussion_category(
        &self,
        req: Request<UpdateDiscussionCategoryRequest>,
    ) -> Result<Response<DiscussionCategory>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Admin, "discussion.category_update")
                .await?;
            let format =
                bee_proto::v1::DiscussionCategoryFormat::try_from(r.format).unwrap_or_default();
            self.disc()
                .update_category(
                    &cx,
                    &h.home,
                    &r.category_id,
                    r.name.as_deref(),
                    r.emoji.as_deref(),
                    r.description.as_deref(),
                    format,
                    r.section.as_deref(),
                    r.position,
                    r.expected_version,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_discussion_category(
        &self,
        req: Request<DeleteDiscussionCategoryRequest>,
    ) -> Result<Response<DeleteDiscussionCategoryResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Admin, "discussion.category_delete")
                .await?;
            self.disc()
                .delete_category(&cx, &h.home, &r.category_id, &r.move_to_category_id)
                .await?;
            Ok(DeleteDiscussionCategoryResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_discussion_categories(
        &self,
        req: Request<ListDiscussionCategoriesRequest>,
    ) -> Result<Response<ListDiscussionCategoriesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self.dauth(&caller, &h, Level::Read, "").await?;
            let (token, limit) = pg(&r.page);
            let (categories, nt) = self
                .disc()
                .list_categories(&cx, &h.home, &token, limit)
                .await?;
            Ok(ListDiscussionCategoriesResponse {
                categories,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_discussion(
        &self,
        req: Request<CreateDiscussionRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Participate, "discussion.create")
                .await?;
            let moderator = self.dmod(&caller, &h).await;
            let assoc = self.dassoc(&cx, &h).await;
            let mut body = r.body.clone();
            if let Some(repo) = &h.repo {
                let cat = self.disc().category(&cx, &h.home, &r.category_id).await?;
                if let Some(fields) = self.dform(repo, &cat.slug).await
                    && !(r.form_responses.is_empty() && !r.body.trim().is_empty())
                {
                    body = compose_form_body(&fields, &r.form_responses)?;
                }
            }
            self.disc()
                .create(
                    &cx,
                    &h.home,
                    NewDiscussion {
                        category: r.category_id,
                        title: r.title,
                        body,
                        labels: if moderator { r.labels } else { Vec::new() },
                        poll_question: r.poll_question,
                        poll_options: r.poll_options,
                        form: r.form_responses,
                        author_association: assoc,
                        moderator,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_discussion(
        &self,
        req: Request<GetDiscussionRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (h, number) = self.dhome_of_discussion(&caller, &r.id).await?;
            let cx = self.dauth(&caller, &h, Level::Read, "").await?;
            self.disc().get(&cx, &h.home, number).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_discussion_by_number(
        &self,
        req: Request<GetDiscussionByNumberRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self.dauth(&caller, &h, Level::Read, "").await?;
            self.disc().get(&cx, &h.home, r.number).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_discussions(
        &self,
        req: Request<ListDiscussionsRequest>,
    ) -> Result<Response<ListDiscussionsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self.dauth(&caller, &h, Level::Read, "").await?;
            let (token, limit) = pg(&r.page);
            let f = ListFilter {
                category: r.category_id,
                labels: r.labels,
                author: r.author,
                include_closed: r.include_closed,
                closed_only: r.closed_only,
                answered: r.answered,
                query: r.query,
                sort: r.sort,
                ascending: r.ascending,
            };
            let (discussions, total_count, nt) =
                self.disc().list(&cx, &h.home, &f, &token, limit).await?;
            Ok(ListDiscussionsResponse {
                discussions,
                total_count,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_discussion(
        &self,
        req: Request<UpdateDiscussionRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Participate, "discussion.update")
                .await?;
            let moderator = self.dmod(&caller, &h).await;
            self.disc()
                .update(
                    &cx,
                    &h.home,
                    r.number,
                    UpdateDiscussion {
                        title: r.title,
                        body: r.body,
                        category: r.category_id,
                        expected_version: r.expected_version,
                        moderator,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_discussion(
        &self,
        req: Request<DeleteDiscussionRequest>,
    ) -> Result<Response<DeleteDiscussionResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Participate, "discussion.delete")
                .await?;
            let moderator = self.dmod(&caller, &h).await;
            self.disc()
                .delete(&cx, &h.home, r.number, moderator)
                .await?;
            Ok(DeleteDiscussionResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn close_discussion(
        &self,
        req: Request<CloseDiscussionRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Participate, "discussion.close")
                .await?;
            let moderator = self.dmod(&caller, &h).await;
            let reason = DiscussionStateReason::try_from(r.reason)
                .unwrap_or(DiscussionStateReason::Unspecified);
            self.disc()
                .close(&cx, &h.home, r.number, reason, moderator)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn reopen_discussion(
        &self,
        req: Request<ReopenDiscussionRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Participate, "discussion.reopen")
                .await?;
            let moderator = self.dmod(&caller, &h).await;
            self.disc().reopen(&cx, &h.home, r.number, moderator).await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_discussion_comment(
        &self,
        req: Request<CreateDiscussionCommentRequest>,
    ) -> Result<Response<DiscussionComment>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Participate, "discussion.comment_create")
                .await?;
            let moderator = self.dmod(&caller, &h).await;
            let assoc = self.dassoc(&cx, &h).await;
            self.disc()
                .create_comment(
                    &cx,
                    &h.home,
                    r.number,
                    &r.body,
                    &r.reply_to_comment_id,
                    moderator,
                    &assoc,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_discussion_comments(
        &self,
        req: Request<ListDiscussionCommentsRequest>,
    ) -> Result<Response<ListDiscussionCommentsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self.dauth(&caller, &h, Level::Read, "").await?;
            let (token, limit) = pg(&r.page);
            let (comments, nt) = self
                .disc()
                .list_comments(&cx, &h.home, r.number, &token, limit)
                .await?;
            Ok(ListDiscussionCommentsResponse {
                comments,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_discussion_comment_replies(
        &self,
        req: Request<ListDiscussionCommentRepliesRequest>,
    ) -> Result<Response<ListDiscussionCommentRepliesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (key, _) = Discussions::comment_home(&r.comment_id)?;
            let h = self.dhome_of_key(&caller, &key).await?;
            let cx = self.dauth(&caller, &h, Level::Read, "").await?;
            let (token, limit) = pg(&r.page);
            let (replies, nt) = self
                .disc()
                .list_replies(&cx, &r.comment_id, &token, limit)
                .await?;
            Ok(ListDiscussionCommentRepliesResponse {
                replies,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn mark_discussion_answer(
        &self,
        req: Request<MarkDiscussionAnswerRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (key, _) = Discussions::comment_home(&r.comment_id)?;
            let h = self.dhome_of_key(&caller, &key).await?;
            let cx = self
                .dauth(&caller, &h, Level::Participate, "discussion.answer")
                .await?;
            let moderator = self.dmod(&caller, &h).await;
            self.disc()
                .mark_answer(&cx, &h.home, &r.comment_id, moderator)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn unmark_discussion_answer(
        &self,
        req: Request<UnmarkDiscussionAnswerRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (key, _) = Discussions::comment_home(&r.comment_id)?;
            let h = self.dhome_of_key(&caller, &key).await?;
            let cx = self
                .dauth(&caller, &h, Level::Participate, "discussion.unanswer")
                .await?;
            let moderator = self.dmod(&caller, &h).await;
            self.disc()
                .unmark_answer(&cx, &h.home, &r.comment_id, moderator)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn vote_discussion_poll(
        &self,
        req: Request<VoteDiscussionPollRequest>,
    ) -> Result<Response<DiscussionPoll>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Participate, "discussion.poll_vote")
                .await?;
            self.disc().vote(&cx, &h.home, r.number, &r.option_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_discussion_poll(
        &self,
        req: Request<GetDiscussionPollRequest>,
    ) -> Result<Response<DiscussionPoll>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self.dauth(&caller, &h, Level::Read, "").await?;
            self.disc().poll(&cx, &h.home, r.number).await
        }
        .await;
        reply(&caller, out)
    }

    async fn lock_discussion(
        &self,
        req: Request<LockDiscussionRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Moderate, "discussion.lock")
                .await?;
            let reason =
                bee_proto::v1::DiscussionLockReason::try_from(r.reason).unwrap_or_default();
            self.disc()
                .set_locked(&cx, &h.home, r.number, true, reason)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn unlock_discussion(
        &self,
        req: Request<UnlockDiscussionRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Moderate, "discussion.unlock")
                .await?;
            self.disc()
                .set_locked(&cx, &h.home, r.number, false, Default::default())
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn pin_discussion(
        &self,
        req: Request<PinDiscussionRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Admin, "discussion.pin")
                .await?;
            self.disc()
                .set_pinned(&cx, &h.home, r.number, true, &r.pattern, &r.gradient)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn unpin_discussion(
        &self,
        req: Request<UnpinDiscussionRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Admin, "discussion.unpin")
                .await?;
            self.disc()
                .set_pinned(&cx, &h.home, r.number, false, "", "")
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_pinned_discussions(
        &self,
        req: Request<ListPinnedDiscussionsRequest>,
    ) -> Result<Response<ListPinnedDiscussionsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self.dauth(&caller, &h, Level::Read, "").await?;
            Ok(ListPinnedDiscussionsResponse {
                discussions: self.disc().list_pinned(&cx, &h.home).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn convert_issue_to_discussion(
        &self,
        req: Request<ConvertIssueToDiscussionRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, "").await?;
            let cx = self
                .dauth(
                    &caller,
                    &h,
                    Level::Moderate,
                    "discussion.convert_from_issue",
                )
                .await?;
            let repo = h.repo.as_ref().ok_or_else(|| ForgeError::missing("repo"))?;
            self.disc()
                .convert_from_issue(
                    &cx,
                    repo,
                    r.issue_number,
                    &r.category_id,
                    self.community().ports.issues.as_ref(),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn convert_discussion_to_issue(
        &self,
        req: Request<ConvertDiscussionToIssueRequest>,
    ) -> Result<Response<ConvertDiscussionToIssueResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Moderate, "discussion.convert_to_issue")
                .await?;
            let target = match (&h.repo, &r.target_repo) {
                (_, Some(t)) if t.repo.is_some() => self.repo(&caller, &r.target_repo).await?,
                (Some(repo), _) => repo.clone(),
                (None, _) => return Err(ForgeError::missing("target_repo")),
            };
            // Opening an issue needs triage on the target repo.
            self.repo_cx(
                &caller,
                &target,
                Action::Triage,
                None,
                Some("discussion.convert_to_issue"),
            )
            .await?;
            writable(&target)?;
            let (issue, discussion) = self
                .disc()
                .convert_to_issue(
                    &cx,
                    &h.home,
                    r.number,
                    &target,
                    &r.title,
                    &r.body,
                    self.community().ports.issues.as_ref(),
                )
                .await?;
            Ok(ConvertDiscussionToIssueResponse {
                issue: Some(issue),
                discussion: Some(discussion),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn add_discussion_labels(
        &self,
        req: Request<AddDiscussionLabelsRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Moderate, "discussion.label")
                .await?;
            self.disc()
                .add_labels(&cx, &h.home, r.number, &r.labels)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_discussion_labels(
        &self,
        req: Request<RemoveDiscussionLabelsRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Moderate, "discussion.unlabel")
                .await?;
            self.disc()
                .remove_labels(&cx, &h.home, r.number, &r.labels)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_discussion_labels(
        &self,
        req: Request<SetDiscussionLabelsRequest>,
    ) -> Result<Response<Discussion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Moderate, "discussion.label")
                .await?;
            self.disc()
                .set_labels(&cx, &h.home, r.number, &r.labels)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn add_discussion_upvote(
        &self,
        req: Request<AddDiscussionUpvoteRequest>,
    ) -> Result<Response<AddDiscussionUpvoteResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Participate, "discussion.upvote")
                .await?;
            Ok(AddDiscussionUpvoteResponse {
                upvote_count: self
                    .disc()
                    .upvote(&cx, &h.home, r.number, &r.comment_id, true)
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_discussion_upvote(
        &self,
        req: Request<RemoveDiscussionUpvoteRequest>,
    ) -> Result<Response<RemoveDiscussionUpvoteResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let h = self.dhome(&caller, &r.repo, &r.org).await?;
            let cx = self
                .dauth(&caller, &h, Level::Participate, "discussion.unupvote")
                .await?;
            Ok(RemoveDiscussionUpvoteResponse {
                upvote_count: self
                    .disc()
                    .upvote(&cx, &h.home, r.number, &r.comment_id, false)
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }
}
