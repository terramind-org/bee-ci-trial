//! `CommentService`: one comment API across domains (PRODUCT §4.21). A comment id is routed
//! to the store that owns it:
//!
//! | id | owner |
//! |---|---|
//! | `<home>.<number>.<ulid>` | discussion comment (`community::discussions`) |
//! | an issue comment id (`work` lookup) | issue comment (`work::issues`) |
//! | a CR conversation comment id (`changes` lookup) | change request comment (`changes::comments`) |
//!
//! Reactions and read markers take a [`SubjectRef`]: `ISSUE`, `CHANGE_REQUEST` and
//! `DISCUSSION` subjects (by id, or for issues and CRs by number with `repo`), and `COMMENT`
//! subjects routed like comment ids. Authorization is the owning service's: reads need repo
//! `read`; edits and deletes are the author's, or a triager's (moderator's) for anyone's
//! comment. Every change is audited by the owning store.

use bee_policy::Action;
use bee_proto::v1::comment_service_server::{CommentService, CommentServiceServer};
use bee_proto::v1::{
    AddReactionRequest, Comment, DeleteCommentRequest, DeleteCommentResponse, GetCommentRequest,
    ListCommentEditsRequest, ListCommentEditsResponse, ListReactionsRequest, ListReactionsResponse,
    MarkSubjectReadRequest, MarkSubjectReadResponse, Reaction, ReactionContent, ReactionGroup,
    RemoveReactionRequest, RemoveReactionResponse, Repo, SubjectKind, SubjectRef,
    UpdateCommentRequest,
};
use tonic::{Request, Response, Status};

use super::work::Level;
use super::{ForgeServices, next, page, reply};
use crate::auth::Caller;
use crate::changes::comments::CommentHome;
use crate::community::discussions::{Discussions, parse_comment_id};
use crate::community::notifications::{Mark, Selector, subject_key};
use crate::community::reactions::{Where, comment_key, discussion_key};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;

/// Where a comment id lives.
enum Located {
    Issue { repo: Repo },
    Discussion,
    Change { repo: Repo, home: CommentHome },
}

/// A resolved reaction subject.
enum Target {
    /// An issue or issue comment: the work store's subject id.
    Work { repo: Repo, subject_id: String },
    /// A discussion or discussion comment: the community reaction key.
    Community {
        key: String,
        at: Where,
        discussion: String,
        comment: bool,
    },
    /// A change request.
    Change { repo: Repo, cr_id: String },
    /// A CR conversation comment.
    ChangeComment {
        home: CommentHome,
        comment_id: String,
        repo: Repo,
    },
}

fn content_of(c: i32) -> Result<ReactionContent> {
    match ReactionContent::try_from(c) {
        Ok(ReactionContent::Unspecified) | Err(_) => Err(ForgeError::invalid(
            "invalid_reaction",
            "content must be one of the eight fixed reactions",
        )),
        Ok(c) => Ok(c),
    }
}

/// Groups reactions by content, marking the viewer's own.
fn groups_of(reactions: &[Reaction], viewer: &str) -> Vec<ReactionGroup> {
    let mut out: Vec<ReactionGroup> = Vec::new();
    for r in reactions {
        let mine = r.user.as_ref().is_some_and(|u| u.id == viewer);
        match out.iter_mut().find(|g| g.content == r.content) {
            Some(g) => {
                g.count += 1;
                g.viewer_reacted |= mine;
            }
            None => out.push(ReactionGroup {
                content: r.content,
                count: 1,
                viewer_reacted: mine,
            }),
        }
    }
    out.sort_by_key(|g| g.content);
    out
}

/// Pages an in-memory list by id.
fn page_by_id(mut items: Vec<Reaction>, after: &str, limit: usize) -> (Vec<Reaction>, String) {
    items.sort_by(|a, b| a.id.cmp(&b.id));
    let rest: Vec<Reaction> = items
        .into_iter()
        .filter(|r| after.is_empty() || r.id.as_str() > after)
        .collect();
    let next = if rest.len() > limit {
        rest[limit - 1].id.clone()
    } else {
        String::new()
    };
    (rest.into_iter().take(limit).collect(), next)
}

