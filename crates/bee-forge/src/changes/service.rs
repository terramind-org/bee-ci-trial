//! The tonic `ChangeService` over [`Changes`], following the forge service shape
//! (`docs/spec/forge-core.md` §7): authenticate, resolve the repo, authorize (denials of
//! state-changing calls are audited), refuse writes to archived repos, call the store.
//!
//! | RPCs | action |
//! |---|---|
//! | reads (get, list, reviews, threads, comments, diffs, templates, stack, queue, conflicts, evaluation, viewed files, context) | `read` |
//! | open, revise, edit, ready/draft, summary, apply suggestions, update base, revert, conflict resolution | `commit` |
//! | close, reopen, labels, assignees, milestone, linked issues, review requests | `triage` |
//! | submit review, line comments, replies, resolve/unresolve | `review` |
//! | conversation and changeset comments, viewed files, reactions | `read` |
//! | lock or unlock the conversation | `triage` |
//! | dismiss a review | `manage_settings`, maintain |
//! | land, auto-merge, enqueue, dequeue | `land` on the CR's paths and target ref |
//! | queue priority above 0 | additionally `manage_settings`, maintain |

use std::sync::Arc;

use bee_policy::Action;
use bee_proto::v1::change_service_server::{ChangeService, ChangeServiceServer};
use bee_proto::v1::*;
use tonic::{Request, Response, Status};

use super::query::ListFilter;
use super::store::{ChangeEdit, Changes, NewChange};
use crate::auth::Caller;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::service::{ForgeServices, ResponseStream, next, page, reply};

/// `ChangeService` over a forge and its changes module.
#[derive(Clone)]
pub struct ChangeServices {
    base: ForgeServices,
    changes: Arc<Changes>,
}

fn review_to_comment(c: &ReviewComment, repo: &Repo, cr_id: &str) -> Comment {
    Comment {
        id: c.id.clone(),
        subject: Some(SubjectRef {
            kind: SubjectKind::ChangeRequest as i32,
            repo: super::store::repo_ref(repo),
            id: cr_id.to_string(),
        }),
        author: c.author.clone(),
        body: c.body.clone(),
        created_at: c.created_at,
        updated_at: c.created_at,
        in_reply_to: c.in_reply_to.clone(),
        reactions: c.reactions.clone(),
        anchor: Some(super::store::anchor_of(c)),
        ..Default::default()
    }
}

impl ChangeServices {
    /// The service over `forge` and `changes`.
    pub fn new(forge: Arc<Forge>, changes: Arc<Changes>) -> ChangeServices {
        ChangeServices {
            base: ForgeServices::new(forge),
            changes,
        }
    }

    /// The changes module.
    pub fn changes(&self) -> &Arc<Changes> {
        &self.changes
    }

    /// The tonic server.
    pub fn change_service(&self) -> ChangeServiceServer<ChangeServices> {
        ChangeServiceServer::new(self.clone())
    }

    /// Adds `ChangeService` to a tonic router builder.
    pub fn register(&self, routes: &mut tonic::service::RoutesBuilder) {
        routes.add_service(self.change_service());
    }

    fn forge(&self) -> &Arc<Forge> {
        self.base.forge()
    }

    async fn read_cx(&self, caller: &Caller, repo: &Repo) -> Result<Cx> {
        self.forge()
            .authz
            .repo(caller, repo, Action::Read, &[], None, None)
            .await
    }

    async fn write_cx(
        &self,
        caller: &Caller,
        repo: &Repo,
        action: Action,
        audit: &str,
    ) -> Result<Cx> {
        let auth = self
            .forge()
            .authz
            .repo(caller, repo, action, &[], None, None)
            .await;
        let cx = self
            .base
            .guarded(caller, &repo.org_id, &repo.id, audit, auth)
            .await?;
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        Ok(cx)
    }