impl ForgeServices {
    /// The comment service server.
    pub fn comment_service(&self) -> CommentServiceServer<ForgeServices> {
        CommentServiceServer::new(self.clone())
    }

    async fn locate_comment(&self, caller: &Caller, id: &str) -> Result<Located> {
        if id.is_empty() {
            return Err(ForgeError::missing("comment_id"));
        }
        if parse_comment_id(id).is_ok() {
            return Ok(Located::Discussion);
        }
        let cx = &caller.cx;
        if let Some((repo_id, _)) = self.forge.work.comment_location(cx, id).await? {
            let repo = self.forge.orgs.repo_any(cx, &repo_id).await?;
            return Ok(Located::Issue { repo });
        }
        if let Some(changes) = self.forge.changes()
            && let Some(home) = changes.comment_home(cx, id).await?
        {
            let repo = self.forge.orgs.repo_any(cx, &home.repo_id).await?;
            return Ok(Located::Change { repo, home });
        }
        Err(ForgeError::not_found(format!("comment {id}")))
    }

    fn changes_or_unavailable(&self) -> Result<&std::sync::Arc<crate::changes::Changes>> {
        self.forge
            .changes()
            .ok_or_else(|| ForgeError::invalid_state("change requests are not enabled here"))
    }

    /// Authorizes a CR comment action: reads need `read`; writes need `read` (any reader
    /// may comment) and are audited as `audit` when denied. Returns the context and whether
    /// the caller may moderate (triage).
    async fn cr_access(
        &self,
        caller: &Caller,
        repo: &Repo,
        write: bool,
        audit: &str,
    ) -> Result<(Cx, bool)> {
        let cx = self
            .repo_cx(caller, repo, Action::Read, None, write.then_some(audit))
            .await?;
        if write && repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        let moderator = self.can(caller, repo, Action::Triage).await;
        Ok((cx, moderator))
    }

    async fn subject_repo(&self, caller: &Caller, s: &SubjectRef) -> Result<Repo> {
        self.repo(caller, &s.repo).await
    }

    async fn resolve_target(&self, caller: &Caller, s: &SubjectRef) -> Result<Target> {
        if s.id.is_empty() {
            return Err(ForgeError::missing("subject.id"));
        }
        match SubjectKind::try_from(s.kind).unwrap_or(SubjectKind::Unspecified) {
            SubjectKind::Issue => {
                let repo = self.subject_repo(caller, s).await?;
                let info = Self::info(&repo);
                let issue = match s.id.parse::<u64>() {
                    Ok(n) => {
                        self.forge
                            .work
                            .get_issue_by_number(&caller.cx, &info, n)
                            .await?
                    }
                    Err(_) => self.forge.work.get_issue(&caller.cx, &info, &s.id).await?,
                };
                Ok(Target::Work {
                    repo,
                    subject_id: issue.id,
                })
            }
            SubjectKind::ChangeRequest => {
                let repo = self.subject_repo(caller, s).await?;
                let changes = self.changes_or_unavailable()?;
                let cr = match s.id.parse::<u64>() {
                    Ok(n) => changes.get_by_number(&caller.cx, &repo, n).await?,
                    Err(_) => changes.get(&caller.cx, &repo, &s.id).await?,
                };
                Ok(Target::Change { repo, cr_id: cr.id })
            }
            SubjectKind::Discussion => {
                let (key, _) = crate::community::discussions::parse_discussion_id(&s.id)?;
                let (org_id, repo_id) = self.home_ids(caller, &key).await?;
                Ok(Target::Community {
                    key: discussion_key(&s.id),
                    at: Where {
                        org_id,
                        repo_id,
                        kind: SubjectKind::Discussion as i32,
                        id: s.id.clone(),
                    },
                    discussion: s.id.clone(),
                    comment: false,
                })
            }
            SubjectKind::Comment => match self.locate_comment(caller, &s.id).await? {
                Located::Issue { repo } => Ok(Target::Work {
                    repo,
                    subject_id: s.id.clone(),
                }),
                Located::Discussion => {
                    let (key, _) = Discussions::comment_home(&s.id)?;
                    let (org_id, repo_id) = self.home_ids(caller, &key).await?;
                    Ok(Target::Community {
                        key: comment_key(&s.id),
                        at: Where {
                            org_id,
                            repo_id,
                            kind: SubjectKind::Comment as i32,
                            id: s.id.clone(),
                        },
                        discussion: s.id.clone(),
                        comment: true,
                    })
                }
                Located::Change { repo, home } => Ok(Target::ChangeComment {
                    home,
                    comment_id: s.id.clone(),
                    repo,
                }),
            },
            _ => Err(ForgeError::invalid(
                "unsupported_subject",
                "reactions apply to issues, change requests, discussions and comments",
            )),
        }
    }

    /// Org and repo ids of a discussion home key (a repo id or an org id).
    async fn home_ids(&self, caller: &Caller, key: &str) -> Result<(String, String)> {
        if let Ok(r) = self.forge.orgs.repo_any(&caller.cx, key).await {
            return Ok((r.org_id, r.id));
        }
        let o = self.forge.orgs.org_any(&caller.cx, key).await?;
        Ok((o.id, String::new()))
    }

    /// Authorizes a reaction call on a resolved target; returns the authorized context.
    async fn target_cx(&self, caller: &Caller, t: &Target, write: bool, audit: &str) -> Result<Cx> {
        match t {
            Target::Work { repo, .. } => {
                let level = if write { Level::Read } else { Level::View };
                Ok(self.issue_access(caller, repo, level, audit).await?.0)
            }
            Target::Community {
                discussion,
                comment,
                ..
            } => {
                if *comment {
                    Ok(self
                        .discussion_comment_access(caller, discussion, write, audit)
                        .await?
                        .0)
                } else {
                    Ok(self
                        .discussion_access(caller, discussion, write, audit)
                        .await?
                        .0)
                }
            }
            Target::Change { repo, .. } | Target::ChangeComment { repo, .. } => {
                Ok(self.cr_access(caller, repo, write, audit).await?.0)
            }
        }
    }

    async fn get_comment_impl(&self, caller: &Caller, r: GetCommentRequest) -> Result<Comment> {
        match self.locate_comment(caller, &r.comment_id).await? {
            Located::Issue { repo } => {
                let (cx, _) = self
                    .issue_access(caller, &repo, Level::View, "issue_comment.get")
                    .await?;
                self.forge.work.get_comment(&cx, &r.comment_id).await
            }
            Located::Discussion => {
                let (cx, _, _) = self
                    .discussion_comment_access(caller, &r.comment_id, false, "")
                    .await?;
                let d = self
                    .discussions_store()
                    .get_comment(&cx, &r.comment_id)
                    .await?;
                Ok(discussion_comment(d))
            }
            Located::Change { repo, home } => {
                let (cx, _) = self.cr_access(caller, &repo, false, "").await?;
                self.changes_or_unavailable()?
                    .get_comment(&cx, &home, &r.comment_id)
                    .await
            }
        }
    }