    /// `commit` on the repo, or, for a change request from a fork, being its author with read
    /// access (outside contributors own the change requests they open from their forks).
    /// Returns the context and whether the caller is the author.
    async fn author_or_write_cx(
        &self,
        caller: &Caller,
        repo: &Repo,
        id: &str,
        audit: &str,
    ) -> Result<(Cx, bool)> {
        let read = self.read_cx(caller, repo).await;
        let read = self
            .base
            .guarded(caller, &repo.org_id, &repo.id, audit, read)
            .await?;
        let (rec, _) = self.changes.record(&read, repo, id).await?;
        let cr = rec.cr();
        let author = cr
            .author
            .as_ref()
            .is_some_and(|a| a.id == caller.cx.actor_id);
        if author && !cr.head_repo_id.is_empty() && cr.head_repo_id != repo.id {
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            return Ok((read, true));
        }
        Ok((
            self.write_cx(caller, repo, Action::Commit, audit).await?,
            author,
        ))
    }

    /// Fills `ChangeRequest.post` of change requests the caller reads in `repo`.
    async fn with_posts(
        &self,
        caller: &Caller,
        cx: &Cx,
        repo: &Repo,
        mut crs: Vec<ChangeRequest>,
    ) -> Result<Vec<ChangeRequest>> {
        crate::posts::service::fill_change_posts(
            self.forge(),
            self.changes.posts(),
            caller,
            cx,
            repo,
            &mut crs,
        )
        .await?;
        Ok(crs)
    }

    async fn with_post(
        &self,
        caller: &Caller,
        cx: &Cx,
        repo: &Repo,
        cr: ChangeRequest,
    ) -> Result<ChangeRequest> {
        let mut v = self.with_posts(caller, cx, repo, vec![cr]).await?;
        v.pop()
            .ok_or_else(|| ForgeError::internal("change request lost while filling its post"))
    }

    /// The forks module, for change requests that come from a fork.
    fn forks(&self) -> Result<Arc<crate::forks::Forks>> {
        self.forge().forks().cloned().ok_or_else(|| {
            crate::service::unserved(
                "ChangeService (fork change requests)",
                "the forks module (crate::forks::Forks::attach)",
            )
        })
    }

    /// Authorizes opening a change request in `repo` whose head lives in fork `fork`: read on
    /// `repo` (outside contributors open change requests from their forks), `commit` on the
    /// fork's `head_ref`, both in one fork network; then copies the head's stack into `repo`
    /// so it can be reviewed, checked and landed there.
    async fn fork_open_cx(
        &self,
        caller: &Caller,
        repo: &Repo,
        fork: &Repo,
        head: Option<&ObjectId>,
        head_ref: &str,
    ) -> Result<Cx> {
        let forks = self.forks()?;
        let read = self.read_cx(caller, repo).await;
        let cx = self
            .base
            .guarded(caller, &repo.org_id, &repo.id, "cr.open", read)
            .await?;
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        let on_ref = (!head_ref.is_empty()).then_some(head_ref);
        let write = self
            .forge()
            .authz
            .repo(caller, fork, Action::Commit, &[], on_ref, None)
            .await;
        self.base
            .guarded(caller, &fork.org_id, &fork.id, "cr.open", write)
            .await?;
        let head = head.ok_or_else(|| ForgeError::missing("head"))?;
        forks.pull_head(&cx, repo, fork, head).await?;
        Ok(cx)
    }

    /// `land` on the CR's paths and target ref.
    async fn land_cx(&self, caller: &Caller, repo: &Repo, id: &str, audit: &str) -> Result<Cx> {
        let (rec, _) = self.changes.record(&caller.cx, repo, id).await?;
        let paths: Vec<String> = rec.paths.iter().map(|p| p.path.clone()).collect();
        let target = rec.cr().target_ref.clone();
        let auth = self
            .forge()
            .authz
            .repo(caller, repo, Action::Land, &paths, Some(&target), None)
            .await;
        let cx = self
            .base
            .guarded(caller, &repo.org_id, &repo.id, audit, auth)
            .await?;
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        Ok(cx)
    }

    async fn moderator(&self, caller: &Caller, repo: &Repo) -> bool {
        self.forge()
            .authz
            .repo(caller, repo, Action::Triage, &[], None, None)
            .await
            .is_ok()
    }