    async fn update_comment_impl(
        &self,
        caller: &Caller,
        r: UpdateCommentRequest,
    ) -> Result<Comment> {
        if r.body.trim().is_empty() {
            return Err(ForgeError::missing("body"));
        }
        match self.locate_comment(caller, &r.comment_id).await? {
            Located::Issue { repo } => {
                let (cx, actor) = self
                    .issue_access(caller, &repo, Level::Read, "issue_comment.update")
                    .await?;
                let effects = self.effects_of(caller, &repo, &r.body).await;
                self.forge
                    .work
                    .update_comment(
                        &cx,
                        &Self::info(&repo),
                        &actor,
                        &r.comment_id,
                        &r.body,
                        r.expected_version,
                        &effects,
                    )
                    .await
            }
            Located::Discussion => {
                let (cx, home, moderator) = self
                    .discussion_comment_access(
                        caller,
                        &r.comment_id,
                        true,
                        "discussion.comment_edit",
                    )
                    .await?;
                let d = self
                    .discussions_store()
                    .edit_comment(
                        &cx,
                        &home,
                        &r.comment_id,
                        &r.body,
                        r.expected_version,
                        moderator,
                    )
                    .await?;
                Ok(discussion_comment(d))
            }
            Located::Change { repo, home } => {
                let (cx, moderator) = self
                    .cr_access(caller, &repo, true, "cr.comment_edit")
                    .await?;
                self.changes_or_unavailable()?
                    .edit_comment(
                        &cx,
                        &home,
                        &r.comment_id,
                        &r.body,
                        r.expected_version,
                        moderator,
                    )
                    .await
            }
        }
    }

    async fn delete_comment_impl(&self, caller: &Caller, r: DeleteCommentRequest) -> Result<()> {
        match self.locate_comment(caller, &r.comment_id).await? {
            Located::Issue { repo } => {
                let (cx, actor) = self
                    .issue_access(caller, &repo, Level::Read, "issue_comment.delete")
                    .await?;
                self.forge
                    .work
                    .delete_comment(&cx, &Self::info(&repo), &actor, &r.comment_id)
                    .await
            }
            Located::Discussion => {
                let (cx, home, moderator) = self
                    .discussion_comment_access(
                        caller,
                        &r.comment_id,
                        true,
                        "discussion.comment_delete",
                    )
                    .await?;
                self.discussions_store()
                    .delete_comment(&cx, &home, &r.comment_id, moderator)
                    .await
            }
            Located::Change { repo, home } => {
                let (cx, moderator) = self
                    .cr_access(caller, &repo, true, "cr.comment_delete")
                    .await?;
                self.changes_or_unavailable()?
                    .delete_comment(&cx, &home, &r.comment_id, moderator)
                    .await
            }
        }
    }

    async fn list_edits_impl(
        &self,
        caller: &Caller,
        r: ListCommentEditsRequest,
    ) -> Result<ListCommentEditsResponse> {
        let (token, limit) = page(&r.page);
        let all = match self.locate_comment(caller, &r.comment_id).await? {
            Located::Issue { repo } => {
                let (cx, _) = self
                    .issue_access(caller, &repo, Level::View, "issue_comment.edits")
                    .await?;
                let (edits, next_token) = self
                    .forge
                    .work
                    .list_comment_edits(&cx, &r.comment_id, limit, &token)
                    .await?;
                return Ok(ListCommentEditsResponse {
                    edits,
                    page: next(next_token),
                });
            }
            Located::Discussion => {
                let (cx, _, _) = self
                    .discussion_comment_access(caller, &r.comment_id, false, "")
                    .await?;
                self.discussions_store()
                    .comment_edits(&cx, &r.comment_id)
                    .await?
            }
            Located::Change { repo, home } => {
                let (cx, _) = self.cr_access(caller, &repo, false, "").await?;
                self.changes_or_unavailable()?
                    .comment_edits(&cx, &home, &r.comment_id)
                    .await?
            }
        };
        // Offset paging over a comment's (short) edit list.
        let start: usize = if token.is_empty() {
            0
        } else {
            token
                .parse()
                .map_err(|_| ForgeError::invalid("bad_page_token", "malformed page token"))?
        };
        let end = (start + limit).min(all.len());
        let edits = all.get(start..end).map(<[_]>::to_vec).unwrap_or_default();
        let next_token = if end < all.len() {
            end.to_string()
        } else {
            String::new()
        };
        Ok(ListCommentEditsResponse {
            edits,
            page: next(next_token),
        })
    }

    async fn react_impl(
        &self,
        caller: &Caller,
        subject: &Option<SubjectRef>,
        content: i32,
        add: bool,
    ) -> Result<Option<Reaction>> {
        let s = subject
            .as_ref()
            .ok_or_else(|| ForgeError::missing("subject"))?;
        let content = content_of(content)?;
        let t = self.resolve_target(caller, s).await?;
        let audit = if add {
            "reaction.add"
        } else {
            "reaction.remove"
        };
        let cx = self.target_cx(caller, &t, true, audit).await?;
        match t {
            Target::Work { repo, subject_id } => {
                let level = Level::Read;
                let (_, actor) = self.issue_access(caller, &repo, level, audit).await?;
                let info = Self::info(&repo);
                if add {
                    Ok(Some(
                        self.forge
                            .work
                            .add_reaction(&cx, &info, &actor, &subject_id, content)
                            .await?,
                    ))
                } else {
                    self.forge
                        .work
                        .remove_reaction(&cx, &info, &actor, &subject_id, content)
                        .await?;
                    Ok(None)
                }
            }
            Target::Community { key, at, .. } => {
                let reactions = &self.community().reactions;
                if add {
                    Ok(Some(reactions.add(&cx, &key, &at, content as i32).await?))
                } else {
                    reactions.remove(&cx, &key, &at, content as i32).await?;
                    Ok(None)
                }
            }
            Target::Change { repo, cr_id } => {
                let changes = self.changes_or_unavailable()?;
                changes.react(&cx, &repo, &cr_id, content, add).await?;
                Ok(add.then(|| Reaction {
                    id: format!("{cr_id}:{}:{}", cx.actor_id, content as i32),
                    content: content as i32,
                    user: Some(cx.principal()),
                    created_at: Some(ts(self.forge.db.now_ms())),
                }))
            }
            Target::ChangeComment {
                home, comment_id, ..
            } => {
                let r = self
                    .changes_or_unavailable()?
                    .react_comment_by_id(&cx, &home, &comment_id, content, add)
                    .await?;
                Ok(add.then_some(r))
            }
        }
    }

    async fn list_reactions_impl(
        &self,
        caller: &Caller,
        r: ListReactionsRequest,
    ) -> Result<ListReactionsResponse> {
        let s = r
            .subject
            .as_ref()
            .ok_or_else(|| ForgeError::missing("subject"))?;
        let (token, limit) = page(&r.page);
        let t = self.resolve_target(caller, s).await?;
        let cx = self.target_cx(caller, &t, false, "").await?;
        let content = ReactionContent::try_from(r.content).unwrap_or(ReactionContent::Unspecified);
        let (all, groups) = match t {
            Target::Work { subject_id, .. } => {
                let (reactions, groups, next_token) = self
                    .forge
                    .work
                    .list_reactions(&cx, &subject_id, content, limit, &token)
                    .await?;
                return Ok(ListReactionsResponse {
                    reactions,
                    groups,
                    page: next(next_token),
                });
            }
            Target::Community { key, .. } => {
                let (reactions, groups, next_token) = self
                    .community()
                    .reactions
                    .list(&cx, &key, content as i32, &token, limit)
                    .await?;
                return Ok(ListReactionsResponse {
                    reactions,
                    groups,
                    page: next(next_token),
                });
            }
            Target::Change { repo, cr_id } => {
                let all = self
                    .changes_or_unavailable()?
                    .cr_reactions(&cx, &repo.id, &cr_id)
                    .await?;
                let groups = groups_of(&all, &cx.actor_id);
                (all, groups)
            }
            Target::ChangeComment {
                home, comment_id, ..
            } => {
                let all = self
                    .changes_or_unavailable()?
                    .comment_reactions(&cx, &home, &comment_id)
                    .await?;
                let groups = groups_of(&all, &cx.actor_id);
                (all, groups)
            }
        };
        let filtered: Vec<Reaction> = all
            .into_iter()
            .filter(|x| content == ReactionContent::Unspecified || x.content == content as i32)
            .collect();
        let (reactions, next_token) = page_by_id(filtered, &token, limit);
        Ok(ListReactionsResponse {
            reactions,
            groups,
            page: next(next_token),
        })
    }