    /// `RepoService.GetContext`, served here (the forge core's `RepoService` delegates).
    pub async fn get_context(
        &self,
        req: Request<GetContextRequest>,
    ) -> std::result::Result<Response<RepoContext>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let paths = crate::service::utf8_paths(&r.paths)?;
            let cx = self
                .forge()
                .authz
                .repo(&caller, &repo, Action::Read, &paths, None, None)
                .await?;
            let at = r.at.map(|a| a.expr).unwrap_or_default();
            self.changes.context(&cx, &repo, &at, &r.paths).await
        }
        .await;
        reply(&caller, out)
    }
}

type Res<T> = std::result::Result<Response<T>, Status>;

#[tonic::async_trait]
impl ChangeService for ChangeServices {
    type DiffChangeRevisionsStream = ResponseStream<DiffItem>;

    async fn open_change(&self, req: Request<OpenChangeRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let fork = match r.head_repo.as_ref().filter(|h| h.repo.is_some()) {
                Some(h) => Some(self.base.repo(&caller, &Some(h.clone())).await?),
                None => None,
            }
            .filter(|f| f.id != repo.id);
            let cx = match &fork {
                Some(f) => {
                    self.fork_open_cx(&caller, &repo, f, r.head.as_ref(), &r.head_ref)
                        .await?
                }
                None => {
                    self.write_cx(&caller, &repo, Action::Commit, "cr.open")
                        .await?
                }
            };
            self.base
                .forge()
                .guard_write(&cx, &repo, crate::hooks::GuardedWrite::ChangeRequest, None)
                .await?;
            // A draft post written with the change: checked before the change is opened, so a
            // refused text opens nothing.
            let post_text = r.post_text.clone();
            let posts = if post_text.is_empty() {
                None
            } else {
                crate::posts::text::normalize(&post_text, false)?;
                Some(self.changes.posts().cloned().ok_or_else(|| {
                    crate::service::unserved(
                        "OpenChange.post_text",
                        "the posts module (crate::posts::Posts::attach)",
                    )
                })?)
            };
            // The post that inspired the change: a published post the caller can read, checked
            // before the change is opened.
            let inspired_by = if r.inspired_by_post_id.trim().is_empty() {
                None
            } else {
                let posts = self.changes.posts().ok_or_else(|| {
                    crate::service::unserved(
                        "OpenChange.inspired_by_post_id",
                        "the posts module (crate::posts::Posts::attach)",
                    )
                })?;
                Some(
                    posts
                        .inspiration(&self.base.forge().orgs, &cx, &r.inspired_by_post_id)
                        .await?,
                )
            };
            let inspired = inspired_by.is_some();
            let head_ref = r.head_ref.clone();
            let run = crate::swarms::outcomes::verified_run(&self.base.forge().db, &caller, &repo)
                .await?;
            let cr = self
                .changes
                .open_attributed(
                    &cx,
                    &repo,
                    NewChange {
                        target_ref: r.target_ref,
                        head: r.head,
                        title: r.title,
                        body: r.body,
                        depends_on: r.depends_on,
                        draft: r.draft,
                        reviewer_ids: r.reviewer_ids,
                        reviewer_team_ids: r.reviewer_team_ids,
                        labels: r.labels,
                        assignee_ids: r.assignee_ids,
                        milestone_id: r.milestone_id,
                        linked_issues: r.linked_issues,
                        template: r.template,
                        revert_of: String::new(),
                        head_repo_id: fork.as_ref().map(|f| f.id.clone()).unwrap_or_default(),
                        head_ref: head_ref.clone(),
                        allow_maintainer_edits: r.allow_maintainer_edits,
                        inspired_by,
                        brief: r.brief.clone(),
                    },
                    run.as_deref(),
                )
                .await?;
            if let Some(f) = &fork {
                self.forks()?
                    .follow(&cx, &f.id, &head_ref, &repo.id, &cr.id)
                    .await?;
            }
            if let Some(posts) = posts {
                posts.set(&cx, &repo, &cr.id, &post_text, &[], 0).await?;
                return self.with_post(&caller, &cx, &repo, cr).await;
            }
            if inspired {
                return self.with_post(&caller, &cx, &repo, cr).await;
            }
            Ok(cr)
        }
        .await;
        reply(&caller, out)
    }

    async fn get_change(&self, req: Request<GetChangeRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            let cr = self.changes.get(&cx, &repo, &r.id).await?;
            self.with_post(&caller, &cx, &repo, cr).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_changes(&self, req: Request<ListChangesRequest>) -> Res<ListChangesResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            let (token, size) = page(&r.page);
            let (changes, nt) = self
                .changes
                .list(
                    &cx,
                    &repo,
                    &ListFilter {
                        state: ChangeState::try_from(r.state).unwrap_or(ChangeState::Unspecified),
                        author_id: r.author_id,
                        target_ref: r.target_ref,
                        labels: r.labels,
                        review_requested_id: r.review_requested_id,
                        assignee_id: r.assignee_id,
                        draft: r.draft,
                        path_prefix: String::from_utf8(r.path_prefix).map_err(|_| {
                            ForgeError::invalid("invalid_path", "path_prefix must be UTF-8")
                        })?,
                        query: r.query,
                    },
                    &token,
                    size,
                )
                .await?;
            Ok(ListChangesResponse {
                changes: self.with_posts(&caller, &cx, &repo, changes).await?,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn revise_change(&self, req: Request<ReviseChangeRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let head = r.head.ok_or_else(|| ForgeError::missing("head"))?;
            let (cx, author) = self
                .author_or_write_cx(&caller, &repo, &r.id, "cr.revise")
                .await?;
            let (rec, _) = self.changes.record(&cx, &repo, &r.id).await?;
            let cr = rec.cr().clone();
            if cr.head_repo_id.is_empty() || cr.head_repo_id == repo.id {
                return self.changes.revise(&cx, &repo, &r.id, &head).await;
            }
            // A change request from a fork.
            let forks = self.forks()?;
            let fork = self.forge().orgs.repo_any(&cx, &cr.head_repo_id).await?;
            if author {
                // The contributor's new head lives in their fork.
                self.fork_open_cx(&caller, &repo, &fork, Some(&head), &cr.head_ref)
                    .await?;
                return self.changes.revise(&cx, &repo, &r.id, &head).await;
            }
            // A maintainer's edit: allowed only when the author allows it; the new head (in
            // this repo) is written back to the fork's branch.
            if !cr.allow_maintainer_edits {
                return Err(ForgeError::permission_denied(
                    "the author of this change request does not allow edits by maintainers",
                ));
            }
            let out = self.changes.revise(&cx, &repo, &r.id, &head).await?;
            forks
                .write_back(&cx, &repo, &fork, &cr.head_ref, &head)
                .await?;
            Ok(out)
        }
        .await;
        reply(&caller, out)
    }

    async fn close_change(&self, req: Request<CloseChangeRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Triage, "cr.close")
                .await?;
            self.changes.close(&cx, &repo, &r.id, &r.reason).await
        }
        .await;
        reply(&caller, out)
    }

    async fn submit_review(&self, req: Request<SubmitReviewRequest>) -> Res<Review> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Review, "review.submit")
                .await?;
            self.changes
                .submit_review(
                    &cx,
                    &repo,
                    &r.change_request_id,
                    r.revision,
                    Verdict::try_from(r.verdict).unwrap_or(Verdict::Unspecified),
                    &r.body,
                    r.comments,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_reviews(&self, req: Request<ListReviewsRequest>) -> Res<ListReviewsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            Ok(ListReviewsResponse {
                reviews: self
                    .changes
                    .reviews(&cx, &repo, &r.change_request_id)
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn evaluate_landing(
        &self,
        req: Request<EvaluateLandingRequest>,
    ) -> Res<LandingEvaluation> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            self.changes
                .evaluate(&cx, &repo, &r.change_request_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_change_by_number(
        &self,
        req: Request<GetChangeByNumberRequest>,
    ) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            let cr = self.changes.get_by_number(&cx, &repo, r.number).await?;
            self.with_post(&caller, &cx, &repo, cr).await
        }
        .await;
        reply(&caller, out)
    }

    async fn reopen_change(&self, req: Request<ReopenChangeRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Triage, "cr.reopen")
                .await?;
            self.changes.reopen(&cx, &repo, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn edit_change(&self, req: Request<EditChangeRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let (cx, author) = self
                .author_or_write_cx(&caller, &repo, &r.id, "cr.edit")
                .await?;
            if r.allow_maintainer_edits.is_some() && !author {
                return Err(ForgeError::permission_denied(
                    "only the change request's author decides whether maintainers may edit it",
                ));
            }
            self.changes
                .edit(
                    &cx,
                    &repo,
                    &r.id,
                    ChangeEdit {
                        title: r.title,
                        body: r.body,
                        target_ref: r.target_ref,
                        depends_on: r.depends_on,
                        expected_version: r.expected_version,
                        allow_maintainer_edits: r.allow_maintainer_edits,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn mark_change_ready(&self, req: Request<MarkChangeReadyRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Commit, "cr.ready_for_review")
                .await?;
            self.changes.set_draft(&cx, &repo, &r.id, false).await
        }
        .await;
        reply(&caller, out)
    }

    async fn convert_change_to_draft(
        &self,
        req: Request<ConvertChangeToDraftRequest>,
    ) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Commit, "cr.convert_to_draft")
                .await?;
            self.changes.set_draft(&cx, &repo, &r.id, true).await
        }
        .await;
        reply(&caller, out)
    }

    async fn request_reviewers(&self, req: Request<RequestReviewersRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Triage, "cr.request_reviewers")
                .await?;
            self.changes
                .request_reviewers(&cx, &repo, &r.id, &r.reviewer_ids, &r.team_ids)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_review_requests(
        &self,
        req: Request<RemoveReviewRequestsRequest>,
    ) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Triage, "cr.remove_review_requests")
                .await?;
            self.changes
                .remove_review_requests(&cx, &repo, &r.id, &r.reviewer_ids, &r.team_ids)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn dismiss_review(&self, req: Request<DismissReviewRequest>) -> Res<Review> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let auth = self
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
                .await;
            let cx = self
                .base
                .guarded(&caller, &repo.org_id, &repo.id, "review.dismiss", auth)
                .await?;
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            self.changes
                .dismiss_review(&cx, &repo, &r.change_request_id, &r.review_id, &r.reason)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn diff_change_revisions(
        &self,
        req: Request<DiffChangeRevisionsRequest>,
    ) -> Res<Self::DiffChangeRevisionsStream> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            let filter = super::review::DiffFilter {
                paths: crate::service::utf8_paths(&r.paths)?
                    .into_iter()
                    .map(|p| p.trim_end_matches('/').to_string())
                    .collect(),
                ignore_whitespace: r.ignore_whitespace,
            };
            self.changes
                .diff_revisions_filtered(
                    &cx,
                    &repo,
                    &r.change_request_id,
                    r.from_revision,
                    r.to_revision,
                    r.include_hunks,
                    r.context_lines,
                    &filter,
                )
                .await
        }
        .await;
        let items = out.map_err(|e| e.to_status(&caller.cx.request_id))?;
        let stream: Self::DiffChangeRevisionsStream =
            Box::pin(tokio_stream::iter(items.into_iter().map(Ok)));
        Ok(Response::new(stream))
    }

    async fn create_change_comment(
        &self,
        req: Request<CreateChangeCommentRequest>,
    ) -> Res<Comment> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let moderator = self.moderator(&caller, &repo).await;
            if r.path.is_empty() {
                let cx = self
                    .write_cx(&caller, &repo, Action::Read, "cr.comment")
                    .await?;
                self.base
                    .forge()
                    .guard_write(
                        &cx,
                        &repo,
                        crate::hooks::GuardedWrite::Comment,
                        Some(bee_proto::v1::SubjectRef {
                            kind: bee_proto::v1::SubjectKind::ChangeRequest as i32,
                            repo: None,
                            id: r.change_request_id.clone(),
                        }),
                    )
                    .await?;
                return self
                    .changes
                    .comment(&cx, &repo, &r.change_request_id, &r.body, moderator)
                    .await;
            }
            let cx = self
                .write_cx(&caller, &repo, Action::Review, "review.comment")
                .await?;
            let c = self
                .changes
                .comment_on_lines(
                    &cx,
                    &repo,
                    &r.change_request_id,
                    &r.body,
                    &r.path,
                    &r.side,
                    r.start_line,
                    r.end_line,
                    r.file_level,
                    r.revision,
                    r.suggestion,
                    moderator,
                )
                .await?;
            Ok(review_to_comment(&c, &repo, &r.change_request_id))
        }
        .await;
        reply(&caller, out)
    }

    async fn list_change_comments(
        &self,
        req: Request<ListChangeCommentsRequest>,
    ) -> Res<ListChangeCommentsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            let (token, size) = page(&r.page);
            let (comments, nt) = self
                .changes
                .comments(&cx, &repo, &r.change_request_id, &token, size)
                .await?;
            Ok(ListChangeCommentsResponse {
                comments,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_review_threads(
        &self,
        req: Request<ListReviewThreadsRequest>,
    ) -> Res<ListReviewThreadsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            let (token, size) = page(&r.page);
            let (threads, nt) = self
                .changes
                .threads(&cx, &repo, &r.change_request_id, r.resolved, &token, size)
                .await?;
            Ok(ListReviewThreadsResponse {
                threads,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn reply_to_review_thread(
        &self,
        req: Request<ReplyToReviewThreadRequest>,
    ) -> Res<ReviewComment> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let moderator = self.moderator(&caller, &repo).await;
            let cx = self
                .write_cx(&caller, &repo, Action::Review, "review.reply")
                .await?;
            self.changes
                .reply(&cx, &repo, &r.thread_id, &r.body, r.suggestion, moderator)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn resolve_review_thread(
        &self,
        req: Request<ResolveReviewThreadRequest>,
    ) -> Res<ReviewThread> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Review, "review.thread_resolve")
                .await?;
            self.changes
                .resolve_thread(&cx, &repo, &r.thread_id, true)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn unresolve_review_thread(
        &self,
        req: Request<UnresolveReviewThreadRequest>,
    ) -> Res<ReviewThread> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Review, "review.thread_unresolve")
                .await?;
            self.changes
                .resolve_thread(&cx, &repo, &r.thread_id, false)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn apply_suggestions(&self, req: Request<ApplySuggestionsRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Commit, "cr.apply_suggestions")
                .await?;
            self.changes
                .apply_suggestions(&cx, &repo, &r.change_request_id, &r.comment_ids, &r.message)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_file_viewed(
        &self,
        req: Request<SetFileViewedRequest>,
    ) -> Res<SetFileViewedResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Read, "cr.view_file")
                .await?;
            self.changes
                .set_viewed(&cx, &repo, &r.change_request_id, &r.path, r.viewed)
                .await?;
            Ok(SetFileViewedResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_viewed_files(
        &self,
        req: Request<ListViewedFilesRequest>,
    ) -> Res<ListViewedFilesResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            Ok(ListViewedFilesResponse {
                files: self
                    .changes
                    .viewed(&cx, &repo, &r.change_request_id)
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_change_labels(&self, req: Request<SetChangeLabelsRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Triage, "cr.set_labels")
                .await?;
            self.changes.set_labels(&cx, &repo, &r.id, &r.labels).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_change_assignees(
        &self,
        req: Request<SetChangeAssigneesRequest>,
    ) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Triage, "cr.set_assignees")
                .await?;
            self.changes
                .set_assignees(&cx, &repo, &r.id, &r.assignee_ids)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_change_milestone(
        &self,
        req: Request<SetChangeMilestoneRequest>,
    ) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Triage, "cr.set_milestone")
                .await?;
            self.changes
                .set_milestone(&cx, &repo, &r.id, &r.milestone_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn link_change_issues(
        &self,
        req: Request<LinkChangeIssuesRequest>,
    ) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Triage, "cr.link_issues")
                .await?;
            self.changes
                .link_issues(
                    &cx,
                    &repo,
                    &r.id,
                    &r.add_issues,
                    &r.remove_issues,
                    r.close_on_land,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_change_templates(
        &self,
        req: Request<ListChangeTemplatesRequest>,
    ) -> Res<ListChangeTemplatesResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            Ok(ListChangeTemplatesResponse {
                templates: self.changes.templates(&cx, &repo, &r.target_ref).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn land_change(&self, req: Request<LandChangeRequest>) -> Res<LandChangeResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let attempt = async {
                let cx = self.land_cx(&caller, &repo, &r.id, "cr.land").await?;
                self.changes
                    .land(
                        &cx,
                        &repo,
                        &r.id,
                        LandMethod::try_from(r.method).unwrap_or(LandMethod::Unspecified),
                        &r.message,
                        r.expected_revision,
                    )
                    .await
            }
            .await;
            // A safety evidence write cannot change the outcome of a committed merge.
            if let Err(error) = crate::safety::record_landing_attempt(
                self.forge(),
                &caller.cx,
                &repo,
                &caller.token,
                &format!("pull request {}", r.id),
                attempt.as_ref().err().map(|error| error.reason),
            )
            .await
            {
                eprintln!("Recording a safety landing attempt failed: {error}");
            }
            let (change, land) = attempt?;
            Ok(LandChangeResponse {
                change: Some(change),
                land: Some(land),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn enable_auto_merge(&self, req: Request<EnableAutoMergeRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .land_cx(&caller, &repo, &r.id, "cr.enable_auto_merge")
                .await?;
            self.changes
                .enable_auto_merge(
                    &cx,
                    &repo,
                    &r.id,
                    LandMethod::try_from(r.method).unwrap_or(LandMethod::Unspecified),
                    &r.message,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn disable_auto_merge(
        &self,
        req: Request<DisableAutoMergeRequest>,
    ) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .land_cx(&caller, &repo, &r.id, "cr.disable_auto_merge")
                .await?;
            self.changes.disable_auto_merge(&cx, &repo, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn enqueue_change(&self, req: Request<EnqueueChangeRequest>) -> Res<MergeQueueEntry> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .land_cx(&caller, &repo, &r.id, "merge_queue.enqueue")
                .await?;
            if r.priority > 0 {
                let auth = self
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
                    .await;
                self.base
                    .guarded(&caller, &repo.org_id, &repo.id, "merge_queue.jump", auth)
                    .await?;
            }
            self.changes
                .enqueue(
                    &cx,
                    &repo,
                    &r.id,
                    LandMethod::try_from(r.method).unwrap_or(LandMethod::Unspecified),
                    r.priority,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn dequeue_change(
        &self,
        req: Request<DequeueChangeRequest>,
    ) -> Res<DequeueChangeResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .land_cx(&caller, &repo, &r.id, "merge_queue.dequeue")
                .await?;
            self.changes.dequeue(&cx, &repo, &r.id).await?;
            Ok(DequeueChangeResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn get_merge_queue(
        &self,
        req: Request<GetMergeQueueRequest>,
    ) -> Res<GetMergeQueueResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            let (entries, lands_per_minute) = self.changes.queue(&cx, &repo, &r.r#ref).await?;
            Ok(GetMergeQueueResponse {
                entries,
                lands_per_minute,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn revert_change(&self, req: Request<RevertChangeRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Commit, "cr.revert")
                .await?;
            self.changes
                .revert(&cx, &repo, &r.id, &r.title, &r.body, r.draft)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_change_base(
        &self,
        req: Request<UpdateChangeBaseRequest>,
    ) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Commit, "cr.update_base")
                .await?;
            self.changes
                .update_base(
                    &cx,
                    &repo,
                    &r.id,
                    LandMethod::try_from(r.method).unwrap_or(LandMethod::Unspecified),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_change_stack(
        &self,
        req: Request<ListChangeStackRequest>,
    ) -> Res<ListChangeStackResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            Ok(ListChangeStackResponse {
                changes: self.changes.stack(&cx, &repo, &r.id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_change_summary(
        &self,
        req: Request<SetChangeSummaryRequest>,
    ) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Commit, "cr.set_summary")
                .await?;
            self.changes
                .set_summary(&cx, &repo, &r.id, &r.summary)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_changeset_comment(
        &self,
        req: Request<CreateChangesetCommentRequest>,
    ) -> Res<Comment> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Read, "changeset_comment.create")
                .await?;
            let cs = r
                .changeset
                .ok_or_else(|| ForgeError::missing("changeset"))?;
            self.changes
                .changeset_comment(&cx, &repo, &cs, &r.body, &r.path, r.line)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_changeset_comments(
        &self,
        req: Request<ListChangesetCommentsRequest>,
    ) -> Res<ListChangesetCommentsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            let cs = r
                .changeset
                .ok_or_else(|| ForgeError::missing("changeset"))?;
            let (token, size) = page(&r.page);
            let (comments, nt) = self
                .changes
                .changeset_comments(&cx, &repo, &cs, &token, size)
                .await?;
            Ok(ListChangesetCommentsResponse {
                comments,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_land_conflict(&self, req: Request<GetLandConflictRequest>) -> Res<LandConflict> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            self.changes.conflict(&cx, &repo, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_land_conflicts(
        &self,
        req: Request<ListLandConflictsRequest>,
    ) -> Res<ListLandConflictsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.read_cx(&caller, &repo).await?;
            let (token, size) = page(&r.page);
            let (conflicts, nt) = self
                .changes
                .conflicts(
                    &cx,
                    &repo,
                    LandConflictState::try_from(r.state).unwrap_or(LandConflictState::Unspecified),
                    &token,
                    size,
                )
                .await?;
            Ok(ListLandConflictsResponse {
                conflicts,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn submit_conflict_resolution(
        &self,
        req: Request<SubmitConflictResolutionRequest>,
    ) -> Res<LandConflict> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .write_cx(&caller, &repo, Action::Commit, "land_conflict.resolve")
                .await?;
            let res = r
                .resolution
                .ok_or_else(|| ForgeError::missing("resolution"))?;
            self.changes
                .submit_resolution(&cx, &repo, &r.id, &res, r.land)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn add_change_reaction(
        &self,
        req: Request<AddChangeReactionRequest>,
    ) -> Res<ChangeReactionResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = self
            .react(
                &caller,
                &r.repo,
                &r.change_request_id,
                &r.comment_id,
                r.content,
                true,
            )
            .await;
        reply(&caller, out)
    }

    async fn remove_change_reaction(
        &self,
        req: Request<RemoveChangeReactionRequest>,
    ) -> Res<ChangeReactionResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = self
            .react(
                &caller,
                &r.repo,
                &r.change_request_id,
                &r.comment_id,
                r.content,
                false,
            )
            .await;
        reply(&caller, out)
    }

    async fn set_change_locked(&self, req: Request<SetChangeLockedRequest>) -> Res<ChangeRequest> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let audit = if r.locked { "cr.lock" } else { "cr.unlock" };
            let cx = self.write_cx(&caller, &repo, Action::Triage, audit).await?;
            self.changes
                .set_locked(&cx, &repo, &r.change_request_id, r.locked)
                .await
        }
        .await;
        reply(&caller, out)
    }
}

impl ChangeServices {
    /// Adds or removes the caller's reaction on a CR or one of its comments (`read`).
    async fn react(
        &self,
        caller: &crate::auth::Caller,
        repo: &Option<RepoRef>,
        cr: &str,
        comment_id: &str,
        content: i32,
        add: bool,
    ) -> Result<ChangeReactionResponse> {
        let repo = self.base.repo(caller, repo).await?;
        let cx = self
            .write_cx(caller, &repo, Action::Read, "cr.react")
            .await?;
        let content = ReactionContent::try_from(content).unwrap_or(ReactionContent::Unspecified);
        let reactions = if comment_id.is_empty() {
            self.changes
                .react(&cx, &repo, cr, content, add)
                .await?
                .reactions
        } else {
            self.changes
                .react_comment(&cx, &repo, cr, comment_id, content, add)
                .await?
        };
        Ok(ChangeReactionResponse { reactions })
    }
}