    async fn mark_read_impl(
        &self,
        caller: &Caller,
        r: MarkSubjectReadRequest,
    ) -> Result<MarkSubjectReadResponse> {
        let s = r
            .subject
            .as_ref()
            .ok_or_else(|| ForgeError::missing("subject"))?;
        if s.id.is_empty() {
            return Err(ForgeError::missing("subject.id"));
        }
        // The subject must be readable; its thread key uses the repo id and the subject id
        // the notification was recorded with.
        let t = self.resolve_target(caller, s).await?;
        self.target_cx(caller, &t, false, "").await?;
        let (repo_id, id) = match &t {
            Target::Work { repo, subject_id } => (repo.id.clone(), subject_id.clone()),
            Target::Change { repo, cr_id } => (repo.id.clone(), cr_id.clone()),
            Target::Community { at, .. } => (at.repo_id.clone(), s.id.clone()),
            Target::ChangeComment { repo, .. } => (repo.id.clone(), s.id.clone()),
        };
        let cx = self.user_cx(caller, Action::Read)?;
        let now = self.forge.db.now_ms();
        let notifications = &self.community().notifications;
        // Threads are keyed by subject; the recorded id may be the ULID or the number.
        for key in [
            subject_key(s.kind, &repo_id, &id),
            subject_key(s.kind, &repo_id, &s.id),
        ] {
            let sel = Selector {
                thread_ids: vec![key],
                filter: None,
            };
            match notifications.mark(&cx, &sel, Mark::Read, now).await {
                Ok(_) => break,
                Err(e) if e.reason == bee_proto::v1::ErrorReason::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(MarkSubjectReadResponse {
            read_at: Some(ts(now)),
        })
    }
}

/// A discussion comment in the generic comment shape.
fn discussion_comment(d: bee_proto::v1::DiscussionComment) -> Comment {
    d.comment.unwrap_or_default()
}

#[tonic::async_trait]
impl CommentService for ForgeServices {
    async fn get_comment(
        &self,
        req: Request<GetCommentRequest>,
    ) -> std::result::Result<Response<Comment>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        reply(&caller, self.get_comment_impl(&caller, r).await)
    }

    async fn update_comment(
        &self,
        req: Request<UpdateCommentRequest>,
    ) -> std::result::Result<Response<Comment>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        reply(&caller, self.update_comment_impl(&caller, r).await)
    }

    async fn delete_comment(
        &self,
        req: Request<DeleteCommentRequest>,
    ) -> std::result::Result<Response<DeleteCommentResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = self
            .delete_comment_impl(&caller, r)
            .await
            .map(|_| DeleteCommentResponse {});
        reply(&caller, out)
    }

    async fn list_comment_edits(
        &self,
        req: Request<ListCommentEditsRequest>,
    ) -> std::result::Result<Response<ListCommentEditsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        reply(&caller, self.list_edits_impl(&caller, r).await)
    }

    async fn add_reaction(
        &self,
        req: Request<AddReactionRequest>,
    ) -> std::result::Result<Response<Reaction>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = self
            .react_impl(&caller, &r.subject, r.content, true)
            .await
            .map(Option::unwrap_or_default);
        reply(&caller, out)
    }

    async fn remove_reaction(
        &self,
        req: Request<RemoveReactionRequest>,
    ) -> std::result::Result<Response<RemoveReactionResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = self
            .react_impl(&caller, &r.subject, r.content, false)
            .await
            .map(|_| RemoveReactionResponse {});
        reply(&caller, out)
    }

    async fn list_reactions(
        &self,
        req: Request<ListReactionsRequest>,
    ) -> std::result::Result<Response<ListReactionsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        reply(&caller, self.list_reactions_impl(&caller, r).await)
    }

    async fn mark_subject_read(
        &self,
        req: Request<MarkSubjectReadRequest>,
    ) -> std::result::Result<Response<MarkSubjectReadResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        reply(&caller, self.mark_read_impl(&caller, r).await)
    }
}
